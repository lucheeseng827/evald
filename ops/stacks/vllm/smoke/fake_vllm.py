"""A stand-in for vLLM in the stack's smoke test, where there is no GPU: an OpenAI-compatible
chat endpoint that, like vLLM with --otlp-traces-endpoint, exports an `llm_request` SERVER span
per request (parented by the request's `traceparent`), and a /metrics with vLLM's series names.
The span's shape is vLLM's (vllm/v1/engine/output_processor.py, do_tracing), not a new one."""
import json
import os
import random
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COLLECTOR = os.environ.get("COLLECTOR", "http://otel-collector:4318/v1/traces")
MODEL = os.environ.get("SERVED_NAME", "Qwen2.5-7B-Instruct")
served = {"requests": 0, "prompt": 0, "generated": 0}


def attr(k, v):
    if isinstance(v, float):
        return {"key": k, "value": {"doubleValue": v}}
    if isinstance(v, int):
        return {"key": k, "value": {"intValue": str(v)}}
    return {"key": k, "value": {"stringValue": v}}


def export_span(traceparent, start_ns, end_ns, prompt, completion):
    parts = (traceparent or "").split("-")
    trace_id = parts[1] if len(parts) == 4 else os.urandom(16).hex()
    parent = parts[2] if len(parts) == 4 else ""
    e2e = (end_ns - start_ns) / 1e9
    span = {
        "traceId": trace_id,
        "spanId": os.urandom(8).hex(),
        "parentSpanId": parent,
        "name": "llm_request",
        "kind": 2,
        "startTimeUnixNano": str(start_ns),
        "endTimeUnixNano": str(end_ns),
        "attributes": [
            attr("gen_ai.latency.time_to_first_token", e2e * 0.3),
            attr("gen_ai.latency.e2e", e2e),
            attr("gen_ai.latency.time_in_queue", e2e * 0.05),
            attr("gen_ai.usage.prompt_tokens", prompt),
            attr("gen_ai.usage.completion_tokens", completion),
            attr("gen_ai.latency.time_in_model_prefill", e2e * 0.25),
            attr("gen_ai.latency.time_in_model_decode", e2e * 0.7),
            attr("gen_ai.latency.time_in_model_inference", e2e * 0.95),
            attr("gen_ai.request.id", "cmpl-" + os.urandom(6).hex()),
        ],
    }
    body = json.dumps({"resourceSpans": [{
        "resource": {"attributes": [attr("service.name", "vllm")]},
        "scopeSpans": [{"scope": {"name": "vllm.llm_engine"}, "spans": [span]}],
    }]}).encode()
    req = urllib.request.Request(COLLECTOR, body, {"content-type": "application/json"})
    urllib.request.urlopen(req, timeout=10).read()


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, body, ctype="application/json"):
        b = body.encode() if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path == "/metrics":
            lbl = f'model_name="{MODEL}",engine="0"'
            self.send(200, "\n".join([
                "# TYPE vllm:num_requests_running gauge", f"vllm:num_requests_running{{{lbl}}} 0",
                "# TYPE vllm:num_requests_waiting gauge", f"vllm:num_requests_waiting{{{lbl}}} 0",
                "# TYPE vllm:kv_cache_usage_perc gauge", f"vllm:kv_cache_usage_perc{{{lbl}}} 0.12",
                "# TYPE vllm:prompt_tokens_total counter", f"vllm:prompt_tokens_total{{{lbl}}} {served['prompt']}",
                "# TYPE vllm:generation_tokens_total counter", f"vllm:generation_tokens_total{{{lbl}}} {served['generated']}",
            ]) + "\n", "text/plain; version=0.0.4")
        elif self.path == "/v1/models":
            self.send(200, json.dumps({"data": [{"id": MODEL, "root": "Qwen/Qwen2.5-7B-Instruct"}]}))
        else:
            self.send(200 if self.path == "/health" else 404, "{}")

    def do_POST(self):
        start = time.time_ns()
        self.rfile.read(int(self.headers.get("content-length", 0)))
        time.sleep(random.uniform(0.05, 0.15))
        prompt, completion = 40, 12
        served["requests"] += 1
        served["prompt"] += prompt
        served["generated"] += completion
        self.send(200, json.dumps({
            "model": MODEL,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "{\"score\": 1.0}"}}],
            "usage": {"prompt_tokens": prompt, "completion_tokens": completion},
        }))
        export_span(self.headers.get("traceparent"), start, time.time_ns(), prompt, completion)


ThreadingHTTPServer(("0.0.0.0", 8000), H).serve_forever()
