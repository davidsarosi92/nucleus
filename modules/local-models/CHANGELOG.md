# Changelog

## Unreleased

- Token counts for chunks (nucleus PLAN.md §M14.22 E6): `LocalModel::count`,
  `Tokenizer::count`, and the provider's `count_tokens`, so
  `chunks(…, 'tokens', n, 0, 'model')` sizes chunks in the model's tokens.
- The `local` model provider (nucleus PLAN.md §M14.22 E3c): `CREATE MODEL
  … USING local WITH (huggingface = 'org/name' | path = '/dir')`, the
  files fetched once (redirects followed, the hub's SHA-256 checked) and
  kept in the database by the core; a loaded model kept for the process.
- The encoder (nucleus PLAN.md §M14.22 E3b): BERT, RoBERTa and
  XLM-RoBERTa from `config.json` and safetensors, CLS or mean pooling,
  packed batches without padding, the same vector on any number of
  threads; checked against ONNX Runtime.
- The tokenizers (nucleus PLAN.md §M14.22 E3a): WordPiece behind BERT's
  normalizer and pre-tokenizer, Unigram behind SentencePiece's
  precompiled normalizer and Metaspace, read from `tokenizer.json` and
  checked against Hugging Face's `tokenizers` on corpora and on every
  code point.
