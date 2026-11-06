#!/usr/bin/env python3
"""hnswlib (the HNSW authors' library) on a dataset in bench/ann/data.py's
format, with the prototypes' parameters and output — the reference the
core in bench/ann/src/hnsw.rs is held to (PLAN.md §M14.12 V4).

    bench/ann/hnswlib_ref.py DIR [--m 16] [--efc 64] [--k 10]
                             [--threads 10] [--queries N]

Built on one thread, rows in order; recall@k as `ann recall` counts it
(the true k nearest found among the k returned).  Needs numpy and
hnswlib (bench/ann/data.py's virtual environment).
"""

import argparse
import os
import time

import hnswlib
import numpy as np


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--m", type=int, default=16)
    ap.add_argument("--efc", type=int, default=64)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--threads", type=int, default=10)
    ap.add_argument("--queries", type=int, default=None)
    a = ap.parse_args()
    meta = dict(kv.split("=") for kv in open(os.path.join(a.dir, "meta.txt")).read().split())
    dim, nq = int(meta["dim"]), int(meta["test"])
    train = np.fromfile(os.path.join(a.dir, "train.f32"), dtype="<f4").reshape(-1, dim)
    test = np.fromfile(os.path.join(a.dir, "test.f32"), dtype="<f4").reshape(-1, dim)
    truth = np.fromfile(os.path.join(a.dir, "neighbors.i32"), dtype="<i4").reshape(nq, -1)
    if a.queries:
        test, truth = test[: a.queries], truth[: a.queries]
    space = {"l2": "l2", "cosine": "ip"}[meta["metric"]]

    idx = hnswlib.Index(space=space, dim=dim)
    idx.init_index(max_elements=len(train), M=a.m, ef_construction=a.efc, random_seed=100)
    idx.set_num_threads(1)
    t = time.perf_counter()
    for i in range(len(train)):
        idx.add_items(train[i : i + 1], [i])
    build = time.perf_counter() - t
    print(
        f"hnswlib: n {len(train)}, dim {dim}, M {a.m}, ef_construction {a.efc}: "
        f"built in {build:.1f} s ({build * 1e6 / len(train):.1f} µs/insert, Python call per row)"
    )
    print(f"ef_search  recall@{a.k}  µs/search  QPS({a.threads})")
    want = [set(t[: a.k]) for t in truth]
    for ef in [10, 20, 40, 80, 120, 200, 400]:
        idx.set_ef(max(ef, a.k))
        idx.set_num_threads(1)
        t = time.perf_counter()
        got = [idx.knn_query(q, k=a.k)[0][0] for q in test]
        us = (time.perf_counter() - t) * 1e6 / len(test)
        r = sum(len(set(g) & w) for g, w in zip(got, want)) / (a.k * len(test))
        idx.set_num_threads(a.threads)
        t = time.perf_counter()
        idx.knn_query(test, k=a.k)
        qps = len(test) / (time.perf_counter() - t)
        print(f"{ef:>9}  {r:>9.4}  {us:>9.1f}  {qps:>8.0f}")


if __name__ == "__main__":
    main()
