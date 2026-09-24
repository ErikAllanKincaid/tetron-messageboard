"use strict";
/* tetron-messageboard frontend. Polls GET /api/messages every 10s (same cadence as
   tetron-webui's dashboard), posts via multipart, soft-deletes via DELETE.
   Identity, "mine", storage and network all come from the server; the client
   never self-reports who it is. */

const POLL_INTERVAL_MS = 10000;

// ---- tiny DOM helpers -------------------------------------------------
const $ = (id) => document.getElementById(id);
const feed = $("feed"), empty = $("empty");

function esc(s) {
  return String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
}

// ---- theme (per-viewer convenience) -----------------------------------
try {
  const saved = localStorage.getItem("tetron-messageboard-theme");
  if (saved) document.documentElement.setAttribute("data-theme", saved);
} catch (_) {}
$("themeToggle").addEventListener("click", () => {
  const cur = getComputedStyle(document.documentElement).colorScheme.includes("dark") ? "dark" : "light";
  const next = cur === "dark" ? "light" : "dark";
  document.documentElement.setAttribute("data-theme", next);
  try { localStorage.setItem("tetron-messageboard-theme", next); } catch (_) {}
});

// ---- identity / avatar ------------------------------------------------
function hashStr(s) { let h = 2166136261; for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); } return h >>> 0; }
function avatarStyle(host) { return `background: hsl(${hashStr(host) % 360}, var(--avatar-s), var(--avatar-l));`; }
function initial(host) { return (host && host[0] ? host[0] : "?").toUpperCase(); }

// ---- time -------------------------------------------------------------
function relTime(sec) {
  const s = Math.floor(Date.now() / 1000) - sec;
  if (s < 45) return "now";
  if (s < 3600) return Math.floor(s / 60) + "m ago";
  if (s < 86400) return Math.floor(s / 3600) + "h ago";
  return Math.floor(s / 86400) + "d ago";
}
function clockTime(sec) { return new Date(sec * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }); }
function dayLabel(sec) {
  const d = new Date(sec * 1000), today = new Date();
  if (d.toDateString() === today.toDateString()) return "Today";
  const y = new Date(today); y.setDate(y.getDate() - 1);
  if (d.toDateString() === y.toDateString()) return "Yesterday";
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}
function fmtBytes(n) {
  if (n >= 1048576) return (n / 1048576).toFixed(n >= 10485760 ? 0 : 1) + " MB";
  if (n >= 1024) return Math.round(n / 1024) + " KB";
  return n + " B";
}

// ---- render -----------------------------------------------------------
const GROUP_GAP = 5 * 60; // seconds of silence before a new header

function nearBottom() { return feed.scrollHeight - feed.scrollTop - feed.clientHeight < 80; }

function render(data) {
  const msgs = data.messages || [];
  if (msgs.length === 0) {
    feed.hidden = true;
    empty.hidden = false;
    $("emptyTitle").textContent = "No messages yet";
    $("emptyText").innerHTML = "This board is shared with everyone on <strong>" +
      esc(data.network || "the mesh") + "</strong>. Say something — text and images are visible to every device on the mesh.";
    return;
  }
  const stick = nearBottom();
  empty.hidden = true;
  feed.hidden = false;
  feed.textContent = "";

  let prev = null, lastDay = null;
  for (const m of msgs) {
    const day = dayLabel(m.ts);
    if (day !== lastDay) {
      const sep = document.createElement("div");
      sep.className = "day-sep"; sep.textContent = day;
      feed.appendChild(sep); lastDay = day; prev = null;
    }
    const cont = prev && prev.host === m.host && (m.ts - prev.ts) < GROUP_GAP && day === dayLabel(prev.ts);
    feed.appendChild(renderGroup(m, cont));
    prev = m;
  }
  if (stick) feed.scrollTop = feed.scrollHeight;
}

function renderGroup(m, cont) {
  const g = document.createElement("div");
  g.className = "group" + (cont ? " cont" : "");

  const av = document.createElement("div");
  av.className = "avatar";
  av.setAttribute("style", avatarStyle(m.host));
  av.textContent = initial(m.host);
  g.appendChild(av);

  const col = document.createElement("div");
  col.style.minWidth = "0";

  const head = document.createElement("div");
  head.className = "grp-head";
  const name = m.mine ? `${m.host} (you)` : m.host;
  head.innerHTML = `<span class="grp-name">${esc(name)}</span>`
    + `<span class="grp-ip mono">${esc(m.ip)}</span>`
    + `<span class="grp-time">${relTime(m.ts)}</span>`;
  col.appendChild(head);
  col.appendChild(renderMsg(m));
  g.appendChild(col);
  return g;
}

function renderMsg(m) {
  const msg = document.createElement("div");
  msg.className = "msg";

  const t = document.createElement("span");
  t.className = "msg-time-inline mono";
  t.textContent = clockTime(m.ts);
  msg.appendChild(t);

  if (m.deleted) {
    const b = document.createElement("div");
    b.className = "msg-body ghost";
    b.textContent = "deleted by " + (m.deleted_by || "someone");
    msg.appendChild(b);
    return msg;
  }

  if (m.text) {
    const b = document.createElement("div");
    b.className = "msg-body";
    b.textContent = m.text;
    msg.appendChild(b);
  }
  if (m.attachment) {
    const a = m.attachment, url = "/attachments/" + a.hash;
    if (a.content_type && a.content_type.startsWith("image/")) {
      const im = document.createElement("img");
      im.className = "thumb"; im.loading = "lazy"; im.src = url; im.alt = a.name || "attachment";
      im.addEventListener("click", () => openLightbox(url));
      msg.appendChild(im);
    } else {
      const link = document.createElement("a");
      link.className = "file-link"; link.href = url; link.target = "_blank"; link.rel = "noopener";
      link.innerHTML = `↓ ${esc(a.name || "file")} <span class="fsize">${fmtBytes(a.size)}</span>`;
      msg.appendChild(link);
    }
  }
  if (m.attachment_evicted) {
    const r = document.createElement("div");
    r.className = "attach-removed";
    r.textContent = "🗑 attachment removed — storage limit";
    msg.appendChild(r);
  }

  msg.appendChild(buildMenu(m));
  return msg;
}

function buildMenu(m) {
  const wrap = document.createElement("div");
  wrap.className = "msg-menu";
  const dots = document.createElement("button");
  dots.className = "dots"; dots.textContent = "⋮"; dots.title = "More"; dots.setAttribute("aria-label", "Message actions");
  wrap.appendChild(dots);

  dots.addEventListener("click", (e) => {
    e.stopPropagation();
    closeMenus();
    wrap.classList.add("open");
    const pop = document.createElement("div");
    pop.className = "menu-pop";
    // Keep clicks inside the popup from bubbling to the document-level
    // closeMenus, which would tear the popup down before the "Delete message"
    // handler could swap in the confirm step. Cancel/Delete close it
    // explicitly instead.
    pop.addEventListener("click", (e) => e.stopPropagation());
    pop.innerHTML = `<button class="danger" data-act="del">Delete message</button>`;
    pop.querySelector("[data-act=del]").addEventListener("click", () => {
      pop.innerHTML = `<div class="confirm-row"><p>Delete this message for everyone on the mesh?</p>`
        + `<div class="confirm-actions"><button data-c="no">Cancel</button>`
        + `<button class="del" data-c="yes">Delete</button></div></div>`;
      pop.querySelector("[data-c=no]").addEventListener("click", closeMenus);
      pop.querySelector("[data-c=yes]").addEventListener("click", () => doDelete(m.id));
    });
    wrap.appendChild(pop);
  });
  return wrap;
}

function closeMenus() {
  document.querySelectorAll(".msg-menu.open").forEach((w) => {
    w.classList.remove("open");
    const p = w.querySelector(".menu-pop"); if (p) p.remove();
  });
}
document.addEventListener("click", closeMenus);
document.addEventListener("keydown", (e) => { if (e.key === "Escape") { closeMenus(); closeLightbox(); } });

// ---- lightbox ---------------------------------------------------------
const lb = $("lightbox"), lbImg = $("lightboxImg");
function openLightbox(src) { lbImg.src = src; lb.hidden = false; }
function closeLightbox() { lb.hidden = true; lbImg.removeAttribute("src"); }
lb.addEventListener("click", closeLightbox);

// ---- data fetch -------------------------------------------------------
let maxAttachBytes = 25 * 1048576;

async function refresh() {
  try {
    const r = await fetch("/api/messages", { cache: "no-store" });
    if (!r.ok) throw new Error("status " + r.status);
    const data = await r.json();
    applyHeader(data);
    render(data);
  } catch (e) {
    // Transient: the daemon or mesh may be momentarily unreachable. Leave
    // the last-rendered feed in place and try again on the next tick.
    if (feed.hidden && empty.hidden === false) {
      $("emptyTitle").textContent = "Can’t reach the board";
      $("emptyText").textContent = "Retrying… (is the tetron mesh up?)";
    }
  }
}

function applyHeader(data) {
  const host = data.host || "this node";
  const who = data.installed_by_coordinator ? "admin-hosted" : "member-hosted";
  $("hostLabel").innerHTML = `hosted on <span class="mono">${esc(host)}</span> · ${who}`;
  $("netChip").textContent = (data.network || "") + (data.subnet ? " · " + data.subnet : "");
  if (data.limits && data.limits.max_attachment_bytes) maxAttachBytes = data.limits.max_attachment_bytes;
  if (data.storage) {
    const used = data.storage.used_bytes || 0, max = data.storage.max_bytes || 1;
    $("storeText").textContent = fmtBytes(used) + " / " + fmtBytes(max);
    $("storeFill").style.width = Math.min(100, (used / max) * 100).toFixed(1) + "%";
  }
}

// ---- composer ---------------------------------------------------------
const input = $("input"), sendBtn = $("sendBtn"), fileInput = $("fileInput");
const preview = $("attachPreview"), errBox = $("postError");
let pendingFile = null, previewUrl = null;

function autosize() { input.style.height = "auto"; input.style.height = Math.min(input.scrollHeight, 140) + "px"; }
function refreshSend() { sendBtn.disabled = !(input.value.trim() || pendingFile); }
function showError(msg) { errBox.textContent = msg; errBox.hidden = false; }
function clearError() { errBox.hidden = true; }

input.addEventListener("input", () => { autosize(); refreshSend(); });
input.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); } });

$("attachBtn").addEventListener("click", () => fileInput.click());
fileInput.addEventListener("change", () => acceptFile(fileInput.files && fileInput.files[0]));
$("attachRemove").addEventListener("click", clearAttach);

// Shared entry point for every way an image can arrive: the picker, a
// drag-and-drop, or a paste. Validates type + size (the server only accepts
// images, and enforces the size cap itself) before staging it.
function acceptFile(f) {
  if (!f) return;
  if (!/^image\//.test(f.type)) {
    showError("Only image attachments are supported.");
    fileInput.value = "";
    return;
  }
  if (f.size > maxAttachBytes) {
    showError("That image is " + fmtBytes(f.size) + "; the limit is " + fmtBytes(maxAttachBytes) + ".");
    fileInput.value = "";
    return;
  }
  clearError();
  setAttach(f);
}

// Drag-and-drop anywhere on the page, plus paste-to-attach. The overlay is
// only shown while files are being dragged (a dragged selection or link has
// no "Files" type), and a depth counter avoids flicker as the pointer moves
// over child elements.
const dropzone = $("dropzone");
let dragDepth = 0;
function dragHasFiles(e) {
  return Array.prototype.includes.call(e.dataTransfer ? e.dataTransfer.types : [], "Files");
}
window.addEventListener("dragenter", (e) => {
  if (!dragHasFiles(e)) return;
  e.preventDefault();
  dragDepth++;
  dropzone.classList.add("show");
});
window.addEventListener("dragover", (e) => { if (dragHasFiles(e)) e.preventDefault(); });
window.addEventListener("dragleave", (e) => {
  if (!dragHasFiles(e)) return;
  dragDepth = Math.max(0, dragDepth - 1);
  if (dragDepth === 0) dropzone.classList.remove("show");
});
window.addEventListener("drop", (e) => {
  if (!dragHasFiles(e)) return;
  e.preventDefault();
  dragDepth = 0;
  dropzone.classList.remove("show");
  acceptFile(e.dataTransfer.files && e.dataTransfer.files[0]);
});
window.addEventListener("paste", (e) => {
  const items = (e.clipboardData && e.clipboardData.items) || [];
  const img = Array.prototype.find.call(items, (i) => i.type.startsWith("image/"));
  if (img) acceptFile(img.getAsFile());
});

function setAttach(f) {
  clearAttach();
  pendingFile = f;
  previewUrl = URL.createObjectURL(f);
  $("attachThumb").src = previewUrl;
  $("attachName").textContent = f.name;
  $("attachMeta").textContent = fmtBytes(f.size) + " · " + (f.type || "image");
  preview.hidden = false;
  refreshSend();
}
function clearAttach() {
  pendingFile = null;
  if (previewUrl) { URL.revokeObjectURL(previewUrl); previewUrl = null; }
  fileInput.value = "";
  preview.hidden = true;
  refreshSend();
}

async function send() {
  const text = input.value.trim();
  if (!text && !pendingFile) return;
  sendBtn.disabled = true;
  const fd = new FormData();
  if (text) fd.append("text", text);
  if (pendingFile) fd.append("file", pendingFile, pendingFile.name);
  try {
    const r = await fetch("/api/messages", { method: "POST", body: fd });
    if (!r.ok) {
      let msg = "Could not post (status " + r.status + ").";
      try { const j = await r.json(); if (j.error) msg = j.error; } catch (_) {}
      showError(msg); sendBtn.disabled = false; return;
    }
    clearError();
    input.value = ""; autosize(); clearAttach();
    await refresh();
  } catch (e) {
    showError("Could not reach the board. Check the mesh connection.");
    sendBtn.disabled = false;
  }
}
sendBtn.addEventListener("click", send);

async function doDelete(id) {
  closeMenus();
  try {
    const r = await fetch("/api/messages/" + id, { method: "DELETE" });
    if (r.ok || r.status === 404) await refresh();
    else showError("Could not delete (status " + r.status + ").");
  } catch (_) {
    showError("Could not reach the board to delete.");
  }
}

// ---- boot -------------------------------------------------------------
refresh();
setInterval(refresh, POLL_INTERVAL_MS);
