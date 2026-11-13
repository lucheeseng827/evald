# Your own inference server

What evald can see of a model you serve yourself, server by server. What each server emits was
read from its source at the commit named; a server's next release can change it.

| Server | Server-side traces | Correlates with the app's span | Metrics | Read at |
|---|---|---|---|---|
| **vLLM** | Yes: one `llm_request` span per request, OTLP/gRPC (default) or OTLP/HTTP | Yes, when the app propagates `traceparent` | Prometheus `/metrics` | `vllm-project/vllm` f46ea25e (2026-10-22) |
| **TGI** | Yes: its router's spans, OTLP/gRPC only | Yes (`traceparent` is read) | Prometheus `/metrics` | `huggingface/text-generation-inference` b4adbf2 (2026-03-21), in maintenance mode |
| **llama.cpp server** | None found | — | Prometheus `/metrics` with `--metrics` | `ggml-org/llama.cpp` 7170cad (2026-10-22) |
| **Ollama** | None found | — | None found | `ollama/ollama` efabf6b (2026-10-21) |

Whatever the server, **the app's own span of the call is the record evald counts**: it names the
model, carries the usage the app was billed or charged for, and is what `evald cost`, `evald
latency` and the usage metrics read. Instrument the client ([INSTRUMENTATION.md](./INSTRUMENTATION.md));
for llama.cpp and Ollama it is the only path.

## vLLM

Start vLLM with its trace exporter pointed at evald (or at a collector that forwards to it):

```bash
vllm serve Qwen/Qwen2.5-7B-Instruct --otlp-traces-endpoint grpc://evald:4317
# or over OTLP/HTTP:
OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf \
  vllm serve Qwen/Qwen2.5-7B-Instruct --otlp-traces-endpoint http://evald:4318/v1/traces
```

vLLM's span for each request is `llm_request` (kind `SERVER`, from the request's arrival). It
carries the prompt and completion tokens and where the time went, in seconds:
`gen_ai.latency.time_in_queue`, `gen_ai.latency.time_in_model_prefill`,
`gen_ai.latency.time_in_model_decode`, `gen_ai.latency.time_in_model_inference`,
`gen_ai.latency.time_to_first_token` and `gen_ai.latency.e2e`. It names no model.

**With `traceparent` propagated** through the app's OpenAI-compatible call (the OpenTelemetry
HTTP client instrumentations do it; so does any client whose request carries the header), vLLM's
span is a child of the app's model-call span: one call seen from both ends. evald keeps the
app's span as the call and **sets the server span's usage aside** as a copy
(`evald.usage = "copy"`), so the tokens are not counted twice; the server span keeps its timings,
and its reported usage stays in `raw_attributes`. **Without a parent**, the server span is the
only record of the call and counts as one, with vLLM's time to first token. It names no model, so
it is unpriced and reported under `(unknown)`.

Either way the span is marked `evald.server = "vllm"`, and `evald latency` adds where the time
went, by the app's model:

```text
server time — vLLM request spans, by model (p50 / p95, ms)
model                          calls parented             queue           prefill            decode            server   client - server
Qwen2.5-7B-Instruct               40       40      12.0 / 350.0     245.8 / 384.5    940.1 / 1746.8   1352.2 / 2154.0     112.3 / 186.5
```

(40 synthetic calls, each an app span with vLLM's span under it, sent through `evald serve`.)

`client - server` is the app's own duration minus vLLM's end-to-end time: network, proxies and
the client library. A large one with a small `queue` points away from the GPU.

One caveat: an app that traces only the HTTP request, with no model-call span of its own, still
propagates `traceparent`, so vLLM's span has a parent and is set aside, and the call then has no
counted usage. Instrument the model call itself.

**Metrics.** vLLM's `/metrics` and evald's share a Prometheus. The board in
[`ops/stacks/vllm/grafana/dashboards/evald-vllm.json`](../ops/stacks/vllm/grafana/dashboards/evald-vllm.json)
puts them side by side: the app's p95 next to vLLM's end-to-end p95, tokens the app reported next
to tokens vLLM generated, vLLM's queue, prefill and decode time, running and waiting requests,
KV-cache use, and evald's own calls, spend and ingest. [`ops/stacks/vllm`](../ops/stacks/vllm)
runs all of it with one command.

## TGI

TGI exports its router's spans over **OTLP/gRPC only** (`--otlp-endpoint`, `--otlp-service-name`),
and reads `traceparent`, so its spans sit under the app's. Its timings are recorded as text
(`total_time = "1.23s"`, `queue_time`, `inference_time`, `time_per_token`): queryable in
`raw_attributes`, not parsed by evald. TGI is in maintenance mode upstream.

## llama.cpp server and Ollama

No server-side traces were found in either. The app's spans are the whole record: instrument the
client. llama.cpp's server has a Prometheus endpoint with `--metrics`; scrape it beside evald's.

## The cost of a model you serve

A self-hosted model has no list price, so its calls are unpriced until you give it one in
`--price-table`. `evald cost gpu-rate` derives a per-token rate from the tokens your own traffic
used and what the GPUs cost, and `evald cost --reprice-as <hosted model>` shows what the same
traffic would have cost on a hosted one ([CONFIG.md § `evald cost`](./CONFIG.md#evald-cost)).
