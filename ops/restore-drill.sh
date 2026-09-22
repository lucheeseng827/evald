#!/usr/bin/env bash
# evald backup/restore drill.
#
# A backup nobody has restored is a cron job producing files. This script performs the
# documented recovery end to end and asserts the property that actually matters:
#
#   A restore loses nothing that was ACK'd before the backup point.
#
# That is the durability claim (the WAL fsync is the ACK boundary) extended across a
# backup. It is deliberately stronger than "the process starts": a restored store that
# comes up and is quietly short a few thousand spans is the failure this exists to catch.
#
# It drills the documented procedure itself — docs/OPERATIONS.md § Backup & restore — not
# a parallel path. A drill that exercises its own special-case code is testing the drill.
#
# Usage:
#   ops/restore-drill.sh [path/to/evald]        # default: target/debug/evald
#
# Exit 0 = the drill passed and printed measured RTO. Non-zero = a real finding.

set -euo pipefail

BIN="${1:-target/debug/evald}"
SPANS_BEFORE="${SPANS_BEFORE:-2000}"   # ACK'd before the backup — these MUST survive
SPANS_AFTER="${SPANS_AFTER:-500}"      # ACK'd after it — these are the expected RPO loss

[ -x "$BIN" ] || { echo "no evald binary at $BIN (cargo build --bin evald)" >&2; exit 2; }
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

WORK="$(mktemp -d)"; DATA="$WORK/data"; BACKUP="$WORK/backup"
trap 'kill -9 "${SRV:-}" 2>/dev/null || true; rm -rf "$WORK"' EXIT

# An ephemeral port, and gRPC off: the drill must not collide with a real evald, or with
# another drill on the same box.
PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"

# Every EVALD_* in the caller's environment, as `env -u` arguments.
#
# The drill starts a server from the flags below and then asserts what a restore loses. An
# inherited EVALD_* silently changes that server — EVALD_AUTH_TOKEN makes every ingest below
# a 401, so the drill would "pass" having stored nothing; EVALD_DATA_DIR points it at another
# store entirely — and the drill would be measuring something other than what it reports.
# Same class of bug as tests/crash_recovery.rs, which clears these for the same reason.
EVALD_ENV_RESET=()
while IFS= read -r k; do
  EVALD_ENV_RESET+=(-u "$k")
done < <(env | sed -n 's/^\(EVALD_[A-Za-z0-9_]*\)=.*/\1/p')
if [ "${#EVALD_ENV_RESET[@]}" -gt 0 ]; then
  echo "note: clearing ${EVALD_ENV_RESET[*]} from the drill server's environment"
fi

start() {
  env ${EVALD_ENV_RESET[@]+"${EVALD_ENV_RESET[@]}"} \
      "$BIN" serve --otlp-http "127.0.0.1:$PORT" --otlp-grpc "" --data-dir "$DATA" \
      --seal-threshold 500 --compact-interval-secs 1 >"$WORK/serve.log" 2>&1 &
  SRV=$!
  for _ in $(seq 1 120); do
    curl -sf "http://127.0.0.1:$PORT/readyz" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  echo "server never became ready; log:" >&2; tail -20 "$WORK/serve.log" >&2; exit 1
}

stop() { kill "${SRV:-}" 2>/dev/null || true; wait "${SRV:-}" 2>/dev/null || true; SRV=; }

# Ingest `n` spans with ids offset by `base`, counting only confirmed 200s — an ACK means
# WAL-fsynced, which is exactly the set the restore must not lose.
ingest() {
  local base="$1" n="$2"
  python3 - "$PORT" "$base" "$n" <<'PY'
import json, sys, urllib.request
port, base, n = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
acked = 0
for i in range(base, base + n):
    body = json.dumps({"resourceSpans":[{"scopeSpans":[{"spans":[{
        "traceId": f"{i:032x}", "spanId": f"{i:016x}", "name": "s", "kind": 1,
        "startTimeUnixNano": "1700000000000000000",
        "endTimeUnixNano": "1700000000500000000"}]}]}]}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}/v1/traces", body,
                                 {"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            if r.status == 200:
                acked += 1
    except Exception:
        pass   # a shed (429) or error is not an ACK, so it is simply not counted
print(acked)
PY
}

count_spans() {
  # `evald query` prints a bare JSON array of rows on stdout; its tracing goes to stderr.
  "$BIN" query "SELECT COUNT(DISTINCT span_id) AS n FROM spans" --data-dir "$DATA" 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['n'])"
}

echo "== evald restore drill =="
echo "binary:   $BIN"
echo "workdir:  $WORK"

# ---- 1. Ingest, then back up the documented way -------------------------------------
start
ACKED_BEFORE="$(ingest 1 "$SPANS_BEFORE")"
echo "ingested: $ACKED_BEFORE spans ACK'd before the backup"
[ "$ACKED_BEFORE" -gt 0 ] || { echo "nothing was ACK'd — the drill proves nothing" >&2; exit 1; }

# Cold backup: OPERATIONS.md's option 1, and the only one that needs no filesystem support.
# Stopping first is what makes it consistent — the doc's warning against copying a live
# directory file-by-file is the failure mode this ordering avoids.
stop
BACKUP_START=$(date +%s.%N)
cp -a "$DATA" "$BACKUP"
BACKUP_SECS=$(python3 -c "print(f'{$(date +%s.%N) - $BACKUP_START:.2f}')")
BACKUP_BYTES=$(du -sb "$BACKUP" | cut -f1)
echo "backup:   $BACKUP_BYTES bytes in ${BACKUP_SECS}s (cold copy)"

# ---- 2. Keep serving, so there is real post-backup data to lose ----------------------
start
ACKED_AFTER="$(ingest 1000000 "$SPANS_AFTER")"
echo "post-backup: $ACKED_AFTER spans ACK'd after the backup (expected RPO loss)"
stop
LIVE_TOTAL="$(count_spans)"
echo "live store before loss: $LIVE_TOTAL spans"

# ---- 3. Lose the data dir, restore, recover -----------------------------------------
rm -rf "$DATA"
RESTORE_START=$(date +%s.%N)
cp -a "$BACKUP" "$DATA"
start                      # readiness is the end of recovery: WAL replay + orphan sweep
RTO_SECS=$(python3 -c "print(f'{$(date +%s.%N) - $RESTORE_START:.2f}')")
stop

RECOVERED="$(count_spans)"
echo "recovered: $RECOVERED spans"

# ---- 4. Assert the property ----------------------------------------------------------
fail=0
if [ "$RECOVERED" -lt "$ACKED_BEFORE" ]; then
  echo "FAIL: $((ACKED_BEFORE - RECOVERED)) span(s) ACK'd BEFORE the backup did not survive the restore." >&2
  echo "      That is data loss inside the backup point, not an RPO window." >&2
  fail=1
fi
if [ "$RECOVERED" -gt "$LIVE_TOTAL" ]; then
  echo "FAIL: recovered ($RECOVERED) exceeds what the live store held ($LIVE_TOTAL) — spans were duplicated." >&2
  fail=1
fi
LOST=$((LIVE_TOTAL - RECOVERED))

cat <<EOF

== result ==
ACK'd before backup : $ACKED_BEFORE   (must all survive)
recovered           : $RECOVERED
lost                : $LOST   (post-backup writes — the RPO window, = backup age)
backup              : $BACKUP_BYTES bytes, ${BACKUP_SECS}s
RTO                 : ${RTO_SECS}s   (copy back + open + WAL replay, to /readyz)
EOF

[ "$fail" -eq 0 ] || exit 1
echo
echo "PASS — nothing ACK'd before the backup was lost; loss is bounded by backup age."
