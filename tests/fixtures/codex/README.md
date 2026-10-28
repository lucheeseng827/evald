# Codex CLI fixtures

`0.163.0/session.{logs,traces}.json`: one real `codex exec` session of two turns, captured by
`scripts/capture-coding-agent.sh codex` against the recipes' stub provider
(`examples/recipes/stub_provider.py`, which plays the turns) and stub MCP server
(`examples/recipes/stub_mcp.py`). Turn 1 runs `echo evald`, finds the MCP tool with
`tool_search`, calls it, runs `exit 3` and answers; turn 2 (`codex exec resume --last`) calls the
MCP tool under the default approval policy, which refuses it, and answers. Seven model calls,
five tool calls.

Scrubbed by `scripts/otlp-fixture.py`: ids are placeholders (`session-fixture-1`,
`tool-fixture-N`, …), the capture directory is `/work`, strings over 512 characters are cut.
**Traces are trimmed**: Codex exports some 750 spans a turn (file reads, auth, tool routing);
the fixture keeps the ones on a turn's path (`--keep-spans` in the capture script lists them).
The logs are whole. Metrics are left out (evald takes none).

The mapping is `src/codex.rs`; its tests read these files.
