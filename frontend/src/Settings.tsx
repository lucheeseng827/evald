import { useEffect } from "react";
import type { Meta } from "./api";
import { Avatar, Badge, Switch } from "./components";
import { saveSettings, type Settings as S } from "./hooks";

const THEMES: S["theme"][] = ["dark", "dim", "midnight"];
const CHANNELS: [string, string, string][] = [
  ["slack", "Slack", "Post alerts to a channel"],
  ["teams", "Microsoft Teams", "Incoming webhook connector"],
  ["email", "Email", "Digest + critical alerts"],
  ["webhook", "Webhook", "POST JSON to your endpoint"],
];

// Account settings — appearance + notification prefs are client-local (localStorage);
// identity/edition come from /v1/meta.
export function SettingsModal({ meta, settings, onChange, onClose }: {
  meta: Meta; settings: S; onChange: (s: S) => void; onClose: () => void;
}) {
  function update(next: S) { saveSettings(next); onChange(next); }
  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) { if (e.key === "Escape") onClose(); }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose]);
  return (
    <div className="modal-scrim" onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="modal" role="dialog" aria-modal="true" aria-labelledby="settings-modal-title">
        <div className="modal-head">
          <span className="modal-title" id="settings-modal-title">Account settings</span>
          <button className="x" onClick={onClose} aria-label="Close">×</button>
        </div>
        <div className="modal-scroll">
          <div className="row" style={{ padding: "6px 0 14px" }}>
            <Avatar email="evald" size={40} />
            <div style={{ flex: 1 }}>
              <div style={{ fontWeight: 600 }}>Operator</div>
              <div className="mono faint" style={{ fontSize: 12 }}>evald {meta.version} · {meta.edition.toUpperCase()}</div>
            </div>
            <Badge tone={meta.fleet ? "brand" : "default"} dot>{meta.edition.toUpperCase()}</Badge>
          </div>

          <div className="set-section">
            <div className="set-label">Appearance</div>
            <div className="row">
              {THEMES.map((t) => (
                <button key={t} className={"theme-btn" + (settings.theme === t ? " on" : "")}
                  onClick={() => { document.documentElement.setAttribute("data-theme", t); update({ ...settings, theme: t }); }}>
                  {t[0].toUpperCase() + t.slice(1)}
                </button>
              ))}
            </div>
            <div className="faint" style={{ fontSize: 12, marginTop: 8 }}>evald is a dark-only console — themes tune contrast, never the hue.</div>
          </div>

          <div className="set-section">
            <div className="set-label">Notification channels (client-local prefs)</div>
            {CHANNELS.map(([k, name, desc]) => (
              <div key={k} className="set-row">
                <div><div className="set-name">{name}</div><div className="set-desc">{desc}</div></div>
                <Switch checked={!!settings.notif[k]} onChange={(v) => update({ ...settings, notif: { ...settings.notif, [k]: v } })} />
              </div>
            ))}
          </div>
        </div>
        <div className="modal-foot">
          <button className="btn" onClick={onClose}>Close</button>
        </div>
      </div>
    </div>
  );
}
