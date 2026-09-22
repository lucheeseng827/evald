#!/usr/bin/env python3
"""Regenerate src/price_baseline.json, the price table compiled into evald.

The baseline is a TRIMMED copy of the LiteLLM `model_prices_and_context_window.json`
(MIT, Copyright (c) 2023 Berri AI; see NOTICE): only text-model entries that carry a
per-token price, and only the fields evald prices with (input, output, cache read,
cache write, reasoning output, and the above-N-tokens tiers of those). It keeps the
LiteLLM shape on purpose, so `--price-table <file>` accepts the upstream file (or any
subset of it) unchanged.

    python scripts/gen_price_baseline.py                 # latest upstream, via the `gh` CLI
    python scripts/gen_price_baseline.py --ref <sha>     # a pinned upstream commit
    python scripts/gen_price_baseline.py --in prices.json --commit <sha> --date YYYY-MM-DD

The output records the source repository, commit and commit date under `_evald`; the
binary uses `<commit8>@<date>` as the `price_version` stamped on every derived cost.
Output is sorted and compact, so regenerating from the same commit is byte-identical.
"""
import argparse
import json
import re
import subprocess
import sys

REPO = "BerriAI/litellm"
PATH = "model_prices_and_context_window.json"
MODES = {"chat", "completion", "embedding", "responses"}
BASE_KEYS = (
    "input_cost_per_token",
    "output_cost_per_token",
    "cache_read_input_token_cost",
    "cache_creation_input_token_cost",
    "output_cost_per_reasoning_token",
)
# input_cost_per_token_above_200k_tokens, cache_read_input_token_cost_above_272k_tokens, ...
# (not the *_priority / *_flex / *_1hr variants, which evald does not price).
TIER = re.compile(
    r"^(input_cost_per_token|output_cost_per_token|cache_read_input_token_cost|"
    r"cache_creation_input_token_cost)_above_(\d+k?)_tokens$"
)


def gh(*args):
    return subprocess.run(["gh", "api", *args], check=True, capture_output=True, text=True).stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="infile")
    ap.add_argument("--ref", default="main")
    ap.add_argument("--commit")
    ap.add_argument("--date")
    ap.add_argument("--out", default="src/price_baseline.json")
    a = ap.parse_args()

    if a.infile:
        raw = open(a.infile, encoding="utf-8").read()
        if not (a.commit and a.date):
            sys.exit("--in needs --commit and --date so the version stamp is honest")
        commit, date = a.commit, a.date
    else:
        meta = json.loads(gh(f"repos/{REPO}/commits?path={PATH}&sha={a.ref}&per_page=1"))[0]
        commit, date = meta["sha"], meta["commit"]["committer"]["date"][:10]
        raw = gh("-H", "Accept: application/vnd.github.raw", f"repos/{REPO}/contents/{PATH}?ref={commit}")

    src = json.loads(raw)
    out = {}
    for name, e in src.items():
        if not isinstance(e, dict) or e.get("mode") not in MODES:
            continue
        keep = {}
        for k, v in e.items():
            if not isinstance(v, (int, float)) or isinstance(v, bool):
                continue
            if k in BASE_KEYS or TIER.match(k):
                keep[k] = v
        if "input_cost_per_token" in keep or "output_cost_per_token" in keep:
            out[name] = dict(sorted(keep.items()))

    doc = {
        "_evald": {
            "source": f"github.com/{REPO}",
            "path": PATH,
            "commit": commit,
            "date": date,
            "license": "MIT, Copyright (c) 2023 Berri AI",
        },
        **dict(sorted(out.items())),
    }
    with open(a.out, "w", encoding="utf-8", newline="\n") as f:
        json.dump(doc, f, separators=(",", ":"), ensure_ascii=True)
        f.write("\n")
    print(f"{len(out)} models from {commit[:8]}@{date} -> {a.out}")


if __name__ == "__main__":
    main()
