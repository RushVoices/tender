// Must be the first import: some Solana libraries expect a global Buffer.
import { Buffer } from "buffer";
(globalThis as unknown as { Buffer: typeof Buffer }).Buffer ??= Buffer;
