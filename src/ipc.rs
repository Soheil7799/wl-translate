//! D-Bus interface for the resident daemon.
//!
//! The methods carry no language arguments on purpose. A keybind should say
//! *what to do*, not restate which languages you are working in - the daemon
//! already knows that, and the UI is where you change it:
//!
//!   busctl --user call org.wl_translate.Daemon /org/wl_translate/Daemon \
//!          org.wl_translate.Daemon1 Selection
//!
//! D-Bus rather than a socket so any compositor can drive it without this
//! program's CLI in the loop at all. It is the one part of Crow Translate that
//! still works perfectly under Wayland.

use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;

use crate::pipeline::Verb;
use crate::shot;

pub const SERVICE: &str = "org.wl_translate.Daemon";
pub const PATH: &str = "/org/wl_translate/Daemon";

/// Server side. Each method drops a verb on the channel and returns at once -
/// the caller is a keybind, so it must never block waiting for OCR.
pub struct Iface {
    pub triggers: UnboundedSender<Verb>,
}

#[zbus::interface(name = "org.wl_translate.Daemon1")]
impl Iface {
    /// Drag a region, read the text in it, translate it.
    fn ocr(&self) {
        let _ = self.triggers.send(Verb::Ocr { geometry: None });
    }

    /// Drag a region and extract its text without translating.
    fn ocr_raw(&self) {
        let _ = self.triggers.send(Verb::OcrRaw);
    }

    /// Translate whatever is highlighted with the mouse.
    fn selection(&self) {
        let _ = self.triggers.send(Verb::Selection);
    }

    /// Translate the clipboard.
    fn clipboard(&self) {
        let _ = self.triggers.send(Verb::Clipboard);
    }

    /// Translate text passed directly.
    fn text(&self, text: String) {
        let _ = self.triggers.send(Verb::Text(text));
    }

    /// Take a screenshot and show it for review. Mode is region, window or
    /// screen; anything else is ignored rather than crashing a keybind.
    fn shot(&self, mode: String) {
        match mode.parse::<shot::Mode>() {
            Ok(mode) => {
                let _ = self.triggers.send(Verb::Shot(mode));
            }
            Err(error) => eprintln!("wl-translate: {error}"),
        }
    }

    /// Raise the window without changing what is in it.
    fn show(&self) {
        let _ = self.triggers.send(Verb::Show);
    }
}

#[zbus::proxy(
    interface = "org.wl_translate.Daemon1",
    default_service = "org.wl_translate.Daemon",
    default_path = "/org/wl_translate/Daemon"
)]
pub trait Daemon {
    async fn ocr(&self) -> zbus::Result<()>;
    async fn ocr_raw(&self) -> zbus::Result<()>;
    async fn selection(&self) -> zbus::Result<()>;
    async fn clipboard(&self) -> zbus::Result<()>;
    async fn text(&self, text: &str) -> zbus::Result<()>;
    async fn shot(&self, mode: &str) -> zbus::Result<()>;
    async fn show(&self) -> zbus::Result<()>;
}

/// Hand a verb to the daemon, starting one if there is none.
///
/// The daemon is meant to be resident - it is in the session autostart - so a
/// missing one is a transient state, not a different mode of operation. Without
/// this a keybind silently changes behaviour depending on whether a background
/// process happens to be up, which is the sort of thing that looks like the
/// keybind never got applied at all.
pub fn forward_or_start(verb: &Verb) -> Result<bool> {
    if forward(verb)? {
        return Ok(true);
    }

    let exe = std::env::current_exe().context("could not find our own executable")?;

    std::process::Command::new(exe)
        .arg("daemon")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("could not start the daemon")?;

    // Claiming the bus name takes a moment; poll rather than guess a delay.
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(100));

        if forward(verb)? {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Hand a verb to a running daemon. `Ok(false)` means no daemon is running and
/// the caller should do the work itself.
pub fn forward(verb: &Verb) -> Result<bool> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start a runtime for the D-Bus call")?;

    runtime.block_on(async {
        let Ok(connection) = zbus::Connection::session().await else {
            return Ok(false);
        };

        let Ok(proxy) = DaemonProxy::new(&connection).await else {
            return Ok(false);
        };

        let called = match verb {
            Verb::Ocr { .. } => proxy.ocr().await,
            Verb::OcrRaw => proxy.ocr_raw().await,
            Verb::Selection => proxy.selection().await,
            Verb::Clipboard => proxy.clipboard().await,
            Verb::Text(text) => proxy.text(text).await,
            // Only the overlay produces this, and it already has the daemon it
            // would be forwarded to.
            Verb::OcrImage { .. } => return Ok(false),
            Verb::Shot(mode) => proxy.shot(&mode.to_string()).await,
            Verb::Show => proxy.show().await,
        };

        // A ServiceUnknown error just means the daemon is not running, which is
        // a normal state rather than a failure.
        Ok(called.is_ok())
    })
}
