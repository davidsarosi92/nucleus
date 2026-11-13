// A slot pool in a directory of files (Node, Bun, Deno): what the
// browser keeps in OPFS, here on disk — so the edition in WebAssembly is
// tested, and persists, outside a browser too (PLAN.md section M14.23).

import { closeSync, existsSync, fstatSync, fsyncSync, ftruncateSync, mkdirSync, openSync, readdirSync, readSync, writeSync } from "node:fs";
import { join } from "node:path";

import type { SlotStore } from "./wasm.ts";

export class FileSlots implements SlotStore {
  private fds: number[] = [];
  private readonly dir: string;

  /** The pool in directory `dir` (made if missing), at least `n` slots. */
  constructor(dir: string, n = 16) {
    this.dir = dir;
    mkdirSync(dir, { recursive: true });
    const names = existsSync(dir) ? readdirSync(dir).filter((f) => /^slot-\d+$/.test(f)).sort() : [];
    for (const name of names) this.fds.push(openSync(join(dir, name), "r+"));
    this.add(Math.max(0, n - this.fds.length));
  }

  /** Adds `n` empty slots. */
  add(n: number): void {
    for (let i = 0; i < n; i++) {
      const name = `slot-${String(this.fds.length).padStart(6, "0")}`;
      this.fds.push(openSync(join(this.dir, name), "w+"));
    }
  }

  count(): number {
    return this.fds.length;
  }

  read(slot: number, into: Uint8Array, offset: number): number {
    let n = 0;
    while (n < into.length) {
      const k = readSync(this.fds[slot], into, n, into.length - n, offset + n);
      if (k === 0) break;
      n += k;
    }
    return n;
  }

  write(slot: number, from: Uint8Array, offset: number): void {
    let n = 0;
    while (n < from.length) n += writeSync(this.fds[slot], from, n, from.length - n, offset + n);
  }

  size(slot: number): number {
    return fstatSync(this.fds[slot]).size;
  }

  truncate(slot: number, len: number): void {
    ftruncateSync(this.fds[slot], len);
  }

  flush(slot: number): void {
    fsyncSync(this.fds[slot]);
  }

  close(): void {
    for (const fd of this.fds) closeSync(fd);
    this.fds = [];
  }
}
