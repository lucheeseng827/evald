# Instrumenting your LLM app for evald

evald is an **OTLP/OpenTelemetry receiver**. Anything that speaks OTLP can send it
traces — point your app's OTel exporter at evald's endpoint and your spans, token
counts, costs, and scores show up in the console (trace list → span tree → scores),
the SQL surface, and the cost view.

- **Endpoints:** `POST /v1/traces` and `POST /v1/logs` on `http://<host>:4318` (OTLP/HTTP), and
  the same two services over OTLP/gRPC on `<host>:4317`.
- **Wire formats:** OTLP **protobuf** (gzip-aware) *and* **OTLP-JSON** — same routes.
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

## Coding agents

Coding agents export OpenTelemetry themselves. Point them at evald and every session shows up
with its model calls, tokens, cost and tool calls, with no code to write.

| Tool | Tested with | evald reads | Setup |
|---|---|---|---|
| Claude Code | 2.1.306 | its events; its beta spans too, one call counted once | `evald connect claude-code` |
| Codex CLI | 0.163.0 | its events; its spans are kept, never counted | `evald connect codex` |
| Gemini CLI | 0.65.0 | its events; its spans are kept, never counted | `evald connect gemini-cli` |

Each mapping was written against what that version actually sends: a real session, captured
with `scripts/capture-coding-agent.sh` (the CLI runs against the recipes' stub provider and a
stub MCP server, so no key and no network), committed under `tests/fixtures/<tool>/<version>/`
and checked by the dialect's tests. The `evald connect` snippets were then run as printed, with
the real CLI, into a real `evald serve`. A newer version is not refused: an attribute a mapping
does not know is kept on the span, so SQL sees it.

Across the three, a session reads the same way: one trace per turn under a `*.user_prompt`
marker, one LLM span per model call, one TOOL span per tool call, and MCP tools named
`mcp__<server>__<tool>` whatever the tool's own spelling.

The snippets are **privacy first**: where a tool exports content by default (Gemini CLI's
prompts, Codex's tool output), `evald connect` turns it off, even when that costs a report a
detail, and the snippet says what. Opting back in is one line in the tool's config.

### Claude Code

Claude Code sends its **events** over the OTLP logs signal: one per API request (model,
tokens, Claude Code's own cost estimate, duration, time to first token), one per tool result
and permission decision, one per prompt. That is all evald needs, and it is stable. Spans are
a beta on top: they add the timing tree of a turn, and are optional.

`evald connect claude-code` prints this block for the evald you run (`--http`, `--traces`,
`--token-env`, or `--settings-json` for a `~/.claude/settings.json` `env` block), so
`eval "$(evald connect claude-code)"` is the whole setup. Then `evald sessions` lists each
session's calls, tool calls, failures and cost. By hand:

```bash
export CLAUDE_CODE_ENABLE_TELEMETRY=1
export OTEL_LOGS_EXPORTER=otlp
export OTEL_METRICS_EXPORTER=none              # evald does not ingest OTLP metrics
export OTEL_EXPORTER_OTLP_PROTOCOL=grpc        # or http/protobuf with :4318
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
export OTEL_LOGS_EXPORT_INTERVAL=1000          # see "both signals" below

# Optional: the span tree of each turn (beta).
export CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1
export OTEL_TRACES_EXPORTER=otlp
```

With auth on, add `export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer $EVALD_TOKEN"`.
Prompt text, tool inputs and responses stay redacted unless you turn Claude Code's
`OTEL_LOG_*` switches on; nothing here needs them. One is worth knowing: without
`OTEL_LOG_TOOL_DETAILS=1` Claude Code reports every MCP tool as `mcp_tool`. With it, evald names
the call as Claude Code does (`mcp__<server>__<tool>`) and the session view can spot repeated
identical calls, at the cost of storing each tool's arguments (Bash commands included).

**What each event becomes** (verified against Claude Code 2.1.306; fixtures in
`tests/fixtures/claude-code/`):

| Event | evald span |
|---|---|
| `api_request` | `claude_code.llm_request`, kind LLM: model, tokens (input excludes cache reads and writes, as Claude Code reports it; priced once), `cost_usd` = Claude Code's estimate, duration, time to first token |
| `api_error` | the same, with an error status and message |
| `tool_result` | `claude_code.tool`, kind TOOL: tool name, `gen_ai.tool.call.id`, duration, an error status when it failed |
| `tool_decision` with `decision=reject` | `claude_code.tool` with the error status `rejected (<source>)` |
| `user_prompt` | `claude_code.user_prompt`, a zero-duration marker per turn |
| accepted `tool_decision`, `assistant_response`, `api_refusal` | folded: the result or request event carries the facts |
| startup, plugin, hook and settings events | not GenAI facts: dropped (or kept as plain spans under `--logs all`) |

Every attribute the event carried is kept on its span, `session.id` and `prompt.id` included,
so anything Claude Code sends is queryable with SQL. A turn is one trace: the record's own trace
context when it has one, otherwise a trace per `prompt.id` under its `user_prompt` marker.

**Spans over traces.** Claude Code 2.1.306 names its token counts `input_tokens`,
`output_tokens`, `cache_read_tokens` and `cache_creation_tokens` on `claude_code.llm_request`
spans, not the `gen_ai.usage.*` names. evald reads both.

**Both signals on.** Each API call then arrives twice. Both copies get the same span id
(derived from Claude Code's `request_id`), and the second to arrive is dropped at ingest, so
cost, tokens and `/metrics` count each call once (`evald_duplicate_calls_dropped_total` counts
the copies dropped). Which copy is kept matters for cost: the event carries Claude Code's own
cost, while a kept span copy is priced from evald's price table, and a model the table does not
know yet is left unpriced (re-price later with `evald cost --price-table`). Exporting logs more
often than traces (`OTEL_LOGS_EXPORT_INTERVAL=1000`; traces default to 5 s) makes the event
arrive first. Calls only spans describe, such as the permission classifier's, still arrive from
the span. Tool calls are different: a tool span is not dropped, because other spans hang under
it. A `tool_result` event that arrives after its span is dropped, one that arrives before it is
kept. So with both signals on, a tool call can appear twice in raw SQL; `evald.source = 'log'`
marks the event copy.

**When spans don't arrive.** Claude Code samples its spans from the parent trace context: under
an inherited `TRACEPARENT` whose sampled flag is `00` (some CI runners and SDK hosts set one),
no spans are exported at all, while events still are. Events alone are a complete record.

**Privacy.** Claude Code puts `user.email`, account and organization ids on every event. evald
stores them as received. `--redact email` rewrites the email before anything is written; the ids
need a `--redact-custom` rule ([CONFIG.md](./CONFIG.md#evald-serve)).

### Codex CLI

Codex sends its **events** over the OTLP logs signal (`[otel] exporter`): one per model request
and one when its stream completes (with the tokens), one per tool decision and tool result, one
per prompt. Its **spans** (`[otel] trace_exporter`) are its internals, hundreds a turn (file
reads, auth, tool routing): evald keeps them if sent, but its reports come from the events.

`evald connect codex` prints the `[otel]` table for `~/.codex/config.toml` (`--http`,
`--traces`, `--token-env`):

```toml
[otel]
exporter = { otlp-grpc = { endpoint = "http://localhost:4317" } }
# or: { otlp-http = { endpoint = "http://localhost:4318/v1/logs", protocol = "binary" } }
log_user_prompt = false          # Codex's default, stated
tool_result = { max_bytes = 0 }  # see Privacy
```

`config.toml` expands no variables: with auth on, the snippet leaves a
`headers = { authorization = "Bearer <your evald token>" }` placeholder in the exporter for the
token itself. Codex sends its **metrics** to OpenAI's analytics (`statsig`) unless
`metrics_exporter` says otherwise, whatever `exporter` is; evald takes no metrics, so the
snippet leaves that setting to you (`metrics_exporter = "none"` stops them).

**What each event becomes** (verified against Codex 0.163.0; fixtures in `tests/fixtures/codex/`):

| Event | evald span |
|---|---|
| `sse_event` with `event.kind = response.completed` and token counts | `codex.llm_request`, kind LLM: model, tokens (input *includes* the cached tokens, output the reasoning ones: OpenAI's reading), time to first token, priced from the table |
| `api_request` / `websocket_request` that failed, `sse_event` with an error | the same, with an error status (`HTTP 429`, the stream's error) |
| `tool_result` | `codex.tool`, kind TOOL: tool name (`mcp__<server>__<tool>` for MCP), `gen_ai.tool.call.id`, arguments, duration, an error status when it failed |
| `tool_decision` that is not an approval | the result's error status: `denied (User)`, `abort (Config)` |
| `user_prompt` | `codex.user_prompt`, a zero-duration marker per turn |
| `conversation_starts`, a successful request, every other `sse_event`, approvals | folded: remembered for the records above, or covered by them |
| `startup_phase`, `turn_ttft`, `turn_cost`, `sandbox_outcome`, … | not mapped (kept as plain spans under `--logs all`) |

Codex fills the OTLP `event_name` field with a Rust `tracing` callsite
(`event otel/src/events/session_telemetry.rs:1132`); evald reads the `event.name` attribute
first, which is where Codex puts the name.

**A command that fails is not a failed tool call to Codex.** `exec_command` reports
`success = true` for `exit 3`, with `Process exited with code 3` in its output. evald gives such
a call the error status `exit code 3` when the output is exported. With `tool_result =
{ max_bytes = 0 }`, which `evald connect codex` sets on purpose, it is not: Codex's verdict
stands, and the session's tool-error count leaves failed commands out (refused calls and
failed MCP calls still count). To count them, drop that line, and evald stores up to 2 KiB of
every command's output.

**Where a span sits.** Codex's records carry a trace context, but not one to place by: with its
spans exported each turn has a trace, without them every turn of a conversation shares one. So
evald places by turn: each `user_prompt` starts a trace, and the spans after it sit under its
marker. The record's own context is kept as `evald.otel.trace_id` / `evald.otel.span_id`, the
join to Codex's spans. The completion record that carries a call's tokens is logged outside any
span and usually lands in a later export than its request (a stream outlasts the exporter's
one-second flush): evald remembers each conversation's last request for a while, across export
requests, to give the call its start time. A restart in between costs that call its duration,
never its tokens or cost.

**Both signals on.** The one span with token counts (`handle_responses`, on the stream's
completion) has them set aside (`evald.usage = "copy"`), and Codex's spans are kept out of the
session rollup, so each call and each turn counts once.

**The WebSocket transport.** Against OpenAI itself, Codex can use the Responses API over a
WebSocket, and then logs `websocket_request` where it would log `api_request`. That is mapped
from Codex's source at the 0.163.0 tag; the captured session ran over HTTP.

**Privacy.** Prompts arrive as `[REDACTED]` unless `log_user_prompt = true`. Tool calls are a
different matter: every `tool_result` carries the call's **arguments** (a shell command in
full) and, by default, up to 2 KiB of its **output**. `tool_result = { max_bytes = 0 }` keeps
the output out (the snippet sets it); the arguments have no switch, and evald stores them.

### Gemini CLI

Gemini CLI sends its **events** over the OTLP logs signal whenever its telemetry is on: one per
model request and one per response (with the tokens), one per tool call, one per prompt. It
also always sends **spans** of the same calls, each carrying its 31 KB system prompt and its
tool list.

`evald connect gemini-cli` prints the variables (`--http`, `--traces`, `--token-env`, or
`--settings-json` for a `~/.gemini/settings.json` `telemetry` block):

```bash
export GEMINI_TELEMETRY_ENABLED=true
export GEMINI_TELEMETRY_TARGET=local
export GEMINI_TELEMETRY_OTLP_PROTOCOL=grpc      # or http, with :4318
export GEMINI_TELEMETRY_OTLP_ENDPOINT=http://localhost:4317
export GEMINI_TELEMETRY_LOG_PROMPTS=false       # Gemini CLI logs prompts by default
export OTEL_TRACES_SAMPLER=always_off           # its spans repeat the events; --traces keeps them
```

With auth on, add `export GEMINI_TELEMETRY_OTLP_HEADERS="Authorization=Bearer $EVALD_TOKEN"`.
Leave `GEMINI_TELEMETRY_TRACES_ENABLED` unset: it does not turn spans on (they are always
sent), it adds prompts and tool output to them.

**What each event becomes** (verified against Gemini CLI 0.65.0; fixtures in
`tests/fixtures/gemini-cli/`):

| Event | evald span |
|---|---|
| `gemini_cli.api_response` | `gemini_cli.llm_request`, kind LLM: model, tokens (input *includes* the cached tokens; the thinking tokens come *on top of* the output, and are priced once, at the output rate), duration |
| `gemini_cli.api_error` | the same, with an error status |
| `gemini_cli.tool_call` | `gemini_cli.tool`, kind TOOL: tool name (`mcp__<server>__<tool>` for MCP), duration, an error status when it failed (`tool_not_registered`, …) or was rejected |
| `gemini_cli.user_prompt` | `gemini_cli.user_prompt`, a zero-duration marker per turn |
| `gemini_cli.api_request`, `gen_ai.client.inference.operation.details` | folded |
| `config`, `model_routing`, `startup_stats`, … | not mapped (kept as plain spans under `--logs all`) |

`gen_ai.client.inference.operation.details` is the GenAI conventions' event: Gemini CLI sends
two per call (at the start and at the end), without the cache or thinking counts, and with its
whole system prompt on each copy whatever `logPrompts` says. evald folds them and stores none.

**A command that fails is not a failed tool call to Gemini CLI either**: `exit 3` is
`success: true`, and the exit code is in output it does not export.

**Where a span sits.** Gemini CLI gives every model call and every tool call a trace of its own.
evald places by turn instead: each `user_prompt` starts a trace, and the spans after it sit
under its marker, with the record's own context kept as `evald.otel.trace_id` /
`evald.otel.span_id` (a model call's names its `llm_call` span). Gemini CLI reuses one
`prompt_id` for every turn of a headless session, so the turn is the prompt event, not the id.

**Both signals on** (`--traces`). An `llm_call` span's usage is set aside, its `tool_call` spans
are not read as tool calls, and none of its spans is part of the session rollup: each call and
each turn counts once.

**Privacy.** Prompts are logged unless `logPrompts` is false (the snippet sets it). The
process's command line rides on every record as a resource attribute (`process.command_args`),
and for `gemini -p "…"` that is the prompt; evald keeps no resource attribute but
`service.name`, so it never reaches the store.

### Compatibility fixes these tools found

Both were refusing data from any exporter that does the same, not only these tools:

* **gzip over gRPC.** Node's OTLP/gRPC exporters compress every message. evald's gRPC door
  refused them all `UNIMPLEMENTED` (Gemini CLI's default protocol); it now accepts gzip.
* **Empty values in OTLP/JSON.** Node's OTLP/HTTP-JSON exporters send `"value": {}` for an
  attribute left undefined. The JSON door refused such an export whole; it now reads an empty
  value as no value, as protobuf does.

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
| Agent | `gen_ai.agent.name` / `gen_ai.agent.id` | `agent.name` |
| Tool and call | `gen_ai.tool.name` / `gen_ai.tool.call.id` / `gen_ai.tool.call.arguments` | `tool.name` / `tool_call.id` / `input.value` |
| MCP tool call | `mcp.method.name` = `tools/call`, with `gen_ai.tool.name` | — |

- **Tokens** power the **Cost** view (grouped by model/provider/service/user) and
  the SQL `spans.total_tokens` column. Set them and cost attribution just works.
- **Cost**: set `gen_ai.usage.cost` (or `llm.cost.total`) if you price calls
  yourself and evald keeps your number. Otherwise evald prices the span from its model and
  token counts (see [Cost and token semantics](#cost-and-token-semantics)); a model it has no
  price for stays empty and is reported as `(no price)`.
- Large `input.value`/`output.value` payloads are offloaded to the blob store and
  shown in the console as an `evald-blob:<key>` reference (fetch via `/v1/blobs/<key>`).
- **Agents and tools.** A tool or agent span that names its tool, call or agent only by an
  alias (OpenInference's, the Vercel AI SDK's `ai.toolCall.*`, Claude Code's `tool_name`) also
  gets the `gen_ai.*` key at ingest, so one key answers a SQL query; the alias stays. The
  session view counts a call once across its copies and reports the trajectory: model calls per
  trace, the tool-error rate, and repeated identical tool calls (three in a row is a suspected
  loop). `/metrics` carries `evald_tool_calls_total` and `evald_tool_call_errors_total` by tool.
- **An agent's total is not spend.** The GenAI conventions let an `invoke_agent` span report
  the usage of the whole invocation. When the agent runs in your process its model calls are
  spans of their own, so evald keeps that total only in the span's raw attributes (marked
  `evald.usage = "aggregate"`) and prices the model calls; otherwise every cost report would
  count the run twice. A `CLIENT` agent span (a remote agent, whose model calls leave no spans
  here) keeps its usage, and is priced.
- **Operations.** `gen_ai.operation.name` decides the span's kind: `chat`, `text_completion`,
  `generate_content` are model calls; `execute_tool` a tool; `invoke_agent`, `create_agent` an
  agent; `retrieval`, `search_memory` a retriever; `embeddings` an embedding. Any other
  operation is a model call when it carries `gen_ai.request.*`, `gen_ai.response.*` or
  `gen_ai.usage.*` attributes, and a chain (a workflow, a plan, an agent step) when it does not.

### Agent frameworks

[`examples/recipes/`](../examples/recipes/README.md) has one tested recipe per framework —
LangGraph, the OpenAI Agents SDK, Pydantic AI, CrewAI, the LiteLLM proxy, the Vercel AI SDK and
the Claude Agent SDK — each the few lines that send its telemetry here, run in CI against a stub
model provider with a check of what evald made of it.

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
dropped and counted in `evald_eval_events_malformed_total`, and never affects the span. The same event
emitted as an OTLP **log record** works too (`POST /v1/logs` or the gRPC `LogsService`), targeting
the span its trace context names. The exact
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
- **Security:** evald binds loopback by default and has no auth until you arm it. Before
  exposing `:4318` / `:4317`, set `--auth-token` (or `EVALD_AUTH_TOKEN`); exporters then send
  `OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <token>"` (`evald connect … --token-env`
  prints it). evald does no TLS: terminate it at a reverse proxy on an untrusted network.
  See [`OPERATIONS.md` § Security posture](./OPERATIONS.md#security-posture).
- **Batching:** use a `BatchSpanProcessor` (not `SimpleSpanProcessor`) in real
  services so export doesn't sit on the request path; evald absorbs bursts but the
  SDK-side batch still matters for your app's latency.

See also: [`API.md`](API.md) (full endpoint reference) · [`CONFIG.md`](CONFIG.md)
(receiver flags, hot-tier bounds, blob offload).
