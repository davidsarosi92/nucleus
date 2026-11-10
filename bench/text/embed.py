#!/usr/bin/env python3
"""Embeddings of BEIR's corpora and test queries for hybrid search (PLAN.md
§M14.21 T7): a dense model's vectors and a learned sparse model's term
weights, computed once by fastembed (ONNX, on the CPU) and kept beside the
data, so that `bench/text/hybrid.py` loads them into tables.

    bench/text/embed.py [--sets scifact,nfcorpus,arguana,fiqa]
                        [--dense BAAI/bge-small-en-v1.5]
                        [--sparse prithivida/Splade_PP_en_v1]
                        [--kinds dense,sparse]

Writes, in each set's directory: `dense-docs.npy` / `dense-queries.npy`
(float32, one row a document or a judged test query, in `corpus.jsonl`'s
and the judgements' order) and `sparse-docs.npz` / `sparse-queries.npz`
(CSR: `indptr`, `indices`, `values`, `dims`), with `ids.json` naming the
rows.  A file already there is kept.  Needs `pip install fastembed`.

One process, small batches: a sparse model's output is a score for
every word of its vocabulary at every token (32 texts of 512 tokens:
2 GB), and fastembed's `parallel` copies the model and that into a
process per core — it ran a 32 GB machine out of memory.  ONNX's
arena keeps what the longest batch took and more: a new model every
thousand texts, and one set and one kind a process
(`--sets arguana --kinds dense`) under a memory cap.
"""

import argparse
import gc
import json
import os
import sys
import time

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import beir  # noqa: E402


def progress(texts, what, every=2000):
    """`texts`, a line to stderr every `every` of them taken."""
    t = time.time()
    for i, x in enumerate(texts):
        if i and i % every == 0:
            print(f"  {what}: {i} in {time.time() - t:.0f} s", file=sys.stderr, flush=True)
        yield x


def chunked(make, texts, query, batch, chunk=1000):
    """`texts` embedded `batch` at a time by a model `make()` makes anew
    every `chunk` of them: ONNX's arena keeps what its largest batch
    took, and more with each new shape, until the model goes."""
    texts = list(texts)
    for i in range(0, len(texts), chunk):
        m = make()
        part = texts[i:i + chunk]
        out = m.query_embed(part, batch_size=batch) if query else m.embed(part, batch_size=batch)
        yield from out
        del m, out
        gc.collect()


def dense(model, texts, query):
    from fastembed import TextEmbedding
    out = chunked(lambda: TextEmbedding(model), texts, query, 16)
    return np.vstack([np.asarray(v, dtype=np.float32) for v in out])


def sparse(model, texts, query, dims=30522):
    from fastembed import SparseTextEmbedding
    out = chunked(lambda: SparseTextEmbedding(model), texts, query, 4)
    indptr, indices, values = [0], [], []
    for v in out:
        order = np.argsort(v.indices)
        indices.append(np.asarray(v.indices, dtype=np.int32)[order])
        values.append(np.asarray(v.values, dtype=np.float32)[order])
        indptr.append(indptr[-1] + len(order))
    return {
        "indptr": np.asarray(indptr, dtype=np.int64),
        "indices": np.concatenate(indices) if indices else np.zeros(0, np.int32),
        "values": np.concatenate(values) if values else np.zeros(0, np.float32),
        "dims": np.asarray([dims]),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sets", default="scifact,nfcorpus,arguana,fiqa")
    ap.add_argument("--data", default="target/text-data")
    ap.add_argument("--dense", default="BAAI/bge-small-en-v1.5")
    ap.add_argument("--sparse", default="prithivida/Splade_PP_en_v1")
    ap.add_argument("--kinds", default="dense,sparse")
    a = ap.parse_args()
    # Every set's dense vectors first, then the sparse ones (slower).
    for kind, fn, ext in [("dense", dense, "npy"), ("sparse", sparse, "npz")]:
        if kind not in a.kinds.split(","):
            continue
        model = getattr(a, kind)
        for name in a.sets.split(","):
            d = os.path.join(a.data, f"beir-{name}", name)
            docs = list(beir.corpus(d))
            queries, qrels = beir.load(d)
            qids = list(qrels)
            with open(os.path.join(d, "ids.json"), "w") as f:
                json.dump({"docs": [i for i, _ in docs], "queries": qids}, f)
            doc_texts = [t for _, t in docs]
            q_texts = [queries[q] for q in qids]
            for what, texts, query in [("queries", q_texts, True), ("docs", doc_texts, False)]:
                path = os.path.join(d, f"{kind}-{what}.{ext}")
                if os.path.exists(path):
                    continue
                t = time.time()
                out = fn(model, progress(texts, f"{name} {kind} {what}"), query)
                if ext == "npy":
                    np.save(path, out)
                else:
                    np.savez(path, **out)
                print(f"{name} {kind} {what}: {len(texts)} in {time.time() - t:.0f} s",
                      flush=True)


if __name__ == "__main__":
    main()
