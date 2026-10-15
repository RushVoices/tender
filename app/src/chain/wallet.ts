// Injected-wallet support (Phantom, Solflare and Backpack all expose this
// shape). Swap in @solana/wallet-adapter if you want a wallet picker.
import { Connection, PublicKey, Transaction, type TransactionInstruction } from "@solana/web3.js";

export interface InjectedWallet {
  publicKey: PublicKey | null;
  connect(opts?: { onlyIfTrusted?: boolean }): Promise<{ publicKey: PublicKey }>;
  disconnect(): Promise<void>;
  signTransaction<T extends Transaction>(tx: T): Promise<T>;
}

declare global {
  interface Window {
    phantom?: { solana?: InjectedWallet };
    solflare?: InjectedWallet;
    backpack?: InjectedWallet;
    solana?: InjectedWallet;
  }
}

let wallet: InjectedWallet | null = null;
let pubkey: PublicKey | null = null;

const provider = () => window.phantom?.solana ?? window.solflare ?? window.backpack ?? window.solana ?? null;

export async function connectWallet(silent = false): Promise<PublicKey | null> {
  const p = provider();
  if (!p) {
    if (silent) return null;
    throw new Error("No Solana wallet found. Install Phantom, Solflare or Backpack.");
  }
  const res = await p.connect(silent ? { onlyIfTrusted: true } : undefined);
  wallet = p;
  pubkey = new PublicKey((res?.publicKey ?? p.publicKey)!.toString());
  return pubkey;
}

export async function disconnectWallet() {
  await wallet?.disconnect().catch(() => {});
  wallet = null;
  pubkey = null;
}

export const walletKey = () => pubkey;

/** Signs one transaction in the wallet, sends it and waits for confirmation. */
export async function signAndSend(conn: Connection, ixs: TransactionInstruction[]): Promise<string> {
  if (!wallet || !pubkey) throw new Error("Connect a wallet first.");
  const { blockhash, lastValidBlockHeight } = await conn.getLatestBlockhash("confirmed");
  const tx = new Transaction({ feePayer: pubkey, blockhash, lastValidBlockHeight }).add(...ixs);
  const signed = await wallet.signTransaction(tx);
  const sig = await conn.sendRawTransaction(signed.serialize());
  const res = await conn.confirmTransaction({ signature: sig, blockhash, lastValidBlockHeight }, "confirmed");
  if (res.value.err) throw new Error(`Transaction failed: ${JSON.stringify(res.value.err)}`);
  return sig;
}
