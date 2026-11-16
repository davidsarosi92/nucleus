// Client-side encrypted columns (nucleus PLAN.md §M14.30 K5, DOCS.md §11):
// a value the driver encrypts before it is sent and decrypts after it
// arrives, so the server stores bytea ciphertext and never holds the key.
// Equality is queryable through a blind index stored beside it; nothing
// else is.  Format 1, every driver's (drivers/encrypted-columns.txt holds
// its test vectors):
//
// - keys: HMAC-SHA-256(master, "nucleus column 1 encrypt\0" + name) and
//   HMAC-SHA-256(master, "nucleus column 1 index\0" + name);
// - a value: a type byte (0 bytes, 1 text in UTF-8, 2 an integer: 8 bytes
//   little-endian) and its bytes;
// - ciphertext: the byte 1, a random 24-byte nonce, XChaCha20-Poly1305 of
//   the value (the byte 1 authenticated beside it);
// - blind index: the first 16 bytes of HMAC-SHA-256 of the value under the
//   index key.
//
//   const key = await ColumnKey.create(master, "users.email");  // master: 32 bytes you keep
//   await db.query("INSERT INTO users VALUES ($1, $2, $3)",
//     [1, key.encrypt("ann@example.com"), await key.blindIndex("ann@example.com")]);
//   const { rows } = await db.query("SELECT email FROM users WHERE email_index = $1",
//     [await key.blindIndex("ann@example.com")]);
//   key.decrypt(rows[0][0] as Buffer);                         // "ann@example.com"
//
// ChaCha20 and Poly1305 written here (WebCrypto has neither), as RFC 8439
// writes them; HChaCha20 as draft-irtf-cfrg-xchacha does.  HMAC-SHA-256
// is WebCrypto's.

import { Buffer } from "./bytes.ts";

/** The format's version: a ciphertext's first byte. */
export const COLUMN_FORMAT = 1;
/** A blind index's length. */
export const INDEX_LEN = 16;

/** A value an encrypted column holds: bytes, a text or an integer. */
export type PlainValue = Uint8Array | string | number | bigint;

const SIGMA = [0x61707865, 0x3320646e, 0x79622d32, 0x6b206574];

function rounds(s: Uint32Array): void {
  const q = (a: number, b: number, c: number, d: number) => {
    s[a] = s[a] + s[b];
    s[d] = ((s[d] ^ s[a]) << 16) | ((s[d] ^ s[a]) >>> 16);
    s[c] = s[c] + s[d];
    s[b] = ((s[b] ^ s[c]) << 12) | ((s[b] ^ s[c]) >>> 20);
    s[a] = s[a] + s[b];
    s[d] = ((s[d] ^ s[a]) << 8) | ((s[d] ^ s[a]) >>> 24);
    s[c] = s[c] + s[d];
    s[b] = ((s[b] ^ s[c]) << 7) | ((s[b] ^ s[c]) >>> 25);
  };
  for (let i = 0; i < 10; i++) {
    q(0, 4, 8, 12);
    q(1, 5, 9, 13);
    q(2, 6, 10, 14);
    q(3, 7, 11, 15);
    q(0, 5, 10, 15);
    q(1, 6, 11, 12);
    q(2, 7, 8, 13);
    q(3, 4, 9, 14);
  }
}

const le32 = (b: Uint8Array, at: number) => (b[at] | (b[at + 1] << 8) | (b[at + 2] << 16) | (b[at + 3] << 24)) >>> 0;

function state(key: Uint8Array, words: Uint8Array): Uint32Array {
  const s = new Uint32Array(16);
  s.set(SIGMA);
  for (let i = 0; i < 8; i++) s[4 + i] = le32(key, 4 * i);
  for (let i = 0; i < 4; i++) s[12 + i] = le32(words, 4 * i);
  return s;
}

function hchacha20(key: Uint8Array, nonce16: Uint8Array): Uint8Array {
  const s = state(key, nonce16);
  rounds(s);
  const out = new Uint8Array(32);
  const v = new DataView(out.buffer);
  for (let i = 0; i < 4; i++) {
    v.setUint32(4 * i, s[i], true);
    v.setUint32(16 + 4 * i, s[12 + i], true);
  }
  return out;
}

function chacha20(key: Uint8Array, counter: number, nonce12: Uint8Array, data: Uint8Array): Uint8Array {
  const words = new Uint8Array(16);
  words.set(nonce12, 4);
  const init = state(key, words);
  const out = Uint8Array.from(data);
  const block = new Uint8Array(64);
  const v = new DataView(block.buffer);
  for (let at = 0; at < data.length; at += 64) {
    init[12] = counter + at / 64;
    const s = Uint32Array.from(init);
    rounds(s);
    for (let i = 0; i < 16; i++) v.setUint32(4 * i, s[i] + init[i], true);
    for (let i = 0; i < 64 && at + i < data.length; i++) out[at + i] ^= block[i];
  }
  return out;
}

function leInt(b: Uint8Array): bigint {
  let n = 0n;
  for (let i = b.length - 1; i >= 0; i--) n = (n << 8n) | BigInt(b[i]);
  return n;
}

function poly1305(key: Uint8Array, msg: Uint8Array): Uint8Array {
  const r = leInt(key.subarray(0, 16)) & 0x0ffffffc0ffffffc0ffffffc0fffffffn;
  const s = leInt(key.subarray(16, 32));
  const p = (1n << 130n) - 5n;
  let acc = 0n;
  for (let at = 0; at < msg.length; at += 16) {
    const chunk = msg.subarray(at, at + 16);
    acc = ((acc + leInt(chunk) + (1n << BigInt(8 * chunk.length))) * r) % p;
  }
  let t = (acc + s) & ((1n << 128n) - 1n);
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i++) {
    out[i] = Number(t & 0xffn);
    t >>= 8n;
  }
  return out;
}

const pad16 = (n: number) => (16 - (n % 16)) % 16;

function tag(key: Uint8Array, nonce12: Uint8Array, aad: Uint8Array, ct: Uint8Array): Uint8Array {
  const otk = chacha20(key, 0, nonce12, new Uint8Array(32));
  const mac = new Uint8Array(aad.length + pad16(aad.length) + ct.length + pad16(ct.length) + 16);
  mac.set(aad, 0);
  const at = aad.length + pad16(aad.length);
  mac.set(ct, at);
  const v = new DataView(mac.buffer);
  v.setBigUint64(mac.length - 16, BigInt(aad.length), true);
  v.setBigUint64(mac.length - 8, BigInt(ct.length), true);
  return poly1305(otk, mac);
}

function xkey(key: Uint8Array, nonce24: Uint8Array): [Uint8Array, Uint8Array] {
  const n = new Uint8Array(12);
  n.set(nonce24.subarray(16, 24), 4);
  return [hchacha20(key, nonce24.subarray(0, 16)), n];
}

/** XChaCha20-Poly1305: the ciphertext and its 16-byte tag. */
export function xseal(key: Uint8Array, nonce24: Uint8Array, aad: Uint8Array, plaintext: Uint8Array): Uint8Array {
  const [k, n] = xkey(key, nonce24);
  const ct = chacha20(k, 1, n, plaintext);
  const out = new Uint8Array(ct.length + 16);
  out.set(ct, 0);
  out.set(tag(k, n, aad, ct), ct.length);
  return out;
}

/** What xseal made, or null when it does not open. */
export function xopen(key: Uint8Array, nonce24: Uint8Array, aad: Uint8Array, sealed: Uint8Array): Uint8Array | null {
  if (sealed.length < 16) return null;
  const [k, n] = xkey(key, nonce24);
  const ct = sealed.subarray(0, sealed.length - 16);
  const want = tag(k, n, aad, ct);
  let diff = 0;
  for (let i = 0; i < 16; i++) diff |= want[i] ^ sealed[ct.length + i];
  return diff === 0 ? chacha20(k, 1, n, ct) : null;
}

function encode(v: PlainValue): Uint8Array {
  if (v instanceof Uint8Array) {
    const out = new Uint8Array(v.length + 1);
    out.set(v, 1);
    return out;
  }
  if (typeof v === "string") {
    const b = new TextEncoder().encode(v);
    const out = new Uint8Array(b.length + 1);
    out[0] = 1;
    out.set(b, 1);
    return out;
  }
  if (typeof v === "bigint" || (typeof v === "number" && Number.isSafeInteger(v))) {
    const out = new Uint8Array(9);
    out[0] = 2;
    new DataView(out.buffer).setBigInt64(1, BigInt(v), true);
    return out;
  }
  throw new TypeError("an encrypted column holds bytes, a text or an integer");
}

function decode(b: Uint8Array): Buffer | string | number | bigint {
  if (b[0] === 0) return Buffer.from(b.subarray(1));
  if (b[0] === 1) return new TextDecoder("utf-8", { fatal: true }).decode(b.subarray(1));
  if (b[0] === 2 && b.length === 9) {
    const i = new DataView(b.buffer, b.byteOffset + 1, 8).getBigInt64(0, true);
    return i >= BigInt(Number.MIN_SAFE_INTEGER) && i <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(i) : i;
  }
  throw new TypeError("an encrypted value of no type known");
}

async function hmacKey(key: Uint8Array): Promise<CryptoKey> {
  return globalThis.crypto.subtle.importKey(
    "raw",
    key as Uint8Array<ArrayBuffer>,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
}

async function hmac(key: CryptoKey, data: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await globalThis.crypto.subtle.sign("HMAC", key, data as Uint8Array<ArrayBuffer>));
}

/** A column's keys, from a 32-byte master key the application keeps (a
 *  KMS's, a keychain's: never the database's) and the column's name:
 *  encrypts values (bytes, a text, an integer), decrypts them, and makes
 *  their blind indexes. */
export class ColumnKey {
  private readonly key: Uint8Array;
  private readonly index: CryptoKey;

  private constructor(key: Uint8Array, index: CryptoKey) {
    this.key = key;
    this.index = index;
  }

  /** The keys of column `name` under `master`. */
  static async create(master: Uint8Array, name: string): Promise<ColumnKey> {
    if (master.length !== 32) throw new TypeError("a master key is 32 bytes");
    const m = await hmacKey(master);
    const derive = (purpose: string) => hmac(m, new TextEncoder().encode(`nucleus column 1 ${purpose}\0${name}`));
    return new ColumnKey(await derive("encrypt"), await hmacKey(await derive("index")));
  }

  /** `value` encrypted, the bytea to store: a fresh random nonce each time
   *  (`nonce` for test vectors only — one used twice gives both values
   *  away). */
  encrypt(value: PlainValue, nonce?: Uint8Array): Buffer {
    const n = nonce ?? globalThis.crypto.getRandomValues(new Uint8Array(24));
    if (n.length !== 24) throw new TypeError("a nonce is 24 bytes");
    const sealed = xseal(this.key, n, Uint8Array.of(COLUMN_FORMAT), encode(value));
    const out = Buffer.alloc(25 + sealed.length);
    out[0] = COLUMN_FORMAT;
    out.set(n, 1);
    out.set(sealed, 25);
    return out;
  }

  /** The value in `sealed` (null stays null); a TypeError when this key did
   *  not make it, or it was changed.  An integer is a number, or a bigint
   *  past 2^53. */
  decrypt(sealed: Uint8Array | null): Buffer | string | number | bigint | null {
    if (sealed === null) return null;
    const fail = () => new TypeError("an encrypted value this column's key does not open");
    if (sealed.length < 25 || sealed[0] !== COLUMN_FORMAT) throw fail();
    const plain = xopen(this.key, sealed.subarray(1, 25), sealed.subarray(0, 1), sealed.subarray(25));
    if (plain === null) throw fail();
    return decode(plain);
  }

  /** `value`'s blind index (16 bytes): equal values have equal ones, so
   *  `WHERE email_index = $1` with it finds a value's rows. */
  async blindIndex(value: PlainValue): Promise<Buffer> {
    return Buffer.from((await hmac(this.index, encode(value))).subarray(0, INDEX_LEN));
  }
}
