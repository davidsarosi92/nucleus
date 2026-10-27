// JavaScript/TypeScript driver for nucleus: the nucleus protocol (ncp,
// DOCS.md section 10).  Node.js, no dependencies.
//
//   import { connect } from "nucleus-db";
//   const db = await connect();            // 127.0.0.1:20035, user/password "nucleus"
//   await db.query("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
//   await db.query("INSERT INTO t VALUES ($1, $2)", [1, "one"]);
//   const { rows } = await db.query("SELECT id, name FROM t");
//   for await (const row of await db.stream("SELECT id FROM t")) {
//     console.log(row);                    // a large result, as it arrives
//   }
//   for await (const batch of await db.batches("SELECT id FROM t")) {
//     const ids = batch.column(0).values;  // an Int32Array, a column at a time
//   }
//   await db.close();
//
// Values: boolean, smallint / integer as number, bigint as number (bigint
// beyond 2^53), real (its shortest digits) and double as number, numeric
// as Decimal (exact), text, char(n) and enums as string, bytea as Buffer,
// date / timestamp / timestamptz as Date (UTC; read with millisecond
// precision), time as Time, interval as Interval, inet as Inet, cidr as
// Cidr, uuid as its canonical string, arrays as arrays, NULL as null;
// another registered type as Ext (its name and binary form).

import { createHash, createHmac, pbkdf2Sync, randomBytes, timingSafeEqual } from "node:crypto";
import { readFileSync } from "node:fs";
import net from "node:net";
import tls from "node:tls";

// 2: registered types (uuid, modules' types) arrive in their binary form;
// 3: smallint, integer, real, bytea, time, inet and cidr in theirs;
// 4: the password is proved by SCRAM-SHA-256, never sent; 5: errors carry
// a hint; 6: results stream, fetched in batches; 7: columnar results.
export const PROTOCOL_VERSION = 7;
/** Rows a stream asks the server for at a time. */
export const DEFAULT_FETCH = 1000;
/** Rows a columnar batch holds when none is given. */
export const DEFAULT_BATCH = 65536;
const EXT = 11;
const ENUM = 20;
export const DEFAULT_PORT = 20035;

const EPOCH_MS = Date.UTC(2000, 0, 1);
const DAY_MS = 86_400_000;
const TYPE_NAMES = [
  "unknown", "boolean", "bigint", "double precision", "text", "numeric",
  "date", "timestamp", "timestamptz", "interval", "array", "", "smallint",
  "integer", "real", "bytea", "time", "character", "inet", "cidr",
];

/**
 * An error from the server (`code` is the SQLSTATE; `hint`, what to do about
 * it — the name meant, the cast that would work) or the driver.
 */
export class NucleusError extends Error {
  readonly code: string | null;
  readonly hint: string | null;
  constructor(message: string, code: string | null = null, hint: string | null = null) {
    super(code ? `${message} (${code})` : message);
    this.name = "NucleusError";
    this.code = code;
    this.hint = hint;
  }
}

/** An exact decimal: `mantissa × 10^-scale`. */
export class Decimal {
  readonly mantissa: bigint;
  readonly scale: number;

  constructor(mantissa: bigint, scale: number) {
    this.mantissa = mantissa;
    this.scale = scale;
  }

  /** Parses `"-12.50"`, `"1e3"`. */
  static parse(text: string): Decimal {
    const m = /^\s*([+-]?)(\d*)(?:\.(\d*))?(?:[eE]([+-]?\d+))?\s*$/.exec(text);
    if (!m || (m[2] === "" && (m[3] ?? "") === "")) {
      throw new NucleusError(`invalid numeric: ${JSON.stringify(text)}`, "22P02");
    }
    const frac = m[3] ?? "";
    let mantissa = BigInt((m[2] || "0") + frac);
    let scale = frac.length - Number(m[4] ?? 0);
    if (scale < 0) {
      mantissa *= 10n ** BigInt(-scale);
      scale = 0;
    }
    return new Decimal(m[1] === "-" ? -mantissa : mantissa, scale);
  }

  toString(): string {
    const neg = this.mantissa < 0n;
    const digits = (neg ? -this.mantissa : this.mantissa).toString();
    if (this.scale === 0) return (neg ? "-" : "") + digits;
    const padded = digits.padStart(this.scale + 1, "0");
    const cut = padded.length - this.scale;
    return `${neg ? "-" : ""}${padded.slice(0, cut)}.${padded.slice(cut)}`;
  }

  toNumber(): number {
    return Number(this.toString());
  }

  toJSON(): string {
    return this.toString();
  }
}

/** An interval: months, days and microseconds, kept apart. */
export class Interval {
  readonly months: number;
  readonly days: number;
  readonly micros: bigint;

  constructor(months: number, days: number, micros: bigint) {
    this.months = months;
    this.days = days;
    this.micros = micros;
  }
}

/** A time of day: microseconds since midnight (`24:00:00` included). */
export class Time {
  readonly micros: number;

  constructor(micros: number) {
    this.micros = micros;
  }

  /** Parses `"12:34:56.5"`. */
  static parse(text: string): Time {
    const m = /^(\d{1,2}):(\d{2})(?::(\d{2})(?:\.(\d{1,6}))?)?$/.exec(text.trim());
    if (!m) throw new NucleusError(`invalid time: ${JSON.stringify(text)}`, "22007");
    const us = Number((m[4] ?? "").padEnd(6, "0"));
    return new Time(((Number(m[1]) * 60 + Number(m[2])) * 60 + Number(m[3] ?? 0)) * 1e6 + us);
  }

  toString(): string {
    const p = (n: number) => String(n).padStart(2, "0");
    const s = Math.floor(this.micros / 1e6);
    const us = this.micros % 1e6;
    const frac = us ? "." + String(us).padStart(6, "0").replace(/0+$/, "") : "";
    return `${p(Math.floor(s / 3600))}:${p(Math.floor(s / 60) % 60)}:${p(s % 60)}${frac}`;
  }
}

/** An address's text: dotted IPv4, or IPv6 with the longest run of zero
 * groups as `::` (as nucleus prints it, mapped IPv4 dotted). */
function addressText(b: Buffer): string {
  if (b.length === 4) return Array.from(b).join(".");
  const w = Array.from({ length: 8 }, (_, i) => b.readUInt16BE(2 * i));
  let best = -1;
  let len = 0;
  for (let i = 0; i < 8; ) {
    if (w[i] !== 0) {
      i++;
      continue;
    }
    const start = i;
    while (i < 8 && w[i] === 0) i++;
    if (i - start > len) {
      best = start;
      len = i - start;
    }
  }
  if (len < 2) best = -1;
  if (best === 0 && (len === 6 || (len === 5 && w[5] === 0xffff))) {
    return (len === 5 ? "::ffff:" : "::") + Array.from(b.subarray(12)).join(".");
  }
  let out = "";
  for (let i = 0; i < 8; ) {
    if (i === best) {
      out += "::";
      i += len;
      continue;
    }
    if (out && !out.endsWith(":")) out += ":";
    out += w[i].toString(16);
    i++;
  }
  return out;
}

/** An address's bytes from its text (IPv4 dotted, or IPv6). */
function addressBytes(text: string): Buffer {
  const bad = () => new NucleusError(`invalid address: ${JSON.stringify(text)}`, "22P02");
  if (!text.includes(":")) {
    const parts = text.split(".").map(Number);
    if (parts.length !== 4 || parts.some((n) => !Number.isInteger(n) || n < 0 || n > 255)) throw bad();
    return Buffer.from(parts);
  }
  // An embedded IPv4 tail (`::ffff:1.2.3.4`) as its two groups.
  let t = text;
  const v4 = /(\d+\.\d+\.\d+\.\d+)$/.exec(t);
  if (v4) {
    const b = addressBytes(v4[1]);
    t = t.slice(0, -v4[1].length) + `${b.readUInt16BE(0).toString(16)}:${b.readUInt16BE(2).toString(16)}`;
  }
  const groups = (s: string) => (s === "" ? [] : s.split(":").map((g) => {
    if (!/^[0-9a-fA-F]{1,4}$/.test(g)) throw bad();
    return parseInt(g, 16);
  }));
  const halves = t.split("::");
  if (halves.length > 2) throw bad();
  const head = groups(halves[0]);
  const rest = halves.length === 2 ? groups(halves[1]) : [];
  const fill = 8 - head.length - rest.length;
  if (halves.length === 1 ? fill !== 0 : fill < 1) throw bad();
  const words = [...head, ...Array(halves.length === 2 ? fill : 0).fill(0), ...rest];
  const b = Buffer.alloc(16);
  words.forEach((w, i) => b.writeUInt16BE(w, 2 * i));
  return b;
}

/** An `inet`: an address and its mask length (32 or 128: a host). */
export class Inet {
  readonly bytes: Buffer;
  readonly bits: number;

  constructor(bytes: Buffer, bits?: number) {
    this.bytes = bytes;
    this.bits = bits ?? bytes.length * 8;
  }

  /** Parses `"10.1.2.3/8"`, `"::1"`. */
  static parse(text: string): Inet {
    const [addr, bits] = text.trim().split("/");
    return new Inet(addressBytes(addr), bits === undefined ? undefined : Number(bits));
  }

  /** The address alone. */
  get address(): string {
    return addressText(this.bytes);
  }

  toString(): string {
    return this.bits === this.bytes.length * 8 ? this.address : `${this.address}/${this.bits}`;
  }
}

/** A `cidr`: a network. */
export class Cidr extends Inet {
  static parse(text: string): Cidr {
    const i = Inet.parse(text);
    return new Cidr(i.bytes, i.bits);
  }

  toString(): string {
    return `${this.address}/${this.bits}`;
  }
}

/** A real's 4 bytes as the number of its shortest digits. */
function realOf(f: number): number {
  if (!Number.isFinite(f)) return f;
  for (let p = 1; p <= 9; p++) {
    const n = Number(f.toPrecision(p));
    if (Math.fround(n) === f) return n;
  }
  return f;
}

/** A value of a registered type this driver has no JavaScript type for:
 * the type's name and the value's binary (canonical) form. */
export class Ext {
  readonly typeName: string;
  readonly data: Buffer;

  constructor(typeName: string, data: Buffer) {
    this.typeName = typeName;
    this.data = data;
  }
}

/** A uuid's canonical text (`8-4-4-4-12`, lower case) from its 16 bytes. */
export function uuidText(b: Buffer): string {
  const h = b.toString("hex");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

export type Value =
  | null
  | boolean
  | number
  | bigint
  | string
  | Decimal
  | Date
  | Interval
  | Time
  | Inet
  | Buffer
  | Uint8Array
  | Ext
  | Value[];

export interface Column {
  name: string;
  type: string;
  /** The protocol's type tag (tells an enum from a registered type). */
  tag?: number;
}

/** One statement's result. */
export class Result {
  readonly columns: Column[];
  readonly rows: Value[][];
  /** `SELECT`, `INSERT`, `CREATE TABLE`, … */
  readonly command: string;
  /** Rows returned or affected. */
  readonly count: number;

  constructor(columns: Column[], rows: Value[][], command: string, count: number) {
    this.columns = columns;
    this.rows = rows;
    this.command = command;
    this.count = count;
  }

  /** The rows as objects by column name. */
  records(): Record<string, Value>[] {
    return this.rows.map((r) =>
      Object.fromEntries(this.columns.map((c, i) => [c.name, r[i]])),
    );
  }
}

export interface ConnectOptions {
  host?: string;
  port?: number;
  user?: string;
  password?: string;
  database?: string;
  clientName?: string;
  timeoutMs?: number;
  /** TLS, the server's certificate checked against Node's CA certificates
   *  — or against `tlsCa`'s (a PEM file; giving it implies `tls`). */
  tls?: boolean;
  tlsCa?: string;
  /** `false`: encrypt without checking the certificate (anyone can pose as
   *  the server). */
  tlsVerify?: boolean;
  /** A client certificate and its key (PEM files): one naming the user logs
   *  it in without a password, if the server accepts its CA. */
  tlsCert?: string;
  tlsKey?: string;
  /** An OpenID Connect identity token to log in with instead of the
   *  password: it names the user. */
  token?: string;
}

// ------------------------------------------------------------------ values

class Writer {
  parts: Buffer[] = [];

  u8(v: number): this {
    this.parts.push(Buffer.from([v]));
    return this;
  }
  u16(v: number): this {
    const b = Buffer.alloc(2);
    b.writeUInt16LE(v);
    this.parts.push(b);
    return this;
  }
  u32(v: number): this {
    const b = Buffer.alloc(4);
    b.writeUInt32LE(v);
    this.parts.push(b);
    return this;
  }
  i32(v: number): this {
    const b = Buffer.alloc(4);
    b.writeInt32LE(v);
    this.parts.push(b);
    return this;
  }
  i64(v: bigint): this {
    const b = Buffer.alloc(8);
    b.writeBigInt64LE(v);
    this.parts.push(b);
    return this;
  }
  f64(v: number): this {
    const b = Buffer.alloc(8);
    b.writeDoubleLE(v);
    this.parts.push(b);
    return this;
  }
  str(s: string): this {
    const b = Buffer.from(s, "utf8");
    this.u32(b.length);
    this.parts.push(b);
    return this;
  }
  bytes(): Buffer {
    return Buffer.concat(this.parts);
  }

  value(v: Value): this {
    if (v === null || v === undefined) return this.u8(0);
    if (typeof v === "boolean") return this.u8(1).u8(v ? 1 : 0);
    if (typeof v === "bigint") return this.u8(2).i64(BigInt.asIntN(64, v));
    if (typeof v === "number") {
      return Number.isSafeInteger(v) ? this.u8(2).i64(BigInt(v)) : this.u8(3).f64(v);
    }
    if (typeof v === "string") return this.u8(4).str(v);
    if (v instanceof Decimal) {
      const b = Buffer.alloc(16);
      const u = BigInt.asUintN(128, v.mantissa);
      b.writeBigUInt64LE(u & 0xffffffffffffffffn, 0);
      b.writeBigUInt64LE(u >> 64n, 8);
      this.u8(5);
      this.parts.push(b);
      return this.u8(v.scale);
    }
    if (v instanceof Date) {
      // A Date is an instant: timestamptz.
      return this.u8(8).i64(BigInt(v.getTime() - EPOCH_MS) * 1000n);
    }
    if (v instanceof Interval) return this.u8(9).i32(v.months).i32(v.days).i64(v.micros);
    if (v instanceof Time) return this.u8(16).i64(BigInt(v.micros));
    if (v instanceof Inet) {
      // (A Cidr is an Inet too.)
      this.u8(v instanceof Cidr ? 19 : 18).u8(v.bytes.length === 4 ? 4 : 6).u8(v.bits);
      this.parts.push(v.bytes);
      return this;
    }
    if (v instanceof Uint8Array) {
      // A Buffer is one too: bytea.
      this.u8(15).u32(v.length);
      this.parts.push(Buffer.from(v));
      return this;
    }
    if (v instanceof Ext) {
      this.u8(EXT).str(v.typeName).u32(v.data.length);
      this.parts.push(v.data);
      return this;
    }
    if (Array.isArray(v)) {
      this.u8(10).u32(v.length);
      for (const item of v) this.value(item);
      return this;
    }
    throw new NucleusError(`cannot send ${typeof v} as a nucleus value`);
  }
}

class Reader {
  b: Buffer;
  at = 0;

  constructor(b: Buffer) {
    this.b = b;
  }

  /** An `E` message: SQLSTATE, message and (from version 5) the hint. */
  serverError(): NucleusError {
    const code = this.str();
    const message = this.str();
    const hint = this.at < this.b.length ? this.str() : "";
    return new NucleusError(message, code, hint === "" ? null : hint);
  }
  need(n: number): void {
    if (this.at + n > this.b.length) throw new NucleusError("message too short");
  }
  u8(): number {
    this.need(1);
    return this.b[this.at++];
  }
  u16(): number {
    this.need(2);
    const v = this.b.readUInt16LE(this.at);
    this.at += 2;
    return v;
  }
  u32(): number {
    this.need(4);
    const v = this.b.readUInt32LE(this.at);
    this.at += 4;
    return v;
  }
  i32(): number {
    this.need(4);
    const v = this.b.readInt32LE(this.at);
    this.at += 4;
    return v;
  }
  i64(): bigint {
    this.need(8);
    const v = this.b.readBigInt64LE(this.at);
    this.at += 8;
    return v;
  }
  u64(): bigint {
    this.need(8);
    const v = this.b.readBigUInt64LE(this.at);
    this.at += 8;
    return v;
  }
  str(): string {
    const n = this.u32();
    this.need(n);
    const s = this.b.toString("utf8", this.at, this.at + n);
    this.at += n;
    return s;
  }

  value(): Value {
    const tag = this.u8();
    switch (tag) {
      case 0:
        return null;
      case 1:
        return this.u8() !== 0;
      case 2: {
        const v = this.i64();
        return v >= BigInt(Number.MIN_SAFE_INTEGER) && v <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(v) : v;
      }
      case 3: {
        this.need(8);
        const v = this.b.readDoubleLE(this.at);
        this.at += 8;
        return v;
      }
      case 4:
        return this.str();
      case 5: {
        this.need(17);
        const lo = this.b.readBigUInt64LE(this.at);
        const hi = this.b.readBigUInt64LE(this.at + 8);
        this.at += 16;
        return new Decimal(BigInt.asIntN(128, (hi << 64n) | lo), this.u8());
      }
      case 6:
        return new Date(EPOCH_MS + this.i32() * DAY_MS);
      case 7:
      case 8:
        return new Date(EPOCH_MS + Number(this.i64() / 1000n));
      case 9:
        return new Interval(this.i32(), this.i32(), this.i64());
      case 12: {
        this.need(2);
        const v = this.b.readInt16LE(this.at);
        this.at += 2;
        return v;
      }
      case 13:
        return this.i32();
      case 14: {
        this.need(4);
        const v = this.b.readFloatLE(this.at);
        this.at += 4;
        return realOf(v);
      }
      case 15: {
        const n = this.u32();
        this.need(n);
        const data = Buffer.from(this.b.subarray(this.at, this.at + n));
        this.at += n;
        return data;
      }
      case 16:
        return new Time(Number(this.i64()));
      case 18:
      case 19: {
        const family = this.u8();
        const bits = this.u8();
        const n = family === 4 ? 4 : 16;
        this.need(n);
        const data = Buffer.from(this.b.subarray(this.at, this.at + n));
        this.at += n;
        return tag === 18 ? new Inet(data, bits) : new Cidr(data, bits);
      }
      case 10: {
        const n = this.u32();
        const items: Value[] = [];
        for (let i = 0; i < n; i++) items.push(this.value());
        return items;
      }
      case EXT: {
        const name = this.str();
        const n = this.u32();
        this.need(n);
        const data = Buffer.from(this.b.subarray(this.at, this.at + n));
        this.at += n;
        return name === "uuid" && n === 16 ? uuidText(data) : new Ext(name, data);
      }
      default:
        throw new NucleusError(`unknown value tag ${tag}`);
    }
  }
}

/** The client's side of a SCRAM-SHA-256 exchange (RFC 5802). */
/** One DER element: its tag, its contents, and what follows. */
function der(b: Buffer): [number, Buffer, Buffer] {
  let n = b[1];
  let i = 2;
  if (n & 0x80) {
    const k = n & 0x7f;
    n = b.subarray(2, 2 + k).reduce((a, x) => a * 256 + x, 0);
    i = 2 + k;
  }
  return [b[0], b.subarray(i, i + n), b.subarray(i + n)];
}

/** Signature algorithm OIDs (hex DER) → the hash of their
 *  tls-server-end-point (RFC 5929: SHA-256 for MD5 and SHA-1). */
const END_POINT_HASHES: Record<string, string> = {
  "2a864886f70d010104": "sha256",
  "2a864886f70d010105": "sha256",
  "2a864886f70d01010b": "sha256",
  "2a864886f70d01010c": "sha384",
  "2a864886f70d01010d": "sha512",
  "2a8648ce3d0401": "sha256",
  "2a8648ce3d040302": "sha256",
  "2a8648ce3d040303": "sha384",
  "2a8648ce3d040304": "sha512",
};

/** A certificate's tls-server-end-point value, or null. */
function serverEndPoint(cert: Buffer): Buffer | null {
  try {
    const body = der(cert)[1];
    const algorithm = der(der(body)[2])[1];
    const hash = END_POINT_HASHES[der(algorithm)[1].toString("hex")];
    return hash === undefined ? null : createHash(hash).update(cert).digest();
  } catch {
    return null;
  }
}

/** The client's side of SCRAM-SHA-256 (RFC 5802); over TLS bound to the
 *  session (SCRAM-SHA-256-PLUS) when the server offers it, else saying it
 *  could have been ("y"), so a stripped offer is caught. */
class Scram {
  private readonly password: Buffer;
  private readonly bare: string;
  private readonly gs2: string;
  private readonly binding: Buffer;
  private auth = Buffer.alloc(0);
  private serverKey = Buffer.alloc(0);

  constructor(password: string, binding: Buffer | null = null, offered = false) {
    this.password = Buffer.from(password, "utf8");
    this.bare = `n=,r=${randomBytes(18).toString("base64")}`;
    this.gs2 = binding === null ? "n,," : offered ? "p=tls-server-end-point,," : "y,,";
    this.binding = binding !== null && offered ? binding : Buffer.alloc(0);
  }

  first(): string {
    return `${this.gs2}${this.bare}`;
  }

  respond(serverFirst: string): string {
    const attrs = new Map(serverFirst.split(",").map((p) => [p[0], p.slice(2)] as [string, string]));
    const nonce = attrs.get("r") ?? "";
    const own = this.bare.slice(5);
    if (!nonce.startsWith(own) || nonce.length === own.length) throw new NucleusError("malformed SCRAM message");
    const salted = pbkdf2Sync(this.password, Buffer.from(attrs.get("s") ?? "", "base64"), Number(attrs.get("i")), 32, "sha256");
    const clientKey = createHmac("sha256", salted).update("Client Key").digest();
    const stored = createHash("sha256").update(clientKey).digest();
    this.serverKey = createHmac("sha256", salted).update("Server Key").digest();
    const c = Buffer.concat([Buffer.from(this.gs2, "utf8"), this.binding]).toString("base64");
    const withoutProof = `c=${c},r=${nonce}`;
    this.auth = Buffer.from(`${this.bare},${serverFirst},${withoutProof}`, "utf8");
    const signature = createHmac("sha256", stored).update(this.auth).digest();
    const proof = Buffer.from(clientKey.map((b, i) => b ^ signature[i]));
    return `${withoutProof},p=${proof.toString("base64")}`;
  }

  verify(serverFinal: string): void {
    const want = createHmac("sha256", this.serverKey).update(this.auth).digest();
    const got = Buffer.from(serverFinal.slice(2), "base64");
    if (got.length !== want.length || !timingSafeEqual(got, want)) {
      throw new NucleusError("the server could not prove it knows the password");
    }
  }
}

function frame(kind: string, body: Buffer): Buffer {
  const h = Buffer.alloc(5);
  h.write(kind, 0, "latin1");
  h.writeUInt32LE(body.length, 1);
  return Buffer.concat([h, body]);
}

// -------------------------------------------------------------- the client

type Message = { kind: string; body: Buffer };
type Status = "idle" | "transaction" | "failed";

/** A connection to a nucleus server. */
export class Client {
  socket: net.Socket;
  buffer: Buffer = Buffer.alloc(0);
  waiting: ((m: Message | Error) => void) | null = null;
  closedError: Error | null = null;
  queue: Promise<unknown> = Promise.resolve();
  /** The transaction status after the last query. */
  status: Status = "idle";
  serverVersion = "";
  sessionId = 0;
  /** The protocol version agreed with the server. */
  version = PROTOCOL_VERSION;
  /** A stream the server is still sending. */
  active: Stream<unknown> | null = null;

  constructor(socket: net.Socket) {
    this.socket = socket;
    socket.on("data", (chunk: Buffer) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.deliver();
    });
    const fail = (e: Error) => {
      this.closedError = e;
      const w = this.waiting;
      this.waiting = null;
      w?.(e);
    };
    socket.on("error", fail);
    socket.on("close", () => fail(new NucleusError("the server closed the connection")));
  }

  deliver(): void {
    if (!this.waiting || this.buffer.length < 5) return;
    const n = this.buffer.readUInt32LE(1);
    if (this.buffer.length < 5 + n) return;
    const m = {
      kind: this.buffer.toString("latin1", 0, 1),
      body: this.buffer.subarray(5, 5 + n),
    };
    this.buffer = this.buffer.subarray(5 + n);
    const w = this.waiting;
    this.waiting = null;
    w(m);
  }

  read(): Promise<Message> {
    return new Promise((resolve, reject) => {
      if (this.closedError && this.buffer.length < 5) return reject(this.closedError);
      this.waiting = (m) => (m instanceof Error ? reject(m) : resolve(m));
      this.deliver();
    });
  }

  setStatus(body: Buffer): void {
    this.status = body[0] === 1 ? "transaction" : body[0] === 2 ? "failed" : "idle";
  }

  static async connect(options: ConnectOptions = {}): Promise<Client> {
    const host = options.host ?? "127.0.0.1";
    const port = options.port ?? DEFAULT_PORT;
    const secure = options.tls === true || options.tlsCa !== undefined;
    const socket = await new Promise<net.Socket>((resolve, reject) => {
      // TLS starts at once; the server sees the handshake (DOCS §10).
      const s = secure
        ? tls.connect({
            host,
            port,
            servername: net.isIP(host) ? undefined : host,
            ca: options.tlsCa === undefined ? undefined : readFileSync(options.tlsCa),
            rejectUnauthorized: options.tlsVerify ?? true,
            cert: options.tlsCert === undefined ? undefined : readFileSync(options.tlsCert),
            key: options.tlsKey === undefined ? undefined : readFileSync(options.tlsKey),
          })
        : net.createConnection({ host, port });
      s.setNoDelay(true);
      s.setTimeout(options.timeoutMs ?? 10_000, () =>
        s.destroy(new NucleusError(`cannot connect to ${host}:${port}: timed out`)),
      );
      s.once(secure ? "secureConnect" : "connect", () => {
        s.setTimeout(0);
        resolve(s);
      });
      s.once("error", (e) => reject(new NucleusError(`cannot connect to ${host}:${port}: ${e.message}`)));
    });
    const c = new Client(socket);
    const hello = new Writer()
      .u16(PROTOCOL_VERSION)
      .str(options.user ?? "nucleus")
      .str(options.database ?? "nucleus")
      .str("")
      .str(options.clientName ?? "nucleus-js")
      .bytes();
    socket.write(frame("H", hello));
    let scram: Scram | null = null;
    for (;;) {
      const m = await c.read();
      const r = new Reader(m.body);
      if (m.kind === "A") {
        const msg = r.str();
        let reply: string | null;
        if (scram === null && options.token !== undefined) {
          // OAUTHBEARER's initial response (RFC 7628).
          if (!msg.split(" ").includes("OAUTHBEARER")) throw new NucleusError("the server does not take bearer tokens");
          socket.write(frame("a", new Writer().str(`n,,\x01auth=Bearer ${options.token}\x01\x01`).bytes()));
          continue;
        }
        if (scram === null && msg.split(" ").includes("PLAIN") && !msg.split(" ").includes("SCRAM-SHA-256")) {
          // A directory checks this role's password: the password itself
          // (RFC 4616), never without TLS.
          if (!(socket instanceof tls.TLSSocket)) throw new NucleusError("the server asks for the password itself, which needs TLS");
          socket.write(frame("a", new Writer().str(`\0${options.user ?? "nucleus"}\0${options.password ?? ""}`).bytes()));
          continue;
        }
        if (scram === null) {
          const offered = msg.split(" ");
          if (!offered.includes("SCRAM-SHA-256")) throw new NucleusError(`unsupported authentication ${msg}`);
          const cert = socket instanceof tls.TLSSocket ? socket.getPeerCertificate().raw : undefined;
          const binding = cert === undefined ? null : serverEndPoint(cert);
          scram = new Scram(options.password ?? "nucleus", binding, offered.includes("SCRAM-SHA-256-PLUS"));
          reply = scram.first();
        } else if (msg.startsWith("v=")) {
          scram.verify(msg);
          reply = null;
        } else {
          reply = scram.respond(msg);
        }
        if (reply !== null) socket.write(frame("a", new Writer().str(reply).bytes()));
      } else if (m.kind === "W") {
        c.version = r.u16();
        c.serverVersion = r.str();
        c.sessionId = r.u32();
      } else if (m.kind === "R") {
        c.setStatus(m.body);
        return c;
      } else if (m.kind === "E") {
        const e = r.serverError();
        socket.destroy();
        throw e;
      } else {
        throw new NucleusError(`unexpected message ${m.kind}`);
      }
    }
  }

  /** Runs `sql`; the result of each statement.  A failing statement
   * rejects with its error. */
  queryAll(sql: string, params: Value[] = []): Promise<Result[]> {
    // A stream left unfinished is closed first: one result at a time.
    void this.active?.close();
    const run = async (): Promise<Result[]> => {
      this.sendQuery(sql, params, 0, 0);
      const results: Result[] = [];
      let error: NucleusError | null = null;
      let columns: Column[] | null = null;
      let rows: Value[][] = [];
      for (;;) {
        const m = await this.read();
        const r = new Reader(m.body);
        switch (m.kind) {
          case "C":
            columns = readColumns(r);
            rows = [];
            break;
          case "D": {
            const row: Value[] = [];
            for (let i = 0; i < (columns?.length ?? 0); i++) row.push(r.value());
            rows.push(row);
            break;
          }
          case "K": {
            const command = r.str();
            const count = Number(r.u64());
            results.push(new Result(columns ?? [], columns ? rows : [], command, count));
            columns = null;
            rows = [];
            break;
          }
          case "E":
            error = r.serverError();
            break;
          case "R":
            this.setStatus(m.body);
            if (error) throw error;
            return results;
          default:
            throw new NucleusError(`unexpected message ${m.kind}`);
        }
      }
    };
    // One query at a time on a connection.
    const next = this.queue.then(run, run);
    this.queue = next.catch(() => undefined);
    return next;
  }

  /** Sends `Q`: rows `fetch` at a time (0: all at once; from version 6). */
  sendQuery(sql: string, params: Value[], format: number, fetch: number): void {
    const w = new Writer().str(sql).u8(format);
    if (this.version >= 6) w.u32(fetch);
    w.u16(params.length);
    for (const p of params) w.value(p);
    this.socket.write(frame("Q", w.bytes()));
  }

  /**
   * Runs one statement and returns a `Stream` of its rows, read as they
   * arrive (`for await`): the server sends `fetch` rows, then waits until
   * they are read, so a result of any size takes a batch of memory.
   * Closing it early — leaving the loop, `close()`, or running another
   * statement — stops the statement.  An error before the first row
   * rejects here; after rows, from the iteration.
   */
  stream(sql: string, params: Value[] = [], options: { fetch?: number } = {}): Promise<Stream> {
    return this.open(new Stream(this, options.fetch || DEFAULT_FETCH), sql, params);
  }

  /**
   * Runs one statement and returns its result as `Batches`: `Batch` after
   * `Batch` of `rows` rows, each column a typed array (`Int32Array`,
   * `Float64Array`, `BigInt64Array`, …) read in place, without a value
   * per cell (protocol version 7).  As `stream` otherwise.
   */
  batches(sql: string, params: Value[] = [], options: { rows?: number } = {}): Promise<Batches> {
    if (this.version < 7) return Promise.reject(new NucleusError("columnar results need protocol version 7"));
    return this.open(new Batches(this, options.rows || DEFAULT_BATCH), sql, params);
  }

  private open<S extends Stream<unknown>>(s: S, sql: string, params: Value[]): Promise<S> {
    void this.active?.close();
    let release: () => void = () => {};
    const held = new Promise<void>((resolve) => (release = resolve));
    s.release = release;
    this.active = s;
    const run = () => s.start(sql, params);
    const started = this.queue.then(run, run);
    s.op = started.catch(() => undefined);
    // The connection is the stream's until it ends.
    this.queue = started.then(() => held, () => undefined);
    return started.then(() => s);
  }

  /** Runs one statement (or several: the last one's result). */
  async query(sql: string, params: Value[] = []): Promise<Result> {
    const all = await this.queryAll(sql, params);
    return all[all.length - 1] ?? new Result([], [], "", 0);
  }

  /** BEGIN, `fn`, then COMMIT — or ROLLBACK if it throws. */
  async transaction<T>(fn: (client: Client) => Promise<T>): Promise<T> {
    await this.query("BEGIN");
    try {
      const v = await fn(this);
      await this.query(this.status === "failed" ? "ROLLBACK" : "COMMIT");
      return v;
    } catch (e) {
      await this.query("ROLLBACK");
      throw e;
    }
  }

  /** Says Bye and closes. */
  async close(): Promise<void> {
    await this.queue;
    await new Promise<void>((resolve) => this.socket.end(frame("X", Buffer.alloc(0)), () => resolve()));
  }
}

function readColumns(r: Reader): Column[] {
  const n = r.u16();
  const columns: Column[] = [];
  for (let i = 0; i < n; i++) {
    const name = r.str();
    const tag = r.u8();
    const type =
      tag === 10
        ? `${TYPE_NAMES[r.u8()] ?? "unknown"}[]`
        : tag === EXT || tag === ENUM
          ? r.str()
          : TYPE_NAMES[tag] ?? "unknown";
    columns.push({ name, type, tag });
  }
  return columns;
}

/**
 * One statement's rows as they arrive (`Client.stream`): iterate it with
 * `for await`; `columns`, and once every row is read `command` and
 * `count`.
 */
export class Stream<T = Value[]> implements AsyncIterable<T> {
  columns: Column[] = [];
  /** `SELECT`, …, once the statement has ended. */
  command: string | null = null;
  /** The rows sent, once the statement has ended. */
  count: number | null = null;
  /** Reads, one after another. */
  op: Promise<unknown> = Promise.resolve();
  /** Ends the stream's hold on the connection. */
  release: () => void = () => {};
  protected format = 0;
  protected dataKind = "D";
  private client: Client;
  private fetch: number;
  private error: NucleusError | null = null;
  private ready = false;

  constructor(client: Client, fetch: number) {
    this.client = client;
    this.fetch = fetch;
  }

  /** A data message's item. */
  protected decode(body: Buffer): T {
    const r = new Reader(body);
    const row: Value[] = [];
    for (let i = 0; i < this.columns.length; i++) row.push(r.value());
    return row as T;
  }

  /** Sends the query and reads up to its columns (or its end). */
  async start(sql: string, params: Value[]): Promise<void> {
    const c = this.client;
    if (c.version < 6) this.fetch = 0;
    try {
      c.sendQuery(sql, params, this.format, this.fetch);
      for (;;) {
        const m = await c.read();
        const r = new Reader(m.body);
        switch (m.kind) {
          case "C":
            this.columns = readColumns(r);
            return;
          case "K":
            this.command = r.str();
            this.count = Number(r.u64());
            break;
          case "E":
            this.error = r.serverError();
            break;
          case "R": {
            this.end(m.body);
            const e = this.error;
            this.error = null;
            if (e) throw e;
            return;
          }
          default:
            throw new NucleusError(`unexpected message ${m.kind}`);
        }
      }
    } catch (e) {
      this.stop();
      throw e;
    }
  }

  private end(body: Buffer): void {
    this.client.setStatus(body);
    this.stop();
  }

  private stop(): void {
    this.ready = true;
    if (this.client.active === this) this.client.active = null;
    this.release();
  }

  /** The next row (or batch); `null` after the last. */
  next(): Promise<T | null> {
    const p = this.op.then(() => this.read());
    this.op = p.catch(() => undefined);
    return p;
  }

  private async read(): Promise<T | null> {
    const c = this.client;
    try {
      while (!this.ready) {
        const m = await c.read();
        if (m.kind === this.dataKind) return this.decode(m.body);
        const r = new Reader(m.body);
        switch (m.kind) {
          case "S":
            // The batch is read: the next.
            c.socket.write(frame("F", new Writer().u32(this.fetch).bytes()));
            break;
          case "K":
            this.command = r.str();
            this.count = Number(r.u64());
            break;
          case "E":
            this.error = r.serverError();
            break;
          case "R": {
            this.end(m.body);
            const e = this.error;
            this.error = null;
            if (e) throw e;
            break;
          }
          case "C":
            throw new NucleusError("a stream runs one statement; this query holds several");
          default:
            throw new NucleusError(`unexpected message ${m.kind}`);
        }
      }
      return null;
    } catch (e) {
      if (!this.ready) this.stop();
      throw e;
    }
  }

  /** Stops reading: the server is told to send no more, and what it
   *  already sent is skipped. */
  close(): Promise<void> {
    const p = this.op.then(async () => {
      const c = this.client;
      try {
        while (!this.ready) {
          const m = await c.read();
          const r = new Reader(m.body);
          if (m.kind === "S") c.socket.write(frame("c", Buffer.alloc(0)));
          else if (m.kind === "K") {
            this.command = r.str();
            this.count = Number(r.u64());
          } else if (m.kind === "R") this.end(m.body);
        }
      } catch (e) {
        if (!this.ready) this.stop();
        throw e;
      }
    });
    this.op = p.catch(() => undefined);
    return p;
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: async () => {
        const row = await this.next();
        return row === null ? { done: true, value: undefined } : { done: false, value: row };
      },
      return: async () => {
        await this.close();
        return { done: true, value: undefined };
      },
    };
  }
}

// ---------------------------------------------------------------- columnar

// Fixed widths by type tag (DOCS.md section 10, columnar results).
const FIXED: Record<number, number> = { 1: 1, 12: 2, 13: 4, 14: 4, 6: 4, 2: 8, 3: 8, 7: 8, 8: 8, 16: 8, 9: 16, 5: 17 };

function widthOf(c: Column): number {
  if (c.tag === EXT && c.type === "uuid") return 16;
  return FIXED[c.tag ?? 0] ?? 0;
}

const align = (at: number) => (at + 7) & ~7;

/** A column's values as a typed array, by type. */
export type ColumnValues = Uint8Array | Int16Array | Int32Array | Float32Array | Float64Array | BigInt64Array;

/**
 * One column of a `Batch`.  `values`: the typed array of a fixed-width
 * column, read in place — `Uint8Array` (boolean, 0 or 1; 16- and 17-byte
 * types' raw bytes), `Int16Array`, `Int32Array` (integer; date as days
 * since 2000-01-01), `Float32Array`, `Float64Array`, `BigInt64Array`
 * (bigint; timestamps as µs since 2000-01-01, time as µs since midnight)
 * — or a variable-width column's bytes, with `offsets`.  `get(i)` gives
 * one value as `stream` does.
 */
export class ColumnBatch {
  readonly column: Column;
  readonly rows: number;
  /** A bit per row, least significant first, set for a value (`null`: no NULLs). */
  readonly valid: Uint8Array | null;
  readonly values: ColumnValues;
  /** A variable width's offsets (`null` for a fixed width). */
  readonly offsets: Uint32Array | null;

  constructor(column: Column, rows: number, valid: Uint8Array | null, values: ColumnValues, offsets: Uint32Array | null) {
    this.column = column;
    this.rows = rows;
    this.valid = valid;
    this.values = values;
    this.offsets = offsets;
  }

  isNull(i: number): boolean {
    return this.valid !== null && (this.valid[i >> 3] & (1 << (i & 7))) === 0;
  }

  /** The value at row `i`, as `stream` gives it. */
  get(i: number): Value {
    if (this.isNull(i)) return null;
    const c = this.column;
    const tag = c.tag ?? 0;
    const v = this.values;
    const bytes = new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
    let slot: Uint8Array;
    const w = widthOf(c);
    if (w) slot = bytes.subarray(i * w, (i + 1) * w);
    else slot = bytes.subarray(this.offsets![i], this.offsets![i + 1]);
    const tagged = (t: number, payload: Uint8Array) => new Reader(Buffer.concat([Buffer.from([t]), payload])).value();
    const lengthed = (t: number, payload: Uint8Array) => {
      const n = Buffer.alloc(4);
      n.writeUInt32LE(payload.length);
      return tagged(t, Buffer.concat([n, payload]));
    };
    if (w && tag !== EXT) return tagged(tag, slot);
    switch (tag) {
      case EXT: {
        const name = new Writer().str(c.type).bytes();
        const n = Buffer.alloc(4);
        n.writeUInt32LE(slot.length);
        return new Reader(Buffer.concat([Buffer.from([EXT]), name, n, slot])).value();
      }
      case 15:
        return Buffer.from(slot);
      case 18:
      case 19:
        return tagged(tag, slot);
      case 10:
        return new Reader(Buffer.from(slot)).value();
      default:
        return lengthed(4, slot);
    }
  }

  /** Every value (a JavaScript value each). */
  toArray(): Value[] {
    const out: Value[] = [];
    for (let i = 0; i < this.rows; i++) out.push(this.get(i));
    return out;
  }
}

/** Rows of a result, a typed array per column (`Client.batches`). */
export class Batch {
  readonly columns: Column[];
  readonly rows: number;
  private cols: ColumnBatch[] = [];

  constructor(columns: Column[], body: Buffer) {
    // Typed arrays need their element size's alignment: the body is copied
    // into a buffer of its own when it is not at a multiple of 8.
    let b = body;
    if (body.byteOffset % 8 !== 0) {
      const copy = new Uint8Array(body.length);
      copy.set(body);
      b = Buffer.from(copy.buffer);
    }
    this.columns = columns;
    const n = b.readUInt32LE(0);
    this.rows = n;
    let at = 4;
    const base = b.byteOffset;
    const view = (at: number, len: number) => new Uint8Array(b.buffer, base + at, len);
    for (const c of columns) {
      const nulls = b[at++] !== 0;
      let valid: Uint8Array | null = null;
      if (nulls) {
        at = align(at);
        valid = view(at, (n + 7) >> 3);
        at += (n + 7) >> 3;
      }
      at = align(at);
      const w = widthOf(c);
      let values: ColumnValues;
      let offsets: Uint32Array | null = null;
      if (w) {
        const off = base + at;
        const tag = c.tag ?? 0;
        values =
          w === 2 ? new Int16Array(b.buffer, off, n)
          : tag === 14 ? new Float32Array(b.buffer, off, n)
          : w === 4 ? new Int32Array(b.buffer, off, n)
          : tag === 3 ? new Float64Array(b.buffer, off, n)
          : w === 8 ? new BigInt64Array(b.buffer, off, n)
          : view(at, n * w);
        at += n * w;
      } else {
        offsets = new Uint32Array(b.buffer, base + at, n + 1);
        at = align(at + (n + 1) * 4);
        const end = offsets[n];
        values = view(at, end);
        at += end;
      }
      at = align(at);
      if (at > b.length) throw new NucleusError("malformed batch");
      this.cols.push(new ColumnBatch(c, n, valid, values, offsets));
    }
  }

  /** Column `key` (an index or a name). */
  column(key: number | string): ColumnBatch {
    const i = typeof key === "number" ? key : this.columns.findIndex((c) => c.name === key);
    if (i < 0 || i >= this.cols.length) throw new NucleusError(`no column ${key}`);
    return this.cols[i];
  }

  /** The rows (a JavaScript value per cell). */
  toRows(): Value[][] {
    const cols = this.cols.map((c) => c.toArray());
    const out: Value[][] = [];
    for (let i = 0; i < this.rows; i++) out.push(cols.map((c) => c[i]));
    return out;
  }
}

/** One statement's result as it arrives, `Batch` by `Batch` (`Client.batches`). */
export class Batches extends Stream<Batch> {
  protected format = 2;
  protected dataKind = "B";

  protected decode(body: Buffer): Batch {
    return new Batch(this.columns, body);
  }
}

/** Opens a connection (the defaults reach a local server as it starts). */
export function connect(options: ConnectOptions = {}): Promise<Client> {
  return Client.connect(options);
}
