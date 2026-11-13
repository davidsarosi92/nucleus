// The engine in WebAssembly (PLAN.md section M14.23): the embedded
// edition built for wasm32-wasip1 (`wasm/build.sh`), its C API called
// directly — no threads, so each session is stepped by the bytes given
// to it (`nce_feed`).  It runs wherever WebAssembly does: a browser's
// worker (browser.ts), Node, Bun, Deno.
//
// It imports two things: a minimal WASI (clocks, randomness, the console;
// no files — it has none of its own) and `nucleus_slots`, the files a
// database in a directory is kept in (a `SlotStore`: the browser's OPFS,
// or files or memory where there is none).

import { NucleusError } from "./core.ts";

/** Files opened ahead, addressed by number, that a database's files are
 *  kept in (the platform's slot pool, `nucleus_platform::slots`).  Slots
 *  may be added between calls, never taken away. */
export interface SlotStore {
  count(): number;
  /** Reads into `into` from `offset`: the bytes read (0 at the end). */
  read(slot: number, into: Uint8Array, offset: number): number;
  write(slot: number, from: Uint8Array, offset: number): void;
  size(slot: number): number;
  truncate(slot: number, len: number): void;
  flush(slot: number): void;
}

/** Slots in memory: gone with the page (for tests and as a fallback). */
export class MemorySlots implements SlotStore {
  private slots: Uint8Array[] = [];
  private lens: number[] = [];

  constructor(n = 16) {
    this.add(n);
  }

  add(n: number): void {
    for (let i = 0; i < n; i++) {
      this.slots.push(new Uint8Array(0));
      this.lens.push(0);
    }
  }

  count(): number {
    return this.slots.length;
  }

  private room(slot: number, end: number): void {
    if (this.slots[slot].length >= end) return;
    const grown = new Uint8Array(Math.max(end, this.slots[slot].length * 2));
    grown.set(this.slots[slot].subarray(0, this.lens[slot]));
    this.slots[slot] = grown;
  }

  read(slot: number, into: Uint8Array, offset: number): number {
    const n = Math.max(0, Math.min(into.length, this.lens[slot] - offset));
    into.set(this.slots[slot].subarray(offset, offset + n));
    return n;
  }

  write(slot: number, from: Uint8Array, offset: number): void {
    this.room(slot, offset + from.length);
    this.slots[slot].set(from, offset);
    this.lens[slot] = Math.max(this.lens[slot], offset + from.length);
  }

  size(slot: number): number {
    return this.lens[slot];
  }

  truncate(slot: number, len: number): void {
    this.room(slot, len);
    if (len > this.lens[slot]) this.slots[slot].fill(0, this.lens[slot], len);
    this.lens[slot] = len;
  }

  flush(): void {}
}

interface Exports {
  memory: WebAssembly.Memory;
  nce_version(): number;
  nce_alloc(len: number): number;
  nce_dealloc(p: number, len: number): void;
  nce_open(path: number, options: number, error: number): number;
  nce_close(db: number, error: number): number;
  nce_exchange(db: number, error: number): number;
  nce_feed(x: number, buf: number, len: number, replyLen: number): number;
  nce_exchange_ended(x: number): number;
  nce_exchange_free(x: number): void;
  nce_free_error(e: number): void;
  nce_pool_free(): bigint;
}

const ERRNO = { SUCCESS: 0, BADF: 8, INVAL: 28, NOENT: 44, NOSYS: 52 };

/** WASI preview 1, as much as the engine calls: no files, no arguments,
 *  no environment; stdout and stderr to the console. */
function wasi(memory: () => WebAssembly.Memory): Record<string, (...a: never[]) => number | void> {
  const view = () => new DataView(memory().buffer);
  const bytes = () => new Uint8Array(memory().buffer);
  const lines: Record<number, string> = { 1: "", 2: "" };
  const decoder = new TextDecoder();
  const start = typeof performance === "undefined" ? Date.now() : performance.now();
  return {
    args_sizes_get: (count: number, size: number) => {
      view().setUint32(count, 0, true);
      view().setUint32(size, 0, true);
      return ERRNO.SUCCESS;
    },
    args_get: () => ERRNO.SUCCESS,
    environ_sizes_get: (count: number, size: number) => {
      view().setUint32(count, 0, true);
      view().setUint32(size, 0, true);
      return ERRNO.SUCCESS;
    },
    environ_get: () => ERRNO.SUCCESS,
    clock_time_get: (id: number, _precision: bigint, out: number) => {
      // 0: the wall clock; else monotonic (from this instance's start).
      const ms = id === 0 ? Date.now() : (typeof performance === "undefined" ? Date.now() : performance.now()) - start;
      view().setBigUint64(out, BigInt(Math.round(ms * 1e6)), true);
      return ERRNO.SUCCESS;
    },
    random_get: (buf: number, len: number) => {
      for (let at = 0; at < len; at += 65536) {
        globalThis.crypto.getRandomValues(bytes().subarray(buf + at, buf + Math.min(len, at + 65536)));
      }
      return ERRNO.SUCCESS;
    },
    fd_write: (fd: number, iovs: number, n: number, written: number) => {
      if (fd !== 1 && fd !== 2) return ERRNO.BADF;
      let total = 0;
      for (let i = 0; i < n; i++) {
        const p = view().getUint32(iovs + 8 * i, true);
        const len = view().getUint32(iovs + 8 * i + 4, true);
        lines[fd] += decoder.decode(bytes().subarray(p, p + len));
        total += len;
      }
      const parts = lines[fd].split("\n");
      lines[fd] = parts.pop() ?? "";
      for (const line of parts) (fd === 1 ? console.log : console.error)(line);
      view().setUint32(written, total, true);
      return ERRNO.SUCCESS;
    },
    fd_read: () => ERRNO.BADF,
    fd_close: () => ERRNO.BADF,
    fd_seek: () => ERRNO.BADF,
    fd_fdstat_get: (fd: number, out: number) => {
      if (fd > 2) return ERRNO.BADF;
      // A character device, no flags, no rights to speak of.
      new Uint8Array(memory().buffer, out, 24).fill(0);
      view().setUint8(out, 2);
      return ERRNO.SUCCESS;
    },
    fd_filestat_get: () => ERRNO.BADF,
    fd_prestat_get: () => ERRNO.BADF,
    fd_prestat_dir_name: () => ERRNO.BADF,
    path_open: () => ERRNO.NOENT,
    poll_oneoff: (subs: number, events: number, n: number, out: number) => {
      // A sleep (the only use): returns at once, every subscription done.
      for (let i = 0; i < n; i++) {
        const userdata = view().getBigUint64(subs + 48 * i, true);
        const type = view().getUint8(subs + 48 * i + 8);
        new Uint8Array(memory().buffer, events + 32 * i, 32).fill(0);
        view().setBigUint64(events + 32 * i, userdata, true);
        view().setUint8(events + 32 * i + 10, type);
      }
      view().setUint32(out, n, true);
      return ERRNO.SUCCESS;
    },
    sched_yield: () => ERRNO.SUCCESS,
    proc_exit: (code: number) => {
      throw new NucleusError(`the engine exited (${code})`);
    },
  };
}

/** The engine's module, compiled once: from a URL (fetched), a path
 *  (Node), or its bytes. */
export async function compile(source: string | URL | BufferSource | WebAssembly.Module): Promise<WebAssembly.Module> {
  if (source instanceof WebAssembly.Module) return source;
  if (typeof source !== "string" && !(source instanceof URL)) return WebAssembly.compile(source);
  const url = source instanceof URL ? source : null;
  if (url === null || url.protocol === "file:") {
    // Node (Bun, Deno): from the file.
    const { readFile } = await import("node:fs/promises");
    return WebAssembly.compile(await readFile(url ?? source));
  }
  const response = fetch(url);
  return WebAssembly.compileStreaming === undefined
    ? WebAssembly.compile(await (await response).arrayBuffer())
    : WebAssembly.compileStreaming(response);
}

/** One instance of the engine: its databases and sessions, all on the
 *  calling thread. */
export class Engine {
  private x!: Exports;
  private readonly encoder = new TextEncoder();

  private constructor() {}

  /** An instance of `module`, its directories kept in `slots` (none: in
   *  memory only). */
  static async start(module: WebAssembly.Module, slots: SlotStore | null): Promise<Engine> {
    const e = new Engine();
    const memory = () => e.x.memory;
    const mem = () => new Uint8Array(e.x.memory.buffer);
    const none = () => {
      throw new NucleusError("no storage here: only :memory:");
    };
    const store = slots ?? { count: () => 0, read: none, write: none, size: none, truncate: none, flush: none };
    // Errors become -1 (the engine reports what failed).
    const guard = <T>(f: () => T, failed: T): T => {
      try {
        return f();
      } catch (err) {
        console.error("nucleus: storage:", err);
        return failed;
      }
    };
    const imports = {
      wasi_snapshot_preview1: wasi(memory),
      nucleus_slots: {
        count: () => store.count(),
        read: (slot: number, p: number, len: number, at: number) =>
          guard(() => store.read(slot, mem().subarray(p, p + len), at), -1),
        write: (slot: number, p: number, len: number, at: number) =>
          guard(() => (store.write(slot, mem().subarray(p, p + len), at), 0), -1),
        size: (slot: number) => guard(() => store.size(slot), -1),
        truncate: (slot: number, len: number) => guard(() => (store.truncate(slot, len), 0), -1),
        flush: (slot: number) => guard(() => (store.flush(slot), 0), -1),
      },
    };
    const instance = await WebAssembly.instantiate(module, imports as unknown as WebAssembly.Imports);
    e.x = instance.exports as unknown as Exports;
    (instance.exports._initialize as (() => void) | undefined)?.();
    return e;
  }

  private mem(): Uint8Array {
    return new Uint8Array(this.x.memory.buffer);
  }

  /** A C string in the engine's memory (free with `dealloc`). */
  private cString(s: string): [number, number] {
    const b = this.encoder.encode(s);
    const p = this.x.nce_alloc(b.length + 1);
    this.mem().set(b, p);
    this.mem()[p + b.length] = 0;
    return [p, b.length + 1];
  }

  private readC(p: number): string {
    const m = this.mem();
    let end = p;
    while (m[end] !== 0) end++;
    return new TextDecoder().decode(m.subarray(p, end));
  }

  /** Calls `f` with a `char **` for an error; throws what it holds. */
  private withError<T>(what: string, f: (error: number) => T, failed: (v: T) => boolean): T {
    const slot = this.x.nce_alloc(4);
    new DataView(this.x.memory.buffer).setUint32(slot, 0, true);
    try {
      const v = f(slot);
      if (failed(v)) {
        const e = new DataView(this.x.memory.buffer).getUint32(slot, true);
        const message = e === 0 ? "failed" : this.readC(e);
        if (e !== 0) this.x.nce_free_error(e);
        throw new NucleusError(`${what}: ${message}`);
      }
      return v;
    } finally {
      this.x.nce_dealloc(slot, 4);
    }
  }

  version(): string {
    return this.readC(this.x.nce_version());
  }

  /** Opens a database (`":memory:"`, or a directory in the slots). */
  open(path: string, options = ""): number {
    const [p, pl] = this.cString(path);
    const [o, ol] = this.cString(options);
    try {
      return this.withError(`cannot open ${path}`, (e) => this.x.nce_open(p, o, e), (db) => db === 0);
    } finally {
      this.x.nce_dealloc(p, pl);
      this.x.nce_dealloc(o, ol);
    }
  }

  /** Closes a database: it checkpoints. */
  close(db: number): void {
    this.withError("cannot close", (e) => this.x.nce_close(db, e), (r) => r !== 0);
  }

  /** A new session on `db`. */
  session(db: number): number {
    return this.withError("cannot start a session", (e) => this.x.nce_exchange(db, e), (x) => x === 0);
  }

  /** Gives session `x` the client's bytes; its reply (a copy). */
  feed(x: number, bytes: Uint8Array): Uint8Array {
    const p = this.x.nce_alloc(bytes.length);
    const lenAt = this.x.nce_alloc(4);
    try {
      this.mem().set(bytes, p);
      const r = this.x.nce_feed(x, p, bytes.length, lenAt);
      const n = new DataView(this.x.memory.buffer).getUint32(lenAt, true);
      return this.mem().slice(r, r + n);
    } finally {
      this.x.nce_dealloc(p, bytes.length);
      this.x.nce_dealloc(lenAt, 4);
    }
  }

  /** Whether session `x` has ended. */
  ended(x: number): boolean {
    return this.x.nce_exchange_ended(x) !== 0;
  }

  /** Frees session `x`. */
  end(x: number): void {
    this.x.nce_exchange_free(x);
  }

  /** The slots of the pool not yet taken (-1: none opened). */
  freeSlots(): number {
    return Number(this.x.nce_pool_free());
  }
}
