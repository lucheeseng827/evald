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
| Cache read / write | `gen_ai.usage.cache_read.input_tokens` / `cache_creation.input_tokens` | `llm.token_count.prompt_details.cache_read` / `cache_write` |
| Reasoning tokens | `gen_ai.usage.reasoning_tokens` | `llm.token_count.completion_details.reasoning` |
| Cost (USD) | `gen_ai.usage.cost` | `llm.cost.total` |
| Input / output payload | `gen_ai.input.messages` / `gen_ai.output.messages` | `input.value` / `output.value` |
| Session / thread | `gen_ai.conversation.id` | `session.id` |
| User | — | `user.id` |
| Service | resource `service.name` | resource `service.name` |

- **Tokens** power the **Cost** view (grouped by model/provider/service/user) and
  the SQL `spans.total_tokens` column. Set them and cost attribution just works.
- **Cost**: set `gen_ai.usage.cost` (or `llm.cost.total`) if you price calls
  yourself; otherwise evald reports tokens and leaves `cost_usd` empty.
- Large `input.value`/`output.value` payloads are offloaded to the blob store and
  shown in the console as an `evald-blob:<key>` reference (fetch via `/v1/blobs/<key>`).

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
