import { useEffect, useMemo, useState, type ReactNode } from "react";
import { api, type Meta } from "./api";
import { Tag } from "./components";
import { Icon } from "./icons";
import type { ViewDef } from "./registry";
import { loadSettings } from "./hooks";
import { SettingsModal } from "./Settings";

const FALLBACK_META: Meta = { edition: "oss", fleet: false, judge: false, version: "" };

export interface AppProps {
  // The console is data-driven: it renders exactly the views/groups it is handed.
  // The OSS build passes the OSS registry; the EE build passes OSS + EE views. That
  // is the edition boundary — the OSS bundle never contains EE view code at all.
  views: ViewDef[];
  groupLabels: Record<string, string>;
  // Optional edition badge (top-right). Kept a prop so edition-specific labelling
  // lives in each build's entrypoint, not in this shared shell.
  editionBadge?: (meta: Meta) => ReactNode;
}

export function App({ views, groupLabels, editionBadge }: AppProps) {
  const [meta, setMeta] = useState<Meta | null>(null);
  const [view, setView] = useState(views[0]?.key ?? "overview");
  const [settings, setSettings] = useState(loadSettings());
  const [settingsOpen, setSettingsOpen] = useState(false);

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", settings.theme);
  }, [settings.theme]);

  useEffect(() => {
    const controller = new AbortController();
    api<Meta>("/v1/meta", { signal: controller.signal })
      .then(setMeta)
      .catch((e) => { if (e?.name !== "AbortError") setMeta(FALLBACK_META); });
    return () => controller.abort();
  }, []);

  // Ordered, de-duplicated groups as they first appear in the registry.
  const groups = useMemo(() => {
    const order: string[] = [];
    for (const v of views) if (!order.includes(v.group)) order.push(v.group);
    return order.map((g) => ({ g, items: views.filter((v) => v.group === g) }));
  }, [views]);

  if (!meta) return null;

  const active = views.find((v) => v.key === view) || views[0];
  const View = active.Component;

  return (
    <div className="app">
      <nav className="nav">
        <div className="brand">
          <div className="brand-glyph">e</div>
          <div><span className="brand-name">evald</span><span className="brand-sub">trace + eval</span></div>
        </div>
        <div className="nav-groups">
          {groups.map(({ g, items }) => (
            <div key={g}>
              <div className="nav-group-label">{groupLabels[g] ?? g}</div>
              {items.map((v) => (
                <button key={v.key} data-view={v.key} className={"nav-item" + (v.key === active.key ? " active" : "")} onClick={() => setView(v.key)}>
                  <Icon name={v.icon} />
                  <span style={{ flex: 1 }}>{v.label}</span>
                </button>
              ))}
            </div>
          ))}
        </div>
        <div className="nav-footer">
          <button className="account-chip" title="Account settings" onClick={() => setSettingsOpen(true)}>
            <div className="avatar">e</div>
            <div className="who">
              <div className="email">operator</div>
              <small>evald {meta.version}</small>
            </div>
            <Icon name="settings" />
          </button>
        </div>
      </nav>

      <main className={"main" + (settings.density === "compact" ? " compact" : "")}>
        <div style={{ paddingBottom: 14 }}>
          <h1 className="page-title">{active.title}</h1>
          <div className="page-sub">{active.sub}</div>
        </div>
        <div className="binds">
          <span className="label">Binds to</span>
          {active.ep.map((e) => <Tag key={e}>{e}</Tag>)}
          <span className="spacer" />
          {editionBadge?.(meta)}
        </div>
        <View key={active.key} />
      </main>

      {settingsOpen && <SettingsModal meta={meta} settings={settings} onChange={setSettings} onClose={() => setSettingsOpen(false)} />}
    </div>
  );
}
