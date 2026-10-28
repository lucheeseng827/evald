#!/usr/bin/env python3
"""A stand-in model provider for the recipes: no key, no network, deterministic answers.

    python3 stub_provider.py --port 9100

It speaks enough of three APIs for an agent framework to run a full turn against it:

* OpenAI Chat Completions  POST /v1/chat/completions   (plain and streamed, tool calls)
* OpenAI Responses         POST /v1/responses          (plain and streamed, function calls)
* Anthropic Messages       POST /v1/messages           (plain and streamed, tool use)
* Gemini                   POST /v1beta/models/{m}:generateContent, :streamGenerateContent

The turn it plays is the one every recipe asserts on. When the request offers tools and no tool
result has come back yet, it calls the first tool (arguments filled from the tool's JSON schema);
once a result is in the conversation, it answers in text. Every response reports token usage
(120 in, 24 out), so the spans the frameworks emit carry tokens, and evald prices them from its
bundled table when the model name is one it knows.

Standard library only: the recipes' CI job runs it before anything is installed.
"""

import argparse
import itertools
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

INPUT_TOKENS = 120
OUTPUT_TOKENS = 24
ANSWER = "It is sunny in Paris, 21 degrees."
_ids = itertools.count(1)
# STUB_DUMP=<dir>: write every request received there, to see what a client sends.
DUMP_DIR = os.environ.get("STUB_DUMP")
_dumps = itertools.count(1)


def next_id(prefix):
    return f"{prefix}_stub{next(_ids):06d}"


def fill_arguments(schema):
    """Arguments for a tool call: every required property, given a plausible value."""
    props = (schema or {}).get("properties") or {}
    required = (schema or {}).get("required") or list(props)
    args = {}
    for name in required:
        kind = (props.get(name) or {}).get("type")
        if kind in ("integer", "number"):
            args[name] = 1
        elif kind == "boolean":
            args[name] = True
        elif kind == "array":
            args[name] = []
        elif kind == "object":
            args[name] = {}
        else:
            args[name] = "Paris"
    return args


# --- OpenAI Chat Completions ----------------------------------------------------------------

def chat_tool(req):
    """The tool to call, or None to answer: tools offered and no tool result since the last user turn."""
    tools = req.get("tools") or []
    if not tools:
        return None
    for msg in reversed(req.get("messages") or []):
        if msg.get("role") == "tool":
            return None
        if msg.get("role") == "user":
            break
    fn = tools[0].get("function") or {}
    return fn.get("name"), fill_arguments(fn.get("parameters"))


def chat_usage():
    return {
        "prompt_tokens": INPUT_TOKENS,
        "completion_tokens": OUTPUT_TOKENS,
        "total_tokens": INPUT_TOKENS + OUTPUT_TOKENS,
        "prompt_tokens_details": {"cached_tokens": 0},
        "completion_tokens_details": {"reasoning_tokens": 0},
    }


def chat_completion(req):
    model = req.get("model", "gpt-4o-mini")
    tool = chat_tool(req)
    if tool:
        message = {
            "role": "assistant",
            "content": None,
            "tool_calls": [{
                "id": next_id("call"),
                "type": "function",
                "function": {"name": tool[0], "arguments": json.dumps(tool[1])},
            }],
        }
        finish = "tool_calls"
    else:
        message = {"role": "assistant", "content": ANSWER}
        finish = "stop"
    return {
        "id": next_id("chatcmpl"),
        "object": "chat.completion",
        "created": int(time.time()),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish, "logprobs": None}],
        "usage": chat_usage(),
    }


def chat_chunks(req):
    """The same completion as a stream: role, content (or the tool call), finish, then usage."""
    full = chat_completion(req)
    base = {k: full[k] for k in ("id", "created", "model")}
    base["object"] = "chat.completion.chunk"
    msg = full["choices"][0]["message"]

    def chunk(delta, finish=None):
        return dict(base, choices=[{"index": 0, "delta": delta, "finish_reason": finish}])

    yield chunk({"role": "assistant", "content": ""})
    if msg.get("tool_calls"):
        call = msg["tool_calls"][0]
        yield chunk({"tool_calls": [{"index": 0, "id": call["id"], "type": "function",
                                     "function": {"name": call["function"]["name"], "arguments": ""}}]})
        yield chunk({"tool_calls": [{"index": 0, "function": {"arguments": call["function"]["arguments"]}}]})
    else:
        yield chunk({"content": msg["content"]})
    yield chunk({}, full["choices"][0]["finish_reason"])
    if (req.get("stream_options") or {}).get("include_usage"):
        yield dict(base, choices=[], usage=full["usage"])


# --- OpenAI Responses -------------------------------------------------------------------------

# Codex's toolbox: `exec_command` and the client-side `tool_search` that finds deferred MCP tools.
CODEX_OUTPUTS = ("function_call_output", "custom_tool_call_output", "tool_search_output")


def codex_output(req, items):
    """The turn the coding-agent capture plays against Codex, one step per model call.

    The first turn of a conversation runs a shell command, searches for the MCP tool and calls
    it, then runs a command that fails, then answers. A later turn calls the MCP tool found
    earlier once, then answers. Returns the output items and the cached input tokens to report
    (0 on a turn's first call, as a real cache is cold then).
    """
    last_user = max((n for n, i in enumerate(items)
                     if i.get("type") == "message" and i.get("role") == "user"), default=-1)
    earlier, turn = items[:last_user], items[last_user + 1:]
    step = sum(1 for i in turn if i.get("type") in CODEX_OUTPUTS)
    cached = 64 if step else 0
    found = next((i for i in reversed(items) if i.get("type") == "tool_search_output"), None)
    searchable = any(t.get("type") == "tool_search" for t in req.get("tools") or [])

    def shell(cmd):
        return [{"type": "function_call", "id": next_id("fc"), "call_id": next_id("call"),
                 "name": "exec_command", "arguments": json.dumps({"cmd": cmd}),
                 "status": "completed"}]

    def mcp_call():
        for tool in (found or {}).get("tools") or []:
            inner = tool.get("tools") if tool.get("type") == "namespace" else [tool]
            for t in inner or []:
                call = {"type": "function_call", "id": next_id("fc"), "call_id": next_id("call"),
                        "name": t["name"], "status": "completed",
                        "arguments": json.dumps(fill_arguments(t.get("parameters")))}
                if tool.get("type") == "namespace":
                    call["namespace"] = tool["name"]
                return [call]
        return None

    later = any(i.get("type") == "function_call" for i in earlier)
    call = mcp_call() if step == (0 if later else 2) else None
    if later:
        # A later turn: the MCP tool once, then the answer.
        if call:
            return call, cached
    elif step == 0:
        return shell("echo evald"), cached
    elif step == 1 and searchable:
        return [{"type": "tool_search_call", "id": next_id("ts"), "call_id": next_id("call"),
                 "status": "completed", "execution": "client",
                 "arguments": {"query": "weather", "limit": 8}}], cached
    elif step == 2 and call:
        return call, cached
    elif step <= 3:
        return shell("exit 3"), cached
    return [{
        "type": "message", "id": next_id("msg"), "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": ANSWER, "annotations": []}],
    }], cached


def response(req):
    model = req.get("model", "gpt-4o-mini")
    items = req.get("input")
    items = items if isinstance(items, list) else []
    items = [i for i in items if isinstance(i, dict)]
    answered = any(i.get("type") == "function_call_output" for i in items)
    tools = [t for t in (req.get("tools") or []) if t.get("type") == "function"]
    cached = 0
    if any(t.get("name") == "exec_command" for t in tools):
        output, cached = codex_output(req, items)
    elif tools and not answered:
        output = [{
            "type": "function_call",
            "id": next_id("fc"),
            "call_id": next_id("call"),
            "name": tools[0]["name"],
            "arguments": json.dumps(fill_arguments(tools[0].get("parameters"))),
            "status": "completed",
        }]
    else:
        output = [{
            "type": "message",
            "id": next_id("msg"),
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": ANSWER, "annotations": []}],
        }]
    return {
        "id": next_id("resp"),
        "object": "response",
        "created_at": int(time.time()),
        "model": model,
        "status": "completed",
        "output": output,
        "parallel_tool_calls": True,
        "tool_choice": "auto",
        "tools": req.get("tools") or [],
        "usage": {
            "input_tokens": INPUT_TOKENS,
            "output_tokens": OUTPUT_TOKENS,
            "total_tokens": INPUT_TOKENS + OUTPUT_TOKENS,
            "input_tokens_details": {"cached_tokens": cached},
            "output_tokens_details": {"reasoning_tokens": 0},
        },
    }


def response_events(req):
    """The same response as a stream, in the order the Responses API sends it."""
    full = response(req)
    seq = itertools.count()

    def ev(kind, **fields):
        return kind, dict({"type": kind, "sequence_number": next(seq)}, **fields)

    yield ev("response.created", response=dict(full, status="in_progress", output=[], usage=None))
    for i, item in enumerate(full["output"]):
        if item["type"] == "message":
            text = item["content"][0]["text"]
            yield ev("response.output_item.added", output_index=i,
                     item=dict(item, status="in_progress", content=[]))
            yield ev("response.output_text.delta", item_id=item["id"], output_index=i,
                     content_index=0, delta=text)
            yield ev("response.output_text.done", item_id=item["id"], output_index=i,
                     content_index=0, text=text)
        elif item["type"] == "function_call":
            yield ev("response.output_item.added", output_index=i,
                     item=dict(item, status="in_progress", arguments=""))
            yield ev("response.function_call_arguments.done", item_id=item["id"],
                     output_index=i, arguments=item["arguments"])
        else:
            yield ev("response.output_item.added", output_index=i, item=item)
        yield ev("response.output_item.done", output_index=i, item=item)
    yield ev("response.completed", response=full)


# --- Anthropic Messages -----------------------------------------------------------------------

def anthropic_tool(req):
    tools = req.get("tools") or []
    if not tools:
        return None
    # Claude Code offers its whole toolbox; a recipe's own tools are the MCP ones. Prefer those,
    # so the stub never asks the agent to run a shell command.
    mine = [t for t in tools if str(t.get("name", "")).startswith("mcp__")]
    if not mine and len(tools) > 1:
        return None
    tool = (mine or tools)[0]
    for msg in reversed(req.get("messages") or []):
        content = msg.get("content")
        if isinstance(content, list) and any(
            isinstance(b, dict) and b.get("type") == "tool_result" for b in content
        ):
            return None
        if msg.get("role") == "user":
            break
    return tool["name"], fill_arguments(tool.get("input_schema"))


def anthropic_message(req):
    tool = anthropic_tool(req)
    if tool:
        content = [{"type": "tool_use", "id": next_id("toolu"), "name": tool[0], "input": tool[1]}]
        stop = "tool_use"
    else:
        content = [{"type": "text", "text": ANSWER}]
        stop = "end_turn"
    return {
        "id": next_id("msg"),
        "type": "message",
        "role": "assistant",
        "model": req.get("model", "claude-sonnet-5"),
        "content": content,
        "stop_reason": stop,
        "stop_sequence": None,
        "usage": {
            "input_tokens": INPUT_TOKENS,
            "output_tokens": OUTPUT_TOKENS,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        },
    }


def anthropic_events(req):
    msg = anthropic_message(req)
    start = dict(msg, content=[], stop_reason=None, usage=dict(msg["usage"], output_tokens=1))
    yield "message_start", {"type": "message_start", "message": start}
    for i, block in enumerate(msg["content"]):
        if block["type"] == "text":
            yield "content_block_start", {"type": "content_block_start", "index": i,
                                          "content_block": {"type": "text", "text": ""}}
            yield "content_block_delta", {"type": "content_block_delta", "index": i,
                                          "delta": {"type": "text_delta", "text": block["text"]}}
        else:
            yield "content_block_start", {"type": "content_block_start", "index": i,
                                          "content_block": dict(block, input={})}
            yield "content_block_delta", {"type": "content_block_delta", "index": i,
                                          "delta": {"type": "input_json_delta",
                                                    "partial_json": json.dumps(block["input"])}}
        yield "content_block_stop", {"type": "content_block_stop", "index": i}
    yield "message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": msg["stop_reason"], "stop_sequence": None},
                            "usage": {"output_tokens": OUTPUT_TOKENS}}
    yield "message_stop", {"type": "message_stop"}


# --- Gemini (Generative Language API) ---------------------------------------------------------

def gemini_parts(req):
    """The parts of the model's reply: the turn the coding-agent capture plays against Gemini CLI.

    Gemini CLI offers its shell tool only when it may run it (`--approval-mode yolo`). A first
    turn runs a shell command if it can, calls the MCP tool (`mcp_<server>_<tool>`), runs a
    command that fails, then answers. A later turn runs a shell command whether or not it was
    offered (so a CLI that may not run it refuses it), then answers.
    """
    contents = [c for c in req.get("contents") or [] if isinstance(c, dict)]

    def parts(c):
        return [p for p in c.get("parts") or [] if isinstance(p, dict)]

    last_user = max((n for n, c in enumerate(contents)
                     if c.get("role") == "user" and any("text" in p for p in parts(c))),
                    default=-1)
    earlier, turn = contents[:last_user], contents[last_user + 1:]
    step = sum(1 for c in turn for p in parts(c) if "functionResponse" in p)
    later = any("functionCall" in p for c in earlier for p in parts(c))
    decls = [d for t in req.get("tools") or [] for d in t.get("functionDeclarations") or []]
    names = {d["name"] for d in decls}
    mcp = next((d for d in decls if d["name"].startswith("mcp_")), None)

    def call(name, args):
        return [{"functionCall": {"name": name, "args": args}}]

    script = ([("run_shell_command", {"command": "echo again"})] if later else
              [("run_shell_command", {"command": "echo evald"})] * ("run_shell_command" in names)
              + [(mcp["name"], fill_arguments(mcp.get("parametersJsonSchema")
                                              or mcp.get("parameters")))] * bool(mcp)
              + [("run_shell_command", {"command": "exit 3"})] * ("run_shell_command" in names))
    if step < len(script):
        return call(*script[step])
    return [{"text": ANSWER}]


# Gemini's own token semantics: cached tokens are part of the prompt count, thinking tokens come
# on top of the candidates count (the total is prompt + candidates + thoughts).
GEMINI_THOUGHTS = 16


def gemini_response(req, model):
    later_call = any("functionResponse" in p for c in req.get("contents") or []
                     for p in (c.get("parts") or []) if isinstance(p, dict))
    usage = {
        "promptTokenCount": INPUT_TOKENS,
        "candidatesTokenCount": OUTPUT_TOKENS,
        "thoughtsTokenCount": GEMINI_THOUGHTS,
        "totalTokenCount": INPUT_TOKENS + OUTPUT_TOKENS + GEMINI_THOUGHTS,
    }
    if later_call:
        usage["cachedContentTokenCount"] = 64
    return {
        "candidates": [{
            "content": {"role": "model", "parts": gemini_parts(req)},
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": usage,
        "modelVersion": model,
        "responseId": next_id("gresp"),
    }


# --- HTTP -------------------------------------------------------------------------------------

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("stub: " + (fmt % args) + "\n")

    def _json(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _sse(self, events):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        for event, data in events:
            if event:
                self.wfile.write(f"event: {event}\n".encode())
            self.wfile.write(f"data: {json.dumps(data) if not isinstance(data, str) else data}\n\n".encode())
            self.wfile.flush()
        self.close_connection = True

    def do_GET(self):
        path = self.path.split("?")[0].rstrip("/")
        if path in ("/health", "/healthz"):
            return self._json(200, {"ok": True})
        if path.endswith("/models"):
            return self._json(200, {"object": "list", "data": [
                {"id": m, "object": "model", "owned_by": "stub"}
                for m in ("gpt-4o-mini", "claude-sonnet-5")]})
        return self._json(404, {"error": {"message": f"no route {path}"}})

    def do_POST(self):
        path = self.path.split("?")[0].rstrip("/")
        length = int(self.headers.get("Content-Length") or 0)
        try:
            req = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            return self._json(400, {"error": {"message": "body is not JSON"}})
        if DUMP_DIR:
            with open(os.path.join(DUMP_DIR, f"{next(_dumps):03d}.json"), "w") as f:
                json.dump({"path": self.path, "headers": dict(self.headers), "body": req}, f,
                          indent=1)
        if path.endswith("/chat/completions"):
            if req.get("stream"):
                return self._sse([(None, c) for c in chat_chunks(req)] + [(None, "[DONE]")])
            return self._json(200, chat_completion(req))
        if path.endswith("/responses"):
            if req.get("stream"):
                return self._sse(list(response_events(req)))
            return self._json(200, response(req))
        if ":" in path and "/models/" in path:
            model, method = path.rsplit("/models/", 1)[1].split(":", 1)
            if method == "countTokens":
                return self._json(200, {"totalTokens": INPUT_TOKENS})
            if method == "streamGenerateContent":
                return self._sse([(None, gemini_response(req, model))])
            if method == "generateContent":
                return self._json(200, gemini_response(req, model))
        if path.endswith("/messages/count_tokens"):
            return self._json(200, {"input_tokens": INPUT_TOKENS})
        if path.endswith("/messages"):
            sys.stderr.write(f"stub: tools offered: {[t.get('name') for t in req.get('tools') or []]}\n")
            if req.get("stream"):
                return self._sse(list(anthropic_events(req)))
            return self._json(200, anthropic_message(req))
        return self._json(404, {"error": {"message": f"no route {path}"}})


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=9100)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    sys.stderr.write(f"stub provider on http://{args.host}:{args.port}\n")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
