//! Tender, written without Anchor so the deployed program is small (and so
//! cheap to deploy). It is wire-compatible with `programs/tender`: the same
//! 8-byte instruction discriminators, Borsh arguments, account order, account
//! layouts and error codes. The Anchor crate's integration tests run against
//! this binary unchanged, and the site's client works with either.
//!
//! Every account check Anchor performed for the original is done by hand
//! below: signers, writability, owners, PDAs, `has_one` links, and token
//! account / mint ownership.

use solana_program::{
    log::sol_log,
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
    rent::Rent,
    system_program,
    sysvar::Sysvar,
};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

/// A panic aborts the transaction either way; skipping the default handler
/// keeps Rust's formatting code out of the binary.
#[cfg(all(target_os = "solana", feature = "custom-panic", not(feature = "no-entrypoint")))]
#[no_mangle]
fn custom_panic(_info: &core::panic::PanicInfo<'_>) {
    sol_log("Error Message: program panicked.");
}

// ---------------------------------------------------------------- constants

pub const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

const PRICE_SCALE: u128 = 1_000_000_000;
const INDEX_SCALE: u128 = 1_000_000_000_000_000_000;
const BPS: u128 = 10_000;
const DOLLAR_DECIMALS: u8 = 6;
const MAX_ROUTES: usize = 4;
const MAX_NAME_LEN: usize = 32;
const MAX_SYMBOL_LEN: usize = 10;
const MAX_URI_LEN: usize = 200;
const MAX_PROTOCOL_FEE_BPS: u16 = 2_000;
const MAX_PRICE_MOVE_BPS: u128 = 50;
const MIN_PRICE_INTERVAL: i64 = 3_600;
const ROUTE_USDC: u8 = 0;
const ROUTE_DOLLAR: u8 = 1;

const RESERVE_SEED: &[u8] = b"reserve";
const USDC_VAULT_SEED: &[u8] = b"usdc_vault";
const TBILL_VAULT_SEED: &[u8] = b"tbill_vault";
const ISSUER_SEED: &[u8] = b"issuer";
const MINT_SEED: &[u8] = b"mint";

/// sha256("account:Reserve")[..8] / sha256("account:Issuer")[..8], as Anchor writes them.
const RESERVE_DISC: [u8; 8] = [43, 242, 204, 202, 26, 247, 59, 127];
const ISSUER_DISC: [u8; 8] = [216, 19, 83, 230, 108, 53, 80, 14];
/// 8 + Reserve::INIT_SPACE and 8 + Issuer::INIT_SPACE in the Anchor program.
const RESERVE_LEN: usize = 8 + 342;
const ISSUER_LEN: usize = 8 + 336;
const TOKEN_ACCOUNT_LEN: usize = 165;
/// Token-2022 mint with the MetadataPointer extension (165 + 1 + 2 + 2 + 64).
const MINT_WITH_POINTER_LEN: usize = 234;

/// sha256("global:<name>")[..8]
mod ix {
    pub const INITIALIZE_RESERVE: [u8; 8] = [91, 188, 92, 135, 153, 155, 112, 16];
    pub const UPDATE_RESERVE: [u8; 8] = [46, 250, 249, 240, 166, 54, 22, 86];
    pub const SET_ADMIN: [u8; 8] = [251, 163, 0, 52, 91, 194, 187, 92];
    pub const CREATE_ISSUER: [u8; 8] = [251, 204, 188, 37, 232, 43, 39, 218];
    pub const SET_ROUTES: [u8; 8] = [95, 172, 119, 157, 123, 127, 91, 208];
    pub const SET_ISSUER_AUTHORITY: [u8; 8] = [254, 251, 236, 139, 54, 58, 91, 221];
    pub const MINT: [u8; 8] = [51, 57, 225, 47, 182, 146, 137, 166];
    pub const REDEEM: [u8; 8] = [184, 12, 86, 149, 70, 196, 97, 225];
    pub const HARVEST: [u8; 8] = [228, 241, 31, 182, 53, 169, 59, 199];
    pub const COLLECT_PROTOCOL_FEE: [u8; 8] = [136, 136, 252, 221, 194, 66, 126, 89];
    pub const UPDATE_PRICE: [u8; 8] = [61, 34, 117, 155, 75, 34, 123, 208];
    pub const INVEST: [u8; 8] = [13, 245, 180, 103, 254, 182, 121, 4];
    pub const DIVEST: [u8; 8] = [196, 19, 245, 104, 6, 82, 109, 77];
}

/// sha256("spl_token_metadata_interface:initialize_account")[..8]
const TOKEN_METADATA_INIT: [u8; 8] = [210, 225, 30, 162, 88, 184, 77, 141];

// ---------------------------------------------------------------- errors

/// Same codes (6000 + index) and messages as the Anchor program's `TenderError`.
#[derive(Clone, Copy)]
#[repr(u32)]
enum E {
    BadSymbol = 6000,
    BadName,
    BadUri,
    BadRoutes,
    BadRouteAccounts,
    ZeroAmount,
    Paused,
    InsufficientLiquidity,
    BelowLiquidityFloor,
    NothingToHarvest,
    BadPrice,
    PriceTooSoon,
    FeeTooHigh,
    BadParams,
    Overflow,
    CapReached,
}

impl E {
    fn msg(self) -> &'static str {
        match self {
            E::BadSymbol => "Error Message: Symbol must be 2-10 characters, A-Z and 0-9 only.",
            E::BadName => "Error Message: Name must be 1-32 characters.",
            E::BadUri => "Error Message: Metadata URI is too long.",
            E::BadRoutes => "Error Message: Routes must be 1-4 wallets whose shares add up to 100%.",
            E::BadRouteAccounts => "Error Message: Route token accounts are missing or don't match the routes.",
            E::ZeroAmount => "Error Message: Amount must be greater than zero.",
            E::Paused => "Error Message: Minting is paused.",
            E::InsufficientLiquidity => "Error Message: The reserve doesn't hold enough liquid USDC right now.",
            E::BelowLiquidityFloor => "Error Message: That would leave less liquid USDC than the reserve's floor.",
            E::NothingToHarvest => "Error Message: No yield to pay out yet.",
            E::BadPrice => "Error Message: Price must be positive and move at most 0.5% per update.",
            E::PriceTooSoon => "Error Message: The price was updated less than an hour ago.",
            E::FeeTooHigh => "Error Message: Protocol fee is above the 20% cap.",
            E::BadParams => "Error Message: Invalid reserve parameters.",
            E::Overflow => "Error Message: Arithmetic overflow.",
            E::CapReached => "Error Message: This would take total supply over the reserve's cap.",
        }
    }
}

impl From<E> for ProgramError {
    fn from(e: E) -> Self {
        sol_log(e.msg());
        ProgramError::Custom(e as u32)
    }
}

fn require(cond: bool, e: E) -> ProgramResult {
    if cond { Ok(()) } else { Err(e.into()) }
}

fn check(cond: bool, what: &str) -> ProgramResult {
    if cond {
        Ok(())
    } else {
        sol_log(what);
        Err(ProgramError::InvalidAccountData)
    }
}

// ---------------------------------------------------------------- byte reading / writing

struct Rd<'a> {
    d: &'a [u8],
    o: usize,
}

impl<'a> Rd<'a> {
    fn new(d: &'a [u8]) -> Self { Self { d, o: 0 } }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProgramError> {
        let end = self.o.checked_add(n).ok_or(ProgramError::InvalidInstructionData)?;
        let s = self.d.get(self.o..end).ok_or(ProgramError::InvalidInstructionData)?;
        self.o = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ProgramError> { Ok(self.take(1)?[0]) }
    fn bool(&mut self) -> Result<bool, ProgramError> {
        match self.u8()? { 0 => Ok(false), 1 => Ok(true), _ => Err(ProgramError::InvalidInstructionData) }
    }
    fn u16(&mut self) -> Result<u16, ProgramError> { Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap())) }
    fn u32(&mut self) -> Result<u32, ProgramError> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn u64(&mut self) -> Result<u64, ProgramError> { Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap())) }
    fn i64(&mut self) -> Result<i64, ProgramError> { Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap())) }
    fn u128(&mut self) -> Result<u128, ProgramError> { Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap())) }
    fn key(&mut self) -> Result<Pubkey, ProgramError> { Ok(Pubkey::new_from_array(self.take(32)?.try_into().unwrap())) }
    fn string(&mut self, max: usize) -> Result<String, ProgramError> {
        let n = self.u32()? as usize;
        if n > max { return Err(ProgramError::InvalidInstructionData); }
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| ProgramError::InvalidInstructionData)
    }
    fn route(&mut self) -> Result<Route, ProgramError> {
        Ok(Route { wallet: self.key()?, bps: self.u16()?, kind: self.u8()? })
    }
    fn routes(&mut self) -> Result<Vec<Route>, ProgramError> {
        let n = self.u32()? as usize;
        if n > MAX_ROUTES { return Err(E::BadRoutes.into()); }
        (0..n).map(|_| self.route()).collect()
    }
}

#[derive(Default)]
struct Wr(Vec<u8>);

impl Wr {
    fn bytes(&mut self, b: &[u8]) -> &mut Self { self.0.extend_from_slice(b); self }
    fn u8(&mut self, v: u8) -> &mut Self { self.bytes(&[v]) }
    fn u16(&mut self, v: u16) -> &mut Self { self.bytes(&v.to_le_bytes()) }
    fn u32(&mut self, v: u32) -> &mut Self { self.bytes(&v.to_le_bytes()) }
    fn u64(&mut self, v: u64) -> &mut Self { self.bytes(&v.to_le_bytes()) }
    fn i64(&mut self, v: i64) -> &mut Self { self.bytes(&v.to_le_bytes()) }
    fn u128(&mut self, v: u128) -> &mut Self { self.bytes(&v.to_le_bytes()) }
    fn key(&mut self, k: &Pubkey) -> &mut Self { self.bytes(k.as_ref()) }
    fn string(&mut self, s: &str) -> &mut Self { self.u32(s.len() as u32).bytes(s.as_bytes()) }
}

// ---------------------------------------------------------------- state

#[derive(Clone, Copy, Default)]
struct Route {
    wallet: Pubkey,
    bps: u16,
    kind: u8,
}

struct Params {
    price_authority: Pubkey,
    allocator: Pubkey,
    treasury: Pubkey,
    protocol_fee_bps: u16,
    min_liquid_bps: u16,
    paused: bool,
    tbill_price: u64,
    max_supply: u64,
}

impl Params {
    fn read(r: &mut Rd) -> Result<Self, ProgramError> {
        Ok(Self {
            price_authority: r.key()?,
            allocator: r.key()?,
            treasury: r.key()?,
            protocol_fee_bps: r.u16()?,
            min_liquid_bps: r.u16()?,
            paused: r.bool()?,
            tbill_price: r.u64()?,
            max_supply: r.u64()?,
        })
    }
    fn validate(&self) -> ProgramResult {
        require(self.protocol_fee_bps <= MAX_PROTOCOL_FEE_BPS, E::FeeTooHigh)?;
        require(self.min_liquid_bps as u128 <= BPS, E::BadParams)
    }
}

struct Reserve {
    admin: Pubkey,
    price_authority: Pubkey,
    allocator: Pubkey,
    treasury: Pubkey,
    usdc_mint: Pubkey,
    tbill_mint: Pubkey,
    usdc_vault: Pubkey,
    tbill_vault: Pubkey,
    tbill_price: u64,
    price_updated_at: i64,
    protocol_fee_bps: u16,
    min_liquid_bps: u16,
    paused: bool,
    total_supply: u64,
    yield_index: u128,
    unclaimed_yield: u64,
    protocol_owed: u64,
    total_yield: u64,
    issuer_count: u64,
    bump: u8,
    max_supply: u64,
}

impl Reserve {
    fn read(d: &[u8]) -> Result<Self, ProgramError> {
        let mut r = Rd::new(&d[8..]);
        Ok(Self {
            admin: r.key()?,
            price_authority: r.key()?,
            allocator: r.key()?,
            treasury: r.key()?,
            usdc_mint: r.key()?,
            tbill_mint: r.key()?,
            usdc_vault: r.key()?,
            tbill_vault: r.key()?,
            tbill_price: r.u64()?,
            price_updated_at: r.i64()?,
            protocol_fee_bps: r.u16()?,
            min_liquid_bps: r.u16()?,
            paused: r.u8()? != 0,
            total_supply: r.u64()?,
            yield_index: r.u128()?,
            unclaimed_yield: r.u64()?,
            protocol_owed: r.u64()?,
            total_yield: r.u64()?,
            issuer_count: r.u64()?,
            bump: r.u8()?,
            max_supply: r.u64()?,
        })
    }

    fn write(&self, d: &mut [u8]) {
        let mut w = Wr::default();
        w.bytes(&RESERVE_DISC)
            .key(&self.admin).key(&self.price_authority).key(&self.allocator).key(&self.treasury)
            .key(&self.usdc_mint).key(&self.tbill_mint).key(&self.usdc_vault).key(&self.tbill_vault)
            .u64(self.tbill_price).i64(self.price_updated_at)
            .u16(self.protocol_fee_bps).u16(self.min_liquid_bps).u8(self.paused as u8)
            .u64(self.total_supply).u128(self.yield_index).u64(self.unclaimed_yield)
            .u64(self.protocol_owed).u64(self.total_yield).u64(self.issuer_count)
            .u8(self.bump).u64(self.max_supply);
        d[..w.0.len()].copy_from_slice(&w.0);
    }

    fn apply(&mut self, p: &Params) {
        self.price_authority = p.price_authority;
        self.allocator = p.allocator;
        self.treasury = p.treasury;
        self.protocol_fee_bps = p.protocol_fee_bps;
        self.min_liquid_bps = p.min_liquid_bps;
        self.paused = p.paused;
        self.max_supply = p.max_supply;
    }

    fn value(&self, usdc: u64, tbill: u64) -> Result<u64, ProgramError> {
        usdc.checked_add(tbill_value(tbill, self.tbill_price, false)?).ok_or_else(|| E::Overflow.into())
    }

    /// Counts any value above what the reserve owes as new yield.
    fn accrue(&mut self, usdc: u64, tbill: u64) -> ProgramResult {
        if self.total_supply == 0 {
            return Ok(());
        }
        let value = self.value(usdc, tbill)?;
        let owed = self.total_supply.checked_add(self.unclaimed_yield).ok_or(E::Overflow)?;
        if value <= owed {
            return Ok(());
        }
        let surplus = value - owed;
        let fee = (surplus as u128 * self.protocol_fee_bps as u128 / BPS) as u64;
        let to_issuers = surplus - fee;
        let delta = to_issuers as u128 * INDEX_SCALE / self.total_supply as u128;
        if delta == 0 && fee == 0 {
            return Ok(());
        }
        let counted_for_issuers = if delta == 0 { 0 } else { to_issuers };
        self.yield_index = self.yield_index.checked_add(delta).ok_or(E::Overflow)?;
        self.protocol_owed = self.protocol_owed.checked_add(fee).ok_or(E::Overflow)?;
        let counted = counted_for_issuers + fee;
        self.unclaimed_yield = self.unclaimed_yield.checked_add(counted).ok_or(E::Overflow)?;
        self.total_yield = self.total_yield.saturating_add(counted);
        Ok(())
    }
}

struct Issuer {
    reserve: Pubkey,
    authority: Pubkey,
    mint: Pubkey,
    name: String,
    symbol: String,
    supply: u64,
    index_snapshot: u128,
    accrued: u64,
    total_yield_paid: u64,
    routes: [Route; MAX_ROUTES],
    route_count: u8,
    created_at: i64,
    bump: u8,
}

impl Issuer {
    fn read(d: &[u8]) -> Result<Self, ProgramError> {
        let mut r = Rd::new(&d[8..]);
        let reserve = r.key()?;
        let authority = r.key()?;
        let mint = r.key()?;
        let name = r.string(MAX_NAME_LEN)?;
        let symbol = r.string(MAX_SYMBOL_LEN)?;
        let supply = r.u64()?;
        let index_snapshot = r.u128()?;
        let accrued = r.u64()?;
        let total_yield_paid = r.u64()?;
        let mut routes = [Route::default(); MAX_ROUTES];
        for route in routes.iter_mut() {
            *route = r.route()?;
        }
        Ok(Self {
            reserve, authority, mint, name, symbol, supply, index_snapshot, accrued, total_yield_paid, routes,
            route_count: r.u8()?,
            created_at: r.i64()?,
            bump: r.u8()?,
        })
    }

    fn write(&self, d: &mut [u8]) -> ProgramResult {
        let mut w = Wr::default();
        w.bytes(&ISSUER_DISC).key(&self.reserve).key(&self.authority).key(&self.mint)
            .string(&self.name).string(&self.symbol)
            .u64(self.supply).u128(self.index_snapshot).u64(self.accrued).u64(self.total_yield_paid);
        for r in &self.routes {
            w.key(&r.wallet).u16(r.bps).u8(r.kind);
        }
        w.u8(self.route_count).i64(self.created_at).u8(self.bump);
        if w.0.len() > d.len() {
            return Err(ProgramError::AccountDataTooSmall);
        }
        d[..w.0.len()].copy_from_slice(&w.0);
        Ok(())
    }

    fn settle(&mut self, index: u128) -> ProgramResult {
        let earned = self.supply as u128 * (index - self.index_snapshot) / INDEX_SCALE;
        self.accrued = self.accrued.checked_add(u64::try_from(earned).map_err(|_| E::Overflow)?).ok_or(E::Overflow)?;
        self.index_snapshot = index;
        Ok(())
    }

    fn set_routes(&mut self, routes: &[Route]) {
        self.routes = [Route::default(); MAX_ROUTES];
        self.routes[..routes.len()].copy_from_slice(routes);
        self.route_count = routes.len() as u8;
    }

    fn seeds_bump(&self) -> [u8; 1] { [self.bump] }
}

fn tbill_value(amount: u64, price: u64, round_up: bool) -> Result<u64, ProgramError> {
    let num = amount as u128 * price as u128;
    let v = if round_up { num.div_ceil(PRICE_SCALE) } else { num / PRICE_SCALE };
    u64::try_from(v).map_err(|_| E::Overflow.into())
}

fn validate_symbol(s: &str) -> ProgramResult {
    require(
        (2..=MAX_SYMBOL_LEN).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
        E::BadSymbol,
    )
}

fn validate_routes(routes: &[Route]) -> ProgramResult {
    require(!routes.is_empty() && routes.len() <= MAX_ROUTES, E::BadRoutes)?;
    let mut total: u32 = 0;
    for r in routes {
        require(r.bps > 0, E::BadRoutes)?;
        require(r.kind == ROUTE_USDC || r.kind == ROUTE_DOLLAR, E::BadRoutes)?;
        require(r.wallet != Pubkey::default(), E::BadRoutes)?;
        total += r.bps as u32;
    }
    require(total == BPS as u32, E::BadRoutes)
}

// ---------------------------------------------------------------- account checks

fn signer(a: &AccountInfo) -> ProgramResult {
    if a.is_signer { Ok(()) } else { Err(ProgramError::MissingRequiredSignature) }
}

fn writable(a: &AccountInfo) -> ProgramResult {
    check(a.is_writable, "Error Message: An account that must be writable is not.")
}

fn is_token_program(k: &Pubkey) -> bool { *k == TOKEN || *k == TOKEN_2022 }

fn token_program(a: &AccountInfo) -> ProgramResult {
    if is_token_program(a.key) { Ok(()) } else { Err(ProgramError::IncorrectProgramId) }
}

/// Loads the reserve and checks it is this program's one reserve PDA.
fn load_reserve(program_id: &Pubkey, a: &AccountInfo) -> Result<Reserve, ProgramError> {
    check(a.owner == program_id, "Error Message: Reserve is not owned by this program.")?;
    let d = a.try_borrow_data()?;
    check(d.len() >= RESERVE_LEN && d[..8] == RESERVE_DISC, "Error Message: Not a reserve account.")?;
    let r = Reserve::read(&d)?;
    let pda = Pubkey::create_program_address(&[RESERVE_SEED, &[r.bump]], program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    check(pda == *a.key, "Error Message: Wrong reserve address.")?;
    Ok(r)
}

fn save_reserve(a: &AccountInfo, r: &Reserve) -> ProgramResult {
    r.write(&mut a.try_borrow_mut_data()?);
    Ok(())
}

fn load_issuer(program_id: &Pubkey, a: &AccountInfo) -> Result<Issuer, ProgramError> {
    check(a.owner == program_id, "Error Message: Issuer is not owned by this program.")?;
    let d = a.try_borrow_data()?;
    check(d.len() >= ISSUER_LEN && d[..8] == ISSUER_DISC, "Error Message: Not an issuer account.")?;
    Issuer::read(&d)
}

fn save_issuer(a: &AccountInfo, i: &Issuer) -> ProgramResult {
    i.write(&mut a.try_borrow_mut_data()?)
}

struct TokenAcct {
    mint: Pubkey,
    owner: Pubkey,
    amount: u64,
}

/// An initialized SPL Token or Token-2022 account. `program`, when given,
/// must be the program that owns it.
fn token_account(a: &AccountInfo, program: Option<&Pubkey>) -> Result<TokenAcct, ProgramError> {
    check(is_token_program(a.owner), "Error Message: Not a token account.")?;
    if let Some(p) = program {
        check(a.owner == p, "Error Message: Token account belongs to a different token program.")?;
    }
    let d = a.try_borrow_data()?;
    check(d.len() >= TOKEN_ACCOUNT_LEN, "Error Message: Not a token account.")?;
    // state at 108: 0 = uninitialized; extended Token-2022 accounts carry type 2 at 165
    check(d[108] != 0 && (d.len() == TOKEN_ACCOUNT_LEN || d[165] == 2), "Error Message: Not a token account.")?;
    Ok(TokenAcct {
        mint: Pubkey::new_from_array(d[0..32].try_into().unwrap()),
        owner: Pubkey::new_from_array(d[32..64].try_into().unwrap()),
        amount: u64::from_le_bytes(d[64..72].try_into().unwrap()),
    })
}

/// An initialized mint owned by `program`; returns its decimals.
fn mint_decimals(a: &AccountInfo, program: &Pubkey) -> Result<u8, ProgramError> {
    check(a.owner == program, "Error Message: Mint belongs to a different token program.")?;
    let d = a.try_borrow_data()?;
    check(d.len() >= 82 && d[45] == 1 && (d.len() == 82 || d.len() > 165 && d[165] == 1), "Error Message: Not a mint.")?;
    Ok(d[44])
}

// ---------------------------------------------------------------- CPI helpers

fn token_ix(program: &Pubkey, keys: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
    Instruction { program_id: *program, accounts: keys, data }
}

#[allow(clippy::too_many_arguments)]
fn transfer_checked<'a>(
    program: &AccountInfo<'a>, from: &AccountInfo<'a>, mint: &AccountInfo<'a>, to: &AccountInfo<'a>,
    authority: &AccountInfo<'a>, amount: u64, decimals: u8, seeds: &[&[&[u8]]],
) -> ProgramResult {
    let mut data = vec![12u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    let ix = token_ix(program.key, vec![
        AccountMeta::new(*from.key, false),
        AccountMeta::new_readonly(*mint.key, false),
        AccountMeta::new(*to.key, false),
        AccountMeta::new_readonly(*authority.key, true),
    ], data);
    invoke_signed(&ix, &[from.clone(), mint.clone(), to.clone(), authority.clone(), program.clone()], seeds)
}

fn mint_to<'a>(
    program: &AccountInfo<'a>, mint: &AccountInfo<'a>, to: &AccountInfo<'a>, authority: &AccountInfo<'a>,
    amount: u64, seeds: &[&[&[u8]]],
) -> ProgramResult {
    let mut data = vec![7u8];
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = token_ix(program.key, vec![
        AccountMeta::new(*mint.key, false),
        AccountMeta::new(*to.key, false),
        AccountMeta::new_readonly(*authority.key, true),
    ], data);
    invoke_signed(&ix, &[mint.clone(), to.clone(), authority.clone(), program.clone()], seeds)
}

fn burn<'a>(
    program: &AccountInfo<'a>, from: &AccountInfo<'a>, mint: &AccountInfo<'a>, authority: &AccountInfo<'a>, amount: u64,
) -> ProgramResult {
    let mut data = vec![8u8];
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = token_ix(program.key, vec![
        AccountMeta::new(*from.key, false),
        AccountMeta::new(*mint.key, false),
        AccountMeta::new_readonly(*authority.key, true),
    ], data);
    invoke(&ix, &[from.clone(), mint.clone(), authority.clone(), program.clone()])
}


// System program instructions, encoded by hand (bincode: u32 tag + fields) so
// the serializer behind `system_instruction` stays out of the binary.
fn sys_ix(keys: Vec<AccountMeta>, tag: u32, rest: &[u8]) -> Instruction {
    let mut data = tag.to_le_bytes().to_vec();
    data.extend_from_slice(rest);
    Instruction { program_id: system_program::ID, accounts: keys, data }
}
fn sys_create_account(from: &Pubkey, to: &Pubkey, lamports: u64, space: u64, owner: &Pubkey) -> Instruction {
    let mut w = Wr::default();
    w.u64(lamports).u64(space).key(owner);
    sys_ix(vec![AccountMeta::new(*from, true), AccountMeta::new(*to, true)], 0, &w.0)
}
fn sys_assign(acct: &Pubkey, owner: &Pubkey) -> Instruction {
    sys_ix(vec![AccountMeta::new(*acct, true)], 1, owner.as_ref())
}
fn sys_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    sys_ix(vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)], 2, &lamports.to_le_bytes())
}
fn sys_allocate(acct: &Pubkey, space: u64) -> Instruction {
    sys_ix(vec![AccountMeta::new(*acct, true)], 8, &space.to_le_bytes())
}

/// Creates a PDA account the way Anchor's `init` does, including when someone
/// has already sent lamports to the address.
fn create_pda<'a>(
    payer: &AccountInfo<'a>, target: &AccountInfo<'a>, system: &AccountInfo<'a>,
    space: usize, owner: &Pubkey, seeds: &[&[u8]],
) -> ProgramResult {
    check(target.owner == &system_program::ID && target.data_is_empty(), "Error Message: That account already exists.")?;
    let rent = Rent::get()?.minimum_balance(space);
    let have = target.lamports();
    if have == 0 {
        return invoke_signed(
            &sys_create_account(payer.key, target.key, rent, space as u64, owner),
            &[payer.clone(), target.clone(), system.clone()],
            &[seeds],
        );
    }
    if rent > have {
        invoke(&sys_transfer(payer.key, target.key, rent - have), &[payer.clone(), target.clone(), system.clone()])?;
    }
    invoke_signed(&sys_allocate(target.key, space as u64), &[target.clone(), system.clone()], &[seeds])?;
    invoke_signed(&sys_assign(target.key, owner), &[target.clone(), system.clone()], &[seeds])
}

// ---------------------------------------------------------------- entry

pub fn process_instruction(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (disc, rest) = data.split_at(8);
    let disc: [u8; 8] = disc.try_into().unwrap();
    let mut r = Rd::new(rest);
    match disc {
        ix::INITIALIZE_RESERVE => initialize_reserve(program_id, accounts, Params::read(&mut r)?),
        ix::UPDATE_RESERVE => update_reserve(program_id, accounts, Params::read(&mut r)?),
        ix::SET_ADMIN => set_admin(program_id, accounts, r.key()?),
        ix::CREATE_ISSUER => {
            let name = r.string(usize::MAX)?;
            let symbol = r.string(usize::MAX)?;
            let uri = r.string(usize::MAX)?;
            let routes = r.routes()?;
            create_issuer(program_id, accounts, name, symbol, uri, routes)
        }
        ix::SET_ROUTES => {
            let routes = r.routes()?;
            set_routes(program_id, accounts, Some(routes), None)
        }
        ix::SET_ISSUER_AUTHORITY => set_routes(program_id, accounts, None, Some(r.key()?)),
        ix::MINT => convert(program_id, accounts, r.u64()?, true),
        ix::REDEEM => convert(program_id, accounts, r.u64()?, false),
        ix::HARVEST => harvest(program_id, accounts),
        ix::COLLECT_PROTOCOL_FEE => collect_protocol_fee(program_id, accounts),
        ix::UPDATE_PRICE => update_price(program_id, accounts, r.u64()?),
        ix::INVEST => rebalance(program_id, accounts, r.u64()?, true),
        ix::DIVEST => rebalance(program_id, accounts, r.u64()?, false),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ---------------------------------------------------------------- instructions

fn initialize_reserve(program_id: &Pubkey, accounts: &[AccountInfo], p: Params) -> ProgramResult {
    let it = &mut accounts.iter();
    let admin = next_account_info(it)?;
    let reserve = next_account_info(it)?;
    let usdc_mint = next_account_info(it)?;
    let tbill_mint = next_account_info(it)?;
    let usdc_vault = next_account_info(it)?;
    let tbill_vault = next_account_info(it)?;
    let usdc_prog = next_account_info(it)?;
    let tbill_prog = next_account_info(it)?;
    let system = next_account_info(it)?;

    signer(admin)?;
    for a in [admin, reserve, usdc_vault, tbill_vault] {
        writable(a)?;
    }
    token_program(usdc_prog)?;
    token_program(tbill_prog)?;
    check(*system.key == system_program::ID, "Error Message: Wrong system program.")?;
    mint_decimals(usdc_mint, usdc_prog.key)?;
    mint_decimals(tbill_mint, tbill_prog.key)?;
    // vaults are plain 165-byte accounts, so extended Token-2022 mints aren't supported
    for (m, prog) in [(usdc_mint, usdc_prog), (tbill_mint, tbill_prog)] {
        check(*prog.key == TOKEN || m.data_len() == 82, "Error Message: Token-2022 mints with extensions aren't supported.")?;
    }
    p.validate()?;
    require(p.tbill_price > 0, E::BadPrice)?;

    let (reserve_pda, bump) = Pubkey::find_program_address(&[RESERVE_SEED], program_id);
    check(reserve_pda == *reserve.key, "Error Message: Wrong reserve address.")?;
    create_pda(admin, reserve, system, RESERVE_LEN, program_id, &[RESERVE_SEED, &[bump]])?;

    for (vault, seed, mint, prog) in [(usdc_vault, USDC_VAULT_SEED, usdc_mint, usdc_prog), (tbill_vault, TBILL_VAULT_SEED, tbill_mint, tbill_prog)] {
        let (pda, vb) = Pubkey::find_program_address(&[seed], program_id);
        check(pda == *vault.key, "Error Message: Wrong vault address.")?;
        create_pda(admin, vault, system, TOKEN_ACCOUNT_LEN, prog.key, &[seed, &[vb]])?;
        // InitializeAccount3: tag 18 + owner
        let mut data = vec![18u8];
        data.extend_from_slice(reserve.key.as_ref());
        let ix = token_ix(prog.key, vec![AccountMeta::new(*vault.key, false), AccountMeta::new_readonly(*mint.key, false)], data);
        invoke(&ix, &[vault.clone(), mint.clone(), prog.clone()])?;
    }

    let mut r = Reserve {
        admin: *admin.key,
        price_authority: Pubkey::default(),
        allocator: Pubkey::default(),
        treasury: Pubkey::default(),
        usdc_mint: *usdc_mint.key,
        tbill_mint: *tbill_mint.key,
        usdc_vault: *usdc_vault.key,
        tbill_vault: *tbill_vault.key,
        tbill_price: p.tbill_price,
        price_updated_at: Clock::get()?.unix_timestamp,
        protocol_fee_bps: 0,
        min_liquid_bps: 0,
        paused: false,
        total_supply: 0,
        yield_index: 0,
        unclaimed_yield: 0,
        protocol_owed: 0,
        total_yield: 0,
        issuer_count: 0,
        bump,
        max_supply: 0,
    };
    r.apply(&p);
    save_reserve(reserve, &r)
}

/// Reserve + the two vaults, with the vault keys matching the reserve.
fn reserve_and_vaults(
    program_id: &Pubkey, reserve: &AccountInfo, usdc_vault: &AccountInfo, tbill_vault: &AccountInfo,
) -> Result<(Reserve, u64, u64), ProgramError> {
    writable(reserve)?;
    let r = load_reserve(program_id, reserve)?;
    check(r.usdc_vault == *usdc_vault.key && r.tbill_vault == *tbill_vault.key, "Error Message: Wrong vault.")?;
    let usdc = token_account(usdc_vault, None)?.amount;
    let tbill = token_account(tbill_vault, None)?.amount;
    Ok((r, usdc, tbill))
}

fn update_reserve(program_id: &Pubkey, accounts: &[AccountInfo], p: Params) -> ProgramResult {
    let it = &mut accounts.iter();
    let admin = next_account_info(it)?;
    let reserve = next_account_info(it)?;
    let usdc_vault = next_account_info(it)?;
    let tbill_vault = next_account_info(it)?;
    signer(admin)?;
    let (mut r, usdc, tbill) = reserve_and_vaults(program_id, reserve, usdc_vault, tbill_vault)?;
    check(r.admin == *admin.key, "Error Message: Only the admin can do that.")?;
    p.validate()?;
    // count yield earned so far under the old fee first
    r.accrue(usdc, tbill)?;
    r.apply(&p);
    save_reserve(reserve, &r)
}

fn set_admin(program_id: &Pubkey, accounts: &[AccountInfo], new_admin: Pubkey) -> ProgramResult {
    let it = &mut accounts.iter();
    let admin = next_account_info(it)?;
    let reserve = next_account_info(it)?;
    signer(admin)?;
    writable(reserve)?;
    let mut r = load_reserve(program_id, reserve)?;
    check(r.admin == *admin.key, "Error Message: Only the admin can do that.")?;
    r.admin = new_admin;
    save_reserve(reserve, &r)
}

fn create_issuer(
    program_id: &Pubkey, accounts: &[AccountInfo], name: String, symbol: String, uri: String, routes: Vec<Route>,
) -> ProgramResult {
    let it = &mut accounts.iter();
    let creator = next_account_info(it)?;
    let reserve = next_account_info(it)?;
    let issuer = next_account_info(it)?;
    let mint = next_account_info(it)?;
    let dollar_prog = next_account_info(it)?;
    let system = next_account_info(it)?;

    signer(creator)?;
    for a in [creator, reserve, issuer, mint] {
        writable(a)?;
    }
    check(*dollar_prog.key == TOKEN_2022, "Error Message: Dollars must use Token-2022.")?;
    check(*system.key == system_program::ID, "Error Message: Wrong system program.")?;
    let mut r = load_reserve(program_id, reserve)?;

    validate_symbol(&symbol)?;
    require(!name.is_empty() && name.len() <= MAX_NAME_LEN, E::BadName)?;
    require(uri.len() <= MAX_URI_LEN, E::BadUri)?;
    validate_routes(&routes)?;

    let (issuer_pda, ibump) = Pubkey::find_program_address(&[ISSUER_SEED, symbol.as_bytes()], program_id);
    check(issuer_pda == *issuer.key, "Error Message: Wrong issuer address.")?;
    let (mint_pda, mbump) = Pubkey::find_program_address(&[MINT_SEED, issuer.key.as_ref()], program_id);
    check(mint_pda == *mint.key, "Error Message: Wrong mint address.")?;

    let ib = [ibump];
    let issuer_seeds: &[&[u8]] = &[ISSUER_SEED, symbol.as_bytes(), &ib];
    create_pda(creator, issuer, system, ISSUER_LEN, program_id, issuer_seeds)?;
    create_pda(creator, mint, system, MINT_WITH_POINTER_LEN, &TOKEN_2022, &[MINT_SEED, issuer.key.as_ref(), &[mbump]])?;

    // MetadataPointer::Initialize (39, 0): authority = issuer, metadata = the mint itself
    let mut data = vec![39u8, 0];
    data.extend_from_slice(issuer.key.as_ref());
    data.extend_from_slice(mint.key.as_ref());
    invoke(&token_ix(&TOKEN_2022, vec![AccountMeta::new(*mint.key, false)], data), &[mint.clone(), dollar_prog.clone()])?;

    // InitializeMint2 (20): decimals, mint authority = issuer, no freeze authority
    let mut data = vec![20u8, DOLLAR_DECIMALS];
    data.extend_from_slice(issuer.key.as_ref());
    data.push(0);
    invoke(&token_ix(&TOKEN_2022, vec![AccountMeta::new(*mint.key, false)], data), &[mint.clone(), dollar_prog.clone()])?;

    // Top the mint up for the metadata Token-2022 is about to append.
    let extra = 4 + 32 + 32 + 4 + name.len() + 4 + symbol.len() + 4 + uri.len() + 4;
    let needed = Rent::get()?.minimum_balance(mint.data_len() + extra);
    let have = mint.lamports();
    if needed > have {
        invoke(&sys_transfer(creator.key, mint.key, needed - have), &[creator.clone(), mint.clone(), system.clone()])?;
    }

    let mut w = Wr::default();
    w.bytes(&TOKEN_METADATA_INIT).string(&name).string(&symbol).string(&uri);
    let ix = token_ix(&TOKEN_2022, vec![
        AccountMeta::new(*mint.key, false),
        AccountMeta::new_readonly(*issuer.key, false),
        AccountMeta::new_readonly(*mint.key, false),
        AccountMeta::new_readonly(*issuer.key, true),
    ], w.0);
    invoke_signed(&ix, &[mint.clone(), issuer.clone(), dollar_prog.clone()], &[issuer_seeds])?;

    let mut i = Issuer {
        reserve: *reserve.key,
        authority: *creator.key,
        mint: *mint.key,
        name,
        symbol,
        supply: 0,
        index_snapshot: r.yield_index,
        accrued: 0,
        total_yield_paid: 0,
        routes: [Route::default(); MAX_ROUTES],
        route_count: 0,
        created_at: Clock::get()?.unix_timestamp,
        bump: ibump,
    };
    i.set_routes(&routes);
    save_issuer(issuer, &i)?;
    r.issuer_count = r.issuer_count.checked_add(1).ok_or(E::Overflow)?;
    save_reserve(reserve, &r)
}

/// set_routes and set_issuer_authority share their accounts: authority, issuer.
fn set_routes(program_id: &Pubkey, accounts: &[AccountInfo], routes: Option<Vec<Route>>, new_authority: Option<Pubkey>) -> ProgramResult {
    let it = &mut accounts.iter();
    let authority = next_account_info(it)?;
    let issuer = next_account_info(it)?;
    signer(authority)?;
    writable(issuer)?;
    let mut i = load_issuer(program_id, issuer)?;
    check(i.authority == *authority.key, "Error Message: Only the issuer's authority can do that.")?;
    if let Some(routes) = routes {
        validate_routes(&routes)?;
        i.set_routes(&routes);
    }
    if let Some(a) = new_authority {
        i.authority = a;
    }
    save_issuer(issuer, &i)
}

fn convert(program_id: &Pubkey, accounts: &[AccountInfo], amount: u64, minting: bool) -> ProgramResult {
    let it = &mut accounts.iter();
    let user = next_account_info(it)?;
    let reserve = next_account_info(it)?;
    let issuer = next_account_info(it)?;
    let mint = next_account_info(it)?;
    let usdc_mint = next_account_info(it)?;
    let usdc_vault = next_account_info(it)?;
    let tbill_vault = next_account_info(it)?;
    let user_usdc = next_account_info(it)?;
    let user_dollar = next_account_info(it)?;
    let usdc_prog = next_account_info(it)?;
    let dollar_prog = next_account_info(it)?;

    require(amount > 0, E::ZeroAmount)?;
    signer(user)?;
    for a in [reserve, issuer, mint, usdc_vault, user_usdc, user_dollar] {
        writable(a)?;
    }
    token_program(usdc_prog)?;
    check(*dollar_prog.key == TOKEN_2022, "Error Message: Dollars use Token-2022.")?;
    let (mut r, usdc, tbill) = reserve_and_vaults(program_id, reserve, usdc_vault, tbill_vault)?;
    check(r.usdc_mint == *usdc_mint.key, "Error Message: Wrong USDC mint.")?;
    let mut i = load_issuer(program_id, issuer)?;
    check(i.reserve == *reserve.key && i.mint == *mint.key, "Error Message: Wrong dollar.")?;
    mint_decimals(mint, &TOKEN_2022)?;
    let usdc_decimals = mint_decimals(usdc_mint, usdc_prog.key)?;
    let uu = token_account(user_usdc, Some(usdc_prog.key))?;
    check(uu.mint == *usdc_mint.key && uu.owner == *user.key, "Error Message: That isn't your USDC account.")?;
    let ud = token_account(user_dollar, Some(&TOKEN_2022))?;
    check(ud.mint == *mint.key && ud.owner == *user.key, "Error Message: That isn't your account for this dollar.")?;

    let reserve_seeds: &[&[u8]] = &[RESERVE_SEED, &[r.bump]];
    if minting {
        require(!r.paused, E::Paused)?;
        require(r.max_supply == 0 || r.total_supply.saturating_add(amount) <= r.max_supply, E::CapReached)?;
        r.accrue(usdc, tbill)?;
        i.settle(r.yield_index)?;
        transfer_checked(usdc_prog, user_usdc, usdc_mint, usdc_vault, user, amount, usdc_decimals, &[])?;
        let b = i.seeds_bump();
        mint_to(dollar_prog, mint, user_dollar, issuer, amount, &[&[ISSUER_SEED, i.symbol.as_bytes(), &b]])?;
        i.supply = i.supply.checked_add(amount).ok_or(E::Overflow)?;
        r.total_supply = r.total_supply.checked_add(amount).ok_or(E::Overflow)?;
    } else {
        require(usdc >= amount, E::InsufficientLiquidity)?;
        r.accrue(usdc, tbill)?;
        i.settle(r.yield_index)?;
        burn(dollar_prog, user_dollar, mint, user, amount)?;
        transfer_checked(usdc_prog, usdc_vault, usdc_mint, user_usdc, reserve, amount, usdc_decimals, &[reserve_seeds])?;
        i.supply = i.supply.checked_sub(amount).ok_or(E::Overflow)?;
        r.total_supply = r.total_supply.checked_sub(amount).ok_or(E::Overflow)?;
    }
    save_issuer(issuer, &i)?;
    save_reserve(reserve, &r)
}

fn harvest(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let reserve = next_account_info(it)?;
    let issuer = next_account_info(it)?;
    let mint = next_account_info(it)?;
    let usdc_mint = next_account_info(it)?;
    let usdc_vault = next_account_info(it)?;
    let tbill_vault = next_account_info(it)?;
    let usdc_prog = next_account_info(it)?;
    let dollar_prog = next_account_info(it)?;
    let route_accounts: Vec<&AccountInfo> = it.collect();

    for a in [issuer, mint, usdc_vault] {
        writable(a)?;
    }
    token_program(usdc_prog)?;
    check(*dollar_prog.key == TOKEN_2022, "Error Message: Dollars use Token-2022.")?;
    let (mut r, usdc, tbill) = reserve_and_vaults(program_id, reserve, usdc_vault, tbill_vault)?;
    check(r.usdc_mint == *usdc_mint.key, "Error Message: Wrong USDC mint.")?;
    let usdc_decimals = mint_decimals(usdc_mint, usdc_prog.key)?;
    let mut i = load_issuer(program_id, issuer)?;
    check(i.reserve == *reserve.key && i.mint == *mint.key, "Error Message: Wrong dollar.")?;
    mint_decimals(mint, &TOKEN_2022)?;

    r.accrue(usdc, tbill)?;
    i.settle(r.yield_index)?;
    let payout = i.accrued;
    require(payout > 0, E::NothingToHarvest)?;
    let n = i.route_count as usize;
    require(route_accounts.len() == n, E::BadRouteAccounts)?;

    let reserve_seeds: &[&[u8]] = &[RESERVE_SEED, &[r.bump]];
    let ib = i.seeds_bump();
    let issuer_seeds: &[&[u8]] = &[ISSUER_SEED, i.symbol.as_bytes(), &ib];
    let mut paid_so_far: u64 = 0;
    let mut usdc_out: u64 = 0;
    let mut dollars_minted: u64 = 0;
    for (k, route) in i.routes.iter().take(n).enumerate() {
        let share = if k + 1 == n { payout - paid_so_far } else { (payout as u128 * route.bps as u128 / BPS) as u64 };
        paid_so_far += share;
        if share == 0 {
            continue;
        }
        let dest = route_accounts[k];
        writable(dest)?;
        let t = token_account(dest, None).map_err(|_| ProgramError::from(E::BadRouteAccounts))?;
        require(t.owner == route.wallet, E::BadRouteAccounts)?;
        if route.kind == ROUTE_USDC {
            require(t.mint == r.usdc_mint, E::BadRouteAccounts)?;
            transfer_checked(usdc_prog, usdc_vault, usdc_mint, dest, reserve, share, usdc_decimals, &[reserve_seeds])?;
            usdc_out += share;
        } else {
            require(t.mint == i.mint, E::BadRouteAccounts)?;
            mint_to(dollar_prog, mint, dest, issuer, share, &[issuer_seeds])?;
            dollars_minted += share;
        }
    }
    require(usdc_out <= usdc, E::InsufficientLiquidity)?;

    i.supply = i.supply.checked_add(dollars_minted).ok_or(E::Overflow)?;
    r.total_supply = r.total_supply.checked_add(dollars_minted).ok_or(E::Overflow)?;
    r.unclaimed_yield = r.unclaimed_yield.saturating_sub(payout);
    i.accrued = 0;
    i.total_yield_paid = i.total_yield_paid.saturating_add(payout);
    save_issuer(issuer, &i)?;
    save_reserve(reserve, &r)
}

fn collect_protocol_fee(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let reserve = next_account_info(it)?;
    let usdc_mint = next_account_info(it)?;
    let usdc_vault = next_account_info(it)?;
    let tbill_vault = next_account_info(it)?;
    let treasury_usdc = next_account_info(it)?;
    let usdc_prog = next_account_info(it)?;

    for a in [usdc_vault, treasury_usdc] {
        writable(a)?;
    }
    token_program(usdc_prog)?;
    let (mut r, usdc, tbill) = reserve_and_vaults(program_id, reserve, usdc_vault, tbill_vault)?;
    check(r.usdc_mint == *usdc_mint.key, "Error Message: Wrong USDC mint.")?;
    let usdc_decimals = mint_decimals(usdc_mint, usdc_prog.key)?;
    let t = token_account(treasury_usdc, None)?;
    check(t.mint == *usdc_mint.key && t.owner == r.treasury, "Error Message: That isn't the treasury's USDC account.")?;

    r.accrue(usdc, tbill)?;
    let owed = r.protocol_owed;
    require(owed > 0, E::NothingToHarvest)?;
    require(usdc >= owed, E::InsufficientLiquidity)?;
    let seeds: &[&[u8]] = &[RESERVE_SEED, &[r.bump]];
    transfer_checked(usdc_prog, usdc_vault, usdc_mint, treasury_usdc, reserve, owed, usdc_decimals, &[seeds])?;
    r.protocol_owed = 0;
    r.unclaimed_yield = r.unclaimed_yield.saturating_sub(owed);
    save_reserve(reserve, &r)
}

fn update_price(program_id: &Pubkey, accounts: &[AccountInfo], new_price: u64) -> ProgramResult {
    let it = &mut accounts.iter();
    let authority = next_account_info(it)?;
    let reserve = next_account_info(it)?;
    let usdc_vault = next_account_info(it)?;
    let tbill_vault = next_account_info(it)?;
    signer(authority)?;
    let (mut r, usdc, tbill) = reserve_and_vaults(program_id, reserve, usdc_vault, tbill_vault)?;
    check(r.price_authority == *authority.key, "Error Message: Only the price authority can do that.")?;
    let now = Clock::get()?.unix_timestamp;
    require(now - r.price_updated_at >= MIN_PRICE_INTERVAL, E::PriceTooSoon)?;
    let old = r.tbill_price;
    let diff = old.abs_diff(new_price) as u128;
    require(new_price > 0 && diff * BPS <= old as u128 * MAX_PRICE_MOVE_BPS, E::BadPrice)?;
    r.accrue(usdc, tbill)?;
    r.tbill_price = new_price;
    r.price_updated_at = now;
    save_reserve(reserve, &r)
}

fn rebalance(program_id: &Pubkey, accounts: &[AccountInfo], tbill_amount: u64, investing: bool) -> ProgramResult {
    let it = &mut accounts.iter();
    let allocator = next_account_info(it)?;
    let reserve = next_account_info(it)?;
    let usdc_mint = next_account_info(it)?;
    let tbill_mint = next_account_info(it)?;
    let usdc_vault = next_account_info(it)?;
    let tbill_vault = next_account_info(it)?;
    let alloc_usdc = next_account_info(it)?;
    let alloc_tbill = next_account_info(it)?;
    let usdc_prog = next_account_info(it)?;
    let tbill_prog = next_account_info(it)?;

    require(tbill_amount > 0, E::ZeroAmount)?;
    signer(allocator)?;
    for a in [usdc_vault, tbill_vault, alloc_usdc, alloc_tbill] {
        writable(a)?;
    }
    token_program(usdc_prog)?;
    token_program(tbill_prog)?;
    let (mut r, usdc, tbill) = reserve_and_vaults(program_id, reserve, usdc_vault, tbill_vault)?;
    check(r.allocator == *allocator.key, "Error Message: Only the allocator can do that.")?;
    check(r.usdc_mint == *usdc_mint.key && r.tbill_mint == *tbill_mint.key, "Error Message: Wrong mint.")?;
    let usdc_decimals = mint_decimals(usdc_mint, usdc_prog.key)?;
    let tbill_decimals = mint_decimals(tbill_mint, tbill_prog.key)?;
    let au = token_account(alloc_usdc, None)?;
    check(au.mint == *usdc_mint.key && au.owner == *allocator.key, "Error Message: That isn't the allocator's USDC account.")?;
    let at = token_account(alloc_tbill, None)?;
    check(at.mint == *tbill_mint.key && at.owner == *allocator.key, "Error Message: That isn't the allocator's Treasury account.")?;

    r.accrue(usdc, tbill)?;
    let seeds: &[&[u8]] = &[RESERVE_SEED, &[r.bump]];
    if investing {
        let usdc_out = tbill_value(tbill_amount, r.tbill_price, false)?;
        let left = usdc.checked_sub(usdc_out).ok_or_else(|| ProgramError::from(E::InsufficientLiquidity))?;
        let floor = (r.total_supply as u128 * r.min_liquid_bps as u128 / BPS) as u64;
        require(left >= floor, E::BelowLiquidityFloor)?;
        transfer_checked(tbill_prog, alloc_tbill, tbill_mint, tbill_vault, allocator, tbill_amount, tbill_decimals, &[])?;
        transfer_checked(usdc_prog, usdc_vault, usdc_mint, alloc_usdc, reserve, usdc_out, usdc_decimals, &[seeds])?;
    } else {
        let usdc_in = tbill_value(tbill_amount, r.tbill_price, true)?;
        transfer_checked(usdc_prog, alloc_usdc, usdc_mint, usdc_vault, allocator, usdc_in, usdc_decimals, &[])?;
        transfer_checked(tbill_prog, tbill_vault, tbill_mint, alloc_tbill, reserve, tbill_amount, tbill_decimals, &[seeds])?;
    }
    save_reserve(reserve, &r)
}
