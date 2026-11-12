//! Embedding models run inside nucleus (PLAN.md §M14.22 E3): BERT-family
//! encoders on nucleus's own kernels, read from the files their
//! publishers give — `tokenizer.json`, `config.json`, sentence-
//! transformers' pooling configuration and safetensors.

pub mod encoder;
pub mod safetensors;
pub mod tokenizer;

use encoder::{Config, Encoder, Pooling};
use safetensors::SafeTensors;
use tokenizer::Tokenizer;

/// A model: its tokenizer and its encoder.
pub struct LocalModel {
    tokenizer: Tokenizer,
    encoder: Encoder,
}

/// A model's files, as they come from its repository.
pub struct Files<'a> {
    /// `tokenizer.json`.
    pub tokenizer: &'a [u8],
    /// `config.json`.
    pub config: &'a [u8],
    /// `1_Pooling/config.json`, if the model has one (else `pooling`).
    pub pooling: Option<&'a [u8]>,
    /// `model.safetensors`.
    pub weights: &'a [u8],
}

impl LocalModel {
    /// Reads a model from its files; `pooling` overrides the files'.
    pub fn new(files: &Files, pooling: Option<Pooling>) -> Result<LocalModel, String> {
        let pooling = match (pooling, files.pooling) {
            (Some(p), _) => p,
            (None, Some(json)) => Pooling::from_json(json)?,
            (None, None) => {
                return Err(
                    "the model has no pooling configuration (1_Pooling/config.json): name one"
                        .into(),
                );
            }
        };
        let config = Config::from_json(files.config)?;
        let weights = SafeTensors::new(files.weights)?;
        Ok(LocalModel {
            tokenizer: Tokenizer::from_json(files.tokenizer)?,
            encoder: Encoder::new(config, pooling, &weights)?,
        })
    }

    /// The vectors' dimensions.
    pub fn dimensions(&self) -> usize {
        self.encoder.config().hidden
    }

    /// The tokenizer.
    pub fn tokenizer(&self) -> &Tokenizer {
        &self.tokenizer
    }

    /// The encoder.
    pub fn encoder(&self) -> &Encoder {
        &self.encoder
    }

    /// The token ids of `text`, cut to the model's length.
    pub fn tokens(&self, text: &str) -> Vec<u32> {
        self.tokenizer
            .encode(text, self.encoder.config().max_tokens())
    }

    /// The vectors of `texts`, of length 1, on up to `threads` threads.
    pub fn embed(&self, texts: &[&str], threads: usize) -> Result<Vec<Vec<f32>>, String> {
        let ids: Vec<Vec<u32>> = texts.iter().map(|t| self.tokens(t)).collect();
        self.encoder.encode(&ids, true, threads)
    }
}
