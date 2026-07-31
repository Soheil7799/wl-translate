//! D-Bus interface for the resident daemon.
//!
//! D-Bus rather than a unix socket so the verbs are bindable from any
//! compositor without this program's CLI in the loop at all:
//!
//!   busctl --user call org.wl_translate.Daemon /org/wl_translate/Daemon \
//!          org.wl_translate.Daemon1 Selection s en
//!
//! That is the same mechanism Crow Translate uses, and the one part of Crow
//! that still works perfectly under Wayland.

use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;

use crate::pipeline::{Job, Verb};

pub const SERVICE: &str = "org.wl_translate.Daemon";
pub const PATH: &str = "/org/wl_translate/Daemon";

/// Server side. Each method drops a job on the channel and returns immediately -
/// the caller is a keybind, so it must never block waiting for OCR to finish.
pub struct Iface {
    pub jobs: UnboundedSender<Job>,
}

impl Iface {
    fn submit(&self, verb: Verb, to: String, raw: bool) {
        let mut job = Job::new(verb);
        job.to = to;
        job.raw = raw;

        // The only failure here is a dead worker thread, in which case the
        // daemon is finished anyway and there is nothing useful to report.
        let _ = self.jobs.send(job);
    }
}

#[zbus::interface(name = "org.wl_translate.Daemon1")]
impl Iface {
    /// Drag a region, read the text in it, translate it.
    fn ocr(&self, to: String) {
        self.submit(Verb::Ocr { geometry: None }, to, false);
    }

    /// Drag a region and extract its text without translating.
    fn ocr_raw(&self, to: String) {
        self.submit(Verb::Ocr { geometry: None }, to, true);
    }

    /// Translate whatever is highlighted with the mouse.
    fn selection(&self, to: String) {
        self.submit(Verb::Selection, to, false);
    }

    /// Translate the clipboard.
    fn clipboard(&self, to: String) {
        self.submit(Verb::Clipboard, to, false);
    }

    /// Translate text passed directly.
    fn text(&self, text: String, to: String) {
        self.submit(Verb::Text(text), to, false);
    }
}

#[zbus::proxy(
    interface = "org.wl_translate.Daemon1",
    default_service = "org.wl_translate.Daemon",
    default_path = "/org/wl_translate/Daemon"
)]
pub trait Daemon {
    async fn ocr(&self, to: &str) -> zbus::Result<()>;
    async fn ocr_raw(&self, to: &str) -> zbus::Result<()>;
    async fn selection(&self, to: &str) -> zbus::Result<()>;
    async fn clipboard(&self, to: &str) -> zbus::Result<()>;
    async fn text(&self, text: &str, to: &str) -> zbus::Result<()>;
}

/// Hand a job to a running daemon. `Ok(false)` means no daemon is running and
/// the caller should do the work itself.
pub fn forward(verb: &Verb, to: &str, raw: bool) -> Result<bool> {
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
            Verb::Ocr { .. } if raw => proxy.ocr_raw(to).await,
            Verb::Ocr { .. } => proxy.ocr(to).await,
            Verb::Selection => proxy.selection(to).await,
            Verb::Clipboard => proxy.clipboard(to).await,
            Verb::Text(text) => proxy.text(text, to).await,
        };

        // A ServiceUnknown error just means the daemon is not running, which is
        // a normal state rather than a failure.
        Ok(called.is_ok())
    })
}
