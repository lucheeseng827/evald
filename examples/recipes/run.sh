#!/bin/sh
# Run one framework recipe end to end and check what evald stored:
#
#   examples/recipes/run.sh <recipe>          e.g. examples/recipes/run.sh langgraph
#
# It starts evald and the stub provider (stub_provider.py: no key, no network), installs the
# recipe's pinned dependencies into a scratch environment, runs the recipe's app with the
# OpenTelemetry exporter pointed at evald, then runs check.py against the recipe's expect.json.
# Exit 0 when every expectation holds, 1 when one does not, 2 when the run could not happen.
#
# Environment:
#   EVALD       the evald binary (default: `evald` on PATH)
#   WORK        scratch directory (default: a fresh mktemp -d, removed afterwards unless KEEP=1;
#               one you name is kept, only its data/ is reset)
#   HTTP_PORT   evald's OTLP/HTTP port (default 14318)   GRPC_PORT  its OTLP/gRPC port (14317)
#   STUB_PORT   the stub provider's port (default 19100)
#   KEEP=1      keep WORK (evald's data dir, the logs, the environment) for a look afterwards
set -eu

R="${1:?usage: examples/recipes/run.sh <recipe>}"
HERE=$(cd "$(dirname "$0")" && pwd)
DIR="$HERE/$R"
[ -f "$DIR/expect.json" ] || { echo "run.sh: no recipe '$R' (no $DIR/expect.json)" >&2; exit 2; }
EVALD="${EVALD:-evald}"
HTTP_PORT="${HTTP_PORT:-14318}"; GRPC_PORT="${GRPC_PORT:-14317}"; STUB_PORT="${STUB_PORT:-19100}"
# A WORK the caller names is theirs: kept. Only the scratch directory made here is removed.
if [ -n "${WORK:-}" ]; then own_work=0; else WORK=$(mktemp -d); own_work=1; fi
mkdir -p "$WORK"
rm -rf "${WORK:?}/data" # a kept WORK keeps its environment, never a previous run's spans

stub=""; ev=""
cleanup() {
  [ -n "$stub" ] && kill "$stub" 2>/dev/null || true
  [ -n "$ev" ] && kill "$ev" 2>/dev/null || true
  wait 2>/dev/null || true
  if [ "${KEEP:-0}" = 1 ] || [ "$own_work" = 0 ]; then echo "run.sh: kept $WORK"; else rm -rf "${WORK:?}"; fi
}
trap cleanup 0
trap 'exit 2' INT TERM

wait_for() { # url
  python3 - "$1" <<'EOF'
import sys, time, urllib.request
for _ in range(100):
    try:
        urllib.request.urlopen(sys.argv[1], timeout=1)
        sys.exit(0)
    except Exception:
        time.sleep(0.1)
sys.exit(f"run.sh: {sys.argv[1]} did not come up")
EOF
}

python3 "$HERE/stub_provider.py" --port "$STUB_PORT" > "$WORK/stub.log" 2>&1 &
stub=$!
"$EVALD" serve --data-dir "$WORK/data" --otlp-http "127.0.0.1:$HTTP_PORT" \
  --otlp-grpc "127.0.0.1:$GRPC_PORT" > "$WORK/evald.log" 2>&1 &
ev=$!
wait_for "http://127.0.0.1:$STUB_PORT/health" || exit 2
wait_for "http://127.0.0.1:$HTTP_PORT/healthz" || exit 2

# Install. Python recipes get a virtualenv, Node recipes a copy of the directory and `npm ci`.
if [ -f "$DIR/requirements.txt" ]; then
  if command -v uv >/dev/null 2>&1; then
    uv venv -q --allow-existing "$WORK/venv" && VIRTUAL_ENV="$WORK/venv" uv pip install -q -r "$DIR/requirements.txt"
  else
    python3 -m venv "$WORK/venv" && "$WORK/venv/bin/pip" install -q -r "$DIR/requirements.txt"
  fi || { echo "run.sh: installing $R's requirements failed" >&2; exit 2; }
  APP="$WORK/venv/bin/python $DIR/app.py"
elif [ -f "$DIR/package.json" ]; then
  mkdir -p "$WORK/app" && cp "$DIR"/package.json "$DIR"/package-lock.json "$DIR"/app.mjs "$WORK/app/"
  (cd "$WORK/app" && npm ci --silent --no-audit --no-fund) \
    || { echo "run.sh: npm ci for $R failed" >&2; exit 2; }
  APP="node $WORK/app/app.mjs"
else
  echo "run.sh: $R has neither requirements.txt nor package.json" >&2; exit 2
fi

# The app sees evald as its OTLP endpoint and the stub as its model provider. Nothing else:
# the frameworks' own phone-home switches are off (each recipe's README names the ones it sets).
echo "=== $R"
# shellcheck disable=SC2086  # APP is an interpreter and a script path
env \
  OTEL_EXPORTER_OTLP_ENDPOINT="http://127.0.0.1:$HTTP_PORT" \
  OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
  OTEL_SERVICE_NAME="recipe-$R" \
  OPENAI_BASE_URL="http://127.0.0.1:$STUB_PORT/v1" \
  OPENAI_API_KEY=stub-key \
  ANTHROPIC_BASE_URL="http://127.0.0.1:$STUB_PORT" \
  ANTHROPIC_API_KEY=stub-key \
  EVALD_OTLP_GRPC="http://127.0.0.1:$GRPC_PORT" \
  STUB_URL="http://127.0.0.1:$STUB_PORT" \
  CREWAI_DISABLE_TELEMETRY=true OTEL_SDK_DISABLED=false \
  $APP > "$WORK/app.log" 2>&1 || { cat "$WORK/app.log" >&2; echo "run.sh: $R's app failed" >&2; exit 2; }
tail -n 5 "$WORK/app.log"

python3 "$HERE/check.py" --url "http://127.0.0.1:$HTTP_PORT" --expect "$DIR/expect.json"
