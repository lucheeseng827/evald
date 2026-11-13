# Open-weight judges

An LLM judge you serve yourself: which models to try, how to serve them on the
[reference stacks](../ops/stacks), and how to find out whether one can be trusted **on your data**
before its scores gate anything. evald ships no weights and chooses no default judge: the model is
yours to pick, and the evidence for it is yours to collect.

## What evald asks of a judge

Each rail sends a system prompt with a rubric and asks for one JSON object,
`{"score": <0-1>, "reasoning": "…"}`, at temperature 0 with a fixed seed. On an OpenAI-compatible
server it also asks for that JSON by schema (`response_format`), stepping down to `json_object`,
then to none, if the server refuses ([CONFIG.md § `endpoints:`](./CONFIG.md#endpoints-entries)).
So a good judge model here is one that:

- follows a rubric and answers in that JSON, or is constrained to by its server;
- gives the same score to the same question twice (temperature 0 is not a guarantee);
- agrees with **your** labels, not a leaderboard's.

`evald judge probe` checks the first two in a minute; `eval calibrate` measures the third.

## Models to try

Licences as each model's Hugging Face card states them, read 2026-10-22. A model fine-tuned from
another may also be bound by its base model's licence: read both, with whoever reads licences for
you.

| Model | Size | Licence (card) | Base model, its licence | Serve it |
|---|---|---|---|---|
| **Atla Selene Mini** `AtlaAI/Selene-1-Mini-Llama-3.1-8B` | 8B | Apache-2.0 | Llama 3.1 8B Instruct, Llama 3.1 Community License | vLLM: the HF id. Ollama: `atla/selene-mini` |
| **Prometheus 2** `prometheus-eval/prometheus-7b-v2.0` (also `-8x7b-v2.0`) | 7B / 8×7B | Apache-2.0 | Mistral 7B Instruct v0.2, Apache-2.0 | vLLM: the HF id |
| **Qwen2.5 Instruct** `Qwen/Qwen2.5-7B-Instruct`, `-14B-Instruct` | 7B / 14B | Apache-2.0 | Qwen2.5 base, Apache-2.0 | vLLM: the HF id. Ollama: `qwen2.5:7b`, `qwen2.5:14b` |
| **Llama 3.1 8B Instruct** `meta-llama/Llama-3.1-8B-Instruct` | 8B | Llama 3.1 Community License | — | vLLM: the HF id (gated: `HF_TOKEN`). Ollama: `llama3.1:8b` |

Notes that decide more than the table:

- **Selene Mini and Prometheus 2 were trained to evaluate**; a general instruct model was not, but
  follows a rubric well at 7B and up and takes a JSON schema from vLLM or Ollama without complaint.
- **Prometheus 2 was trained on its own prompt template**, answering `Feedback: … [RESULT] n` on a
  1-5 scale. evald's rails are not that template and ask for JSON, so whether it scores evald's
  rails well is an open question for your data: probe it, and look at the `scores` line.
- **Size against your GPU:** a 7-8B model in 16-bit needs about 16 GB of GPU memory for the weights
  alone; Ollama's default 4-bit builds fit in about 5-6 GB, at some cost to the judgement.
- **Do not judge with the model under test.** A model grading its own answers is a known bias;
  keep the judge a different model, and preferably a different family.

## Serving one on the stacks

**Ollama** ([`ops/stacks/ollama`](../ops/stacks/ollama)):

```bash
docker compose exec ollama ollama pull atla/selene-mini
docker compose exec evald evald judge probe --base-url http://ollama:11434/v1 --model atla/selene-mini
```

```yaml
# judge-selene.yaml — an eval judged by Selene Mini on the stack's Ollama.
dataset: ./dataset.jsonl
endpoints:
  - name: ollama
    api: openai-chat
    base_url: http://ollama:11434/v1
    max_in_flight: 1
    timeout_secs: 300
judges:
  - rail: qa_correctness
    endpoint: ollama
    model: atla/selene-mini
thresholds:
  judge_qa_correctness: 0.7
```

**vLLM** ([`ops/stacks/vllm`](../ops/stacks/vllm)): start the stack with the judge as its model,
or run a second vLLM for the judge so it does not share the GPU that serves production
([online evals](./CONFIG.md#online-sampled-evals) warn when it does).

```bash
VLLM_MODEL=prometheus-eval/prometheus-7b-v2.0 VLLM_SERVED_NAME=prometheus-7b docker compose up -d
docker compose exec evald evald judge probe --base-url http://vllm:8000/v1 --model prometheus-7b
```

```yaml
# judge-prometheus.yaml — the same eval, judged by Prometheus 2 on the stack's vLLM.
dataset: ./dataset.jsonl
endpoints:
  - name: vllm
    api: openai-chat
    base_url: http://vllm:8000/v1
    max_in_flight: 4
judges:
  - rail: qa_correctness
    endpoint: vllm
    model: prometheus-7b
```

## Before you trust it: calibrate on your own labels

A judge's scores are evidence only once you know how often it agrees with a person on **your**
items. With 50-100 labelled items:

1. **Label.** Put the items in a dataset with ids and their `span_id`s, and annotate those spans
   with a human score (`POST /v1/span_annotations`, or the UI).
2. **Judge them**: `evald eval run --config judge-selene.yaml`.
3. **Compare**: `evald eval calibrate --judge judge_qa_correctness --pass-threshold 0.5`. Read
   the bias (does it over-score?), the MAE, and **Cohen's kappa** for pass/fail: agreement beyond
   what the two sides' own pass rates give by chance. A judge that passes everything agrees with a
   mostly-passing label set and scores a kappa of 0.
4. **Repeat it**: run the same eval twice more with `--no-cache`, then
   `evald eval calibrate --judge judge_qa_correctness --runs "$RUN1,$RUN2,$RUN3"`. How often the
   verdict held, and Fleiss' kappa across the runs, say how much one score is worth.

What is good enough is your call, and the numbers to make it are those. As a reading aid, Landis
and Koch call a kappa of 0.61-0.80 "substantial" and above 0.80 "almost perfect"; a judge with
verdicts that move between identical runs, or a kappa under 0.4 against your labels, is advisory
at best. `eval calibrate --fail-on-divergence` turns drift from your labels into a CI failure.

**Judging in another language.** The built-in rails are English rubrics. Whether an English or
a translated rubric judges your Malay or Thai answers better is a question for your labels, one
language at a time: [LANGUAGES.md § Judging in other languages](./LANGUAGES.md#judging-in-other-languages).

**Calibrate the weights, not the name.** Each run's manifest records which weights judged it: on
Ollama the tag's digest and quantisation, on vLLM the weights it loaded
([CONFIG.md § Which weights answered](./CONFIG.md#which-weights-answered)). A calibration holds for
those weights; `ollama pull` over the same tag, or a different quantisation, is a new judge to
calibrate. `eval compare` warns when the judge's weights differ between the two runs it compares,
and the judge cache never serves one set of weights' scores for another.
