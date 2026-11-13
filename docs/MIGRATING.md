# Can model B replace model A here?

The migration gate: whether a regional or self-hosted model can take over from the one you use
now, **on your traffic**, by language, and what it saves. It is four commands, and the answer is
a verdict a reviewer can check, not a leaderboard number.

```text
production spans ──dataset from-spans──▶ dataset.jsonl
                                           │
                     evald replay --endpoint A ──▶ run A ─┐
                     evald replay --endpoint B ──▶ run B ─┴─▶ eval compare --non-inferior --margin δ
```

## 1. A dataset from your own traffic

```bash
evald dataset from-spans --model gpt-4o-mini --limit 500 --out traffic.jsonl
```

Each row is a model call your app made: its prompt (`input`), its answer, and its span. Three
things make it a migration dataset:

- **An `id` per row**, so the two runs pair item by item. `from-spans` keys rows by span id, which
  works; an id of your own survives a re-export.
- **What to slice by in `metadata`**: `language`, `route` and `task` are recorded by default
  (`slice_by:` in the config names others). A verdict per language is what tells you that B is as
  good in English and not in Malay.
- **Something to score against**: an `expected_output`, for exact matching or a `qa_correctness`
  judge, or a judge that needs none (`answer_relevancy` on the item's own input). The production
  answer in `output` is replaced by each replay's.

```yaml
# migrate.yaml
dataset: ./traffic.jsonl
evaluators:
  - type: exact_match
endpoints:
  - name: hosted                     # what you use now
    api: openai-chat
    base_url: https://api.openai.com
    api_key_env: OPENAI_API_KEY
    jurisdiction: US
  - name: gpu-box                    # what you would move to
    api: openai-chat
    base_url: http://gpu-box:8000/v1
    max_in_flight: 8
    jurisdiction: MY
  - name: judge-box                  # a judge that is neither A nor B
    api: openai-chat
    base_url: http://judge-box:8000/v1
    jurisdiction: MY
judges:
  - rail: qa_correctness
    endpoint: judge-box
    model: prometheus-7b
```

## 2. Replay it on both

```bash
ALLOW=api.openai.com,gpu-box,judge-box
RUN_A=$(evald --egress-allow $ALLOW replay --config migrate.yaml --endpoint hosted  --model gpt-4o-mini | sed -n 's/^run_id: //p')
RUN_B=$(evald --egress-allow $ALLOW replay --config migrate.yaml --endpoint gpu-box --model qwen2.5:7b  | sed -n 's/^run_id: //p')
```

Each replay sends every item's prompt, at temperature 0 with a fixed seed, and scores the answers
with the config's evaluators and judges as a run of its own. Replaying A as well as B (rather than
comparing B against A's production answers) holds the parameters equal: same prompts, same
sampling, same day. Every call is in the egress ledger (`evald egress report`) with the endpoint's
jurisdiction, and the run's manifest records which weights answered.

A hosted model is priced from the built-in table. A self-hosted one has no list price: give it
what its GPUs cost per token, in a price-table entry, so the comparison has a cost on both sides.
If the box already serves real traffic that evald sees, `evald cost gpu-rate` works it out from a
window evald saw in full (the GPUs are paid for idle hours too, so a short replay is not that
window); otherwise write your own figure:

```bash
evald cost gpu-rate --gpu-hour-usd 2.50 --gpus 1 --model qwen2.5:7b --since 7d   # prints the entry
echo '{"qwen2.5:7b": {"input_cost_per_token": 6e-8, "output_cost_per_token": 6e-8}}' > gpu-rate.json
```

## 3. Compare with a margin

```bash
evald eval compare "$RUN_A" "$RUN_B" --non-inferior --margin 0.02 --price-table gpu-rate.json
```

`--price-table` prices both replays from their recorded tokens at the table's rates (over the
built-in one).

**Choose the margin before you look at the results**, and write down why: it is the largest drop
you would accept for what B gives you (cost, latency, keeping data in-country). evald never
defaults it. The verdicts ([CONFIG.md § Non-inferiority](./CONFIG.md#non-inferiority---non-inferior---margin)):

| Verdict | Means | Gate |
|---|---|---|
| `non-inferior` | B is shown no worse than A by more than the margin | passes |
| `inferior` | B is shown worse than A by more than the margin | fails |
| `inconclusive` | the data cannot tell either way | **fails** |

`inconclusive` failing is the point. A paired t-test on 200 items can call a one-point drop
"noise", meaning only that 200 items cannot see it; non-inferiority asks whether they can rule
out a drop bigger than the margin, and says so when they cannot. Rows per slice show where B
falls short; the gate is every evaluator over all its items.

## 4. Read the cost and latency

When the runs are replays, the comparison sets their tokens, cost and latency side by side
(the [example in CONFIG.md](./CONFIG.md#non-inferiority---non-inferior---margin)). Latency on
replays measures the endpoint under the replay's own load (`max_in_flight`), not your production
traffic: compare them with each other, not with your dashboards.

## In CI

```bash
evald replay --config migrate.yaml --endpoint gpu-box --model "$CANDIDATE" | tee replay.out
evald eval compare "$BASELINE_RUN" "$(sed -n 's/^run_id: //p' replay.out)" \
  --non-inferior --margin 0.02 --junit migration.xml
```

The JUnit report has one `non-inferior: <evaluator>` case per evaluator, failing as the gate does.
Keep the baseline run in the store the job reads (`--data-dir`), or replay A in the same job.
