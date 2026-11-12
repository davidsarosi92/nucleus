# Changelog

## Unreleased

- First version (nucleus PLAN.md §M14.22 E2b): the model providers
  `openai` (any OpenAI-compatible `/embeddings` endpoint), `cohere` (v2
  `embed`, `input_type` by query or document) and `voyage` (`input_type`
  likewise); known models' dimensions, batches by each API's limit, one
  retry of a throttled or failed call, errors that never carry the key.
