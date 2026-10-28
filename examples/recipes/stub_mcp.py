#!/usr/bin/env python3
"""A stand-in MCP server on stdio: one tool, `get_weather`, with a fixed answer.

    python3 stub_mcp.py

The coding-agent captures (scripts/capture-coding-agent.sh) configure it as an MCP server so
the agent's MCP tool calls are real ones: the agent starts this process, lists its tools and
calls one, exactly as it would any MCP server. Newline-delimited JSON-RPC 2.0, the stdio
transport of the MCP spec. Standard library only.
"""

import json
import sys

TOOL = {
    "name": "get_weather",
    "description": "The current weather in a city.",
    "inputSchema": {
        "type": "object",
        "properties": {"city": {"type": "string", "description": "The city."}},
        "required": ["city"],
    },
}


def handle(msg):
    method = msg.get("method")
    if method == "initialize":
        return {
            "protocolVersion": (msg.get("params") or {}).get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "weather", "version": "1.0.0"},
        }
    if method == "tools/list":
        return {"tools": [TOOL]}
    if method == "tools/call":
        city = ((msg.get("params") or {}).get("arguments") or {}).get("city", "Paris")
        return {"content": [{"type": "text", "text": f"Sunny in {city}, 21 degrees."}],
                "isError": False}
    if method == "ping":
        return {}
    raise LookupError(method)


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        if "id" not in msg:
            continue  # a notification (initialized, cancelled): nothing to answer
        try:
            reply = {"jsonrpc": "2.0", "id": msg["id"], "result": handle(msg)}
        except LookupError as e:
            reply = {"jsonrpc": "2.0", "id": msg["id"],
                     "error": {"code": -32601, "message": f"method not found: {e}"}}
        sys.stdout.write(json.dumps(reply) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
