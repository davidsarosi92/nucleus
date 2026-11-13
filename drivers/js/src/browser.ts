// The nucleus driver in a browser (PLAN.md section M14.23): the same API
// as Node's — `connect`, `Client`, its results — on a database in the
// page, the engine in WebAssembly.  In a page the engine runs in a worker
// of its own (its storage, OPFS, is reachable only there); in a worker,
// and in Node, Bun and Deno, on the calling thread.
//
//   import { connect } from "nucleus-db";      // the browser's entry
//   const db = await connect("file:app.db");   // kept in OPFS: survives reloads
//   const mem = await connect(":memory:");     // gone with the page
//
// A browser cannot reach a server over TCP: `nucleus://` is refused here.

import { Buffer } from "./bytes.ts";
import { Client, NucleusError, parseTarget } from "./core.ts";
import type { ConnectOptions, Socket } from "./core.ts";
import { compile, Engine } from "./wasm.ts";
import type { SlotStore } from "./wasm.ts";

export * from "./core.ts";
export { MemorySlots } from "./wasm.ts";
export type { SlotStore } from "./wasm.ts";

/** Where the engine and its storage come from. */
export interface Settings {
  /** The engine's module: a URL, a path (Node), its bytes, or compiled
   *  (default: `wasm/nucleus.wasm` beside the package's code). */
  wasm?: string | URL | BufferSource | WebAssembly.Module;
  /** Where directories are kept: OPFS's directory in a browser, a
   *  directory of files in Node (`NUCLEUS_POOL`); default `.nucleus`. */
  pool?: string;
  /** A slot store of one's own (a browser's or Node's by default). */
  slots?: SlotStore;
  /** In a page: the worker script (default: `worker.js` beside this). */
  worker?: string | URL;
}

/** What runs the engine: on this thread, or in a worker. */
interface Backend {
  open(path: string, options: string): Promise<number>;
  close(db: number): Promise<void>;
  session(db: number): Promise<number>;
  feed(x: number, bytes: Uint8Array): Promise<{ reply: Uint8Array; ended: boolean }>;
  end(x: number): Promise<void>;
}

/** Slots set once storage is first needed (a page that only uses
 *  `:memory:` never opens OPFS). */
class LazySlots implements SlotStore {
  store: SlotStore | null = null;
  count(): number { return this.store?.count() ?? 0; }
  read(s: number, b: Uint8Array, o: number): number { return this.store!.read(s, b, o); }
  write(s: number, b: Uint8Array, o: number): void { this.store!.write(s, b, o); }
  size(s: number): number { return this.store!.size(s); }
  truncate(s: number, l: number): void { this.store!.truncate(s, l); }
  flush(s: number): void { this.store!.flush(s); }
}

/** Free slots below which more are added between calls, and how many. */
const LOW = 8;
const TOP_UP = 16;

const isNode = typeof process !== "undefined" && process.versions?.node !== undefined;
const inPage = !isNode && typeof (globalThis as { document?: unknown }).document !== "undefined";

/** The engine on this thread. */
export class LocalBackend implements Backend {
  private readonly lazy = new LazySlots();
  private engine: Engine | null = null;

  private readonly settings: Settings;

  constructor(settings: Settings = {}) {
    this.settings = settings;
  }

  private async ready(): Promise<Engine> {
    if (this.engine === null) {
      const module = await compile(this.settings.wasm ?? new URL("../wasm/nucleus.wasm", import.meta.url));
      this.engine = await Engine.start(module, this.lazy);
    }
    return this.engine;
  }

  /** Storage for directories, opened on first use. */
  private async storage(): Promise<void> {
    if (this.lazy.store !== null) return;
    if (this.settings.slots) {
      this.lazy.store = this.settings.slots;
    } else if (isNode) {
      const { FileSlots } = await import("./node-slots.ts");
      this.lazy.store = new FileSlots(this.settings.pool ?? process.env.NUCLEUS_POOL ?? ".nucleus");
    } else {
      const { OpfsSlots } = await import("./opfs.ts");
      this.lazy.store = await OpfsSlots.open(this.settings.pool ?? ".nucleus");
    }
  }

  /** More slots when few are free (between calls: adding is async in a
   *  browser). */
  private async topUp(): Promise<void> {
    const store = this.lazy.store as (SlotStore & { add?(n: number): unknown }) | null;
    if (store?.add && this.engine && this.engine.freeSlots() >= 0 && this.engine.freeSlots() < LOW) {
      await store.add(TOP_UP);
    }
  }

  async open(path: string, options: string): Promise<number> {
    const engine = await this.ready();
    if (path !== ":memory:") {
      await this.storage();
      const db = engine.open(path, options);
      await this.topUp();
      return db;
    }
    return engine.open(path, options);
  }

  async close(db: number): Promise<void> {
    (await this.ready()).close(db);
  }

  async session(db: number): Promise<number> {
    return (await this.ready()).session(db);
  }

  async feed(x: number, bytes: Uint8Array): Promise<{ reply: Uint8Array; ended: boolean }> {
    const engine = await this.ready();
    const reply = engine.feed(x, bytes);
    const ended = engine.ended(x);
    await this.topUp();
    return { reply, ended };
  }

  async end(x: number): Promise<void> {
    (await this.ready()).end(x);
  }
}

/** The engine in a worker (a page's main thread). */
class WorkerBackend implements Backend {
  private readonly worker: Worker;
  private next = 1;
  private readonly waiting = new Map<number, { resolve(v: unknown): void; reject(e: Error): void }>();

  constructor(settings: Settings) {
    this.worker = new Worker(settings.worker ?? new URL("./worker.js", import.meta.url), { type: "module" });
    this.worker.onmessage = (ev: MessageEvent) => {
      const { id, ok, value, error } = ev.data as { id: number; ok: boolean; value: unknown; error: string };
      const w = this.waiting.get(id);
      this.waiting.delete(id);
      if (ok) w?.resolve(value);
      else w?.reject(new NucleusError(error));
    };
    const { wasm, pool } = settings;
    void this.call("init", [{ wasm: wasm instanceof URL ? wasm.href : wasm, pool }]);
  }

  private call<T>(op: string, args: unknown[], transfer: Transferable[] = []): Promise<T> {
    const id = this.next++;
    return new Promise<T>((resolve, reject) => {
      this.waiting.set(id, { resolve: resolve as (v: unknown) => void, reject });
      this.worker.postMessage({ id, op, args }, transfer);
    });
  }

  open(path: string, options: string): Promise<number> {
    return this.call("open", [path, options]);
  }
  close(db: number): Promise<void> {
    return this.call("close", [db]);
  }
  session(db: number): Promise<number> {
    return this.call("session", [db]);
  }
  feed(x: number, bytes: Uint8Array): Promise<{ reply: Uint8Array; ended: boolean }> {
    return this.call("feed", [x, bytes]);
  }
  end(x: number): Promise<void> {
    return this.call("end", [x]);
  }
}

let shared: Backend | null = null;

function backend(settings: Settings): Backend {
  if (Object.keys(settings).length > 0) return inPage ? new WorkerBackend(settings) : new LocalBackend(settings);
  shared ??= inPage ? new WorkerBackend({}) : new LocalBackend({});
  return shared;
}

type Listener = (...a: never[]) => void;

/** A session's bytes as a socket carries them: what the client writes in
 *  one turn of the event loop goes to the engine together (so a COPY's
 *  data goes with it), and the reply comes back as data. */
class Session implements Socket {
  readonly eager = true;
  private pending: Uint8Array[] = [];
  private flushing: Promise<void> = Promise.resolve();
  private scheduled = false;
  private ended = false;
  private readonly listeners = new Map<string, Listener[]>();

  private readonly backend: Backend;
  private readonly x: number;

  constructor(backend: Backend, x: number) {
    this.backend = backend;
    this.x = x;
  }

  on(event: string, f: Listener): this {
    this.listeners.set(event, [...(this.listeners.get(event) ?? []), f]);
    return this;
  }

  once(event: string, f: Listener): this {
    const g = ((...a: never[]) => {
      this.listeners.set(event, (this.listeners.get(event) ?? []).filter((h) => h !== g));
      f(...a);
    }) as Listener;
    return this.on(event, g);
  }

  private emit(event: string, ...a: unknown[]): void {
    for (const f of this.listeners.get(event) ?? []) (f as (...a: unknown[]) => void)(...a);
  }

  write(b: Uint8Array): boolean {
    if (this.ended) return false;
    this.pending.push(b);
    if (!this.scheduled) {
      this.scheduled = true;
      queueMicrotask(() => {
        this.scheduled = false;
        this.flush();
      });
    }
    return true;
  }

  /** Sends what is pending, in order after what was sent before. */
  private flush(): Promise<void> {
    if (this.pending.length === 0) return this.flushing;
    const bytes = this.pending.length === 1 ? this.pending[0] : Buffer.concat(this.pending);
    this.pending = [];
    this.flushing = this.flushing.then(async () => {
      try {
        const { reply, ended } = await this.backend.feed(this.x, bytes);
        if (reply.length) this.emit("data", Buffer.from(reply.buffer, reply.byteOffset, reply.byteLength));
        if (ended) this.finish();
      } catch (e) {
        this.emit("error", e instanceof Error ? e : new NucleusError(String(e)));
        this.finish();
      }
    });
    return this.flushing;
  }

  private finish(): void {
    if (this.ended) return;
    this.ended = true;
    void this.backend.end(this.x);
    this.emit("close");
  }

  end(b: Uint8Array, done: () => void): void {
    this.write(b);
    this.scheduled = false;
    void this.flush().then(() => {
      this.finish();
      done();
    });
  }

  destroy(): void {
    this.pending = [];
    void this.flushing.then(() => this.finish());
  }
}

/**
 * A database in the page — the embedded edition in WebAssembly: in
 * directory `path` of the page's storage (OPFS: it survives reloads; one
 * tab holds it at a time) or, for `":memory:"`, in memory.  Each
 * `connect()` is a session with the API a server's connection has.
 * There is no login.
 */
export class Database {
  private db: number | null;

  private readonly backend: Backend;
  readonly path: string;

  private constructor(backend: Backend, path: string, db: number) {
    this.backend = backend;
    this.path = path;
    this.db = db;
  }

  /** Opens it; `options` as `"cache=64MB"`. */
  static async open(path = ":memory:", options = "", settings: Settings = {}): Promise<Database> {
    const b = backend(settings);
    return new Database(b, path, await b.open(path, options));
  }

  /** A new session, as `user` (who it runs as: there is no login). */
  async connect(options: { user?: string; database?: string; clientName?: string } = {}): Promise<Client> {
    if (this.db === null) throw new NucleusError("the database is closed");
    const x = await this.backend.session(this.db);
    return Client.hello(new Session(this.backend, x), { clientName: "nucleus-js (wasm)", ...options });
  }

  /** Closes it: a directory is checkpointed.  Its sessions are to be
   *  closed first. */
  async close(): Promise<void> {
    const db = this.db;
    this.db = null;
    if (db !== null) await this.backend.close(db);
  }
}

/**
 * Opens a connection to a database in the page: `"file:app.db"` (kept,
 * options as parameters: `file:app.db?cache=64MB`) or `":memory:"` — the
 * same strings, and the same client, as on a server's machine.
 */
export async function connect(target: string | ConnectOptions = ":memory:", options: ConnectOptions & Settings = {}): Promise<Client> {
  const t = typeof target === "string" ? parseTarget(target) : null;
  if (t === null || !("file" in t)) {
    throw new NucleusError("a browser reaches a database in the page (file:, :memory:), not a server");
  }
  const { wasm, pool, slots, worker, ...rest } = options;
  const settings: Settings = Object.fromEntries(
    Object.entries({ wasm, pool, slots, worker }).filter(([, v]) => v !== undefined),
  );
  const db = await Database.open(t.file, rest.options ?? t.options, settings);
  try {
    const c = await db.connect(rest);
    c.owned = db;
    return c;
  } catch (e) {
    await db.close();
    throw e;
  }
}

export type { Backend };
