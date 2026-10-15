//! End-to-end tests of the compiled program in LiteSVM, with the real SPL
//! Token and Token-2022 programs.
//!
//! Run: `cargo build-sbf` then `cargo test -p tender --test integration`.

use anchor_lang::{system_program, AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token;
use anchor_spl::token_2022::spl_token_2022;
use litesvm::LiteSVM;
use solana_sdk::{
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use tender::{Issuer, Reserve, ReserveParams, Route, ROUTE_DOLLAR, ROUTE_USDC};

const USD: u64 = 1_000_000; // 6 decimals
const PRICE_ONE: u64 = 1_000_000_000;

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    oracle: Keypair,
    allocator: Keypair,
    treasury: Keypair,
    usdc: Pubkey,
    tbill: Pubkey,
    allocator_usdc: Pubkey,
    allocator_tbill: Pubkey,
}

fn reserve_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"reserve"], &tender::ID).0
}
fn usdc_vault() -> Pubkey {
    Pubkey::find_program_address(&[b"usdc_vault"], &tender::ID).0
}
fn tbill_vault() -> Pubkey {
    Pubkey::find_program_address(&[b"tbill_vault"], &tender::ID).0
}
fn issuer_pda(symbol: &str) -> Pubkey {
    Pubkey::find_program_address(&[b"issuer", symbol.as_bytes()], &tender::ID).0
}
fn mint_pda(issuer: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"mint", issuer.as_ref()], &tender::ID).0
}

impl Env {
    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
        self.svm.expire_blockhash();
        let payer = signers[0].pubkey();
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&payer),
            signers,
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}\n{}", e.err, e.meta.logs.join("\n")))
    }

    fn wallet(&mut self) -> Keypair {
        let k = Keypair::new();
        self.svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
        k
    }

    fn new_mint(&mut self, program: &Pubkey) -> Pubkey {
        let mint = Keypair::new();
        let len = spl_token::state::Mint::LEN;
        let rent = self.svm.minimum_balance_for_rent_exemption(len);
        let admin = self.admin.insecure_clone();
        let ixs = [
            system_instruction::create_account(&admin.pubkey(), &mint.pubkey(), rent, len as u64, program),
            spl_token::instruction::initialize_mint2(program, &mint.pubkey(), &admin.pubkey(), None, 6)
                .unwrap(),
        ];
        self.send(&ixs, &[&admin, &mint]).unwrap();
        mint.pubkey()
    }

    /// Plain token account (works for both token programs).
    fn token_account(&mut self, mint: &Pubkey, owner: &Pubkey, program: &Pubkey) -> Pubkey {
        let acct = Keypair::new();
        let len = spl_token::state::Account::LEN;
        let rent = self.svm.minimum_balance_for_rent_exemption(len);
        let admin = self.admin.insecure_clone();
        let init = if *program == spl_token_2022::ID {
            spl_token_2022::instruction::initialize_account3(program, &acct.pubkey(), mint, owner).unwrap()
        } else {
            spl_token::instruction::initialize_account3(program, &acct.pubkey(), mint, owner).unwrap()
        };
        let ixs = [
            system_instruction::create_account(&admin.pubkey(), &acct.pubkey(), rent, len as u64, program),
            init,
        ];
        self.send(&ixs, &[&admin, &acct]).unwrap();
        acct.pubkey()
    }

    fn fund(&mut self, mint: &Pubkey, to: &Pubkey, amount: u64) {
        let admin = self.admin.insecure_clone();
        let ix = spl_token::instruction::mint_to(&spl_token::ID, mint, to, &admin.pubkey(), &[], amount)
            .unwrap();
        self.send(&[ix], &[&admin]).unwrap();
    }

    fn balance(&self, acct: &Pubkey) -> u64 {
        let data = self.svm.get_account(acct).unwrap().data;
        // amount sits at the same offset for both token programs
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    }

    fn reserve(&self) -> Reserve {
        let data = self.svm.get_account(&reserve_pda()).unwrap().data;
        Reserve::try_deserialize(&mut data.as_slice()).unwrap()
    }

    fn issuer(&self, symbol: &str) -> Issuer {
        let data = self.svm.get_account(&issuer_pda(symbol)).unwrap().data;
        Issuer::try_deserialize(&mut data.as_slice()).unwrap()
    }

    fn warp(&mut self, secs: i64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp += secs;
        self.svm.set_sysvar(&clock);
    }

    fn params(&self) -> ReserveParams {
        ReserveParams {
            price_authority: self.oracle.pubkey(),
            allocator: self.allocator.pubkey(),
            treasury: self.treasury.pubkey(),
            protocol_fee_bps: 1_000, // 10% of yield
            min_liquid_bps: 2_000,   // keep 20% of supply liquid
            paused: false,
            tbill_price: PRICE_ONE,
        }
    }

    // ------------------------------------------------------------ instructions

    fn create_issuer(&mut self, creator: &Keypair, name: &str, symbol: &str, routes: Vec<Route>) -> Result<(), String> {
        let issuer = issuer_pda(symbol);
        let ix = Instruction {
            program_id: tender::ID,
            accounts: tender::accounts::CreateIssuer {
                creator: creator.pubkey(),
                reserve: reserve_pda(),
                issuer,
                mint: mint_pda(&issuer),
                dollar_token_program: spl_token_2022::ID,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
            data: tender::instruction::CreateIssuer {
                args: tender::CreateIssuerArgs {
                    name: name.into(),
                    symbol: symbol.into(),
                    uri: "https://tender.example/meta.json".into(),
                    routes,
                },
            }
            .data(),
        };
        self.send(&[ix], &[creator])
    }

    fn convert_ix(&self, user: &Pubkey, symbol: &str, user_usdc: &Pubkey, user_dollar: &Pubkey, amount: u64, mint: bool) -> Instruction {
        let issuer = issuer_pda(symbol);
        let accounts = tender::accounts::Convert {
            user: *user,
            reserve: reserve_pda(),
            issuer,
            mint: mint_pda(&issuer),
            usdc_mint: self.usdc,
            usdc_vault: usdc_vault(),
            tbill_vault: tbill_vault(),
            user_usdc: *user_usdc,
            user_dollar: *user_dollar,
            usdc_token_program: spl_token::ID,
            dollar_token_program: spl_token_2022::ID,
        }
        .to_account_metas(None);
        let data = if mint {
            tender::instruction::Mint { amount }.data()
        } else {
            tender::instruction::Redeem { amount }.data()
        };
        Instruction { program_id: tender::ID, accounts, data }
    }

    fn harvest(&mut self, caller: &Keypair, symbol: &str, route_accounts: &[Pubkey]) -> Result<(), String> {
        let issuer = issuer_pda(symbol);
        let mut accounts = tender::accounts::Harvest {
            reserve: reserve_pda(),
            issuer,
            mint: mint_pda(&issuer),
            usdc_mint: self.usdc,
            usdc_vault: usdc_vault(),
            tbill_vault: tbill_vault(),
            usdc_token_program: spl_token::ID,
            dollar_token_program: spl_token_2022::ID,
        }
        .to_account_metas(None);
        accounts.extend(route_accounts.iter().map(|k| AccountMeta::new(*k, false)));
        let ix = Instruction { program_id: tender::ID, accounts, data: tender::instruction::Harvest {}.data() };
        self.send(&[ix], &[caller])
    }

    fn rebalance(&mut self, tbill_amount: u64, invest: bool) -> Result<(), String> {
        let accounts = tender::accounts::Rebalance {
            allocator: self.allocator.pubkey(),
            reserve: reserve_pda(),
            usdc_mint: self.usdc,
            tbill_mint: self.tbill,
            usdc_vault: usdc_vault(),
            tbill_vault: tbill_vault(),
            allocator_usdc: self.allocator_usdc,
            allocator_tbill: self.allocator_tbill,
            usdc_token_program: spl_token::ID,
            tbill_token_program: spl_token::ID,
        }
        .to_account_metas(None);
        let data = if invest {
            tender::instruction::Invest { tbill_amount }.data()
        } else {
            tender::instruction::Divest { tbill_amount }.data()
        };
        let allocator = self.allocator.insecure_clone();
        self.send(&[Instruction { program_id: tender::ID, accounts, data }], &[&allocator])
    }

    fn update_price(&mut self, new_price: u64) -> Result<(), String> {
        let ix = Instruction {
            program_id: tender::ID,
            accounts: tender::accounts::UpdatePrice {
                price_authority: self.oracle.pubkey(),
                reserve: reserve_pda(),
                usdc_vault: usdc_vault(),
                tbill_vault: tbill_vault(),
            }
            .to_account_metas(None),
            data: tender::instruction::UpdatePrice { new_price }.data(),
        };
        let oracle = self.oracle.insecure_clone();
        self.send(&[ix], &[&oracle])
    }

    fn update_reserve(&mut self, params: ReserveParams) -> Result<(), String> {
        let ix = Instruction {
            program_id: tender::ID,
            accounts: tender::accounts::AdminOnly {
                admin: self.admin.pubkey(),
                reserve: reserve_pda(),
                usdc_vault: usdc_vault(),
                tbill_vault: tbill_vault(),
            }
            .to_account_metas(None),
            data: tender::instruction::UpdateReserve { params }.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    fn collect_fee(&mut self, treasury_usdc: &Pubkey) -> Result<(), String> {
        let ix = Instruction {
            program_id: tender::ID,
            accounts: tender::accounts::CollectProtocolFee {
                reserve: reserve_pda(),
                usdc_mint: self.usdc,
                usdc_vault: usdc_vault(),
                tbill_vault: tbill_vault(),
                treasury_usdc: *treasury_usdc,
                usdc_token_program: spl_token::ID,
            }
            .to_account_metas(None),
            data: tender::instruction::CollectProtocolFee {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin])
    }

    /// Reserve value must always cover every dollar plus counted yield.
    fn assert_solvent(&self) {
        let r = self.reserve();
        let value = self.balance(&usdc_vault()) as u128
            + self.balance(&tbill_vault()) as u128 * r.tbill_price as u128 / 1_000_000_000;
        assert!(
            value >= (r.total_supply + r.unclaimed_yield) as u128,
            "insolvent: value {value} < supply {} + unclaimed {}",
            r.total_supply,
            r.unclaimed_yield
        );
    }
}

fn setup() -> Env {
    let mut svm = LiteSVM::new();
    let so = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/tender.so");
    svm.add_program_from_file(tender::ID, so).expect("build first: cargo build-sbf");
    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();
    let mut env = Env {
        svm,
        admin,
        oracle: Keypair::new(),
        allocator: Keypair::new(),
        treasury: Keypair::new(),
        usdc: Pubkey::default(),
        tbill: Pubkey::default(),
        allocator_usdc: Pubkey::default(),
        allocator_tbill: Pubkey::default(),
    };
    for k in [env.oracle.pubkey(), env.allocator.pubkey(), env.treasury.pubkey()] {
        env.svm.airdrop(&k, 10_000_000_000).unwrap();
    }
    env.usdc = env.new_mint(&spl_token::ID);
    env.tbill = env.new_mint(&spl_token::ID);
    let (usdc, tbill, alloc) = (env.usdc, env.tbill, env.allocator.pubkey());
    env.allocator_usdc = env.token_account(&usdc, &alloc, &spl_token::ID);
    env.allocator_tbill = env.token_account(&tbill, &alloc, &spl_token::ID);
    env.fund(&tbill, &env.allocator_tbill.clone(), 1_000_000 * USD);
    env.fund(&usdc, &env.allocator_usdc.clone(), 1_000_000 * USD);

    let ix = Instruction {
        program_id: tender::ID,
        accounts: tender::accounts::InitializeReserve {
            admin: env.admin.pubkey(),
            reserve: reserve_pda(),
            usdc_mint: usdc,
            tbill_mint: tbill,
            usdc_vault: usdc_vault(),
            tbill_vault: tbill_vault(),
            usdc_token_program: spl_token::ID,
            tbill_token_program: spl_token::ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: tender::instruction::InitializeReserve { params: env.params() }.data(),
    };
    let admin = env.admin.insecure_clone();
    env.send(&[ix], &[&admin]).unwrap();
    env
}

struct User {
    kp: Keypair,
    usdc: Pubkey,
    dollar: Pubkey,
}

fn user(env: &mut Env, symbol: &str, usdc_amount: u64) -> User {
    let kp = env.wallet();
    let usdc_mint = env.usdc;
    let usdc = env.token_account(&usdc_mint, &kp.pubkey(), &spl_token::ID);
    env.fund(&usdc_mint, &usdc, usdc_amount);
    let dollar = env.token_account(&mint_pda(&issuer_pda(symbol)), &kp.pubkey(), &spl_token_2022::ID);
    User { kp, usdc, dollar }
}

fn mint(env: &mut Env, u: &User, symbol: &str, amount: u64) -> Result<(), String> {
    let ix = env.convert_ix(&u.kp.pubkey(), symbol, &u.usdc, &u.dollar, amount, true);
    env.send(&[ix], &[&u.kp])
}

fn redeem(env: &mut Env, u: &User, symbol: &str, amount: u64) -> Result<(), String> {
    let ix = env.convert_ix(&u.kp.pubkey(), symbol, &u.usdc, &u.dollar, amount, false);
    env.send(&[ix], &[&u.kp])
}

fn usdc_route(wallet: &Pubkey, bps: u16) -> Route {
    Route { wallet: *wallet, bps, kind: ROUTE_USDC }
}

/// Issuer with one USDC route to `payee`.
fn simple_issuer(env: &mut Env, symbol: &str, payee: &Pubkey) -> Keypair {
    let creator = env.wallet();
    env.create_issuer(&creator, &format!("{symbol} dollar"), symbol, vec![usdc_route(payee, 10_000)])
        .unwrap();
    creator
}

// ---------------------------------------------------------------- tests

#[test]
fn issuer_gets_token2022_mint_with_metadata() {
    let mut env = setup();
    let creator = env.wallet();
    env.create_issuer(&creator, "Bonk Dollar", "BONKUSD", vec![usdc_route(&creator.pubkey(), 10_000)])
        .unwrap();

    let issuer = env.issuer("BONKUSD");
    assert_eq!(issuer.authority, creator.pubkey());
    assert_eq!(issuer.symbol, "BONKUSD");
    assert_eq!(issuer.route_count, 1);
    assert_eq!(env.reserve().issuer_count, 1);

    let mint_acct = env.svm.get_account(&mint_pda(&issuer_pda("BONKUSD"))).unwrap();
    assert_eq!(mint_acct.owner, spl_token_2022::ID);
    let data = mint_acct.data;
    // base mint: mint authority = issuer PDA, no freeze authority, 6 decimals
    assert_eq!(&data[4..36], issuer_pda("BONKUSD").as_ref());
    assert_eq!(data[44], 6);
    assert_eq!(&data[46..50], &[0, 0, 0, 0]);
    // on-chain metadata carries name, symbol and uri
    let hay = String::from_utf8_lossy(&data);
    assert!(hay.contains("Bonk Dollar") && hay.contains("BONKUSD") && hay.contains("meta.json"));
    // rent-exempt after the metadata realloc
    assert!(env.svm.get_balance(&mint_pda(&issuer_pda("BONKUSD"))).unwrap()
        >= env.svm.minimum_balance_for_rent_exemption(data.len()));
}

#[test]
fn bad_symbols_names_and_routes_are_rejected() {
    let mut env = setup();
    let c = env.wallet();
    let r = vec![usdc_route(&c.pubkey(), 10_000)];
    for bad in ["usd", "A", "TOOLONGSYMBOL", "AB-C"] {
        assert!(env.create_issuer(&c, "x", bad, r.clone()).is_err(), "{bad}");
    }
    assert!(env.create_issuer(&c, "", "OKUSD", r.clone()).is_err());
    // shares must total 100%
    assert!(env.create_issuer(&c, "x", "OKUSD", vec![usdc_route(&c.pubkey(), 9_999)]).is_err());
    // at most four routes
    assert!(env.create_issuer(&c, "x", "OKUSD", vec![usdc_route(&c.pubkey(), 2_000); 5]).is_err());
    env.create_issuer(&c, "x", "OKUSD", r.clone()).unwrap();
    // symbols are unique
    let c2 = env.wallet();
    assert!(env.create_issuer(&c2, "y", "OKUSD", r).is_err());
}

#[test]
fn mint_and_redeem_at_par() {
    let mut env = setup();
    let payee = env.wallet().pubkey();
    simple_issuer(&mut env, "DAOUSD", &payee);
    let alice = user(&mut env, "DAOUSD", 1_000 * USD);

    mint(&mut env, &alice, "DAOUSD", 1_000 * USD).unwrap();
    assert_eq!(env.balance(&alice.dollar), 1_000 * USD);
    assert_eq!(env.balance(&alice.usdc), 0);
    assert_eq!(env.balance(&usdc_vault()), 1_000 * USD);
    assert_eq!(env.issuer("DAOUSD").supply, 1_000 * USD);
    assert_eq!(env.reserve().total_supply, 1_000 * USD);

    redeem(&mut env, &alice, "DAOUSD", 400 * USD).unwrap();
    assert_eq!(env.balance(&alice.dollar), 600 * USD);
    assert_eq!(env.balance(&alice.usdc), 400 * USD);
    assert_eq!(env.reserve().total_supply, 600 * USD);
    // can't redeem more than you hold
    assert!(redeem(&mut env, &alice, "DAOUSD", 601 * USD).is_err());
    // can't mint without the USDC
    assert!(mint(&mut env, &alice, "DAOUSD", 401 * USD).is_err());
    assert!(mint(&mut env, &alice, "DAOUSD", 0).is_err());
    env.assert_solvent();
}

#[test]
fn cannot_redeem_one_issuers_dollar_through_another() {
    let mut env = setup();
    let payee = env.wallet().pubkey();
    simple_issuer(&mut env, "AAUSD", &payee);
    simple_issuer(&mut env, "BBUSD", &payee);
    let alice = user(&mut env, "AAUSD", 100 * USD);
    mint(&mut env, &alice, "AAUSD", 100 * USD).unwrap();
    // pass AAUSD tokens against the BBUSD issuer
    let ix = env.convert_ix(&alice.kp.pubkey(), "BBUSD", &alice.usdc, &alice.dollar, 100 * USD, false);
    assert!(env.send(&[ix], &[&alice.kp]).is_err());
}

#[test]
fn yield_is_split_by_supply_and_routed() {
    let mut env = setup();
    let route_a = env.wallet();
    let holders = env.wallet();
    // A: 60% USDC to a wallet, 40% minted as A's own dollar to another wallet
    let creator_a = env.wallet();
    env.create_issuer(
        &creator_a,
        "A dollar",
        "AUSD",
        vec![
            usdc_route(&route_a.pubkey(), 6_000),
            Route { wallet: holders.pubkey(), bps: 4_000, kind: ROUTE_DOLLAR },
        ],
    )
    .unwrap();
    let route_b = env.wallet().pubkey();
    simple_issuer(&mut env, "BUSD", &route_b);

    let alice = user(&mut env, "AUSD", 1_000 * USD);
    let bob = user(&mut env, "BUSD", 3_000 * USD);
    mint(&mut env, &alice, "AUSD", 1_000 * USD).unwrap();
    mint(&mut env, &bob, "BUSD", 3_000 * USD).unwrap();

    // Allocator moves 2,000 USDC into Treasuries at $1.00
    env.rebalance(2_000 * USD, true).unwrap();
    assert_eq!(env.balance(&usdc_vault()), 2_000 * USD);
    assert_eq!(env.balance(&tbill_vault()), 2_000 * USD);

    // An hour later the Treasury token is worth 0.5% more: +10 USDC
    env.warp(3_600);
    env.update_price(PRICE_ONE + PRICE_ONE / 200).unwrap();

    let usdc_mint = env.usdc;
    let a_usdc = env.token_account(&usdc_mint, &route_a.pubkey(), &spl_token::ID);
    let a_dollar = env.token_account(&mint_pda(&issuer_pda("AUSD")), &holders.pubkey(), &spl_token_2022::ID);
    let b_usdc = env.token_account(&usdc_mint, &route_b, &spl_token::ID);
    let keeper = env.wallet();

    // 10 USDC of yield, 10% to protocol, 9 to issuers split 1:3
    env.harvest(&keeper, "AUSD", &[a_usdc, a_dollar]).unwrap();
    env.harvest(&keeper, "BUSD", &[b_usdc]).unwrap();
    assert_eq!(env.balance(&a_usdc), 1_350_000); // 2.25 x 60%
    assert_eq!(env.balance(&a_dollar), 900_000); // 2.25 x 40%, as AUSD
    assert_eq!(env.balance(&b_usdc), 6_750_000);
    assert_eq!(env.issuer("AUSD").supply, 1_000 * USD + 900_000);
    assert_eq!(env.issuer("AUSD").total_yield_paid, 2_250_000);
    let r = env.reserve();
    assert_eq!(r.protocol_owed, 1_000_000);
    assert_eq!(r.total_yield, 10 * USD);

    // nothing left to harvest
    assert!(env.harvest(&keeper, "BUSD", &[b_usdc]).is_err());

    let treasury = env.treasury.pubkey();
    let t_usdc = env.token_account(&usdc_mint, &treasury, &spl_token::ID);
    env.collect_fee(&t_usdc).unwrap();
    assert_eq!(env.balance(&t_usdc), 1_000_000);
    env.assert_solvent();

    // Every dollar is still redeemable at par once the allocator divests.
    env.rebalance(2_000 * USD, false).unwrap();
    redeem(&mut env, &bob, "BUSD", 3_000 * USD).unwrap();
    redeem(&mut env, &alice, "AUSD", 1_000 * USD).unwrap();
    assert_eq!(env.balance(&bob.usdc), 3_000 * USD);
    env.assert_solvent();
}

#[test]
fn late_minters_do_not_earn_earlier_yield() {
    let mut env = setup();
    let pa = env.wallet().pubkey();
    let pb = env.wallet().pubkey();
    simple_issuer(&mut env, "EARLY", &pa);
    simple_issuer(&mut env, "LATE", &pb);
    let alice = user(&mut env, "EARLY", 1_000 * USD);
    let bob = user(&mut env, "LATE", 1_000 * USD);
    mint(&mut env, &alice, "EARLY", 1_000 * USD).unwrap();

    // 100 USDC of yield arrives (e.g. a coupon paid into the reserve)
    let usdc_mint = env.usdc;
    env.fund(&usdc_mint, &usdc_vault(), 100 * USD);
    // Bob mints after it: the mint counts the yield before adding his supply
    mint(&mut env, &bob, "LATE", 1_000 * USD).unwrap();

    let a = env.token_account(&usdc_mint, &pa, &spl_token::ID);
    let b = env.token_account(&usdc_mint, &pb, &spl_token::ID);
    let keeper = env.wallet();
    env.harvest(&keeper, "EARLY", &[a]).unwrap();
    assert_eq!(env.balance(&a), 90 * USD); // 100 minus 10% protocol fee
    assert!(env.harvest(&keeper, "LATE", &[b]).is_err());

    // Next 100 is split evenly
    env.fund(&usdc_mint, &usdc_vault(), 100 * USD);
    env.harvest(&keeper, "EARLY", &[a]).unwrap();
    env.harvest(&keeper, "LATE", &[b]).unwrap();
    assert_eq!(env.balance(&a), 135 * USD);
    assert_eq!(env.balance(&b), 45 * USD);
    env.assert_solvent();
}

#[test]
fn harvest_only_pays_the_routes() {
    let mut env = setup();
    let payee = env.wallet();
    simple_issuer(&mut env, "ROUTE", &payee.pubkey());
    let alice = user(&mut env, "ROUTE", 100 * USD);
    mint(&mut env, &alice, "ROUTE", 100 * USD).unwrap();
    let usdc_mint = env.usdc;
    env.fund(&usdc_mint, &usdc_vault(), 10 * USD);

    let thief = env.wallet();
    let thief_usdc = env.token_account(&usdc_mint, &thief.pubkey(), &spl_token::ID);
    assert!(env.harvest(&thief, "ROUTE", &[thief_usdc]).is_err());
    assert!(env.harvest(&thief, "ROUTE", &[]).is_err());
    // dollar account for a USDC route is rejected too
    let wrong_mint = env.token_account(&mint_pda(&issuer_pda("ROUTE")), &payee.pubkey(), &spl_token_2022::ID);
    assert!(env.harvest(&thief, "ROUTE", &[wrong_mint]).is_err());

    let good = env.token_account(&usdc_mint, &payee.pubkey(), &spl_token::ID);
    env.harvest(&thief, "ROUTE", &[good]).unwrap();
    assert_eq!(env.balance(&good), 9 * USD);
}

#[test]
fn only_the_issuer_authority_can_change_routes() {
    let mut env = setup();
    let payee = env.wallet().pubkey();
    let creator = simple_issuer(&mut env, "OWNED", &payee);
    let stranger = env.wallet();
    let set = |who: &Keypair, wallet: Pubkey| Instruction {
        program_id: tender::ID,
        accounts: tender::accounts::IssuerAuthority { authority: who.pubkey(), issuer: issuer_pda("OWNED") }
            .to_account_metas(None),
        data: tender::instruction::SetRoutes { routes: vec![usdc_route(&wallet, 10_000)] }.data(),
    };
    assert!(env.send(&[set(&stranger, stranger.pubkey())], &[&stranger]).is_err());
    let new_payee = Pubkey::new_unique();
    env.send(&[set(&creator, new_payee)], &[&creator]).unwrap();
    assert_eq!(env.issuer("OWNED").routes[0].wallet, new_payee);
}

#[test]
fn allocator_must_leave_the_liquidity_floor() {
    let mut env = setup();
    let payee = env.wallet().pubkey();
    simple_issuer(&mut env, "FLOOR", &payee);
    let alice = user(&mut env, "FLOOR", 1_000 * USD);
    mint(&mut env, &alice, "FLOOR", 1_000 * USD).unwrap();
    // floor is 20% of 1,000 = 200 liquid USDC
    assert!(env.rebalance(800 * USD + 1, true).is_err());
    env.rebalance(800 * USD, true).unwrap();
    // reserve value unchanged by the swap
    env.assert_solvent();
    assert_eq!(env.reserve().unclaimed_yield, 0);

    // Redemptions beyond the liquid USDC fail cleanly until divested
    redeem(&mut env, &alice, "FLOOR", 200 * USD).unwrap();
    assert!(redeem(&mut env, &alice, "FLOOR", 1).is_err());
    env.rebalance(800 * USD, false).unwrap();
    redeem(&mut env, &alice, "FLOOR", 800 * USD).unwrap();
    assert_eq!(env.balance(&alice.usdc), 1_000 * USD);
    env.assert_solvent();
}

#[test]
fn price_updates_are_bounded_and_rate_limited() {
    let mut env = setup();
    // too soon after init
    assert!(env.update_price(PRICE_ONE + 1).is_err());
    env.warp(3_600);
    // more than 0.5%
    assert!(env.update_price(PRICE_ONE + PRICE_ONE / 200 + 1).is_err());
    assert!(env.update_price(PRICE_ONE - PRICE_ONE / 200 - 1).is_err());
    env.update_price(PRICE_ONE - PRICE_ONE / 200).unwrap();
    assert!(env.update_price(PRICE_ONE).is_err()); // within the hour
    // wrong signer
    let ix = Instruction {
        program_id: tender::ID,
        accounts: tender::accounts::UpdatePrice {
            price_authority: env.admin.pubkey(),
            reserve: reserve_pda(),
            usdc_vault: usdc_vault(),
            tbill_vault: tbill_vault(),
        }
        .to_account_metas(None),
        data: tender::instruction::UpdatePrice { new_price: PRICE_ONE }.data(),
    };
    env.warp(3_600);
    let admin = env.admin.insecure_clone();
    assert!(env.send(&[ix], &[&admin]).is_err());
}

#[test]
fn price_drop_pauses_yield_instead_of_breaking_par() {
    let mut env = setup();
    let payee = env.wallet().pubkey();
    simple_issuer(&mut env, "DIP", &payee);
    let alice = user(&mut env, "DIP", 1_000 * USD);
    mint(&mut env, &alice, "DIP", 1_000 * USD).unwrap();
    env.rebalance(500 * USD, true).unwrap();
    env.warp(3_600);
    env.update_price(PRICE_ONE - PRICE_ONE / 200).unwrap(); // -2.5 USDC
    let usdc_mint = env.usdc;
    let p = env.token_account(&usdc_mint, &payee, &spl_token::ID);
    let keeper = env.wallet();
    // a 5 USDC coupon first refills the 2.5 hole; only the rest is yield
    env.fund(&usdc_mint, &usdc_vault(), 5 * USD);
    env.harvest(&keeper, "DIP", &[p]).unwrap();
    assert_eq!(env.balance(&p), 2_250_000); // 2.5 x 90%
    env.assert_solvent();
}

#[test]
fn pause_stops_mints_but_never_redemptions() {
    let mut env = setup();
    let payee = env.wallet().pubkey();
    simple_issuer(&mut env, "PAUSE", &payee);
    let alice = user(&mut env, "PAUSE", 100 * USD);
    mint(&mut env, &alice, "PAUSE", 50 * USD).unwrap();
    let mut p = env.params();
    p.paused = true;
    env.update_reserve(p.clone()).unwrap();
    assert!(mint(&mut env, &alice, "PAUSE", 50 * USD).is_err());
    redeem(&mut env, &alice, "PAUSE", 50 * USD).unwrap();
    // fee cap
    p.paused = false;
    p.protocol_fee_bps = 2_001;
    assert!(env.update_reserve(p).is_err());
}

#[test]
fn fee_change_does_not_reach_back() {
    let mut env = setup();
    let payee = env.wallet().pubkey();
    simple_issuer(&mut env, "FEE", &payee);
    let alice = user(&mut env, "FEE", 100 * USD);
    mint(&mut env, &alice, "FEE", 100 * USD).unwrap();
    let usdc_mint = env.usdc;
    env.fund(&usdc_mint, &usdc_vault(), 10 * USD); // earned under 10% fee
    let mut p = env.params();
    p.protocol_fee_bps = 2_000;
    env.update_reserve(p).unwrap();
    let a = env.token_account(&usdc_mint, &payee, &spl_token::ID);
    let keeper = env.wallet();
    env.harvest(&keeper, "FEE", &[a]).unwrap();
    assert_eq!(env.balance(&a), 9 * USD);
}

#[test]
fn many_conversions_stay_solvent() {
    let mut env = setup();
    let payees: Vec<Pubkey> = (0..3).map(|_| env.wallet().pubkey()).collect();
    let syms = ["XUSD", "YUSD", "ZUSD"];
    for (s, p) in syms.iter().zip(&payees) {
        simple_issuer(&mut env, s, p);
    }
    let users: Vec<User> = syms.iter().map(|s| user(&mut env, s, 10_000 * USD)).collect();
    let usdc_mint = env.usdc;
    let routes: Vec<Pubkey> = payees.iter().map(|p| env.token_account(&usdc_mint, p, &spl_token::ID)).collect();
    let keeper = env.wallet();
    let mut seed: u64 = 7;
    let mut rnd = |m: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % m
    };
    for round in 0..60 {
        let i = rnd(3) as usize;
        let held = env.balance(&users[i].dollar);
        match rnd(4) {
            0 | 1 => {
                let amt = 1 + rnd(1_000 * USD);
                if env.balance(&users[i].usdc) >= amt {
                    mint(&mut env, &users[i], syms[i], amt).unwrap();
                }
            }
            2 if held > 0 => {
                let amt = 1 + rnd(held);
                if env.balance(&usdc_vault()) >= amt {
                    redeem(&mut env, &users[i], syms[i], amt).unwrap();
                }
            }
            _ => env.fund(&usdc_mint, &usdc_vault(), 1 + rnd(3 * USD)),
        }
        if round % 7 == 0 {
            let _ = env.harvest(&keeper, syms[i], &[routes[i]]);
        }
        env.assert_solvent();
    }
    for i in 0..3 {
        let _ = env.harvest(&keeper, syms[i], &[routes[i]]);
        let held = env.balance(&users[i].dollar);
        if held > 0 {
            redeem(&mut env, &users[i], syms[i], held).unwrap();
        }
        assert_eq!(env.issuer(syms[i]).supply, 0);
    }
    env.assert_solvent();
    assert_eq!(env.reserve().total_supply, 0);
}
