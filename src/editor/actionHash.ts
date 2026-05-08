// FNV-1a 64-bit truncated to the low 32 bits as 8 hex chars. Mirrors
// Rust's `action_text_hash` (notes.rs:898-909) so the editor can look
// up actions by text hash without an IPC round-trip and the keys
// match the `bundle_id:hash` ids stored in the DB.

const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;
const U64_MASK = 0xffffffffffffffffn;

export function actionTextHash(text: string): string {
  let h = FNV_OFFSET;
  // Hash by UTF-8 byte to match Rust's `text.bytes()` exactly.
  const bytes = new TextEncoder().encode(text);
  for (let i = 0; i < bytes.length; i++) {
    h = (h ^ BigInt(bytes[i]!)) & U64_MASK;
    h = (h * FNV_PRIME) & U64_MASK;
  }
  // Rust does `h as u32` — keep the low 32 bits, format as 8-char hex.
  const low32 = Number(h & 0xffffffffn);
  return low32.toString(16).padStart(8, "0");
}
