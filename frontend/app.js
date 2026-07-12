/* =========================================================================
   evald UI — app.js
   Vanilla JS, no dependencies, same-origin fetches only.
   Two views: Traces (default) and SQL.
   ========================================================================= */
'use strict';

/* ----------------------------------------------------------------------
   Section 0: small DOM + formatting helpers
   ---------------------------------------------------------------------- */

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

/** Escape a string for safe insertion into HTML. */
function escapeHtml(value) {
  if (value === null || value === undefined) return '';
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/** Create an element with optional class, text and attributes. */
function el(tag, opts = {}) {
  const node = document.createElement(tag);
  if (opts.class) node.className = opts.class;
  if (opts.text !== undefined) node.textContent = opts.text;
  if (opts.html !== undefined) node.innerHTML = opts.html; // ONLY for trusted, pre-escaped html
  if (opts.title !== undefined) node.title = opts.title;
  if (opts.attrs) for (const [k, v] of Object.entries(opts.attrs)) node.setAttribute(k, v);
  return node;
}

/** Nanoseconds -> milliseconds (loses ns precision; fine for display). */
function nanoToMs(nano) {
  if (typeof nano !== 'number' || !isFinite(nano)) return null;
  return nano / 1e6;
}

/** Format a duration given start/end nanos. */
function durationMs(span) {
  const s = nanoToMs(span.start_unix_nano);
  const e = nanoToMs(span.end_unix_nano);
  if (s === null || e === null) return null;
  return Math.max(0, e - s);
}

/** Human friendly duration label. */
function fmtDuration(ms) {
  if (ms === null || ms === undefined) return '—';
  if (ms < 1) return ms.toFixed(2) + 'ms';
  if (ms < 1000) return ms.toFixed(ms < 10 ? 1 : 0) + 'ms';
  return (ms / 1000).toFixed(2) + 's';
}

/** Relative "2m ago" style label from a ms epoch. */
function fmtRelative(ms) {
  if (ms === null || ms === undefined) return '';
  const diff = Date.now() - ms;
  if (diff < 0) return 'just now';
  const sec = Math.floor(diff / 1000);
  if (sec < 5) return 'just now';
  if (sec < 60) return sec + 's ago';
  const min = Math.floor(sec / 60);
  if (min < 60) return min + 'm ago';
  const hr = Math.floor(min / 60);
  if (hr < 24) return hr + 'h ago';
  const day = Math.floor(hr / 24);
  if (day < 30) return day + 'd ago';
  return new Date(ms).toLocaleDateString();
}

/** Absolute timestamp string (for title= tooltips). */
function fmtAbsolute(ms) {
  if (ms === null || ms === undefined) return '';
  try { return new Date(ms).toLocaleString(); } catch (_) { return ''; }
}

/** Format a number with thousands separators; tolerant of undefined. */
function fmtNum(n) {
  if (typeof n !== 'number' || !isFinite(n)) return '0';
  return n.toLocaleString();
}

/** OTel SpanKind int -> label (for detail panel). */
const OTEL_KIND_LABEL = {
  0: 'Internal', 1: 'Server', 2: 'Client', 3: 'Producer', 4: 'Consumer', 5: 'Internal',
};

/** Status code -> {label, cls}. */
function statusInfo(code) {
  if (code === 2) return { label: 'Error', cls: 'error' };
  if (code === 1) return { label: 'Ok', cls: 'ok' };
  return { label: 'Unset', cls: 'unset' };
}

/* ----------------------------------------------------------------------
   Section 1: network layer + error banner
   ---------------------------------------------------------------------- */

let bannerTimer = null;
function showBanner(message) {
  const banner = $('#banner');
  $('#banner-msg').textContent = message;
  banner.hidden = false;
  clearTimeout(bannerTimer);
  bannerTimer = setTimeout(hideBanner, 8000);
}
function hideBanner() { $('#banner').hidden = true; }

/** GET JSON from a same-origin path. Throws Error with server text on failure. */
async function getJSON(path) {
  const resp = await fetch(path, { headers: { Accept: 'application/json' } });
  if (!resp.ok) {
    const body = await resp.text().catch(() => '');
    throw new Error(`${resp.status} ${resp.statusText}${body ? ': ' + body.slice(0, 300) : ''}`);
  }
  return resp.json();
}

/* ----------------------------------------------------------------------
   Section 2: global app state
   ---------------------------------------------------------------------- */

const state = {
  view: 'traces',
  spans: [],              // all spans from /v1/spans?limit=1000
  traces: [],             // grouped + summarized traces, newest-first (each carries a .haystack)
  filter: '',             // trace-list full-text filter (AND of whitespace-split terms)
  selectedTraceId: null,
  traceSpans: [],         // spans of the currently open trace
  selectedSpanId: null,
  collapsed: new Set(),   // span_ids that are collapsed in the tree
};

/* Full-text search / payload-virtualization tunables. */
const SEARCH_FIELD_CAP = 2000;    // per-span, per-field cap when building the search index
const INLINE_DISPLAY_CAP = 20000; // chars of a payload rendered inline before a "show all" gate
const ATTR_VALUE_CAP = 600;       // chars per raw-attribute cell before truncation
const BLOB_REF_PREFIX = 'evald-blob:';

/* Dashboard tunables. */
const OTEL_STATUS_ERROR = 2;      // OTel StatusCode.ERROR (status_code === 2)
const TOP_MODELS_LIMIT = 8;       // rows in the "top models" chart
const HEALTH_CACHE_MS = 5000;     // reuse a recent /v1/stats fetch rather than re-hitting it

/** True when a value is an offloaded-payload reference (see the blob store). */
function isBlobRef(v) {
  return typeof v === 'string' && v.startsWith(BLOB_REF_PREFIX);
}
function blobKey(ref) {
  return ref.slice(BLOB_REF_PREFIX.length);
}

/* ----------------------------------------------------------------------
   Section 3: data loading + trace grouping
   ---------------------------------------------------------------------- */

async function loadSpans() {
  const list = $('#trace-list');
  list.innerHTML = '<div class="loading">Loading…</div>';
  try {
    const spans = await getJSON('/v1/spans?limit=1000');
    state.spans = Array.isArray(spans) ? spans : [];
    state.traces = groupIntoTraces(state.spans);
    updateHeaderStats();
    renderTraceList();
    if (state.view === 'dashboard') renderDashboard();
    // If a previously selected trace is gone, clear detail.
    if (state.selectedTraceId &&
        !state.traces.some(t => t.trace_id === state.selectedTraceId)) {
      clearTraceDetail();
    }
  } catch (err) {
    showBanner('Failed to load spans — ' + err.message);
    state.spans = [];
    state.traces = [];
    updateHeaderStats();
    renderTraceList();
  }
}

/** Group flat spans into trace summaries, newest-first. */
function groupIntoTraces(spans) {
  const byTrace = new Map();
  for (const sp of spans) {
    if (!sp || !sp.trace_id) continue;
    if (!byTrace.has(sp.trace_id)) byTrace.set(sp.trace_id, []);
    byTrace.get(sp.trace_id).push(sp);
  }

  const traces = [];
  for (const [traceId, group] of byTrace) {
    // Root = span with no parent_span_id; else earliest start.
    let root = group.find(s => !s.parent_span_id);
    if (!root) {
      root = group.reduce((a, b) =>
        (a.start_unix_nano ?? Infinity) <= (b.start_unix_nano ?? Infinity) ? a : b);
    }

    let totalTokens = 0;
    let minStart = Infinity;
    let hasError = false;
    let serviceName;
    for (const s of group) {
      const t = s.tokens && typeof s.tokens.total === 'number' ? s.tokens.total : 0;
      totalTokens += t;
      if (typeof s.start_unix_nano === 'number' && s.start_unix_nano < minStart) {
        minStart = s.start_unix_nano;
      }
      if (s.status_code === 2) hasError = true;
      if (!serviceName && s.service_name) serviceName = s.service_name;
    }

    traces.push({
      trace_id: traceId,
      name: root && root.name ? root.name : '(unnamed trace)',
      service_name: serviceName,
      span_count: group.length,
      total_tokens: totalTokens,
      start_ms: isFinite(minStart) ? minStart / 1e6 : null,
      start_nano: isFinite(minStart) ? minStart : 0,
      has_error: hasError,
      haystack: buildHaystack(group, traceId),
    });
  }

  // Newest-first by earliest start.
  traces.sort((a, b) => b.start_nano - a.start_nano);
  return traces;
}

/**
 * Build a lowercased full-text search index for a trace: every searchable field of every
 * span (identity, model/provider, service/session/user, input/output, and raw attribute
 * keys+values), joined by a separator so a term never matches across a field boundary. Each
 * field is length-capped so one huge payload can't blow up the index. Substring-based, so it
 * needs no word tokenization — CJK and other non-space-delimited scripts match naturally.
 */
function buildHaystack(group, traceId) {
  const parts = [traceId];
  const push = (v) => {
    if (typeof v === 'string' && v) parts.push(v.length > SEARCH_FIELD_CAP ? v.slice(0, SEARCH_FIELD_CAP) : v);
  };
  for (const s of group) {
    push(s.name);
    push(s.model);
    push(s.provider);
    push(s.service_name);
    push(s.session_id);
    push(s.user_id);
    push(s.oi_kind);
    push(s.status_message);
    push(s.input_value);
    push(s.output_value);
    if (s.raw_attributes && typeof s.raw_attributes === 'object') {
      for (const [k, v] of Object.entries(s.raw_attributes)) {
        push(k);
        push(typeof v === 'string' ? v : JSON.stringify(v));
      }
    }
  }
  return parts.join('').toLowerCase();
}

/** The active filter as normalized AND-terms (whitespace-split, lowercased, empties dropped). */
function currentFilterTerms() {
  return state.filter.toLowerCase().split(/\s+/).filter(Boolean);
}

/** A trace matches when EVERY term is a substring of its haystack (reliable AND semantics). */
function traceMatches(trace, terms) {
  const hay = trace.haystack || '';
  return terms.every((t) => hay.includes(t));
}

function updateHeaderStats() {
  $('#stat-traces').textContent = `${fmtNum(state.traces.length)} traces`;
  $('#stat-spans').textContent = `${fmtNum(state.spans.length)} spans`;
}

/* ----------------------------------------------------------------------
   Section 4: trace list rendering
   ---------------------------------------------------------------------- */

function renderTraceList() {
  const list = $('#trace-list');
  const terms = currentFilterTerms();
  const all = state.traces;
  const visible = terms.length ? all.filter((t) => traceMatches(t, terms)) : all;

  // Count shows "matched / total" while filtering, else just the total.
  $('#trace-list-count').textContent = terms.length
    ? `${visible.length} / ${all.length}`
    : (all.length ? `${all.length}` : '');

  list.innerHTML = '';

  if (all.length === 0) {
    list.appendChild(renderTracesEmptyState());
    return;
  }
  if (visible.length === 0) {
    list.appendChild(renderNoMatchState());
    return;
  }

  for (const tr of visible) {
    const card = el('button', { class: 'trace-card', attrs: { type: 'button' } });
    card.dataset.traceId = tr.trace_id;
    if (tr.trace_id === state.selectedTraceId) card.classList.add('selected');

    // top row: name + error dot
    const top = el('div', { class: 'trace-card-top' });
    top.appendChild(el('span', { class: 'trace-card-name', text: tr.name, title: tr.name }));
    if (tr.has_error) {
      top.appendChild(el('span', { class: 'trace-err-dot', title: 'Has error span(s)' }));
    }
    card.appendChild(top);

    // meta row
    const meta = el('div', { class: 'trace-card-meta' });
    if (tr.service_name) {
      meta.appendChild(el('span', { class: 'svc', text: tr.service_name, title: tr.service_name }));
    }
    meta.appendChild(el('span', { text: `${tr.span_count} span${tr.span_count === 1 ? '' : 's'}` }));
    if (tr.total_tokens > 0) {
      meta.appendChild(el('span', { text: `${fmtNum(tr.total_tokens)} tok` }));
    }
    meta.appendChild(el('span', {
      class: 'trace-card-time',
      text: fmtRelative(tr.start_ms),
      title: fmtAbsolute(tr.start_ms),
    }));
    card.appendChild(meta);

    card.addEventListener('click', () => selectTrace(tr.trace_id));
    list.appendChild(card);
  }
}

function renderNoMatchState() {
  const wrap = el('div', { class: 'empty-state' });
  wrap.appendChild(el('h3', { text: 'No matching traces' }));
  wrap.appendChild(el('p', {
    text: `Nothing matches “${state.filter.trim()}”. Search covers span name, model, `
      + `provider, service, input/output and raw attributes. Press Esc to clear.`,
  }));
  return wrap;
}

function renderTracesEmptyState() {
  const origin = window.location.origin || '<this server>';
  const wrap = el('div', { class: 'empty-state' });
  wrap.appendChild(el('h3', { text: 'No traces yet' }));
  wrap.appendChild(el('p', {
    text: 'Point an OTel / OpenInference exporter at this server to start capturing LLM traces.',
  }));
  const p2 = el('p', {});
  p2.appendChild(document.createTextNode('Set your exporter endpoint to this origin:'));
  wrap.appendChild(p2);
  wrap.appendChild(el('code', {
    class: 'codeblock',
    text: `OTEL_EXPORTER_OTLP_ENDPOINT=${origin}\n# traces are received at ${origin}/v1/traces`,
  }));
  return wrap;
}

/* ----------------------------------------------------------------------
   Section 5: trace selection + span tree
   ---------------------------------------------------------------------- */

async function selectTrace(traceId) {
  state.selectedTraceId = traceId;
  state.selectedSpanId = null;
  state.collapsed = new Set();
  $('#span-detail').hidden = true;

  // highlight in list
  $$('.trace-card').forEach(c =>
    c.classList.toggle('selected', c.dataset.traceId === traceId));

  $('#trace-detail-title').textContent = 'Loading trace…';
  $('#trace-detail-id').textContent = traceId;
  $('#span-tree').innerHTML = '<div class="loading">Loading…</div>';

  try {
    const spans = await getJSON('/v1/traces/' + encodeURIComponent(traceId));
    state.traceSpans = Array.isArray(spans) ? spans : [];
    renderTraceDetail();
  } catch (err) {
    showBanner('Failed to load trace — ' + err.message);
    $('#span-tree').innerHTML = '';
    $('#span-tree').appendChild(el('div', { class: 'loading', text: 'Could not load this trace.' }));
  }
}

function clearTraceDetail() {
  state.selectedTraceId = null;
  state.selectedSpanId = null;
  state.traceSpans = [];
  $('#trace-detail-title').textContent = 'Select a trace';
  $('#trace-detail-id').textContent = '';
  $('#span-tree').innerHTML = '';
  $('#span-detail').hidden = true;
}

function renderTraceDetail() {
  const spans = state.traceSpans;
  const tree = $('#span-tree');
  tree.innerHTML = '';

  if (spans.length === 0) {
    tree.appendChild(el('div', { class: 'loading', text: 'This trace has no spans.' }));
    return;
  }

  // title: name of root span
  const root = spans.find(s => !s.parent_span_id) || spans[0];
  $('#trace-detail-title').textContent = (root && root.name) || '(trace)';

  // Compute trace time window for the waterfall bars.
  let winMin = Infinity, winMax = -Infinity;
  for (const s of spans) {
    if (typeof s.start_unix_nano === 'number') winMin = Math.min(winMin, s.start_unix_nano);
    if (typeof s.end_unix_nano === 'number') winMax = Math.max(winMax, s.end_unix_nano);
  }
  const winSpan = (isFinite(winMin) && isFinite(winMax) && winMax > winMin)
    ? (winMax - winMin) : 1;

  // Build parent -> children map.
  const byId = new Map();
  for (const s of spans) if (s.span_id) byId.set(s.span_id, s);
  const children = new Map();
  const roots = [];
  for (const s of spans) {
    const pid = s.parent_span_id;
    if (pid && byId.has(pid)) {
      if (!children.has(pid)) children.set(pid, []);
      children.get(pid).push(s);
    } else {
      roots.push(s); // root or orphan (parent not in this trace)
    }
  }
  // chronological order within each sibling group
  const byStart = (a, b) => (a.start_unix_nano ?? 0) - (b.start_unix_nano ?? 0);
  roots.sort(byStart);
  for (const arr of children.values()) arr.sort(byStart);

  // Depth-first render respecting collapse state.
  const renderNode = (span, depth) => {
    const kids = children.get(span.span_id) || [];
    const row = buildSpanRow(span, depth, kids.length > 0, winMin, winSpan);
    tree.appendChild(row);
    if (kids.length && !state.collapsed.has(span.span_id)) {
      for (const k of kids) renderNode(k, depth + 1);
    }
  };
  for (const r of roots) renderNode(r, 0);
}

function buildSpanRow(span, depth, hasChildren, winMin, winSpan) {
  const row = el('div', { class: 'span-row' });
  row.dataset.spanId = span.span_id || '';
  if (span.span_id === state.selectedSpanId) row.classList.add('selected');
  row.style.paddingLeft = (14 + depth * 16) + 'px';

  // toggle
  const toggle = el('span', { class: 'span-toggle' });
  if (hasChildren) {
    toggle.textContent = state.collapsed.has(span.span_id) ? '▸' : '▾';
    toggle.title = 'Expand / collapse';
    toggle.addEventListener('click', (e) => {
      e.stopPropagation();
      if (state.collapsed.has(span.span_id)) state.collapsed.delete(span.span_id);
      else state.collapsed.add(span.span_id);
      renderTraceDetail();
    });
  } else {
    toggle.classList.add('placeholder');
  }
  row.appendChild(toggle);

  // name
  row.appendChild(el('span', {
    class: 'span-name',
    text: span.name || '(unnamed)',
    title: span.name || '',
  }));

  // oi_kind badge
  if (span.oi_kind) {
    const kindCls = 'kind-' + String(span.oi_kind).toUpperCase().replace(/[^A-Z]/g, '');
    row.appendChild(el('span', {
      class: `kind-badge ${kindCls}`,
      text: span.oi_kind,
    }));
  }

  // model
  if (span.model) {
    row.appendChild(el('span', { class: 'span-model', text: span.model, title: span.model }));
  }

  // waterfall bar
  const track = el('div', { class: 'span-bar-track' });
  const bar = el('div', { class: 'span-bar' + (span.status_code === 2 ? ' error' : '') });
  if (typeof span.start_unix_nano === 'number' && typeof span.end_unix_nano === 'number') {
    const left = ((span.start_unix_nano - winMin) / winSpan) * 100;
    const width = ((span.end_unix_nano - span.start_unix_nano) / winSpan) * 100;
    bar.style.left = Math.max(0, Math.min(100, left)) + '%';
    bar.style.width = Math.max(0.5, Math.min(100, width)) + '%';
  } else {
    bar.style.left = '0%';
    bar.style.width = '0.5%';
  }
  track.appendChild(bar);
  row.appendChild(track);

  // duration
  row.appendChild(el('span', { class: 'span-dur', text: fmtDuration(durationMs(span)) }));

  // tokens (prompt / completion)
  const tk = span.tokens;
  let tokText = '';
  if (tk) {
    const up = typeof tk.prompt === 'number' ? tk.prompt : null;
    const down = typeof tk.completion === 'number' ? tk.completion : null;
    if (up !== null || down !== null) {
      tokText = `↑${up ?? 0} ↓${down ?? 0}`;
    }
  }
  row.appendChild(el('span', { class: 'span-tokens', text: tokText }));

  // error dot
  if (span.status_code === 2) {
    row.appendChild(el('span', { class: 'span-status-dot', title: 'Error' }));
  }

  row.addEventListener('click', () => selectSpan(span.span_id));
  return row;
}

/* ----------------------------------------------------------------------
   Section 6: span detail panel
   ---------------------------------------------------------------------- */

function selectSpan(spanId) {
  state.selectedSpanId = spanId;
  $$('.span-row').forEach(r =>
    r.classList.toggle('selected', r.dataset.spanId === spanId));
  const span = state.traceSpans.find(s => s.span_id === spanId);
  if (span) renderSpanDetail(span);
}

function renderSpanDetail(span) {
  const panel = $('#span-detail');
  panel.hidden = false;
  panel.innerHTML = '';

  // --- core identity / status section ---
  const st = statusInfo(span.status_code);
  const core = el('div', { class: 'detail-section' });
  core.appendChild(el('div', { class: 'detail-h', text: span.name || '(unnamed span)' }));

  const grid = el('dl', { class: 'kv-grid' });
  const addKV = (key, valueText, valueTitle) => {
    grid.appendChild(el('dt', { text: key }));
    grid.appendChild(el('dd', { text: valueText, title: valueTitle }));
  };
  addKV('span_id', span.span_id || '—');
  addKV('trace_id', span.trace_id || '—');
  if (span.parent_span_id) addKV('parent_span_id', span.parent_span_id);

  // status as a pill
  grid.appendChild(el('dt', { text: 'status' }));
  const stDd = el('dd');
  const pill = el('span', { class: `pill ${st.cls}`, text: st.label });
  stDd.appendChild(pill);
  if (span.status_message) {
    stDd.appendChild(document.createTextNode(' '));
    stDd.appendChild(el('span', { text: span.status_message }));
  }
  grid.appendChild(stDd);

  if (span.oi_kind) addKV('oi_kind', span.oi_kind);
  addKV('otel_kind', OTEL_KIND_LABEL[span.otel_kind] || String(span.otel_kind ?? '—'));
  if (span.dialect) addKV('dialect', span.dialect);
  if (span.model) addKV('model', span.model);
  if (span.provider) addKV('provider', span.provider);
  if (span.service_name) addKV('service', span.service_name);
  if (span.scope_name) {
    addKV('scope', span.scope_name + (span.scope_version ? ' @ ' + span.scope_version : ''));
  }
  if (span.session_id) addKV('session_id', span.session_id);
  if (span.user_id) addKV('user_id', span.user_id);
  if (typeof span.cost_usd === 'number') addKV('cost', '$' + span.cost_usd.toFixed(6));

  const startMs = nanoToMs(span.start_unix_nano);
  if (startMs !== null) addKV('start', fmtAbsolute(startMs), fmtAbsolute(startMs));
  addKV('duration', fmtDuration(durationMs(span)));

  // tokens breakdown
  const tk = span.tokens;
  if (tk) {
    const parts = [];
    const tkFields = [
      ['prompt', 'prompt'], ['completion', 'completion'], ['total', 'total'],
      ['cache_read', 'cache_read'], ['cache_write', 'cache_write'], ['reasoning', 'reasoning'],
    ];
    for (const [field, label] of tkFields) {
      if (typeof tk[field] === 'number') parts.push(`${label} ${fmtNum(tk[field])}`);
    }
    if (parts.length) addKV('tokens', parts.join('  ·  '));
  }

  core.appendChild(grid);
  panel.appendChild(core);

  // --- input / output ---
  const attrs = (span.raw_attributes && typeof span.raw_attributes === 'object') ? span.raw_attributes : {};
  if (span.input_value) {
    panel.appendChild(buildPayloadSection('Input', span.input_value, attrs['evald.blob.input']));
  }
  if (span.output_value) {
    panel.appendChild(buildPayloadSection('Output', span.output_value, attrs['evald.blob.output']));
  }

  // --- scores (async) ---
  const scoresSec = el('div', { class: 'detail-section' });
  scoresSec.appendChild(el('div', { class: 'detail-h', text: 'Scores' }));
  const scoresBody = el('div', { class: 'scores-body' });
  scoresBody.appendChild(el('div', { class: 'loading', text: 'Loading scores…' }));
  scoresSec.appendChild(scoresBody);
  panel.appendChild(scoresSec);
  loadScores(span.span_id, scoresBody);

  // --- raw attributes (collapsible) ---
  if (span.raw_attributes && typeof span.raw_attributes === 'object') {
    const keys = Object.keys(span.raw_attributes);
    if (keys.length) {
      const details = el('details', { class: 'raw-attrs' });
      const summary = el('summary', { text: `Raw attributes (${keys.length})` });
      details.appendChild(summary);
      const table = el('table', { class: 'attr-table' });
      for (const k of keys.sort()) {
        const tr = el('tr');
        tr.appendChild(el('td', { class: 'attr-key', text: k }));
        tr.appendChild(buildAttrValueCell(span.raw_attributes[k]));
        table.appendChild(tr);
      }
      details.appendChild(table);
      panel.appendChild(details);
    }
  }
}

/**
 * Build an Input/Output detail section. Three cases, all kept responsive on huge payloads:
 *  - an offloaded `evald-blob:<key>` reference → the inline preview + a download link;
 *  - a normal value ≤ cap → rendered inline;
 *  - a normal value > cap → first `INLINE_DISPLAY_CAP` chars + a one-click "show all" gate,
 *    so a multi-megabyte inline value never blocks the main thread on first render.
 */
function buildPayloadSection(label, value, blobMeta) {
  const sec = el('div', { class: 'detail-section' });
  sec.appendChild(el('div', { class: 'detail-h', text: label }));
  if (isBlobRef(value)) {
    appendBlobRef(sec, value, blobMeta);
  } else {
    appendPayloadBlock(sec, value);
  }
  return sec;
}

/** Append a `<pre>` of `text`, gating anything past `INLINE_DISPLAY_CAP` behind a button. */
function appendPayloadBlock(section, text) {
  const pre = el('pre', { class: 'pre-block' });
  if (text.length <= INLINE_DISPLAY_CAP) {
    pre.textContent = text;
    section.appendChild(pre);
    return;
  }
  pre.textContent = text.slice(0, INLINE_DISPLAY_CAP);
  section.appendChild(pre);
  const more = el('button', {
    class: 'show-all-btn',
    text: `Show all (${fmtNum(text.length)} chars)`,
    attrs: { type: 'button' },
  });
  more.addEventListener('click', () => {
    pre.textContent = text; // user opted in to the full payload
    more.remove();
  });
  section.appendChild(more);
}

/** Render an offloaded payload: its inline preview (if any) + a link to fetch the full blob. */
function appendBlobRef(section, ref, meta) {
  const key = blobKey(ref);
  if (meta && typeof meta.preview === 'string' && meta.preview) {
    const truncated = typeof meta.bytes === 'number' && meta.bytes > meta.preview.length;
    section.appendChild(el('pre', {
      class: 'pre-block',
      text: meta.preview + (truncated ? '…' : ''),
    }));
  }
  const bytes = meta && typeof meta.bytes === 'number' ? meta.bytes : null;
  const info = el('div', { class: 'blob-ref' });
  info.appendChild(el('a', {
    class: 'blob-link',
    text: bytes !== null
      ? `Download full payload (${fmtNum(bytes)} bytes)`
      : 'Download full payload',
    attrs: { href: '/v1/blobs/' + encodeURIComponent(key), target: '_blank', rel: 'noopener' },
  }));
  section.appendChild(info);
}

/** A raw-attribute value cell: blob refs become links; long values are capped (with a title). */
function buildAttrValueCell(v) {
  if (isBlobRef(v)) {
    const key = blobKey(v);
    const td = el('td');
    td.appendChild(el('a', {
      class: 'blob-link',
      text: `blob ${key.slice(0, 12)}…`,
      title: v,
      attrs: { href: '/v1/blobs/' + encodeURIComponent(key), target: '_blank', rel: 'noopener' },
    }));
    return td;
  }
  const vText = typeof v === 'string' ? v : JSON.stringify(v);
  if (vText.length > ATTR_VALUE_CAP) {
    // Cap both the rendered text and the tooltip so one giant attribute can't jank the panel.
    return el('td', { text: vText.slice(0, ATTR_VALUE_CAP) + '…', title: vText.slice(0, 2000) });
  }
  return el('td', { text: vText });
}

async function loadScores(spanId, container) {
  if (!spanId) { container.innerHTML = ''; return; }
  try {
    const scores = await getJSON('/v1/scores?span_id=' + encodeURIComponent(spanId));
    container.innerHTML = '';
    // The selected span may have changed while we awaited; that's fine —
    // container is captured, so stale results just won't be attached anywhere
    // visible if a new render replaced the panel.
    if (!Array.isArray(scores) || scores.length === 0) {
      container.appendChild(el('div', { class: 'loading', text: 'No scores for this span.' }));
      return;
    }
    for (const sc of scores) container.appendChild(buildScoreRow(sc));
  } catch (err) {
    container.innerHTML = '';
    container.appendChild(el('div', { class: 'loading', text: 'Could not load scores.' }));
    showBanner('Failed to load scores — ' + err.message);
  }
}

function buildScoreRow(sc) {
  const row = el('div', { class: 'score-row' });
  row.appendChild(el('span', { class: 'score-name', text: sc.name || '(score)' }));

  // value: num_value or str_value
  let valText = '';
  if (typeof sc.num_value === 'number') {
    valText = String(sc.num_value);
  } else if (sc.str_value !== undefined && sc.str_value !== null) {
    valText = sc.str_value;
  } else {
    valText = '—';
  }
  row.appendChild(el('span', { class: 'score-val', text: valText }));

  if (sc.data_type) row.appendChild(el('span', { class: 'dt-badge', text: sc.data_type }));

  const src = (sc.source || '').toLowerCase();
  const srcCls = ['eval', 'human', 'api'].includes(src) ? 'src-' + src : '';
  if (sc.source) row.appendChild(el('span', { class: `src-badge ${srcCls}`, text: sc.source }));

  if (sc.comment) row.appendChild(el('div', { class: 'score-comment', text: sc.comment }));
  return row;
}

/* ----------------------------------------------------------------------
   Section 7: SQL view
   ---------------------------------------------------------------------- */

const SAMPLE_SQL =
  "SELECT s.model, COUNT(*) AS spans, AVG(sc.num_value) AS avg_score\n" +
  "FROM spans s JOIN scores sc ON sc.target_id = s.span_id AND sc.target_type = 'span'\n" +
  "GROUP BY s.model ORDER BY spans DESC";

const SQL_EXAMPLES = [
  {
    label: 'Top models by tokens',
    sql: 'SELECT model, SUM(total_tokens) t FROM spans GROUP BY model ORDER BY t DESC',
  },
  {
    label: 'Error spans',
    sql: 'SELECT name, status_message, model FROM spans WHERE status_code = 2',
  },
  {
    label: 'Recent spans',
    sql: 'SELECT name, model, total_tokens FROM spans ORDER BY start_unix_nano DESC LIMIT 50',
  },
];

function initSqlView() {
  $('#sql-input').value = SAMPLE_SQL;

  const exWrap = $('#sql-examples');
  exWrap.innerHTML = '';
  for (const ex of SQL_EXAMPLES) {
    const btn = el('button', { class: 'sql-example-btn', text: ex.label, attrs: { type: 'button' } });
    btn.addEventListener('click', () => {
      $('#sql-input').value = ex.sql;
      runSql();
    });
    exWrap.appendChild(btn);
  }

  $('#sql-run').addEventListener('click', runSql);
  $('#sql-input').addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
      e.preventDefault();
      runSql();
    }
  });
}

async function runSql() {
  const sql = $('#sql-input').value.trim();
  const meta = $('#sql-result-meta');
  const errBox = $('#sql-result-error');
  const tableWrap = $('#sql-table-wrap');

  errBox.hidden = true;
  errBox.textContent = '';
  if (!sql) { meta.textContent = ''; tableWrap.innerHTML = ''; return; }

  meta.textContent = 'Running…';
  tableWrap.innerHTML = '';

  try {
    const resp = await fetch('/v1/sql', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify({ sql, limit: 1000 }),
    });

    if (!resp.ok) {
      // 400 -> plain text error body
      const text = await resp.text().catch(() => '');
      meta.textContent = '';
      errBox.hidden = false;
      errBox.textContent = text || `${resp.status} ${resp.statusText}`;
      return;
    }

    const data = await resp.json();
    renderSqlResult(data);
  } catch (err) {
    meta.textContent = '';
    errBox.hidden = false;
    errBox.textContent = 'Request failed — ' + err.message;
  }
}

function renderSqlResult(data) {
  const meta = $('#sql-result-meta');
  const tableWrap = $('#sql-table-wrap');
  const columns = Array.isArray(data.columns) ? data.columns : [];
  const rows = Array.isArray(data.rows) ? data.rows : [];
  const rowCount = typeof data.row_count === 'number' ? data.row_count : rows.length;

  // meta line
  meta.textContent = '';
  meta.appendChild(document.createTextNode(
    `${fmtNum(rowCount)} row${rowCount === 1 ? '' : 's'}`));
  if (data.truncated) {
    meta.appendChild(document.createTextNode('  '));
    meta.appendChild(el('span', { class: 'trunc', text: '(truncated to limit)' }));
  }

  tableWrap.innerHTML = '';
  if (columns.length === 0 || rows.length === 0) {
    tableWrap.appendChild(el('div', { class: 'loading', text: 'No rows returned.' }));
    return;
  }

  const table = el('table', { class: 'sql-table' });

  // header
  const thead = el('thead');
  const htr = el('tr');
  for (const col of columns) htr.appendChild(el('th', { text: String(col) }));
  thead.appendChild(htr);
  table.appendChild(thead);

  // body
  const tbody = el('tbody');
  for (const row of rows) {
    const tr = el('tr');
    for (const col of columns) {
      const v = row ? row[col] : undefined;
      if (v === null || v === undefined) {
        tr.appendChild(el('td', { class: 'null-cell', text: 'null' }));
      } else if (isBlobRef(v)) {
        tr.appendChild(buildAttrValueCell(v)); // offloaded payload → download link
      } else {
        // Cap the rendered cell (and its tooltip) so a huge value can't freeze the grid.
        const sv = typeof v === 'object' ? JSON.stringify(v) : String(v);
        const shown = sv.length > ATTR_VALUE_CAP ? sv.slice(0, ATTR_VALUE_CAP) + '…' : sv;
        tr.appendChild(el('td', { text: shown, title: sv.slice(0, 2000) }));
      }
    }
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);
  tableWrap.appendChild(table);
}

/* ----------------------------------------------------------------------
   Section 7.5: Dashboard view — client-side aggregates over the loaded spans
   ---------------------------------------------------------------------- */

/** The value at percentile `p` (0–100) of an ascending-sorted array; null if empty. */
function percentile(sortedAsc, p) {
  const n = sortedAsc.length;
  if (n === 0) return null;
  // Nearest-rank method (no interpolation): rank = ceil(p/100 · n), 1-indexed, clamped into
  // range. Consistent across array sizes — e.g. p95 always lands on the ceil-rank element.
  const rank = Math.ceil((p / 100) * n);
  const idx = Math.min(n - 1, Math.max(0, rank - 1));
  return sortedAsc[idx];
}

/**
 * Aggregate the loaded span window into dashboard metrics: trace/span counts, total tokens
 * and cost, error rate, latency percentiles, and a per-model rollup (tokens/spans/cost).
 * Pure over its input, so it is unit-testable without the DOM.
 */
function computeDashboard(spans) {
  const traceIds = new Set();
  const durations = [];
  const byModel = new Map();
  let totalTokens = 0, totalCost = 0, errors = 0;

  for (const s of spans) {
    if (s && s.trace_id) traceIds.add(s.trace_id);
    const tk = s && s.tokens && typeof s.tokens.total === 'number' ? s.tokens.total : 0;
    totalTokens += tk;
    const cost = s && typeof s.cost_usd === 'number' ? s.cost_usd : 0;
    totalCost += cost;
    if (s && s.status_code === OTEL_STATUS_ERROR) errors++;
    const d = durationMs(s);
    if (d !== null) durations.push(d);
    if (s && s.model) {
      const m = byModel.get(s.model) || { model: s.model, spans: 0, tokens: 0, cost: 0 };
      m.spans++; m.tokens += tk; m.cost += cost;
      byModel.set(s.model, m);
    }
  }
  durations.sort((a, b) => a - b);
  const topModels = Array.from(byModel.values())
    .sort((a, b) => (b.tokens - a.tokens) || (b.spans - a.spans))
    .slice(0, TOP_MODELS_LIMIT);

  return {
    traces: traceIds.size,
    spans: spans.length,
    totalTokens,
    totalCost,
    errors,
    errorRate: spans.length ? errors / spans.length : 0,
    latency: {
      p50: percentile(durations, 50),
      p95: percentile(durations, 95),
      p99: percentile(durations, 99),
    },
    topModels,
  };
}

function statTile(label, value, cls) {
  const tile = el('div', { class: 'dash-tile' + (cls ? ' ' + cls : '') });
  tile.appendChild(el('div', { class: 'dash-tile-value', text: value }));
  tile.appendChild(el('div', { class: 'dash-tile-label', text: label }));
  return tile;
}

function renderTopModels(models) {
  const sec = el('div', { class: 'dash-section' });
  sec.appendChild(el('div', { class: 'detail-h', text: 'Top models by tokens' }));
  if (models.length === 0) {
    sec.appendChild(el('div', { class: 'loading', text: 'No model spans in this window.' }));
    return sec;
  }
  const max = Math.max(...models.map(m => m.tokens), 1);
  const list = el('div', { class: 'bar-list' });
  for (const m of models) {
    const row = el('div', { class: 'bar-row' });
    row.appendChild(el('span', { class: 'bar-label', text: m.model, title: m.model }));
    const track = el('div', { class: 'bar-track' });
    const fill = el('div', { class: 'bar-fill' });
    fill.style.width = Math.max(2, (m.tokens / max) * 100) + '%';
    track.appendChild(fill);
    row.appendChild(track);
    const meta = `${fmtNum(m.tokens)} tok · ${m.spans} span${m.spans === 1 ? '' : 's'}`
      + (m.cost > 0 ? ` · $${m.cost.toFixed(4)}` : '');
    row.appendChild(el('span', { class: 'bar-value', text: meta }));
    list.appendChild(row);
  }
  sec.appendChild(list);
  return sec;
}

/** Render the dashboard from the currently-loaded spans (+ live ingest health). */
function renderDashboard() {
  const root = $('#view-dashboard');
  root.innerHTML = '';

  if (state.spans.length === 0) {
    const empty = el('div', { class: 'empty-state' });
    empty.appendChild(el('h3', { text: 'No data yet' }));
    empty.appendChild(el('p', { text: 'Send traces to this server, then Refresh to see aggregates.' }));
    root.appendChild(empty);
    return;
  }

  const d = computeDashboard(state.spans);

  root.appendChild(el('div', {
    class: 'dash-note',
    text: `Aggregated over the ${fmtNum(state.spans.length)} most-recent spans loaded.`,
  }));

  const tiles = el('div', { class: 'dash-tiles' });
  tiles.appendChild(statTile('Traces', fmtNum(d.traces)));
  tiles.appendChild(statTile('Spans', fmtNum(d.spans)));
  tiles.appendChild(statTile('Tokens', fmtNum(d.totalTokens)));
  tiles.appendChild(statTile('Cost', '$' + d.totalCost.toFixed(4)));
  tiles.appendChild(statTile(
    'Error rate',
    (d.errorRate * 100).toFixed(1) + '%',
    d.errors ? 'error' : '',
  ));
  root.appendChild(tiles);

  const lat = el('div', { class: 'dash-tiles' });
  lat.appendChild(statTile('Latency p50', fmtDuration(d.latency.p50)));
  lat.appendChild(statTile('Latency p95', fmtDuration(d.latency.p95)));
  lat.appendChild(statTile('Latency p99', fmtDuration(d.latency.p99)));
  root.appendChild(lat);

  root.appendChild(renderTopModels(d.topModels));

  const health = el('div', { class: 'dash-section' });
  health.appendChild(el('div', { class: 'detail-h', text: 'Ingest health' }));
  const healthBody = el('div', { class: 'dash-health' });
  healthBody.appendChild(el('div', { class: 'loading', text: 'Loading…' }));
  health.appendChild(healthBody);
  root.appendChild(health);
  loadIngestHealth(healthBody);
}

// Short-lived cache for /v1/stats so re-rendering the dashboard (tab switch, span refresh)
// doesn't re-hit the endpoint or flash "Loading…" when the data is seconds old.
let healthCache = null; // { at: epoch_ms, stats }

function renderIngestHealth(container, s) {
  container.innerHTML = '';
  const grid = el('dl', { class: 'kv-grid' });
  const add = (k, v, cls) => {
    grid.appendChild(el('dt', { text: k }));
    grid.appendChild(el('dd', { class: cls || '', text: v }));
  };
  add('Hot backlog', `${fmtNum(s.hot_spans)}${s.max_hot_spans ? ' / ' + fmtNum(s.max_hot_spans) : ''} spans`);
  add('Shedding', s.shedding ? 'YES — ingest overloaded' : 'no', s.shedding ? 'null-cell' : '');
  add('Rejections', fmtNum(s.rejections));
  add('Channel capacity', fmtNum(s.channel_capacity));
  container.appendChild(grid);
}

/** Render ingest backlog / shed stats, reusing a recent /v1/stats fetch when fresh. */
async function loadIngestHealth(container) {
  if (healthCache && (Date.now() - healthCache.at) < HEALTH_CACHE_MS) {
    renderIngestHealth(container, healthCache.stats); // fresh enough — no refetch, no flicker
    return;
  }
  try {
    const stats = await getJSON('/v1/stats');
    healthCache = { at: Date.now(), stats };
    renderIngestHealth(container, stats);
  } catch (err) {
    container.innerHTML = '';
    container.appendChild(el('div', { class: 'loading', text: 'Ingest stats unavailable.' }));
  }
}

/* ----------------------------------------------------------------------
   Section 8: view switching + wiring
   ---------------------------------------------------------------------- */

function switchView(view) {
  state.view = view;
  $('#view-traces').hidden = view !== 'traces';
  $('#view-sql').hidden = view !== 'sql';
  $('#view-dashboard').hidden = view !== 'dashboard';
  $$('.nav-btn').forEach(b => b.classList.toggle('active', b.dataset.view === view));
  if (view === 'dashboard') renderDashboard();
}

/** Wire the trace-list full-text filter: debounced re-render on input, Esc to clear. */
let searchTimer = null;
function initTraceSearch() {
  const input = $('#trace-search');
  if (!input) return;
  input.addEventListener('input', () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      state.filter = input.value;
      renderTraceList();
    }, 120);
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && input.value) {
      input.value = '';
      state.filter = '';
      clearTimeout(searchTimer);
      renderTraceList();
    }
  });
}

function init() {
  // nav
  $$('.nav-btn').forEach(btn => {
    btn.addEventListener('click', () => switchView(btn.dataset.view));
  });

  // refresh (manual; no aggressive polling)
  $('#refresh-btn').addEventListener('click', () => {
    loadSpans();
    if (state.selectedTraceId) selectTrace(state.selectedTraceId);
  });

  // banner close
  $('#banner-close').addEventListener('click', hideBanner);

  initTraceSearch();
  initSqlView();
  switchView('traces');
  loadSpans();
}

document.addEventListener('DOMContentLoaded', init);
