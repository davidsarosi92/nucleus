// The engine's worker in a page (PLAN.md section M14.23): browser.ts's
// Database sends it calls; it runs them on the engine, its storage in
// OPFS (reachable for synchronous access only from a worker).

import { LocalBackend } from "./browser.ts";

let backend: LocalBackend | null = null;

type Call = { id: number; op: string; args: unknown[] };

self.onmessage = async (ev: MessageEvent<Call>) => {
  const { id, op, args } = ev.data;
  try {
    let value: unknown;
    if (op === "init") {
      backend = new LocalBackend((args[0] ?? {}) as { wasm?: string; pool?: string });
    } else {
      backend ??= new LocalBackend();
      const b = backend as unknown as Record<string, (...a: unknown[]) => Promise<unknown>>;
      value = await b[op](...args);
    }
    const transfer = op === "feed" ? [(value as { reply: Uint8Array }).reply.buffer as ArrayBuffer] : [];
    (self as unknown as Worker).postMessage({ id, ok: true, value }, transfer);
  } catch (e) {
    (self as unknown as Worker).postMessage({ id, ok: false, error: e instanceof Error ? e.message : String(e) });
  }
};
