# Reference stacks

evald beside a model server you run, each started with one command. Both build evald from this
checkout with the judge backend (`--features judge`), so `eval run`, `evald judge probe` and
online evals can use the served model as a judge; the first build takes a while. Everything is
published on `127.0.0.1` only.

| Stack | What runs | Needs |
|---|---|---|
| [`ollama/`](./ollama) | evald + Ollama | Docker. A GPU is optional. |
| [`vllm/`](./vllm) | evald + vLLM + an OpenTelemetry Collector + Prometheus + Grafana | An NVIDIA GPU and the container toolkit. |

## evald + Ollama

```bash
cd ops/stacks/ollama
docker compose up -d --build
docker compose exec ollama ollama pull qwen2.5:7b          # the judge model you choose
docker compose exec evald evald judge probe --config /stack/judge.yaml
docker compose exec evald evald eval run --config /stack/judge.yaml --data-dir /data/evals
```

`judge.yaml` is an eval whose judge runs on the stack's Ollama; change its `model:` to the model
you pulled. No model is chosen or pulled for you ([docs/JUDGES.md](../../docs/JUDGES.md) helps pick
one and check it on your own labels). An eval run uses `/data/evals`, a store of its own: the
server holds `/data`'s lock. To judge a sample of live calls too, set
`EVALD_ONLINE_EVAL=/stack/online.yaml` before `up`.

The UI and API are on http://localhost:4318/, OTLP/HTTP on 4318 and OTLP/gRPC on 4317, Ollama on
11434.

## evald + vLLM + Collector + Prometheus + Grafana

```bash
cd ops/stacks/vllm
VLLM_MODEL=Qwen/Qwen2.5-7B-Instruct docker compose up -d --build
```

| Where | What |
|---|---|
| localhost:4317 / :4318 | The collector's OTLP receivers. Point your app here; it forwards to evald. |
| localhost:8000/v1 | vLLM's OpenAI-compatible API. |
| http://localhost:4320/ | evald's UI and API. |
| http://localhost:3000/d/evald-vllm | The Grafana board: the app's calls beside vLLM's own metrics. |
| http://localhost:9090/ | Prometheus, scraping evald and vLLM. |

vLLM sends its request spans to the collector. Propagate `traceparent` on the app's calls to vLLM
and each one lands under the app's span of the call, which is what lets `evald latency` say how
long a call queued, prefilled and decoded on the server ([docs/INFERENCE-SERVERS.md](../../docs/INFERENCE-SERVERS.md)).
A gated model needs `HF_TOKEN`.

## Smoke tests

```bash
python3 ops/stacks/smoke.py vllm-cpu [--image evald:judge]
python3 ops/stacks/smoke.py ollama   [--image evald:judge] [--model qwen2.5:0.5b]
```

`vllm-cpu` runs the vLLM stack with vLLM replaced by a CPU stand-in that emits vLLM's span and
metric shapes ([`vllm/smoke/fake_vllm.py`](./vllm/smoke/fake_vllm.py)), so it runs without a GPU.
It asserts the app's calls and vLLM's spans reach evald through the collector with each server
span under the app's, each call's tokens counted once, Prometheus scraping both, and the board
provisioned in Grafana. The real vLLM container is not exercised there. `ollama` runs the Ollama
stack for real with a small model: `judge probe` passes and an `eval run` is judged by it. CI runs
both on every release tag, and builds the image with `ops/stacks/image.sh` from a static binary.
