// Inline stroke icons (currentColor). Keyed so the view registry can name one.
const PATHS: Record<string, string> = {
  gauge: "M12 14a2 2 0 100-4 2 2 0 000 4zM12 12l3-3M4 20a9 9 0 1116 0",
  list: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01",
  check: "M20 6L9 17l-5-5",
  spark: "M12 3l2.5 6.5L21 12l-6.5 2.5L12 21l-2.5-6.5L3 12l6.5-2.5z",
  file: "M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9zM14 3v6h6",
  activity: "M22 12h-4l-3 9L9 3l-3 9H2",
  layers: "M12 2l9 5-9 5-9-5 9-5zM3 12l9 5 9-5M3 17l9 5 9-5",
  users: "M17 21v-2a4 4 0 00-4-4H5a4 4 0 00-4 4v2M9 11a4 4 0 100-8 4 4 0 000 8z",
  receipt: "M4 2v20l2-1 2 1 2-1 2 1 2-1 2 1V2l-2 1-2-1-2 1-2-1-2 1-2-1zM8 7h8M8 11h8M8 15h5",
  shield: "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z",
  server: "M4 4h16v6H4zM4 14h16v6H4zM8 7h.01M8 17h.01",
  lock: "M5 11h14v10H5zM8 11V7a4 4 0 018 0v4",
  settings:
    "M12 15a3 3 0 100-6 3 3 0 000 6zM19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 11-2.83 2.83l-.06-.06a1.65 1.65 0 00-2.9 1.09V21a2 2 0 11-4 0v-.09a1.65 1.65 0 00-2.9-1.09l-.06.06a2 2 0 11-2.83-2.83l.06-.06A1.65 1.65 0 004.6 15H4a2 2 0 110-4h.09a1.65 1.65 0 001.51-1V10a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 112.83-2.83l.06.06A1.65 1.65 0 009 4.6H9a2 2 0 114 0v.09a1.65 1.65 0 002.9 1.09l.06-.06a2 2 0 112.83 2.83l-.06.06A1.65 1.65 0 0019.4 9H21a2 2 0 110 4h-.09a1.65 1.65 0 00-1.51 1z",
};

export function Icon({ name }: { name: string }) {
  return (
    <span className="ico">
      <svg viewBox="0 0 24 24">
        <path d={PATHS[name] ?? PATHS.file} strokeLinecap="round" strokeLinejoin="round" />
      </svg>
    </span>
  );
}

// The evald mark from docs/images/evald-logo.svg — two trace bars, the child one
// indented, over a brand-blue baseline. Same geometry as the source file, colour
// inverted: that file is drawn dark-on-light for the README and a favicon, and its
// opaque #fcfcfd ground would sit in this dark-only console as a white tile. Fills
// come from the theme tokens so the mark follows `dim` / `midnight` like everything
// else. Inline rather than an <img> so it costs no second request inside the
// rust-embed bundle and can pick up those tokens at all.
export function BrandMark() {
  return (
    <span className="brand-glyph" aria-hidden="true">
      <svg viewBox="0 0 24 24" fill="none">
        <rect x="3" y="4.5" width="13" height="3.2" fill="var(--eg-fg)" />
        <rect x="7" y="9.6" width="9" height="3.2" fill="var(--eg-fg)" opacity="0.55" />
        <rect x="3" y="17" width="18" height="3" fill="var(--eg-brand)" />
      </svg>
    </span>
  );
}
