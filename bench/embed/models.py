#!/usr/bin/env python3
"""The embedding models §M14.22 E3 is checked with, fetched once from
their publishers' Hugging Face repositories into target/models.

    bench/embed/models.py [--out target/models] [name ...]

Each model gets `tokenizer.json`, `config.json`, `1_Pooling/config.json`
(sentence-transformers' pooling), `model.safetensors` (the weights
nucleus reads) and `onnx/model.onnx` (the publishers' f32 export, the
reference ONNX Runtime runs).  A file already there is kept.
"""

import argparse
import os
import sys
import urllib.request

MODELS = {
    "bge-small-en-v1.5": "BAAI/bge-small-en-v1.5",
    "multilingual-e5-small": "intfloat/multilingual-e5-small",
}
FILES = ["tokenizer.json", "config.json", "1_Pooling/config.json",
         "model.safetensors", "onnx/model.onnx"]


def fetch(repo, path, dest):
    if os.path.exists(dest):
        return
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    url = f"https://huggingface.co/{repo}/resolve/main/{path}"
    print(f"  {url}", file=sys.stderr)
    tmp = dest + ".part"
    with urllib.request.urlopen(url) as r, open(tmp, "wb") as f:
        while chunk := r.read(1 << 20):
            f.write(chunk)
    os.replace(tmp, dest)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", default="target/models")
    p.add_argument("names", nargs="*", default=list(MODELS))
    a = p.parse_args()
    for name in a.names:
        for f in FILES:
            fetch(MODELS[name], f, os.path.join(a.out, name, f))
    print("ok")


if __name__ == "__main__":
    main()
