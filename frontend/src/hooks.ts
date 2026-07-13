import { useCallback, useEffect, useRef, useState } from "react";
import { api, type ApiError } from "./api";

export interface AsyncState<T> {
  data: T | null;
  error: ApiError | null;
  loading: boolean;
  reload: () => void;
}

// Fetch `path` (or run `fn`) once per key change, tracking loading/error. The
// building block every view uses to bind to a `/v1/*` endpoint.
export function useApi<T>(path: string | null, deps: unknown[] = []): AsyncState<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState<boolean>(!!path);
  const [tick, setTick] = useState(0);
  const alive = useRef(true);

  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; };
  }, []);

  useEffect(() => {
    if (!path) { setLoading(false); return; }
    setLoading(true);
    setError(null);
    const controller = new AbortController();
    api<T>(path, { signal: controller.signal })
      .then((d) => { if (alive.current) { setData(d); setLoading(false); } })
      .catch((e: ApiError) => {
        if (e.name === "AbortError") return;
        if (alive.current) { setError(e); setLoading(false); }
      });
    return () => controller.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, tick, ...deps]);

  const reload = useCallback(() => setTick((t) => t + 1), []);
  return { data, error, loading, reload };
}

// ---- persisted client-local settings ----
export interface Settings {
  theme: "dark" | "dim" | "midnight";
  density: "comfortable" | "compact";
  notif: Record<string, boolean>;
}
const DEFAULT_SETTINGS: Settings = { theme: "dark", density: "comfortable", notif: {} };

export function loadSettings(): Settings {
  try {
    return { ...DEFAULT_SETTINGS, ...JSON.parse(localStorage.getItem("evald.settings") || "{}") };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}
export function saveSettings(s: Settings) {
  localStorage.setItem("evald.settings", JSON.stringify(s));
}
