# Changelog

## Unreleased

- The providers name their `url` (`ModelProvider::urls`), so nucleus
  holds a model not made by an administrator to the database's
  `model_urls`.

- The tokens each answer reports (OpenAI's and Voyage's
  `usage.total_tokens`, Cohere's `meta.billed_units.input_tokens`) go to
  the statement's costs (`Request::report_tokens`, §M14.22 E8a).

- A call that failed twice for a `Retry-After` carries it
  (`models::retry_hint`), so the embedding columns' queue waits as long
  as the provider asked (§M14.22 E4b).
- First version (nucleus PLAN.md §M14.22 E2b): the model providers
  `openai` (any OpenAI-compatible `/embeddings` endpoint), `cohere` (v2
  `embed`, `input_type` by query or document) and `voyage` (`input_type`
  likewise); known models' dimensions, batches by each API's limit, one
  retry of a throttled or failed call, errors that never carry the key.
