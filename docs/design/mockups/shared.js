// Shared mockup behaviour: theme, icons, popovers, palette, settings, toast.
// No real functionality — just enough to click through the design.

const store = {
  get(k, d) { try { const v = localStorage.getItem("ghr." + k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem("ghr." + k, JSON.stringify(v)); } catch {} },
};

// theme is applied before paint by the inline snippet in each page head
function setTheme(t) {
  document.documentElement.dataset.theme = t;
  store.set("theme", t);
  document.querySelectorAll("[data-theme-icon]").forEach((el) => {
    el.innerHTML = `<i data-lucide="${t === "light" ? "moon" : "sun"}"></i>`;
  });
  icons();
}
function toggleTheme() { setTheme(document.documentElement.dataset.theme === "light" ? "dark" : "light"); }

function icons() { if (window.lucide) lucide.createIcons({ attrs: { "stroke-width": 1.75 } }); }
const ico = (n, cls = "") => `<i data-lucide="${n}" class="${cls}"></i>`;

// File-type icons: vscode-material-icon-theme (MIT). The app can embed the same
// SVGs via egui_extras' svg loader, keyed by extension / file name.
const MI = "https://cdn.jsdelivr.net/npm/material-icon-theme@5.11.1/icons/";
function fileIcon(path) {
  const name = path.split("/").pop().toLowerCase();
  const ext = name.includes(".") ? name.split(".").pop() : "";
  let icon = "file";
  if (/\.test\.tsx?$|\.spec\.tsx?$/.test(name)) icon = "test-ts";
  else if (ext === "tsx") icon = "react_ts";
  else if (ext === "ts") icon = "typescript";
  else if (ext === "rs") icon = "rust";
  else if (ext === "json") icon = "json";
  else if (ext === "md") icon = "markdown";
  else if (["png", "jpg", "jpeg", "gif", "webp"].includes(ext)) icon = "image";
  else if (ext === "svg") icon = "svg";
  else if (ext === "yml" || ext === "yaml") icon = "yaml";
  else if (ext === "css") icon = "css";
  else if (ext === "lock") icon = "lock";
  return `<img class="ficon" src="${MI}${icon}.svg" alt="">`;
}
function folderIcon(name, open) {
  const known = { src: "src", lib: "lib", components: "components", test: "test", tests: "test", public: "public", docs: "docs" };
  const k = known[name.split("/").pop()];
  return `<img class="ficon" src="${MI}folder${k ? "-" + k : ""}${open ? "-open" : ""}.svg" alt="">`;
}

const AVATAR_COLORS = ["#5b7ee5", "#c0629b", "#3f9d8f", "#c47a35", "#8a6bd1", "#4f97c9", "#b5534f", "#6c9a3c"];
function avatar(login, cls = "") {
  let h = 0; for (const c of login) h = (h * 31 + c.charCodeAt(0)) | 0;
  const initials = login.replace(/[^a-zA-Z]/g, "").slice(0, 2).toUpperCase();
  return `<span class="avatar ${cls}" style="background:${AVATAR_COLORS[Math.abs(h) % AVATAR_COLORS.length]}">${initials}</span>`;
}

// popovers: [data-pop="id"] toggles #id, positioned under the trigger
function initPopovers() {
  document.addEventListener("click", (e) => {
    const trig = e.target.closest("[data-pop]");
    const inside = e.target.closest(".pop");
    if (trig) {
      const pop = document.getElementById(trig.dataset.pop);
      const wasOpen = pop.classList.contains("open");
      closePops();
      if (!wasOpen) {
        const r = trig.getBoundingClientRect();
        pop.classList.add("open");
        const w = pop.offsetWidth;
        const left = trig.dataset.popAlign === "right" ? r.right - w : r.left;
        pop.style.left = Math.max(8, Math.min(left, innerWidth - w - 8)) + "px";
        pop.style.top = r.bottom + 6 + "px";
        pop.querySelector("input")?.focus();
      }
      return;
    }
    if (!inside) closePops();
  });
}
function closePops() { document.querySelectorAll(".pop.open").forEach((p) => p.classList.remove("open")); }

function openModal(id) { closePops(); document.getElementById(id).classList.add("open"); document.querySelector(`#${id} input`)?.focus(); }
function closeModals() { document.querySelectorAll(".scrim.open").forEach((m) => m.classList.remove("open")); }

let toastTimer;
function toast(msg, icon = "check") {
  let t = document.querySelector(".toast");
  if (!t) { t = document.createElement("div"); t.className = "toast"; document.body.append(t); }
  t.innerHTML = ico(icon) + msg; icons();
  t.classList.add("show");
  clearTimeout(toastTimer); toastTimer = setTimeout(() => t.classList.remove("show"), 1800);
}

// Command palette + settings markup, shared by list and detail screens
function injectGlobalModals(commands) {
  document.body.insertAdjacentHTML("beforeend", `
  <div class="scrim" id="palette" onclick="if(event.target===this)closeModals()">
    <div class="modal palette">
      <label class="field">${ico("search")}<input placeholder="Type a command or a file name" oninput="filterPalette(this.value)"></label>
      <div class="palette-list" id="palette-list"></div>
    </div>
  </div>
  <div class="scrim" id="settings" onclick="if(event.target===this)closeModals()">
    <div class="modal">
      <div class="modal-head">${ico("settings")} Settings<span class="spacer"></span><button class="icon-btn" onclick="closeModals()">${ico("x")}</button></div>
      <div class="modal-body settings-grid">
        <label>Account</label>
        <div style="display:flex;align-items:center;gap:10px">${avatar("sercand", "lg")}<div><div>sercand</div><div class="faint">github.com, scopes repo and read:org</div></div><span class="spacer"></span><a class="btn danger" href="login.html">${ico("log-out")}Sign out</a></div>
        <label>Theme</label>
        <div class="seg"><button onclick="setTheme('dark');markSeg(this)" class="${document.documentElement.dataset.theme !== "light" ? "on" : ""}">${ico("moon")}Dark</button><button onclick="setTheme('light');markSeg(this)" class="${document.documentElement.dataset.theme === "light" ? "on" : ""}">${ico("sun")}Light</button></div>
        <label>Default scopes</label>
        <div class="toggles">${[["eye", "Review requested", 1], ["user-check", "Assigned", 1], ["pencil-line", "Authored", 1], ["users", "Involves", 0], ["at-sign", "Mentions", 0]].map(([i, l, on]) => `<button class="toggle ${on ? "on" : ""}" onclick="this.classList.toggle('on')">${ico(i)}${l}</button>`).join("")}</div>
        <label>Diff layout</label>
        <div class="seg"><button class="on">${ico("columns-2")}Split</button><button>${ico("rows-2")}Unified</button></div>
        <label>Code font size</label>
        <div class="seg"><button>12</button><button class="on">13</button><button>14</button></div>
        <label>OAuth client ID</label>
        <label class="field"><input placeholder="Built-in (Iv1.4f2c…)"></label>
      </div>
    </div>
  </div>`);
  window._commands = commands;
  filterPalette("");
  // #settings / #palette in the URL opens that modal, handy while iterating
  if (["#settings", "#palette"].includes(location.hash)) openModal(location.hash.slice(1));
}
function markSeg(btn) { btn.parentElement.querySelectorAll("button").forEach((b) => b.classList.toggle("on", b === btn)); }
function filterPalette(q) {
  const list = document.getElementById("palette-list");
  const items = window._commands.filter((c) => c.label.toLowerCase().includes(q.toLowerCase()));
  list.innerHTML = items.map((c, i) => `<button class="menu-item ${i === 0 ? "active" : ""}" onclick="closeModals();(${c.run})()">${c.file ? fileIcon(c.label) : ico(c.icon)}<span>${c.label}</span><span class="end">${c.keys ? c.keys.map((k) => `<kbd>${k}</kbd>`).join(" ") : ""}</span></button>`).join("") || `<div class="pop-head">No matches</div>`;
  icons();
}

function mockNav(active) {
  const pages = [["login.html", "Login"], ["prs.html", "PR list"], ["pr.html", "PR detail"]];
  document.body.insertAdjacentHTML("beforeend", `<nav class="mock-nav">${pages.map(([h, l]) => `<a href="${h}" class="${h === active ? "on" : ""}">${l}</a>`).join("")}</nav>`);
}

function typingTarget(e) { return e.target.closest("input, textarea, [contenteditable]"); }

document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k" && document.getElementById("palette")) { e.preventDefault(); openModal("palette"); }
  if (e.key === "Escape") { closeModals(); closePops(); }
});

document.addEventListener("DOMContentLoaded", () => {
  initPopovers();
  setTheme(document.documentElement.dataset.theme || "dark");
});
