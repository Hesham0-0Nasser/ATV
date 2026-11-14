// ATV page logic. Every real job is done by the Rust side (src/commands.rs);
// this file keeps the screen state, shows and hides parts of index.html, and
// fills them in. No framework and no build step.

import { fillIcons, icon } from "./icons.js";

const tauri = window.__TAURI__;
const invoke = tauri.core.invoke;
const listen = tauri.event.listen;
const appWindow = tauri.window.getCurrentWindow();

const $ = (id) => document.getElementById(id);
const all = (selector, root = document) => Array.from(root.querySelectorAll(selector));
const show = (element, visible) => { element.hidden = !visible; };
const esc = (text) => String(text ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

// ------------------------------------------------------------------- state

const S = {
  os: "windows",
  look: { accent: null, transparency: false },
  revealLabel: "Show in Explorer",
  page: "encode",
  ffmpeg: null, // { found, version, path, problem, saved } once checked
  settings: null,
  history: [],
  enc: {
    items: [], // { path, name, icon, kind, size, bytes, measuring }
    resolution: "1080p", mode: "lossless", cell: 8, parity: 64,
    encrypt: true, compress: true, smallest: false, verify: true,
    dest: "keep", fps: null, // Smart delivery: where the video is going
    av1Choice: null, // null: automatic (YouTube, small jobs); true/false: chosen
    keyFile: null, keyError: "",
    pw: null, // { score, enough, warning } for the password being typed
    packed: null, // { key, bytes } once the items' compressed size is measured
    chosenOutput: null, output: null,
    result: null, // { done } or { error }
    lastPreview: null,
  },
  dec: {
    video: null, probing: false, keyFile: null,
    out: null, // { path, friendly }
    unlockError: "",
    result: null,
  },
  yt: {
    text: "", // the link as last typed or pasted
    checking: false, info: null, error: null,
    playing: false, // the preview player is on
    downloading: false, progress: null, // { fraction, text }
  },
  tools: null, // { rows, checked, updates } from tools_status
  toolsChecking: false,
  toolJob: null, // { key, fraction, text } while a tool installs
  toolNotes: {}, // key -> { text, kind: "error" | "done" } after an install
  job: null, // { encoding, name, meta, tag, view }
  dialog: null, // "cancel" | "clear"
  drag: null, // number of items being dragged over the window
};

// ------------------------------------------------------------ look and feel

const media = {
  dark: window.matchMedia("(prefers-color-scheme: dark)"),
  lessTransparency: window.matchMedia("(prefers-reduced-transparency: reduce)"),
};

function isDark() {
  const appearance = S.settings?.appearance ?? "system";
  return appearance === "dark" || (appearance === "system" && media.dark.matches);
}

function hexToRgb(hex) {
  const value = parseInt(hex.slice(1), 16);
  return [(value >> 16) & 255, (value >> 8) & 255, value & 255];
}
function rgbToHex([r, g, b]) {
  return "#" + [r, g, b].map((c) => Math.round(Math.max(0, Math.min(255, c))).toString(16).padStart(2, "0")).join("");
}
function luminance(rgb) {
  const [r, g, b] = rgb.map((c) => { c /= 255; return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function contrast(a, b) {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
}

/** The accent tokens from the system accent, by the rules in the design values table. */
function applyAccent(dark) {
  const root = $("app");
  const accent = S.look.accent?.[dark ? "dark" : "light"];
  const names = ["--accent", "--accent-ink", "--on-accent", "--accent-soft", "--focus"];
  if (!accent) { names.forEach((name) => root.style.removeProperty(name)); return; }
  const rgb = hexToRgb(accent);
  const background = dark ? [28, 28, 30] : [246, 246, 247];
  // accent-ink: darken (light) or lighten (dark) until it reaches 3:1 against the content.
  let ink = rgb.slice();
  for (let step = 0; step < 20 && contrast(ink, background) < 3; step++) {
    ink = ink.map((c) => (dark ? c + (255 - c) * 0.12 : c * 0.88));
  }
  const [r, g, b] = rgb;
  root.style.setProperty("--accent", accent);
  root.style.setProperty("--accent-ink", rgbToHex(ink));
  root.style.setProperty("--on-accent", luminance(rgb) > 0.45 ? "rgba(0,0,0,0.85)" : "#fff");
  root.style.setProperty("--accent-soft", `rgba(${r},${g},${b},${dark ? 0.2 : 0.12})`);
  root.style.setProperty("--focus", `rgba(${r},${g},${b},${dark ? 0.6 : 0.5})`);
}

function applyTheme() {
  const dark = isDark();
  const root = $("app");
  root.classList.toggle("t-dark", dark);
  root.classList.toggle("t-light", !dark);
  root.classList.toggle("os-win", S.os === "windows");
  root.classList.toggle("os-linux", S.os === "linux");
  const material = S.settings?.material ?? "acrylic";
  root.classList.toggle("solid", !S.look.transparency || media.lessTransparency.matches || material === "solid");
  root.classList.toggle("mat-mica", material === "mica");
  applyAccent(dark);
}

// ------------------------------------------------------------------- helpers

function setSeg(name, value) {
  all(`[data-seg="${name}"] button`).forEach((button) => {
    const on = button.dataset.value === String(value);
    button.setAttribute("aria-checked", on ? "true" : "false");
    button.tabIndex = on ? 0 : -1;
  });
}
function setSwitch(name, on) {
  all(`[data-switch="${name}"]`).forEach((button) => button.setAttribute("aria-checked", on ? "true" : "false"));
}

function drawGray(canvas, preview) {
  if (!canvas || !preview) return;
  const { width, height } = preview;
  const bytes = Uint8Array.from(atob(preview.pixels), (c) => c.charCodeAt(0));
  if (canvas.width !== width || canvas.height !== height) { canvas.width = width; canvas.height = height; }
  const context = canvas.getContext("2d");
  const image = context.createImageData(width, height);
  for (let i = 0; i < bytes.length; i++) {
    const at = i * 4;
    image.data[at] = image.data[at + 1] = image.data[at + 2] = bytes[i];
    image.data[at + 3] = 255;
  }
  context.putImageData(image, 0, 0);
}
function clearCanvas(canvas) {
  const context = canvas.getContext("2d");
  context.fillStyle = "#000";
  context.fillRect(0, 0, canvas.width, canvas.height);
}

/** Ask the Rust side how hard the password is to guess (zxcvbn, spec 6.4).
 * Answers can arrive out of order, so only the one for the current text is kept. */
async function checkStrength() {
  const password = $("password").value;
  if (!password) { S.enc.pw = null; render(); return; }
  try {
    const result = await invoke("password_strength", { password });
    if ($("password").value === password) { S.enc.pw = result; render(); }
  } catch (error) { fail(error); }
}

/** Hide the password text again and clear both fields. */
function resetPasswordFields() {
  for (const id of ["password", "confirm"]) { $(id).value = ""; $(id).type = "password"; }
  S.enc.pw = null;
  $("pw-eye").innerHTML = icon("eye", 15, 1.3);
  $("pw-eye").title = "Show password";
}

const plural = (count, noun) => `${count} ${noun}${count === 1 ? "" : "s"}`;

function fail(message) {
  // An unexpected failure talking to the Rust side: show it rather than do nothing.
  console.error(message);
  S.enc.keyError = String(message);
  render();
}

// -------------------------------------------------------------------- render

const TITLES = {
  encode: ["File to Video", "Turn files into a video. Decode restores them exactly."],
  decode: ["Video to File", "Restore the original files from an ATV video."],
  history: ["History", ""],
  settings: ["Settings", ""],
  setup: ["Welcome to ATV", ""],
  wizard: ["Setup", ""],
};

let currentScreen = null;

function screenFor() {
  if (S.wizard) return "wizard";
  const ffmpegMissing = S.ffmpeg && !S.ffmpeg.found;
  if (S.page === "encode" || S.page === "decode") {
    const encoding = S.page === "encode";
    if (S.job && S.job.encoding === encoding) return "running";
    if (ffmpegMissing) return "setup";
    const result = encoding ? S.enc.result : S.dec.result;
    if (result?.error) return "error";
    if (result) return encoding ? "enc-done" : "dec-done";
    return S.page;
  }
  return S.page;
}

function render() {
  const screen = screenFor();
  const ffmpegMissing = S.ffmpeg && !S.ffmpeg.found;

  // Sidebar
  all(".nav-item[data-page]").forEach((item) => {
    const current = item.dataset.page === S.page;
    if (current) item.setAttribute("aria-current", "page"); else item.removeAttribute("aria-current");
    item.classList.toggle("off", Boolean(S.wizard) || (Boolean(ffmpegMissing) && (item.dataset.page === "encode" || item.dataset.page === "decode")));
  });
  const jobElsewhere = S.job && S.page !== (S.job.encoding ? "encode" : "decode");
  show($("job-chip"), Boolean(jobElsewhere));

  // Toolbar
  const [title, subtitle] = TITLES[screen === "setup" || screen === "wizard" ? screen : S.page];
  $("title").textContent = title;
  $("subtitle").textContent = subtitle;
  show($("clear-history"), S.page === "history");
  $("clear-history").disabled = S.history.length === 0;
  show($("banner"), Boolean(ffmpegMissing) && screen !== "setup" && screen !== "wizard");
  show($("banner-open"), S.page !== "settings");

  // Screens
  const ids = { wizard: "screen-wizard", setup: "screen-setup", encode: "screen-encode", decode: "screen-decode", running: "screen-running", "enc-done": "screen-enc-done", "dec-done": "screen-dec-done", error: "screen-error", history: "screen-history", settings: "screen-settings" };
  for (const [name, id] of Object.entries(ids)) show($(id), name === screen);
  if (screen !== currentScreen) {
    const element = $(ids[screen]);
    element.classList.remove("enter"); void element.offsetWidth; element.classList.add("enter");
    currentScreen = screen;
  }

  if (screen === "wizard") renderWizard();
  if (screen === "setup") renderSetup();
  if (screen === "encode") renderEncode();
  if (screen === "decode") renderDecode();
  if (screen === "error") renderError();
  if (screen === "history") renderHistory();
  if (screen === "settings") renderSettings();
  if (screen === "running") renderRunHeader();
  renderTools();

  // Drag-over (1c)
  const dragging = S.drag !== null;
  show($("drag-overlay"), dragging);
  all(".screen").forEach((element) => element.classList.toggle("dimmed", dragging));
  if (dragging) {
    $("drag-title").textContent = S.page === "decode" ? "Drop to open the video" : `Drop to add ${plural(S.drag, "item")}`;
  }

  // Dialog
  show($("dialog"), S.dialog !== null);
}

function renderSetup() {
  $("setup-problem").textContent = S.ffmpeg?.problem ?? "";
  const row = S.tools?.rows.find((r) => r.key === "ffmpeg");
  const installable = row?.action === "install";
  const busy = S.toolJob?.key === "ffmpeg";
  $("setup-sub").textContent = installable
    ? "ffmpeg is a free video tool. ATV uses it to write and read videos. ATV can download the official build for you, or install it yourself and choose Check Again."
    : "ffmpeg is a free video tool. ATV uses it to write and read videos. Install it, then choose Check Again.";
  show($("setup-install"), installable && !busy);
  $("setup-install").disabled = Boolean(S.toolJob || S.job);
  $("setup-check").className = installable ? "btn" : "btn primary";
  $("setup-check").disabled = busy;
  show($("setup-progress"), busy);
  const note = S.toolNotes.ffmpeg;
  show($("setup-error"), note?.kind === "error");
  $("setup-error-text").textContent = note?.kind === "error" ? note.text : "";
  renderToolProgress();
}

// ------------------------------------------------------------------ setup wizard

const WIZARD_STEPS = ["welcome", "tools", "ready"];
const WIZARD_TOOLS = ["ffmpeg", "ytdlp", "deno"];

/** A fresh wizard. It shows on the first start, and from Settings → Tools. */
function newWizard(step = "welcome") {
  return { step, youtube: true, queue: [], installing: false };
}

function wizardRow(key) { return S.tools?.rows.find((row) => row.key === key) ?? null; }

function wizardInstalled(key) {
  if (key === "ffmpeg") return Boolean(S.ffmpeg?.found);
  const row = wizardRow(key);
  return Boolean(row) && row.state !== "missing";
}

function wizardWanted(key) { return key === "ffmpeg" || S.wizard.youtube; }

/** The wanted tools ATV can install itself that are not there yet. */
function wizardNeeded() {
  return WIZARD_TOOLS.filter((key) => wizardWanted(key) && !wizardInstalled(key) && wizardRow(key)?.action === "install");
}

function renderWizard() {
  const w = S.wizard;
  const at = WIZARD_STEPS.indexOf(w.step);
  all("#wz-steps span").forEach((span) => {
    const index = WIZARD_STEPS.indexOf(span.dataset.step);
    span.className = index === at ? "current" : index < at ? "done" : "";
  });
  for (const step of WIZARD_STEPS) show($(`wz-${step}`), step === w.step);
  if (w.step === "tools") renderWizardTools();
  if (w.step === "ready") {
    const youtube = wizardInstalled("ytdlp") && wizardInstalled("deno");
    const ffmpeg = S.ffmpeg?.found
      ? `ffmpeg ${wizardRow("ffmpeg")?.version || S.ffmpeg.version} is ready, so you can make and restore videos.`
      : "ffmpeg is not installed yet: Encode and Decode stay off until it is.";
    const links = youtube ? "You can open videos from YouTube links too." : "YouTube links are off; you can add them any time in Settings → Tools.";
    $("wz-ready-text").textContent = `${ffmpeg} ${links}`;
  }
}

function renderWizardTools() {
  const w = S.wizard;
  $("wz-youtube").setAttribute("aria-checked", w.youtube ? "true" : "false");
  $("wz-youtube").disabled = w.installing;
  for (const key of WIZARD_TOOLS) {
    const el = document.querySelector(`[data-wz="${key}"]`);
    const part = (name) => el.querySelector(`[data-part="${name}"]`);
    const row = wizardRow(key);
    const wanted = wizardWanted(key);
    const installed = wizardInstalled(key);
    const busy = S.toolJob?.key === key;
    el.classList.toggle("off", !wanted);
    if (row?.name) el.querySelector(".tool-name").textContent = row.name;
    part("purpose").textContent = row?.purpose ?? "Checking…";
    let state = "Checking…";
    if (busy) state = "Installing…";
    else if (installed) state = `Installed${row?.version ? ` · ${row.version}` : ""}`;
    else if (w.queue.includes(key)) state = "Waiting";
    else if (!wanted) state = "Skipped";
    else if (row || key === "ffmpeg") state = "Not installed";
    part("state").textContent = state;
    part("dot").className = `status-dot${installed || !wanted ? "" : " bad"}`;
    const note = S.toolNotes[key];
    // Where ATV cannot install it itself (macOS), the row's note says how.
    const text = note?.text || (!installed && wanted && row && row.action !== "install" ? row.note : "");
    part("note").textContent = text;
    part("note").className = `tool-note${note ? ` ${note.kind}` : ""}`;
    show(part("note"), Boolean(text));
    show(part("progress"), busy);
  }
  const needed = wizardNeeded();
  const ffmpegMissing = !S.ffmpeg?.found;
  show($("wz-install"), needed.length > 0 && !w.installing);
  $("wz-install").textContent = needed.length === 1 ? `Install ${wizardRow(needed[0])?.name ?? needed[0]}` : `Install ${needed.length} Tools`;
  $("wz-install").disabled = Boolean(S.job || S.toolJob || S.yt.downloading);
  show($("wz-continue"), needed.length === 0 && !w.installing);
  show($("wz-check"), ffmpegMissing && !w.installing && needed.length === 0);
  $("wz-back").disabled = w.installing;
  show($("wz-skip"), !w.installing);
  renderToolProgress();
}

/** Install the next tool in the wizard's queue, one at a time. */
function wizardInstallNext() {
  const w = S.wizard;
  if (!w) return;
  const key = w.queue.shift();
  if (!key) { w.installing = false; render(); return; }
  installTool(key);
}

/** The wizard is done or skipped: remember it, and go to Encode. */
async function finishWizard() {
  try { S.settings = await invoke("finish_setup"); } catch (error) { fail(error); }
  S.wizard = null;
  S.page = "encode";
  render();
}

// ------------------------------------------------------------------ tools

const CHIPS = { ok: "Up to date", missing: "Not installed", unknown: "Unknown version" };

function renderTools() {
  const v = S.tools;
  show($("nav-badge"), Boolean(v?.updates));
  $("tools-checked").textContent = S.toolsChecking ? "Checking…" : (v?.checked ?? "");
  $("tools-check").disabled = S.toolsChecking || Boolean(S.toolJob);
  $("wz-again").disabled = Boolean(S.job || S.toolJob || S.yt.downloading);
  const busyAnywhere = Boolean(S.job || S.toolJob || S.yt.downloading);
  for (const el of all("[data-tool-row]")) {
    const key = el.dataset.toolRow;
    const row = v?.rows.find((r) => r.key === key);
    const part = (name) => el.querySelector(`[data-part="${name}"]`);
    if (!row) { part("purpose").textContent = "Checking…"; continue; }
    el.querySelector(".tool-name").textContent = row.name;
    part("version").textContent = row.version ? [row.version, row.source].filter(Boolean).join(" · ") : "";
    const chip = part("chip");
    chip.className = `tool-chip ${row.state}`;
    chip.textContent = row.state === "update" ? `Update to ${row.latest}` : (CHIPS[row.state] ?? "");
    show(chip, Boolean(chip.textContent));
    const dot = part("dot");
    const missing = key === "ffmpeg" ? S.ffmpeg && !S.ffmpeg.found : row.state === "missing";
    dot.className = `status-dot${missing ? " bad" : row.state === "update" ? " warn" : ""}`;
    part("purpose").textContent = row.purpose;
    const pathLine = part("path");
    if (pathLine) { pathLine.textContent = row.path; pathLine.title = row.path; show(pathLine, Boolean(row.path)); }
    const note = S.toolNotes[key];
    part("note").textContent = note?.text || row.note;
    part("note").className = `tool-note${note ? ` ${note.kind}` : ""}`;
    show(part("note"), Boolean(note?.text || row.note));
    const busy = S.toolJob?.key === key;
    const action = part("action");
    action.hidden = !row.action || busy;
    action.textContent = row.action === "install" ? `Install ${row.name}` : "Update";
    action.className = `btn small${row.action ? " primary" : ""}`;
    action.disabled = busyAnywhere;
    show(part("progress"), busy);
  }
  renderToolProgress();
}

/** Progress bars for the tool being installed, wherever they show. */
function renderToolProgress() {
  const job = S.toolJob;
  if (!job) return;
  const width = job.fraction == null ? "3%" : `${Math.round(job.fraction * 1000) / 10}%`;
  const text = job.fraction == null ? job.text : `${Math.floor(job.fraction * 100)}% · ${job.text}`;
  const el = document.querySelector(`[data-tool-row="${job.key}"]`);
  if (el) {
    el.querySelector('[data-part="bar"]').style.width = width;
    el.querySelector('[data-part="progress-text"]').textContent = text;
  }
  if (job.key === "ffmpeg") { $("setup-bar").style.width = width; $("setup-progress-text").textContent = text; }
  const wizardRowEl = document.querySelector(`[data-wz="${job.key}"]`);
  if (wizardRowEl) {
    wizardRowEl.querySelector('[data-part="bar"]').style.width = width;
    wizardRowEl.querySelector('[data-part="progress-text"]').textContent = text;
  }
  if (job.key === "ytdlp" || job.key === "deno") $("yt-fix-progress").textContent = `Installing… ${text}`;
}

async function loadTools(check) {
  if (check !== "no") { S.toolsChecking = true; render(); }
  try { S.tools = await invoke("tools_status", { check }); } catch (error) { console.error(error); }
  S.toolsChecking = false;
  render();
}

async function installTool(key) {
  if (S.job || S.toolJob || S.yt.downloading) return;
  delete S.toolNotes[key];
  S.toolJob = { key, fraction: null, text: "Asking for the newest official release…" };
  render();
  try {
    await invoke("install_tool", { key });
  } catch (error) {
    S.toolJob = null;
    S.toolNotes[key] = { kind: "error", text: errorText(error) };
    if (S.wizard) { S.wizard.queue = []; S.wizard.installing = false; }
    render();
  }
}

function errorText(error) {
  if (typeof error === "string") return error;
  return [`${error.title}. ${error.text}`, error.detail].filter(Boolean).join(" ");
}

async function toolFinished(finished) {
  const key = finished.key;
  S.toolJob = null;
  if (finished.outcome === "done") S.toolNotes[key] = { kind: "done", text: `${finished.name} ${finished.version} installed from the official release.` };
  else if (finished.outcome === "failed") S.toolNotes[key] = { kind: "error", text: errorText(finished.error) };
  else S.toolNotes[key] = { kind: "error", text: "Cancelled. Nothing was changed." };
  render();
  if (key === "ffmpeg") await checkFfmpeg();
  await loadTools("no");
  if (S.wizard?.installing) {
    if (finished.outcome === "done") wizardInstallNext();
    else { S.wizard.queue = []; S.wizard.installing = false; render(); }
  }
  // A YouTube link waiting for yt-dlp: try it again now.
  if (finished.outcome === "done" && key !== "ffmpeg" && S.yt.text && S.yt.error) ytInput(true);
}

function renderEncode() {
  const e = S.enc;
  const ready = e.items.length > 0;
  show($("enc-empty"), !ready);
  show($("enc-ready"), ready);
  $("quiet-res").textContent = e.resolution === "4k" ? "4K" : "1080p";
  $("quiet-mode").textContent = e.mode === "robust" ? "Robust" : "Lossless";
  $("quiet-enc").textContent = e.encrypt ? "On" : "Off";
  $("quiet-cmp").textContent = e.compress ? "On" : "Off";
  $("quiet-vrf").textContent = e.verify ? "On" : "Off";
  if (!ready) return;

  // Items
  $("enc-items").innerHTML = e.items.map((item, index) => `
    <div class="item">
      <div class="tile">${icon(item.icon === "folder" ? "folder" : item.icon === "archive" ? "archive" : "document", 16, 1.3)}</div>
      <div class="names"><span title="${esc(item.path)}">${esc(item.name)}</span><span>${esc(item.measuring ? "Measuring…" : item.kind)}</span></div>
      <div class="size">${esc(item.measuring ? "" : item.size)}</div>
      <button class="icon-btn" data-action="remove-item" data-index="${index}" title="Remove" aria-label="Remove ${esc(item.name)}">${icon("close", 10, 2)}</button>
    </div>`).join("");
  const measuring = e.items.some((item) => item.measuring);
  const total = e.items.reduce((sum, item) => sum + (item.bytes || 0), 0);
  $("enc-count").textContent = plural(e.items.length, "item");
  $("enc-total").textContent = measuring ? "Measuring…" : `${e.items.length ? sizeText(total) : ""} total`;

  // Options
  setSeg("resolution", e.resolution);
  setSeg("mode", e.mode);
  all("[data-dest]").forEach((b) => b.setAttribute("aria-checked", String(b.dataset.dest === e.dest)));
  $("dest-hint").textContent = DESTINATIONS[e.dest]?.hint ?? "";
  setSeg("cell", e.cell);
  setSeg("parity", e.parity);
  const av1Ready = Boolean(S.ffmpeg?.av1);
  show($("av1-row"), av1Ready && e.mode === "robust");
  setSwitch("av1", useAv1());
  const minutes = av1Minutes();
  $("av1-hint").textContent = useAv1()
    ? `AV1: about half the size. Creating it takes about ${minutes < 1 ? "a minute" : `${Math.ceil(minutes)} minutes`}.`
    : `H.264: fast. AV1 would be about half the size but take about ${minutes < 1 ? "a minute" : `${Math.ceil(minutes)} minutes`}.`;
  $("parity-hint").textContent = Number(e.parity) === 32
    ? "Standard holds about 6% more, with less margin. Made for YouTube, 720p copy included."
    : "Strong survives harsher copies, such as WhatsApp’s 480p, and a lost frame. Holds about 6% less.";
  show($("block-row"), e.mode === "robust");
  $("mode-hint").textContent = e.mode === "robust"
    ? "Bigger blocks plus error correction. Made to survive sites that re-compress video."
    : "One pixel per bit. Highest capacity. For your own disk or cloud drive.";
  setSwitch("encrypt", e.encrypt);
  setSwitch("compress", e.compress);
  setSwitch("smallest", e.smallest);
  show($("smallest-row"), e.compress);
  setSwitch("verify", e.verify);

  // Protection
  show($("protection"), e.encrypt);
  $("protection-note").textContent = e.encrypt
    ? "Without the password or key file, the files cannot be recovered."
    : "Encryption is off: anyone with the video can restore the files.";
  const password = $("password").value;
  const confirm = $("confirm").value;
  // zxcvbn's 0-4 score; only 4 may be a video's only secret (spec 6.4).
  const pw = password ? e.pw : null;
  const bars = pw ? Math.max(pw.score, 1) : 0;
  show($("strength"), Boolean(pw));
  all("#strength .bars i").forEach((bar, index) => {
    bar.style.background = index >= bars ? "var(--ctl)" : pw.enough ? "var(--good)" : bars >= 2 ? "var(--warn)" : "var(--bad)";
  });
  $("strength-word").textContent = pw ? (pw.enough ? "Strong" : ["Weak", "Weak", "Fair", "Almost"][pw.score] ?? "Weak") : "";
  const weak = e.encrypt && Boolean(pw) && !pw.enough && !e.keyFile;
  show($("pw-weak"), weak);
  $("pw-weak-text").textContent = weak ? `${pw.warning ? `${pw.warning} ` : ""}Too easy to guess: use a longer phrase, or add a key file.` : "";
  const matches = password.length > 0 && password === confirm;
  const mismatch = password.length > 0 && confirm.length > 0 && password !== confirm;
  show($("pw-match"), matches);
  $("confirm-field").classList.toggle("bad", mismatch);
  show($("key-none"), !e.keyFile);
  show($("key-chip"), Boolean(e.keyFile));
  $("key-name").textContent = e.keyFile?.name ?? "";
  show($("key-error"), Boolean(e.keyError));
  $("key-error-text").textContent = e.keyError;
  const chip = (label) => `<span class="chip">${icon("check", 11, 2.4)}${label}</span>`;
  const missing = (label) => `<span class="chip missing">${label}</span>`;
  const usesPassword = password.length > 0;
  const parts = [];
  parts.push(usesPassword ? chip("Password") : missing("Password"));
  if (usesPassword && e.keyFile) parts.push("<span>and</span>");
  parts.push(e.keyFile ? chip("Key file") : missing("Key file"));
  $("needed").innerHTML = parts.join("");

  // Output
  $("out-name").textContent = e.output?.name ?? "";
  $("out-folder").textContent = e.output?.folder ?? "";
  $("out-folder").title = e.output?.path ?? "";

  // Summary and the button (brief: disabled until there is an item and, with
  // encryption on, a password or a key file).
  const summary = e.summary;
  $("sum-length").textContent = measuring || !summary ? "—" : summary.length;
  $("sum-size").textContent = measuring || !summary ? "—" : summary.size;
  $("sum-frames").textContent = measuring || !summary ? "—" : summary.frames;
  let reason = "";
  if (measuring) reason = "Measuring the items…";
  else if (e.encrypt && !usesPassword && !e.keyFile) reason = "Add a password or a key file";
  else if (e.encrypt && usesPassword && !e.keyFile && !e.pw) reason = "Checking the password…";
  else if (weak) reason = "Choose a harder password, or add a key file";
  else if (e.encrypt && usesPassword && !confirm) reason = "Type the password again to confirm it";
  else if (mismatch) reason = "The two passwords don’t match";
  $("start-reason").textContent = reason || (measuringPacked() && e.compress ? "Working out the compressed size…" : "");
  $("create-video").disabled = Boolean(reason) || Boolean(S.job);
}

function sizeText(bytes) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes, unit = 0;
  while (value >= 1000 && unit < units.length - 1) { value /= 1000; unit++; }
  if (unit === 0) return `${bytes} B`;
  return `${value >= 100 ? value.toFixed(0) : value >= 10 ? value.toFixed(1) : value.toFixed(2)} ${units[unit]}`;
}

function renderDecode() {
  const d = S.dec;
  const loaded = Boolean(d.video);
  show($("dec-empty"), !loaded);
  show($("dec-loaded"), loaded);
  $("dec-drop-title").textContent = d.probing ? "Reading the video…" : "Drop an ATV video";
  $("dec-choose").disabled = d.probing || S.yt.downloading;
  if (!loaded) { renderYoutube(); return; }
  const v = d.video;
  show($("dec-source-line"), Boolean(d.online));
  $("dec-source").textContent = d.online ? `YouTube, ${d.online.quality} copy` : "";
  $("dec-name").textContent = v.name;
  $("dec-format").textContent = `ATV format ${v.version}`;
  $("dec-name").title = v.path;
  $("dec-res").textContent = v.resolution;
  show($("dec-resized-line"), Boolean(v.resized));
  $("dec-resized").textContent = v.resized ? `${v.resized}, read scaled back` : "";
  $("dec-mode").textContent = v.mode;
  $("dec-length").textContent = v.length;
  $("dec-protection").textContent = v.encrypted ? "Encrypted" : "Not encrypted";
  show($("dec-lock-icon"), v.encrypted);
  show($("dec-names-hidden"), v.encrypted);
  show($("dec-names-plain"), !v.encrypted);
  $("dec-size").textContent = `${v.size} video file`;
  show($("unlock-card"), v.encrypted);
  show($("dec-pw-field"), v.needs_password);
  show($("dec-key-row"), v.needs_key_file);
  show($("dec-key-chip"), Boolean(d.keyFile));
  $("dec-key-name").textContent = d.keyFile?.name ?? "";
  show($("unlock-error"), Boolean(d.unlockError));
  $("unlock-error-text").textContent = d.unlockError;
  $("dec-pw-field").classList.toggle("bad", Boolean(d.unlockError) && v.needs_password);
  $("dec-out").textContent = d.out?.friendly ?? "";
  $("dec-out").title = d.out?.path ?? "";
  $("dec-bar-length").textContent = v.length;
  $("dec-bar-frames").textContent = v.frames;
  let reason = "";
  if (v.needs_password && !$("dec-password").value) reason = v.needs_key_file ? "Enter the password and choose the key file" : "Enter the password";
  else if (v.needs_key_file && !d.keyFile) reason = "Choose the key file";
  $("dec-reason").textContent = reason;
  $("restore-files").disabled = Boolean(reason) || Boolean(S.job);
}

function renderYoutube() {
  const y = S.yt;
  $("yt-link").disabled = y.downloading;
  show($("yt-checking"), y.checking);
  show($("yt-error"), Boolean(y.error));
  $("yt-error-text").textContent = !y.error ? "" : typeof y.error === "string" ? y.error : `${y.error.title}. ${y.error.text}`;
  const detail = y.error?.detail ?? "";
  show($("yt-error-detail"), Boolean(detail));
  $("yt-error-detail").textContent = detail;
  // A missing or outdated yt-dlp can be fixed from here.
  const code = y.error?.code;
  const ytRow = S.tools?.rows.find((r) => r.key === "ytdlp");
  const fix = code === "downloader" && ytRow?.action ? "install"
    : code === "download" && /403|forbidden|sign in|bot/i.test(detail) ? "check" : null;
  const installing = S.toolJob && (S.toolJob.key === "ytdlp" || S.toolJob.key === "deno");
  show($("yt-fix-row"), Boolean(fix) || Boolean(installing));
  $("yt-fix").hidden = Boolean(installing) || !fix;
  $("yt-fix").textContent = fix === "install" ? "Install yt-dlp" : "Check for Tool Updates";
  $("yt-fix").dataset.fix = fix ?? "";
  $("yt-fix").disabled = Boolean(S.job || S.toolJob);
  if (!installing) $("yt-fix-progress").textContent = "";
  show($("yt-preview"), Boolean(y.info));
  if (!y.info) { $("yt-player").replaceChildren(); delete $("yt-player").dataset.state; return; }
  const v = y.info;
  $("yt-title").textContent = v.title;
  $("yt-title").title = v.title;
  $("yt-channel").textContent = v.channel || "—";
  $("yt-length").textContent = v.length;
  $("yt-quality").textContent = v.quality;
  show($("yt-small"), v.small);
  renderPlayer(v, y.playing);
  show($("yt-actions"), !y.downloading);
  show($("yt-progress"), y.downloading);
  $("yt-download").disabled = Boolean(S.job) || !S.ffmpeg?.found;
  renderDownloadProgress();
}

/** The preview: YouTube's thumbnail, and YouTube's own player once asked for. */
function renderPlayer(v, playing) {
  const player = $("yt-player");
  const state = `${v.id}:${playing}`;
  if (player.dataset.state === state) return;
  player.dataset.state = state;
  if (playing) {
    const frame = document.createElement("iframe");
    frame.title = v.title;
    frame.allow = "autoplay; encrypted-media; picture-in-picture; fullscreen";
    frame.allowFullscreen = true;
    // YouTube's player needs to know which page it is on.
    frame.referrerPolicy = "strict-origin-when-cross-origin";
    frame.src = v.embed;
    player.replaceChildren(frame);
  } else {
    const picture = document.createElement("img");
    picture.alt = "";
    picture.src = v.thumbnail;
    const play = document.createElement("button");
    play.className = "yt-play";
    play.dataset.action = "yt-play";
    play.title = "Play here";
    play.setAttribute("aria-label", "Play the video here");
    play.innerHTML = icon("play", 22, 1.2);
    player.replaceChildren(picture, play);
  }
}

function renderDownloadProgress() {
  const p = S.yt.progress;
  if (!S.yt.downloading || !p) return;
  $("yt-bar").style.width = p.fraction == null ? "4%" : `${Math.round(p.fraction * 1000) / 10}%`;
  $("yt-progress-text").textContent = p.fraction == null ? p.text : `${Math.floor(p.fraction * 100)}% · ${p.text}`;
}

function renderError() {
  const encoding = S.page === "encode";
  const error = (encoding ? S.enc.result : S.dec.result).error;
  const warn = error.code === "disk" || error.code === "not_atv";
  $("err-disc").className = `disc ${warn ? "warn" : "bad"}`;
  $("err-title").textContent = error.title;
  $("err-text").textContent = error.text;
  $("err-tech").textContent = error.detail || "";
  const recoverable = error.recoverable;
  show($("err-recover"), recoverable !== null && recoverable !== undefined);
  if (recoverable !== null && recoverable !== undefined) {
    const pct = `${Math.floor(recoverable * 100)}%`;
    $("err-recover-pct").textContent = pct;
    $("err-recover-fill").style.width = pct;
    $("err-recover-note").textContent = S.dec.video?.encrypted
      ? "Encrypted data can only be restored when every block is recovered."
      : "The files can only be restored when every block is recovered.";
  }
  let first, second;
  if (encoding) { first = "Try Again"; second = "Back to Options"; }
  else if (error.code === "not_atv") { first = "Choose Another Video"; second = ""; }
  else { first = "Choose Another Video"; second = "Back"; }
  $("err-first").textContent = first;
  $("err-second").textContent = second;
  show($("err-second"), Boolean(second));
}

function renderRunHeader() {
  const job = S.job;
  $("run-name").textContent = job.name;
  $("run-meta").textContent = job.meta;
  $("run-tag").textContent = job.tag;
}

function renderHistory() {
  const empty = S.history.length === 0;
  show($("history-list"), !empty);
  show($("history-empty"), empty);
  const result = {
    done: ["good", "Done"], failed: ["bad", "Failed"], cancelled: ["plain", "Cancelled"],
  };
  $("history-rows").innerHTML = S.history.map((row, index) => {
    const [tone, label] = result[row.result];
    return `
    <div class="trow">
      <div class="tile">${icon(row.encode ? "video" : "document", 15)}</div>
      <div class="names"><span title="${esc(row.name)}">${esc(row.name)}</span><span>${esc(row.direction)} · ${esc(row.date)}</span></div>
      <span class="fmt">${esc(row.format)}</span>
      <span class="lock" title="${row.encrypted ? "Encrypted" : ""}">${row.encrypted ? icon("lock", 13) : ""}</span>
      <span class="size">${esc(row.size)}</span>
      <span><span class="badge small ${tone}">${label}</span></span>
      <div class="acts">
        <button class="icon-btn square" data-action="history-reveal" data-index="${index}" title="${esc(S.revealLabel)}" aria-label="${esc(S.revealLabel)}" ${row.can_show ? "" : "disabled"}>${icon("folder-reveal", 14, 1.3)}</button>
        <button class="icon-btn square" data-action="history-remove" data-index="${index}" title="Remove from history" aria-label="Remove from history">${icon("close", 10, 2)}</button>
      </div>
    </div>`;
  }).join("");
}

function renderSettings() {
  const s = S.settings;
  setSeg("appearance", s.appearance);
  setSeg("material", s.material);
  show($("material-row"), S.os === "windows");
  $("material-hint").textContent = !S.look.transparency
    ? "Transparency effects are off in Windows settings, so the window stays solid."
    : { acrylic: "See-through glass: what is behind the window shows, blurred.",
        mica: "Calm glass: only your wallpaper’s colours show, never other windows.",
        solid: "No transparency." }[s.material];
  setSeg("default-resolution", s.resolution);
  setSeg("default-mode", s.mode);
  setSeg("default-cell", s.cell);
  setSeg("default-parity", s.parity);
  show($("default-cell-row"), s.mode === "robust");
  show($("default-parity-row"), s.mode === "robust");
  setSwitch("default-compress", s.compress);
  setSwitch("default-smallest", s.smallest);
  show($("default-smallest-row"), s.compress);
  setSwitch("default-verify", s.verify);
  $("set-out").textContent = s.output_folder_friendly;
  $("set-out").title = s.output_folder;
  const f = S.ffmpeg;
  const checking = f === null;
  $("ff-path").textContent = f?.path || (checking ? "" : "Not set — searched for automatically");
  $("ff-path").title = f?.path ?? "";
  show($("ff-auto"), Boolean(f?.saved));
  show($("ff-missing"), Boolean(f && (!f.found || f.problem)));
  show($("ff-problem-line"), Boolean(f?.problem));
  $("ff-problem").textContent = f?.problem ?? "";
  $("about-version").textContent = `Version ${S.version}`;
}

// --------------------------------------------------------------- the running job

function updateRunning(update) {
  const view = update;
  $("run-pct").textContent = view.percent;
  $("run-fill").style.width = `${Math.round(view.progress * 1000) / 10}%`;
  $("run-bar").setAttribute("aria-valuenow", String(Math.round(view.progress * 100)));
  const current = view.stages.filter((stage) => stage.state === 1).map((stage) => stage.label);
  $("run-now").textContent = current[current.length - 1] ?? "";
  $("run-frame").textContent = view.frame_text || "Starting…";
  $("run-speed").textContent = view.speed;
  $("run-elapsed").textContent = view.elapsed;
  $("run-left").textContent = view.remaining;
  $("run-repaired").textContent = view.repaired;
  $("run-steps").innerHTML = view.stages.map((stage) => {
    const mark = stage.state === 2
      ? `<span class="done">${icon("check", 10, 2.6)}</span>`
      : stage.state === 1
        ? `<svg class="spin" width="18" height="18" viewBox="0 0 18 18" aria-hidden="true"><circle cx="9" cy="9" r="7" fill="none" stroke="var(--ctl)" stroke-width="2"/><circle cx="9" cy="9" r="7" fill="none" stroke="var(--accent)" stroke-width="2" stroke-linecap="round" stroke-dasharray="13 44"/></svg>`
        : `<span class="dot"></span>`;
    const state = ["waiting", "current", "done"][stage.state];
    return `<div class="step ${state}">${mark}<span>${esc(stage.label)}</span></div>`;
  }).join("");
  if (update.preview) {
    drawGray($("run-canvas"), update.preview);
    if (S.job?.encoding) S.enc.lastPreview = update.preview;
  }
  // Sidebar job indicator
  $("job-chip-pct").textContent = view.percent;
  $("job-chip-ring").setAttribute("stroke-dasharray", `${(view.progress * 47.1).toFixed(1)} 47.1`);
}

function beginJob(job) {
  S.job = { ...job };
  $("job-chip-verb").textContent = job.encoding ? "Encoding" : "Decoding";
  $("job-chip-name").textContent = job.name;
  clearCanvas($("run-canvas"));
  updateRunning({ progress: 0, percent: "0%", frame_text: "", stages: [], speed: "—", elapsed: "0:00", remaining: "—", repaired: "" });
  render();
}

function jobFinished(finished) {
  const encoding = S.job?.encoding ?? finished.encoding;
  S.job = null;
  if (S.dialog === "cancel") S.dialog = null;
  switch (finished.outcome) {
    case "encode_done":
      S.enc.result = { done: finished };
      $("done-folder").textContent = finished.folder;
      $("done-name").textContent = finished.name;
      $("done-length").textContent = finished.length;
      $("done-size").textContent = finished.size;
      $("done-details").textContent = finished.details;
      show($("done-verified"), finished.verified);
      show($("done-keep"), finished.details.includes("Encrypted") && !finished.details.includes("Not encrypted"));
      if (S.enc.lastPreview) drawGray($("done-thumb"), S.enc.lastPreview); else clearCanvas($("done-thumb"));
      // Secrets are not kept on screen longer than needed.
      resetPasswordFields();
      break;
    case "decode_done":
      S.dec.result = { done: finished };
      $("restored-summary").textContent = finished.summary;
      $("restored-repaired").textContent = finished.repaired;
      $("restored-more").textContent = finished.more;
      $("restored-items").innerHTML = finished.items.map((item) => `
        <div class="item tall">
          <div class="tile">${icon(item.icon === "folder" ? "folder" : item.icon === "archive" ? "archive" : "document", 16, 1.3)}</div>
          <div class="names"><span>${esc(item.name)}</span><span>${esc(item.kind)}</span></div>
          <span class="size">${esc(item.size)}</span>
          <span style="color:var(--good)">${icon("check", 14, 2)}</span>
        </div>`).join("");
      $("dec-password").value = "";
      break;
    case "cancelled":
      break;
    case "failed": {
      const error = finished.error;
      if (!encoding && (error.code === "wrong_key" || error.code === "key_required")) {
        // Stays on the unlock step so the person can try again (brief 2e).
        S.dec.unlockError = error.code === "wrong_key" ? error.text : error.text;
      } else if (encoding) {
        S.enc.result = { error };
      } else {
        S.dec.result = { error };
      }
      break;
    }
  }
  render();
}

// ---------------------------------------------------------------- encode actions

async function addInputs(paths) {
  const known = new Set(S.enc.items.map((item) => item.path));
  const fresh = paths.filter((path) => !known.has(path));
  if (fresh.length === 0) return;
  for (const path of fresh) {
    S.enc.items.push({ path, name: path.split(/[\\/]/).filter(Boolean).pop() ?? path, icon: "document", kind: "", size: "", bytes: 0, measuring: true });
  }
  S.enc.result = null;
  await refreshEncode();
  try {
    const described = await invoke("describe_items", { paths: fresh });
    for (const item of described) {
      const index = S.enc.items.findIndex((existing) => existing.path === item.path);
      if (index >= 0) S.enc.items[index] = { ...item, measuring: false };
    }
  } catch (error) { fail(error); }
  await refreshEncode();
}

/** The items, as one string: a measurement belongs to exactly this list. */
function itemsKey() {
  return S.enc.items.map((item) => item.path).join("|");
}

/** The measured compressed size of the current items, or null. */
function packedBytes() {
  const packed = S.enc.packed;
  return packed && packed.key === itemsKey() && packed.bytes !== null ? packed.bytes : null;
}

/** True while the compressed size of the current items is being measured. */
function measuringPacked() {
  const packed = S.enc.packed;
  return Boolean(packed) && packed.key === itemsKey() && packed.bytes === null;
}

/** Ask the Rust side for the output name and the summary strip, then redraw. */
async function refreshEncode() {
  const e = S.enc;
  try {
    e.output = await invoke("suggest_output", { inputs: e.items.map((item) => item.path), mode: e.mode, chosen: e.chosenOutput });
    const total = e.items.reduce((sum, item) => sum + (item.bytes || 0), 0);
    const ask = () => invoke("encode_summary", { bytes: total, resolution: e.resolution, mode: e.mode, cell: Number(e.cell), parity: Number(e.parity), compress: e.compress, frameRate: e.mode === "robust" ? e.fps : null, av1: useAv1(), packed: packedBytes(), smallest: e.smallest });
    e.summary = await ask();
    // The AV1 choice depends on the size of the job, so ask again if it changed.
    e.summary = await ask();
  } catch (error) { fail(error); }
  render();
  measurePacked();
}

/** Compress the items once in the background to learn the real size of the
 * video (spec 4.1). The summary shows "≤" until the answer comes. A newer
 * list of items, or an encode, stops a measurement that is still running. */
async function measurePacked() {
  const e = S.enc;
  const key = itemsKey();
  if (!e.compress || e.items.length === 0 || e.items.some((item) => item.measuring)) return;
  if (e.packed?.key === key) return;
  e.packed = { key, bytes: null };
  render();
  try {
    const bytes = await invoke("measure_packed", { paths: e.items.map((item) => item.path) });
    if (e.packed?.key === key) { e.packed.bytes = bytes; await refreshEncode(); }
  } catch {
    // Stopped by a newer list or an encode, or a file could not be read:
    // keep the upper limit, and measure again when the items change.
    if (e.packed?.key === key) { e.packed = { key, bytes: undefined }; render(); }
  }
}

// Smart delivery: the settings each destination needs, from the real-site
// tests (spec 8.4, 8.5 and 9.6).
const DESTINATIONS = {
  keep: { mode: "lossless", resolution: "1080p", fps: null,
    hint: "Lossless: the video is about the size of your files, or much smaller for documents. Send it as a file or document, never as a compressed video." },
  whatsapp: { mode: "robust", resolution: "1080p", cell: 8, parity: 64, fps: 10,
    hint: "1080p, 8 × 8 blocks, strong repair, 10 frames a second: a low enough bitrate that WhatsApp sends it untouched, and phones can play it." },
  youtube: { mode: "robust", resolution: "4k", cell: 4, parity: 32, fps: null,
    hint: "4K, 4 × 4 blocks, standard repair: made to restore from any copy YouTube makes, its 720p copy included, so there is no waiting for HD." },
  custom: { hint: "Choose every setting yourself below." },
};

// AV1 speed measured on this kind of PC, in frames per second (spec 7.1).
const AV1_FPS = { "1080p-8": 5, "1080p-4": 3.5, "4k-8": 1.4, "4k-4": 1.0 };
// Automatic AV1 for the YouTube destination when it takes at most this long.
const AV1_AUTO_MINUTES = 5;

function av1Minutes() {
  const e = S.enc;
  const frames = Number(String(e.summary?.frames ?? "0").replace(/[^0-9]/g, "")) || 0;
  return frames / (AV1_FPS[`${e.resolution}-${e.cell}`] ?? 1) / 60;
}

function useAv1() {
  const e = S.enc;
  if (e.mode !== "robust" || !S.ffmpeg?.av1) return false;
  if (e.av1Choice !== null && e.av1Choice !== undefined) return e.av1Choice;
  return e.dest === "youtube" && av1Minutes() <= AV1_AUTO_MINUTES;
}

function applyDestination(dest) {
  const { hint, ...settings } = DESTINATIONS[dest] ?? DESTINATIONS.keep;
  S.enc.dest = DESTINATIONS[dest] ? dest : "keep";
  S.enc.av1Choice = null;
  if (S.enc.dest !== "custom") Object.assign(S.enc, settings);
}

function applyDefaults() {
  const s = S.settings;
  Object.assign(S.enc, { resolution: s.resolution, mode: s.mode, cell: s.cell, parity: s.parity, compress: s.compress, smallest: s.smallest, verify: s.verify, fps: null });
  applyDestination(s.destination ?? "keep");
}

async function startEncode() {
  const e = S.enc;
  if ($("create-video").disabled) return;
  const password = $("password").value;
  const request = {
    inputs: e.items.map((item) => item.path),
    output: e.output.path,
    resolution: e.resolution, mode: e.mode, cell: Number(e.cell), parity: Number(e.parity),
    compress: e.compress, smallest: e.smallest, verify: e.verify, encrypt: e.encrypt,
    frame_rate: e.mode === "robust" ? e.fps : null,
    av1: e.mode === "robust" && useAv1(),
    password: e.encrypt && password ? password : null,
    key_file: e.encrypt && e.keyFile ? e.keyFile.path : null,
    input_bytes: e.items.reduce((sum, item) => sum + (item.bytes || 0), 0),
  };
  const meta = [plural(e.items.length, "item"), sizeText(request.input_bytes), e.encrypt ? "Encrypted" : "Not encrypted"].join(" · ");
  const tag = `${e.resolution === "4k" ? "4K" : "1080p"} · ${e.mode === "robust" ? `Robust ${e.cell} × ${e.cell}` : "Lossless"}`;
  S.enc.lastPreview = null;
  beginJob({ encoding: true, name: e.output.name, meta, tag });
  try {
    await invoke("start_encode", { request });
  } catch (error) {
    S.job = null;
    S.enc.result = { error };
    render();
  }
}

// ---------------------------------------------------------------- decode actions

async function openVideo(path, online = null) {
  if (S.job) return;
  S.page = "decode";
  S.dec = { ...S.dec, video: null, probing: true, keyFile: null, unlockError: "", result: null, online };
  $("dec-password").value = "";
  render();
  try {
    const video = await invoke("probe_video", { path });
    // A downloaded file is named by its YouTube id; its title says more.
    if (online) video.name = online.title;
    S.dec.video = video;
    if (video.preview) drawGray($("dec-thumb"), video.preview); else clearCanvas($("dec-thumb"));
  } catch (error) {
    S.dec.result = { error };
  }
  S.dec.probing = false;
  render();
}

async function startDecode() {
  const d = S.dec;
  const v = d.video;
  if (!v || $("restore-files").disabled) return;
  d.unlockError = "";
  const request = {
    video: v.path,
    output_dir: d.out.path,
    password: v.needs_password ? $("dec-password").value : null,
    key_file: v.needs_key_file && d.keyFile ? d.keyFile.path : null,
    resolution: v.resolution,
    mode: v.tag.split(" · ")[1] ?? v.mode,
    robust: v.robust, encrypted: v.encrypted, compressed: v.compressed,
    data_frames: v.data_frames,
  };
  beginJob({ encoding: false, name: v.name, meta: [v.resolution, v.length, v.encrypted ? "Encrypted" : "Not encrypted"].join(" · "), tag: v.tag });
  try {
    await invoke("start_decode", { request });
  } catch (error) {
    S.job = null;
    S.dec.result = { error };
    render();
  }
}

// ------------------------------------------------------------------ YouTube

let ytTimer = null;

/** The link box changed: ask YouTube about it once typing pauses. */
function ytInput(now = false) {
  clearTimeout(ytTimer);
  const text = $("yt-link").value.trim();
  if (text === S.yt.text && !now) return;
  Object.assign(S.yt, { text, info: null, error: null, playing: false, checking: Boolean(text) });
  render();
  if (text) ytTimer = setTimeout(() => ytCheck(text), now ? 0 : 600);
}

async function ytCheck(text) {
  let info = null, error = null;
  try { info = await invoke("youtube_details", { link: text }); } catch (problem) { error = problem; }
  if (S.yt.text !== text) return; // the box changed while YouTube answered
  Object.assign(S.yt, { info, error, checking: false });
  render();
}

async function ytDownload() {
  const v = S.yt.info;
  if (!v || S.job || S.yt.downloading) return;
  Object.assign(S.yt, { downloading: true, playing: false, error: null, progress: { fraction: null, text: "Asking YouTube for the best copy…" } });
  render();
  try {
    await invoke("start_youtube_download", { link: v.url });
  } catch (error) {
    Object.assign(S.yt, { downloading: false, progress: null, error });
    render();
  }
}

async function ytFinished(finished) {
  const online = S.yt.info;
  Object.assign(S.yt, { downloading: false, progress: null });
  if (finished.outcome === "done") {
    // Format 2's standard repair survives YouTube's 1080p and 4K copies, not
    // its smaller ones (spec 8.7): wait for the HD copy instead of failing.
    // Format 3 (LDPC) restores from the 720p copy too.
    try {
      const video = await invoke("probe_video", { path: finished.path });
      if (video.robust && video.version < 3 && video.parity < 64 && video.copy_height < 1080) {
        S.yt.error = {
          code: "wait-hd",
          title: "YouTube is still making the HD copy",
          text: `YouTube only offers ${video.copy_height}p of this video so far. It was made with standard repair, which needs the 1080p or 4K copy. Try again in a while; YouTube usually finishes within an hour.`,
          detail: "",
        };
        render();
        return;
      }
    } catch (error) { /* openVideo below reports any problem */ }
    await openVideo(finished.path, online);
    return;
  }
  if (finished.outcome === "failed") S.yt.error = finished.error;
  render();
}

// ------------------------------------------------------------------ settings

async function saveSettings(change) {
  Object.assign(S.settings, change);
  try {
    S.settings = await invoke("save_settings", { input: {
      appearance: S.settings.appearance, material: S.settings.material,
      resolution: S.settings.resolution, mode: S.settings.mode,
      cell: Number(S.settings.cell), parity: Number(S.settings.parity), compress: S.settings.compress, smallest: S.settings.smallest, destination: S.settings.destination ?? "keep", verify: S.settings.verify,
    } });
  } catch (error) { fail(error); }
  if ("appearance" in change || "material" in change) {
    applyTheme();
    invoke("set_appearance", { appearance: S.settings.appearance, material: S.settings.material }).catch(fail);
  }
  // New defaults apply straight away while nothing has been added yet.
  if (S.enc.items.length === 0 && !S.job) { applyDefaults(); await refreshEncode(); }
  render();
}

async function checkFfmpeg() {
  S.ffmpeg = null;
  render();
  S.ffmpeg = await invoke("check_ffmpeg").catch((error) => ({ found: false, version: "", path: "", problem: String(error), saved: false }));
  render();
}

// ------------------------------------------------------------------- actions

const ACTIONS = {
  "pick-files": async () => addInputs(await invoke("pick_inputs", { folder: false })),
  "pick-folder": async () => addInputs(await invoke("pick_inputs", { folder: true })),
  "remove-item": async (button) => { S.enc.items.splice(Number(button.dataset.index), 1); await refreshEncode(); },
  "toggle-password": () => {
    const shown = $("password").type === "text";
    for (const id of ["password", "confirm"]) $(id).type = shown ? "password" : "text";
    $("pw-eye").innerHTML = icon(shown ? "eye" : "eye-off", 15, 1.3);
    $("pw-eye").title = shown ? "Show password" : "Hide password";
  },
  "create-key": async () => {
    try {
      const key = await invoke("create_key_file");
      if (key) { S.enc.keyFile = key; S.enc.keyError = ""; }
    } catch (error) { S.enc.keyError = String(error); }
    render();
  },
  "choose-key": async () => {
    try {
      const key = await invoke("choose_key_file");
      if (key) { S.enc.keyFile = key; S.enc.keyError = ""; }
    } catch (error) { S.enc.keyError = String(error); }
    render();
  },
  "clear-key": () => { S.enc.keyFile = null; S.enc.keyError = ""; render(); },
  "change-output": async () => {
    const chosen = await invoke("choose_output_file", { current: S.enc.output.path, mode: S.enc.mode });
    if (chosen) { S.enc.chosenOutput = chosen.path; await refreshEncode(); }
  },
  "start-encode": startEncode,
  "pick-dest": async (button) => {
    applyDestination(button.dataset.dest);
    await saveSettings({ destination: S.enc.dest });
    await refreshEncode();
  },
  "reveal-video": () => S.enc.result?.done && invoke("reveal", { path: S.enc.result.done.path }),
  "encode-another": async () => {
    Object.assign(S.enc, { items: [], chosenOutput: null, result: null, lastPreview: null });
    resetPasswordFields();
    applyDefaults();
    await refreshEncode();
  },
  "pick-video": async () => { const path = await invoke("pick_video"); if (path) await openVideo(path); },
  "dec-choose-key": async () => {
    try {
      const key = await invoke("choose_key_file");
      if (key) { S.dec.keyFile = key; S.dec.unlockError = ""; }
    } catch (error) { S.dec.unlockError = String(error); }
    render();
  },
  "dec-clear-key": () => { S.dec.keyFile = null; render(); },
  "dec-change-output": async () => {
    const folder = await invoke("choose_restore_folder", { current: S.dec.out.path });
    if (folder) { S.dec.out = folder; render(); }
  },
  "start-decode": startDecode,
  "yt-play": () => { S.yt.playing = true; render(); },
  "yt-watch": () => S.yt.info && invoke("open_youtube", { link: S.yt.info.url }),
  "yt-download": ytDownload,
  "yt-cancel": () => invoke("cancel_job"),
  "tool-install": (button) => installTool(button.dataset.tool),
  "wz-next": () => {
    const w = S.wizard;
    w.step = WIZARD_STEPS[Math.min(WIZARD_STEPS.indexOf(w.step) + 1, WIZARD_STEPS.length - 1)];
    render();
    if (w.step === "tools") loadTools("no");
  },
  "wz-back": () => { S.wizard.step = "welcome"; render(); },
  "wz-skip": () => finishWizard(),
  "wz-finish": () => finishWizard(),
  "wz-youtube": () => { if (!S.wizard.installing) { S.wizard.youtube = !S.wizard.youtube; render(); } },
  "wz-install": () => {
    const w = S.wizard;
    w.queue = wizardNeeded();
    w.installing = w.queue.length > 0;
    for (const key of w.queue) delete S.toolNotes[key];
    wizardInstallNext();
  },
  "wz-again": () => { if (!S.job && !S.toolJob) { S.wizard = newWizard("tools"); render(); } },
  "tool-cancel": () => invoke("cancel_job"),
  "tools-check": () => loadTools("now"),
  "reveal-restored": () => S.dec.result?.done && invoke("reveal", { path: S.dec.result.done.reveal }),
  "decode-another": () => {
    Object.assign(S.dec, { video: null, keyFile: null, unlockError: "", result: null });
    $("dec-password").value = "";
    render();
  },
  "err-first": async () => {
    if (S.page === "encode") { S.enc.result = null; render(); await startEncode(); }
    else { ACTIONS["decode-another"](); await ACTIONS["pick-video"](); }
  },
  "err-second": () => {
    if (S.page === "encode") S.enc.result = null;
    else S.dec.result = null;
    render();
  },
  "ask-cancel": () => { S.dialog = "cancel"; openDialog(); },
  "go-to-job": () => { if (S.job) { S.page = S.job.encoding ? "encode" : "decode"; render(); } },
  "ask-clear-history": () => { if (S.history.length) { S.dialog = "clear"; openDialog(); } },
  "history-reveal": (button) => invoke("reveal_history", { index: Number(button.dataset.index) }),
  "history-remove": async (button) => { S.history = await invoke("history_remove", { index: Number(button.dataset.index) }); render(); },
  "dialog-keep": () => closeDialog(),
  "dialog-go": async () => {
    const kind = S.dialog;
    closeDialog();
    if (kind === "cancel") await invoke("cancel_job");
    if (kind === "clear") { S.history = await invoke("history_clear"); render(); }
  },
  "choose-output-folder": async () => {
    const view = await invoke("choose_output_folder");
    if (view) { S.settings = view; if (!S.dec.out || !S.dec.video) S.dec.out = { path: view.output_folder, friendly: view.output_folder_friendly }; await refreshEncode(); }
  },
  "check-ffmpeg": checkFfmpeg,
  "choose-ffmpeg": async () => { S.ffmpeg = await invoke("choose_ffmpeg"); render(); },
  "clear-ffmpeg": async () => { S.ffmpeg = null; render(); S.ffmpeg = await invoke("clear_ffmpeg_path"); render(); },
  "open-ffmpeg-site": () => invoke("open_ffmpeg_site"),
  "open-author": () => invoke("open_author_page"),
  "win-minimize": () => appWindow.minimize(),
  "win-maximize": () => appWindow.toggleMaximize(),
  "win-close": () => appWindow.close(),
};

const DIALOGS = {
  cancel: () => S.job?.encoding
    ? ["Stop creating the video?", "Progress so far will be lost and the partly written video deleted. Your original files are not changed.", "Keep Going", "Stop"]
    : ["Stop restoring files?", "Files restored so far will be removed. The video is not changed.", "Keep Going", "Stop"],
  clear: () => ["Clear history?", "This removes the list only. Videos and restored files stay where they are.", "Cancel", "Clear History"],
};

let focusBeforeDialog = null;
function openDialog() {
  const [title, body, keep, go] = DIALOGS[S.dialog]();
  $("dialog-title").textContent = title;
  $("dialog-body").textContent = body;
  $("dialog-keep").textContent = keep;
  $("dialog-go").textContent = go;
  focusBeforeDialog = document.activeElement;
  render();
  // The safe choice has focus (design: Confirm cancel).
  $("dialog-keep").focus();
}
function closeDialog() {
  S.dialog = null;
  render();
  focusBeforeDialog?.focus?.();
}

// -------------------------------------------------------------------- wiring

function wire() {
  document.addEventListener("click", async (event) => {
    const target = event.target.closest("[data-action], [data-page], [data-seg] button, [data-switch]");
    if (!target || target.disabled) return;
    try {
      if (target.dataset.page) {
        S.page = target.dataset.page;
        render();
      } else if (target.dataset.action) {
        await ACTIONS[target.dataset.action]?.(target);
      } else if (target.closest("[data-seg]")) {
        await pickSegment(target.closest("[data-seg]").dataset.seg, target.dataset.value);
      } else if (target.dataset.switch) {
        await flipSwitch(target.dataset.switch);
      }
    } catch (error) { fail(error); }
  });

  // Arrow keys move through a segmented control, like a radio group.
  document.addEventListener("keydown", async (event) => {
    if (S.dialog) {
      if (event.key === "Escape") { event.preventDefault(); closeDialog(); }
      if (event.key === "Tab") {
        // Keep focus inside the dialog.
        const buttons = [$("dialog-keep"), $("dialog-go")];
        event.preventDefault();
        (document.activeElement === buttons[0] ? buttons[1] : buttons[0]).focus();
      }
      return;
    }
    const button = event.target.closest?.("[data-seg] button");
    if (button && ["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(event.key)) {
      event.preventDefault();
      const buttons = all("button", button.parentElement);
      const step = event.key === "ArrowLeft" || event.key === "ArrowUp" ? -1 : 1;
      const next = buttons[(buttons.indexOf(button) + step + buttons.length) % buttons.length];
      next.focus();
      await pickSegment(button.parentElement.dataset.seg, next.dataset.value);
    }
  });

  $("password").addEventListener("input", () => { render(); checkStrength(); });
  $("confirm").addEventListener("input", () => render());
  $("password").addEventListener("keydown", (event) => { if (event.key === "Enter") $("confirm").focus(); });
  $("confirm").addEventListener("keydown", (event) => { if (event.key === "Enter") startEncode(); });
  $("dec-password").addEventListener("input", () => { S.dec.unlockError = ""; render(); });
  $("dec-password").addEventListener("keydown", (event) => { if (event.key === "Enter") startDecode(); });
  $("yt-link").addEventListener("input", () => ytInput());
  $("yt-link").addEventListener("paste", () => setTimeout(() => ytInput(true)));
  $("yt-link").addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    if (S.yt.info && !S.yt.checking) ytDownload(); else ytInput(true);
  });

  media.dark.addEventListener("change", () => { applyTheme(); render(); });
  media.lessTransparency.addEventListener("change", () => applyTheme());

  listen("job-update", (event) => { if (S.job) { S.job.view = event.payload; updateRunning(event.payload); } });
  listen("job-finished", (event) => jobFinished(event.payload));
  listen("download-update", (event) => { if (S.yt.downloading) { S.yt.progress = event.payload; renderDownloadProgress(); } });
  listen("download-finished", (event) => ytFinished(event.payload));
  listen("tool-progress", (event) => { if (S.toolJob?.key === event.payload.key) { Object.assign(S.toolJob, event.payload); renderToolProgress(); } });
  listen("tool-finished", (event) => toolFinished(event.payload));
  $("yt-fix").addEventListener("click", async () => {
    const fix = $("yt-fix").dataset.fix;
    if (fix === "install") await installTool("ytdlp");
    else if (fix === "check") { S.page = "settings"; await loadTools("now"); }
  });
  listen("history-changed", (event) => { S.history = event.payload; if (S.page === "history") render(); });

  // Drag and drop from the operating system (spec 9.4): Tauri's own webview events.
  tauri.webview.getCurrentWebview().onDragDropEvent(async (event) => {
    const { type, paths } = event.payload;
    const accepts = S.ffmpeg?.found && !S.job && !S.yt.downloading;
    if (type === "enter") {
      S.drag = accepts ? (paths?.length ?? 1) : null;
      if (accepts && S.page !== "decode" && S.page !== "encode") S.page = "encode";
      render();
    } else if (type === "leave") {
      S.drag = null; render();
    } else if (type === "drop") {
      S.drag = null; render();
      if (!accepts || !paths?.length) return;
      if (S.page === "decode") await openVideo(paths[0]);
      else { S.page = "encode"; await addInputs(paths); }
    }
  });
}

async function pickSegment(name, value) {
  const number = (text) => Number(text);
  switch (name) {
    // Changing a detail by hand makes the choice "Custom".
    case "resolution": S.enc.resolution = value; S.enc.dest = "custom"; S.enc.fps = null; break;
    case "mode": S.enc.mode = value; S.enc.dest = "custom"; S.enc.fps = null; break;
    case "cell": S.enc.cell = number(value); S.enc.dest = "custom"; S.enc.fps = null; break;
    case "parity": S.enc.parity = number(value); S.enc.dest = "custom"; S.enc.fps = null; break;
    case "appearance": return saveSettings({ appearance: value });
    case "material": return saveSettings({ material: value });
    case "default-resolution": return saveSettings({ resolution: value });
    case "default-mode": return saveSettings({ mode: value });
    case "default-cell": return saveSettings({ cell: number(value) });
    case "default-parity": return saveSettings({ parity: number(value) });
  }
  await refreshEncode();
}

async function flipSwitch(name) {
  switch (name) {
    case "encrypt": S.enc.encrypt = !S.enc.encrypt; break;
    case "compress": S.enc.compress = !S.enc.compress; break;
    case "av1": S.enc.av1Choice = !useAv1(); break;
    case "smallest": S.enc.smallest = !S.enc.smallest; break;
    case "verify": S.enc.verify = !S.enc.verify; break;
    case "default-compress": return saveSettings({ compress: !S.settings.compress });
    case "default-smallest": return saveSettings({ smallest: !S.settings.smallest });
    case "default-verify": return saveSettings({ verify: !S.settings.verify });
  }
  await refreshEncode();
}

// -------------------------------------------------------------------- start

async function main() {
  fillIcons();
  wire();
  const startup = await invoke("startup");
  S.os = startup.os;
  S.look = startup.look;
  S.version = startup.version;
  S.revealLabel = startup.reveal_label;
  S.settings = startup.settings;
  S.history = startup.history;
  if (!startup.settings.setup_done) S.wizard = newWizard();
  all(".reveal-label").forEach((button) => { button.textContent = startup.reveal_label; });
  $("install-hint").textContent = startup.install_hint;
  $("install-command").textContent = startup.install_command;
  $("ff-install-hint").textContent = startup.install_hint;
  $("ff-install-command").textContent = startup.install_command;
  show($("caption"), S.os === "windows");
  S.dec.out = { path: startup.settings.output_folder, friendly: startup.settings.output_folder_friendly };
  applyDefaults();
  applyTheme();
  await refreshEncode();
  render();
  await invoke("page_ready");
  await checkFfmpeg();
  // What is installed, then (at most once a day) what is new.
  await loadTools("no");
  await loadTools("auto");
}

main().catch((error) => {
  document.body.textContent = `ATV could not start: ${error}`;
  invoke("page_ready").catch(() => {});
});
