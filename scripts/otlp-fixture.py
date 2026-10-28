#!/usr/bin/env python3
"""Capture real OTLP output from a tool and turn it into a test fixture.

Dialect mappings (Claude Code, and later Codex and Gemini CLI) are tested against what the tool
actually sends, not what its docs say it sends. This is how those fixtures are made:

  1. Receive. A minimal OTLP/HTTP sink that saves every request body to a directory:

       scripts/otlp-fixture.py sink --port 4318 --out /tmp/capture

     Point the tool at it with OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 and
     OTEL_EXPORTER_OTLP_PROTOCOL=http/json, run it, stop the sink.

  2. Scrub and merge. Every identifier that could name a person, an account, a machine or a
     real request is replaced with a stable placeholder (the same input gets the same
     placeholder throughout, so correlations survive), and each signal's requests are merged
     into one export request:

       scripts/otlp-fixture.py scrub /tmp/capture tests/fixtures/claude-code/<version> \\
         --name traced

     writes <name>.logs.json and <name>.traces.json (and .metrics.json if any arrived).

`--path DIR` replaces a local path (the capture's working directory) wherever it appears, and
`--keep-spans a,b` keeps only the spans named: Codex traces its own internals (hundreds of
filesystem and auth spans a turn), which no mapping reads. Say in the fixture's README what was
kept. `--max-string N` cuts long string values (Gemini CLI repeats its 31 KB system prompt on
every model call); a cut value ends in a marker saying so.

Read the scrubbed output before committing it. The scrubber knows the identifier attributes the
tools in tests/fixtures send today; a new tool version can add one it does not know.
"""

import argparse
import glob
import gzip
import hashlib
import http.server
import itertools
import json
import os
import sys

# Attribute keys whose values identify a person, an account, a machine, a session or a request.
# Each maps to a placeholder prefix; values are replaced consistently across a whole capture.
SCRUB_KEYS = {
    "user.email": "email",
    "user.id": "user",
    "user.account_uuid": "account",
    "user.account_id": "account",
    "organization.id": "org",
    "session.id": "session",
    "gen_ai.conversation.id": "session",
    "ccr.session.id": "session",
    "prompt.id": "prompt",
    "message.uuid": "message",
    "message.id": "message",
    "request_id": "request",
    "gen_ai.response.id": "request",
    "client_request_id": "client-request",
    "request_body_id": "request-body",
    "tool_use_id": "tool",
    "gen_ai.tool.call.id": "tool",
    "plugin_id_hash": "plugin",
    "host.name": "host",
    "os.version": "os",
    "vcs.ref.head.revision": "revision",
    # Codex CLI (0.163.0): its conversation is its thread; every turn, call and approval has an id.
    "conversation.id": "session",
    "thread_id": "session",
    "turn.id": "turn",
    "turn_id": "turn",
    "root_turn_id": "turn",
    "submission.id": "submission",
    "call_id": "tool",
    "tool.call_id": "tool",
    "approval_id": "approval",
    "cwd": "path",
    # Gemini CLI (0.65.0).
    "installation.id": "installation",
    "prompt_id": "prompt",
    "gen_ai.tool.call_id": "tool",
}
# Attributes whose whole value is content, not an id: blanked. Node's resource detector exports
# the command line, and Gemini CLI's carries a `-p` prompt.
BLANK_KEYS = {"process.command_args", "process.command", "process.owner"}
BLANK = "[blanked by otlp-fixture]"
# Ids worth replacing wherever they appear inside another string (a debug-formatted struct, a
# message): every value scrubbed by key at least this long.
MIN_EMBEDDED = 8
# Ids carried as OTLP fields rather than attributes.
ID_FIELDS = {"traceId": 32, "spanId": 16, "parentSpanId": 16}


# Appended to a string cut by --max-string.
TRUNCATED = "...[cut by otlp-fixture --max-string]"


class Scrubber:
    def __init__(self, max_string=0):
        self.seen = {}
        self.max_string = max_string

    def placeholder(self, kind, value):
        key = (kind, value)
        if key not in self.seen:
            n = sum(1 for k in self.seen if k[0] == kind) + 1
            if kind == "email":
                self.seen[key] = f"dev{n}@example.com"
            else:
                self.seen[key] = f"{kind}-fixture-{n}"
        return self.seen[key]

    def hex_id(self, field, value):
        if not value or set(value) == {"0"}:
            return value
        width = ID_FIELDS[field]
        digest = hashlib.sha256(f"evald-fixture|{value}".encode()).hexdigest()
        return self.seen.setdefault(("hex", value), digest[:width])

    def walk(self, node):
        if isinstance(node, dict):
            if "key" in node and "value" in node and node["key"] in BLANK_KEYS:
                v = node["value"]
                if "stringValue" in v:
                    v["stringValue"] = BLANK
                elif "arrayValue" in v:
                    v["arrayValue"] = {"values": [{"stringValue": BLANK}]}
                return
            if "key" in node and "value" in node and node["key"] in SCRUB_KEYS:
                kind = SCRUB_KEYS[node["key"]]
                v = node["value"]
                if "stringValue" in v and v["stringValue"]:
                    v["stringValue"] = self.placeholder(kind, v["stringValue"])
                return
            for k, v in node.items():
                if k in ID_FIELDS and isinstance(v, str):
                    node[k] = self.hex_id(k, v)
                elif k == "links" and isinstance(v, list):
                    for link in v:
                        self.walk(link)
                else:
                    self.walk(v)
        elif isinstance(node, list):
            for item in node:
                self.walk(item)


    def embedded(self, node, paths):
        """Second pass: ids already scrubbed by key, and local paths, inside any other string."""
        swaps = sorted(
            ((orig, ph) for (kind, orig), ph in self.seen.items()
             if kind != "hex" and len(orig) >= MIN_EMBEDDED),
            key=lambda x: -len(x[0]),
        ) + [(p, "/work") for p in sorted(paths, key=len, reverse=True)]
        self._embedded(node, swaps)

    def _embedded(self, node, swaps):
        if isinstance(node, dict):
            for k, v in node.items():
                if k == "stringValue" and isinstance(v, str):
                    for orig, ph in swaps:
                        if orig in v:
                            v = v.replace(orig, ph)
                    if self.max_string and len(v) > self.max_string:
                        v = v[: self.max_string] + TRUNCATED
                    node[k] = v
                else:
                    self._embedded(v, swaps)
        elif isinstance(node, list):
            for item in node:
                self._embedded(item, swaps)


def keep_spans(body, names):
    """Keep only the spans named (the ones a mapping reads), each exactly as it was. A parent
    left out stays referenced: evald reads such a span as a root, as it does a lost parent."""
    dropped = 0
    for rs in body.get("resourceSpans", []):
        for ss in rs.get("scopeSpans", []):
            keep = [x for x in ss.get("spans", []) if x.get("name") in names]
            dropped += len(ss.get("spans", [])) - len(keep)
            ss["spans"] = keep
    return dropped


SIGNAL_ROOT = {"logs": "resourceLogs", "traces": "resourceSpans", "metrics": "resourceMetrics"}


def scrub(args):
    s = Scrubber(args.max_string)
    merged = {sig: {root: []} for sig, root in SIGNAL_ROOT.items()}
    files = sorted(glob.glob(os.path.join(args.capture, "*.json")))
    if not files:
        sys.exit(f"no *.json captures in {args.capture}")
    for f in files:
        sig = next((x for x in SIGNAL_ROOT if f.endswith(f"_{x}.json")), None)
        if sig is None:
            continue
        body = json.load(open(f))
        merged[sig][SIGNAL_ROOT[sig]].extend(body.get(SIGNAL_ROOT[sig], []))
    os.makedirs(args.out, exist_ok=True)
    signals = [sig for sig, body in merged.items() if body[SIGNAL_ROOT[sig]]
               and not (sig == "metrics" and args.no_metrics)]
    for sig in signals:
        s.walk(merged[sig])
    for sig in signals:
        s.embedded(merged[sig], args.path)
    if args.keep_spans and "traces" in signals:
        n = keep_spans(merged["traces"], set(args.keep_spans.split(",")))
        print(f"left out {n} spans not named in --keep-spans")
    for sig in signals:
        body = merged[sig]
        path = os.path.join(args.out, f"{args.name}.{sig}.json")
        with open(path, "w") as out:
            json.dump(body, out, indent=1, sort_keys=False)
            out.write("\n")
        print(f"wrote {path}")


def sink(args):
    os.makedirs(args.out, exist_ok=True)
    counter = itertools.count()

    class Handler(http.server.BaseHTTPRequestHandler):
        def read_body(self):
            # Node's OTLP exporters send chunked, gzip-compressed bodies (Gemini CLI does).
            if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
                body = b""
                while True:
                    size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                    if size == 0:
                        self.rfile.readline()
                        break
                    body += self.rfile.read(size)
                    self.rfile.readline()
            else:
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            if self.headers.get("Content-Encoding", "").lower() == "gzip":
                body = gzip.decompress(body)
            return body

        def do_POST(self):
            body = self.read_body()
            ct = self.headers.get("Content-Type", "")
            ext = "json" if "json" in ct else "pb"
            signal = self.path.strip("/").replace("/", "_")
            name = f"{next(counter):03d}-{signal}.{ext}"
            with open(os.path.join(args.out, name), "wb") as f:
                f.write(body)
            reply = b"{}" if ext == "json" else b""
            self.send_response(200)
            self.send_header("Content-Type", ct or "application/json")
            self.send_header("Content-Length", str(len(reply)))
            self.end_headers()
            self.wfile.write(reply)

        def log_message(self, *a):
            pass

    print(f"OTLP sink on 127.0.0.1:{args.port}, saving to {args.out}", flush=True)
    http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("sink", help="receive OTLP/HTTP and save each request body")
    a.add_argument("--port", type=int, default=4318)
    a.add_argument("--out", required=True)
    a.set_defaults(func=sink)
    b = sub.add_parser("scrub", help="anonymise and merge a capture into fixtures")
    b.add_argument("capture")
    b.add_argument("out")
    b.add_argument("--name", required=True)
    b.add_argument("--path", action="append", default=[],
                   help="a local path to replace with /work wherever it appears (repeatable)")
    b.add_argument("--keep-spans", default="",
                   help="comma-separated span names to keep, leaving out the rest (a tool's "
                        "internal noise); default: keep all")
    b.add_argument("--no-metrics", action="store_true", help="leave metrics out of the fixture")
    b.add_argument("--max-string", type=int, default=0,
                   help="cut string values longer than this many characters (a system prompt "
                        "repeated on every record); default: keep whole")
    b.set_defaults(func=scrub)
    args = p.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
