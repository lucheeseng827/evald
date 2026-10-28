#!/bin/sh
# Capture a coding agent's real OTLP output for a dialect fixture: no key, no network.
#
#   scripts/capture-coding-agent.sh codex      tests/fixtures/codex/<version>
#   scripts/capture-coding-agent.sh gemini-cli tests/fixtures/gemini-cli/<version>
#
# The real CLI runs a real session: its model calls go to examples/recipes/stub_provider.py
# (which plays a scripted turn: a shell command, the MCP tool, a command that fails, then the
# answer), its MCP calls to examples/recipes/stub_mcp.py, and its telemetry to the fixture sink
# (scripts/otlp-fixture.py). The capture is then scrubbed into <out>/session.{logs,traces}.json.
#
# The CLI is found on PATH, or set CODEX= / GEMINI= to its path. Ports: STUB_PORT (19100) and
# SINK_PORT (14319). Read the scrubbed fixture before committing it (otlp-fixture.py says why).
set -eu

tool=${1:?usage: capture-coding-agent.sh codex|gemini-cli <out-dir>}
out=${2:?usage: capture-coding-agent.sh codex|gemini-cli <out-dir>}
here=$(cd "$(dirname "$0")/.." && pwd)
stub_port=${STUB_PORT:-19100}
sink_port=${SINK_PORT:-14319}
work=$(mktemp -d)
pids=""
cleanup() {
    for p in $pids; do kill "$p" 2>/dev/null || true; done
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

# The session must start its own trace: a TRACEPARENT inherited from the shell (a CI job, an
# agent running this script) would parent every span to someone else's.
unset TRACEPARENT TRACESTATE

mkdir -p "$work/sink" "$work/project"
python3 "$here/examples/recipes/stub_provider.py" --port "$stub_port" 2>"$work/stub.log" &
pids="$pids $!"
python3 "$here/scripts/otlp-fixture.py" sink --port "$sink_port" --out "$work/sink" >/dev/null &
pids="$pids $!"
sleep 1

capture_codex() {
    codex=${CODEX:-codex}
    "$codex" --version
    mkdir -p "$work/home"
    cat >"$work/home/config.toml" <<EOF
model = "gpt-5.5"
model_provider = "stub"

[model_providers.stub]
name = "stub"
base_url = "http://127.0.0.1:$stub_port/v1"
env_key = "STUB_API_KEY"
wire_api = "responses"

[otel]
log_user_prompt = false
exporter = { otlp-http = { endpoint = "http://127.0.0.1:$sink_port/v1/logs", protocol = "json" } }
trace_exporter = { otlp-http = { endpoint = "http://127.0.0.1:$sink_port/v1/traces", protocol = "json" } }
metrics_exporter = "none"

[mcp_servers.weather]
command = "python3"
args = ["$here/examples/recipes/stub_mcp.py"]
EOF
    cd "$work/project"
    # Turn 1 approves the MCP tool for this run; turn 2 (the same session, resumed) runs under the
    # default policy, so its MCP call is refused: one fixture has a success, a failure and a denial.
    CODEX_HOME="$work/home" STUB_API_KEY=stub "$codex" exec --skip-git-repo-check \
        -c 'mcp_servers.weather.default_tools_approval_mode="approve"' \
        "What is the weather in Paris?" </dev/null >/dev/null 2>"$work/turn1.log"
    CODEX_HOME="$work/home" STUB_API_KEY=stub "$codex" exec --skip-git-repo-check \
        resume --last "Thanks, and tomorrow?" </dev/null >/dev/null 2>"$work/turn2.log"
    cd "$here"
    # Codex traces its internals: hundreds of spans a turn. Keep the ones on a turn's path.
    keep="codex.exec,session_task.turn,run_turn,run_sampling_request,try_run_sampling_request"
    keep="$keep,codex.sampling,model_client.stream_responses_api,receiving_stream"
    keep="$keep,handle_responses,handle_tool_call,exec_command,tool_search,mcp.tools.call"
    keep="$keep,mcp__weatherget_weather"
}

capture_gemini() {
    gemini=${GEMINI:-gemini}
    "$gemini" --version
    # Gemini CLI reads ~/.gemini/settings.json: a home of its own keeps the developer's out.
    mkdir -p "$work/home/.gemini"
    cat >"$work/home/.gemini/settings.json" <<EOF
{
  "security": {"auth": {"selectedType": "gemini-api-key"}},
  "telemetry": {"enabled": true, "target": "local", "otlpProtocol": "http",
                "otlpEndpoint": "http://127.0.0.1:$sink_port", "logPrompts": false, "traces": true},
  "mcpServers": {"weather": {"command": "python3",
                             "args": ["$here/examples/recipes/stub_mcp.py"], "trust": true}}
}
EOF
    cd "$work/project"
    # Turn 1 may run anything (yolo); turn 2, the same session resumed under the default policy,
    # asks for the shell tool it was not offered, and is refused.
    HOME="$work/home" GEMINI_API_KEY=stub GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:$stub_port" \
        "$gemini" -m gemini-3.8-flash --skip-trust --approval-mode yolo \
        -p "What is the weather in Paris?" </dev/null >/dev/null 2>"$work/turn1.log"
    HOME="$work/home" GEMINI_API_KEY=stub GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:$stub_port" \
        "$gemini" -m gemini-3.8-flash --skip-trust --resume latest \
        -p "Thanks, and tomorrow?" </dev/null >/dev/null 2>"$work/turn2.log"
    cd "$here"
    keep="" # Gemini CLI's spans are few: keep them all
}

case "$tool" in
    codex) capture_codex ;;
    gemini-cli) capture_gemini ;;
    *) echo "unknown tool: $tool (codex | gemini-cli)" >&2; exit 2 ;;
esac

sleep 2 # the exporters flush on exit; let the last request land
# Gemini CLI repeats its system prompt (31 KB) on every model call, as an attribute no mapping
# reads; 512 characters of it is plenty for a fixture.
python3 "$here/scripts/otlp-fixture.py" scrub "$work/sink" "$out" --name session \
    --path "$work" --path "$here" --keep-spans "$keep" --no-metrics --max-string 512
