#!/usr/bin/env python3
"""The public ANN datasets for the HNSW prototypes (PLAN.md §M14.12 V4),
as ann-benchmarks publishes them, converted once to raw files the Rust
side reads without an HDF5 library.

    bench/ann/data.py [--out target/ann-data] [sift-128-euclidean]
                      [glove-100-angular]

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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="target/ann-data")
    ap.add_argument("names", nargs="*", default=DEFAULT)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    for name in a.names:
        convert(name, a.out)


if __name__ == "__main__":
    main()
