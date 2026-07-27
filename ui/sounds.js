// UI sound effects, shared by the bubble and the onboarding window.
//
// Playback lives in the webview, not in Rust: the bubble window stays alive
// even in tray mode (it is hidden, not closed), so the record/transcribe cues
// still fire when there is nothing on screen. The master switch is the
// `ui.sounds` config field — mirrored here via Sfx.setEnabled() whenever the
// config is loaded or changes (the tray menu can flip it too).
//
// Why Web Audio and not `new Audio(src)`: in a release build the UI is served
// over the `tauri://` custom scheme, and WebKitGTK's media pipeline can't load
// media from a custom scheme — its player resolves URLs outside the webview's
// URI-scheme handlers, so the element stays silent with no error. (Dev doesn't
// show this: the CLI serves the same files over http://localhost.) `fetch` does
// go through the scheme handler, so the bytes are pulled in JS and decoded into
// AudioBuffers, which play from memory with no URL loading involved. That also
// makes each cue start instantly. A blob-URL `<audio>` fallback covers the case
// where decoding isn't available.
//
// Files live in ui/sounds/. Add one by dropping the mp3 there and adding a
// FILES entry; nothing else needs to know about it.
//
// Note for Linux: WebKitGTK decodes through GStreamer either way, so the
// gst-plugins-base/good packages are a runtime requirement (see BUILD.md).

const Sfx = (() => {
  const FILES = {
    menuOpen: "sounds/menu-open.mp3",
    menuClose: "sounds/menu-close.mp3",
    recordStart: "sounds/record-start.mp3",
    recordStop: "sounds/record-stop.mp3",
    transcribeDone: "sounds/transcribe-done.mp3",
    onboarding: "sounds/onboarding.mp3",
  };

  // Default on: a config that predates the field, or a load failure, should
  // still make noise — the user turns it off explicitly, never by accident.
  let enabled = true;

  const AC = window.AudioContext || window.webkitAudioContext;
  let ctx = null;
  const buffers = {}; // name -> AudioBuffer (primary path)
  const blobs = {}; // name -> HTMLAudioElement over a blob: URL (fallback)

  function context() {
    if (!ctx && AC) {
      try {
        ctx = new AC();
      } catch (e) {
        console.warn("sfx: no AudioContext", e);
      }
    }
    return ctx;
  }

  // Older/odd builds only implement the callback form of decodeAudioData.
  function decode(ac, bytes) {
    return new Promise((resolve, reject) => {
      const p = ac.decodeAudioData(bytes, resolve, reject);
      if (p && typeof p.then === "function") p.then(resolve, reject);
    });
  }

  async function load(name, src) {
    const res = await fetch(src);
    if (!res.ok) throw new Error(`${src}: HTTP ${res.status}`);
    const bytes = await res.arrayBuffer();
    const ac = context();
    if (ac) {
      try {
        buffers[name] = await decode(ac, bytes.slice(0));
        return;
      } catch (e) {
        console.warn("sfx: decode failed, falling back to blob", name, e);
      }
    }
    // Fallback: hand the same bytes to a media element as a blob: URL. Unlike
    // a tauri:// URL, a blob is resolved inside the webview.
    const el = new Audio(URL.createObjectURL(new Blob([bytes], { type: "audio/mpeg" })));
    el.preload = "auto";
    blobs[name] = el;
  }

  // Preloaded up front so the first cue isn't late by a fetch + decode. The
  // whole set is a couple hundred KB off the local asset server.
  const ready = Promise.all(
    Object.entries(FILES).map(([name, src]) =>
      load(name, src).catch((e) => console.warn("sfx: load failed", name, e))
    )
  );

  function play(name) {
    if (!enabled) return;
    const buf = buffers[name];
    if (buf) {
      const ac = context();
      if (!ac) return;
      // A context created before any user interaction can start suspended;
      // resuming is a no-op once it's running.
      if (ac.state === "suspended") ac.resume().catch(() => {});
      const src = ac.createBufferSource();
      src.buffer = buf;
      src.connect(ac.destination);
      src.start();
      return;
    }
    const el = blobs[name];
    if (!el) return;
    // Re-triggering a still-playing cue restarts it rather than overlapping —
    // hammering the hotkey should sound like one app, not a choir.
    try {
      el.currentTime = 0;
    } catch (_) {}
    // Autoplay policies can reject this; a missing sound must never break the
    // action that triggered it, so swallow the rejection.
    const p = el.play();
    if (p && typeof p.catch === "function") {
      p.catch((e) => console.warn("sfx: blocked", name, e));
    }
  }

  return {
    play,
    // Resolves once every clip has been loaded (or failed) — used by callers
    // that fire a cue immediately at startup.
    ready,
    setEnabled(v) {
      enabled = v !== false;
    },
    // Adopt the master switch from a config object (either window's shape).
    fromConfig(config) {
      if (config && config.ui) this.setEnabled(config.ui.sounds);
    },
    get enabled() {
      return enabled;
    },
  };
})();
