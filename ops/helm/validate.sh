#!/usr/bin/env bash
# The single-node evald chart's gate: render a matrix and assert both directions — what each
# configuration renders to, and that the configurations which would deploy something healthy and
# wrong are refused by name. A guard that has not been seen to refuse is not a guard.
#
#   ops/helm/validate.sh [helm]              render + lint checks (no cluster)
#   ops/helm/validate.sh [helm] --kind IMAGE also install IMAGE on the current kube context (a
#                                            kind cluster in CI), wait for readiness, send a span
#                                            and read it back through the API
#
# Exit 0 = everything asserted held.
set -euo pipefail

HELM="${1:-helm}"
MODE="${2:-}"
IMAGE="${3:-}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CHART="$HERE/evald"
command -v "$HELM" >/dev/null 2>&1 || { echo "no helm binary at '$HELM'" >&2; exit 2; }

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL $1" >&2; }

render() { "$HELM" template rel "$CHART" "$@" 2>&1; }

# A render that succeeds and whose objects satisfy a python predicate (`objs`, `kind(k)`).
expect_render() {
  local name="$1" pred="$2"; shift 2
  local out
  if ! out="$(render "$@")"; then
    bad "$name — render failed: $(printf '%s' "$out" | tail -2 | tr '\n' ' ')"; return
  fi
  if printf '%s' "$out" | python3 -c "
import sys, yaml
objs = [o for o in yaml.safe_load_all(sys.stdin) if o]
def kind(k): return [o for o in objs if o['kind'] == k]
def ss(): return kind('StatefulSet')[0]
def ctr(): return ss()['spec']['template']['spec']['containers'][0]
def env(): return {e['name']: e.get('value') for e in ctr().get('env', [])}
sys.exit(0 if ($pred) else 1)
"; then ok "$name"; else bad "$name — rendered, but the assertion did not hold"; fi
}

# A render that fails, with a message naming the reason.
expect_refusal() {
  local name="$1" needle="$2"; shift 2
  local out
  if out="$(render "$@")"; then bad "$name — expected a refusal, but it rendered"; return; fi
  if printf '%s' "$out" | grep -qF -- "$needle"; then ok "$name"
  else bad "$name — refused, but without '$needle': $(printf '%s' "$out" | tail -1)"; fi
}

echo "== helm lint =="
if "$HELM" lint "$CHART" >/dev/null 2>&1; then ok "chart lints"; else bad "chart lints"; fi

echo "== renders =="
expect_render "default: one replica, a volume, probes on the open paths" \
  "ss()['spec']['replicas'] == 1 and ss()['spec']['volumeClaimTemplates'][0]['spec']['resources']['requests']['storage'] == '20Gi' and ctr()['livenessProbe']['httpGet']['path'] == '/healthz' and ctr()['readinessProbe']['httpGet']['path'] == '/readyz' and env()['EVALD_DATA_DIR'] == '/data'"
expect_render "default: no auth, no secret, no ServiceMonitor" \
  "not kind('Secret') and not kind('ServiceMonitor') and 'EVALD_AUTH_TOKEN_FILE' not in env()"
expect_render "default: nonroot, read-only root, no service account token" \
  "ss()['spec']['template']['spec']['securityContext']['runAsNonRoot'] and ctr()['securityContext']['readOnlyRootFilesystem'] and ss()['spec']['template']['spec']['automountServiceAccountToken'] is False"
expect_render "image tag defaults to the appVersion" \
  "ctr()['image'].endswith(':0.5.0')"
expect_render "an existing Secret is mounted as a token file, never put in env" \
  "env()['EVALD_AUTH_TOKEN_FILE'] == '/run/secrets/evald/tokens' and ss()['spec']['template']['spec']['volumes'][0]['secret']['secretName'] == 'mine' and not kind('Secret')" \
  --set auth.existingSecret=mine
expect_render "auth.create makes the Secret and rolls the pod when the token changes" \
  "kind('Secret')[0]['stringData']['tokens'] == 'a-token-of-twenty-chars' and 'checksum/auth' in ss()['spec']['template']['metadata']['annotations']" \
  --set auth.create=true --set auth.token=a-token-of-twenty-chars
expect_render "LoadBalancer with auth renders" \
  "[s for s in kind('Service') if s['spec'].get('type') == 'LoadBalancer']" \
  --set service.type=LoadBalancer --set auth.existingSecret=mine
expect_render "grpc off: no grpc port, gRPC listener disabled" \
  "env()['EVALD_OTLP_GRPC_ADDR'] == '' and all(p['name'] != 'grpc' for p in ctr()['ports'])" \
  --set service.grpc=false
expect_render "retention and extra flags reach serve" \
  "env()['EVALD_RETENTION'] == '30d' and ctr()['args'] == ['serve', '--redact', 'email']" \
  --set retention=30d --set 'extraArgs={--redact,email}'
expect_render "ServiceMonitor scrapes /metrics, with the token when auth is on" \
  "kind('ServiceMonitor')[0]['spec']['endpoints'][0]['path'] == '/metrics' and kind('ServiceMonitor')[0]['spec']['endpoints'][0]['authorization']['credentials']['name'] == 'mine'" \
  --set serviceMonitor.enabled=true --set auth.existingSecret=mine
expect_render "a storage class is passed through" \
  "ss()['spec']['volumeClaimTemplates'][0]['spec']['storageClassName'] == 'fast'" \
  --set persistence.storageClass=fast

echo "== refusals =="
expect_refusal "two replicas" "one replica per store" --set replicas=2
expect_refusal "LoadBalancer without auth" "no authentication" --set service.type=LoadBalancer
expect_refusal "NodePort without auth" "no authentication" --set service.type=NodePort
expect_refusal "a short created token" "16 or more" --set auth.create=true --set auth.token=short
expect_refusal "both an existing and a created Secret" "not both" \
  --set auth.create=true --set auth.token=a-token-of-twenty-chars --set auth.existingSecret=mine
expect_refusal "an unknown service type" "must be ClusterIP" --set service.type=ExternalName

if [ "$MODE" = "--kind" ]; then
  echo "== install on the current cluster ($IMAGE) =="
  command -v kubectl >/dev/null || { echo "--kind needs kubectl" >&2; exit 2; }
  repo="${IMAGE%:*}"; tag="${IMAGE##*:}"
  "$HELM" install smoke "$CHART" --wait --timeout 180s \
    --set image.repository="$repo" --set image.tag="$tag" --set image.pullPolicy=Never \
    --set persistence.size=1Gi --set auth.create=true --set auth.token=kind-smoke-token-0123
  kubectl port-forward svc/smoke-evald 14318:4318 >/dev/null 2>&1 &
  pf=$!; trap 'kill $pf 2>/dev/null || true' EXIT
  for _ in $(seq 1 30); do curl -fsS http://127.0.0.1:14318/healthz >/dev/null 2>&1 && break; sleep 1; done
  auth="Authorization: Bearer kind-smoke-token-0123"
  code="$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:14318/v1/spans)"
  if [ "$code" = 401 ]; then ok "the API refuses a request with no token"; else bad "no-token request got $code"; fi
  span='{"resourceSpans":[{"scopeSpans":[{"spans":[{"traceId":"0af7651916cd43dd8448eb211c80319c","spanId":"b7ad6b7169203331","name":"kind-smoke","kind":1,"startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000500000000"}]}]}]}'
  code="$(curl -s -o /dev/null -w '%{http_code}' -H "$auth" -H 'content-type: application/json' -d "$span" http://127.0.0.1:14318/v1/traces)"
  if [ "$code" = 200 ]; then ok "a span is ACK'd"; else bad "ingest got $code"; fi
  if curl -fsS -H "$auth" http://127.0.0.1:14318/v1/traces/0af7651916cd43dd8448eb211c80319c | grep -q kind-smoke; then
    ok "and read back"; else bad "the span did not read back"; fi
  # The store survives the pod: delete it, let the StatefulSet bring it back, read again.
  kubectl delete pod smoke-evald-0 --wait=true >/dev/null
  kubectl wait --for=condition=ready pod/smoke-evald-0 --timeout=120s >/dev/null
  kill $pf 2>/dev/null || true
  kubectl port-forward svc/smoke-evald 14318:4318 >/dev/null 2>&1 &
  pf=$!
  for _ in $(seq 1 30); do curl -fsS http://127.0.0.1:14318/healthz >/dev/null 2>&1 && break; sleep 1; done
  if curl -fsS -H "$auth" http://127.0.0.1:14318/v1/traces/0af7651916cd43dd8448eb211c80319c | grep -q kind-smoke; then
    ok "the span survives the pod being replaced (the volume holds the store)"; else bad "the span was lost with the pod"; fi
fi

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
