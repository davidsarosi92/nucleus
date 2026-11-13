// The browser's slot pool: files in the Origin Private File System, each
// held open for synchronous access (PLAN.md section M14.23).  Only a
// worker may hold such a handle, and opening one is asynchronous — which
// the engine, synchronous, cannot wait for mid-statement — so the pool is
// opened ahead and topped up between calls (SQLite's opfs-sahpool idea).
// One page holds the pool at a time: a second tab's open is refused.

import type { SlotStore } from "./wasm.ts";

interface SyncHandle {
  read(into: Uint8Array, o: { at: number }): number;
  write(from: Uint8Array, o: { at: number }): number;
  getSize(): number;
  truncate(len: number): void;
  flush(): void;
  close(): void;
}

interface FileHandle {
  createSyncAccessHandle(): Promise<SyncHandle>;
}

interface DirHandle {
  getDirectoryHandle(name: string, o?: { create?: boolean }): Promise<DirHandle>;
  getFileHandle(name: string, o?: { create?: boolean }): Promise<FileHandle>;
  keys(): AsyncIterable<string>;
}

export class OpfsSlots implements SlotStore {
  private handles: SyncHandle[] = [];
  private readonly dir: DirHandle;

  private constructor(dir: DirHandle) {
    this.dir = dir;
  }

  /** The pool in OPFS directory `name`, at least `n` slots. */
  static async open(name = ".nucleus", n = 16): Promise<OpfsSlots> {
    const storage = (globalThis.navigator as unknown as { storage: { getDirectory(): Promise<DirHandle> } }).storage;
    const root = await storage.getDirectory();
    const dir = await root.getDirectoryHandle(name, { create: true });
    const pool = new OpfsSlots(dir);
    const names: string[] = [];
    for await (const k of dir.keys()) if (/^slot-\d+$/.test(k)) names.push(k);
    names.sort();
    try {
      for (const k of names) pool.handles.push(await (await dir.getFileHandle(k)).createSyncAccessHandle());
    } catch (e) {
      pool.close();
      throw new Error(`the database's storage is open in another tab or worker (${e instanceof Error ? e.message : e})`);
    }
    await pool.add(Math.max(0, n - pool.handles.length));
    return pool;
  }

  /** Adds `n` empty slots. */
  async add(n: number): Promise<void> {
    for (let i = 0; i < n; i++) {
      const name = `slot-${String(this.handles.length).padStart(6, "0")}`;
      const f = await this.dir.getFileHandle(name, { create: true });
      this.handles.push(await f.createSyncAccessHandle());
    }
  }

  count(): number {
    return this.handles.length;
  }

  read(slot: number, into: Uint8Array, offset: number): number {
    return this.handles[slot].read(into, { at: offset });
  }

  write(slot: number, from: Uint8Array, offset: number): void {
    let n = 0;
    while (n < from.length) n += this.handles[slot].write(from.subarray(n), { at: offset + n });
  }

  size(slot: number): number {
    return this.handles[slot].getSize();
  }

  truncate(slot: number, len: number): void {
    this.handles[slot].truncate(len);
  }

  flush(slot: number): void {
    this.handles[slot].flush();
  }

  /** Lets the files go (another page may open the pool then). */
  close(): void {
    for (const h of this.handles) h.close();
    this.handles = [];
  }
}
