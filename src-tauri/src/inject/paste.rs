//! Linux text injection: put the transcript on the clipboard, paste it, put
//! the clipboard back.
//!
//! # Why not synthesize the keystrokes
//!
//! Typing a transcript out character by character is the obvious approach and
//! it is what enigo, libxdo and `xdotool type` do, but on Linux it is a bad
//! trade. Any character the active keyboard layout cannot produce has to be
//! bound to a spare keycode first, and on a KWin/Wayland session that never
//! works: XTEST events reach an X server (XWayland) which the compositor then
//! re-injects into the Wayland session, translating keycodes through *its own*
//! keymap, so the X server's keymap is never consulted. The inter-key delay
//! those backends carry is largely a settle window for that remapping, which
//! is why removing it produced wrong characters and stuck modifiers — and why
//! keeping it bought no correctness, only seconds of latency. Measured here,
//! enigo needed 9 seconds for a 1700-character transcript and still corrupted
//! it.
//!
//! Pasting sidesteps all of it. It is one keystroke rather than thousands, so
//! there is no layout to resolve, no modifier pacing, and the cost does not
//! grow with the length of the transcript. It is also what the established
//! dictation apps do: Wispr Flow's own documentation describes insertion as a
//! paste, and its unofficial Linux port pairs a clipboard write with a
//! synthetic paste keystroke the same way.
//!
//! # Shift+Insert, not Ctrl+V
//!
//! Terminals do not accept Ctrl+V — it is Ctrl+Shift+V there — and GUI toolkits
//! do not accept Ctrl+Shift+V. Detecting which kind of window has focus is not
//! reliable on Wayland, where a native client has no X window to inspect.
//! Shift+Insert is the one binding both honour, and it reads the clipboard
//! rather than the primary selection in each. Verified on this stack against
//! Konsole, a GTK entry, and Kate.
//!
//! # The clipboard is borrowed, not taken
//!
//! The previous contents are read back before the transcript goes on, and
//! restored afterwards, so dictating does not cost the user whatever they had
//! copied. Two limits are worth knowing: a clipboard holding something that is
//! neither text nor an image (copied files, say) cannot be captured, and is
//! cleared rather than left holding the transcript; and a clipboard manager
//! such as KDE's Klipper will record the transcript in its history, which no
//! amount of restoring can undo.

use anyhow::{anyhow, Result};
use arboard::{Clipboard, ImageData};
use std::thread::sleep;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::wrapper::ConnectionExt as _;

/// How long to let the clipboard settle before pressing paste, so the
/// compositor and any clipboard manager have registered the new owner. This
/// is the only part of the delay the user can perceive — the text appears
/// once the keystroke lands.
const BEFORE_PASTE: Duration = Duration::from_millis(40);

/// How long to leave the transcript on the clipboard afterwards.
///
/// The pasting application fetches the data from us asynchronously, so
/// restoring too eagerly is a race that hands it the *old* contents instead.
/// Paste and restore were both reliable over repeated runs at half these
/// figures; the headroom is for a machine under load.
const AFTER_PASTE: Duration = Duration::from_millis(150);

/// Paced like this because a modifier released in the same instant as the key
/// before it is dropped often enough to matter on KWin's re-injection path.
const MODIFIER_RELEASE_MS: u32 = 5;

const XK_SHIFT_L: u32 = 0xffe1;
const XK_INSERT: u32 = 0xff63;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_V: u32 = 0x76;

/// Whatever was on the clipboard before we borrowed it.
enum Saved {
    Text(String),
    Image(ImageData<'static>),
    /// Empty, or holding something we cannot round-trip.
    Unknown,
}

impl Saved {
    fn capture(clipboard: &mut Clipboard) -> Self {
        if let Ok(text) = clipboard.get_text() {
            return Self::Text(text);
        }
        if let Ok(image) = clipboard.get_image() {
            return Self::Image(image);
        }
        Self::Unknown
    }

    fn restore(self, clipboard: &mut Clipboard) {
        let restored = match self {
            Self::Text(text) => clipboard.set_text(text),
            Self::Image(image) => clipboard.set_image(image),
            // Better to leave nothing behind than to leave the transcript.
            Self::Unknown => clipboard.clear(),
        };
        if let Err(e) = restored {
            eprintln!("[inject] could not restore the previous clipboard: {e}");
        }
    }
}

/// Find the keycode that carries `keysym` on the current layout.
fn keycode_for(conn: &impl Connection, keysym: u32) -> Option<u8> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let mapping = conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
    let per = mapping.keysyms_per_keycode as usize;
    (min..=max).find(|keycode| {
        let row = (keycode - min) as usize * per;
        mapping.keysyms[row..row + per].contains(&keysym)
    })
}

/// Press the paste shortcut in whatever window has focus.
fn press_paste() -> Result<()> {
    let (conn, screen) = x11rb::connect(None).map_err(|e| {
        anyhow!("failed to connect to the X server (Spoke pastes through X11/XWayland): {e}")
    })?;
    let root = conn.setup().roots[screen].root;

    // Shift+Insert everywhere it exists. A layout without an Insert key at all
    // is unusual but cheap to fall back from, and Ctrl+V at least covers the
    // GUI applications.
    let (modifier, key) = match keycode_for(&conn, XK_INSERT) {
        Some(insert) => (
            keycode_for(&conn, XK_SHIFT_L)
                .ok_or_else(|| anyhow!("the keyboard layout has no Shift key"))?,
            insert,
        ),
        None => (
            keycode_for(&conn, XK_CONTROL_L)
                .ok_or_else(|| anyhow!("the keyboard layout has no Control key"))?,
            keycode_for(&conn, XK_V).ok_or_else(|| anyhow!("the keyboard layout has no V key"))?,
        ),
    };

    conn.xtest_fake_input(KEY_PRESS_EVENT, modifier, 0, root, 0, 0, 0)?;
    conn.xtest_fake_input(KEY_PRESS_EVENT, key, 0, root, 0, 0, 0)?;
    conn.xtest_fake_input(KEY_RELEASE_EVENT, key, 0, root, 0, 0, 0)?;
    conn.xtest_fake_input(
        KEY_RELEASE_EVENT,
        modifier,
        MODIFIER_RELEASE_MS,
        root,
        0,
        0,
        0,
    )?;
    conn.sync()?;
    Ok(())
}

/// Paste `text` into whatever window has focus, leaving the clipboard as it
/// was found.
pub fn inject_text(text: &str) -> Result<()> {
    let mut clipboard =
        Clipboard::new().map_err(|e| anyhow!("failed to open the clipboard: {e}"))?;
    let saved = Saved::capture(&mut clipboard);

    clipboard
        .set_text(text.to_owned())
        .map_err(|e| anyhow!("failed to put the transcript on the clipboard: {e}"))?;
    sleep(BEFORE_PASTE);

    let pasted = press_paste();

    // The clipboard goes back whether or not the keystroke landed; the user
    // losing what they had copied is the worse outcome of the two.
    sleep(AFTER_PASTE);
    saved.restore(&mut clipboard);
    pasted
}
