# Instrumenting your LLM app for evald

evald is an **OTLP/OpenTelemetry receiver**. Anything that speaks OTLP can send it
traces — point your app's OTel exporter at evald's endpoint and your spans, token
counts, costs, and scores show up in the console (trace list → span tree → scores),
the SQL surface, and the cost view.

- **Endpoint:** `POST /v1/traces` on `http://<host>:4318` (OTLP/HTTP).
- **Wire formats:** OTLP **protobuf** (gzip-aware) *and* **OTLP-JSON** — same route.
- **No lock-in:** it's stock OpenTelemetry. The same instrumentation can fan out to
  evald and to any other OTLP backend at the same time.

```text
your LLM app ──(OTel SDK + OTLP/HTTP exporter)──▶  evald  :4318 /v1/traces
                                                     │ normalize → durable store
                                             console ▲  SQL  ▲  scores ▲  cost ▲
```

There are two ways in. Prefer **(A)** — it fills in the right attributes for you.

---

## A. Auto-instrumentation (recommended)

Drop-in libraries trace your LLM/agent calls with zero manual attributes — real
model, real token usage from the provider response, real latency. evald understands
both the **OpenInference** and the **OpenTelemetry GenAI** semantic conventions, so
either family works.

### Python — Anthropic via OpenInference (verified)

```bash
pip install \
  opentelemetry-sdk \
  opentelemetry-exporter-otlp-proto-http \
  openinference-instrumentation-anthropic \
  "anthropic>=0.84"
```

```python
from opentelemetry import trace
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from openinference.instrumentation.anthropic import AnthropicInstrumentor
import anthropic

# 1) send spans to evald over OTLP/HTTP
provider = TracerProvider(resource=Resource.create({"service.name": "my-agent"}))
provider.add_span_processor(
    BatchSpanProcessor(OTLPSpanExporter(endpoint="http://localhost:4318/v1/traces"))
)
trace.set_tracer_provider(provider)

# 2) auto-trace every Anthropic call (real gen_ai.* attrs + real token usage)
AnthropicInstrumentor().instrument(tracer_provider=provider)

client = anthropic.Anthropic()  # ANTHROPIC_API_KEY from env
client.messages.create(
    model="claude-haiku-4-5-20251001",
    max_tokens=80,
    messages=[{"role": "user", "content": "what is distributed tracing?"}],
)

provider.force_flush()   # flush before a short-lived script exits
```

Open `http://localhost:4318/` → **Traces** → the `messages.create` LLM span shows
`gen_ai.request.model`, `provider`, and the real prompt/completion/total tokens.

Swap the instrumentor for your stack — same three steps:

| Provider / framework | Package (OpenInference) |
|---|---|
| Anthropic | `openinference-instrumentation-anthropic` |
| OpenAI | `openinference-instrumentation-openai` |
| LangChain | `openinference-instrumentation-langchain` |
| LlamaIndex | `openinference-instrumentation-llama-index` |
| CrewAI / Autogen / DSPy | `openinference-instrumentation-<name>` |

> **OpenLLMetry** (`traceloop-sdk`) works too — it emits OTel GenAI attributes evald
> also reads. `Traceloop.init(api_endpoint="http://localhost:4318")`.

### Point an already-instrumented app at evald (env vars only)

If your service already exports OTLP, you don't touch code — just redirect it:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf   # evald HTTP is :4318
export OTEL_SERVICE_NAME=my-agent
```

### JavaScript / TypeScript

```bash
npm i @opentelemetry/sdk-node @opentelemetry/exporter-trace-otlp-http \
      @arizeai/openinference-instrumentation-openai
```

```ts
import { NodeSDK } from "@opentelemetry/sdk-node";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-http";
import { OpenAIInstrumentation } from "@arizeai/openinference-instrumentation-openai";

new NodeSDK({
  traceExporter: new OTLPTraceExporter({ url: "http://localhost:4318/v1/traces" }),
  instrumentations: [new OpenAIInstrumentation()],
}).start();
```

---

## B. Manual spans (any language, or custom frameworks)

No auto-instrumentor for your stack? Emit plain OTel spans and set the attributes
evald normalizes. Use an **LLM** span kind and the token keys below.

```python
with tracer.start_as_current_span("chat.completion") as span:
    span.set_attribute("openinference.span.kind", "LLM")   # marks it an LLM call
    span.set_attribute("gen_ai.system", "anthropic")
    span.set_attribute("gen_ai.request.model", "claude-opus-4-8")
    span.set_attribute("input.value", prompt)
    # ... make the call ...
    span.set_attribute("gen_ai.usage.input_tokens", resp.usage.input_tokens)
    span.set_attribute("gen_ai.usage.output_tokens", resp.usage.output_tokens)
    span.set_attribute("output.value", completion)
```

### Attributes evald reads

Both dialects are accepted; set **one** family per concern.

| Concept | OpenTelemetry GenAI | OpenInference |
|---|---|---|
| Span is an LLM/retriever/tool/chain | `gen_ai.operation.name` | `openinference.span.kind` = `LLM`/`RETRIEVER`/`TOOL`/`CHAIN`/`EMBEDDING` |
| Model | `gen_ai.request.model` | `llm.model_name` |
| Provider | `gen_ai.system` / `gen_ai.provider.name` | `llm.provider` / `llm.system` |
| Prompt tokens | `gen_ai.usage.input_tokens` (or `prompt_tokens`) | `llm.token_count.prompt` |
| Completion tokens | `gen_ai.usage.output_tokens` (or `completion_tokens`) | `llm.token_count.completion` |
| Total tokens | `gen_ai.usage.total_tokens` | `llm.token_count.total` |
| Cache read / write | `gen_ai.usage.cache_read.input_tokens` / `cache_write.input_tokens` (also read: `cache_creation.input_tokens`) | `llm.token_count.prompt_details.cache_read` / `cache_write` |
| Reasoning tokens | `gen_ai.usage.reasoning.output_tokens` (also read: `reasoning_tokens`) | `llm.token_count.completion_details.reasoning` |
| Cost (USD) | `gen_ai.usage.cost` | `llm.cost.total` |
| Input / output payload | `gen_ai.input.messages` / `gen_ai.output.messages` | `input.value` / `output.value` |
| Session / thread | `gen_ai.conversation.id` | `session.id` |
| User | — | `user.id` |
| Service | resource `service.name` | resource `service.name` |

- **Tokens** power the **Cost** view (grouped by model/provider/service/user) and
  the SQL `spans.total_tokens` column. Set them and cost attribution just works.
- **Cost**: set `gen_ai.usage.cost` (or `llm.cost.total`) if you price calls
  yourself and evald keeps your number. Otherwise evald prices the span from its model and
  token counts (see [Cost and token semantics](#cost-and-token-semantics)); a model it has no
  price for stays empty and is reported as `(no price)`.
- Large `input.value`/`output.value` payloads are offloaded to the blob store and
  shown in the console as an `evald-blob:<key>` reference (fetch via `/v1/blobs/<key>`).

---

## Cost and token semantics

`gen_ai.*` has no cost attribute and most instrumentors do not set `llm.cost.*`, so a span
normally arrives with a model and token counts but no cost. evald prices it from a model
price table:

- **Only when the span has no cost of its own.** A cost the instrumentor reported is never
  overwritten.
- **At ingest.** `cost_usd` is filled, and nothing else is stored per span: a reported cost
  always comes with the attribute it was read from (`llm.cost.total` or `gen_ai.usage.cost`,
  kept in `raw_attributes` like every attribute), so "a cost and no cost attribute" is what marks
  a derived one. The table that priced a block's spans is in the block's Parquet metadata
  (`evald.price_tables`, `docs/FORMAT.md`) and the running one on `GET /v1/meta`; the entry a
  model matched follows from the model id and the version. `evald.cost.basis` is added only when
  the token counts had to be read the non-default way (below). Nothing about the on-disk format
  changes.
- **A model that is not in the table is left without a cost**, not priced at `$0`;
  `evald cost` shows it as `(no price)`.
- **The table** is a copy of the community-maintained LiteLLM
  `model_prices_and_context_window.json`, compiled into the binary (text models only; the
  version is `<commit>@<date>`). `--price-table <file>` lays your file over it: it wins for
  every model it names, so you can add fine-tunes and correct prices. The file may be the
  upstream file or any subset of it. evald never fetches anything: refresh it yourself with
  `curl`/cron/config management. It is re-read when its modification time changes; a file that
  fails to parse at start-up stops `serve`, and a bad edit later keeps the previous table and
  logs a warning. A table older than 90 days logs a warning too.
- **Correcting a price after the fact** does not rewrite stored spans. Run
  `evald cost --price-table corrected.json`: the report is recomputed from the stored token
  counts under that table (spans whose cost you reported yourself keep it).

Model ids are matched exactly first, then after lowercasing, dropping a leading `provider/`
(and a Bedrock `us.` / `eu.` region prefix), dropping `@date`, and removing trailing
snapshot segments (`-20250929`, `-2024-08-06`, `-v1:0`, `-latest`). Words are never stripped
(`gpt-4o-mini` does not fall back to `gpt-4o`).

Cache reads and writes are priced at the model's cache rates (at the input rate when the
table has none), requests above a model's long-context threshold (for example 200k input
tokens) at its tier rates, and reasoning tokens exactly once, at the model's reasoning rate
when it has one and otherwise as output. Cache writes are priced at the 5-minute rate; a
one-hour write is not distinguished.

### Does the input count include cached tokens?

This is where cost goes wrong silently: sources disagree, and reading one as the other either
double-counts or under-counts the cache. evald does not keep a list of SDKs. It decides from
the numbers on the span, in this order:

1. **The span reports a total** (`llm.token_count.total` / `gen_ai.usage.total_tokens`): the
   difference `total - (prompt + completion)` says what was added on top. Nothing means the
   prompt includes the cache and the completion includes reasoning (the OpenTelemetry
   reading); a difference equal to the cache tokens means the cache is reported *on top of*
   the prompt; equal to the reasoning tokens, reasoning is on top of the completion.
2. **No total, and the cache tokens are larger than the whole prompt:** they cannot be part of
   it, so they are on top.
3. **Otherwise** the OpenTelemetry reading applies. This is the low-confidence case: a source
   that reports the cache on top of the prompt *and* no total. Have the instrumentation
   report a total, or set the cost yourself.

The reading used is stored (`evald.cost.basis`) whenever it is not the default, so the same
span is re-priced the same way later.

What the sources do, checked against their source code or documentation on 2026-09-22
(*verified* means the primary text was read; the commit or revision is given):

| Source | Input count includes cached tokens? | Reasoning in the completion? | Evidence | Confidence |
|---|---|---|---|---|
| OpenTelemetry GenAI conventions (`gen_ai.usage.*`) | **Yes**: `input_tokens` "SHOULD include all types of input tokens, including cached tokens"; `cache_read` / `cache_write` "SHOULD be included in" it | **Yes**: `reasoning.output_tokens` "SHOULD be included in" `output_tokens` | the `semantic-conventions-genai` repository, `docs/registry/attributes/gen-ai.md` notes 35, 36, 40, 42, at `cc07f72` | verified |
| Anthropic Messages API `usage` | **No**: `input_tokens` counts only tokens after the last cache breakpoint; total input = `cache_read_input_tokens + cache_creation_input_tokens + input_tokens` | n/a (thinking is billed as output) | Anthropic docs, *Prompt caching, tracking cache performance* | verified |
| OpenAI `usage` | **Yes**: `prompt_tokens_details.cached_tokens` is "Cached tokens present in the prompt" | **Yes**: `reasoning_tokens` is a detail of `completion_tokens` | `openai-python`, `src/openai/types/completion_usage.py` at `febbcdfe3` | verified |
| Gemini `usageMetadata` | **Yes**: `promptTokenCount` "includes the number of tokens in the cached content" | **No**: `thoughtsTokenCount` is reported separately from `candidatesTokenCount` | `googleapis`, `google/ai/generativelanguage/v1beta/generative_service.proto` at `e9ad9bb00` | verified |
| OpenInference OpenAI instrumentor | Yes: passes `prompt_tokens`; `cached_tokens` becomes `prompt_details.cache_read` | Yes: passes through | OpenInference repository, `openinference-instrumentation-openai/.../_response_attributes_extractor.py` at `a719562e2` | verified |
| OpenInference Anthropic instrumentor | **Yes**: emits `prompt = input_tokens + cache_creation + cache_read` | n/a | `openinference-instrumentation-anthropic/.../_utils.py` at `a719562e2` | verified |
| OpenInference LangChain tracer | Yes for a raw Anthropic usage (sums it in). For LangChain's `usage_metadata` it passes `input_tokens` through and adds the cache back with a heuristic for Bedrock models (the heuristic compares the total) | Passed through | `openinference-instrumentation-langchain/.../_tracer.py` at `a719562e2` | verified (heuristic) |
| OpenInference LlamaIndex | Yes: OpenAI shape passes `prompt_tokens`; Anthropic shape sums `input + cache_creation + cache_read` | Passed through | `openinference-instrumentation-llama-index/.../_handler.py` at `a719562e2` | verified |
| OpenInference Bedrock, InvokeModel (Anthropic models) | **Yes**: sums `input + cache_creation + cache_read` | n/a | `openinference-instrumentation-bedrock/.../utils/anthropic/_attributes.py` at `a719562e2` | verified |
| OpenInference Bedrock, **Converse** | **No**: emits the raw `inputTokens` as the prompt, with `cacheReadInputTokens` / `cacheWriteInputTokens` separately | Passed through | `openinference-instrumentation-bedrock/.../_converse_attributes.py` at `a719562e2` | verified |
| OpenInference google-genai | **Yes**: the code notes cached tokens are already in `prompt_token_count` | Completion is built as candidates plus thoughts (per a code comment; the whole computation was not read) | `openinference-instrumentation-google-genai/.../_utils.py` at `a719562e2` | verified for the cache, medium for reasoning |
| OpenLLMetry Anthropic | **Yes**: `gen_ai.usage.input_tokens = input + cache_read + cache_creation`; cache counts under the `cache_read.input_tokens` / `cache_creation.input_tokens` keys | n/a | `opentelemetry-instrumentation-anthropic/.../__init__.py` at `dac2534fa` | verified |
| OpenLLMetry OpenAI | Yes: `input_tokens` is `prompt_tokens`; cached tokens under `cache_read.input_tokens` | Passed through | `opentelemetry-instrumentation-openai/.../shared/__init__.py` at `dac2534fa` | verified |
| Vercel AI SDK usage | Yes: `inputTokens` is the total (`noCacheTokens` + `cacheReadTokens`) | Reported as `reasoningTokens` | `vercel/ai`, `packages/ai/src/types/usage.ts`. How the OpenInference Vercel processor maps it into `llm.token_count.prompt` was **not** read | SDK verified, mapping unverified |
| Other instrumentors and hand-written spans (OpenLLMetry Bedrock, Vertex and LangChain; OpenInference LiteLLM, Groq and others) | **Unverified** | Unverified | not read | unverified: falls to the arithmetic rules above |

A source marked *No* (or an unverified one that reports the cache on top) is handled by rule 1
or 2 above only if it reports a total or the cache exceeds the prompt. That is why the
recommendation is to have instrumentation report a total: with one, every row of this table
prices correctly whichever way the source counts.

---

## Attach scores (evals, human feedback, guardrails)

A **score** is any measurement on a span/trace/session/run — one schema for eval
results, human annotations, and API writes. Target a span by id:

```bash
curl localhost:4318/v1/scores -d '{
  "span_id": "<span_id>", "name": "faithfulness", "value": 0.91, "source": "eval"
}'
curl 'localhost:4318/v1/scores?span_id=<span_id>'     # read scores on a span
```

Scores appear on the span in the console and in the **Scores** view (filterable by
`eval` / `human` / `api`).

### Emit the standard evaluation event

If your evaluator already follows the OpenTelemetry GenAI conventions, it can attach the result
to the span it evaluated as a `gen_ai.evaluation.result` **span event**, and evald stores it as a
score with no evald-specific code:

```python
with tracer.start_as_current_span("chat") as span:
    ...  # the LLM call
    span.add_event("gen_ai.evaluation.result", {
        "gen_ai.evaluation.name": "Relevance",            # required
        "gen_ai.evaluation.score.value": 0.8,             # a number, and/or:
        "gen_ai.evaluation.score.label": "relevant",      # a short label
        "gen_ai.evaluation.explanation": "answers the question",
    })
```

It works over OTLP/HTTP (protobuf or JSON) and OTLP/gRPC. Re-sent batches do not duplicate the
score. An event with no evaluation name, or with neither a number, a label nor an `error.type`, is
dropped and counted in `evald_eval_events_malformed_total`, and never affects the span. Events
emitted as OTLP **log records** are not ingested: attach them to the span instead. The exact
mapping, limits and the pinned specification version are in [API.md](./API.md#post-v1traces); read
scores back out in the same shape with `evald scores export --format gen_ai-event`.

**Phoenix-compatible:** already POSTing to Phoenix? Point it at evald —
`POST /v1/span_annotations` is accepted with Phoenix semantics (a `HUMAN`
annotation maps to a span-targeted score with `source=human`; a non-empty
`identifier` upserts).

---

## Verify it's flowing

```bash
curl localhost:4318/v1/spans                    # recent normalized spans (JSON)
curl localhost:4318/v1/stats                    # hot-tier backlog / shedding
curl localhost:4318/v1/sql \
  -d '{"sql":"SELECT model, COUNT(*) n, SUM(total_tokens) tok FROM spans GROUP BY model ORDER BY tok DESC"}'
open  http://localhost:4318/                     # the console — Overview / Traces / Cost / SQL
```

The **Overview** KPIs (spans, traces, scores, ingest health) are read live from
these endpoints — send a trace and the counts move.

A default `evald serve` logs **one line per ingest request**, not one per span. When you
want to see what your SDK actually sent — dialect, ids, `oi_kind`, model, provider, token
counts, cost, duration — run the server with the per-span line turned on:

```bash
RUST_LOG=evald=debug evald serve
```

Use it while wiring up instrumentation, not under load: that line costs about a third of
the ingest hot path ([OPERATIONS.md § Logs](./OPERATIONS.md#logs)).

---

## No SDK at all

Any OTLP-JSON `POST` works — handy for smoke tests or non-OTel languages:

```bash
curl -H 'content-type: application/json' localhost:4318/v1/traces -d '{
  "resourceSpans":[{"scopeSpans":[{"spans":[{
    "traceId":"0123456789abcdef0123456789abcdef","spanId":"0123456789abcdef",
    "name":"chat","kind":3,
    "startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000500000000",
    "attributes":[
      {"key":"openinference.span.kind","value":{"stringValue":"LLM"}},
      {"key":"gen_ai.request.model","value":{"stringValue":"gpt-4o"}},
      {"key":"gen_ai.usage.input_tokens","value":{"intValue":"123"}},
      {"key":"gen_ai.usage.output_tokens","value":{"intValue":"45"}}
    ]}]}]}]}'
```

---

## Production notes

- **Endpoint:** in a single-node (OSS) deployment, point apps straight at the node's
  `:4318`. In an EE fleet, apps export to the **gateway** (`trace_id`-sharded ingest)
  instead; the console served by the fleet-query node then reads the consolidated
  store. Same OTLP contract either way.
- **Security:** `:4318` has no auth in the OSS node — bind it to localhost or a
  trusted network, or terminate auth/TLS at a reverse proxy in front of it.
  See [`OPERATIONS.md`](OPERATIONS.md).
- **Batching:** use a `BatchSpanProcessor` (not `SimpleSpanProcessor`) in real
  services so export doesn't sit on the request path; evald absorbs bursts but the
  SDK-side batch still matters for your app's latency.

See also: [`API.md`](API.md) (full endpoint reference) · [`CONFIG.md`](CONFIG.md)
(receiver flags, hot-tier bounds, blob offload).
