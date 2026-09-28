//! The real backend: watches the compositor's clipboard over
//! `ext-data-control-v1`, falling back to `zwlr-data-control-v1`.
//!
//! Two protocols do this. `ext-data-control-v1` (in `wayland-protocols`,
//! still under `staging` — it has not graduated to stable) is the
//! standardised successor; `zwlr-data-control-v1` (in
//! `wayland-protocols-wlr`) is the older wlroots-only one its own XML
//! now calls deprecated. Hyprland 0.56 on this machine answers
//! `wl-paste --list-types` over data-control, which the `ext` module's
//! doc comment records as the check that was actually run. A compositor
//! that only ever shipped the older protocol still needs to work, so
//! [`connect`](WaylandWatcher::connect) tries `ext` first and falls back to `wlr` rather than
//! choosing one at compile time.
//!
//! `ext.rs` and `wlr.rs` are close to line-for-line copies of each
//! other: the two protocols share requests, events and argument order,
//! and `wayland-client`'s generated types make each an entirely
//! different Rust type even though nothing about the *logic* differs.
//! What is shared lives in `pipe.rs` (the bounded read) and
//! `crate::resolve` (the pure selection and the sensitivity gate) —
//! everything outside those is unavoidably protocol-specific glue.

mod ext;
mod keyboard;
mod pipe;
mod read_ext;
mod wlr;
mod write_ext;
mod write_wlr;

use crate::backend::ClipboardWatcher;
use crate::types::{Content, Entry, Mime};
use crate::write::{ClipboardWriter, Offers, Waiter};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

pub use keyboard::WaylandPaster;

/// The real clipboard watcher: a compositor connection running on its
/// own thread, publishing every recordable copy to whoever subscribed.
pub struct WaylandWatcher {
    subscribers: Arc<Mutex<Vec<UnboundedSender<Entry>>>>,
}

impl WaylandWatcher {
    /// Connects to the compositor on `$WAYLAND_DISPLAY`, preferring
    /// `ext-data-control-v1` and falling back to
    /// `zwlr-data-control-v1`, and spawns the dispatch thread.
    ///
    /// Fails only when *neither* protocol is available — callers should
    /// surface this as a clear, actionable startup error (a compositor
    /// with no clipboard protocol at all is not something to retry
    /// silently against), matching `hyprforge-displayd::backend::wlr`'s
    /// `WlrBackend::connect`.
    pub fn connect() -> anyhow::Result<Self> {
        let subscribers = Arc::new(Mutex::new(Vec::new()));

        match ext::connect(subscribers.clone()) {
            Ok(()) => return Ok(WaylandWatcher { subscribers }),
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "ext-data-control-v1 unavailable; falling back to zwlr-data-control-v1"
                );
            }
        }

        wlr::connect(subscribers.clone())
            .map_err(|e| anyhow::anyhow!("no clipboard protocol available (tried ext-data-control-v1, then zwlr-data-control-v1): {e}"))?;
        Ok(WaylandWatcher { subscribers })
    }
}

impl ClipboardWatcher for WaylandWatcher {
    fn subscribe(&self) -> UnboundedReceiver<Entry> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }
}

/// The real clipboard writer: puts content on the selection, preferring
/// `ext-data-control-v1` and falling back to `zwlr-data-control-v1`,
/// exactly like [`WaylandWatcher::connect`].
///
/// Unlike the watcher, there is no persistent connection to hold here:
/// each [`ClipboardWriter::set_selection`] call opens its own short-lived
/// connection and hands its dispatch loop to a background thread that
/// keeps the new data source alive for as long as it remains the
/// selection — see `write_ext`'s module doc for why. This mirrors
/// `hyprforge-tray`'s `TrayIcon::register`, which opens its own
/// connection per registration for the same reason: the alternative is
/// routing every write back through the read side's single dispatch
/// thread, which knows nothing about sources or `send` events today.
pub struct WaylandWriter;

impl WaylandWriter {
    pub fn new() -> Self {
        WaylandWriter
    }
}

impl WaylandWriter {
    /// Makes exactly these types and bytes the clipboard's content —
    /// [`ClipboardWriter::set_selection`] for callers whose content is
    /// not text or an image. Same `ext`-then-`wlr` fallback.
    ///
    /// An empty [`Offers`] leaves a selection that offers nothing, which
    /// is how a caller empties the clipboard it owns.
    pub fn set_offers(&self, offers: Offers) -> anyhow::Result<Arc<Waiter>> {
        match write_ext::set_offers(offers.clone()) {
            Ok(waiter) => Ok(waiter),
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "ext-data-control-v1 unavailable for writing; falling back to zwlr-data-control-v1"
                );
                write_wlr::set_offers(offers).map_err(|e| {
                    anyhow::anyhow!(
                        "no clipboard protocol available to write to (tried ext-data-control-v1, then zwlr-data-control-v1): {e}"
                    )
                })
            }
        }
    }
}

/// The current selection in the first of `preferred` it offers, read
/// once — or `Ok(None)` when the clipboard is empty or offers none of
/// them.
///
/// Bounded by `timeout` as a whole, round trips included: the
/// compositor is another process, and `roundtrip` has no timeout of its
/// own. The work runs on its own thread, which is left behind if the
/// bound is hit — the same trade `pipe::receive` makes and documents.
///
/// `ext-data-control-v1` only; see `read_ext`'s module doc for what a
/// `wlr`-only compositor gets instead.
pub fn read_selection(
    preferred: Vec<Mime>,
    timeout: std::time::Duration,
) -> anyhow::Result<Option<(Mime, Vec<u8>)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("hyprforge-clip-read-once".to_string())
        .spawn(move || {
            let _ = tx.send(read_ext::read(&preferred));
        })?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("the compositor did not answer a clipboard read in time")),
    }
}

impl Default for WaylandWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardWriter for WaylandWriter {
    type Guard = Arc<Waiter>;

    fn set_selection(&self, content: Content) -> anyhow::Result<Self::Guard> {
        match write_ext::set_selection(content.clone()) {
            Ok(waiter) => Ok(waiter),
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "ext-data-control-v1 unavailable for writing; falling back to zwlr-data-control-v1"
                );
                write_wlr::set_selection(content).map_err(|e| {
                    anyhow::anyhow!(
                        "no clipboard protocol available to write to (tried ext-data-control-v1, then zwlr-data-control-v1): {e}"
                    )
                })
            }
        }
    }
}
