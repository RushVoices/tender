//! Tender: community dollars on one shared reserve.
//!
//! A memecoin, an app, a DAO or a creator registers an `Issuer` and gets its
//! own dollar: a Token-2022 mint (6 decimals, on-chain name and symbol, no
//! freeze authority) whose only mint authority is this program. Anyone can
//! mint that dollar 1:1 by depositing USDC and redeem it 1:1 for USDC.
//!
//! Every issuer's USDC lands in one shared `Reserve`, which holds two vaults:
//!
//! * `usdc_vault`  - liquid USDC, which redemptions are paid from;
//! * `tbill_vault` - a tokenized Treasury (e.g. USDY), valued at `tbill_price`.
//!
//! How yield is counted
//! --------------------
//! The reserve owes `total_supply` (every community dollar outstanding) plus
//! `unclaimed_yield` (yield already counted but not yet paid out). Whatever
//! the vaults are worth above that is new yield. `accrue` takes the protocol
//! fee off it and adds the rest to `yield_index`, a running "yield per dollar
//! outstanding" counter. An issuer earns `supply x (index now - index when it
//! last settled)`, so yield is split by how many dollars each issuer had
//! outstanding, and for how long. Every instruction that changes an issuer's
//! supply settles it first.
//!
//! Where yield goes
//! ----------------
//! Each issuer sets up to four routes that split 10,000 bps. A route pays
//! either USDC to a wallet or freshly minted community dollars to a wallet
//! (the USDC that backs them is the yield itself, already in the reserve).
//! `harvest` is permissionless and can only pay the issuer's routes.

use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{
    self, token_metadata_initialize, Burn, Mint, MintTo, TokenAccount, TokenInterface,
    TokenMetadataInitialize, TransferChecked,
};

declare_id!("Tnd4ZV5367L8r7DDxLH97bA64SkR4Ke1Lf6GZa9VwgJ");

/// `tbill_price` is USDC base units per tbill base unit, times 1e9.
pub const PRICE_SCALE: u128 = 1_000_000_000;
/// `yield_index` is USDC base units of yield per dollar base unit, times 1e18.
pub const INDEX_SCALE: u128 = 1_000_000_000_000_000_000;
pub const BPS: u128 = 10_000;
pub const DOLLAR_DECIMALS: u8 = 6;

pub const MAX_ROUTES: usize = 4;
pub const MAX_NAME_LEN: usize = 32;
pub const MAX_SYMBOL_LEN: usize = 10;
pub const MAX_URI_LEN: usize = 200;

/// Hard cap on the protocol's cut of yield: 20%.
pub const MAX_PROTOCOL_FEE_BPS: u16 = 2_000;
/// A price update may move the Treasury price by at most 0.5%...
pub const MAX_PRICE_MOVE_BPS: u64 = 50;
/// ...and at most once an hour.
pub const MIN_PRICE_INTERVAL: i64 = 3_600;

pub const ROUTE_USDC: u8 = 0;
pub const ROUTE_DOLLAR: u8 = 1;

pub const RESERVE_SEED: &[u8] = b"reserve";
pub const USDC_VAULT_SEED: &[u8] = b"usdc_vault";
pub const TBILL_VAULT_SEED: &[u8] = b"tbill_vault";
pub const ISSUER_SEED: &[u8] = b"issuer";
pub const MINT_SEED: &[u8] = b"mint";

#[program]
pub mod tender {
    use super::*;

    /// One-time setup of the shared reserve. The signer becomes admin.
    pub fn initialize_reserve(ctx: Context<InitializeReserve>, params: ReserveParams) -> Result<()> {
        params.validate()?;
        require!(params.tbill_price > 0, TenderError::BadPrice);
        let r = &mut ctx.accounts.reserve;
        r.admin = ctx.accounts.admin.key();
        r.usdc_mint = ctx.accounts.usdc_mint.key();
        r.tbill_mint = ctx.accounts.tbill_mint.key();
        r.usdc_vault = ctx.accounts.usdc_vault.key();
        r.tbill_vault = ctx.accounts.tbill_vault.key();
        r.tbill_price = params.tbill_price;
        r.price_updated_at = Clock::get()?.unix_timestamp;
        r.bump = ctx.bumps.reserve;
        r.apply(&params);
        Ok(())
    }

    /// Admin-only. Changes roles and limits; never touches anyone's dollars.
    pub fn update_reserve(ctx: Context<AdminOnly>, params: ReserveParams) -> Result<()> {
        params.validate()?;
        let r = &mut ctx.accounts.reserve;
        let (usdc, tbill) = (ctx.accounts.usdc_vault.amount, ctx.accounts.tbill_vault.amount);
        // Count yield earned so far under the old fee before changing it.
        r.accrue(usdc, tbill)?;
        r.apply(&params);
        Ok(())
    }

    pub fn set_admin(ctx: Context<AdminOnlyLight>, new_admin: Pubkey) -> Result<()> {
        ctx.accounts.reserve.admin = new_admin;
        Ok(())
    }

    /// Registers a community dollar. Creates its Token-2022 mint with
    /// on-chain metadata. The signer becomes the issuer's authority (the only
    /// key that can change its routes).
    pub fn create_issuer(ctx: Context<CreateIssuer>, args: CreateIssuerArgs) -> Result<()> {
        validate_symbol(&args.symbol)?;
        require!(
            !args.name.is_empty() && args.name.len() <= MAX_NAME_LEN,
            TenderError::BadName
        );
        require!(args.uri.len() <= MAX_URI_LEN, TenderError::BadUri);
        validate_routes(&args.routes)?;

        let reserve = &mut ctx.accounts.reserve;
        let issuer = &mut ctx.accounts.issuer;
        issuer.reserve = reserve.key();
        issuer.authority = ctx.accounts.creator.key();
        issuer.mint = ctx.accounts.mint.key();
        issuer.name = args.name.clone();
        issuer.symbol = args.symbol.clone();
        issuer.supply = 0;
        issuer.index_snapshot = reserve.yield_index;
        issuer.accrued = 0;
        issuer.total_yield_paid = 0;
        issuer.set_routes(&args.routes);
        issuer.created_at = Clock::get()?.unix_timestamp;
        issuer.bump = ctx.bumps.issuer;
        reserve.issuer_count = reserve.issuer_count.checked_add(1).ok_or(TenderError::Overflow)?;

        // Top the mint up for the metadata Token-2022 is about to append:
        // TLV header (2 type + 2 length) + update authority + mint + three
        // length-prefixed strings + empty additional-metadata vec.
        let mint_info = ctx.accounts.mint.to_account_info();
        let extra = 4 + 32 + 32 + 4 + args.name.len() + 4 + args.symbol.len() + 4 + args.uri.len() + 4;
        let needed = Rent::get()?.minimum_balance(mint_info.data_len() + extra);
        let have = mint_info.lamports();
        if needed > have {
            system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer {
                        from: ctx.accounts.creator.to_account_info(),
                        to: mint_info.clone(),
                    },
                ),
                needed - have,
            )?;
        }

        let symbol = args.symbol.clone();
        let seeds: &[&[u8]] = &[ISSUER_SEED, symbol.as_bytes(), &[ctx.bumps.issuer]];
        token_metadata_initialize(
            CpiContext::new_with_signer(
                ctx.accounts.dollar_token_program.to_account_info(),
                TokenMetadataInitialize {
                    program_id: ctx.accounts.dollar_token_program.to_account_info(),
                    mint: mint_info.clone(),
                    metadata: mint_info,
                    mint_authority: ctx.accounts.issuer.to_account_info(),
                    update_authority: ctx.accounts.issuer.to_account_info(),
                },
                &[seeds],
            ),
            args.name,
            args.symbol,
            args.uri,
        )?;

        emit!(IssuerCreated {
            issuer: ctx.accounts.issuer.key(),
            mint: ctx.accounts.mint.key(),
            symbol,
        });
        Ok(())
    }

    /// Issuer authority only. Yield already earned goes out under the new
    /// routes at the next harvest.
    pub fn set_routes(ctx: Context<IssuerAuthority>, routes: Vec<Route>) -> Result<()> {
        validate_routes(&routes)?;
        ctx.accounts.issuer.set_routes(&routes);
        Ok(())
    }

    pub fn set_issuer_authority(ctx: Context<IssuerAuthority>, new_authority: Pubkey) -> Result<()> {
        ctx.accounts.issuer.authority = new_authority;
        Ok(())
    }

    /// Deposit `amount` USDC, receive `amount` of the community dollar.
    pub fn mint(ctx: Context<Convert>, amount: u64) -> Result<()> {
        require!(amount > 0, TenderError::ZeroAmount);
        let a = ctx.accounts;
        require!(!a.reserve.paused, TenderError::Paused);
        let cap = a.reserve.max_supply;
        require!(
            cap == 0 || a.reserve.total_supply.saturating_add(amount) <= cap,
            TenderError::CapReached
        );
        a.reserve.accrue(a.usdc_vault.amount, a.tbill_vault.amount)?;
        a.issuer.settle(a.reserve.yield_index)?;

        token_interface::transfer_checked(
            CpiContext::new(
                a.usdc_token_program.to_account_info(),
                TransferChecked {
                    from: a.user_usdc.to_account_info(),
                    mint: a.usdc_mint.to_account_info(),
                    to: a.usdc_vault.to_account_info(),
                    authority: a.user.to_account_info(),
                },
            ),
            amount,
            a.usdc_mint.decimals,
        )?;

        let symbol = a.issuer.symbol.clone();
        let seeds: &[&[u8]] = &[ISSUER_SEED, symbol.as_bytes(), &[a.issuer.bump]];
        token_interface::mint_to(
            CpiContext::new_with_signer(
                a.dollar_token_program.to_account_info(),
                MintTo {
                    mint: a.mint.to_account_info(),
                    to: a.user_dollar.to_account_info(),
                    authority: a.issuer.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;

        a.issuer.supply = a.issuer.supply.checked_add(amount).ok_or(TenderError::Overflow)?;
        a.reserve.total_supply = a.reserve.total_supply.checked_add(amount).ok_or(TenderError::Overflow)?;
        emit!(Converted { issuer: a.issuer.key(), user: a.user.key(), amount, minted: true });
        Ok(())
    }

    /// Burn `amount` of the community dollar, receive `amount` USDC.
    /// Never paused: the only thing that can stop a redemption is the
    /// reserve not holding enough liquid USDC at that moment.
    pub fn redeem(ctx: Context<Convert>, amount: u64) -> Result<()> {
        require!(amount > 0, TenderError::ZeroAmount);
        let a = ctx.accounts;
        require!(a.usdc_vault.amount >= amount, TenderError::InsufficientLiquidity);
        a.reserve.accrue(a.usdc_vault.amount, a.tbill_vault.amount)?;
        a.issuer.settle(a.reserve.yield_index)?;

        token_interface::burn(
            CpiContext::new(
                a.dollar_token_program.to_account_info(),
                Burn {
                    mint: a.mint.to_account_info(),
                    from: a.user_dollar.to_account_info(),
                    authority: a.user.to_account_info(),
                },
            ),
            amount,
        )?;

        let seeds: &[&[u8]] = &[RESERVE_SEED, &[a.reserve.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                a.usdc_token_program.to_account_info(),
                TransferChecked {
                    from: a.usdc_vault.to_account_info(),
                    mint: a.usdc_mint.to_account_info(),
                    to: a.user_usdc.to_account_info(),
                    authority: a.reserve.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            a.usdc_mint.decimals,
        )?;

        a.issuer.supply = a.issuer.supply.checked_sub(amount).ok_or(TenderError::Overflow)?;
        a.reserve.total_supply = a.reserve.total_supply.checked_sub(amount).ok_or(TenderError::Overflow)?;
        emit!(Converted { issuer: a.issuer.key(), user: a.user.key(), amount, minted: false });
        Ok(())
    }

    /// Permissionless. Counts new yield, then pays everything the issuer has
    /// earned to its routes.
    ///
    /// remaining_accounts: one writable token account per route, in route
    /// order, owned by the route's wallet. USDC for a USDC route, the
    /// community dollar for a dollar route.
    pub fn harvest<'info>(ctx: Context<'_, '_, 'info, 'info, Harvest<'info>>) -> Result<()> {
        let a = ctx.accounts;
        a.reserve.accrue(a.usdc_vault.amount, a.tbill_vault.amount)?;
        a.issuer.settle(a.reserve.yield_index)?;

        let payout = a.issuer.accrued;
        require!(payout > 0, TenderError::NothingToHarvest);
        let n = a.issuer.route_count as usize;
        require!(ctx.remaining_accounts.len() == n, TenderError::BadRouteAccounts);

        let routes = a.issuer.routes;
        let mut paid_so_far: u64 = 0;
        let mut usdc_out: u64 = 0;
        let mut dollars_minted: u64 = 0;
        for (i, route) in routes.iter().take(n).enumerate() {
            let share = if i + 1 == n {
                payout - paid_so_far
            } else {
                (payout as u128 * route.bps as u128 / BPS) as u64
            };
            paid_so_far += share;
            if share == 0 {
                continue;
            }
            let dest_info = &ctx.remaining_accounts[i];
            let dest = InterfaceAccount::<TokenAccount>::try_from(dest_info)?;
            require_keys_eq!(dest.owner, route.wallet, TenderError::BadRouteAccounts);

            if route.kind == ROUTE_USDC {
                require_keys_eq!(dest.mint, a.reserve.usdc_mint, TenderError::BadRouteAccounts);
                let seeds: &[&[u8]] = &[RESERVE_SEED, &[a.reserve.bump]];
                token_interface::transfer_checked(
                    CpiContext::new_with_signer(
                        a.usdc_token_program.to_account_info(),
                        TransferChecked {
                            from: a.usdc_vault.to_account_info(),
                            mint: a.usdc_mint.to_account_info(),
                            to: dest_info.clone(),
                            authority: a.reserve.to_account_info(),
                        },
                        &[seeds],
                    ),
                    share,
                    a.usdc_mint.decimals,
                )?;
                usdc_out += share;
            } else {
                require_keys_eq!(dest.mint, a.issuer.mint, TenderError::BadRouteAccounts);
                let symbol = a.issuer.symbol.clone();
                let seeds: &[&[u8]] = &[ISSUER_SEED, symbol.as_bytes(), &[a.issuer.bump]];
                token_interface::mint_to(
                    CpiContext::new_with_signer(
                        a.dollar_token_program.to_account_info(),
                        MintTo {
                            mint: a.mint.to_account_info(),
                            to: dest_info.clone(),
                            authority: a.issuer.to_account_info(),
                        },
                        &[seeds],
                    ),
                    share,
                )?;
                dollars_minted += share;
            }
        }
        require!(usdc_out <= a.usdc_vault.amount, TenderError::InsufficientLiquidity);

        // Minted dollars are now backed by the yield that paid for them.
        a.issuer.supply = a.issuer.supply.checked_add(dollars_minted).ok_or(TenderError::Overflow)?;
        a.reserve.total_supply = a.reserve.total_supply.checked_add(dollars_minted).ok_or(TenderError::Overflow)?;
        // The index math guarantees issuers never collectively claim more
        // than was counted; saturate anyway so dust can never brick harvests.
        a.reserve.unclaimed_yield = a.reserve.unclaimed_yield.saturating_sub(payout);
        a.issuer.accrued = 0;
        a.issuer.total_yield_paid = a.issuer.total_yield_paid.saturating_add(payout);
        emit!(Harvested { issuer: a.issuer.key(), amount: payout, usdc_out, dollars_minted });
        Ok(())
    }

    /// Permissionless. Pays the protocol's share of yield to the treasury.
    pub fn collect_protocol_fee(ctx: Context<CollectProtocolFee>) -> Result<()> {
        let a = ctx.accounts;
        a.reserve.accrue(a.usdc_vault.amount, a.tbill_vault.amount)?;
        let owed = a.reserve.protocol_owed;
        require!(owed > 0, TenderError::NothingToHarvest);
        require!(a.usdc_vault.amount >= owed, TenderError::InsufficientLiquidity);
        let seeds: &[&[u8]] = &[RESERVE_SEED, &[a.reserve.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                a.usdc_token_program.to_account_info(),
                TransferChecked {
                    from: a.usdc_vault.to_account_info(),
                    mint: a.usdc_mint.to_account_info(),
                    to: a.treasury_usdc.to_account_info(),
                    authority: a.reserve.to_account_info(),
                },
                &[seeds],
            ),
            owed,
            a.usdc_mint.decimals,
        )?;
        a.reserve.protocol_owed = 0;
        a.reserve.unclaimed_yield = a.reserve.unclaimed_yield.saturating_sub(owed);
        Ok(())
    }

    /// Price authority only. Moves the Treasury price by at most 0.5%, at
    /// most once an hour. Yield up to now is counted at the old price first.
    pub fn update_price(ctx: Context<UpdatePrice>, new_price: u64) -> Result<()> {
        let a = ctx.accounts;
        let now = Clock::get()?.unix_timestamp;
        require!(
            now - a.reserve.price_updated_at >= MIN_PRICE_INTERVAL,
            TenderError::PriceTooSoon
        );
        let old = a.reserve.tbill_price;
        let diff = old.abs_diff(new_price) as u128;
        require!(
            new_price > 0 && diff * BPS <= old as u128 * MAX_PRICE_MOVE_BPS as u128,
            TenderError::BadPrice
        );
        a.reserve.accrue(a.usdc_vault.amount, a.tbill_vault.amount)?;
        a.reserve.tbill_price = new_price;
        a.reserve.price_updated_at = now;
        Ok(())
    }

    /// Allocator only. Moves USDC into Treasuries: the allocator hands the
    /// reserve `tbill_amount` of the Treasury token and takes out USDC worth
    /// exactly that at the reserve's price (rounded in the reserve's favour).
    /// Must leave at least `min_liquid_bps` of supply in liquid USDC.
    pub fn invest(ctx: Context<Rebalance>, tbill_amount: u64) -> Result<()> {
        require!(tbill_amount > 0, TenderError::ZeroAmount);
        let a = ctx.accounts;
        a.reserve.accrue(a.usdc_vault.amount, a.tbill_vault.amount)?;
        let usdc_out = tbill_value(tbill_amount, a.reserve.tbill_price, false)?;
        let left = a
            .usdc_vault
            .amount
            .checked_sub(usdc_out)
            .ok_or(TenderError::InsufficientLiquidity)?;
        let floor = (a.reserve.total_supply as u128 * a.reserve.min_liquid_bps as u128 / BPS) as u64;
        require!(left >= floor, TenderError::BelowLiquidityFloor);

        token_interface::transfer_checked(
            CpiContext::new(
                a.tbill_token_program.to_account_info(),
                TransferChecked {
                    from: a.allocator_tbill.to_account_info(),
                    mint: a.tbill_mint.to_account_info(),
                    to: a.tbill_vault.to_account_info(),
                    authority: a.allocator.to_account_info(),
                },
            ),
            tbill_amount,
            a.tbill_mint.decimals,
        )?;
        let seeds: &[&[u8]] = &[RESERVE_SEED, &[a.reserve.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                a.usdc_token_program.to_account_info(),
                TransferChecked {
                    from: a.usdc_vault.to_account_info(),
                    mint: a.usdc_mint.to_account_info(),
                    to: a.allocator_usdc.to_account_info(),
                    authority: a.reserve.to_account_info(),
                },
                &[seeds],
            ),
            usdc_out,
            a.usdc_mint.decimals,
        )?;
        Ok(())
    }

    /// Allocator only. The reverse of `invest`: the allocator pays USDC worth
    /// `tbill_amount` (rounded up) and takes that much of the Treasury token.
    pub fn divest(ctx: Context<Rebalance>, tbill_amount: u64) -> Result<()> {
        require!(tbill_amount > 0, TenderError::ZeroAmount);
        let a = ctx.accounts;
        a.reserve.accrue(a.usdc_vault.amount, a.tbill_vault.amount)?;
        let usdc_in = tbill_value(tbill_amount, a.reserve.tbill_price, true)?;

        token_interface::transfer_checked(
            CpiContext::new(
                a.usdc_token_program.to_account_info(),
                TransferChecked {
                    from: a.allocator_usdc.to_account_info(),
                    mint: a.usdc_mint.to_account_info(),
                    to: a.usdc_vault.to_account_info(),
                    authority: a.allocator.to_account_info(),
                },
            ),
            usdc_in,
            a.usdc_mint.decimals,
        )?;
        let seeds: &[&[u8]] = &[RESERVE_SEED, &[a.reserve.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                a.tbill_token_program.to_account_info(),
                TransferChecked {
                    from: a.tbill_vault.to_account_info(),
                    mint: a.tbill_mint.to_account_info(),
                    to: a.allocator_tbill.to_account_info(),
                    authority: a.reserve.to_account_info(),
                },
                &[seeds],
            ),
            tbill_amount,
            a.tbill_mint.decimals,
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------- math

/// USDC value of `tbill_amount` at `price`, rounded down or up.
pub fn tbill_value(tbill_amount: u64, price: u64, round_up: bool) -> Result<u64> {
    let num = tbill_amount as u128 * price as u128;
    let v = if round_up { num.div_ceil(PRICE_SCALE) } else { num / PRICE_SCALE };
    u64::try_from(v).map_err(|_| TenderError::Overflow.into())
}

impl Reserve {
    fn apply(&mut self, p: &ReserveParams) {
        self.price_authority = p.price_authority;
        self.allocator = p.allocator;
        self.treasury = p.treasury;
        self.protocol_fee_bps = p.protocol_fee_bps;
        self.min_liquid_bps = p.min_liquid_bps;
        self.paused = p.paused;
        self.max_supply = p.max_supply;
    }

    /// Total the vaults are worth, in USDC base units.
    pub fn value(&self, usdc_balance: u64, tbill_balance: u64) -> Result<u64> {
        usdc_balance
            .checked_add(tbill_value(tbill_balance, self.tbill_price, false)?)
            .ok_or(TenderError::Overflow.into())
    }

    /// Counts any value above what the reserve owes as new yield.
    pub fn accrue(&mut self, usdc_balance: u64, tbill_balance: u64) -> Result<()> {
        if self.total_supply == 0 {
            // Nobody to attribute it to yet; it waits for the first dollar.
            return Ok(());
        }
        let value = self.value(usdc_balance, tbill_balance)?;
        let owed = self
            .total_supply
            .checked_add(self.unclaimed_yield)
            .ok_or(TenderError::Overflow)?;
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
        // If delta rounds to zero the issuers' part stays uncounted (it is
        // picked up by a later accrue); only the fee is counted now.
        let counted_for_issuers = if delta == 0 { 0 } else { to_issuers };
        self.yield_index = self.yield_index.checked_add(delta).ok_or(TenderError::Overflow)?;
        self.protocol_owed = self.protocol_owed.checked_add(fee).ok_or(TenderError::Overflow)?;
        let counted = counted_for_issuers + fee;
        self.unclaimed_yield = self.unclaimed_yield.checked_add(counted).ok_or(TenderError::Overflow)?;
        self.total_yield = self.total_yield.saturating_add(counted);
        Ok(())
    }
}

impl Issuer {
    pub fn settle(&mut self, index: u128) -> Result<()> {
        let earned = self.supply as u128 * (index - self.index_snapshot) / INDEX_SCALE;
        self.accrued = self
            .accrued
            .checked_add(u64::try_from(earned).map_err(|_| TenderError::Overflow)?)
            .ok_or(TenderError::Overflow)?;
        self.index_snapshot = index;
        Ok(())
    }

    fn set_routes(&mut self, routes: &[Route]) {
        self.routes = [Route::default(); MAX_ROUTES];
        self.routes[..routes.len()].copy_from_slice(routes);
        self.route_count = routes.len() as u8;
    }
}

fn validate_symbol(s: &str) -> Result<()> {
    require!(
        (2..=MAX_SYMBOL_LEN).contains(&s.len())
            && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
        TenderError::BadSymbol
    );
    Ok(())
}

fn validate_routes(routes: &[Route]) -> Result<()> {
    require!(
        !routes.is_empty() && routes.len() <= MAX_ROUTES,
        TenderError::BadRoutes
    );
    let mut total: u32 = 0;
    for r in routes {
        require!(r.bps > 0, TenderError::BadRoutes);
        require!(r.kind == ROUTE_USDC || r.kind == ROUTE_DOLLAR, TenderError::BadRoutes);
        require!(r.wallet != Pubkey::default(), TenderError::BadRoutes);
        total += r.bps as u32;
    }
    require!(total == BPS as u32, TenderError::BadRoutes);
    Ok(())
}

impl ReserveParams {
    fn validate(&self) -> Result<()> {
        require!(self.protocol_fee_bps <= MAX_PROTOCOL_FEE_BPS, TenderError::FeeTooHigh);
        require!(self.min_liquid_bps as u128 <= BPS, TenderError::BadParams);
        Ok(())
    }
}

// ---------------------------------------------------------------- state

#[account]
#[derive(InitSpace)]
pub struct Reserve {
    pub admin: Pubkey,
    /// Sets `tbill_price` (bounded; see `update_price`).
    pub price_authority: Pubkey,
    /// Swaps between USDC and Treasuries at the reserve's price.
    pub allocator: Pubkey,
    /// Wallet that receives the protocol fee (as USDC).
    pub treasury: Pubkey,
    pub usdc_mint: Pubkey,
    pub tbill_mint: Pubkey,
    pub usdc_vault: Pubkey,
    pub tbill_vault: Pubkey,
    pub tbill_price: u64,
    pub price_updated_at: i64,
    pub protocol_fee_bps: u16,
    /// Liquid USDC the allocator must leave behind, as bps of total supply.
    pub min_liquid_bps: u16,
    /// Stops new mints. Redemptions are never paused.
    pub paused: bool,
    /// Every community dollar outstanding, across all issuers.
    pub total_supply: u64,
    pub yield_index: u128,
    /// Yield counted (to issuers and protocol) but not yet paid out.
    pub unclaimed_yield: u64,
    pub protocol_owed: u64,
    /// Lifetime yield counted.
    pub total_yield: u64,
    pub issuer_count: u64,
    pub bump: u8,
    /// Most community dollars that may be outstanding across all issuers,
    /// in base units; 0 means no cap. Only `mint` checks it, so yield routed
    /// as new dollars and redemptions always go through.
    pub max_supply: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, InitSpace, PartialEq, Eq, Debug)]
pub struct Route {
    pub wallet: Pubkey,
    pub bps: u16,
    /// 0 = pay USDC, 1 = mint this community dollar.
    pub kind: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Issuer {
    pub reserve: Pubkey,
    pub authority: Pubkey,
    pub mint: Pubkey,
    #[max_len(32)]
    pub name: String,
    #[max_len(10)]
    pub symbol: String,
    pub supply: u64,
    pub index_snapshot: u128,
    pub accrued: u64,
    pub total_yield_paid: u64,
    pub routes: [Route; MAX_ROUTES],
    pub route_count: u8,
    pub created_at: i64,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct ReserveParams {
    pub price_authority: Pubkey,
    pub allocator: Pubkey,
    pub treasury: Pubkey,
    pub protocol_fee_bps: u16,
    pub min_liquid_bps: u16,
    pub paused: bool,
    /// Only read by `initialize_reserve`.
    pub tbill_price: u64,
    /// 0 = no cap.
    pub max_supply: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct CreateIssuerArgs {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub routes: Vec<Route>,
}

// ---------------------------------------------------------------- accounts

#[derive(Accounts)]
pub struct InitializeReserve<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Reserve::INIT_SPACE, seeds = [RESERVE_SEED], bump)]
    pub reserve: Account<'info, Reserve>,
    #[account(mint::token_program = usdc_token_program)]
    pub usdc_mint: InterfaceAccount<'info, Mint>,
    #[account(mint::token_program = tbill_token_program)]
    pub tbill_mint: InterfaceAccount<'info, Mint>,
    #[account(
        init, payer = admin, seeds = [USDC_VAULT_SEED], bump,
        token::mint = usdc_mint, token::authority = reserve, token::token_program = usdc_token_program
    )]
    pub usdc_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init, payer = admin, seeds = [TBILL_VAULT_SEED], bump,
        token::mint = tbill_mint, token::authority = reserve, token::token_program = tbill_token_program
    )]
    pub tbill_vault: InterfaceAccount<'info, TokenAccount>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub tbill_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [RESERVE_SEED], bump = reserve.bump, has_one = admin, has_one = usdc_vault, has_one = tbill_vault)]
    pub reserve: Account<'info, Reserve>,
    pub usdc_vault: InterfaceAccount<'info, TokenAccount>,
    pub tbill_vault: InterfaceAccount<'info, TokenAccount>,
}

#[derive(Accounts)]
pub struct AdminOnlyLight<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [RESERVE_SEED], bump = reserve.bump, has_one = admin)]
    pub reserve: Account<'info, Reserve>,
}

#[derive(Accounts)]
#[instruction(args: CreateIssuerArgs)]
pub struct CreateIssuer<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut, seeds = [RESERVE_SEED], bump = reserve.bump)]
    pub reserve: Account<'info, Reserve>,
    #[account(
        init, payer = creator, space = 8 + Issuer::INIT_SPACE,
        seeds = [ISSUER_SEED, args.symbol.as_bytes()], bump
    )]
    pub issuer: Account<'info, Issuer>,
    #[account(
        init, payer = creator, seeds = [MINT_SEED, issuer.key().as_ref()], bump,
        mint::decimals = DOLLAR_DECIMALS,
        mint::authority = issuer,
        mint::token_program = dollar_token_program,
        extensions::metadata_pointer::authority = issuer,
        extensions::metadata_pointer::metadata_address = mint,
    )]
    pub mint: InterfaceAccount<'info, Mint>,
    pub dollar_token_program: Program<'info, Token2022>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct IssuerAuthority<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority)]
    pub issuer: Account<'info, Issuer>,
}

#[derive(Accounts)]
pub struct Convert<'info> {
    pub user: Signer<'info>,
    #[account(
        mut, seeds = [RESERVE_SEED], bump = reserve.bump,
        has_one = usdc_mint, has_one = usdc_vault, has_one = tbill_vault
    )]
    pub reserve: Account<'info, Reserve>,
    #[account(mut, has_one = reserve, has_one = mint)]
    pub issuer: Account<'info, Issuer>,
    #[account(mut, mint::token_program = dollar_token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mint::token_program = usdc_token_program)]
    pub usdc_mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub usdc_vault: InterfaceAccount<'info, TokenAccount>,
    pub tbill_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = usdc_mint, token::authority = user, token::token_program = usdc_token_program)]
    pub user_usdc: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = user, token::token_program = dollar_token_program)]
    pub user_dollar: InterfaceAccount<'info, TokenAccount>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub dollar_token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct Harvest<'info> {
    #[account(
        mut, seeds = [RESERVE_SEED], bump = reserve.bump,
        has_one = usdc_mint, has_one = usdc_vault, has_one = tbill_vault
    )]
    pub reserve: Account<'info, Reserve>,
    #[account(mut, has_one = reserve, has_one = mint)]
    pub issuer: Account<'info, Issuer>,
    #[account(mut, mint::token_program = dollar_token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mint::token_program = usdc_token_program)]
    pub usdc_mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub usdc_vault: InterfaceAccount<'info, TokenAccount>,
    pub tbill_vault: InterfaceAccount<'info, TokenAccount>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub dollar_token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct CollectProtocolFee<'info> {
    #[account(
        mut, seeds = [RESERVE_SEED], bump = reserve.bump,
        has_one = usdc_mint, has_one = usdc_vault, has_one = tbill_vault
    )]
    pub reserve: Account<'info, Reserve>,
    #[account(mint::token_program = usdc_token_program)]
    pub usdc_mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub usdc_vault: InterfaceAccount<'info, TokenAccount>,
    pub tbill_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = usdc_mint, token::authority = reserve.treasury)]
    pub treasury_usdc: InterfaceAccount<'info, TokenAccount>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct UpdatePrice<'info> {
    pub price_authority: Signer<'info>,
    #[account(
        mut, seeds = [RESERVE_SEED], bump = reserve.bump,
        has_one = price_authority, has_one = usdc_vault, has_one = tbill_vault
    )]
    pub reserve: Account<'info, Reserve>,
    pub usdc_vault: InterfaceAccount<'info, TokenAccount>,
    pub tbill_vault: InterfaceAccount<'info, TokenAccount>,
}

#[derive(Accounts)]
pub struct Rebalance<'info> {
    pub allocator: Signer<'info>,
    #[account(
        mut, seeds = [RESERVE_SEED], bump = reserve.bump,
        has_one = allocator, has_one = usdc_mint, has_one = tbill_mint,
        has_one = usdc_vault, has_one = tbill_vault
    )]
    pub reserve: Account<'info, Reserve>,
    #[account(mint::token_program = usdc_token_program)]
    pub usdc_mint: InterfaceAccount<'info, Mint>,
    #[account(mint::token_program = tbill_token_program)]
    pub tbill_mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub usdc_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub tbill_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = usdc_mint, token::authority = allocator)]
    pub allocator_usdc: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = tbill_mint, token::authority = allocator)]
    pub allocator_tbill: InterfaceAccount<'info, TokenAccount>,
    pub usdc_token_program: Interface<'info, TokenInterface>,
    pub tbill_token_program: Interface<'info, TokenInterface>,
}

// ---------------------------------------------------------------- events & errors

#[event]
pub struct IssuerCreated {
    pub issuer: Pubkey,
    pub mint: Pubkey,
    pub symbol: String,
}

#[event]
pub struct Converted {
    pub issuer: Pubkey,
    pub user: Pubkey,
    pub amount: u64,
    pub minted: bool,
}

#[event]
pub struct Harvested {
    pub issuer: Pubkey,
    pub amount: u64,
    pub usdc_out: u64,
    pub dollars_minted: u64,
}

#[error_code]
pub enum TenderError {
    #[msg("Symbol must be 2-10 characters, A-Z and 0-9 only")]
    BadSymbol,
    #[msg("Name must be 1-32 characters")]
    BadName,
    #[msg("Metadata URI is too long")]
    BadUri,
    #[msg("Routes must be 1-4 wallets whose shares add up to 100%")]
    BadRoutes,
    #[msg("Route token accounts are missing or don't match the routes")]
    BadRouteAccounts,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Minting is paused")]
    Paused,
    #[msg("The reserve doesn't hold enough liquid USDC right now")]
    InsufficientLiquidity,
    #[msg("That would leave less liquid USDC than the reserve's floor")]
    BelowLiquidityFloor,
    #[msg("No yield to pay out yet")]
    NothingToHarvest,
    #[msg("Price must be positive and move at most 0.5% per update")]
    BadPrice,
    #[msg("The price was updated less than an hour ago")]
    PriceTooSoon,
    #[msg("Protocol fee is above the 20% cap")]
    FeeTooHigh,
    #[msg("Invalid reserve parameters")]
    BadParams,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("This would take total supply over the reserve's cap")]
    CapReached,
}
