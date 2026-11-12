//! Embedding models run inside nucleus (PLAN.md §M14.22 E3): BERT-family
//! encoders on nucleus's own kernels, read from the files their
//! publishers give — `tokenizer.json`, `config.json`, safetensors.

pub mod tokenizer;
