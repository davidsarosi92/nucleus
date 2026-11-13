// Node's Buffer where there is one; elsewhere (a browser, a worker) a
// Uint8Array with the part of Buffer's interface the driver uses (PLAN.md
// section M14.23).

type Encoding = "utf8" | "latin1" | "hex" | "base64";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function bytesOf(s: string, enc: Encoding = "utf8"): Uint8Array {
  switch (enc) {
    case "utf8":
      return encoder.encode(s);
    case "latin1":
      return Uint8Array.from(s, (c) => c.charCodeAt(0) & 0xff);
    case "hex": {
      const out = new Uint8Array(s.length >> 1);
      for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
      return out;
    }
    case "base64": {
      const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/"));
      return Uint8Array.from(bin, (c) => c.charCodeAt(0));
    }
  }
}

/** The subset of Buffer the driver uses, over a Uint8Array. */
class Shim extends Uint8Array {
  private view(): DataView {
    return new DataView(this.buffer, this.byteOffset, this.byteLength);
  }

  static alloc(n: number, fill = 0): Shim {
    const b = new Shim(n);
    if (fill) b.fill(fill);
    return b;
  }

  static override from(x: unknown, a?: unknown, b?: unknown): Shim {
    if (typeof x === "string") {
      const v = bytesOf(x, (a as Encoding) ?? "utf8");
      return new Shim(v.buffer as ArrayBuffer, v.byteOffset, v.byteLength);
    }
    if (x instanceof ArrayBuffer) return new Shim(x, (a as number) ?? 0, b as number | undefined);
    const src = x as ArrayLike<number>;
    const out = new Shim(src.length);
    out.set(src);
    return out;
  }

  static concat(list: readonly Uint8Array[], total?: number): Shim {
    const n = total ?? list.reduce((s, b) => s + b.length, 0);
    const out = new Shim(n);
    let at = 0;
    for (const b of list) {
      if (at >= n) break;
      const part = b.length > n - at ? b.subarray(0, n - at) : b;
      out.set(part, at);
      at += part.length;
    }
    return out;
  }

  static isBuffer(x: unknown): boolean {
    return x instanceof Shim;
  }

  override toString(enc: Encoding = "utf8", start = 0, end = this.length): string {
    const b = this.subarray(start, end);
    switch (enc) {
      case "utf8":
        return decoder.decode(b);
      case "latin1":
        return Array.from(b, (c) => String.fromCharCode(c)).join("");
      case "hex":
        return Array.from(b, (c) => c.toString(16).padStart(2, "0")).join("");
      case "base64":
        return btoa(Array.from(b, (c) => String.fromCharCode(c)).join(""));
    }
  }

  write(s: string, offset = 0, enc: Encoding = "utf8"): number {
    const v = bytesOf(s, enc).subarray(0, this.length - offset);
    this.set(v, offset);
    return v.length;
  }

  copy(target: Uint8Array, targetStart = 0, start = 0, end = this.length): number {
    const part = this.subarray(start, Math.min(end, start + target.length - targetStart));
    target.set(part, targetStart);
    return part.length;
  }

  readUInt16LE(o = 0): number { return this.view().getUint16(o, true); }
  readUInt16BE(o = 0): number { return this.view().getUint16(o, false); }
  readInt16LE(o = 0): number { return this.view().getInt16(o, true); }
  readUInt32LE(o = 0): number { return this.view().getUint32(o, true); }
  readInt32LE(o = 0): number { return this.view().getInt32(o, true); }
  readFloatLE(o = 0): number { return this.view().getFloat32(o, true); }
  readDoubleLE(o = 0): number { return this.view().getFloat64(o, true); }
  readBigInt64LE(o = 0): bigint { return this.view().getBigInt64(o, true); }
  readBigUInt64LE(o = 0): bigint { return this.view().getBigUint64(o, true); }
  writeUInt16LE(v: number, o = 0): number { this.view().setUint16(o, v, true); return o + 2; }
  writeUInt16BE(v: number, o = 0): number { this.view().setUint16(o, v, false); return o + 2; }
  writeInt16LE(v: number, o = 0): number { this.view().setInt16(o, v, true); return o + 2; }
  writeUInt32LE(v: number, o = 0): number { this.view().setUint32(o, v, true); return o + 4; }
  writeInt32LE(v: number, o = 0): number { this.view().setInt32(o, v, true); return o + 4; }
  writeFloatLE(v: number, o = 0): number { this.view().setFloat32(o, v, true); return o + 4; }
  writeDoubleLE(v: number, o = 0): number { this.view().setFloat64(o, v, true); return o + 8; }
  writeBigInt64LE(v: bigint, o = 0): number { this.view().setBigInt64(o, v, true); return o + 8; }
  writeBigUInt64LE(v: bigint, o = 0): number { this.view().setBigUint64(o, v, true); return o + 8; }
}

const node = (globalThis as { Buffer?: typeof globalThis.Buffer }).Buffer;

/** Node's Buffer, or the shim. */
export const Buffer: typeof globalThis.Buffer = node ?? (Shim as unknown as typeof globalThis.Buffer);
export type Buffer = globalThis.Buffer;
