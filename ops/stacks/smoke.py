#!/usr/bin/env python3
"""Smoke-test a reference stack: bring it up with one command, drive it, assert what the stack
is for, and tear it down.

    python3 ops/stacks/smoke.py vllm-cpu [--image evald:judge] [--keep]
    python3 ops/stacks/smoke.py ollama   [--image evald:judge] [--model qwen2.5:0.5b] [--keep]

vllm-cpu  The vLLM stack with vLLM replaced by a CPU stand-in that emits vLLM's span and metric
          shapes (vllm/smoke/fake_vllm.py): app calls carrying `traceparent` -> vLLM's spans ->
          the collector -> evald, with each server span under the app's; Prometheus scraping
          evald and vLLM; the board provisioned in Grafana.
ollama    The Ollama stack for real: a small model pulled, `evald judge probe` on it, and an
          `eval run` judged by it.

`--image` names an evald image already built (with `--features judge`); without it compose
builds one from this checkout. Standard library only.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def log(msg):
    print(f"smoke: {msg}", flush=True)


def fail(msg):
    print(f"smoke: FAIL {msg}", file=sys.stderr, flush=True)
    raise SystemExit(1)


def http(method, url, body=None, headers=None, timeout=10):
    data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
    req = urllib.request.Request(url, data, {"content-type": "application/json", **(headers or {})},
                                 method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
    except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
        return None, ""


def until(what, check, timeout=180, every=2):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = check()
        if last:
            return last
        time.sleep(every)
    fail(f"timed out after {timeout}s waiting for {what}")


class Stack:
    def __init__(self, name, files, image):
        self.dir = os.path.join(HERE, name)
        self.args = ["docker", "compose", "-p", f"evald-smoke-{name}"]
        for f in files:
            self.args += ["-f", f]
        self.env = dict(os.environ)
        if image:
            self.env["EVALD_IMAGE"] = image

    def run(self, *args, check=True, capture=False):
        r = subprocess.run(self.args + list(args), cwd=self.dir, env=self.env, text=True, check=False,
                           capture_output=capture)
        if check and r.returncode != 0:
            fail(f"`docker compose {' '.join(args)}` exited {r.returncode}\n{r.stdout or ''}{r.stderr or ''}")
        return r

    def up(self, build):
        self.run("up", "-d", *(["--build"] if build else ["--no-build"]))

    def down(self):
        self.run("logs", "--no-color", "--tail", "40", check=False)
        self.run("down", "-v", "--remove-orphans", check=False)


def otlp_client_span(trace_id, span_id, start, end):
    kv = lambda k, v: {"key": k, "value": ({"intValue": str(v)} if isinstance(v, int) else {"stringValue": v})}
    return {"resourceSpans": [{
        "resource": {"attributes": [kv("service.name", "smoke-app")]},
        "scopeSpans": [{"spans": [{
            "traceId": trace_id, "spanId": span_id, "name": "chat Qwen2.5-7B-Instruct", "kind": 3,
            "startTimeUnixNano": str(start), "endTimeUnixNano": str(end),
            "attributes": [kv("gen_ai.operation.name", "chat"),
                           kv("gen_ai.request.model", "Qwen2.5-7B-Instruct"),
                           kv("gen_ai.usage.input_tokens", 40), kv("gen_ai.usage.output_tokens", 12)],
        }]}],
    }]}


def sql(q):
    code, body = http("POST", "http://127.0.0.1:4320/v1/sql", {"sql": q})
    if code != 200:
        return []
    return json.loads(body)["rows"]


def smoke_vllm_cpu(stack):
    until("evald ready", lambda: http("GET", "http://127.0.0.1:4320/readyz")[0] == 200)
    until("the fake vLLM", lambda: http("GET", "http://127.0.0.1:8000/health")[0] == 200)
    until("the collector", lambda: http("POST", "http://127.0.0.1:4318/v1/traces",
                                        {"resourceSpans": []})[0] == 200)
    calls = 5
    ids = []
    for _ in range(calls):
        trace_id, span_id = os.urandom(16).hex(), os.urandom(8).hex()
        start = time.time_ns()
        code, _ = http("POST", "http://127.0.0.1:8000/v1/chat/completions",
                       {"model": "Qwen2.5-7B-Instruct", "messages": [{"role": "user", "content": "hi"}]},
                       {"traceparent": f"00-{trace_id}-{span_id}-01"})
        if code != 200:
            fail(f"the model call returned {code}")
        end = time.time_ns()
        code, _ = http("POST", "http://127.0.0.1:4318/v1/traces", otlp_client_span(trace_id, span_id, start, end))
        if code != 200:
            fail(f"the collector refused the app's span: {code}")
        ids.append(span_id)
    log(f"{calls} app calls made, each with traceparent")

    joined = (
        "SELECT count(*) AS n FROM spans s JOIN spans c "
        "ON c.trace_id = s.trace_id AND c.span_id = s.parent_span_id "
        "WHERE strpos(s.raw_attributes_json, '\"evald.server\":\"vllm\"') > 0 "
        "AND c.service_name = 'smoke-app'"
    )
    until("vLLM's spans under the app's in evald",
          lambda: (sql(joined) or [{"n": 0}])[0]["n"] == calls, timeout=120)
    log(f"evald holds {calls} vLLM server spans, each a child of the app's span")
    copies = sql("SELECT count(*) AS n FROM spans WHERE strpos(raw_attributes_json, "
                 "'\"evald.usage\":\"copy\"') > 0")
    if copies[0]["n"] != calls:
        fail(f"expected the {calls} server spans' usage set aside as copies: {copies}")
    tokens = sql("SELECT sum(total_tokens) AS t FROM spans")
    if tokens[0]["t"] != calls * 52:
        fail(f"tokens counted {tokens[0]['t']}, not {calls * 52} (once per call)")
    log("each call's tokens counted once")

    def targets_up():
        code, body = http("GET", "http://127.0.0.1:9090/api/v1/targets")
        if code != 200:
            return False
        health = {t["labels"]["job"]: t["health"] for t in json.loads(body)["data"]["activeTargets"]}
        return health.get("evald") == "up" and health.get("vllm") == "up"
    until("Prometheus scraping evald and vLLM", targets_up, timeout=120)
    code, body = http("GET", "http://127.0.0.1:9090/api/v1/query?query=vllm:kv_cache_usage_perc")
    if code != 200 or not json.loads(body)["data"]["result"]:
        fail(f"no vLLM series in Prometheus: {body[:200]}")
    log("Prometheus scrapes both, vLLM's series included")

    def board():
        code, body = http("GET", "http://127.0.0.1:3000/api/dashboards/uid/evald-vllm")
        return code == 200 and json.loads(body)["dashboard"]["title"] == "evald + vLLM"
    until("the board in Grafana", board, timeout=120)
    code, body = http("GET", "http://127.0.0.1:3000/api/datasources/uid/prometheus/health")
    if code != 200 or json.loads(body).get("status") != "OK":
        fail(f"Grafana cannot reach Prometheus: {code} {body[:200]}")
    log("the board is provisioned and its datasource answers")


def smoke_ollama(stack, model):
    until("evald ready", lambda: http("GET", "http://127.0.0.1:4318/readyz")[0] == 200)
    until("Ollama", lambda: http("GET", "http://127.0.0.1:11434/api/version")[0] == 200)
    log(f"pulling {model}")
    stack.run("exec", "-T", "ollama", "ollama", "pull", model)
    probe = stack.run("exec", "-T", "evald", "evald", "judge", "probe", "--base-url",
                      "http://ollama:11434/v1", "--model", model, check=False, capture=True)
    print(probe.stdout)
    if probe.returncode != 0:
        fail(f"judge probe exited {probe.returncode}:\n{probe.stderr}")
    log("judge probe: the endpoint can judge")
    # An eval run needs a data dir of its own (the server holds /data's lock): /data/evals,
    # inside the volume evald's user owns. The config, with the smoke's model, goes in beside it.
    # `compose cp` lands the files as root with the modes they have here, and mkdtemp's is 0700:
    # evald (nonroot) could not enter the directory, so it and the files are opened up first.
    tmp = tempfile.mkdtemp()
    try:
        with open(os.path.join(stack.dir, "judge.yaml")) as f:
            cfg = f.read().replace("model: qwen2.5:7b", f"model: {model}")
        with open(os.path.join(tmp, "judge.yaml"), "w") as f:
            f.write(cfg)
        shutil.copy(os.path.join(stack.dir, "dataset.jsonl"), tmp)
        os.chmod(tmp, 0o755)
        for name in os.listdir(tmp):
            os.chmod(os.path.join(tmp, name), 0o644)
        stack.run("cp", f"{tmp}/.", "evald:/data/smoke")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    run = stack.run("exec", "-T", "evald", "evald", "eval", "run", "--config",
                    "/data/smoke/judge.yaml", "--data-dir", "/data/evals", check=False, capture=True)
    print(run.stdout)
    # A small model may well miss the threshold (exit 1); what is smoke-tested is that it judged.
    if run.returncode not in (0, 1) or "judge_qa_correctness" not in run.stdout:
        fail(f"eval run exited {run.returncode}:\n{run.stderr}")
    log("eval run judged the dataset on the stack's Ollama")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("stack", choices=["vllm-cpu", "ollama"])
    ap.add_argument("--image", help="an evald image built with --features judge")
    ap.add_argument("--model", default="qwen2.5:0.5b", help="(ollama) the model to pull and judge with")
    ap.add_argument("--keep", action="store_true", help="leave the stack running")
    a = ap.parse_args()
    if a.stack == "vllm-cpu":
        stack = Stack("vllm", ["compose.yaml", "smoke/compose.smoke.yaml"], a.image)
    else:
        stack = Stack("ollama", ["compose.yaml"], a.image)
    log(f"bringing up the {a.stack} stack")
    stack.up(build=not a.image)
    try:
        if a.stack == "vllm-cpu":
            smoke_vllm_cpu(stack)
        else:
            smoke_ollama(stack, a.model)
    finally:
        if not a.keep:
            stack.down()
    log(f"{a.stack}: PASS")


if __name__ == "__main__":
    main()
