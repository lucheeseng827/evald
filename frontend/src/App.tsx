import { useEffect, useMemo, useState, type ReactNode } from "react";
import { api, type Meta } from "./api";
import { Avatar, Tag } from "./components";
import { BrandMark, Icon } from "./icons";
import type { ViewDef } from "./registry";
import { loadSettings } from "./hooks";
import { SettingsModal } from "./Settings";

// No route exposes the signed-in principal. The EE gate resolves a subject on every
// request (ee/src/fleet/query.rs) but never returns it, and the OSS node has no auth
// to resolve one from — so the console names its user generically. Defined once here
// because it was previously spelled three different ways: an "e" avatar over the
// label "operator" in the chip, and an "E" avatar over "Operator" in the settings
// modal. Swap this for the real subject when a whoami route lands.
const OPERATOR = "Operator";

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
          <BrandMark />
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
            <Avatar name={OPERATOR} />
            <div className="who">
              <div className="email">{OPERATOR}</div>
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

      {settingsOpen && <SettingsModal meta={meta} operator={OPERATOR} settings={settings} onChange={setSettings} onClose={() => setSettingsOpen(false)} />}
    </div>
  );
}
