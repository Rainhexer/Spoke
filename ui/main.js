const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const $ = (id) => document.getElementById(id);

const root = $("root");
const bubble = $("bubble");
const ring = $("ring");
const orbit = $("orbit");
const subcard = $("subcard");
const toast = $("toast");
const minimize = $("minimize");

// True while hidden to the tray. When set, state changes push a colored tray
// icon; cleared by the `spoke:restored` event when the window comes back.
let minimized = false;

let config = null;
let recording = false;
let modelDownloading = false;
let coremlDownloading = false;
let buildInfo = null;

let history = [];
const MAX_HISTORY = 50;

// ---- Warnings (permissions + missing model) -------------------------------
// No floating bar. Warnings surface two ways: the main bubble pulses amber
// (the mosaic "warning" animation) whenever anything is wrong, and the related
// category bubble gets an amber "!" badge. Opening that category's card shows a
// yellow bar with a Grant/Download action.

let permissions = null;
let modelMissing = false;      // offline model for current selection not on disk
let lastWarnSig = "";

const PERM_INFO = {
  microphone: { label: "Microphone access denied", hint: "Spoke can't hear you" },
  accessibility: { label: "Accessibility not granted", hint: "Spoke can't type for you" },
};

// Category bubbles that can carry a warning badge → the condition that lights it.
const WARN_CATS = ["engine", "output", "mic"];

function missingPermissions() {
  if (!permissions) return [];
  const missing = [];
  if (permissions.microphone !== "granted" && permissions.microphone !== "unknown") missing.push("microphone");
  // Accessibility only matters for keystroke injection; pure clipboard mode
  // doesn't use it, so don't nag about it there.
  const injecting = !!(config && config.general && config.general.output_dest !== "copy");
  if (injecting && permissions.accessibility === "denied") missing.push("accessibility");
  return missing;
}

// True when offline mode is selected but its model isn't downloaded.
function modelWarn() {
  return !!(config && config.general.mode === "offline" && modelMissing);
}

// Recompute the aggregate warning state: drive the bubble's amber pulse, light
// the per-category badges, and refresh an open card so its warning bar tracks.
function updateWarnings() {
  const missing = missingPermissions();
  const mWarn = modelWarn();
  warnActive = missing.length > 0 || mWarn;
  updateTray();

  const lit = {
    mic: missing.includes("microphone"),
    output: missing.includes("accessibility"),
    engine: mWarn,
  };
  for (const id of WARN_CATS) {
    const el = $(`obw-${id}`);
    if (el) el.classList.toggle("hide", !lit[id]);
  }

  const sig = missing.join(",") + "|" + mWarn;
  if (sig !== lastWarnSig) {
    lastWarnSig = sig;
    // Refresh an open card so its warning bar appears/clears in place.
    if (menuState !== "closed" && menuState !== "ring") rerenderCard();
    presentFrame();
  }
}

let fastPollTimer = null;
// After the user takes a grant action, watch for the change at a 1.5s cadence
// (the baseline poll is 15s) so the warning clears the moment the OS registers
// the grant instead of looking ignored. Stops itself once nothing is missing.
function watchForGrant() {
  clearInterval(fastPollTimer);
  const deadline = Date.now() + 90000;
  fastPollTimer = setInterval(() => {
    if (Date.now() > deadline) {
      clearInterval(fastPollTimer);
      return;
    }
    checkPermissions();
  }, 1500);
}

async function checkPermissions() {
  const prev = permissions;
  try {
    permissions = await invoke("check_permissions");
  } catch (_) {
    permissions = null;
  }
  // Confirm grants the moment they register, so the user knows their trip
  // through Settings (or the native prompt) actually took effect.
  if (prev && permissions) {
    if (prev.microphone !== "granted" && permissions.microphone === "granted") {
      if (prev.microphone === "denied") {
        // A Settings-toggle grant on a running app can need a fresh process
        // before CoreAudio delivers audio; offer the restart up front.
        flash("Microphone granted. If dictation still fails, restart Spoke.", {
          label: "Restart",
          action: () => invoke("restart_app"),
        });
      } else {
        flash("Microphone access granted");
      }
    }
    if (prev.accessibility !== "granted" && permissions.accessibility === "granted") {
      flash("Accessibility granted — Spoke can type now");
    }
  }
  updateWarnings();
  if (missingPermissions().length === 0) clearInterval(fastPollTimer);
}

// Poll disk for the currently-selected offline model so the warning is right
// even when the engine card is closed.
async function refreshModelWarning() {
  if (!config || config.general.mode !== "offline") {
    modelMissing = false;
    updateWarnings();
    return;
  }
  const model = config.offline.model;
  try {
    const info = await invoke("check_model", { model });
    if (config.general.mode === "offline" && model === config.offline.model) {
      modelMissing = !info.exists;
    }
  } catch (_) { /* leave prior value */ }
  updateWarnings();
}

// A yellow warning bar for the top of a card: explanation + action buttons.
// `buttons`: [{ label, onClick }] — first entry is the primary action.
function warnBar(text, buttons) {
  const bar = document.createElement("div");
  bar.className = "warn-bar";
  const span = document.createElement("span");
  span.className = "warn-text";
  span.textContent = text;
  bar.appendChild(span);
  for (const b of buttons) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "mini-btn";
    btn.textContent = b.label;
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      b.onClick();
    });
    bar.appendChild(btn);
  }
  return bar;
}

// ── Mosaic shape canvas ────────────────────────────────────────────────────
// Organic tile-mosaic blob on a transparent canvas — the tile alpha defines
// the silhouette (no containing circle). Each state morphs the boundary and
// palette; motion is procedural only (no audio input).
const S   = 48;
const dpr = Math.min(window.devicePixelRatio || 1, 2);
ring.width  = S * dpr;
ring.height = S * dpr;
const mCtx  = ring.getContext('2d');
mCtx.scale(dpr, dpr);

const COLS = 13;
const cell = S / COLS;
const GAP  = cell * 0.14;
const EDGE_SOFT = 1.6; // px of alpha falloff at the shape boundary
const STAR_PTS  = 6;
const CX = S / 2;
const CY = S / 2;

// Per-state palette + boundary parameters (radii in px at 48px scale).
const MODES = {
  idle: {
    pal: { bg: [10, 10, 12], fg: [228, 228, 234] },
    shape: { baseR: 17.5, blobAmp: 0.045, blobSpeed: 0.7, starAmp: 0, starSpeed: 0.5, breathAmp: 0.028, breathSpeed: 0.9, jitterAmp: 0 },
  },
  listening: {
    pal: { bg: [44, 10, 22], fg: [255, 108, 75] },
    shape: { baseR: 18.5, blobAmp: 0.105, blobSpeed: 2.7, starAmp: 0, starSpeed: 0.5, breathAmp: 0.055, breathSpeed: 3.4, jitterAmp: 0 },
  },
  processing: {
    pal: { bg: [8, 32, 44], fg: [38, 212, 196] },
    shape: { baseR: 17, blobAmp: 0.020, blobSpeed: 1.0, starAmp: 0.095, starSpeed: 1.7, breathAmp: 0.015, breathSpeed: 1.2, jitterAmp: 0 },
  },
};
const ERROR_PAL = { bg: [46, 26, 6], fg: [255, 186, 46] };

let mosaicT    = 0;
let mosaicPrev = performance.now();

// Animated state — mode weights and shape params are lerped every frame so
// state changes morph instead of cutting.
const shapeP = Object.assign({}, MODES.idle.shape);
const modeW  = { idle: 1, listening: 0, processing: 0 };
let errB = 0;                 // transient error blend 0..1 (overlays any mode)
let warnB = 0;                // persistent warning blend 0..1 (perms / model)
let warnActive = false;       // set by updateWarnings() from perm/model state
let press = 0;                // squish spring on press
let pressTarget = 0;
let pressT = -10;             // time of last press (drives the ripple)
let popT = -10;               // time processing finished (drives the "spit" pop)
// Phase accumulators so speed changes never make the motion jump.
let phB = 0, phBr = 0, phSt = 0;

const lerp    = (a, b, k) => a + (b - a) * k;
const clamp01 = (x) => (x < 0 ? 0 : x > 1 ? 1 : x);

// Reused per-frame color buffers (see drawMosaic) — module-level to avoid
// allocating on every animation frame.
const bbg = [0, 0, 0];
const bfg = [0, 0, 0];

// Ambient animation is throttled to this rate once the idle state has fully
// settled; transitions, listening/processing, error, and the press spring run
// at full refresh. Continuous 60fps full-canvas redraws when idle are the main
// driver of WebKit web-process memory/CPU.
const IDLE_INTERVAL = 1000 / 20;
let lastFrame = 0;

// ---- Window sizing --------------------------------------------------------

// Don't import LogicalSize from @tauri-apps/api/window — use the global.
const LOGICAL_SIZE   = window.__TAURI__.window.LogicalSize;
const LOGICAL_POS    = window.__TAURI__.window.LogicalPosition;

const IS_LINUX = navigator.userAgent.includes("Linux");
if (IS_LINUX) document.documentElement.classList.add("linux");

const MENU_W = 340;
const MENU_H = 480;
const BUBBLE_W = 80;
const BUBBLE_H = 80;
const MARGIN = 24;
const BUBBLE_HALF = BUBBLE_W / 2; // bubble centre sits this far inside its anchor corner

// Which way the menu grows from the bubble. Default (both false): the bubble
// anchors the window's bottom-right and the menu fans out up-left. When the
// bubble sits too close to the screen's left/top edge for the menu to fit,
// the axis flips and the menu grows the other way instead. The bubble itself
// never moves — only the window grows around it.
let flipX = false;
let flipY = false;

// Logical bounds of the monitor the window is on (falls back to window.screen).
async function monitorBounds() {
  try {
    const mon = await window.__TAURI__.window.currentMonitor();
    if (mon) {
      const f = mon.scaleFactor;
      return {
        x: mon.position.x / f,
        y: mon.position.y / f,
        w: mon.size.width / f,
        h: mon.size.height / f,
      };
    }
  } catch (_) {}
  return { x: 0, y: 0, w: window.screen.width, h: window.screen.height };
}

// Apply window size + position together (boot placement; menu open/close on
// non-Linux). On Linux this goes through one Rust command so both requests
// drain in the same event-loop iteration; menu open/close on Linux uses
// set_window_size_anchored instead (see resizeAndReposition).
async function applyBounds(x, y, w, h) {
  if (IS_LINUX) {
    await invoke("set_window_bounds", { x, y, w, h });
  } else {
    await appWindow.setSize(new LOGICAL_SIZE(w, h));
    await appWindow.setPosition(new LOGICAL_POS(x, y));
  }
}

// Linux: WebKitGTK blends every repaint OVER the transparent window's stale
// buffer instead of replacing it — anything translucent that gets repainted
// (moving elements, fading opacity, changing shadows) stacks and trails. A
// fresh buffer arrives only on a window resize. So on Linux the menu's
// transitions are disabled in CSS (state changes are instant; see the
// html.linux block in style.css) and every discrete change is followed by
// exactly one 1px gravity-anchored resize: the buffer is replaced with a
// single clean render of the new state. The flutter lands on the window's
// far, fully transparent edge; the bubble corner stays pinned by the WM.
let nudgeParity = false;
function nudgeOnce() {
  if (!IS_LINUX || menuState === "closed") return;
  nudgeParity = !nudgeParity;
  const gravity = (flipY ? "n" : "s") + (flipX ? "w" : "e");
  invoke("set_window_size_anchored", {
    w: MENU_W + (nudgeParity ? 1 : 0),
    h: MENU_H,
    gravity,
  }).catch(() => {});
}

// Present the current frame after a discrete menu change: X11 needs the
// buffer-swap resize above; a forced-Wayland backend needs the Rust-side
// repaint nudge (no-op elsewhere).
function presentFrame() {
  nudgeOnce();
  invoke("nudge_repaint");
}

// Scrolling a card repaints its region every tick; any translucent pixels in
// it accumulate (see nudgeOnce above). Swap the buffer once scrolling
// settles. Capture-phase on #subcard because scroll doesn't bubble and the
// scrolling .card-body is rebuilt on every render.
if (IS_LINUX) {
  let scrollNudgeTimer = null;
  subcard.addEventListener(
    "scroll",
    () => {
      clearTimeout(scrollNudgeTimer);
      scrollNudgeTimer = setTimeout(nudgeOnce, 150);
    },
    true
  );
}

/// Resize while keeping the bubble's screen position fixed. Recomputes the
/// grow direction per axis from where the bubble sits on its monitor, mirrors
/// the layout via flip-x/flip-y on #root, then sizes and places the window so
/// the bubble centre lands on the exact same screen pixel as before.
/// `initial` places the window at the screen's bottom-right instead — used
/// once at boot, mirroring position_bubble() in Rust.
/// `keepFlip` skips the flip recompute — used when shrinking back to
/// bubble-only size, where re-flipping mid-close would mirror the layout while
/// the window is still menu-sized and make the bubble jump corners.
async function resizeAndReposition(w, h, initial = false, keepFlip = false) {
  try {
    if (initial) {
      await applyBounds(
        window.screen.width - MARGIN - w,
        window.screen.height - MARGIN - h,
        w,
        h
      );
      return;
    }
    const factor = await appWindow.scaleFactor();
    const pos = (await appWindow.outerPosition()).toLogical(factor);
    const size = (await appWindow.outerSize()).toLogical(factor);
    // Bubble centre on screen under the current layout — the fixed point.
    const bx = pos.x + (flipX ? BUBBLE_HALF : size.width - BUBBLE_HALF);
    const by = pos.y + (flipY ? BUBBLE_HALF : size.height - BUBBLE_HALF);

    if (!keepFlip) {
      // Flip an axis when the menu wouldn't fit between the bubble and the
      // monitor edge it normally grows toward.
      const mon = await monitorBounds();
      flipX = bx + BUBBLE_HALF - MENU_W < mon.x;
      flipY = by + BUBBLE_HALF - MENU_H < mon.y;

      root.classList.toggle("flip-x", flipX);
      root.classList.toggle("flip-y", flipY);
      buildOrbit();
    }

    if (IS_LINUX) {
      // Resize anchored to the bubble's corner via WM gravity — no move
      // request, so the WM can't clamp the transient geometry and walk the
      // bubble (see set_window_size_anchored in lib.rs).
      const gravity = (flipY ? "n" : "s") + (flipX ? "w" : "e");
      await invoke("set_window_size_anchored", { w, h, gravity });
      return;
    }
    const x = flipX ? bx - BUBBLE_HALF : bx + BUBBLE_HALF - w;
    const y = flipY ? by - BUBBLE_HALF : by + BUBBLE_HALF - h;
    await applyBounds(x, y, w, h);
  } catch (_) { /* best‑effort — some platforms may lack the API */ }
}

// ---- Orbit menu -----------------------------------------------------------
// Sims-style radial menu: category bubbles fan out on two arcs around the
// main bubble (which sits 40px from the window's bottom-right corner).
// Angles are measured from screen-right: 90° = straight up, 180° = left.
// Clicking a category opens a floating sub-card with chip controls.

const BUBBLE_CX = 40; // main bubble centre, from the window's anchor corner

const CATS = [
  // Engine first and biggest: what model + acceleration is in use.
  // Radii tuned so neighbouring bubbles sit a few px apart (compact ring);
  // the sub-card opens in the band above the outer ring (see #subcard CSS),
  // so the outer ring must stay below ~200px from the anchor corner.
  { id: "engine",   label: "Engine",   r: 94,  angle: 98,  size: 60 },
  { id: "hotkey",   label: "Hotkey",   r: 86,  angle: 135, size: 50 },
  { id: "output",   label: "Output",   r: 86,  angle: 172, size: 50 },
  { id: "language", label: "Language", r: 144, angle: 108, size: 46 },
  { id: "mic",      label: "Mic",      r: 144, angle: 140, size: 46 },
  { id: "history",  label: "History",  r: 144, angle: 175, size: 46 },
];

// 'closed' | 'ring' | one of the CATS ids (sub-menu open).
let menuState = "closed";

function buildOrbit() {
  orbit.innerHTML = "";
  CATS.forEach((cat, i) => {
    const rad = (cat.angle * Math.PI) / 180;
    const dx = cat.r * Math.cos(rad); // negative = left of the bubble
    const dy = cat.r * Math.sin(rad); // positive = above the bubble
    const b = document.createElement("button");
    b.type = "button";
    b.className = "orbit-b";
    b.dataset.cat = cat.id;
    b.style.width = `${cat.size}px`;
    b.style.height = `${cat.size}px`;
    // Position from the bubble's anchor corner; flipped axes mirror the fan.
    b.style[flipX ? "left" : "right"] = `${BUBBLE_CX - dx - cat.size / 2}px`;
    b.style[flipY ? "top" : "bottom"] = `${BUBBLE_CX + dy - cat.size / 2}px`;
    // Vector back to the main bubble's centre — the closed-state collapse.
    // Screen-space, so it flips sign along a mirrored axis.
    b.style.setProperty("--cx", `${flipX ? dx : -dx}px`);
    b.style.setProperty("--cy", `${flipY ? -dy : dy}px`);
    // Staggered pop-out; reverse order on collapse so the ring folds inward.
    b.style.setProperty("--d", `${i * 45}ms`);
    b.style.setProperty("--dr", `${(CATS.length - 1 - i) * 30}ms`);

    const inner = document.createElement("span");
    inner.className = "ob-inner";
    inner.innerHTML = `
      <span class="ob-label">${cat.label}</span>
      <span class="ob-value" id="obv-${cat.id}"><span class="ob-marq">—</span></span>
      ${cat.id === "engine" ? '<span class="ob-badge" id="obb-engine">—</span>' : ""}
    `;
    b.appendChild(inner);
    // Badge lives on the button, not inside .ob-inner — the inner clips its
    // content to the circle and would eat the badge's corner overhang.
    if (WARN_CATS.includes(cat.id)) {
      const w = document.createElement("span");
      w.className = "ob-warn hide";
      w.id = `obw-${cat.id}`;
      w.textContent = "!";
      b.appendChild(w);
    }

    b.addEventListener("pointerdown", (e) => e.stopPropagation());
    b.addEventListener("click", (e) => {
      e.stopPropagation();
      if (menuState === cat.id) backToRing();
      else openCat(cat.id);
    });
    orbit.appendChild(b);
  });
  // Rebuilding wiped the badge nodes — restore their lit state.
  if (config) updateWarnings();
}

const MODEL_SHORT = { "large-v3-turbo": "turbo", "large-v3": "large" };
const modelShort = (m) => MODEL_SHORT[m] || m;

// Set a bubble's value text. When the text is wider than the bubble allows,
// mark it to scroll back and forth (CSS marquee) instead of overflowing the
// circle — long mic names are the usual case.
function setOrbitValue(id, text) {
  const el = $(`obv-${id}`);
  if (!el || !el.firstElementChild) return;
  el.firstElementChild.textContent = text;
  el.classList.remove("scroll");
  // Measure the inner span directly: with centered text the container's
  // scrollWidth only reports the right-side overhang (half the real overflow).
  // offsetWidth is layout-based, so the closed ring's scale() doesn't skew it.
  const overflow = el.firstElementChild.offsetWidth - el.clientWidth;
  if (overflow > 1) {
    el.style.setProperty("--marqueeShift", `${-overflow}px`);
    // Slower for longer overhang, clamped so it never crawls or zips.
    el.style.setProperty("--marqueeDur", `${Math.min(8, Math.max(2.5, overflow / 14))}s`);
    el.classList.add("scroll");
  }
}

// Refresh the live values shown inside the orbit bubbles.
function updateOrbitValues() {
  if (!config) return;
  const online = config.general.mode === "online";
  setOrbitValue("engine", online ? "online" : modelShort(config.offline.model));
  const badge = $("obb-engine");
  if (online) {
    badge.textContent = "API";
    badge.dataset.accel = "";
  } else {
    const label = getEffectiveAccel(config.offline.accel || "auto", buildInfo);
    badge.textContent = label;
    badge.dataset.accel = label;
  }
  setOrbitValue("hotkey", config.general.hotkey || "—");
  setOrbitValue(
    "language",
    config.general.language === "auto" ? "Auto" : config.general.language.toUpperCase()
  );
  setOrbitValue(
    "output",
    { type: "Type", copy: "Copy", both: "Both" }[config.general.output_dest] || "Type"
  );
  setOrbitValue("mic", config.recording.input_device || "Default");
  setOrbitValue("history", String(history.length));
}

async function openRing() {
  clearTimeout(closeTimer); // cancel a pending post-animation shrink
  Sfx.play("menuOpen");
  menuState = "ring";
  // Grow the window (and rebuild the orbit for the current flip direction)
  // before the pop-out plays, so the animation is never clipped.
  await resizeAndReposition(MENU_W, MENU_H);
  // The window starts with focus:false and some Linux WMs don't focus an
  // undecorated skip-taskbar window on click — without focus, Escape and the
  // hotkey recorder never see key events. Best-effort everywhere else.
  try { await appWindow.setFocus(); } catch (_) {}
  updateOrbitValues();
  orbit.classList.remove("closed", "dimmed");
  subcard.classList.add("hidden");
  for (const b of orbit.children) b.classList.remove("active");
  updateWarnings();
  minimize.classList.remove("hidden");
  presentFrame();
  // Re-check on open so badges reflect grants/downloads made since the last poll.
  checkPermissions();
  refreshModelWarning();
}

// Tray state priority: active pipeline wins, then warnings, then idle.
function currentTrayState() {
  if (bubble.classList.contains("recording")) return "recording";
  if (bubble.classList.contains("processing")) return "processing";
  if (warnActive || bubble.classList.contains("error")) return "warning";
  return "idle";
}

// Push the tray color, but only while minimized (the tray is neutral otherwise).
function updateTray() {
  if (!minimized) return;
  invoke("set_tray_state", { state: currentTrayState() });
}

function openCat(id) {
  menuState = id;
  orbit.classList.add("dimmed");
  for (const b of orbit.children) b.classList.toggle("active", b.dataset.cat === id);
  renderCard(id);
  subcard.classList.remove("hidden");
  presentFrame();
}

function backToRing() {
  menuState = "ring";
  orbit.classList.remove("dimmed");
  for (const b of orbit.children) b.classList.remove("active");
  subcard.classList.add("hidden");
  presentFrame();
}

let closeTimer = null;

function closeMenu() {
  if (capturing) endCapture();
  // Only when something was actually open — closeMenu() is also called
  // defensively (minimize, restore) where there is no ring to fold away.
  if (menuState !== "closed") Sfx.play("menuClose");
  menuState = "closed";
  orbit.classList.add("closed");
  orbit.classList.remove("dimmed");
  subcard.classList.add("hidden");
  minimize.classList.add("hidden");
  invoke("nudge_repaint");
  clearTimeout(closeTimer);
  if (IS_LINUX) {
    // Transitions are disabled on Linux, so the collapse is instant — shrink
    // right away; the resize also discards any residue in the stale buffer.
    resizeAndReposition(BUBBLE_W, BUBBLE_H, false, true);
    return;
  }
  // Shrinking the window is what restores click-through around the bubble,
  // but doing it immediately clips the collapse animation (0.45s spring +
  // up to 150ms stagger). Wait for the fold to land, then shrink.
  closeTimer = setTimeout(() => {
    if (menuState === "closed") resizeAndReposition(BUBBLE_W, BUBBLE_H, false, true);
  }, 650);
}

document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || capturing) return;
  if (menuState === "ring") closeMenu();
  else if (menuState !== "closed") backToRing();
});

// Close the menu when clicking outside all of its pieces.
document.addEventListener("pointerdown", (e) => {
  if (menuState === "closed") return;
  if (
    !subcard.contains(e.target) &&
    !orbit.contains(e.target) &&
    !bubble.contains(e.target) &&
    !minimize.contains(e.target)
  ) {
    closeMenu();
  }
});
subcard.addEventListener("pointerdown", (e) => e.stopPropagation());

// ---- Config ---------------------------------------------------------------

async function saveConfig() {
  updateOrbitValues();
  Sfx.fromConfig(config);
  try {
    await invoke("set_config", { newConfig: config });
    flash("Saved");
  } catch (e) {
    flash(String(e));
  }
  // Clipboard mode toggles whether the Accessibility warning is relevant; a
  // model/mode switch changes whether the missing-model warning applies.
  updateWarnings();
  refreshModelWarning();
}

let flashTimer = null;
// `action` (optional): { label, action } renders a button that stays until
// tapped instead of auto-hiding, for prompts the user must act on.
function flash(msg, action) {
  toast.textContent = msg;
  toast.classList.remove("hide");
  clearTimeout(flashTimer);
  presentFrame();
  if (action) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "toast-action";
    btn.textContent = action.label;
    btn.addEventListener("click", () => {
      toast.classList.add("hide");
      presentFrame();
      action.action();
    });
    toast.appendChild(btn);
    return;
  }
  flashTimer = setTimeout(() => {
    toast.classList.add("hide");
    presentFrame();
  }, 2500);
}

// ---- Sub-menu cards ---------------------------------------------------------
// Each card is a small template + wiring function. Dropdowns are replaced by
// chip buttons: current value highlighted, one tap to change.

function cardShell(title, extra = "") {
  return `
    <header class="card-head">
      <span class="card-title">${title}</span>
      ${extra}
      <button type="button" class="card-back" title="Back">&times;</button>
    </header>
    <div class="card-body"></div>
  `;
}

// Build a row of chip buttons. opts: [{value, label, note?}]
function chipRow(opts, selected, onPick) {
  const wrap = document.createElement("div");
  wrap.className = "chips";
  for (const o of opts) {
    const c = document.createElement("button");
    c.type = "button";
    c.className = "chip" + (o.value === selected ? " selected" : "");
    c.innerHTML = o.note ? `${o.label}<span class="chip-note">${o.note}</span>` : o.label;
    c.addEventListener("click", () => onPick(o.value));
    wrap.appendChild(c);
  }
  return wrap;
}

function section(label) {
  const sec = document.createElement("div");
  sec.className = "card-sec";
  if (label) {
    const l = document.createElement("div");
    l.className = "sec-label";
    l.textContent = label;
    sec.appendChild(l);
  }
  return sec;
}

const CARD_TITLES = {
  engine: "Engine",
  hotkey: "Hotkey",
  language: "Language",
  output: "Output",
  mic: "Microphone",
  history: "History",
};

function renderCard(id) {
  const extra =
    id === "engine" ? '<span class="muted" id="version">v0.1</span>' : "";
  subcard.innerHTML = cardShell(CARD_TITLES[id], extra);
  subcard.querySelector(".card-back").addEventListener("click", backToRing);
  const body = subcard.querySelector(".card-body");
  CARD_BUILDERS[id](body);
  presentFrame();
}

// Re-render the currently open card in place (after a config change that
// alters its own layout, e.g. switching offline/online).
function rerenderCard() {
  if (menuState !== "closed" && menuState !== "ring") renderCard(menuState);
}

// ---- Engine card: mode, model, acceleration -------------------------------

function buildEngineCard(body) {
  if (modelWarn() && !modelDownloading) {
    body.appendChild(
      warnBar(
        `Model “${modelShort(config.offline.model)}” not downloaded — Spoke can't transcribe offline`,
        [{ label: "Download", onClick: startModelDownload }]
      )
    );
  }

  const modeSec = section("Mode");
  modeSec.appendChild(
    chipRow(
      [
        { value: "offline", label: "Offline" },
        { value: "online", label: "Online" },
      ],
      config.general.mode,
      async (v) => {
        config.general.mode = v;
        await saveConfig();
        rerenderCard();
      }
    )
  );
  body.appendChild(modeSec);

  if (config.general.mode === "online") {
    const keySec = section("API key");
    const input = document.createElement("input");
    input.type = "password";
    input.spellcheck = false;
    input.value = config.online.api_key;
    input.addEventListener("change", () => {
      config.online.api_key = input.value;
      saveConfig();
    });
    keySec.appendChild(input);
    body.appendChild(keySec);
    return;
  }

  // -- Offline: model choice + install state --
  const modelSec = section("Model");
  modelSec.appendChild(
    chipRow(
      [
        { value: "tiny", label: "tiny", note: "74 MB" },
        { value: "base", label: "base", note: "141 MB" },
        { value: "small", label: "small", note: "465 MB" },
        { value: "medium", label: "medium", note: "1.4 GB" },
        { value: "large-v3-turbo", label: "turbo", note: "1.5 GB" },
        { value: "large-v3", label: "large", note: "2.9 GB" },
      ],
      config.offline.model,
      async (v) => {
        if (modelDownloading) return;
        config.offline.model = v;
        await saveConfig();
        rerenderCard();
      }
    )
  );
  const dl = document.createElement("div");
  dl.className = "dl-row";
  dl.innerHTML = `
    <span id="modelStatus">…</span>
    <button id="downloadModel" type="button" class="mini-btn hide">Download</button>
    <button id="deleteModel" type="button" class="mini-btn danger hide">Delete</button>
  `;
  modelSec.appendChild(dl);
  body.appendChild(modelSec);
  $("downloadModel").addEventListener("click", startModelDownload);
  $("deleteModel").addEventListener("click", deleteCurrentModel);

  // Mark installed model chips with a checkmark.
  const modelIdx = { tiny: 0, base: 1, small: 2, medium: 3, "large-v3-turbo": 4, "large-v3": 5 };
  invoke("check_models").then((installed) => {
    const chips = modelSec.querySelectorAll(".chip");
    for (const name of installed) {
      const idx = modelIdx[name];
      if (idx !== undefined && idx < chips.length) chips[idx].classList.add("installed");
    }
  }).catch(() => {});

  // -- Acceleration backend (only when the build offers a choice) --
  const backends = (buildInfo && buildInfo.backends) || [];
  if (buildInfo && buildInfo.whisper && backends.length > 1) {
    const accelSec = section("Acceleration");
    const opts = [
      { value: "auto", label: `Auto`, note: buildInfo.acceleration },
      ...backends.map((b) => ({ value: b.id, label: b.label })),
    ];
    const saved = config.offline.accel;
    const selected =
      saved === "auto" || backends.some((b) => b.id === saved) ? saved : "auto";
    accelSec.appendChild(
      chipRow(opts, selected || "auto", async (v) => {
        config.offline.accel = v;
        await saveConfig();
        rerenderCard();
        if (!modelDownloading) checkCurrentModel();
      })
    );
    body.appendChild(accelSec);
  } else if (buildInfo && !buildInfo.whisper) {
    const note = section("Acceleration");
    note.insertAdjacentHTML("beforeend", '<span class="muted">CPU (no whisper)</span>');
    body.appendChild(note);
  }

  // -- CoreML bundle (Apple Neural Engine) --
  if (coremlRelevant()) {
    const cmSec = section("Neural engine bundle");
    const desc = document.createElement("p");
    desc.className = "muted";
    desc.style.cssText = "margin: 0 0 6px; line-height: 1.4; font-size: 10px;";
    desc.textContent =
      "Offloads the Whisper encoder to the Apple Neural Engine for faster transcription. " +
      "When not downloaded the GPU (Metal) backend is used as fallback.";
    cmSec.appendChild(desc);

    const row = document.createElement("div");
    row.className = "dl-row";
    row.title = "CoreML encoder (.mlmodelc) — enables Apple Neural Engine";
    const bundleSize = coremlBundleSize(config.offline.model);
    row.innerHTML = `
      <span id="coremlStatus">…</span>
      <span class="muted">${bundleSize}</span>
      <button id="downloadCoreml" type="button" class="mini-btn hide">Download</button>
    `;
    cmSec.appendChild(row);
    body.appendChild(cmSec);
    $("downloadCoreml").addEventListener("click", startCoremlDownload);
  }

  if (!modelDownloading) checkCurrentModel();
  else setModelStatusText(); // repopulate progress into the fresh DOM
}

// True when the given backend id was compiled into this build.
function hasBackend(id) {
  return !!(buildInfo && buildInfo.backends && buildInfo.backends.some((b) => b.id === id));
}

function coremlBundleSize(model) {
  const sizes = {
    tiny: "14 MB",
    base: "36 MB",
    small: "156 MB",
    medium: "542 MB",
    "large-v3-turbo": "1.1 GB",
    "large-v3": "1.1 GB",
  };
  return sizes[model] || "";
}

function coremlRelevant() {
  const accel = config.offline.accel || "auto";
  return hasBackend("coreml") && (accel === "coreml" || accel === "auto");
}

// Badge label for the currently selected accel value. "auto" resolves to the
// build's best backend; unknown/stale ids fall back to CPU.
function getEffectiveAccel(accel, info) {
  if (!info) return "—";
  if (!info.whisper) return "CPU";
  if (accel === "auto" || !accel) return info.acceleration || "CPU";
  const backend = (info.backends || []).find((b) => b.id === accel);
  return backend ? backend.badge : "CPU";
}

async function loadBuildInfo() {
  try {
    buildInfo = await invoke("get_build_info");
  } catch (_) {
    buildInfo = null;
  }
  updateOrbitValues();
}

// ---- Hotkey card ------------------------------------------------------------

function buildHotkeyCard(body) {
  const trigSec = section("Trigger");
  trigSec.appendChild(
    chipRow(
      [
        { value: "push_to_talk", label: "Push to talk" },
        { value: "toggle", label: "Toggle" },
      ],
      config.general.trigger,
      async (v) => {
        config.general.trigger = v;
        await saveConfig();
        rerenderCard();
      }
    )
  );
  body.appendChild(trigSec);

  const hkSec = section("Shortcut");
  const ctl = document.createElement("div");
  ctl.className = "hotkey-ctl";
  ctl.innerHTML = `
    <span id="hotkey" class="hotkey-display">${config.general.hotkey || "—"}</span>
    <button id="recordHotkey" type="button" class="mini-btn">Record</button>
  `;
  hkSec.appendChild(ctl);
  body.appendChild(hkSec);
  $("recordHotkey").addEventListener("click", (e) => {
    e.stopPropagation();
    startCapture();
  });
}

// ---- Language card ----------------------------------------------------------

function buildLanguageCard(body) {
  const sec = section("Spoken language");
  sec.appendChild(
    chipRow(
      [
        { value: "auto", label: "Auto" },
        { value: "en", label: "English" },
        { value: "da", label: "Danish" },
        { value: "de", label: "German" },
        { value: "es", label: "Spanish" },
        { value: "fr", label: "French" },
      ],
      config.general.language,
      async (v) => {
        config.general.language = v;
        await saveConfig();
        rerenderCard();
      }
    )
  );
  body.appendChild(sec);
}

// ---- Output card: destination + audio saving --------------------------------

function buildOutputCard(body) {
  if (missingPermissions().includes("accessibility")) {
    body.appendChild(
      warnBar(`${PERM_INFO.accessibility.label} — ${PERM_INFO.accessibility.hint}`, [
        {
          label: "Grant permission",
          onClick: async () => {
            // The native AX prompt registers the *current* binary with TCC;
            // Settings then opens on the right pane so the user can flip the
            // toggle. The grant registers live — no restart in the normal path.
            try { await invoke("request_accessibility_permission"); } catch (_) { /* not macOS */ }
            invoke("open_permission_settings", { which: "accessibility" });
            watchForGrant();
            flash("Enable Spoke in the Accessibility list — it applies within seconds.");
          },
        },
        {
          label: "Already on? Fix it",
          onClick: async () => {
            // Stale grant: Settings shows Spoke enabled, but the entry was
            // recorded for a previous build of the binary so the OS still says
            // no. Reset the entry and re-register this binary.
            try { await invoke("reset_permission", { which: "accessibility" }); } catch (_) { /* not macOS */ }
            try { await invoke("request_accessibility_permission"); } catch (_) { /* not macOS */ }
            invoke("open_permission_settings", { which: "accessibility" });
            watchForGrant();
            flash("Permission reset — enable Spoke in the list again. Still stuck?", {
              label: "Restart Spoke",
              action: () => invoke("restart_app"),
            });
          },
        },
      ])
    );
  }

  const destSec = section("Destination");
  destSec.appendChild(
    chipRow(
      [
        { value: "type", label: "Type it out" },
        { value: "copy", label: "Clipboard" },
        { value: "both", label: "Both" },
      ],
      config.general.output_dest,
      async (v) => {
        config.general.output_dest = v;
        await saveConfig();
        rerenderCard();
      }
    )
  );
  body.appendChild(destSec);

  // Master switch for every UI sound (menu, record start/stop, done chime).
  const soundSec = section("Sounds");
  soundSec.appendChild(
    chipRow(
      [
        { value: "on", label: "On" },
        { value: "off", label: "Off" },
      ],
      config.ui.sounds ? "on" : "off",
      async (v) => {
        config.ui.sounds = v === "on";
        await saveConfig();
        // Confirm the new setting audibly — silent when it was just turned off.
        Sfx.play("menuOpen");
        rerenderCard();
      }
    )
  );
  body.appendChild(soundSec);

  const saveSec = section("Save audio");
  saveSec.appendChild(
    chipRow(
      [
        { value: "off", label: "Off" },
        { value: "on", label: "On" },
      ],
      config.recording.save_audio ? "on" : "off",
      async (v) => {
        config.recording.save_audio = v === "on";
        await saveConfig();
        rerenderCard();
      }
    )
  );
  body.appendChild(saveSec);

  if (config.recording.save_audio) {
    const pathSec = section("Save path");
    const input = document.createElement("input");
    input.type = "text";
    input.spellcheck = false;
    input.value = config.recording.save_path;
    input.addEventListener("change", () => {
      config.recording.save_path = input.value.trim();
      saveConfig();
    });
    pathSec.appendChild(input);
    body.appendChild(pathSec);

    const modeSec = section("Save which audio");
    modeSec.appendChild(
      chipRow(
        [
          { value: "original", label: "Original" },
          { value: "processed", label: "Processed" },
        ],
        config.recording.save_processed ? "processed" : "original",
        async (v) => {
          config.recording.save_processed = v === "processed";
          await saveConfig();
          rerenderCard();
        }
      )
    );
    body.appendChild(modeSec);
  }
}

// ---- Microphone card ----------------------------------------------------------

async function buildMicCard(body) {
  if (permissions && permissions.microphone !== "granted" && permissions.microphone !== "unknown") {
    // Fire the native mic prompt and refresh. A grant from this prompt applies
    // to the running process instantly — no Settings trip, no restart.
    const promptForMic = async () => {
      const granted = await invoke("request_microphone_permission").catch(() => true);
      await checkPermissions();
      if (!granted) rerenderCard();
    };
    body.appendChild(
      permissions.microphone === "denied"
        ? warnBar(`${PERM_INFO.microphone.label} — ${PERM_INFO.microphone.hint}`, [
            {
              label: "Ask me again",
              onClick: async () => {
                // Clear the denied (or stale, from an older build) TCC entry so
                // the OS treats us as never-asked, then re-prompt natively.
                try { await invoke("reset_permission", { which: "microphone" }); } catch (_) { /* not macOS */ }
                await promptForMic();
              },
            },
            {
              label: "Open Settings",
              onClick: () => {
                invoke("open_permission_settings", { which: "microphone" });
                watchForGrant();
                flash("Toggle Spoke on. If macOS asks to reopen the app, let it.");
              },
            },
          ])
        : warnBar("Microphone not enabled yet — Spoke can't hear you", [
            { label: "Enable microphone", onClick: promptForMic },
          ])
    );
  }

  const sec = section("Input device");
  sec.insertAdjacentHTML("beforeend", '<span class="muted">Loading…</span>');
  body.appendChild(sec);
  let devices = [];
  try {
    devices = await invoke("list_audio_devices");
  } catch (e) {
    console.error("Failed to list audio devices:", e);
  }
  if (menuState !== "mic") return; // card changed while we were fetching
  sec.querySelector(".muted").remove();
  sec.appendChild(
    chipRow(
      [{ value: "", label: "Default" }, ...devices.map((d) => ({ value: d, label: d }))],
      config.recording.input_device || "",
      async (v) => {
        config.recording.input_device = v;
        await saveConfig();
        rerenderCard();
      }
    )
  );
}

// ---- History card ---------------------------------------------------------------

async function copyToClipboard(text) {
  try {
    await navigator.clipboard.writeText(text);
    flash("Copied");
  } catch {
    flash("Copy failed");
  }
}

function buildHistoryCard(body) {
  const list = document.createElement("div");
  list.id = "historyList";
  body.appendChild(list);
  renderHistory();
}

function renderHistory() {
  const list = $("historyList");
  if (!list) return;
  list.innerHTML = "";
  if (history.length === 0) {
    list.innerHTML = '<div class="history-empty">No transcriptions yet</div>';
    return;
  }
  for (const entry of history) {
    const row = document.createElement("div");
    row.className = "history-entry";
    const label = document.createElement("span");
    label.className = "history-text";
    label.textContent = entry.text;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "mini-btn";
    btn.textContent = "Copy";
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      copyToClipboard(entry.text);
    });
    row.appendChild(label);
    row.appendChild(btn);
    list.appendChild(row);
  }
}

const CARD_BUILDERS = {
  engine: buildEngineCard,
  hotkey: buildHotkeyCard,
  language: buildLanguageCard,
  output: buildOutputCard,
  mic: buildMicCard,
  history: buildHistoryCard,
};

// ---- Hotkey recorder ----------------------------------------------------

function keyName(code) {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F([1-9]|1[0-2])$/.test(code)) return code.toLowerCase();
  const map = {
    Space: "space",
    Enter: "enter",
    Return: "enter",
    Tab: "tab",
    Escape: "esc",
  };
  return map[code] || null;
}

function comboFromEvent(e) {
  const mods = [];
  if (e.metaKey) mods.push("cmd");
  if (e.ctrlKey) mods.push("ctrl");
  if (e.altKey) mods.push("alt");
  if (e.shiftKey) mods.push("shift");
  const key = keyName(e.code);
  if (!key) return null;
  return [...mods, key].join("+");
}

let capturing = false;

function startCapture() {
  if (capturing) return;
  capturing = true;
  const btn = $("recordHotkey");
  const disp = $("hotkey");
  if (btn) {
    btn.classList.add("recording");
    btn.textContent = "Press keys…";
  }
  if (disp) disp.classList.add("listening");
  window.addEventListener("keydown", onCaptureKey, true);
}

function endCapture() {
  capturing = false;
  const btn = $("recordHotkey");
  const disp = $("hotkey");
  if (btn) {
    btn.classList.remove("recording");
    btn.textContent = "Record";
  }
  if (disp) disp.classList.remove("listening");
  window.removeEventListener("keydown", onCaptureKey, true);
}

async function onCaptureKey(e) {
  e.preventDefault();
  e.stopPropagation();
  if (e.key === "Escape" && !e.metaKey && !e.ctrlKey && !e.altKey) {
    endCapture();
    return;
  }
  const combo = comboFromEvent(e);
  if (!combo) return;
  config.general.hotkey = combo;
  const disp = $("hotkey");
  if (disp) disp.textContent = combo;
  endCapture();
  await saveConfig();
}

// ---- Bubble state -------------------------------------------------------

function setBubbleState(state, message) {
  const wasDragging = bubble.classList.contains("dragging");
  const wasRecording = bubble.classList.contains("recording");
  const wasProcessing = bubble.classList.contains("processing");
  bubble.className = state;
  if (wasDragging) bubble.classList.add("dragging");
  // Sound cues follow the pipeline transitions, not the raw state, so a repeat
  // of the same state is silent and a failed run (which lands on error before
  // settling to idle) doesn't ring the "done" chime.
  if (state === "recording" && !wasRecording) Sfx.play("recordStart");
  else if (state === "processing" && wasRecording) Sfx.play("recordStop");
  else if (state === "idle" && wasProcessing) Sfx.play("transcribeDone");
  // Processing → idle means transcription landed and typing begins: fire the
  // "spit" pop so the bubble visibly reacts to finishing.
  if (wasProcessing && state === "idle") popT = mosaicT;
  recording = state === "recording";
  if (state === "error" && message) flash(message);
  updateTray();
}

// Advance mode weights, error blend, press spring, and shape params.
function mosaicStep(dt) {
  const mode = bubble.classList.contains('recording') ? 'listening'
             : bubble.classList.contains('processing') ? 'processing'
             : 'idle';
  const isError = bubble.classList.contains('error');
  const k  = 1 - Math.exp(-dt * 4);
  const k2 = 1 - Math.exp(-dt * 5);

  let sum = 0;
  for (const m in modeW) {
    modeW[m] = lerp(modeW[m], mode === m ? 1 : 0, k2);
    sum += modeW[m];
  }
  for (const m in modeW) modeW[m] /= sum;

  errB  = lerp(errB, isError ? 1 : 0, k);
  warnB = lerp(warnB, warnActive ? 1 : 0, k);
  // Both error and warning use the same amber pulse — take whichever is stronger.
  const amber = Math.max(errB, warnB);

  // Press spring: fast attack, softer release.
  const pk = 1 - Math.exp(-dt * (pressTarget > press ? 22 : 8));
  press = lerp(press, pressTarget, pk);

  for (const key in shapeP) {
    let v = 0;
    for (const m in modeW) v += MODES[m].shape[key] * modeW[m];
    if (amber > 0.001) {
      if (key === 'jitterAmp')   v = lerp(v, 0.06, amber);
      if (key === 'breathAmp')   v = lerp(v, 0.06, amber);
      if (key === 'breathSpeed') v = lerp(v, 5.2, amber);
    }
    shapeP[key] = lerp(shapeP[key], v, k);
  }

  phB  += shapeP.blobSpeed   * dt;
  phBr += shapeP.breathSpeed * dt;
  phSt += shapeP.starSpeed   * dt;
}

// Organic boundary radius at angle th.
function shapeR(th) {
  const p = shapeP;
  const blob = Math.sin(th * 3 + phB) * 0.5
             + Math.sin(th * 5 - phB * 1.37 + 1.7) * 0.30
             + Math.sin(th * 2 + phB * 0.80 + 4.2) * 0.35;
  const star   = Math.cos(th * STAR_PTS - phSt);
  const jag    = Math.sin(th * 9 + mosaicT * 6) * Math.sin(th * 13 - mosaicT * 7.3);
  const breath = Math.sin(phBr);
  return p.baseR * (1 + p.breathAmp * breath + p.blobAmp * blob + p.starAmp * star + p.jitterAmp * jag);
}

function drawMosaic(now) {
  const dt   = Math.min((now - mosaicPrev) / 1000, 0.05);
  mosaicT   += dt;
  mosaicPrev = now;
  mosaicStep(dt);

  // Blend palettes once per frame. Reuse module-level arrays (bbg/bfg) instead
  // of allocating on every frame — avoids GC churn that inflates the WebKit
  // web process heap under the continuous animation loop.
  bbg[0] = bbg[1] = bbg[2] = 0;
  bfg[0] = bfg[1] = bfg[2] = 0;
  for (const m in modeW) {
    const pal = MODES[m].pal;
    for (let i = 0; i < 3; i++) {
      bbg[i] += pal.bg[i] * modeW[m];
      bfg[i] += pal.fg[i] * modeW[m];
    }
  }
  // Amber overlay = strongest of transient error and persistent warning.
  const amber = Math.max(errB, warnB);
  for (let i = 0; i < 3; i++) {
    bbg[i] = lerp(bbg[i], ERROR_PAL.bg[i], amber * 0.85);
    bfg[i] = lerp(bfg[i], ERROR_PAL.fg[i], amber * 0.85);
  }

  // Transparent outside the blob — everything beyond the boundary stays clear.
  mCtx.clearRect(0, 0, S, S);

  const wI = modeW.idle, wL = modeW.listening, wP = modeW.processing;
  // Damped-spring "spit" when processing finishes: quick outward push that
  // rebounds and settles (~0.9s), plus a bright ripple in the tile loop.
  const popAge = mosaicT - popT;
  const pop = popAge >= 0 && popAge < 1
    ? Math.exp(-popAge * 4.5) * Math.sin(popAge * 14)
    : 0;
  const sx = 1 + press * 0.09 + pop * 0.08;
  const sy = 1 - press * 0.13 + pop * 0.13;
  const pressAge = mosaicT - pressT;
  const blink = Math.pow(Math.max(0, Math.sin(mosaicT * 3.2)), 3);

  // Fill the silhouette with the background palette color first so the gaps
  // between tiles read as part of the blob instead of showing the desktop
  // through. Trace the boundary once and reuse the path for the outline.
  const N = 64;
  const path = new Path2D();
  for (let i = 0; i <= N; i++) {
    const th = (i / N) * Math.PI * 2;
    const R = shapeR(th);
    const X = CX + Math.cos(th) * R * sx;
    const Y = CY + Math.sin(th) * R * sy;
    if (i === 0) path.moveTo(X, Y);
    else path.lineTo(X, Y);
  }
  path.closePath();
  mCtx.fillStyle = `rgb(${bbg[0] | 0},${bbg[1] | 0},${bbg[2] | 0})`;
  mCtx.fill(path);
  // Clip the tiles to the silhouette so partially-faded edge tiles never
  // poke past the boundary (they read as grey stubs on light desktops).
  mCtx.save();
  mCtx.clip(path);

  for (let row = 0; row < COLS; row++) {
    for (let col = 0; col < COLS; col++) {
      const x = col * cell + cell / 2;
      const y = row * cell + cell / 2;
      const dx = (x - CX) / sx;
      const dy = (y - CY) / sy;
      const th = Math.atan2(dy, dx);
      const d  = Math.sqrt(dx * dx + dy * dy);
      const R  = shapeR(th);
      const a  = clamp01((R - d) / EDGE_SOFT + 0.5);
      if (a < 0.02) continue;

      const distN = clamp01(d / R);
      const nx = x / S;
      const ny = y / S;
      const hash = ((col * 7 + row * 13) % 31) / 31;

      let v = 0;
      if (wI > 0.02) {
        v += wI * (0.22
          + 0.18 * Math.sin(mosaicT * 0.55 + nx * 4.0 + ny * 2.5 + hash * 1.5)
          + 0.08 * Math.sin(mosaicT * 0.40 + nx * 1.5 - ny * 3.0 + hash * 2.0));
      }
      if (wL > 0.02) {
        const r1 = Math.sin(mosaicT * 2.8 - distN * 4.5);
        const r2 = Math.sin(mosaicT * 3.1 - distN * 5.5 + 2.1);
        const fl = Math.abs(Math.sin(mosaicT * 5.5 + col * 1.1)) * Math.max(0, 1 - distN * 1.6);
        v += wL * (0.08 + Math.max(0, r1) * 0.52 + Math.max(0, r2) * 0.30 + fl * 0.15);
      }
      if (wP > 0.02) {
        const waveA = Math.sin(mosaicT * 1.6 + th * 3 + distN * 6);
        const waveB = Math.sin(mosaicT * 2.4 - distN * 8 + th * 2);
        v += wP * (0.10 + (waveA * 0.5 + 0.5) * 0.38 + (waveB * 0.5 + 0.5) * 0.30);
      }
      if (amber > 0.02) {
        const ve = 0.12 + Math.max(0, Math.sin(mosaicT * 5 - distN * 4)) * 0.55 + blink * 0.35;
        v = lerp(v, ve, amber * 0.85);
      }
      // Press ripple radiating outward from the click.
      if (pressAge < 0.8) {
        const band = Math.max(0, 1 - Math.abs(distN * 1.15 - pressAge * 1.9) * 5) * (1 - pressAge / 0.8);
        v += band * 0.6;
      }
      // Brighter, faster ripple for the done-processing spit.
      if (popAge >= 0 && popAge < 0.7) {
        const band = Math.max(0, 1 - Math.abs(distN * 1.15 - popAge * 2.4) * 4) * (1 - popAge / 0.7);
        v += band * 0.85;
      }
      v = clamp01(v);

      const ir = Math.round(bbg[0] + (bfg[0] - bbg[0]) * v);
      const ig = Math.round(bbg[1] + (bfg[1] - bbg[1]) * v);
      const ib = Math.round(bbg[2] + (bfg[2] - bbg[2]) * v);
      const sz = 0.68 + v * 0.32;
      const cw = (cell - GAP) * sz;
      mCtx.globalAlpha = a;
      mCtx.fillStyle = `rgb(${ir},${ig},${ib})`;
      mCtx.fillRect(x - cw / 2, y - cw / 2, cw, cw);
    }
  }
  mCtx.globalAlpha = 1;
  mCtx.restore();

  // Soft outline tracing the boundary — helps the silhouette read against
  // busy desktop backgrounds.
  const baseA = 0.34 + amber * blink * 0.45;
  mCtx.lineWidth = 1;
  mCtx.strokeStyle = `rgba(${bfg[0] | 0},${bfg[1] | 0},${bfg[2] | 0},${baseA.toFixed(3)})`;
  mCtx.stroke(path);
}

// Animation driver: always schedules the next frame, but skips the actual draw
// when the window is hidden (occluded/minimized) and throttles to IDLE_INTERVAL
// once the idle state has fully settled. Transitions, active states, error,
// and the press spring run at full refresh so the morphing stays smooth.
function tick(now) {
  requestAnimationFrame(tick);
  if (document.hidden) return;
  const settled = modeW.idle > 0.995 && errB < 0.005 &&
                  warnB < 0.005 && !warnActive &&
                  press < 0.01 && pressTarget === 0 &&
                  mosaicT - popT > 1.2;
  if (settled && now - lastFrame < IDLE_INTERVAL) return;
  lastFrame = now;
  drawMosaic(now);
}

// ---- Events from Rust ---------------------------------------------------

listen("spoke:state", (e) => {
  const { state, message } = e.payload;
  setBubbleState(state, message);
});

// Tray-click restore: the bubble is visible again, so stop coloring the tray.
listen("spoke:restored", () => {
  minimized = false;
});

// Minimize pill: close the menu, seed the tray color, then hide to the tray.
minimize.addEventListener("click", async (e) => {
  e.stopPropagation();
  minimized = true;
  closeMenu();
  await invoke("set_tray_state", { state: currentTrayState() });
  invoke("minimize_to_tray");
});

listen("spoke:transcript", (e) => {
  const text = e.payload.text;
  if (!text) return;
  history.unshift({ text, time: Date.now() });
  if (history.length > MAX_HISTORY) history.pop();
  if (menuState !== "closed") updateOrbitValues();
  if (menuState === "history") renderHistory();
});

// Config changed from the tray menu — adopt it and refresh any open UI so the
// bubble's controls stay in sync with the tray.
listen("spoke:config", (e) => {
  if (!e.payload) return;
  config = e.payload;
  Sfx.fromConfig(config);
  if (menuState === "ring") updateOrbitValues();
  else rerenderCard();
});

// ---- Model download --------------------------------------------------
// The status elements live inside the engine card, which may be closed while a
// download runs — every DOM touch goes through null-safe helpers, and the
// card re-populates from the download state when it re-opens.

function setStat(id, text, cls) {
  const el = $(id);
  if (!el) return;
  el.textContent = text;
  el.className = cls ? cls : "";
}

let modelDlText = "";
function setModelStatusText() {
  if (modelDlText) setStat("modelStatus", modelDlText, "downloading");
}

async function checkCurrentModel() {
  const model = config.offline.model;
  setStat("modelStatus", "…", "");
  $("downloadModel") && $("downloadModel").classList.add("hide");
  $("deleteModel") && $("deleteModel").classList.add("hide");
  try {
    const info = await invoke("check_model", { model });
    if (model !== config.offline.model) return; // stale response
    if (info.exists) {
      setStat("modelStatus", "✓ installed", "installed");
      $("deleteModel") && $("deleteModel").classList.remove("hide");
    } else {
      setStat("modelStatus", "not downloaded", "");
      $("downloadModel") && $("downloadModel").classList.remove("hide");
    }
    modelMissing = !info.exists;
    updateWarnings();
    if (coremlRelevant()) updateCoremlStatus(info.coreml_exists);
  } catch (_) {
    setStat("modelStatus", "—", "");
  }
}

function updateCoremlStatus(exists) {
  if (exists) {
    setStat("coremlStatus", "✓ installed", "installed");
    $("downloadCoreml") && $("downloadCoreml").classList.add("hide");
  } else {
    setStat("coremlStatus", "not downloaded", "");
    $("downloadCoreml") && $("downloadCoreml").classList.remove("hide");
  }
}

async function startModelDownload() {
  if (modelDownloading) return;
  const model = config.offline.model;
  modelDownloading = true;
  modelDlText = "0%";
  const btn = $("downloadModel");
  if (btn) {
    btn.disabled = true;
    btn.textContent = "…";
  }
  setStat("modelStatus", "0%", "downloading");
  try {
    await invoke("download_model", { model });
  } catch (e) {
    modelDownloading = false;
    modelDlText = "";
    setStat("modelStatus", "✗ failed", "error");
    const b = $("downloadModel");
    if (b) {
      b.disabled = false;
      b.textContent = "Download";
    }
    flash(String(e));
  }
}

async function deleteCurrentModel() {
  if (modelDownloading) return;
  const model = config.offline.model;
  if (!window.confirm(`Delete the downloaded “${model}” model? You can re-download it anytime.`)) {
    return;
  }
  const btn = $("deleteModel");
  if (btn) {
    btn.disabled = true;
    btn.textContent = "…";
  }
  try {
    await invoke("delete_model", { model });
    // Rebuild the card so chip install-marks and status refresh from disk.
    rerenderCard();
  } catch (e) {
    flash(String(e));
    const b = $("deleteModel");
    if (b) {
      b.disabled = false;
      b.textContent = "Delete";
    }
  }
}

async function startCoremlDownload() {
  if (coremlDownloading) return;
  const model = config.offline.model;
  coremlDownloading = true;
  const btn = $("downloadCoreml");
  if (btn) {
    btn.disabled = true;
    btn.textContent = "…";
  }
  setStat("coremlStatus", "0%", "downloading");
  try {
    await invoke("download_coreml_bundle", { model });
  } catch (e) {
    coremlDownloading = false;
    setStat("coremlStatus", "✗ failed", "error");
    const b = $("downloadCoreml");
    if (b) {
      b.disabled = false;
      b.textContent = "Download";
    }
    flash(String(e));
  }
}

listen("spoke:download-progress", (e) => {
  if (!modelDownloading) return;
  const { model, percent } = e.payload;
  if (model !== config.offline.model) return;
  modelDlText = `${percent}%`;
  setStat("modelStatus", modelDlText, "downloading");
});

listen("spoke:download-complete", (e) => {
  if (!modelDownloading) return;
  modelDownloading = false;
  modelDlText = "";
  const { model } = e.payload;
  if (model === config.offline.model) {
    setStat("modelStatus", "✓ installed", "installed");
    $("downloadModel") && $("downloadModel").classList.add("hide");
    modelMissing = false;
    updateWarnings();
  }
  const b = $("downloadModel");
  if (b) {
    b.disabled = false;
    b.textContent = "Download";
  }
});

listen("spoke:model-deleted", () => {
  // A model was removed (possibly from the tray); refresh the card if it's open.
  if (menuState !== "closed" && menuState !== "ring") rerenderCard();
});

listen("spoke:coreml-progress", (e) => {
  if (!coremlDownloading) return;
  const { model, percent, phase } = e.payload;
  if (model !== config.offline.model) return;
  setStat("coremlStatus", phase === "extract" ? "unzip…" : `${percent}%`, "downloading");
});

listen("spoke:coreml-complete", (e) => {
  if (!coremlDownloading) return;
  coremlDownloading = false;
  const { model } = e.payload;
  if (model === config.offline.model) updateCoremlStatus(true);
  const b = $("downloadCoreml");
  if (b) {
    b.disabled = false;
    b.textContent = "Download";
  }
});

// ---- Dragging the bubble ------------------------------------------------

let dragOrigin = null;
let didDrag = false;

// Use one event model (Pointer Events) for both drag and toggle. Mixing mouse
// events for drag with pointer events for the toggle desyncs on WebKitGTK
// transparent/unfocused windows: mouseup may not fire, leaving dragOrigin/didDrag
// stale so the toggle's `if (didDrag) return` swallows the close.
bubble.addEventListener("pointerdown", (e) => {
  if (e.button !== 0) return;
  // Stop propagation so the document "close on outside click" handler does not
  // run when the user clicks the bubble itself — toggle is handled below.
  e.stopPropagation();
  dragOrigin = { x: e.screenX, y: e.screenY };
  didDrag = false;
  // Squish the blob while held (see press spring in mosaicStep).
  pressTarget = 1;
  pressT = mosaicT;
});

window.addEventListener("pointermove", (e) => {
  if (!dragOrigin) return;
  const dx = e.screenX - dragOrigin.x;
  const dy = e.screenY - dragOrigin.y;
  if (Math.hypot(dx, dy) > 5) {
    didDrag = true;
    dragOrigin = null;
    bubble.classList.add("dragging");
    appWindow.startDragging();
  }
});

bubble.addEventListener("pointerup", (e) => {
  if (e.button !== 0) return;
  // Always reset drag state on release so it can never go stale.
  dragOrigin = null;
  pressTarget = 0;
  bubble.classList.remove("dragging");
  if (didDrag) {
    didDrag = false;
    return;
  }
  if (menuState === "closed") openRing();
  else closeMenu();
});

// Safety net: if startDragging() swallows the pointerup (WM grabs the gesture),
// clear drag state when the pointer leaves so the next click is never blocked.
window.addEventListener("pointercancel", () => {
  dragOrigin = null;
  didDrag = false;
  pressTarget = 0;
  bubble.classList.remove("dragging");
});

// Release the squish if the pointer slides off the bubble mid-press,
// but not during an active drag (the pointer may leave the small bubble
// element while the window drag is still in flight).
bubble.addEventListener("pointerleave", () => {
  if (!didDrag) pressTarget = 0;
});

// ---- Boot -----------------------------------------------------------------

async function init() {
  try {
    config = await invoke("get_config");
  } catch (e) {
    flash("Failed to load config: " + e);
  }
  Sfx.fromConfig(config);
  buildOrbit();
  await loadBuildInfo();
  updateOrbitValues();
  checkPermissions();
  refreshModelWarning();
  // Permissions can change behind our back (System Settings, TCC resets on
  // rebuild); poll cheaply so warnings appear and clear without a restart.
  setInterval(checkPermissions, 15000);
  setInterval(refreshModelWarning, 15000);
  requestAnimationFrame(tick);
  // Window starts at 320×320; shrink to bubble-only size immediately.
  resizeAndReposition(BUBBLE_W, BUBBLE_H, true);
}

init();
