// End-to-end check of the site's client (src/chain/tender.ts) against the
// real compiled program on a local validator:
//
//   solana-test-validator --reset --bpf-program Tnd4ZV5367L8r7DDxLH97bA64SkR4Ke1Lf6GZa9VwgJ ../target/deploy/tender.so
//   npm run e2e
//
// Creates USDC and Treasury test mints, sets up the reserve, issues a
// community dollar, mints, redeems, earns yield and harvests it, and checks
// every balance along the way.

import assert from "node:assert/strict";
import {
  Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, Transaction, sendAndConfirmTransaction,
  type TransactionInstruction,
} from "@solana/web3.js";
import {
  TOKEN_2022_PROGRAM_ID, createMint, getOrCreateAssociatedTokenAccount, mintTo, getTokenMetadata, getAccount,
} from "@solana/spl-token";
import { Tender, ROUTE_DOLLAR, ROUTE_USDC, harvestable, formatUsd, decodeIssuer } from "../src/chain/tender";

const RPC = process.env.RPC_URL ?? "http://127.0.0.1:8899";
const conn = new Connection(RPC, "confirmed");
const USD = 1_000_000n;

async function send(ixs: TransactionInstruction[], ...signers: Keypair[]) {
  return sendAndConfirmTransaction(conn, new Transaction().add(...ixs), signers, { commitment: "confirmed" });
}
async function funded() {
  const k = Keypair.generate();
  await conn.confirmTransaction(await conn.requestAirdrop(k.publicKey, 20 * LAMPORTS_PER_SOL), "confirmed");
  return k;
}
const bal = async (a: PublicKey, prog?: PublicKey) => (await getAccount(conn, a, "confirmed", prog)).amount;
const step = (s: string) => console.log("  ✓ " + s);

async function main() {
  const admin = await funded();
  const allocator = await funded();
  const treasury = Keypair.generate();
  const usdcMint = await createMint(conn, admin, admin.publicKey, null, 6);
  const tbillMint = await createMint(conn, admin, admin.publicKey, null, 6);
  const t = new Tender(conn);

  await send([t.initializeReserve(admin.publicKey, usdcMint, tbillMint, {
    priceAuthority: admin.publicKey, allocator: allocator.publicKey, treasury: treasury.publicKey,
    protocolFeeBps: 1000, minLiquidBps: 2000, paused: false, tbillPrice: 1_000_000_000n, maxSupply: 0n,
  })], admin);
  const reserve0 = (await t.fetchReserve())!;
  assert.ok(reserve0.usdcVault.equals(t.usdcVault));
  step("reserve initialized");

  const creator = await funded();
  const holders = Keypair.generate();
  await send([t.createIssuer(creator.publicKey, {
    name: "Bonk Dollar", symbol: "BONKUSD", uri: "https://example.com/bonkusd.json",
    routes: [
      { wallet: creator.publicKey, bps: 7000, kind: ROUTE_USDC },
      { wallet: holders.publicKey, bps: 3000, kind: ROUTE_DOLLAR },
    ],
  })], creator);
  const issuer = (await t.fetchIssuer("BONKUSD"))!;
  assert.equal(issuer.name, "Bonk Dollar");
  assert.equal(issuer.routes.length, 2);
  const meta = await getTokenMetadata(conn, issuer.mint, "confirmed", TOKEN_2022_PROGRAM_ID);
  assert.equal(meta?.name, "Bonk Dollar");
  assert.equal(meta?.symbol, "BONKUSD");
  step(`issuer BONKUSD created, mint ${issuer.mint.toBase58()} with on-chain metadata`);
  assert.equal((await t.fetchIssuers()).length, 1);
  step("issuer list via getProgramAccounts");

  const alice = await funded();
  const aliceUsdc = await getOrCreateAssociatedTokenAccount(conn, admin, usdcMint, alice.publicKey);
  await mintTo(conn, admin, usdcMint, aliceUsdc.address, admin, 5_000n * USD);
  await send(t.convert("mint", alice.publicKey, usdcMint, "BONKUSD", 5_000n * USD), alice);
  const aliceDollar = t.dollarAta(alice.publicKey, issuer.mint);
  assert.equal(await bal(aliceDollar, TOKEN_2022_PROGRAM_ID), 5_000n * USD);
  step("alice minted 5,000 BONKUSD for 5,000 USDC");

  await send(t.convert("redeem", alice.publicKey, usdcMint, "BONKUSD", 1_000n * USD), alice);
  assert.equal(await bal(aliceDollar, TOKEN_2022_PROGRAM_ID), 4_000n * USD);
  assert.equal(await bal(aliceUsdc.address), 1_000n * USD);
  step("alice redeemed 1,000 BONKUSD for 1,000 USDC");

  // allocator moves 3,000 USDC into Treasuries
  const allocTbill = await getOrCreateAssociatedTokenAccount(conn, admin, tbillMint, allocator.publicKey);
  await getOrCreateAssociatedTokenAccount(conn, admin, usdcMint, allocator.publicKey);
  await mintTo(conn, admin, tbillMint, allocTbill.address, admin, 10_000n * USD);
  await send([t.rebalance("invest", allocator.publicKey, (await t.fetchReserve())!, 3_000n * USD)], allocator);
  let v = await t.vaultBalances();
  assert.equal(v.usdc, 1_000n * USD);
  assert.equal(v.tbill, 3_000n * USD);
  step("allocator invested 3,000 USDC into Treasuries; 1,000 stays liquid");

  // 40 USDC of yield arrives (a coupon paid into the reserve)
  await mintTo(conn, admin, usdcMint, t.usdcVault, admin, 40n * USD);
  v = await t.vaultBalances();
  const r = (await t.fetchReserve())!;
  const preview = harvestable(r, (await t.fetchIssuer("BONKUSD"))!, v.usdc, v.tbill);
  assert.equal(preview, 36n * USD);
  step(`site preview says ${formatUsd(preview)} harvestable (40 minus 10% protocol)`);

  const keeper = await funded();
  await send(t.harvest(keeper.publicKey, usdcMint, issuer), keeper);
  const creatorUsdc = t.usdcAta(creator.publicKey, usdcMint);
  const holdersDollar = t.dollarAta(holders.publicKey, issuer.mint);
  assert.equal(await bal(creatorUsdc), 25_200_000n); // 70% of 36
  assert.equal(await bal(holdersDollar, TOKEN_2022_PROGRAM_ID), 10_800_000n); // 30%, as BONKUSD
  const after = decodeIssuer(issuer.address, (await conn.getAccountInfo(issuer.address))!.data as Buffer);
  assert.equal(after.totalYieldPaid, 36n * USD);
  assert.equal(after.supply, 4_000n * USD + 10_800_000n);
  step("keeper harvested: 25.20 USDC to the creator, 10.80 BONKUSD to the holders wallet");

  await getOrCreateAssociatedTokenAccount(conn, admin, usdcMint, treasury.publicKey);
  await send([t.collectProtocolFee((await t.fetchReserve())!)], keeper);
  assert.equal(await bal(t.usdcAta(treasury.publicKey, usdcMint)), 4n * USD);
  step("protocol fee of 4 USDC collected to the treasury");

  await send([t.rebalance("divest", allocator.publicKey, (await t.fetchReserve())!, 3_000n * USD)], allocator);
  await send(t.convert("redeem", alice.publicKey, usdcMint, "BONKUSD", 4_000n * USD), alice);
  assert.equal(await bal(aliceUsdc.address), 5_000n * USD);
  const end = (await t.fetchReserve())!;
  v = await t.vaultBalances();
  assert.ok(v.usdc + v.tbill >= end.totalSupply + end.unclaimedYield);
  step("alice redeemed the rest: got back all 5,000 USDC; reserve still covers every dollar");

  console.log("\nAll end-to-end checks passed.");
}

main().catch((e) => { console.error(e); process.exit(1); });
