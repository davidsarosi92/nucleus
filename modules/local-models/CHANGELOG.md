# Changelog

## Unreleased

- The encoder (nucleus PLAN.md §M14.22 E3b): BERT, RoBERTa and
  XLM-RoBERTa from `config.json` and safetensors, CLS or mean pooling,
  packed batches without padding, the same vector on any number of
  threads; checked against ONNX Runtime.
- The tokenizers (nucleus PLAN.md §M14.22 E3a): WordPiece behind BERT's
  normalizer and pre-tokenizer, Unigram behind SentencePiece's
  precompiled normalizer and Metaspace, read from `tokenizer.json` and
  checked against Hugging Face's `tokenizers` on corpora and on every
  code point.
