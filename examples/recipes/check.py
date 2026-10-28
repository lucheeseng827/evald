#!/usr/bin/env python3
"""Check that a recipe's telemetry reached evald with what the recipe promises.

    python3 check.py --url http://127.0.0.1:14318 --expect langgraph/expect.json

expect.json (every key optional):

    {
      "service": "recipe-langgraph",       only spans of this service.name count
      "llm":  {"min": 2, "model": "gpt-4o-mini", "tokens": true, "cost": true},
      "tool": {"min": 1, "name": "get_weather"},
      "wait_seconds": 15                   how long to wait for the spans to land
    }

`llm` counts spans evald typed LLM: at least `min`, every one with that model, token counts and
a cost (derived from evald's price table when the framework reports none). `tool` counts spans
typed TOOL, at least one of them for the named tool under `gen_ai.tool.name`, the key evald
normalises every framework's spelling to. It polls evald's GET /v1/spans until every
expectation holds or the wait runs out, then prints what it saw. Exit 0 pass, 1 fail.

What it checks is evald's reading of the telemetry, not the framework's: a span typed wrong,
usage counted twice, or a tool known only by an alias fails here.
"""

import argparse
import json
import sys
import time
import urllib.request


def tool_name(span):
    name = (span.get("raw_attributes") or {}).get("gen_ai.tool.name")
    return name if isinstance(name, str) else None


def spans(url, service):
    with urllib.request.urlopen(f"{url}/v1/spans?limit=10000", timeout=5) as r:
        got = json.load(r)
    return [s for s in got if service is None or s.get("service_name") == service]


def failures(expect, got):
    out = []
    # Spend lives on the model calls. Usage on any other span (an agent's total of its calls)
    # would be counted twice by every report that sums cost.
    for s in got:
        if s.get("oi_kind") not in ("LLM", "EMBEDDING") and (s.get("cost_usd") or (s.get("tokens") or {}).get("total")):
            out.append(f"{s.get('oi_kind') or 'untyped'} span {s['name']!r} carries usage or cost")
    llm = [s for s in got if s.get("oi_kind") == "LLM"]
    tools = [s for s in got if s.get("oi_kind") == "TOOL"]
    want = expect.get("llm")
    if want:
        if len(llm) < want.get("min", 1):
            out.append(f"{len(llm)} LLM spans, want at least {want.get('min', 1)}")
        for s in llm:
            if want.get("model") and s.get("model") != want["model"]:
                out.append(f"LLM span {s['name']!r} has model {s.get('model')!r}, want {want['model']!r}")
            if want.get("tokens") and not (s.get("tokens") or {}).get("total"):
                out.append(f"LLM span {s['name']!r} has no token counts")
            if want.get("cost") and not s.get("cost_usd"):
                out.append(f"LLM span {s['name']!r} has no cost")
    want = expect.get("tool")
    if want:
        if len(tools) < want.get("min", 1):
            out.append(f"{len(tools)} TOOL spans, want at least {want.get('min', 1)}")
        if want.get("name") and want["name"] not in {tool_name(s) for s in tools}:
            out.append(f"no TOOL span for tool {want['name']!r}")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", required=True)
    ap.add_argument("--expect", required=True)
    args = ap.parse_args()
    with open(args.expect) as f:
        expect = json.load(f)
    deadline = time.monotonic() + expect.get("wait_seconds", 15)
    while True:
        got = spans(args.url, expect.get("service"))
        bad = failures(expect, got)
        if not bad or time.monotonic() > deadline:
            break
        time.sleep(0.5)

    print(f"{'kind':<8} {'name':<40} {'model':<18} {'tokens':>7} {'cost_usd':>10}  tool")
    for s in sorted(got, key=lambda s: s.get("start_unix_nano", 0)):
        tokens = (s.get("tokens") or {}).get("total")
        cost = s.get("cost_usd")
        print(f"{s.get('oi_kind') or '-':<8} {s['name'][:40]:<40} {(s.get('model') or '-')[:18]:<18} "
              f"{tokens if tokens is not None else '-':>7} {f'{cost:.6f}' if cost else '-':>10}  "
              f"{tool_name(s) or ''}")
    if bad:
        print("FAIL:", *bad, sep="\n  ")
        sys.exit(1)
    print(f"ok: {len(got)} spans")


if __name__ == "__main__":
    main()
