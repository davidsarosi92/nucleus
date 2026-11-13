// JavaScript/TypeScript driver for nucleus: the nucleus protocol (ncp,
// DOCS.md section 10).  Node.js (Bun, Deno), no dependencies; in a
// browser, browser.ts is the package's entry.
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
//   await db.load("t", { id: new Int32Array([2, 3]), name: ["two", "three"] });
//   await db.copy("COPY t FROM STDIN WITH (FORMAT csv)", createReadStream("t.csv"));
//   await db.close();
//
// Values: boolean, smallint / integer as number, bigint as number (bigint
// beyond 2^53), real (its shortest digits) and double as number, numeric
// as Decimal (exact), text, char(n) and enums as string, bytea as Buffer,
// date / timestamp / timestamptz as Date (UTC; read with millisecond
// precision), time as Time, interval as Interval, inet as Inet, cidr as
// Cidr, uuid as its canonical string, vector as Float32Array, sparsevec
// as SparseVector, arrays as arrays, NULL as null;
// another registered type as Ext (its name and binary form).

import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import net from "node:net";
import { Duplex } from "node:stream";
import tls from "node:tls";

import { Buffer } from "./bytes.ts";
import { Client, NucleusError, DEFAULT_PORT, parseTarget, setTransport } from "./core.ts";
import type { ConnectOptions } from "./core.ts";

export * from "./core.ts";

// ------------------------------------------------------------------ a server

/** A TCP connection (TLS from the first byte when asked: DOCS §10). */
setTransport(
  (options) =>
    new Promise<net.Socket>((resolve, reject) => {
      const host = options.host ?? "127.0.0.1";
      const port = options.port ?? DEFAULT_PORT;
      const secure = options.tls === true || options.tlsCa !== undefined;
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
    }),
);

// ----------------------------------------------------- in this process

/** The embedded edition's addon (native/addon.c, PLAN.md §M14.23). */
interface Addon {
  version(): string;
  open(path: string, options: string): object;
  close(db: object): void;
  connect(db: object, onData: (chunk: Buffer | null) => void): object;
  send(conn: object, data: Buffer): boolean;
  end(conn: object): void;
}

let addon: Addon | null = null;

/** The addon: `NUCLEUS_EMBEDDED_ADDON` (a path), the package made for
 *  this platform (`nucleus-db-<platform>-<arch>`), or one built here
 *  (`native/build.sh`). */
function loadAddon(): Addon {
  if (addon) return addon;
  const require = createRequire(import.meta.url);
  const platform = `${process.platform}-${process.arch}`;
  const given = process.env.NUCLEUS_EMBEDDED_ADDON;
  const here = new URL(`../native/${platform}/nucleus.node`, import.meta.url);
  const tried: string[] = [];
  for (const candidate of [given, `nucleus-db-${platform}`, here.pathname]) {
    if (!candidate) continue;
    tried.push(candidate);
    if (candidate.startsWith("/") && !existsSync(candidate)) continue;
    try {
      addon = require(candidate) as Addon;
      return addon;
    } catch (e) {
      if (candidate === given) throw e;
    }
  }
  throw new NucleusError(
    `the embedded edition is not installed (tried ${tried.join(", ")}): install nucleus-db-${platform}, ` +
      "build it (native/build.sh), or set NUCLEUS_EMBEDDED_ADDON",
  );
}

/** A session's bytes in this process, as a socket carries them. */
class Session extends Duplex {
  conn: object;
  native: Addon;

  constructor(native: Addon, db: object) {
    super();
    this.native = native;
    this.conn = native.connect(db, (chunk) => {
      if (chunk === null) {
        this.push(null);
        this.destroy();
      } else this.push(chunk);
    });
  }

  override _read(): void {}

  override _write(chunk: Buffer, _encoding: BufferEncoding, done: (e?: Error | null) => void): void {
    done(this.native.send(this.conn, chunk) ? null : new NucleusError("the session has ended"));
  }

  override _final(done: (e?: Error | null) => void): void {
    this.native.end(this.conn);
    done();
  }

  override _destroy(e: Error | null, done: (e?: Error | null) => void): void {
    this.native.end(this.conn);
    done(e);
  }
}

/**
 * A database in this process — the embedded edition (no server): in
 * directory `path` (made if missing) or, for `":memory:"`, in memory,
 * gone when closed.  Each `connect()` is a session on it with the API a
 * server's connection has.  A directory this process has open already is
 * shared; another process's is refused.  There is no login: the
 * directory's file permissions guard it.
 */
export class Database {
  readonly path: string;
  private db: object | null;
  private native: Addon;

  private constructor(native: Addon, path: string, db: object) {
    this.native = native;
    this.path = path;
    this.db = db;
  }

  /** Opens it; `options` as `"cache=64MB; sync=full"`. */
  static open(path = ":memory:", options = ""): Database {
    const native = loadAddon();
    try {
      return new Database(native, path, native.open(path, options));
    } catch (e) {
      throw new NucleusError(`cannot open ${path}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  /** A new session, as `user` (who it runs as: there is no login). */
  connect(options: { user?: string; database?: string; clientName?: string } = {}): Promise<Client> {
    if (this.db === null) return Promise.reject(new NucleusError("the database is closed"));
    return Client.hello(new Session(this.native, this.db), options);
  }

  /** Closes it: its sessions end (each after its statement), and a
   *  directory is checkpointed and free for others again. */
  close(): void {
    const db = this.db;
    this.db = null;
    if (db !== null) this.native.close(db);
  }
}

/**
 * Opens a connection (the defaults reach a local server as it starts).
 *
 * A connection string instead moves a program between a server and a
 * database in its own process by changing only that string:
 * `"nucleus://user:password@host:port/database?tls=true"` (a server;
 * `tls`, `tls_ca`, `tls_verify`, `tls_cert`, `tls_key`, `token` and
 * `timeout` as parameters), `"file:app.db"` (a database in directory
 * `app.db` in this process, made if missing: the embedded edition; its
 * options as parameters, `file:app.db?cache=64MB`) or `":memory:"`.
 * `options` override what the string says.
 */
export async function connect(target: string | ConnectOptions = {}, options: ConnectOptions = {}): Promise<Client> {
  if (typeof target !== "string") return Client.connect(target);
  const t = parseTarget(target);
  if ("file" in t) {
    const db = Database.open(t.file, options.options ?? t.options);
    try {
      const c = await db.connect(options);
      c.owned = db;
      return c;
    } catch (e) {
      db.close();
      throw e;
    }
  }
  return Client.connect({ ...t, ...options });
}
