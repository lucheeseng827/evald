# Framework recipes

One directory per agent framework: a small agent with one tool, the few lines that send its
telemetry to evald, and the check CI runs on what evald made of it. Copy the first block of a
recipe's `app.py` / `app.mjs` into your own app; everything after it is ordinary framework code.

| Recipe | Framework | How its telemetry reaches evald |
|---|---|---|
| [`langgraph`](langgraph/app.py) | LangGraph | OpenInference LangChain instrumentor → OTLP traces |
| [`openai-agents`](openai-agents/app.py) | OpenAI Agents SDK | OpenInference instrumentor, `exclusive_processor=True` (nothing uploaded to OpenAI) |
| [`pydantic-ai`](pydantic-ai/app.py) | Pydantic AI | built in (`Agent.instrument_all()`): OTel GenAI conventions |
| [`crewai`](crewai/app.py) | CrewAI | OpenInference CrewAI + OpenAI instrumentors; `CREWAI_DISABLE_TELEMETRY=true` |
| [`litellm-proxy`](litellm-proxy/config.yaml) | LiteLLM proxy | `callbacks: ["otel"]` in the proxy config |
| [`vercel-ai-sdk`](vercel-ai-sdk/app.mjs) | Vercel AI SDK 7 | `@ai-sdk/otel` integration + an OTel NodeSDK |
| [`claude-agent-sdk`](claude-agent-sdk/app.py) | Claude Agent SDK | Claude Code's telemetry events → OTLP **logs** (`evald connect claude-code` prints the variables) |

Every recipe exports to the address in `OTEL_EXPORTER_OTLP_ENDPOINT` over OTLP/HTTP, so pointing
one at your own evald is that variable and nothing else.

## Run one

```sh
examples/recipes/run.sh langgraph          # needs python3 (and uv, if you have it) or node
```

`run.sh` starts evald on scratch ports, starts [`stub_provider.py`](stub_provider.py) (an
OpenAI- and Anthropic-compatible stand-in: no key, no network, a tool call then an answer, fixed
token counts), installs the recipe's pinned dependencies into a scratch environment, runs the app
and then [`check.py`](check.py) against the recipe's `expect.json`. `EVALD=path/to/evald` picks
the binary; `KEEP=1` keeps the scratch directory (evald's data, the logs) for a look.

## What the check asserts

`check.py` asserts **evald's reading** of the telemetry, not the framework's output:

- the model calls are typed LLM, with the model, token counts and a cost (derived from evald's
  price table when the framework reports none);
- the tool call is a TOOL span under `gen_ai.tool.name`, the key evald normalises every
  framework's spelling to;
- **no other span carries usage or cost.** An agent span that reports the total of its model
  calls would make every cost report count the run twice; evald keeps such a total in the span's
  raw attributes only (`evald.usage = "aggregate"`, see `src/agent.rs`).

CI runs every recipe on each push (the `recipes` jobs), so a framework release that renames an
attribute, or an evald change that misreads one, fails there before it reaches you.
