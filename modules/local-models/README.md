# nucleus-local-models

Embedding models run inside [nucleus](https://github.com/davidsarosi92/nucleus)
(PLAN.md §M14.22 E3), on nucleus's own kernels: no ONNX Runtime, no
C++ library and no network.  BERT-family encoders, read from the files
their publishers give on Hugging Face — `tokenizer.json`, `config.json`
and `model.safetensors`.

Built so far: **the tokenizers** (E3a), each exactly as Hugging Face's
`tokenizers` runs it, so a model sees the ids it was trained and
published with, and **the encoder** (E3b).

| family | models | pipeline |
|---|---|---|
| WordPiece | bge, MiniLM, GTE, BERT | BERT's normalizer (controls removed, CJK ideographs spaced, accents stripped, lower case), BERT's pre-tokenizer (whitespace, punctuation alone), greedy longest pieces with `##` |
| Unigram | XLM-RoBERTa: multilingual-e5 | SentencePiece's precompiled character map by grapheme cluster, runs of spaces as one, Metaspace (`▁`), the best segmentation by Viterbi |

Special tokens written in a text are those tokens (as `tokenizers`
matches them); a text is cut to the model's length with its closing
token kept.  What else a `tokenizer.json` may hold (BPE, other
normalizers, added tokens matched in other ways) is refused by name,
not approximated.

- BERT's character classes are **Unicode 8's**: `tokenizers` takes them
  from a crate whose tables are that version's, and with Unicode 16's
  560 code points would tokenize differently
  (`scripts/gen-tokenizer-tables.py`).  The grapheme clusters are
  Unicode 16's (UAX #29, checked by its `GraphemeBreakTest.txt`).
- Checked against `tokenizers` 0.23 on bge-small-en-v1.5 and
  multilingual-e5-small: BEIR's corpora and queries and twelve
  languages' word lists (103,821 texts), every code point alone and
  between letters (2,224,128) and 200,000 random mixtures — the same
  ids for all (`bench/embed/tokens.py`).
- On one core, the 73 MB of those corpora in 1.9 s (WordPiece) and 7.6 s
  (Unigram, over a byte trie of its 250,002 pieces).

`tests/tokenizers.rs` runs on the models' files when
`bench/embed/models.py` has fetched them into `target/models` (or
`$NUCLEUS_MODELS` names a folder), and says it skipped otherwise.

## The encoder

BERT, RoBERTa and XLM-RoBERTa encoders (`config.json`: GELU, absolute
positions) from `model.safetensors` (F32, F16 or BF16), pooled as
sentence-transformers' `1_Pooling/config.json` says (the first token, or
the mean) and scaled to length 1:

```rust
let model = LocalModel::new(&Files { tokenizer, config, pooling, weights }, None)?;
let vectors = model.embed(&["query: how old is the Danube?"], threads)?;
```

- On nucleus's kernels (`ext::kernels`): an f32 matrix product in
  BLIS's structure whose every output is one chain of fused
  multiply-adds, GELU, softmax, layer normalization — no ONNX Runtime,
  no BLAS.
- A batch's texts are packed end to end, not padded; threads take whole
  texts.  A text's vector is the same bits alone, in any batch and on
  any number of threads.
- Against the publishers' own f32 ONNX exports in ONNX Runtime
  (`bench/embed/encoder.py`): cosine ≥ 0.9999998 on bge-small-en-v1.5
  and multilingual-e5-small.  One M2 Pro core: about 1,550 tokens a
  second; 8 threads about 9,000.
