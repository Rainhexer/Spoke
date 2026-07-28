// First-run onboarding wizard for the standalone "onboard" window.
// Talks to the same Tauri commands as the bubble UI; on completion it calls
// `finish_onboarding`, which flips the persisted `onboarded` flag and reveals
// the bubble (see lib.rs).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const $ = (id) => document.getElementById(id);
const body = $("ob-body");
const dots = $("ob-dots");
const backBtn = $("ob-back");
const nextBtn = $("ob-next");

let config = null;
let buildInfo = null;
let perms = { microphone: "unknown", accessibility: "unknown" };
let installed = new Set();
let steps = [];
let idx = 0;

// Model catalogue mirrors the bubble UI's engine card.
const MODELS = [
  { value: "tiny", label: "tiny", note: "74 MB" },
  { value: "base", label: "base", note: "141 MB" },
  { value: "small", label: "small", note: "465 MB" },
  { value: "medium", label: "medium", note: "1.4 GB" },
  { value: "large-v3-turbo", label: "turbo", note: "1.5 GB" },
  { value: "large-v3", label: "large", note: "2.9 GB" },
];

let modelDownloading = false;
let permTimer = null;
// Live nodes of the engine step's download row, valid only while it is rendered.
let modelStatusEl = null;
let modelBtnEl = null;

// ---- helpers --------------------------------------------------------------

function el(tag, cls, html) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (html != null) n.innerHTML = html;
  return n;
}

async function saveConfig() {
  try {
    await invoke("set_config", { newConfig: config });
  } catch (e) {
    console.error("save config failed", e);
  }
}

function prettyHotkey(hk) {
  return (hk || "")
    .split("+")
    .map((p) => {
      const t = p.trim().toLowerCase();
      if (t === "cmd") return "Cmd";
      if (t === "ctrl") return "Ctrl";
      if (t === "alt") return "Alt";
      if (t === "shift") return "Shift";
      return t.charAt(0).toUpperCase() + t.slice(1);
    })
    .join(" + ");
}

// ---- step: welcome --------------------------------------------------------

function renderWelcome(root) {
  const hk = prettyHotkey(config.general.hotkey);
  root.appendChild(el("h1", "ob-title", "Welcome to Spoke"));
  root.appendChild(
    el(
      "p",
      "ob-lede",
      "Push-to-talk dictation for your whole desktop. Hold a hotkey, speak, and your words are typed into whatever window you're in."
    )
  );

  const hiw = el("div", "hiw");
  const rows = [
    ["Hold", `Press and hold <span class="kbd">${hk}</span> anywhere.`],
    ["Speak", "Talk while you hold it — the bubble listens."],
    ["Release", "Let go; your speech is transcribed and typed for you."],
  ];
  rows.forEach(([h, p], i) => {
    const row = el("div", "hiw-row");
    row.appendChild(el("div", "hiw-num", String(i + 1)));
    row.appendChild(el("div", "hiw-txt", `<b>${h}.</b> <span>${p}</span>`));
    hiw.appendChild(row);
  });
  root.appendChild(hiw);
  root.appendChild(
    el(
      "p",
      "ob-lede",
      "Let's set a few things up. It only takes a minute — you can change all of it later from the bubble."
    )
  );
}

// ---- step: startup --------------------------------------------------------

function renderStartup(root) {
  root.appendChild(el("h1", "ob-title", "How should Spoke start?"));
  root.appendChild(
    el(
      "p",
      "ob-lede",
      "Spoke runs quietly in the background. Pick how it appears when you launch it — the hotkey works either way, and you can switch at any time."
    )
  );

  const grid = el("div", "opt-grid");
  const opts = [
    [
      false,
      "◍",
      "Show the bubble",
      "A small floating bubble sits in the corner of your screen. Click it for settings and history.",
    ],
    [
      true,
      "▾",
      "Start in the tray",
      "No bubble on screen — Spoke lives in the system tray/menu bar. Settings and history are in the tray menu, and “Show Spoke” brings the bubble back whenever you want it.",
    ],
  ];
  opts.forEach(([val, ico, h, p]) => {
    const o = el("button", "opt");
    o.type = "button";
    if (!!config.ui.start_minimized === val) o.classList.add("sel");
    o.innerHTML = `
      <span class="opt-ico">${ico}</span>
      <span class="opt-body">
        <span class="opt-h">${h}</span>
        <span class="opt-p">${p}</span>
      </span>
      <span class="opt-check">●</span>`;
    o.addEventListener("click", () => {
      config.ui.start_minimized = val;
      saveConfig();
      grid.querySelectorAll(".opt").forEach((n) => n.classList.remove("sel"));
      o.classList.add("sel");
    });
    grid.appendChild(o);
  });
  root.appendChild(grid);

  // Wayland itself can't place or pin a floating window, so the bubble runs
  // through XWayland instead. That is stable now, but WebKitGTK's transparent
  // window buffer forces a flatter look — worth saying once, up front, rather
  // than leaving people to wonder why Linux doesn't match the screenshots.
  if (buildInfo && buildInfo.wayland) {
    root.appendChild(
      el(
        "div",
        "ob-note",
        "<b>Wayland session.</b>The bubble runs through XWayland so it can hold its position and stay on top. WebKitGTK never clears a transparent window's buffer, which can cause ghosting. So the Linux bubble disables drop shadows and glow to keep a fixed outline. It is flatter than on macOS or Windows, but has no ghosting. Tray mode is recommended if you'd rather not deal with visual glitches."
      )
    );
  }
}

// ---- step: permissions (macOS) --------------------------------------------

const PERM_META = {
  microphone: {
    ico: "🎙",
    h: "Microphone",
    p: "Required to record your voice. macOS will ask once.",
  },
  accessibility: {
    ico: "⌨",
    h: "Accessibility",
    p: "Lets Spoke type the transcript into other apps.",
  },
};

function permStateLabel(s) {
  if (s === "granted") return ["Granted", "granted"];
  if (s === "denied") return ["Grant", "denied"];
  if (s === "undetermined") return ["Allow", "denied"];
  return ["—", ""];
}

function renderPermissions(root) {
  root.appendChild(el("h1", "ob-title", "Grant access"));
  root.appendChild(
    el(
      "p",
      "ob-lede",
      "Spoke needs two macOS permissions to hear you and type for you. Nothing leaves your machine in offline mode."
    )
  );

  ["microphone", "accessibility"].forEach((which) => {
    const meta = PERM_META[which];
    const row = el("div", "perm");
    row.innerHTML = `
      <span class="perm-ico">${meta.ico}</span>
      <span class="perm-main">
        <div class="perm-h">${meta.h}</div>
        <div class="perm-p">${meta.p}</div>
      </span>`;
    const btn = el("button", "perm-state");
    btn.type = "button";
    btn.dataset.which = which;
    row.appendChild(btn);
    btn.addEventListener("click", () => grantPermission(which));
    root.appendChild(row);
    paintPermButton(btn, perms[which]);
  });

  // Live-refresh while the step is visible (grants made in System Settings).
  clearInterval(permTimer);
  permTimer = setInterval(refreshPerms, 1200);
}

function paintPermButton(btn, state) {
  const [label, cls] = permStateLabel(state);
  btn.textContent = label;
  btn.className = "perm-state" + (cls ? " " + cls : "");
  btn.disabled = state === "granted" || state === "unknown";
}

async function refreshPerms() {
  try {
    perms = await invoke("check_permissions");
  } catch (_) {
    return;
  }
  body.querySelectorAll(".perm-state").forEach((btn) => {
    paintPermButton(btn, perms[btn.dataset.which]);
  });
}

async function grantPermission(which) {
  if (which === "microphone") {
    await invoke("request_microphone_permission");
  } else {
    await invoke("request_accessibility_permission");
    // Accessibility grants land asynchronously via System Settings.
    invoke("open_permission_settings", { which });
  }
  refreshPerms();
}

// ---- step: engine ---------------------------------------------------------

function renderEngine(root) {
  const hasWhisper = buildInfo && buildInfo.whisper;
  root.appendChild(el("h1", "ob-title", "Choose an engine"));
  root.appendChild(
    el(
      "p",
      "ob-lede",
      hasWhisper
        ? "Offline runs a local whisper model — private and free, no internet. Online sends audio to Google for the fastest, most accurate results."
        : "This build transcribes online via Google. Paste an API key to get started."
    )
  );

  if (hasWhisper) {
    root.appendChild(el("div", "ob-section-label", "Mode"));
    const modeChips = el("div", "chips");
    [
      ["offline", "Offline"],
      ["online", "Online"],
    ].forEach(([val, label]) => {
      const c = el("button", "chip" + (config.general.mode === val ? " sel" : ""));
      c.type = "button";
      c.textContent = label;
      c.addEventListener("click", async () => {
        config.general.mode = val;
        await saveConfig();
        renderStep(); // re-render for the mode's sub-controls
      });
      modeChips.appendChild(c);
    });
    root.appendChild(modeChips);
  } else if (config.general.mode !== "online") {
    config.general.mode = "online";
    saveConfig();
  }

  if (config.general.mode === "online") {
    root.appendChild(el("div", "ob-section-label", "Google API key"));
    const inp = el("input", "ob-input");
    inp.type = "password";
    inp.spellcheck = false;
    inp.placeholder = "AIza…";
    inp.value = config.online.api_key || "";
    inp.addEventListener("change", () => {
      config.online.api_key = inp.value.trim();
      saveConfig();
    });
    root.appendChild(inp);
    return;
  }

  // Offline: model picker + download.
  root.appendChild(el("div", "ob-section-label", "Model"));
  const grid = el("div", "chips");
  MODELS.forEach((m) => {
    const c = el("button", "chip" + (config.offline.model === m.value ? " sel" : ""));
    c.type = "button";
    c.dataset.model = m.value;
    if (installed.has(m.value)) c.classList.add("installed");
    c.innerHTML = `<span>${m.label}</span><span class="note">${m.note}</span>`;
    c.addEventListener("click", async () => {
      if (modelDownloading) return;
      config.offline.model = m.value;
      await saveConfig();
      renderStep();
    });
    grid.appendChild(c);
  });
  root.appendChild(grid);

  // Keep node references rather than looking them up by id: this subtree is
  // still detached (renderStep appends it only after the renderer returns), so
  // getElementById would come back null and blow up mid-render.
  const dl = el("div", "dl");
  dl.innerHTML = `
    <span class="dl-status">…</span>
    <button type="button" class="mini-btn hide">Download</button>`;
  modelStatusEl = dl.querySelector(".dl-status");
  modelBtnEl = dl.querySelector(".mini-btn");
  root.appendChild(dl);
  modelBtnEl.addEventListener("click", startDownload);
  refreshModelStatus();

  // Acceleration backend — only when the build compiled more than the CPU
  // fallback (e.g. a CUDA or Vulkan build). "auto" picks the best available.
  const backends = (buildInfo && buildInfo.backends) || [];
  if (backends.length > 1) {
    root.appendChild(el("div", "ob-section-label", "Acceleration"));
    const opts = [{ value: "auto", label: "Auto", note: buildInfo.acceleration }].concat(
      backends.map((b) => ({ value: b.id, label: b.badge, note: null }))
    );
    const saved = config.offline.accel || "auto";
    const selected =
      saved === "auto" || backends.some((b) => b.id === saved) ? saved : "auto";
    const accelChips = el("div", "chips");
    opts.forEach((o) => {
      const c = el("button", "chip" + (selected === o.value ? " sel" : ""));
      c.type = "button";
      c.innerHTML = `<span>${o.label}</span>${o.note ? `<span class="note">${o.note}</span>` : ""}`;
      c.addEventListener("click", async () => {
        config.offline.accel = o.value;
        await saveConfig();
        renderStep();
      });
      accelChips.appendChild(c);
    });
    root.appendChild(accelChips);
  }
}

async function refreshModelStatus() {
  const model = config.offline.model;
  const status = modelStatusEl;
  const btn = modelBtnEl;
  if (!status) return;
  status.textContent = "…";
  status.className = "dl-status";
  try {
    const info = await invoke("check_model", { model });
    if (model !== config.offline.model) return;
    if (info.exists) {
      installed.add(model);
      status.textContent = "✓ installed — ready to go";
      status.className = "dl-status installed";
      btn && btn.classList.add("hide");
    } else {
      status.textContent = "not downloaded yet";
      status.className = "dl-status";
      btn && btn.classList.remove("hide");
    }
  } catch (_) {
    status.textContent = "—";
  }
}

async function startDownload() {
  if (modelDownloading) return;
  const model = config.offline.model;
  modelDownloading = true;
  const status = modelStatusEl;
  const btn = modelBtnEl;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "…";
  }
  if (status) {
    status.textContent = "0%";
    status.className = "dl-status downloading";
  }
  try {
    await invoke("download_model", { model });
  } catch (e) {
    modelDownloading = false;
    if (status) {
      status.textContent = "✗ failed — you can retry later";
      status.className = "dl-status error";
    }
    if (btn) {
      btn.disabled = false;
      btn.textContent = "Download";
    }
    console.error(e);
  }
}

listen("spoke:download-progress", (e) => {
  if (!modelDownloading) return;
  const { model, percent } = e.payload;
  if (model !== config.offline.model) return;
  const status = modelStatusEl;
  if (status) {
    status.textContent = `${percent}%`;
    status.className = "dl-status downloading";
  }
});

listen("spoke:download-complete", (e) => {
  if (!modelDownloading) return;
  modelDownloading = false;
  const { model } = e.payload;
  installed.add(model);
  if (model === config.offline.model) {
    const status = modelStatusEl;
    const btn = modelBtnEl;
    if (status) {
      status.textContent = "✓ installed — ready to go";
      status.className = "dl-status installed";
    }
    btn && btn.classList.add("hide");
    // Light up the chip's install mark.
    const chip = body.querySelector(`.chip[data-model="${model}"]`);
    chip && chip.classList.add("installed");
  }
});

// ---- step: done -----------------------------------------------------------

function renderDone(root) {
  root.appendChild(el("div", "done-mark", "✓"));
  root.appendChild(el("h1", "ob-title", "You're all set"));
  root.appendChild(
    el(
      "p",
      "ob-lede",
      "Hold your hotkey anywhere to dictate. Everything here can be changed later from the bubble or tray."
    )
  );

  const online = config.general.mode === "online";
  const engineVal = online
    ? "Online (Google)"
    : `Offline · ${MODELS.find((m) => m.value === config.offline.model)?.label || config.offline.model}`;

  const rows = [
    ["Hotkey", prettyHotkey(config.general.hotkey)],
    ["Engine", engineVal],
    ["On launch", config.ui.start_minimized ? "Tray only" : "Show bubble"],
  ];
  const ul = el("ul", "summary");
  rows.forEach(([k, v]) => {
    const li = el("li");
    li.appendChild(el("span", "k", k));
    li.appendChild(el("span", "v", v));
    ul.appendChild(li);
  });
  root.appendChild(ul);
}

// ---- wizard shell ---------------------------------------------------------

const RENDERERS = {
  welcome: renderWelcome,
  startup: renderStartup,
  permissions: renderPermissions,
  engine: renderEngine,
  done: renderDone,
};

function renderStep() {
  const name = steps[idx];
  if (name !== "permissions") clearInterval(permTimer);
  modelStatusEl = null;
  modelBtnEl = null;

  body.innerHTML = "";
  const wrap = el("div", "ob-step");
  try {
    RENDERERS[name](wrap);
  } catch (e) {
    // Never leave the user staring at a blank card if a step fails to build.
    console.error("render step failed", name, e);
    wrap.appendChild(el("h1", "ob-title", "Something went wrong"));
    wrap.appendChild(
      el("p", "ob-lede", "This step couldn't be shown. You can continue and set it up later from the bubble.")
    );
  }
  body.appendChild(wrap);
  body.scrollTop = 0;

  // Dots.
  dots.innerHTML = "";
  steps.forEach((_, i) => {
    const d = el("span", "dot" + (i === idx ? " active" : i < idx ? " done" : ""));
    dots.appendChild(d);
  });

  // Nav.
  backBtn.classList.toggle("invisible", idx === 0);
  nextBtn.textContent = idx === steps.length - 1 ? "Start Spoke" : "Next";
}

backBtn.addEventListener("click", () => {
  if (idx > 0) {
    idx--;
    renderStep();
  }
});

nextBtn.addEventListener("click", () => {
  if (idx < steps.length - 1) {
    idx++;
    renderStep();
  } else {
    finish();
  }
});

$("ob-close").addEventListener("click", finish);

let finishing = false;
async function finish() {
  if (finishing) return;
  finishing = true;
  clearInterval(permTimer);
  try {
    await invoke("finish_onboarding");
  } catch (e) {
    console.error("finish_onboarding failed", e);
    // Fall back to just closing so the user isn't trapped.
    try {
      await appWindow.close();
    } catch (_) {}
  }
}

// ---- boot -----------------------------------------------------------------

async function boot() {
  config = await invoke("get_config");
  Sfx.fromConfig(config);
  buildInfo = await invoke("get_build_info");
  try {
    perms = await invoke("check_permissions");
  } catch (_) {}
  try {
    installed = new Set(await invoke("check_models"));
  } catch (_) {}

  steps = ["welcome", "startup"];
  // Only show the permissions step where the OS actually has a queryable model
  // (macOS). Elsewhere check_permissions reports "unknown" and there's nothing
  // to grant from here.
  if (perms.microphone !== "unknown" || perms.accessibility !== "unknown") {
    steps.push("permissions");
  }
  steps.push("engine", "done");

  renderStep();

  // The window is built hidden (an unpainted transparent window shows as a
  // black rectangle for the first frames). Reveal it only once the first step
  // is laid out and painted — two frames is enough for WebKit to present it.
  requestAnimationFrame(() =>
    requestAnimationFrame(() => {
      invoke("show_onboarding").catch((e) => console.error("show failed", e));
      // Welcome chime, once the card is on screen and the clip is decoded.
      Sfx.ready.then(() => Sfx.play("onboarding"));
    })
  );
}

boot().catch((e) => {
  console.error("onboarding boot failed", e);
  invoke("show_onboarding").catch(() => {});
});
