// One-time reserve setup after deploying the program.
//
//   RPC_URL=https://api.devnet.solana.com npx tsx scripts/init-reserve.ts \
//     [--usdc <mint>] [--tbill <mint>] [--treasury <wallet>] [--price-authority <wallet>] \
//     [--allocator <wallet>] [--fee-bps 1000] [--min-liquid-bps 2000] [--tbill-price 1.0] \
//     [--keypair ~/.config/solana/id.json]
//
// Without --usdc / --tbill it creates test mints you control (devnet only),
// mints yourself 10,000 of each, and prints their addresses. The keypair
// becomes the reserve admin and, unless given, every other role.

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { Connection, Keypair, PublicKey, Transaction, sendAndConfirmTransaction } from "@solana/web3.js";
import { createMint, getOrCreateAssociatedTokenAccount, mintTo } from "@solana/spl-token";
import { Tender } from "../src/chain/tender";

const args = new Map<string, string>();
for (let i = 2; i < process.argv.length; i += 2) args.set(process.argv[i].replace(/^--/, ""), process.argv[i + 1]);

const rpc = process.env.RPC_URL ?? "https://api.devnet.solana.com";
const conn = new Connection(rpc, "confirmed");
const kpPath = (args.get("keypair") ?? "~/.config/solana/id.json").replace(/^~/, homedir());
const admin = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(kpPath, "utf8"))));
const key = (k: string) => (args.get(k) ? new PublicKey(args.get(k)!) : admin.publicKey);

async function testMint(label: string) {
  const mint = await createMint(conn, admin, admin.publicKey, null, 6);
  const ata = await getOrCreateAssociatedTokenAccount(conn, admin, mint, admin.publicKey);
  await mintTo(conn, admin, mint, ata.address, admin, 10_000_000_000n);
  console.log(`test ${label} mint: ${mint.toBase58()} (10,000 minted to you)`);
  return mint;
}

async function main() {
  const t = new Tender(conn, args.get("program") ? new PublicKey(args.get("program")!) : undefined);
  if (await t.fetchReserve()) throw new Error(`Reserve already exists at ${t.reserve.toBase58()}`);
  const usdc = args.get("usdc") ? new PublicKey(args.get("usdc")!) : await testMint("USDC");
  const tbill = args.get("tbill") ? new PublicKey(args.get("tbill")!) : await testMint("Treasury");
  const price = BigInt(Math.round(Number(args.get("tbill-price") ?? "1") * 1e9));
  const ix = t.initializeReserve(admin.publicKey, usdc, tbill, {
    priceAuthority: key("price-authority"),
    allocator: key("allocator"),
    treasury: key("treasury"),
    protocolFeeBps: Number(args.get("fee-bps") ?? 1000),
    minLiquidBps: Number(args.get("min-liquid-bps") ?? 2000),
    paused: false,
    tbillPrice: price,
  });
  const sig = await sendAndConfirmTransaction(conn, new Transaction().add(ix), [admin]);
  console.log(`reserve: ${t.reserve.toBase58()}\nusdc vault: ${t.usdcVault.toBase58()}\ntx: ${sig}`);
}

main().catch((e) => { console.error(e); process.exit(1); });
