# nucleus-embeddings

Embedding model providers for [nucleus](https://github.com/davidsarosi92/nucleus)
(PLAN.md §M14.22): what `CREATE MODEL … USING openai | cohere | voyage`
calls, so `embed(text, model)` turns text into a `vector` inside the
database.  A module, so nucleus's core carries no JSON or TLS library;
the `nucleus` command-line tool includes it.

```sql
CREATE SECRET openai_key VALUE 'sk-…';           -- sealed, never shown
CREATE MODEL small USING openai WITH (model = 'text-embedding-3-small', secret = openai_key);
SELECT id FROM docs ORDER BY v <=> embed('how do I reset a password', 'small', 'query') LIMIT 5;
```

| provider | API | options |
|---|---|---|
| `openai` | `POST {url}/embeddings` — OpenAI, and any server that speaks it: Ollama (`url = 'http://localhost:11434/v1'`), vLLM, LM Studio, text-embeddings-inference, Mistral, Together | `model` (required), `url` (default `https://api.openai.com/v1`), `dimensions` (required unless the model is `text-embedding-3-small` / `-large` / `ada-002`; sent to OpenAI's v3 models to shorten their vectors), `batch` (texts a call, default 512, at most 2,048) |
| `cohere` | `POST {url}/v2/embed`, `input_type` `search_query` or `search_document` | `model` (`embed-v4.0`, `embed-english-v3.0`, `embed-multilingual-v3.0` and their `-light` kinds known), `url` (default `https://api.cohere.com`), `dimensions` (`output_dimension`), `batch` (at most 96) |
| `voyage` | `POST {url}/v1/embeddings`, `input_type` `query` or `document` | `model` (`voyage-3.5`, `-3.5-lite`, `-3-large`, `-3`, `-3-lite`, `-code-3`, `-multilingual-2` known), `url` (default `https://api.voyageai.com`), `dimensions` (`output_dimension`), `batch` (default 128, at most 1,000) |

Every provider also takes nucleus's own options: `secret` (the key,
sent as `Authorization: Bearer …`; naming one needs `USAGE` on it),
`query_prefix` and `document_prefix`, and the limits your account has:
`batch_tokens`, `requests_per_minute`, `tokens_per_minute` and
`parallel` (calls at once) — nucleus packs batches and paces calls by
them, and charges each statement the tokens the API reports (`usage`,
Cohere's billed units) in `EXPLAIN ANALYZE` and `nc_catalog.statements`.

- A throttled call (429) or a server's failure (5xx), or a network
  error, is tried once more — after `Retry-After` (at most 10 s) or half
  a second — and then fails with `58030` (worth trying again later); a
  refusal (a bad key, an unknown model) fails with `55000`.  No error
  carries the key.
- Plain `http://` only to a loopback address (a local Ollama), so a key
  never crosses a network in the clear.
- A `url` of a model made by a role other than an administrator must be
  under the database's `model_urls` (`ALTER DATABASE nucleus SET
  model_urls = 'http://localhost:11434/v1'`), checked when it is made and
  at every call; a provider's own address needs no listing.
- One connection a call (no reuse yet): batches carry the cost.

The protocols are checked against servers on loopback that answer as
each API does (`tests/providers.rs`).
