#!/usr/bin/env python3
"""nucleus's encoder against the models' publishers' own f32 ONNX
exports run by ONNX Runtime (PLAN.md §M14.22 E3b): the cosine of each
text's two vectors.

    bench/embed/encoder.py [--models target/models] [--data target/text-data]
                           [--only bge-small-en-v1.5,multilingual-e5-small]
                           [--texts 2000] [--threads 1]

The texts: a sample of BEIR's corpora and queries (the longest cut to
the model's 512 tokens) and of the Snowball word lists' languages.  Both
sides get the ids of Hugging Face's `tokenizers` (`tokens.py` checks
nucleus's own are the same) one text at a time, so ONNX Runtime pads
nothing; the reference pools as sentence-transformers does (the first
token's vector, or the mean) and scales to length 1.  Needs
`pip install onnxruntime tokenizers numpy`; run it under a memory cap.
"""

import argparse
import json
import os
import random
import subprocess
import sys
import time

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tokens  # noqa: E402


def sample(data, n, seed=7):
    texts = list(tokens.corpus_texts(data))
    r = random.Random(seed)
    r.shuffle(texts)
    # The longest few, so truncation is exercised, and the rest at random.
    longest = sorted(texts, key=len)[-20:]
    return longest + texts[: n - len(longest)]


def reference(model_dir, texts):
    import onnxruntime as ort
    from tokenizers import Tokenizer
    tok = Tokenizer.from_file(os.path.join(model_dir, "tokenizer.json"))
    tok.enable_truncation(512)
    pool = json.load(open(os.path.join(model_dir, "1_Pooling/config.json")))
    opts = ort.SessionOptions()
    opts.intra_op_num_threads = 1
    s = ort.InferenceSession(os.path.join(model_dir, "onnx/model.onnx"), opts,
                             providers=["CPUExecutionProvider"])
    names = [i.name for i in s.get_inputs()]
    out = []
    t = time.time()
    for x in texts:
        ids = np.array([tok.encode(x).ids], dtype=np.int64)
        feed = {"input_ids": ids, "attention_mask": np.ones_like(ids)}
        if "token_type_ids" in names:
            feed["token_type_ids"] = np.zeros_like(ids)
        h = s.run(None, feed)[0][0]
        v = h[0] if pool["pooling_mode_cls_token"] else h.mean(axis=0)
        out.append(v / max(np.linalg.norm(v), 1e-12))
    return np.array(out, dtype=np.float32), time.time() - t


def ours(binary, model_dir, texts, threads):
    payload = "".join(json.dumps(t) + "\n" for t in texts).encode()
    r = subprocess.run([binary, model_dir, str(threads)], input=payload,
                       capture_output=True, check=True)
    sys.stderr.write("  nucleus: " + r.stderr.decode())
    return np.frombuffer(r.stdout, dtype="<f4").reshape(len(texts), -1)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--models", default="target/models")
    p.add_argument("--data", default="target/text-data")
    p.add_argument("--only", default="bge-small-en-v1.5,multilingual-e5-small")
    p.add_argument("--texts", type=int, default=2000)
    p.add_argument("--threads", type=int, default=1)
    a = p.parse_args()
    subprocess.run(["cargo", "build", "-q", "--release", "-p", "nucleus-local-models",
                    "--example", "embed"], check=True)
    texts = sample(a.data, a.texts)
    worst_all = 1.0
    for name in a.only.split(","):
        d = os.path.join(a.models, name)
        want, t_ref = reference(d, texts)
        got = ours("target/release/examples/embed", d, texts, a.threads)
        cos = np.sum(want * got, axis=1) / np.linalg.norm(got, axis=1)
        worst = float(cos.min())
        worst_all = min(worst_all, worst)
        print(f"{name:24} {len(texts):6} texts  cosine min {worst:.7f}  mean {cos.mean():.8f}"
              f"  (ONNX Runtime one text a call, one thread: {t_ref:.1f} s)")
        for i in np.argsort(cos)[:3]:
            print(f"    {cos[i]:.7f} {texts[i][:70]!r}")
    print("ok" if worst_all >= 0.9999 else "BELOW 0.9999")
    sys.exit(0 if worst_all >= 0.9999 else 1)


if __name__ == "__main__":
    main()
