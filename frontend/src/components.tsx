// The evald design system, as reusable TSX primitives. The Claude Design mockup
// is the visual guide; these are the parts every view composes from.
import type { ReactNode } from "react";

export type Tone = "default" | "brand" | "good" | "warn" | "danger";

export const Mono = ({ children, className }: { children: ReactNode; className?: string }) => (
  <span className={"mono" + (className ? " " + className : "")}>{children}</span>
);
export const Faint = ({ children }: { children: ReactNode }) => <span className="faint">{children}</span>;

export function Badge({ tone = "default", dot, children }: { tone?: Tone; dot?: boolean; children: ReactNode }) {
  return (
    <span className={"badge " + tone}>
      {dot && <span className="dot" />}
      {children}
    </span>
  );
}

export const Tag = ({ children }: { children: ReactNode }) => <span className="tag">{children}</span>;

export function Kpi({ label, value, foot, accent }: { label: string; value: ReactNode; foot?: ReactNode; accent?: boolean }) {
  return (
    <div className={"kpi" + (accent ? " accent" : "")}>
      <div className="label">{label}</div>
      <div className="value">{value}</div>
      {foot != null && <div className="foot">{foot}</div>}
    </div>
  );
}

export function Card({ title, action, flush, children }: { title?: ReactNode; action?: ReactNode; flush?: boolean; children: ReactNode }) {
  return (
    <div className={"card" + (flush ? " flush" : "")}>
      {title != null && (
        <div className="card-head">
          <span className="card-title">{title}</span>
          {action}
        </div>
      )}
      <div className="card-body">{children}</div>
    </div>
  );
}

export function Meter({ label, value, max, valueLabel, tone = "brand" }: { label: string; value: number; max: number; valueLabel: string; tone?: "brand" | "good" | "warn" | "danger" }) {
  const pct = max > 0 ? Math.min(100, (value / max) * 100) : 0;
  return (
    <div className="meter">
      <div className="meter-top">
        <span className="m-label">{label}</span>
        <span className="m-val">{valueLabel}</span>
      </div>
      <div className="track">
        <div className={"fill " + tone} style={{ width: pct + "%" }} />
      </div>
    </div>
  );
}

export function TokenPill({ value }: { value: string }) {
  return (
    <div className="token-pill">
      <span className="val">{value}</span>
      <button className="copy" title="Copy" onClick={() => navigator.clipboard?.writeText(value)}>copy</button>
    </div>
  );
}

export function Sparkline({ data, width = 200, height = 44, tone = "brand" }: { data: number[]; width?: number; height?: number; tone?: "brand" | "good" }) {
  if (!data || data.length < 2) return <svg width={width} height={height} className="sparkline" />;
  const min = Math.min(...data), max = Math.max(...data), span = max - min || 1;
  const d = data
    .map((v, i) => {
      const x = (i / (data.length - 1)) * width;
      const y = height - ((v - min) / span) * (height - 4) - 2;
      return `${i ? "L" : "M"}${x.toFixed(1)} ${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg width={width} height={height} className="sparkline">
      <path d={d} fill="none" stroke={tone === "brand" ? "#4f8cff" : "#36c08b"} strokeWidth="1.6" />
    </svg>
  );
}

export function Avatar({ email, size }: { email?: string; size?: number }) {
  const letter = (email || "?").trim().charAt(0).toUpperCase();
  const style = size ? { width: size, height: size, fontSize: Math.round(size / 2.3) } : undefined;
  return <div className="avatar" style={style}>{letter}</div>;
}

export function Switch({ checked, onChange }: { checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className="switch">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      <span className="slider" />
    </label>
  );
}

export interface Column<R> {
  header: ReactNode;
  width?: string;
  align?: "right";
  cell: (row: R, i: number) => ReactNode;
}

export function DataTable<R>({ columns, rows, rowKey, onRowClick, selKey }: {
  columns: Column<R>[];
  rows: R[];
  rowKey: (row: R, i: number) => string;
  onRowClick?: (row: R) => void;
  selKey?: (row: R) => boolean;
}) {
  const grid = columns.map((c) => c.width || "1fr").join(" ");
  return (
    <div className="dt">
      <div className="dt-head" style={{ gridTemplateColumns: grid }}>
        {columns.map((c, i) => (
          <span key={i} className={"cell" + (c.align === "right" ? " r" : "")}>{c.header}</span>
        ))}
      </div>
      {rows.map((r, i) => (
        <div
          key={rowKey(r, i)}
          className={"dt-row" + (onRowClick ? " click" : "") + (selKey?.(r) ? " sel" : "")}
          style={{ gridTemplateColumns: grid }}
          onClick={onRowClick ? () => onRowClick(r) : undefined}
        >
          {columns.map((c, j) => (
            <span key={j} className={"cell" + (c.align === "right" ? " r" : "")}>{c.cell(r, i)}</span>
          ))}
        </div>
      ))}
    </div>
  );
}

export function EmptyState({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      <div className="empty-title">{title}</div>
      {children && <div className="empty-body">{children}</div>}
    </div>
  );
}

export const SampleNote = ({ children }: { children: ReactNode }) => (
  <div className="sample-note">▲ {children}</div>
);

export function Loading({ label = "loading…" }: { label?: string }) {
  return <div className="loading">{label}</div>;
}

// Shared score → tone/label mapping used across Overview / Scores / Traces.
export function scoreTone(s: { num_value?: number | null; str_value?: string | null }): Tone {
  if (s.num_value != null) return s.num_value >= 0.8 ? "good" : s.num_value >= 0.5 ? "warn" : "danger";
  if (s.str_value) return /correct|pass|good|true/i.test(s.str_value) ? "good" : "default";
  return "default";
}
export function scoreValue(s: { num_value?: number | null; str_value?: string | null }): string {
  return s.num_value != null ? String(s.num_value) : s.str_value || "—";
}
