// JavaScript/TypeScript driver for nucleus: the nucleus protocol (ncp,
// DOCS.md section 10).  Node.js, no dependencies.
//
//   import { connect } from "nucleus-db";
//   const db = await connect();            // 127.0.0.1:20035, user/password "nucleus"
//   await db.query("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
//   await db.query("INSERT INTO t VALUES ($1, $2)", [1, "one"]);
//   const { rows } = await db.query("SELECT id, name FROM t");
//   await db.close();
//
// Values: boolean, bigint columns as number (bigint beyond 2^53), double
// as number, numeric as Decimal (exact), text as string, date / timestamp /
// timestamptz as Date (UTC; read with millisecond precision), interval as
// Interval, uuid as its canonical string, arrays as arrays, NULL as null;
// another registered type as Ext (its name and binary form).

import net from "node:net";

// 2: registered types (uuid, modules' types) arrive in their binary form.
export const PROTOCOL_VERSION = 2;
const EXT = 11;
export const DEFAULT_PORT = 20035;

const EPOCH_MS = Date.UTC(2000, 0, 1);
const DAY_MS = 86_400_000;
const TYPE_NAMES = [
  "unknown", "boolean", "bigint", "double precision", "text", "numeric",
  "date", "timestamp", "timestamptz", "interval", "array",
];

/** An error from the server (`code` is the SQLSTATE) or the driver. */
export class NucleusError extends Error {
  readonly code: string | null;
  constructor(message: string, code: string | null = null) {
    super(code ? `${message} (${code})` : message);
    this.name = "NucleusError";
    this.code = code;
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
  | Ext
  | Value[];

export interface Column {
  name: string;
  type: string;
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
    const socket = await new Promise<net.Socket>((resolve, reject) => {
      const s = net.createConnection({ host, port });
      s.setNoDelay(true);
      s.setTimeout(options.timeoutMs ?? 10_000, () =>
        s.destroy(new NucleusError(`cannot connect to ${host}:${port}: timed out`)),
      );
      s.once("connect", () => {
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
      .str(options.password ?? "nucleus")
      .str(options.clientName ?? "nucleus-js")
      .bytes();
    socket.write(frame("H", hello));
    for (;;) {
      const m = await c.read();
      const r = new Reader(m.body);
      if (m.kind === "W") {
        r.u16();
        c.serverVersion = r.str();
        c.sessionId = r.u32();
      } else if (m.kind === "R") {
        c.setStatus(m.body);
        return c;
      } else if (m.kind === "E") {
        const code = r.str();
        const message = r.str();
        socket.destroy();
        throw new NucleusError(message, code);
      } else {
        throw new NucleusError(`unexpected message ${m.kind}`);
      }
    }
  }

  /** Runs `sql`; the result of each statement.  A failing statement
   * rejects with its error. */
  queryAll(sql: string, params: Value[] = []): Promise<Result[]> {
    const run = async (): Promise<Result[]> => {
      const w = new Writer().str(sql).u8(0).u16(params.length);
      for (const p of params) w.value(p);
      this.socket.write(frame("Q", w.bytes()));
      const results: Result[] = [];
      let error: NucleusError | null = null;
      let columns: Column[] | null = null;
      let rows: Value[][] = [];
      for (;;) {
        const m = await this.read();
        const r = new Reader(m.body);
        switch (m.kind) {
          case "C": {
            const n = r.u16();
            columns = [];
            for (let i = 0; i < n; i++) {
              const name = r.str();
              const tag = r.u8();
              const type =
                tag === 10
                  ? `${TYPE_NAMES[r.u8()] ?? "unknown"}[]`
                  : tag === EXT
                    ? r.str()
                    : TYPE_NAMES[tag] ?? "unknown";
              columns.push({ name, type });
            }
            rows = [];
            break;
          }
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
          case "E": {
            const code = r.str();
            error = new NucleusError(r.str(), code);
            break;
          }
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

/** Opens a connection (the defaults reach a local server as it starts). */
export function connect(options: ConnectOptions = {}): Promise<Client> {
  return Client.connect(options);
}
