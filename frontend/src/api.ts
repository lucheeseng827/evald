// API client + wire types for the evald console. Every view binds to a real
// `/v1/*` endpoint through these helpers.

export interface Meta {
  edition: "oss" | "ee";
  fleet: boolean;
  judge: boolean;
  version: string;
}

export interface IngestStats {
  hot_spans: number;
  max_hot_spans: number;
  channel_capacity: number;
  rejections: number;
  shedding: boolean;
}

export interface Tokens {
  prompt?: number;
  completion?: number;
  total?: number;
  cache_read?: number;
  cache_write?: number;
  reasoning?: number;
}

export interface Span {
  trace_id: string;
  span_id: string;
  parent_span_id?: string | null;
  name: string;
  otel_kind: number;
  oi_kind?: string | null;
  start_unix_nano: number;
  end_unix_nano: number;
  status_code: number;
  model?: string | null;
  provider?: string | null;
  tokens: Tokens;
  cost_usd?: number | null;
  input_value?: string | null;
  output_value?: string | null;
  session_id?: string | null;
  user_id?: string | null;
  service_name?: string | null;
  raw_attributes?: Record<string, unknown>;
  orphan_parent?: boolean;
}

export type ScoreSource = "eval" | "human" | "api";
export interface Score {
  id: string;
  target_type: "span" | "trace" | "session" | "run";
  target_id: string;
  name: string;
  num_value?: number | null;
  str_value?: string | null;
  data_type: string;
  source: ScoreSource;
  ts_unix_nano: number;
}

export interface SqlResponse {
  columns: string[];
  rows: Record<string, unknown>[];
  truncated: boolean;
}

export interface ApiError extends Error {
  status?: number;
}

export async function api<T = unknown>(path: string, opts?: RequestInit): Promise<T> {
  const res = await fetch(path, { signal: AbortSignal.timeout(30_000), ...opts });
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    const err: ApiError = new Error(text.trim() || `${res.status} ${res.statusText}`);
    err.status = res.status;
    throw err;
  }
  const ct = res.headers.get("content-type") || "";
  return (ct.includes("json") ? res.json() : res.text()) as Promise<T>;
}

export function postSql(sql: string, limit = 1000): Promise<SqlResponse> {
  return api<SqlResponse>("/v1/sql", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ sql, limit }),
  });
}

// ---- formatting ----------------------------------------------------------
const NANOS_PER_MS = 1e6;
export const fmtNum = (n: number | null | undefined): string =>
  n == null ? "—" : Number(n).toLocaleString("en-US");

export function fmtAgo(nanos: number | null | undefined): string {
  if (!nanos) return "—";
  const s = Math.max(0, Math.round((Date.now() - nanos / NANOS_PER_MS) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}

export function fmtDur(startNano?: number, endNano?: number): string {
  if (!startNano || !endNano || endNano < startNano) return "—";
  const ms = (endNano - startNano) / NANOS_PER_MS;
  return ms < 1000 ? `${Math.round(ms)}ms` : `${(ms / 1000).toFixed(2)}s`;
}

export const shortId = (id?: string): string =>
  id && id.length > 10 ? `${id.slice(0, 4)}…${id.slice(-4)}` : id || "—";

export function fmtBytes(b?: number): string {
  if (!b) return "0";
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0, n = b;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(n < 10 ? 1 : 0)} ${u[i]}`;
}
