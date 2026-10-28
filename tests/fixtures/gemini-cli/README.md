# Gemini CLI fixtures

`0.65.0/session.{logs,traces}.json`: one real `gemini -p` session of two turns, captured by
`scripts/capture-coding-agent.sh gemini-cli` against the recipes' stub provider
(`examples/recipes/stub_provider.py`, which speaks the Gemini API and plays the turns) and stub
MCP server (`examples/recipes/stub_mcp.py`). Turn 1 (`--approval-mode yolo`) runs
`echo evald`, calls the MCP tool, runs `exit 3` and answers; turn 2 (`--resume latest`, the
default policy) asks for the shell tool it was not offered, is refused (`tool_not_registered`),
and answers. Six model calls, four tool calls. Captured with `telemetry.traces` on, so the spans
carry what that setting adds.

Scrubbed by `scripts/otlp-fixture.py`: ids are placeholders, the capture directory is `/work`,
the process's command line (`process.command_args`, which holds the `-p` prompt) is blanked,
and strings over 512 characters are cut: Gemini CLI repeats its 31 KB system prompt on every
model call. Every record and span is kept. Metrics are left out (evald takes none).

The mapping is `src/gemini_cli.rs`; its tests read these files.
