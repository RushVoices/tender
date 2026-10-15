# Tender

**Issue your community's own dollar.** A memecoin, an app, a DAO or a creator names a dollar on Solana (`BONKUSD`, `DAOUSD`…). Anyone can mint it 1:1 with USDC and redeem it the same way. Every Tender dollar sits on one shared reserve of USDC and tokenized Treasuries, and each issuer sends its share of the yield wherever it chooses.

> **Status:** the program compiles, passes 14 end-to-end tests in LiteSVM, and the site's client passes a full run against a local validator. It has **not been audited**. Use devnet until it has.

| Path | What it is |
| --- | --- |
| `programs/tender/` | The Solana program (Rust, Anchor 0.31): reserve, issuers, mint/redeem, yield index, routes, allocator, price bounds. |
| `programs/tender/tests/integration.rs` | 14 end-to-end tests against the compiled `.so` with the real SPL Token and Token-2022 programs. |
| `app/` | The site (Vite + TypeScript + three.js): landing page with a 3D coin and a live reserve simulation, the app (`app.html`), and the docs (`docs.html`). |
| `app/src/chain/tender.ts` | Client with no Anchor dependency: PDAs, instruction builders, account decoders, and the yield math mirrored from the program. |
| `app/scripts/e2e.ts` | Runs the client against a local validator: issue, mint, redeem, invest, yield, harvest, protocol fee. |
| `app/scripts/init-reserve.ts` | One-time reserve setup after deploying. |

## Live on devnet

| | Address |
| --- | --- |
| Program | `Tnd4ZV5367L8r7DDxLH97bA64SkR4Ke1Lf6GZa9VwgJ` |
| Reserve | `9C4Tf22GakAo6STmU5JnrgFbTqJ5mgGKnWd2aEcqZyAa` |
| USDC (Circle devnet) | `4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU` |
| Test Treasury token | `4LRrwJ6SwiP138wDcqzZQDBiKLLAsaSFpXCc5DMCSbs6` |
| Admin / allocator / price authority | `DKaTHhGvt4b9gEqKFPtUyaoaBMJLTdfgDwR76cbH8BWB` |

Get test USDC at [faucet.circle.com](https://faucet.circle.com) (Solana Devnet) and test SOL at [faucet.solana.com](https://faucet.solana.com).

**Host the site on Vercel:** Add New → Project → this repo → Root Directory `tender/app`. No environment variables are needed; it defaults to devnet and the program above.

The devnet build uses `--features no-idl,no-log-ix-name` to keep the binary smaller (466 KB), which lowers the SOL needed to deploy.

## How it works (short)

- **Issuer:** `create_issuer` makes a Token-2022 mint (on-chain name and symbol, no freeze authority) whose only mint authority is the program, plus 1–4 yield routes.
- **Mint / redeem:** 1 USDC in = 1 dollar out, and back. No fee. Redemptions can't be paused.
- **Reserve:** liquid USDC + a tokenized Treasury valued at `tbill_price`. The allocator swaps between them at that price, and the program won't let it go below a liquidity floor. The price authority can move the price by at most 0.5% an hour.
- **Yield:** whatever the reserve holds above what it owes is yield. A 1e18-scaled `yield_index` splits it by each issuer's outstanding supply over time. Permissionless `harvest` pays each route as USDC or as newly minted units of the issuer's dollar.

Full docs are in `app/docs.html` (served at `/docs.html`).

## Build and test

Needs Rust, Solana CLI 2.2 (Agave), and Node 20+.

```bash
cargo build-sbf --manifest-path programs/tender/Cargo.toml
cargo test -p tender --test integration        # 14 tests

# client against the real program on a local validator
solana-test-validator --reset --bpf-program Tnd4ZV5367L8r7DDxLH97bA64SkR4Ke1Lf6GZa9VwgJ target/deploy/tender.so
cd app && npm install && npm run e2e
```

If `cargo build-sbf` complains about `edition2024`, a dependency has outgrown the SBF toolchain's Cargo. The committed `Cargo.lock` pins versions that build, so keep it.

## Deploy (devnet)

```bash
solana program deploy target/deploy/tender.so --program-id keys/tender-program-keypair.json
cd app
RPC_URL=https://api.devnet.solana.com npx tsx scripts/init-reserve.ts   # creates test USDC/Treasury mints
cp .env.example .env && npm run dev
```

`keys/` is gitignored. To use your own program id, run `solana-keygen new -o keys/tender-program-keypair.json` and replace `Tnd4ZV5…` in `programs/tender/src/lib.rs`, `Anchor.toml` and `app/src/chain/tender.ts` (or set `VITE_PROGRAM_ID`).

## Before mainnet

1. Get it audited.
2. Put the admin, price authority, allocator and program upgrade authority behind a multisig.
3. Replace the trusted price signer with an oracle or NAV feed for the Treasury token you use (e.g. USDY), and connect the allocator to a real subscribe/redeem flow.
4. Get legal advice. Dollar-like tokens and passed-through Treasury yield are regulated in many places.
