// Client for the Tender program: addresses, instruction builders and account
// decoders. No Anchor runtime needed; the encodings below mirror
// programs/tender/src/lib.rs exactly (Borsh, 8-byte Anchor discriminators).
// Used by the site and by scripts/e2e.ts, which runs it against a local
// validator.

import { Buffer } from "buffer";
import { sha256 } from "@noble/hashes/sha256";
import {
  PublicKey,
  SystemProgram,
  TransactionInstruction,
  type AccountMeta,
  type Connection,
} from "@solana/web3.js";
import {
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountIdempotentInstruction,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";

export const PRICE_SCALE = 1_000_000_000n;
export const INDEX_SCALE = 1_000_000_000_000_000_000n;
export const BPS = 10_000n;
export const DECIMALS = 6;
export const ROUTE_USDC = 0;
export const ROUTE_DOLLAR = 1;

export const DEFAULT_PROGRAM_ID = new PublicKey("Tnd4ZV5367L8r7DDxLH97bA64SkR4Ke1Lf6GZa9VwgJ");

// ---- encoding ----------------------------------------------------------------

const disc = (s: string) => Buffer.from(sha256(new TextEncoder().encode(s))).subarray(0, 8);
const ixDisc = (name: string) => disc(`global:${name}`);
export const ACCOUNT_DISC = { Reserve: disc("account:Reserve"), Issuer: disc("account:Issuer") };

class W {
  parts: Buffer[] = [];
  u8(v: number) { this.parts.push(Buffer.from([v])); return this; }
  bool(v: boolean) { return this.u8(v ? 1 : 0); }
  u16(v: number) { const b = Buffer.alloc(2); b.writeUInt16LE(v); this.parts.push(b); return this; }
  u32(v: number) { const b = Buffer.alloc(4); b.writeUInt32LE(v); this.parts.push(b); return this; }
  u64(v: bigint) { const b = Buffer.alloc(8); b.writeBigUInt64LE(v); this.parts.push(b); return this; }
  key(k: PublicKey) { this.parts.push(k.toBuffer()); return this; }
  str(s: string) { const b = Buffer.from(s, "utf8"); this.u32(b.length); this.parts.push(b); return this; }
  routes(rs: Route[]) {
    this.u32(rs.length);
    for (const r of rs) this.key(r.wallet).u16(r.bps).u8(r.kind);
    return this;
  }
  done(name: string) { return Buffer.concat([ixDisc(name), ...this.parts]); }
}

class R {
  constructor(public d: Buffer, public o = 8) {}
  u8() { return this.d[this.o++]; }
  bool() { return this.u8() !== 0; }
  u16() { const v = this.d.readUInt16LE(this.o); this.o += 2; return v; }
  u32() { const v = this.d.readUInt32LE(this.o); this.o += 4; return v; }
  u64() { const v = this.d.readBigUInt64LE(this.o); this.o += 8; return v; }
  i64() { const v = this.d.readBigInt64LE(this.o); this.o += 8; return v; }
  u128() { const lo = this.u64(); const hi = this.u64(); return (hi << 64n) | lo; }
  key() { const k = new PublicKey(this.d.subarray(this.o, this.o + 32)); this.o += 32; return k; }
  str() { const n = this.u32(); const s = this.d.subarray(this.o, this.o + n).toString("utf8"); this.o += n; return s; }
}

// ---- types -------------------------------------------------------------------

export interface Route { wallet: PublicKey; bps: number; kind: number }

export interface ReserveParams {
  priceAuthority: PublicKey;
  allocator: PublicKey;
  treasury: PublicKey;
  protocolFeeBps: number;
  minLiquidBps: number;
  paused: boolean;
  tbillPrice: bigint;
}

export interface ReserveState {
  admin: PublicKey; priceAuthority: PublicKey; allocator: PublicKey; treasury: PublicKey;
  usdcMint: PublicKey; tbillMint: PublicKey; usdcVault: PublicKey; tbillVault: PublicKey;
  tbillPrice: bigint; priceUpdatedAt: bigint; protocolFeeBps: number; minLiquidBps: number;
  paused: boolean; totalSupply: bigint; yieldIndex: bigint; unclaimedYield: bigint;
  protocolOwed: bigint; totalYield: bigint; issuerCount: bigint; bump: number;
}

export interface IssuerState {
  address: PublicKey;
  reserve: PublicKey; authority: PublicKey; mint: PublicKey; name: string; symbol: string;
  supply: bigint; indexSnapshot: bigint; accrued: bigint; totalYieldPaid: bigint;
  routes: Route[]; createdAt: bigint; bump: number;
}

export function decodeReserve(data: Buffer): ReserveState {
  if (!data.subarray(0, 8).equals(ACCOUNT_DISC.Reserve)) throw new Error("Not a Reserve account");
  const r = new R(data);
  return {
    admin: r.key(), priceAuthority: r.key(), allocator: r.key(), treasury: r.key(),
    usdcMint: r.key(), tbillMint: r.key(), usdcVault: r.key(), tbillVault: r.key(),
    tbillPrice: r.u64(), priceUpdatedAt: r.i64(), protocolFeeBps: r.u16(), minLiquidBps: r.u16(),
    paused: r.bool(), totalSupply: r.u64(), yieldIndex: r.u128(), unclaimedYield: r.u64(),
    protocolOwed: r.u64(), totalYield: r.u64(), issuerCount: r.u64(), bump: r.u8(),
  };
}

export function decodeIssuer(address: PublicKey, data: Buffer): IssuerState {
  if (!data.subarray(0, 8).equals(ACCOUNT_DISC.Issuer)) throw new Error("Not an Issuer account");
  const r = new R(data);
  const reserve = r.key(), authority = r.key(), mint = r.key();
  const name = r.str(), symbol = r.str();
  const supply = r.u64(), indexSnapshot = r.u128(), accrued = r.u64(), totalYieldPaid = r.u64();
  const all: Route[] = [];
  for (let i = 0; i < 4; i++) all.push({ wallet: r.key(), bps: r.u16(), kind: r.u8() });
  const count = r.u8();
  return {
    address, reserve, authority, mint, name, symbol, supply, indexSnapshot, accrued, totalYieldPaid,
    routes: all.slice(0, count), createdAt: r.i64(), bump: r.u8(),
  };
}

// ---- yield math (mirrors Reserve::accrue / Issuer::settle) ---------------------

export function reserveValue(r: ReserveState, usdc: bigint, tbill: bigint) {
  return usdc + (tbill * r.tbillPrice) / PRICE_SCALE;
}

/** The index `accrue` would produce right now. */
export function projectedIndex(r: ReserveState, usdc: bigint, tbill: bigint): bigint {
  if (r.totalSupply === 0n) return r.yieldIndex;
  const value = reserveValue(r, usdc, tbill);
  const owed = r.totalSupply + r.unclaimedYield;
  if (value <= owed) return r.yieldIndex;
  const surplus = value - owed;
  const fee = (surplus * BigInt(r.protocolFeeBps)) / BPS;
  return r.yieldIndex + ((surplus - fee) * INDEX_SCALE) / r.totalSupply;
}

/** What `harvest` would pay this issuer right now. */
export function harvestable(r: ReserveState, i: IssuerState, usdc: bigint, tbill: bigint): bigint {
  return i.accrued + (i.supply * (projectedIndex(r, usdc, tbill) - i.indexSnapshot)) / INDEX_SCALE;
}

// ---- client --------------------------------------------------------------------

export class Tender {
  readonly reserve: PublicKey;
  readonly usdcVault: PublicKey;
  readonly tbillVault: PublicKey;

  constructor(
    readonly connection: Connection,
    readonly programId: PublicKey = DEFAULT_PROGRAM_ID,
    readonly usdcTokenProgram: PublicKey = TOKEN_PROGRAM_ID,
    readonly tbillTokenProgram: PublicKey = TOKEN_PROGRAM_ID,
  ) {
    this.reserve = this.pda([Buffer.from("reserve")]);
    this.usdcVault = this.pda([Buffer.from("usdc_vault")]);
    this.tbillVault = this.pda([Buffer.from("tbill_vault")]);
  }

  pda(seeds: Buffer[]) { return PublicKey.findProgramAddressSync(seeds, this.programId)[0]; }
  issuerAddress(symbol: string) { return this.pda([Buffer.from("issuer"), Buffer.from(symbol)]); }
  mintAddress(issuer: PublicKey) { return this.pda([Buffer.from("mint"), issuer.toBuffer()]); }
  dollarAta(owner: PublicKey, mint: PublicKey) {
    return getAssociatedTokenAddressSync(mint, owner, true, TOKEN_2022_PROGRAM_ID);
  }
  usdcAta(owner: PublicKey, usdcMint: PublicKey) {
    return getAssociatedTokenAddressSync(usdcMint, owner, true, this.usdcTokenProgram);
  }

  private ix(keys: AccountMeta[], data: Buffer) {
    return new TransactionInstruction({ programId: this.programId, keys, data });
  }

  // ---- reads

  async fetchReserve(): Promise<ReserveState | null> {
    const acc = await this.connection.getAccountInfo(this.reserve);
    return acc ? decodeReserve(acc.data as Buffer) : null;
  }

  async fetchIssuer(symbol: string): Promise<IssuerState | null> {
    const addr = this.issuerAddress(symbol);
    const acc = await this.connection.getAccountInfo(addr);
    return acc ? decodeIssuer(addr, acc.data as Buffer) : null;
  }

  async fetchIssuers(): Promise<IssuerState[]> {
    const accs = await this.connection.getProgramAccounts(this.programId, {
      filters: [{ memcmp: { offset: 0, bytes: bs58(ACCOUNT_DISC.Issuer) } }],
    });
    return accs
      .map((a) => decodeIssuer(a.pubkey, a.account.data as Buffer))
      .sort((a, b) => (b.supply > a.supply ? 1 : -1));
  }

  async vaultBalances(): Promise<{ usdc: bigint; tbill: bigint }> {
    const [u, t] = await Promise.all([
      this.connection.getTokenAccountBalance(this.usdcVault),
      this.connection.getTokenAccountBalance(this.tbillVault),
    ]);
    return { usdc: BigInt(u.value.amount), tbill: BigInt(t.value.amount) };
  }

  // ---- instructions

  initializeReserve(admin: PublicKey, usdcMint: PublicKey, tbillMint: PublicKey, p: ReserveParams) {
    return this.ix(
      [
        { pubkey: admin, isSigner: true, isWritable: true },
        { pubkey: this.reserve, isSigner: false, isWritable: true },
        { pubkey: usdcMint, isSigner: false, isWritable: false },
        { pubkey: tbillMint, isSigner: false, isWritable: false },
        { pubkey: this.usdcVault, isSigner: false, isWritable: true },
        { pubkey: this.tbillVault, isSigner: false, isWritable: true },
        { pubkey: this.usdcTokenProgram, isSigner: false, isWritable: false },
        { pubkey: this.tbillTokenProgram, isSigner: false, isWritable: false },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      ],
      encodeParams(new W(), p).done("initialize_reserve"),
    );
  }

  createIssuer(creator: PublicKey, args: { name: string; symbol: string; uri: string; routes: Route[] }) {
    const issuer = this.issuerAddress(args.symbol);
    return this.ix(
      [
        { pubkey: creator, isSigner: true, isWritable: true },
        { pubkey: this.reserve, isSigner: false, isWritable: true },
        { pubkey: issuer, isSigner: false, isWritable: true },
        { pubkey: this.mintAddress(issuer), isSigner: false, isWritable: true },
        { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      ],
      new W().str(args.name).str(args.symbol).str(args.uri).routes(args.routes).done("create_issuer"),
    );
  }

  setRoutes(authority: PublicKey, issuer: PublicKey, routes: Route[]) {
    return this.ix(
      [
        { pubkey: authority, isSigner: true, isWritable: false },
        { pubkey: issuer, isSigner: false, isWritable: true },
      ],
      new W().routes(routes).done("set_routes"),
    );
  }

  /**
   * Mint or redeem. Returns the instructions to send, including idempotent
   * creation of the user's dollar account so a first mint is one click.
   */
  convert(kind: "mint" | "redeem", user: PublicKey, usdcMint: PublicKey, symbol: string, amount: bigint) {
    const issuer = this.issuerAddress(symbol);
    const mint = this.mintAddress(issuer);
    const userUsdc = this.usdcAta(user, usdcMint);
    const userDollar = this.dollarAta(user, mint);
    return [
      createAssociatedTokenAccountIdempotentInstruction(user, userDollar, user, mint, TOKEN_2022_PROGRAM_ID),
      createAssociatedTokenAccountIdempotentInstruction(user, userUsdc, user, usdcMint, this.usdcTokenProgram),
      this.ix(
        [
          { pubkey: user, isSigner: true, isWritable: false },
          { pubkey: this.reserve, isSigner: false, isWritable: true },
          { pubkey: issuer, isSigner: false, isWritable: true },
          { pubkey: mint, isSigner: false, isWritable: true },
          { pubkey: usdcMint, isSigner: false, isWritable: false },
          { pubkey: this.usdcVault, isSigner: false, isWritable: true },
          { pubkey: this.tbillVault, isSigner: false, isWritable: false },
          { pubkey: userUsdc, isSigner: false, isWritable: true },
          { pubkey: userDollar, isSigner: false, isWritable: true },
          { pubkey: this.usdcTokenProgram, isSigner: false, isWritable: false },
          { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
        ],
        new W().u64(amount).done(kind),
      ),
    ];
  }

  /** Permissionless harvest, creating each route's token account if needed. */
  harvest(payer: PublicKey, usdcMint: PublicKey, issuer: IssuerState) {
    const pre: TransactionInstruction[] = [];
    const routeAccounts: AccountMeta[] = issuer.routes.map((r) => {
      const usdc = r.kind === ROUTE_USDC;
      const mint = usdc ? usdcMint : issuer.mint;
      const prog = usdc ? this.usdcTokenProgram : TOKEN_2022_PROGRAM_ID;
      const ata = getAssociatedTokenAddressSync(mint, r.wallet, true, prog);
      pre.push(createAssociatedTokenAccountIdempotentInstruction(payer, ata, r.wallet, mint, prog));
      return { pubkey: ata, isSigner: false, isWritable: true };
    });
    return [
      ...pre,
      this.ix(
        [
          { pubkey: this.reserve, isSigner: false, isWritable: true },
          { pubkey: issuer.address, isSigner: false, isWritable: true },
          { pubkey: issuer.mint, isSigner: false, isWritable: true },
          { pubkey: usdcMint, isSigner: false, isWritable: false },
          { pubkey: this.usdcVault, isSigner: false, isWritable: true },
          { pubkey: this.tbillVault, isSigner: false, isWritable: false },
          { pubkey: this.usdcTokenProgram, isSigner: false, isWritable: false },
          { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
          ...routeAccounts,
        ],
        new W().done("harvest"),
      ),
    ];
  }

  collectProtocolFee(r: ReserveState) {
    return this.ix(
      [
        { pubkey: this.reserve, isSigner: false, isWritable: true },
        { pubkey: r.usdcMint, isSigner: false, isWritable: false },
        { pubkey: this.usdcVault, isSigner: false, isWritable: true },
        { pubkey: this.tbillVault, isSigner: false, isWritable: false },
        { pubkey: this.usdcAta(r.treasury, r.usdcMint), isSigner: false, isWritable: true },
        { pubkey: this.usdcTokenProgram, isSigner: false, isWritable: false },
      ],
      new W().done("collect_protocol_fee"),
    );
  }

  updatePrice(priceAuthority: PublicKey, newPrice: bigint) {
    return this.ix(
      [
        { pubkey: priceAuthority, isSigner: true, isWritable: false },
        { pubkey: this.reserve, isSigner: false, isWritable: true },
        { pubkey: this.usdcVault, isSigner: false, isWritable: false },
        { pubkey: this.tbillVault, isSigner: false, isWritable: false },
      ],
      new W().u64(newPrice).done("update_price"),
    );
  }

  rebalance(kind: "invest" | "divest", allocator: PublicKey, r: ReserveState, tbillAmount: bigint) {
    return this.ix(
      [
        { pubkey: allocator, isSigner: true, isWritable: false },
        { pubkey: this.reserve, isSigner: false, isWritable: true },
        { pubkey: r.usdcMint, isSigner: false, isWritable: false },
        { pubkey: r.tbillMint, isSigner: false, isWritable: false },
        { pubkey: this.usdcVault, isSigner: false, isWritable: true },
        { pubkey: this.tbillVault, isSigner: false, isWritable: true },
        { pubkey: this.usdcAta(allocator, r.usdcMint), isSigner: false, isWritable: true },
        { pubkey: getAssociatedTokenAddressSync(r.tbillMint, allocator, true, this.tbillTokenProgram), isSigner: false, isWritable: true },
        { pubkey: this.usdcTokenProgram, isSigner: false, isWritable: false },
        { pubkey: this.tbillTokenProgram, isSigner: false, isWritable: false },
      ],
      new W().u64(tbillAmount).done(kind),
    );
  }
}

function encodeParams(w: W, p: ReserveParams) {
  return w.key(p.priceAuthority).key(p.allocator).key(p.treasury)
    .u16(p.protocolFeeBps).u16(p.minLiquidBps).bool(p.paused).u64(p.tbillPrice);
}

// ---- helpers -----------------------------------------------------------------------

const B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
export function bs58(bytes: Uint8Array): string {
  let n = 0n;
  for (const b of bytes) n = n * 256n + BigInt(b);
  let s = "";
  while (n > 0n) { s = B58[Number(n % 58n)] + s; n /= 58n; }
  for (const b of bytes) { if (b !== 0) break; s = "1" + s; }
  return s;
}

/** "12.5" -> 12_500_000n (6 decimals). */
export function parseUsd(s: string): bigint {
  const m = s.trim().replace(/,/g, "").match(/^(\d*)(?:\.(\d{0,6})\d*)?$/);
  if (!m || (!m[1] && !m[2])) throw new Error("Enter an amount like 100 or 12.50");
  return BigInt(m[1] || "0") * 1_000_000n + BigInt((m[2] || "").padEnd(6, "0") || "0");
}

export function formatUsd(v: bigint, dp = 2): string {
  const neg = v < 0n; if (neg) v = -v;
  const whole = v / 1_000_000n;
  const frac = (v % 1_000_000n).toString().padStart(6, "0").slice(0, dp);
  return (neg ? "-" : "") + whole.toLocaleString("en-US") + (dp ? "." + frac : "");
}

export function validSymbol(s: string) { return /^[A-Z0-9]{2,10}$/.test(s); }

/** Program error messages, in the order of `TenderError` (codes 6000+). */
export const ERRORS = [
  "Symbol must be 2-10 characters, A-Z and 0-9 only",
  "Name must be 1-32 characters",
  "Metadata URI is too long",
  "Routes must be 1-4 wallets whose shares add up to 100%",
  "Route token accounts are missing or don't match the routes",
  "Amount must be greater than zero",
  "Minting is paused",
  "The reserve doesn't hold enough liquid USDC right now",
  "That would leave less liquid USDC than the reserve's floor",
  "No yield to pay out yet",
  "Price must be positive and move at most 0.5% per update",
  "The price was updated less than an hour ago",
  "Protocol fee is above the 20% cap",
  "Invalid reserve parameters",
  "Arithmetic overflow",
];

/** Turns a failed send into one readable sentence. */
export function friendlyError(e: unknown): string {
  const err = e as { message?: string; logs?: string[] };
  const text = [err?.message ?? String(e), ...(err?.logs ?? [])].join("\n");
  const anchor = text.match(/Error Message: ([^\n]+?)\.?(\n|$)/);
  if (anchor) return anchor[1];
  const hex = text.match(/custom program error: 0x([0-9a-f]+)/i);
  if (hex) {
    const code = parseInt(hex[1], 16);
    if (code >= 6000 && ERRORS[code - 6000]) return ERRORS[code - 6000];
    if (code === 1) return "Not enough tokens in your account";
    if (code === 0) return "That account already exists (is the symbol taken?)";
  }
  if (/already in use/.test(text)) return "That symbol is already taken";
  if (/insufficient (funds|lamports)/i.test(text) || /0x1\b/.test(text)) return "Not enough balance for this";
  if (/User rejected|rejected the request/i.test(text)) return "You cancelled in the wallet";
  return (err?.message ?? String(e)).split("\n")[0].slice(0, 200);
}
