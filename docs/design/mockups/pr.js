// PR detail mockup: file tree, filters, single-file diff, threads, review panel.
const $ = (s) => document.querySelector(s);

// ---------- data ----------
// s: A added, M modified, D removed, R renamed. kind: text | image | binary | large
const FILES = [
  { p: "src/components/apps/metadata/metadata-sections.tsx", s: "M", a: 3, d: 3 },
  { p: "src/components/apps/metadata/schema.ts", s: "M", a: 4, d: 1 },
  { p: "src/components/flows/canvas/node-kinds.test.ts", s: "A", a: 21, d: 0 },
  { p: "src/components/flows/canvas/node-kinds.ts", s: "M", a: 8, d: 3 },
  { p: "src/components/flows/nodes/form-shared.ts", s: "M", a: 6, d: 0 },
  { p: "src/components/flows/nodes/node-pane.tsx", s: "M", a: 2, d: 0 },
  { p: "src/components/flows/nodes/push-action-form.tsx", s: "M", a: 29, d: 5, c: 1 },
  { p: "src/components/flows/flow-editor.tsx", s: "M", a: 1, d: 0 },
  { p: "src/components/flows/flow-panes.tsx", s: "M", a: 3, d: 0 },
  { p: "src/components/flows/flow-workspace.tsx", s: "M", a: 6, d: 1 },
  { p: "src/components/notifications/notification-editor.tsx", s: "M", a: 1, d: 0 },
  { p: "src/components/notifications/notification-fields.tsx", s: "M", a: 18, d: 2 },
  { p: "src/components/notifications/notification-kind.ts", s: "R", a: 18, d: 9, prev: "src/components/notifications/kind.ts" },
  { p: "src/components/notifications/use-notification-editor.ts", s: "M", a: 11, d: 0 },
  { p: "src/lib/data-tools/push.test.ts", s: "M", a: 88, d: 1 },
  { p: "src/lib/data-tools/push.ts", s: "M", a: 41, d: 2, c: 2, pend: 1 },
  { p: "src/lib/flows/schema.test.ts", s: "M", a: 6, d: 0 },
  { p: "src/lib/flows/schema.ts", s: "M", a: 3, d: 0 },
  { p: "src/lib/notifications/default-links.test.ts", s: "A", a: 143, d: 0 },
  { p: "src/lib/notifications/default-links.ts", s: "A", a: 22, d: 0, pend: 1 },
  { p: "src/lib/notifications/legacy-links.ts", s: "D", a: 0, d: 42 },
  { p: "src/lib/notifications/schema.test.ts", s: "M", a: 14, d: 0 },
  { p: "src/lib/notifications/schema.ts", s: "M", a: 3, d: 0 },
  { p: "public/icons/push-badge.png", s: "M", a: 0, d: 0, kind: "image" },
  { p: "public/fonts/badge-glyphs.woff2", s: "A", a: 0, d: 0, kind: "binary" },
  { p: "package-lock.json", s: "M", a: 812, d: 640, kind: "large" },
];
FILES.forEach((f) => (f.kind ??= "text"));

const DIFFS = {
  "src/lib/data-tools/push.ts": {
    tail: 41,
    hunks: [
      { os: 7, ns: 7, fn: "", lines: [
        '-import { content } from "@/api";',
        '+import { apps, content } from "@/api";',
        ' import type { ResourceState } from "@/api/pb/journey/journey_resources.pb";',
        " import type {",
        "   Notification,",
        "   NotificationRevision,",
        "   NotificationTemplate,",
        ' } from "@/api/pb/journey/journey_notification.pb";',
        '+import { hasNoLinks, withDefaultLinks } from "@/lib/notifications/default-links";',
        ' import { probeRevisions } from "@/lib/revision";',
        ' import { resolveMaskPaths } from "./mask";',
        ' import { PUSH_NOTIFICATION, type DataRecord, type DataToolContext, type WriteInput } from "./types";',
      ] },
      { os: 71, ns: 72, fn: "async function createPushTemplate(", lines: [
        "   });",
        "   const template = created.template ?? {};",
        "   let revision = created.revision ?? {};",
        "+  const prefilled = await withCreateDefaults(ctx, envelope.notification);",
        "   // createPushTemplate seeds revision 1 with an empty Notification, so a",
        "   // create that stopped here would hand the user an empty shell.",
        "   if (envelope.notification) {",
        "     revision = await writeRevision(",
        '       template.id ?? "",',
        "       revision.revision ?? 1,",
        "       revision.version ?? 0,",
        "-      envelope.notification,",
        "+      prefilled ?? envelope.notification,",
        "     );",
        "+  } else if (prefilled) {",
        "+    // Only the default links to write. The template exists already, so a",
        "+    // failure here must not read as a failed create: a retry would mint a",
        "+    // second template.",
        "+    try {",
        "+      revision = await writeRevision(",
        '+        template.id ?? "",',
        "+        revision.revision ?? 1,",
        "+        revision.version ?? 0,",
        "+        prefilled,",
        "+      );",
        "+    } catch (err) {",
        "+      return {",
        "+        template,",
        "+        warning: `Default links were not saved: ${String(err)}`,",
        "+      };",
        "+    }",
        "   }",
        "   return { template, revision };",
        " }",
      ] },
      { os: 118, ns: 138, fn: "", lines: [
        " }",
        " ",
        "+/** Fills deep links the author left empty with the app's bare scheme. */",
        "+async function withCreateDefaults(",
        "+  ctx: DataToolContext,",
        "+  notification: Notification | undefined,",
        "+): Promise<Notification | undefined> {",
        "+  if (!notification || !hasNoLinks(notification)) return undefined;",
        "+  const app = await apps.getApp({ id: ctx.appId });",
        "+  return withDefaultLinks(notification, app.scheme);",
        "+}",
        "+",
        " export async function readPush(ctx: DataToolContext, id: string) {",
        "   const res = await content.getPushTemplate({ id });",
      ] },
    ],
  },
  "src/lib/notifications/default-links.ts": {
    hunks: [
      { os: 0, ns: 1, fn: "", lines: [
        '+import type { Notification } from "@/api/pb/journey/journey_notification.pb";',
        "+",
        "+/** Matches a bare app scheme like `kedis://` with nothing after it. */",
        "+const BARE_SCHEME = /^[a-z][a-z0-9+.-]*:\\/\\/$/i;",
        "+",
        "+export function hasNoLinks(n: Notification): boolean {",
        "+  return !n.deepLink && !(n.actions ?? []).some((a) => a.deepLink);",
        "+}",
        "+",
        "+export function withDefaultLinks(n: Notification, scheme: string): Notification {",
        '+  const bare = scheme.endsWith("://") ? scheme : `${scheme}://`;',
        "+  if (!BARE_SCHEME.test(bare)) {",
        "+    throw new Error(`not a bare scheme: ${scheme}`);",
        "+  }",
        "+  return {",
        "+    ...n,",
        "+    deepLink: n.deepLink || bare,",
        "+    actions: (n.actions ?? []).map((a) => ({",
        "+      ...a,",
        "+      deepLink: a.deepLink || bare,",
        "+    })),",
        "+  };",
        "+}",
      ] },
    ],
  },
};

const THREADS = {
  "src/lib/data-tools/push.ts": [
    { side: "R", line: 8, resolved: true, comments: [
      { who: "sercand", when: "5d", body: "Import order: keep `apps` next to `content`." },
    ] },
    { side: "R", line: 75, comments: [
      { who: "egealtar", when: "2d", body: "<p>This runs an <code>apps.getApp</code> lookup on every create. Can we skip it when the author already set links?</p>" },
      { who: "dogacanaydin", when: "1d", body: "<p>It returns before the lookup when any link is set; see <code>withCreateDefaults</code> at line 145.</p>" },
    ] },
    { side: "R", line: 99, pending: true, comments: [
      { who: "sercand", when: "now", body: "<p>Should the warning include the template id? It makes the toast actionable.</p>",
        suggestion: ["        warning: `Default links were not saved: ${String(err)}`,", "        warning: `Default links for ${template.id} were not saved: ${String(err)}`,"] },
    ] },
  ],
  "src/lib/notifications/default-links.ts": [
    { side: "R", line: 4, pending: true, comments: [
      { who: "sercand", when: "now", body: "<p>nit: export the scheme regex so the form can validate as you type.</p>" },
    ] },
  ],
};

// ---------- state (persisted bits survive reloads, like the session file) ----------
const state = {
  file: store.get("pr19.file", "src/lib/data-tools/push.ts"),
  view: store.get("view", "split"),
  wrap: store.get("wrap", true),
  ws: false,
  hideViewed: store.get("hideViewed", false),
  bySize: false,
  exts: new Set(),
  statuses: new Set(),
  path: "",
  viewed: new Set(store.get("pr19.viewed", [
    "src/components/apps/metadata/metadata-sections.tsx",
    "src/components/apps/metadata/schema.ts",
    "src/components/flows/canvas/node-kinds.test.ts",
    "src/components/flows/canvas/node-kinds.ts",
    "src/components/flows/flow-editor.tsx",
  ])),
  collapsed: new Set(store.get("pr19.collapsed", [])),
  details: store.get("details", true),
  dtab: store.get("dtab", "overview"),
  composer: null,
  imgMode: "2up",
  verdict: "COMMENT",
};
const saveViewed = () => store.set("pr19.viewed", [...state.viewed]);

const extOf = (p) => { const n = p.split("/").pop(); const i = n.lastIndexOf("."); return i > 0 ? n.slice(i + 1) : "(none)"; };
const nameOf = (p) => p.split("/").pop();
const cur = () => FILES.find((f) => f.p === state.file) || FILES[0];
const STATUS = { A: ["Added", "ok"], M: ["Modified", "warn"], D: ["Removed", "bad"], R: ["Renamed", "info"] };

function passes(f, ignoreViewed) {
  if (state.exts.size && !state.exts.has(extOf(f.p))) return false;
  if (state.statuses.size && !state.statuses.has(f.s)) return false;
  if (state.path) {
    const re = new RegExp(state.path.split("*").map((s) => s.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*"), "i");
    if (!re.test(f.p)) return false;
  }
  if (!ignoreViewed && state.hideViewed && state.viewed.has(f.p)) return false;
  return true;
}

// ---------- tree ----------
function buildTree(files) {
  const root = { name: "", path: "", dirs: {}, files: [] };
  for (const f of files) {
    const parts = f.p.split("/");
    let n = root;
    parts.slice(0, -1).forEach((part, i) => {
      n = n.dirs[part] ??= { name: part, path: parts.slice(0, i + 1).join("/"), dirs: {}, files: [] };
    });
    n.files.push(f);
  }
  (function compress(n) {
    for (const k of Object.keys(n.dirs)) {
      let c = n.dirs[k];
      while (!c.files.length && Object.keys(c.dirs).length === 1) {
        const only = Object.values(c.dirs)[0];
        c = { ...only, name: c.name + "/" + only.name };
      }
      n.dirs[k] = c;
      compress(c);
    }
  })(root);
  return root;
}
function orderedFiles(n, out = []) {
  Object.values(n.dirs).sort((a, b) => a.name.localeCompare(b.name)).forEach((d) => orderedFiles(d, out));
  [...n.files].sort(fileSort).forEach((f) => out.push(f));
  return out;
}
const fileSort = (a, b) => state.bySize ? b.a + b.d - (a.a + a.d) : nameOf(a.p).localeCompare(nameOf(b.p));
const visibleFiles = () => orderedFiles(buildTree(FILES.filter((f) => passes(f))));

function tri(dirPath) {
  const ds = FILES.filter((f) => passes(f, true) && f.p.startsWith(dirPath + "/"));
  const v = ds.filter((f) => state.viewed.has(f.p)).length;
  return v === 0 ? "" : v === ds.length ? "on" : "some";
}

function renderTree() {
  const vis = FILES.filter((f) => passes(f));
  const root = buildTree(vis);
  let html = "";
  (function walk(n, d) {
    Object.values(n.dirs).sort((a, b) => a.name.localeCompare(b.name)).forEach((dir) => {
      const open = !state.collapsed.has(dir.path);
      const fs = vis.filter((f) => f.p.startsWith(dir.path + "/"));
      const a = fs.reduce((s, f) => s + f.a, 0), dl = fs.reduce((s, f) => s + f.d, 0);
      const t = tri(dir.path);
      html += `<div class="node dir" style="--d:${d}" onclick="toggleDir('${dir.path}')">
        <span class="chev">${ico(open ? "chevron-down" : "chevron-right")}</span>${folderIcon(dir.name, open)}
        <span class="nm">${dir.name}</span>
        <span class="end">${a + dl ? `<span class="stat"><span class="add">+${a}</span> <span class="del">−${dl}</span></span>` : ""}
        <button class="check ${t}" onclick="event.stopPropagation();toggleFolder('${dir.path}')" data-tip="${t === "on" ? "Unmark" : "Mark"} folder viewed" data-tip-pos="left">${t === "on" ? ico("check") : ""}</button></span></div>`;
      if (open) walk(dir, d + 1);
    });
    [...n.files].sort(fileSort).forEach((f) => {
      const v = state.viewed.has(f.p);
      const cm = (f.c ? `<span class="cmt">${ico("message-square")}${f.c}</span>` : "") + (f.pend ? `<span class="cmt pend" data-tip="Pending in your review">${ico("message-square-dashed")}${f.pend}</span>` : "");
      const stat = f.kind === "text" || f.kind === "large" ? `<span class="stat"><span class="add">+${f.a}</span> <span class="del">−${f.d}</span></span>` : "";
      html += `<div class="node file ${f.p === state.file ? "sel" : ""} ${v ? "viewed" : ""}" style="--d:${d}" onclick="openFile('${f.p}')" title="${f.p}">
        <span class="chev"></span>${fileIcon(f.p)}<span class="nm">${nameOf(f.p)}</span>
        <span class="end">${cm}${stat}<span class="st st-${f.s}" data-tip="${STATUS[f.s][0]}" data-tip-pos="left">${f.s}</span>
        <button class="check ${v ? "on" : ""}" onclick="event.stopPropagation();toggleViewed('${f.p}')">${v ? ico("check") : ""}</button></span></div>`;
    });
  })(root, 0);
  $("#tree").innerHTML = html || `<div class="placeholder" style="margin-top:30px"><p>No files match these filters.</p><button class="btn" onclick="clearFilters()">${ico("x")}Clear filters</button></div>`;

  const total = FILES.length, viewed = FILES.filter((f) => state.viewed.has(f.p)).length;
  const filtered = state.exts.size || state.statuses.size || state.path;
  const fbase = FILES.filter((f) => passes(f, true));
  const fv = fbase.filter((f) => state.viewed.has(f.p)).length;
  $("#prog-txt").textContent = filtered ? `${fv} of ${fbase.length} shown viewed, ${viewed} of ${total} total` : `${viewed} of ${total} viewed`;
  $("#prog-bar").style.width = (filtered ? fv / (fbase.length || 1) : viewed / total) * 100 + "%";
  const sa = vis.reduce((s, f) => s + f.a, 0), sd = vis.reduce((s, f) => s + f.d, 0);
  $("#sum").innerHTML = `${vis.length} files <span class="add">+${sa}</span> <span class="del">−${sd}</span>`;
  $("#hide-viewed").classList.toggle("on", state.hideViewed);
  $("#sort-size").classList.toggle("on", state.bySize);
  const nf = state.exts.size + state.statuses.size;
  $("#filter-btn").classList.toggle("on", nf > 0);
  $("#filter-btn").innerHTML = ico("list-filter") + (nf ? `<span class="dot">${nf}</span>` : "");
  $("#chips").innerHTML =
    [...state.exts].map((e) => `<button class="chip" onclick="toggleExt('${e}')">${fileIcon("x." + e)}.${e}${ico("x")}</button>`).join("") +
    [...state.statuses].map((s) => `<button class="chip" onclick="toggleStatus('${s}')"><b class="st-${s}">${s}</b>${STATUS[s][0]}${ico("x")}</button>`).join("");
  $("#chips").style.display = nf ? "flex" : "none";
  icons();
}

function renderFilterPop(q = "") {
  const counts = {};
  FILES.forEach((f) => (counts[extOf(f.p)] = (counts[extOf(f.p)] || 0) + 1));
  $("#ext-opts").innerHTML = Object.entries(counts)
    .filter(([e]) => e.includes(q.replace(/^\./, "").toLowerCase()))
    .sort((a, b) => b[1] - a[1])
    .map(([e, n]) => `<button class="opt ${state.exts.has(e) ? "on" : ""}" onclick="toggleExt('${e}')"><span class="box">${state.exts.has(e) ? ico("check") : ""}</span>${fileIcon("x." + e)}<span>.${e}</span><span class="n">${n}</span></button>`)
    .join("") || `<div class="pop-head">No file types match</div>`;
  $("#status-opts").innerHTML = Object.entries(STATUS)
    .map(([s, [label]]) => `<button class="opt ${state.statuses.has(s) ? "on" : ""}" onclick="toggleStatus('${s}')"><span class="box">${state.statuses.has(s) ? ico("check") : ""}</span><b class="st-${s} mono">${s}</b>${label}<span class="n">${FILES.filter((f) => f.s === s).length}</span></button>`)
    .join("");
  icons();
}

function toggleExt(e) { state.exts.has(e) ? state.exts.delete(e) : state.exts.add(e); renderFilterPop($("#filter-pop input").value); renderTree(); }
function toggleStatus(s) { state.statuses.has(s) ? state.statuses.delete(s) : state.statuses.add(s); renderFilterPop(); renderTree(); }
function clearFilters() { state.exts.clear(); state.statuses.clear(); state.path = ""; $("#pathq").value = ""; closePops(); renderFilterPop(); renderTree(); }
function toggleDir(p) { state.collapsed.has(p) ? state.collapsed.delete(p) : state.collapsed.add(p); store.set("pr19.collapsed", [...state.collapsed]); renderTree(); }
function collapseAll() {
  const dirs = new Set(); FILES.forEach((f) => { const ps = f.p.split("/"); for (let i = 1; i < ps.length; i++) dirs.add(ps.slice(0, i).join("/")); });
  state.collapsed = state.collapsed.size ? new Set() : dirs; store.set("pr19.collapsed", [...state.collapsed]); renderTree();
}
function toggleViewed(p) { state.viewed.has(p) ? state.viewed.delete(p) : state.viewed.add(p); saveViewed(); renderTree(); if (p === state.file) renderHead(); }
function toggleFolder(dir) {
  // snapshot, then toggle (spec §10.3)
  const snap = FILES.filter((f) => passes(f, true) && f.p.startsWith(dir + "/"));
  const all = snap.every((f) => state.viewed.has(f.p));
  snap.forEach((f) => (all ? state.viewed.delete(f.p) : state.viewed.add(f.p)));
  saveViewed(); renderTree(); renderHead();
}
function markAll() { FILES.filter((f) => passes(f, true)).forEach((f) => state.viewed.add(f.p)); saveViewed(); renderTree(); renderHead(); toast("Marked all viewed", "check-check"); }

// ---------- file view ----------
function openFile(p) {
  state.file = p; state.composer = null; store.set("pr19.file", p);
  renderTree(); renderFile();
  $("#diffwrap").scrollTop = 0;
  document.querySelector(".node.sel")?.scrollIntoView({ block: "nearest" });
}
function step(dir) {
  const list = visibleFiles(); const i = list.findIndex((f) => f.p === state.file);
  const n = list[Math.max(0, Math.min(list.length - 1, i + dir))]; if (n) openFile(n.p);
}
function viewedAndNext() { state.viewed.add(state.file); saveViewed(); const list = visibleFiles(); const i = list.findIndex((f) => f.p === state.file); const n = list[i + 1]; renderTree(); n ? openFile(n.p) : renderHead(); }

function setView(v) { state.view = v; store.set("view", v); renderFile(); }
function setWrap() { state.wrap = !state.wrap; store.set("wrap", state.wrap); renderFile(); }

function renderHead() {
  const f = cur();
  const parts = f.p.split("/"); const base = parts.pop();
  const v = state.viewed.has(f.p);
  const [label, tone] = STATUS[f.s];
  const isImg = f.kind === "image";
  const modes = isImg
    ? `<div class="seg">${[["2up", "columns-2", "2-up"], ["swipe", "square-split-horizontal", "Swipe"], ["onion", "layers", "Onion skin"]].map(([m, i, l]) => `<button class="${state.imgMode === m ? "on" : ""}" onclick="state.imgMode='${m}';renderFile()">${ico(i)}${l}</button>`).join("")}</div>`
    : f.kind === "text" ? `<div class="seg icons">
        <button class="${state.view === "split" ? "on" : ""}" onclick="setView('split')" data-tip="Split  S">${ico("columns-2")}</button>
        <button class="${state.view === "unified" ? "on" : ""}" onclick="setView('unified')" data-tip="Unified  U">${ico("rows-2")}</button></div>
      <button class="icon-btn ${state.wrap ? "on" : ""}" onclick="setWrap()" data-tip="Wrap long lines">${ico("wrap-text")}</button>
      <button class="icon-btn ${state.ws ? "on" : ""}" onclick="state.ws=!state.ws;renderHead()" data-tip="Hide whitespace-only changes">${ico("pilcrow")}</button>
      <button class="icon-btn" onclick="toggleFind(true)" data-tip="Find in file  ⌘F">${ico("search")}</button>` : "";
  $("#fhead").innerHTML = `
    ${fileIcon(f.p)}
    <div class="fpath"><span class="dir">${parts.join("/")}/</span><span class="base">${base}</span></div>
    <button class="icon-btn" onclick="toast('Path copied','copy')" data-tip="Copy path">${ico("copy")}</button>
    <span class="pill ${tone}">${label}</span>
    ${f.prev ? `<span class="faint" style="font-size:12px;white-space:nowrap">from ${nameOf(f.prev)}</span>` : ""}
    ${f.kind === "text" || f.kind === "large" ? `<span class="mono add">+${f.a}</span><span class="mono del">−${f.d}</span>` : ""}
    <span class="spacer"></span>
    ${modes}
    <span class="vsep"></span>
    <button class="icon-btn" onclick="fileComment()" data-tip="Comment on file">${ico("message-square-plus")}</button>
    <button class="icon-btn" onclick="toast('Opened on github.com','external-link')" data-tip="Open on GitHub" data-tip-pos="left">${ico("external-link")}</button>
    <button class="btn viewed-btn ${v ? "on" : ""}" onclick="toggleViewed('${f.p}')">${ico(v ? "square-check" : "square")}Viewed<kbd>V</kbd></button>`;
  const list = visibleFiles(); const i = list.findIndex((x) => x.p === f.p);
  const prev = list[i - 1], next = list[i + 1];
  $("#ffoot").innerHTML = `
    <button class="btn ghost" ${prev ? "" : "disabled"} onclick="step(-1)">${ico("chevron-left")}<span class="n">${prev ? nameOf(prev.p) : "First file"}</span><kbd>K</kbd></button>
    <span class="spacer"></span><span class="faint">${i + 1} of ${list.length}</span><span class="spacer"></span>
    <button class="btn" onclick="viewedAndNext()">${ico("check")}Viewed, next<kbd>⇧V</kbd></button>
    <button class="btn ghost" ${next ? "" : "disabled"} onclick="step(1)"><kbd>J</kbd><span class="n">${next ? nameOf(next.p) : "Last file"}</span>${ico("chevron-right")}</button>`;
  icons();
}

const LANG = { ts: "typescript", tsx: "typescript", js: "javascript", json: "json", md: "markdown", rs: "rust" };
let lang = "typescript";
const hl = (t) => (t ? hljs.highlight(t, { language: lang, ignoreIllegals: true }).value : "");

function wordRange(a, b) {
  let p = 0; while (p < a.length && p < b.length && a[p] === b[p]) p++;
  let s = 0; while (s < a.length - p && s < b.length - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
  const changed = a.length - p - s + (b.length - p - s);
  return changed > 0.6 * (a.length + b.length) ? null : [p, s];
}
function codeHtml(text, range, cls) {
  if (!range) return hl(text);
  const [p, s] = range; const mid = text.slice(p, text.length - s);
  return hl(text.slice(0, p)) + (mid ? `<span class="${cls}">${hl(mid)}</span>` : "") + hl(text.slice(text.length - s));
}

function numbered(h) {
  let o = h.os, n = h.ns; const rows = [];
  for (const l of h.lines) {
    const k = l[0], t = l.slice(1);
    if (k === " ") rows.push({ k: "c", o: o++, n: n++, t });
    else if (k === "-") rows.push({ k: "d", o: o++, t });
    else rows.push({ k: "a", n: n++, t });
  }
  return rows;
}
// group into context rows and change blocks (removed run + added run) for pairing
function blocks(rows) {
  const out = []; let i = 0;
  while (i < rows.length) {
    if (rows[i].k === "c") { out.push({ ctx: rows[i++] }); continue; }
    const dels = [], adds = [];
    while (i < rows.length && rows[i].k === "d") dels.push(rows[i++]);
    while (i < rows.length && rows[i].k === "a") adds.push(rows[i++]);
    out.push({ dels, adds });
  }
  return out;
}

function notesAt(side, line) {
  const f = cur(); let html = "";
  (THREADS[f.p] || []).filter((t) => t.side === side && t.line === line).forEach((t) => (html += threadHtml(t)));
  const c = state.composer;
  if (c && !c.file && c.side === side && c.line === line) html += composerHtml(c);
  return html;
}
const anchorOf = (r) => (r.k === "d" ? ["L", r.o] : ["R", r.n]);

function plusBtn(r) { const [s, l] = anchorOf(r); return `<button class="plus" onclick="openComposer('${s}',${l},${r.k === "c"})" data-tip="Comment on line" data-tip-pos="right">${ico("plus")}</button>`; }
function sign(r) { return `<span class="sign">${r.k === "a" ? "+" : r.k === "d" ? "−" : " "}</span>`; }
const selected = (r) => { const c = state.composer; if (!c || c.file) return false; const [s, l] = anchorOf(r); return c.side === s && c.line === l; };

function unifiedRow(r, range) {
  const cls = r.k === "a" ? "add-row" : r.k === "d" ? "del-row" : "";
  const [s, l] = anchorOf(r);
  const notes = notesAt(s, l);
  return `<div class="r u ${cls} ${selected(r) ? "sel-row" : ""}"><span class="ln" onclick="openComposer('${s}',${l},${r.k === "c"})">${r.o ?? ""}</span><span class="ln" onclick="openComposer('${s}',${l},${r.k === "c"})">${r.n ?? ""}</span><span class="code">${plusBtn(r)}${sign(r)}${codeHtml(r.t, range, r.k === "a" ? "wd-add" : "wd-del")}</span></div>` +
    (notes ? `<div class="note-row u">${notes}</div>` : "");
}
function splitRow(l, r, rangeL, rangeR) {
  const lk = !l ? "empty" : l.k === "d" ? "del" : "";
  const rk = !r ? "empty" : r.k === "a" ? "add" : "";
  const half = (x, k, num, range, cls) => x
    ? `<span class="ln ${k}" onclick="openComposer('${anchorOf(x)[0]}',${anchorOf(x)[1]},${x.k === "c"})">${num}</span><span class="code ${k}">${plusBtn(x)}${sign(x)}${codeHtml(x.t, range, cls)}</span>`
    : `<span class="ln empty"></span><span class="code empty"></span>`;
  const sel = (l && selected(l)) || (r && selected(r));
  let html = `<div class="r s ${sel ? "sel-row" : ""}">${half(l, lk, l?.o, rangeL, "wd-del")}${half(r, rk, r?.n, rangeR, "wd-add")}</div>`;
  // context rows anchor to the right side by default
  const nl = l && l.k === "d" ? notesAt("L", l.o) : "";
  const nr = r ? notesAt("R", r.n) : "";
  if (nl || nr) html += `<div class="note-row s"><div class="slot ${nl ? "" : "empty"}">${nl}</div><div class="slot ${nr ? "" : "empty"}">${nr}</div></div>`;
  return html;
}

function hunkRow(h, hidden, rows) {
  const oc = rows.filter((r) => r.k !== "a").length, nc = rows.filter((r) => r.k !== "d").length;
  const exp = hidden > 0
    ? `<button class="exp" onclick="toast('Loaded ${Math.min(20, hidden)} lines from the head blob','unfold-vertical')" data-tip="Show ${Math.min(20, hidden)} more lines" data-tip-pos="right">${ico("chevrons-up")}${Math.min(20, hidden)}</button>` +
      (hidden > 20 ? `<button class="exp" onclick="toast('Loaded ${hidden} lines','unfold-vertical')">${ico("unfold-vertical")}All ${hidden} lines</button>` : "")
    : "";
  return `<div class="hunk">${exp}<span class="hdr">@@ -${h.os},${oc} +${h.ns},${nc} @@ ${h.fn}</span></div>`;
}

function genericDiff(f) {
  const id = nameOf(f.p).split(".")[0].replace(/-(\w)/g, (_, c) => c.toUpperCase());
  const pool = [
    `export function ${id}(props: Props) {`, "  const { value, onChange } = props;", "  const [draft, setDraft] = useState(value);",
    "  useEffect(() => setDraft(value), [value]);", "  if (!draft) return null;", '  const label = t("notifications.deepLink");',
    "  return <Field label={label} value={draft} onChange={setDraft} />;", "}", "",
    `export const ${id}Defaults = { deepLink: "", actions: [] };`,
  ];
  if (f.s === "A" || f.s === "D") {
    const sgn = f.s === "A" ? "+" : "-";
    return { hunks: [{ os: f.s === "A" ? 0 : 1, ns: f.s === "A" ? 1 : 0, fn: "", lines: pool.map((l) => sgn + l) }] };
  }
  return { tail: 12, hunks: [{ os: 24, ns: 24, fn: `function ${id}(`, lines: [
    " " + pool[0], " " + pool[1], "-  const [draft, setDraft] = useState(value ?? \"\");", "+" + pool[2], "+" + pool[3],
    " " + pool[4], "-  return <Field value={draft} onChange={setDraft} />;", "+" + pool[5], "+" + pool[6], " " + pool[7],
  ] }] };
}

function renderFile() {
  renderHead();
  const f = cur(); const el = $("#diff");
  el.className = "diff " + (state.wrap ? "wrap" : "nowrap") + (state.view === "unified" ? " unified" : "");
  lang = LANG[extOf(f.p)] || "plaintext";
  let html = "";
  if (state.composer?.file) html += `<div class="note-row u">${composerHtml(state.composer)}</div>`;

  if (f.kind === "image") { el.innerHTML = html + imageHtml(); icons(); return; }
  if (f.kind === "binary") { el.innerHTML = html + placeholder("file-question", "Binary file not shown", "badge-glyphs.woff2 was added (18.4 KB). You can still comment on the file.", `<button class="btn" onclick="fileComment()">${ico("message-square-plus")}Comment on file</button><button class="btn">${ico("external-link")}Open on GitHub</button>`); icons(); return; }
  if (f.kind === "large") { el.innerHTML = html + placeholder("file-diff", "Large diff not loaded", "GitHub didn't send a patch for this file (1,452 changed lines). Loading computes it locally; comments fall back to file-level if GitHub rejects the line.", `<button class="btn primary" onclick="toast('Diff computed locally','file-diff')">${ico("download")}Load diff</button>`); icons(); return; }

  const d = DIFFS[f.p] || genericDiff(f);
  // added or removed files have one side only: use a single column even in split mode
  const split = state.view === "split" && f.s !== "A" && f.s !== "D";
  let prevOldEnd = 1;
  d.hunks.forEach((h) => {
    const rows = numbered(h);
    html += hunkRow(h, h.os - prevOldEnd, rows);
    prevOldEnd = h.os + rows.filter((r) => r.k !== "a").length;
    for (const b of blocks(rows)) {
      if (b.ctx) { html += split ? splitRow(b.ctx, b.ctx) : unifiedRow(b.ctx); continue; }
      const ranges = b.dels.map((dl, i) => (b.adds[i] ? wordRange(dl.t, b.adds[i].t) : null));
      if (split) {
        for (let i = 0; i < Math.max(b.dels.length, b.adds.length); i++) html += splitRow(b.dels[i], b.adds[i], ranges[i], ranges[i] && [ranges[i][0], ranges[i][1]]);
      } else {
        b.dels.forEach((r, i) => (html += unifiedRow(r, ranges[i])));
        b.adds.forEach((r, i) => (html += unifiedRow(r, ranges[i])));
      }
    }
  });
  if (d.tail) html += `<div class="hunk"><button class="exp" onclick="toast('Loaded 20 lines from the head blob','unfold-vertical')">${ico("chevrons-down")}20</button><button class="exp" onclick="toast('Loaded ${d.tail} lines','unfold-vertical')">${ico("unfold-vertical")}All ${d.tail} lines</button></div>`;
  el.innerHTML = html;
  icons();
  el.querySelector(".composer textarea")?.focus();
}

function placeholder(icon, title, body, actions) {
  return `<div class="placeholder"><div class="big">${ico(icon)}</div><h3>${title}</h3><p>${body}</p><div style="display:flex;gap:8px;justify-content:center">${actions}</div></div>`;
}

// ---------- threads + composer ----------
function threadHtml(t) {
  if (t.resolved) return `<div class="thread resolved"><div class="thread-bar" style="border:0">${ico("circle-check")}<span>Resolved by ${t.comments[0].who}</span><span class="faint">${t.comments[0].body.replace(/<[^>]+>/g, "").slice(0, 60)}</span><span class="spacer"></span><button class="btn ghost" style="height:24px">${ico("chevrons-up-down")}Show</button></div></div>`;
  const items = t.comments.map((c) => `
    <div class="cmt-item">${avatar(c.who, "lg")}<div>
      <div class="cmt-meta"><b>${c.who}</b><span class="faint">${c.when}</span>${t.pending ? `<span class="pill pending">${ico("message-square-dashed")}Pending</span>` : ""}
        <span class="spacer"></span>${t.pending ? `<button class="icon-btn" data-tip="Edit">${ico("pencil")}</button><button class="icon-btn" data-tip="Delete" data-tip-pos="left">${ico("trash-2")}</button>` : `<button class="icon-btn" data-tip="More" data-tip-pos="left">${ico("ellipsis")}</button>`}</div>
      <div class="cmt-body">${c.body.startsWith("<") ? c.body : `<p>${c.body}</p>`}</div>
      ${c.suggestion ? `<div class="suggest"><div class="suggest-head">${ico("file-diff")}Suggested change<span class="spacer"></span><button class="icon-btn" data-tip="Copy" data-tip-pos="left">${ico("copy")}</button></div>
        <div class="del-row">${hl(c.suggestion[0])}</div><div class="add-row">${hl(c.suggestion[1])}</div></div>` : ""}
    </div></div>`).join("");
  return `<div class="thread ${t.pending ? "pending" : ""}">
    ${t.pending ? "" : `<div class="thread-bar">${ico("messages-square")}<span>${t.comments.length} comments</span><span class="spacer"></span><button class="btn ghost" style="height:24px" onclick="toast('Thread resolved','circle-check')">${ico("check")}Resolve</button></div>`}
    ${items}
    ${t.pending ? "" : `<div class="reply">${avatar("sercand")}<label class="field"><input placeholder="Reply"></label></div>`}
  </div>`;
}

function composerHtml(c) {
  const hasReview = true;
  const sideToggle = c.ctx ? `<div class="seg" data-tip="Which side to anchor a context line to"><button onclick="markSeg(this)">Left</button><button class="on" onclick="markSeg(this)">Right</button></div>` : "";
  return `<div class="composer">
    <div class="composer-top">
      <div class="seg"><button class="on" onclick="markSeg(this)">Write</button><button onclick="markSeg(this)">Preview</button></div>
      <button class="icon-btn" data-tip="Bold">${ico("bold")}</button>
      <button class="icon-btn" data-tip="Code">${ico("code")}</button>
      <button class="icon-btn" data-tip="Link">${ico("link")}</button>
      ${c.file || c.side === "L" ? "" : `<button class="icon-btn" data-tip="Insert suggestion">${ico("file-diff")}</button>`}
      <span class="spacer"></span>${sideToggle}
      <span class="faint" style="font-size:12px;margin-left:8px">${c.file ? "Whole file" : `Line ${c.side}${c.line}`}</span>
    </div>
    <textarea class="area" placeholder="Leave a comment. Markdown works."></textarea>
    <div class="composer-foot">
      <span class="hint">${ico("hard-drive")}Saved as you type</span>
      <button class="btn ghost" onclick="closeComposer()">Cancel</button>
      ${c.file
        ? `<button class="btn primary" onclick="closeComposer();toast('Comment posted','message-square')">Add single comment</button>`
        : `<button class="btn" onclick="closeComposer();toast('Comment posted','message-square')">Add single comment</button>
           <button class="btn primary" onclick="closeComposer();toast('Added to your review','message-square-dashed')">${hasReview ? "Add to review" : "Start a review"}<kbd style="background:transparent;color:inherit;border-color:currentColor;opacity:.7">⌘↵</kbd></button>`}
    </div></div>`;
}
function openComposer(side, line, ctx) { state.composer = { side, line, ctx }; renderFile(); }
function fileComment() { state.composer = { file: true }; renderFile(); $("#diffwrap").scrollTop = 0; }
function closeComposer() { state.composer = null; renderFile(); }

// ---------- image diff ----------
function badgeSvg(fill, dot) {
  const s = `<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 64 64"><rect x="8" y="8" width="48" height="48" rx="${dot ? 14 : 24}" fill="${fill}"/><path d="M32 18c-6 0-10 4-10 10v8l-4 5h28l-4-5v-8c0-6-4-10-10-10zm-4 26a4 4 0 0 0 8 0" fill="#fff"/>${dot ? '<circle cx="48" cy="16" r="7" fill="#f0605a" stroke="#fff" stroke-width="2"/>' : ""}</svg>`;
  return "data:image/svg+xml," + encodeURIComponent(s);
}
function imageHtml() {
  const base = badgeSvg("#5b7ee5", false), head = badgeSvg("#e0a940", true);
  if (state.imgMode === "2up") return `<div class="img-stage"><div class="twoup">
      <figure class="b"><div class="checker"><img src="${base}" width="200" height="200"></div><figcaption>Before, 256 × 256, 4.2 KB</figcaption></figure>
      <figure class="h"><div class="checker"><img src="${head}" width="200" height="200"></div><figcaption>After, 256 × 256, 5.1 KB <span class="add">(+0.9 KB)</span></figcaption></figure>
    </div></div>`;
  if (state.imgMode === "swipe") return `<div class="img-stage"><div class="checker swipe" id="swipe">
      <img src="${base}"><img src="${head}" id="swipe-head" style="clip-path:inset(0 0 0 50%)"><div class="divider" id="swipe-div" style="left:50%"></div></div>
      <input type="range" value="50" oninput="$('#swipe-head').style.clipPath='inset(0 0 0 '+this.value+'%)';$('#swipe-div').style.left=this.value+'%'"></div>`;
  return `<div class="img-stage"><div class="checker swipe"><img src="${base}"><img src="${head}" id="onion" style="opacity:.5"></div>
      <label class="muted" style="display:flex;gap:10px;align-items:center">Before<input type="range" value="50" oninput="$('#onion').style.opacity=this.value/100">After</label></div>`;
}

// ---------- details pane ----------
const CHECKS = [["build", "2m 14s"], ["lint", "38s"], ["typecheck", "1m 02s"], ["test", "4m 11s"], ["preview-deploy", "1m 47s"], ["CodeQL", "3m 05s"]];
const checkRows = () => CHECKS.map(([n, d]) => `<div class="check-row"><span class="add">${ico("circle-check")}</span>${n}<span class="dur">${d}</span></div>`).join("");
const DTABS = [["overview", "file-text", "Overview", ""], ["conversation", "messages-square", "", 5], ["commits", "git-commit-horizontal", "", 3], ["checks", "circle-check", "", 6]];

function renderDetails() {
  $("#details").classList.toggle("closed", !state.details);
  $("#details-toggle").classList.toggle("on", state.details);
  $("#dtabs").innerHTML = DTABS.map(([id, i, l, n]) => `<button class="${state.dtab === id ? "on" : ""}" onclick="state.dtab='${id}';store.set('dtab','${id}');renderDetails()" data-tip="${id[0].toUpperCase() + id.slice(1)}">${ico(i)}${l}${n ? `<span class="n">${n}</span>` : ""}</button>`).join("");
  const body = {
    overview: `
      <h2>Default a push's deep links to the app's bare schemes</h2>
      <dl class="kv">
        <dt>Author</dt><dd>${avatar("dogacanaydin")} dogacanaydin</dd>
        <dt>Branch</dt><dd><span class="branch">push-default-links</span>${ico("arrow-right", "faint")}<span class="branch">main</span></dd>
        <dt>Labels</dt><dd><span class="pill accent">notifications</span><span class="pill">journey</span></dd>
        <dt>Linked</dt><dd>${ico("circle-dot", "add")}<a href="#">#17 Empty deep links crash Android</a></dd>
      </dl>
      <div class="md">
        <p>A push created without deep links opens nothing on tap. This fills the empty links with the app's bare scheme (<code>kedis://</code>) at create time.</p>
        <h4>Changes</h4>
        <ul><li><code>withDefaultLinks</code> and <code>hasNoLinks</code> in <code>lib/notifications</code></li><li>Create path writes the prefilled revision; a failed write returns a warning instead of failing the create</li><li>Flow editor shows the default in the field placeholder</li></ul>
        <h4>Testing</h4>
        <p>Unit tests for both helpers, plus the create path in <code>push.test.ts</code>.</p>
      </div>
      <div class="sect"><div class="sect-h">Reviewers</div>
        <div class="person">${avatar("sercand")}sercand<span class="state pill warn">${ico("eye")}Requested</span></div>
        <div class="person">${avatar("egealtar")}egealtar<span class="state pill">${ico("message-square")}Commented</span></div>
      </div>`,
    conversation: `
      <div class="tl">${avatar("dogacanaydin")}<b>dogacanaydin</b> <span class="faint">opened this 2w ago</span></div>
      <div class="tl">${avatar("egealtar")}<b>egealtar</b> <span class="faint">reviewed 2d ago</span><div class="bubble">Looks right. One question on the create path, left inline.</div></div>
      <div class="tl">${avatar("dogacanaydin")}<b>dogacanaydin</b> <span class="faint">pushed 2 commits 1d ago</span></div>
      <div class="tl">${avatar("dogacanaydin")}<b>dogacanaydin</b> <span class="faint">commented 1d ago</span><div class="bubble">Rebased on main. The Android fix in #17 depends on this.</div></div>
      <div class="tl">${avatar("sercand")}<b>sercand</b> <span class="faint">was requested for review</span></div>
      <p class="faint" style="font-size:12px">Conversation comments are read-only here. Reply on github.com.</p>`,
    commits: [["a41c9e2", "Add default-links helpers", "2w"], ["5d03f7b", "Prefill deep links on push create", "2w"], ["7f3e0b1", "Return a warning when default links fail to save", "1d"]]
      .map(([s, m, w]) => `<div class="commit">${ico("git-commit-horizontal", "faint")}<div>${m}<div class="sha">${s}</div></div><span class="faint">${w}</span></div>`).join(""),
    checks: `<div class="sect-h">All 6 checks passed on 7f3e0b1</div>${checkRows()}`,
  }[state.dtab];
  $("#dbody").innerHTML = body;
  icons();
}
function toggleDetails() { state.details = !state.details; store.set("details", state.details); renderDetails(); }

function renderVerdicts() {
  const V = [["COMMENT", "message-square", "Comment", "Feedback without approving"], ["APPROVE", "circle-check", "Approve", "Ready to merge"], ["REQUEST_CHANGES", "file-diff", "Request changes", "Needs work before merging; needs a summary"]];
  $("#verdicts").innerHTML = V.map(([id, i, l, d]) => `<div class="verdict ${state.verdict === id ? "on" : ""}" onclick="state.verdict='${id}';renderVerdicts()">${ico(state.verdict === id ? "circle-dot" : "circle")}<span style="display:flex;gap:6px;align-items:center">${ico(i)}${l}</span><small>${d}</small></div>`).join("");
  icons();
}

function toggleFind(open) { $("#find").classList.toggle("open", open); if (open) $("#find input").focus(); }

// ---------- resizer ----------
function initResizer() {
  const r = $("#resizer"), body = $("#body");
  body.style.setProperty("--left", store.get("leftWidth", 300) + "px");
  r.addEventListener("mousedown", (e) => {
    r.classList.add("drag"); e.preventDefault();
    const move = (ev) => { const w = Math.max(220, Math.min(560, ev.clientX)); body.style.setProperty("--left", w + "px"); store.set("leftWidth", w); };
    const up = () => { r.classList.remove("drag"); removeEventListener("mousemove", move); removeEventListener("mouseup", up); };
    addEventListener("mousemove", move); addEventListener("mouseup", up);
  });
}

// ---------- keys ----------
document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") { e.preventDefault(); toggleFind(true); return; }
  if (e.key === "Escape") { if (state.composer) closeComposer(); toggleFind(false); return; }
  if (typingTarget(e) || e.metaKey || e.ctrlKey) return;
  const k = e.key;
  if (k === "j") step(1);
  else if (k === "k") step(-1);
  else if (k === "s") setView("split");
  else if (k === "u") setView("unified");
  else if (k === "v") toggleViewed(state.file);
  else if (k === "V") viewedAndNext();
  else if (k === "o") toggleDetails();
});

document.addEventListener("DOMContentLoaded", () => {
  // ?file=…&view=unified lets you link straight to a state while iterating
  const qs = new URLSearchParams(location.search);
  if (qs.get("file")) state.file = qs.get("file");
  if (qs.get("view")) state.view = qs.get("view");
  $("#checks-mini").innerHTML = checkRows();
  initResizer();
  renderFilterPop(); renderTree(); renderFile(); renderDetails(); renderVerdicts();
  injectGlobalModals([
    { label: "Submit review", icon: "send", run: () => document.querySelector(".review-btn").click(), keys: ["⌘", "↵"] },
    { label: "Toggle split / unified", icon: "columns-2", run: () => setView(state.view === "split" ? "unified" : "split"), keys: ["S", "U"] },
    { label: "Mark file viewed and go to next", icon: "check", run: () => viewedAndNext(), keys: ["⇧", "V"] },
    { label: "Toggle details pane", icon: "panel-right", run: () => toggleDetails(), keys: ["O"] },
    { label: "Preview: pending review started on github.com", icon: "cloud", run: () => document.getElementById("banner-server").classList.remove("hide") },
    { label: "Preview: new commits pushed while reviewing", icon: "git-commit-horizontal", run: () => document.getElementById("banner-moved").classList.remove("hide") },
    { label: "Back to pull requests", icon: "arrow-left", run: () => (location.href = "prs.html") },
    ...FILES.map((f) => ({ label: f.p, file: true, run: eval(`() => openFile(${JSON.stringify(f.p)})`) })),
  ]);
  mockNav("pr.html");
});
