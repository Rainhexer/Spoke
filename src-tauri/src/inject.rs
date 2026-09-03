//! Text injection into the focused window.
//!
//! macOS and Windows synthesize keystrokes through enigo. Linux pastes
//! instead, for the reasons in [`paste`].

use anyhow::Result;

#[cfg(target_os = "linux")]
mod paste;

/// Put `text` into whatever window currently has focus. No-op for empty input.
pub fn inject_text(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    return paste::inject_text(text);
    #[cfg(not(target_os = "linux"))]
    return enigo_inject(text);
}

/// `Enigo` is not `Send`, so it must be created and used within a single
/// thread. Construct it inside the call (e.g. from `spawn_blocking`) and let it
/// drop before the closure returns.
#[cfg(not(target_os = "linux"))]
fn enigo_inject(text: &str) -> Result<()> {
    use anyhow::anyhow;
    use enigo::{Enigo, Keyboard, Settings};

    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| anyhow!("failed to init keyboard simulation: {e}"))?;
    enigo
        .text(text)
        .map_err(|e| anyhow!("failed to inject text: {e}"))?;
    Ok(())
}
