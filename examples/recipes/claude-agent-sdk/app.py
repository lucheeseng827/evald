"""Claude Agent SDK -> evald: an agent with one in-process MCP tool, on Claude Code's telemetry.

The Agent SDK runs Claude Code, and Claude Code reports each model call, tool call and prompt as
an OpenTelemetry log event; evald maps those into spans (docs/INSTRUMENTATION.md, Coding
agents). So the integration is environment only: the block below, which `evald connect
claude-code` prints for your evald. Run it with examples/recipes/run.sh claude-agent-sdk, or:

    OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 python app.py
"""

import asyncio
import os

from claude_agent_sdk import ClaudeAgentOptions, create_sdk_mcp_server, query, tool

TELEMETRY = {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "OTEL_LOGS_EXPORTER": "otlp",
    "OTEL_METRICS_EXPORTER": "none",
    "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
    "OTEL_EXPORTER_OTLP_ENDPOINT": os.environ["OTEL_EXPORTER_OTLP_ENDPOINT"],
    "OTEL_LOGS_EXPORT_INTERVAL": "1000",
    # Name MCP tools (and log tool arguments) in the events; without it every MCP tool is
    # reported as `mcp_tool`.
    "OTEL_LOG_TOOL_DETAILS": "1",
    # Nothing but the model calls and the telemetry leaves the process.
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    # Offer the MCP tool to the model directly rather than behind a tool search, so this
    # one-tool agent calls it on its first turn.
    "ENABLE_TOOL_SEARCH": "false",
}


@tool("get_weather", "The current weather in a city.", {"city": str})
async def get_weather(args):
    return {"content": [{"type": "text", "text": f"sunny, 21 degrees in {args['city']}"}]}


async def main():
    options = ClaudeAgentOptions(
        model="claude-sonnet-5",
        tools=[],  # no built-in tools: this agent has only its own
        mcp_servers={"weather": create_sdk_mcp_server("weather", tools=[get_weather])},
        allowed_tools=["mcp__weather__get_weather"],
        env=TELEMETRY,
        max_turns=3,
    )
    async for message in query(prompt="What is the weather in Paris?", options=options):
        if type(message).__name__ == "ResultMessage":
            print(message.result)
    await asyncio.sleep(2)  # Claude Code exports its last events on exit; give evald a moment


asyncio.run(main())
