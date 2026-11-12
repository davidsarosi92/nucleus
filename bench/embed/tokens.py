#!/usr/bin/env python3
"""nucleus's tokenizers against Hugging Face's `tokenizers` (PLAN.md
§M14.22 E3a): the ids of every text, compared exactly.

    bench/embed/tokens.py [--models target/models] [--data target/text-data]
                              [--only bge-small-en-v1.5,multilingual-e5-small]

The texts: BEIR's corpora and queries (scifact, nfcorpus, arguana, fiqa:
titles and bodies), the Snowball stemmers' word lists of a dozen
languages, every Unicode code point alone and between letters, and
random mixtures of letters, marks, emoji, spaces, controls and the
tokenizers' special tokens.  The tokenizer files come from the models'
Hugging Face repositories (`tokenizer.json`); nucleus's side is
`modules/local-models/examples/tokenize.rs`, built here.  Needs
`pip install tokenizers`.
"""

import argparse
import json
import os
import random
import subprocess
import sys
import time


def corpus_texts(data):
    for name in ["scifact", "nfcorpus", "arguana", "fiqa"]:
        d = os.path.join(data, f"beir-{name}", name)
        for f in ["corpus.jsonl", "queries.jsonl"]:
            p = os.path.join(d, f)
            if not os.path.exists(p):
                print(f"  (no {p})", file=sys.stderr)
                continue
            for line in open(p, encoding="utf-8"):
                r = json.loads(line)
                t = (r.get("title", "") + " " + r["text"]).strip()
                yield t
    snow = os.path.join(data, "snowball-data")
    if os.path.isdir(snow):
        for lang in sorted(os.listdir(snow)):
            p = os.path.join(snow, lang, "voc.txt")
            if os.path.exists(p):
                words = open(p, encoding="utf-8").read().split()
                # A line of thirty words, as a text.
                for i in range(0, len(words), 30):
                    yield " ".join(words[i:i + 30])


def code_point_texts():
    for c in range(0x110000):
        if 0xD800 <= c <= 0xDFFF:
            continue
        ch = chr(c)
        yield ch
        yield f"a{ch}b {ch}c"


POOL = (
    "abcxyzABCXYZ0123456789 ,.;:!?'\"-()[]{}#$%&*+/<=>@\\^_`|~"
    "áéíóöőúüűÁÉÍÓÖŐÚÜŰçñßøåæœÆŒ"
    "̧̨̣́̈̋̀̂̃॑ि्"
    "αβγΣσςΩЖжшщДЯ中文字日本語한국어ไทยहिन्दीকখগ"
    "​‌‍­﻿ 　 \t\n\r\x00\x07\x7f�"
    "😀👩🏽‍🔬🇭🇺🏳️‍🌈👍🏻✨①②ⅷﬁﬀ½ｈｅｌｌｏ　"
)
SPECIAL = ["[CLS]", "[SEP]", "[UNK]", "[MASK]", "[PAD]", "<s>", "</s>", "<unk>", "<mask>", "<pad>"]


def random_texts(n, seed=2026):
    r = random.Random(seed)
    for _ in range(n):
        parts = []
        for _ in range(r.randint(1, 40)):
            x = r.random()
            if x < 0.05:
                parts.append(r.choice(SPECIAL))
            elif x < 0.15:
                parts.append(" " * r.randint(1, 4))
            elif x < 0.2:
                c = r.randint(0x80, 0x2FFFF)
                parts.append(chr(c) if not 0xD800 <= c <= 0xDFFF else "x")
            else:
                parts.append(r.choice(POOL))
        yield "".join(parts)


def ours(binary, tokenizer, texts):
    payload = "".join(json.dumps(t) + "\n" for t in texts).encode()
    out = subprocess.run([binary, tokenizer], input=payload, capture_output=True, check=True)
    return [json.loads(line) for line in out.stdout.decode().splitlines()]


def compare(name, tokenizer, binary, groups):
    from tokenizers import Tokenizer
    hf = Tokenizer.from_file(tokenizer)
    total_bad = 0
    for what, texts in groups:
        texts = list(texts)
        t = time.time()
        want = [e.ids for e in hf.encode_batch(texts, add_special_tokens=True)]
        t_hf = time.time() - t
        t = time.time()
        got = ours(binary, tokenizer, texts)
        t_us = time.time() - t
        bad = [i for i, (a, b) in enumerate(zip(want, got)) if a != b]
        if len(got) != len(want):
            bad.append(-1)
        total_bad += len(bad)
        print(f"{name:24} {what:12} {len(texts):9,} texts  {len(bad):6,} differ"
              f"   (tokenizers {t_hf:.1f} s, nucleus {t_us:.1f} s with the process)")
        for i in bad[:8]:
            if i < 0:
                print("    a different number of answers")
                continue
            print(f"    {texts[i][:80]!r}\n      want {hf.decode(want[i], skip_special_tokens=False)[:80]!r} {want[i][:24]}"
                  f"\n      got  {hf.decode(got[i], skip_special_tokens=False)[:80]!r} {got[i][:24]}")
    return total_bad


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--models", default="target/models")
    p.add_argument("--data", default="target/text-data")
    p.add_argument("--only", default="bge-small-en-v1.5,multilingual-e5-small")
    p.add_argument("--random", type=int, default=200_000)
    a = p.parse_args()
    subprocess.run(["cargo", "build", "-q", "--release", "-p", "nucleus-local-models",
                    "--example", "tokenize"], check=True)
    binary = "target/release/examples/tokenize"
    corpus = list(corpus_texts(a.data))
    points = list(code_point_texts())
    rand = list(random_texts(a.random))
    bad = 0
    for name in a.only.split(","):
        tok = os.path.join(a.models, name, "tokenizer.json")
        bad += compare(name, tok, binary,
                       [("corpora", corpus), ("code points", points), ("random", rand)])
    print("all the same" if bad == 0 else f"{bad:,} texts differ")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
