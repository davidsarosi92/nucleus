#!/usr/bin/env python3
"""The public ANN datasets for the HNSW prototypes (PLAN.md §M14.12 V4),
as ann-benchmarks publishes them, converted once to raw files the Rust
side reads without an HDF5 library.

    bench/ann/data.py [--out target/ann-data] [sift-128-euclidean]
                      [glove-100-angular] [dbpedia-openai3-1536-100k]

dbpedia-openai3-1536-100k is not ann-benchmarks': 100,000 DBpedia
entities embedded by OpenAI's text-embedding-3-large (1536 dimensions,
unit length), as Qdrant publishes them on Hugging Face for its
quantization benchmarks — 99,000 rows indexed and the last 1,000 the
queries, the ground truth found here (needs pyarrow as well).

Each dataset lands in <out>/<name>/:

    train.f32      the vectors indexed, little-endian f32, row after row
    test.f32       the queries, the same way
    neighbors.i32  each query's 100 true nearest train rows, little-endian
                   i32 (ann-benchmarks' ground truth)
    meta.txt       dim=…  train=…  test=…  metric=l2|cosine

Angular datasets are stored normalized (so the cosine distance is one
minus the inner product, and the ground truth's order is kept).  Needs
h5py and numpy (a virtual environment is enough:
`python3 -m venv target/ann-data/.venv`, then pip install them there).
"""

import argparse
import os
import shutil
import subprocess
import sys
import urllib.request

import h5py
import numpy as np

URL = "http://ann-benchmarks.com/{}.hdf5"
DEFAULT = ["sift-128-euclidean", "glove-100-angular"]


def fetch(name, out):
    path = os.path.join(out, name + ".hdf5")
    if not os.path.exists(path):
        print(f"downloading {URL.format(name)}", file=sys.stderr)
        # The site refuses urllib's own User-Agent.
        req = urllib.request.Request(URL.format(name), headers={"User-Agent": "nucleus-bench"})
        with urllib.request.urlopen(req) as r, open(path + ".part", "wb") as f:
            shutil.copyfileobj(r, f, 1 << 20)
        os.rename(path + ".part", path)
    return path


def convert(name, out):
    with h5py.File(fetch(name, out), "r") as f:
        metric = f.attrs.get("distance", "")
        metric = metric.decode() if isinstance(metric, bytes) else metric
        train = np.asarray(f["train"], dtype="<f4")
        test = np.asarray(f["test"], dtype="<f4")
        neighbors = np.asarray(f["neighbors"], dtype="<i4")
    if metric == "angular":
        for a in (train, test):
            norms = np.linalg.norm(a, axis=1, keepdims=True)
            norms[norms == 0] = 1
            a /= norms
        kind = "cosine"
    elif metric == "euclidean":
        kind = "l2"
    else:
        sys.exit(f"{name}: unknown distance {metric!r}")
    d = os.path.join(out, name)
    os.makedirs(d, exist_ok=True)
    train.tofile(os.path.join(d, "train.f32"))
    test.tofile(os.path.join(d, "test.f32"))
    np.ascontiguousarray(neighbors[:, :100]).tofile(os.path.join(d, "neighbors.i32"))
    with open(os.path.join(d, "meta.txt"), "w") as m:
        m.write(f"dim={train.shape[1]} train={train.shape[0]} test={test.shape[0]} metric={kind}\n")
    print(f"{name}: {train.shape[0]} × {train.shape[1]}, {test.shape[0]} queries, {kind}")


HF = ("https://huggingface.co/api/datasets/Qdrant/"
      "dbpedia-entities-openai3-text-embedding-3-large-1536-100K/parquet/default/train/{}.parquet")


def dbpedia(name, out):
    import pyarrow.parquet as pq

    parts = []
    for i in range(3):
        path = os.path.join(out, f"{name}-{i}.parquet")
        if not os.path.exists(path):
            print(f"downloading {HF.format(i)}", file=sys.stderr)
            # curl: Python's own certificates are not always installed.
            subprocess.run(["curl", "-sfL", "-A", "nucleus-bench", "-o", path + ".part", HF.format(i)], check=True)
            os.rename(path + ".part", path)
        t = pq.read_table(path)
        col = [c for c in t.column_names if "embedding" in c][0]
        parts.append(np.asarray(t.column(col).to_pylist(), dtype="<f4"))
    data = np.concatenate(parts)
    data /= np.linalg.norm(data, axis=1, keepdims=True)
    train, test = data[:-1000], data[-1000:]
    neighbors = []
    for b in range(0, len(test), 100):
        sims = test[b:b + 100] @ train.T
        top = np.argpartition(-sims, 100, axis=1)[:, :100]
        for q, c in zip(sims, top):
            neighbors.append(c[np.lexsort((c, -q[c]))])
    neighbors = np.asarray(neighbors, dtype="<i4")
    d = os.path.join(out, name)
    os.makedirs(d, exist_ok=True)
    train.tofile(os.path.join(d, "train.f32"))
    test.tofile(os.path.join(d, "test.f32"))
    neighbors.tofile(os.path.join(d, "neighbors.i32"))
    with open(os.path.join(d, "meta.txt"), "w") as m:
        m.write(f"dim={train.shape[1]} train={train.shape[0]} test={test.shape[0]} metric=cosine\n")
    print(f"{name}: {train.shape[0]} × {train.shape[1]}, {test.shape[0]} queries, cosine")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="target/ann-data")
    ap.add_argument("names", nargs="*", default=DEFAULT)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    for name in a.names:
        if name == "dbpedia-openai3-1536-100k":
            dbpedia(name, a.out)
        else:
            convert(name, a.out)


if __name__ == "__main__":
    main()
