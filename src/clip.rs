//! Clipboard access.
//!
//! Shells out to wl-clipboard rather than binding a crate, for one specific
//! reason: wl-copy/wl-paste use the wlr-data-control protocol, which is the only
//! way to touch the clipboard from a process that does NOT hold keyboard focus.
//! A toolkit clipboard API cannot read the selection from a background daemon -
//! that is exactly the wall Crow Translate hits on Wayland.

use anyhow::{ensure, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

pub fn copy(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .spawn()
        .context("could not run `wl-copy` - is wl-clipboard installed?")?;

    child
        .stdin
        .as_mut()
        .context("wl-copy stdin unavailable")?
        .write_all(text.as_bytes())?;

    let status = child.wait()?;
    ensure!(status.success(), "wl-copy exited with {status}");
    Ok(())
}

/// Read the primary selection - the text the user has highlighted with the
/// mouse, no Ctrl+C needed.
pub fn primary() -> Result<String> {
    read(&["--primary", "--no-newline"])
}

pub fn clipboard() -> Result<String> {
    read(&["--no-newline"])
}

fn read(args: &[&str]) -> Result<String> {
    let out = Command::new("wl-paste")
        .args(args)
        .output()
        .context("could not run `wl-paste` - is wl-clipboard installed?")?;

    // wl-paste exits non-zero when the selection is simply empty.
    if !out.status.success() {
        return Ok(String::new());
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
