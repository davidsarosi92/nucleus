#!/usr/bin/env python3
"""nucleus's local models against fastembed (ONNX Runtime) on the same
texts (PLAN.md §M14.22 E3c): texts and tokens a second, on one core and
on all of them.

    bench/embed/speed.py [--models target/models] [--data target/text-data]
                         [--texts 2000] [--threads 1,10]

The texts are `encoder.py`'s sample of BEIR and the Snowball word
lists.  fastembed runs the publishers' own f32 ONNX exports (added as
custom models, so both sides compute the same thing) in batches of 32,
and — for bge — also its default, Qdrant's optimized export
(`bge-small-en-v1.5-onnx-Q`).  nucleus runs
`modules/local-models/examples/embed.rs` (its own clock: tokenizing and
the encoder, not loading).  Tokens are counted by the models'
tokenizers, cut to 512.  Run it under a memory cap; fastembed's
`parallel` is not used (it copies the model into a process per core).
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import encoder  # noqa: E402

MODELS = {
    "bge-small-en-v1.5": ("BAAI/bge-small-en-v1.5", "cls"),
    "multilingual-e5-small": ("intfloat/multilingual-e5-small", "mean"),
}


def fastembed_run(name, texts, threads, custom):
    from fastembed import TextEmbedding
    from fastembed.common.model_description import ModelSource, PoolingType
    repo, pooling = MODELS[name]
    model = repo
    if custom:
        model = f"nucleus-bench/{name}-f32"
        try:
            TextEmbedding.add_custom_model(
                model=model,
                pooling=PoolingType.CLS if pooling == "cls" else PoolingType.MEAN,
                normalization=True,
                sources=ModelSource(hf=repo),
                dim=384,
                model_file="onnx/model.onnx",
            )
        except ValueError:
            pass  # added before in this process
    m = TextEmbedding(model, threads=threads)
    list(m.embed(texts[:64], batch_size=32))  # warm
    t = time.time()
    n = sum(1 for _ in m.embed(texts, batch_size=32))
    assert n == len(texts)
    return time.time() - t


def ours_run(model_dir, texts, threads):
    payload = "".join(json.dumps(t) + "\n" for t in texts).encode()
    r = subprocess.run(["target/release/examples/embed", model_dir, str(threads)],
                       input=payload, capture_output=True, check=True)
    m = re.search(r"tokens in ([0-9.]+) s", r.stderr.decode())
    return float(m.group(1))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--models", default="target/models")
    p.add_argument("--data", default="target/text-data")
    p.add_argument("--texts", type=int, default=2000)
    p.add_argument("--threads", default=f"1,{os.cpu_count()}")
    p.add_argument("--only", default=",".join(MODELS))
    a = p.parse_args()
    subprocess.run(["cargo", "build", "-q", "--release", "-p", "nucleus-local-models",
                    "--example", "embed"], check=True)
    texts = encoder.sample(a.data, a.texts)
    from tokenizers import Tokenizer
    print(f"{'model':24} {'runtime':34} {'threads':>7} {'texts/s':>9} {'tokens/s':>10}")
    for name in a.only.split(","):
        d = os.path.join(a.models, name)
        tok = Tokenizer.from_file(os.path.join(d, "tokenizer.json"))
        tok.enable_truncation(512)
        tokens = sum(len(e.ids) for e in tok.encode_batch(texts))
        for threads in [int(t) for t in a.threads.split(",")]:
            runs = [("nucleus", lambda: ours_run(d, texts, threads)),
                    ("fastembed, publisher's f32 ONNX", lambda: fastembed_run(name, texts, threads, True))]
            if name == "bge-small-en-v1.5":
                runs.append(("fastembed, its default (Qdrant's)",
                             lambda: fastembed_run(name, texts, threads, False)))
            for what, run in runs:
                s = run()
                print(f"{name:24} {what:34} {threads:7} {len(texts) / s:9.1f} {tokens / s:10.0f}",
                      flush=True)


if __name__ == "__main__":
    main()
