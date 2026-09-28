//! `hyprforge-clipd`: watches the compositor's clipboard, records what
//! is worth keeping, and — since this daemon took over `set-clipboard`
//! — is also the thing that keeps a chosen entry pasteable.
//!
//! # This daemon is still the only writer — now other processes can ask
//!
//! This crate used to say there was no IPC here at all: no socket, no
//! D-Bus name, nothing a popup could ask this daemon to do. That is no
//! longer true — [`hyprforge_clipboard::ipc`] runs a control socket at
//! `$XDG_RUNTIME_DIR/clipd.sock` so the popup can ask this daemon to pin,
//! unpin or remove an entry, and list or check on the history it keeps.
//! What has not changed, and is the reason this is safe, is *why* there
//! was no IPC before: two writers racing to save the same index file —
//! the daemon recording a new copy while a popup saved a pin toggle
//! directly — is exactly the kind of thing `write_atomic` cannot make
//! safe on its own (last write wins either way), and the fix was never
//! "add locking to the file", it was "have only one writer at all". This
//! daemon still is that one writer: [`History::save`] is called from
//! exactly two places in this process — the watcher loop below, and the
//! control socket's request handler in `ipc.rs` — and both hold the same
//! `tokio::sync::Mutex<History>` (see [`ipc::Shared`](hyprforge_clipboard::ipc::Shared)) while they touch
//! it, so a pin request and an incoming copy still cannot interleave
//! into two half-applied writes. The popup gained a way to change
//! history state; it did not gain a second path to the file. That request
//! channel is exactly what this module doc used to say would have to
//! exist before the popup could ask for a pin or a remove at all.
//!
//! # This daemon is now also the only thing holding the selection open
//!
//! On Wayland the clipboard selection is served by whichever client set
//! it — the compositor asks *that process* for the bytes on every
//! paste. `hyprforge-clipmenu` is a popup: it appears, the user chooses
//! an entry, and it exits. Setting the selection from the popup and then
//! exiting destroys the very thing serving it, so the clipboard reads as
//! empty the moment the popup is gone — worse than before an entry was
//! chosen, because now there is nothing to paste at all where there used
//! to be whatever was on the clipboard before. `ipc::Request::SetClipboard`
//! is this daemon's answer: it looks the entry up in the history it
//! already holds, calls [`hyprforge_clipboard::ClipboardWriter::set_selection`]
//! itself, and keeps the returned guard in [`ipc::Shared::selection`](hyprforge_clipboard::ipc::Shared::selection) for
//! as long as this process runs, replacing (and thereby dropping) the
//! previous one on every new `set-clipboard`. What this buys: an entry,
//! once chosen, stays pasteable — over and over, from any window — for
//! as long as `hyprforge-clipd` is running, with no popup involved after
//! the choice is made. What it costs: the clipboard is now only as
//! durable as this daemon. If it is killed or crashes, the selection
//! goes with it, exactly as it always has for every other Wayland
//! clipboard manager — there is no way around that on this protocol, only
//! a choice of *which* long-lived process holds it, and this daemon is
//! the one already guaranteed to be running for as long as the history
//! it serves is meaningful at all.
//!
//! # Protocol
//!
//! One JSON object per line over the socket, request → response,
//! modelled on `notif-ipc` (see [`hyprforge_clipboard::ipc`] for the
//! full doc, including why nothing here logs an entry's content):
//!
//! | Request                              | Response                     |
//! |----------------------------------------|-------------------------------|
//! | `{"cmd":"pin","id":"<entry id>"}`       | `{"ok":true}`                 |
//! | `{"cmd":"unpin","id":"<entry id>"}`     | `{"ok":true}`                 |
//! | `{"cmd":"remove","id":"<entry id>"}`    | `{"ok":true}`                 |
//! | `{"cmd":"set-clipboard","id":"<entry id>"}` | `{"ok":true}`             |
//! | `{"cmd":"set-clipboard-text","text":"<s>"}` | `{"ok":true}`             |
//! | `{"cmd":"list"}`                        | `{"ok":true,"entries":[…]}`   |
//! | `{"cmd":"status"}`                      | `{"ok":true,"status":{…}}`    |
//! | unknown / malformed                     | `{"ok":false,"error":"…"}`    |
//!
//! # Startup: a missing history is not the same as a broken one
//!
//! [`History::load`] already draws this line — see its own doc — and
//! this daemon's only job is to not defeat it. A **missing** index is
//! first run: start from empty, and saving is safe. An index that
//! **exists and will not parse** is reported loudly (error level: this
//! is the one case where losing a user's history is one save away) and
//! is never saved over — new copies are still recorded into the
//! in-memory history and this daemon keeps watching, but [`History::save`]
//! is never called until the on-disk file is fixed by hand and this
//! daemon is restarted. Silently discarding the broken file and starting
//! fresh would mean the next save overwrites whatever the user actually
//! had — the exact mistake `hlconfig::storage` and `hyprforge-tray`'s
//! `prefs::load_from` both already avoid.

use hyprforge_clipboard::ipc::Shared;
use hyprforge_clipboard::{
    ClipboardWatcher, Entry, History, Recordable, Sensitivity, WaylandWatcher, WaylandWriter,
};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// How long to wait before trying to (re)connect to the compositor's
/// clipboard protocol, whether the first attempt failed or a previously
/// working connection just died. Short enough that a compositor
/// restarting is followed quickly; long enough not to spin against one
/// that is simply never going to answer.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Default to `info`, the same reasoning `hyprforge-trayd` gives: a
    // daemon with no GUI that prints nothing on start is indistinguishable
    // from a hung one. RUST_LOG still overrides this when set.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let (history, can_save) = load_history();
    tracing::info!(
        entries = history.entries().len(),
        can_save,
        "loaded clipboard history"
    );
    if !can_save {
        // Loud and only once at startup — CLAUDE.md: "the message
        // telling the user to start the service keeps being shown after
        // they do" is the wrong shape of repetition; the wrong shape
        // here would be saying this on every single copy instead.
        tracing::error!(
            "the on-disk clipboard history could not be parsed; new copies will still be \
             recorded in memory and shown to the popup process is not this daemon's job, but \
             NOTHING will be saved to disk until the file is fixed by hand and this daemon is \
             restarted. Losing today's copies would be bad; overwriting a history that failed \
             to parse would be worse."
        );
    }

    // One `History` behind one lock, shared between the watcher loop
    // below and every control-socket connection in `ipc.rs` — see this
    // module's doc on why that lock is what keeps this the only writer.
    // `selection` is a *separate* lock (see `ipc::Shared`'s doc) holding
    // the guard from this daemon's own most recent `set-clipboard` — kept
    // alive for as long as this process runs, which is the entire point.
    let shared = Arc::new(Shared {
        history: tokio::sync::Mutex::new(history),
        can_save,
        writer: WaylandWriter::new(),
        selection: tokio::sync::Mutex::new(None),
    });

    let ipc_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        if let Err(e) = hyprforge_clipboard::ipc::run(ipc_shared).await {
            // Fatal only for the socket, not for this daemon: a clipboard
            // manager that can still record copies but cannot take pin
            // requests is in a worse state than before, not a state
            // worth exiting the whole process over. The popup will see
            // "daemon not running" for its own connection attempts,
            // which is the client's problem to report — see CLAUDE.md.
            tracing::error!(error = %e, "clipboard control socket is not running");
        }
    });

    loop {
        match WaylandWatcher::connect() {
            Ok(watcher) => {
                tracing::info!("connected to the compositor's clipboard");
                run_until_disconnected(&watcher, &shared).await;
                tracing::warn!(
                    "clipboard watcher stopped delivering entries; the compositor connection \
                     likely died. Reconnecting."
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not connect to the compositor's clipboard; will retry");
            }
        }
        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

/// Drains the watcher's channel until it closes (the dispatch thread on
/// the other end died — see `wayland::ext`/`wayland::wlr`'s `Finished`
/// handling), recording each entry as it arrives.
async fn run_until_disconnected(watcher: &WaylandWatcher, shared: &Shared<WaylandWriter>) {
    let mut entries = watcher.subscribe();
    while let Some(entry) = entries.recv().await {
        // Never log the content — only its id (a content hash, not the
        // content) and its size, per CLAUDE.md.
        let id = entry.id.clone();
        let size = entry.content.size();
        let now = now_unix();

        // Same lock the control socket takes around a request — see the
        // module doc. Held only across the record-and-maybe-save pair
        // below, never across the `.recv().await` above, so a pin
        // request is never blocked on the next clipboard copy arriving.
        let mut history = shared.history.lock().await;
        if record(&mut history, entry, now) {
            tracing::info!(id = ?id, size, "recorded a clipboard entry");
        } else {
            // `record`/`History::record` refused it (empty, or over the
            // single-entry byte cap) — not an error, just nothing to
            // keep.
            tracing::debug!(id = ?id, size, "clipboard entry was not recorded");
            continue;
        }

        if shared.can_save {
            if let Err(e) = history.save() {
                tracing::error!(error = %e, "failed to save clipboard history; will retry on the next copy");
            }
        }
        // When `!can_save`, saving is deliberately skipped — see the
        // module doc. Whether saving is safe is decided once, at
        // startup, from whether the on-disk file parsed; fixing it
        // requires a restart of this daemon, not a change that could
        // happen mid-run.
    }
}

/// Turns a watcher's `Entry` into a history append.
///
/// Always classifies as [`Sensitivity::Recordable`] — there is nothing
/// else it could be. [`ClipboardWatcher::subscribe`]'s own contract (see
/// `backend.rs`) is that nothing secret is ever read to find out, so
/// nothing secret ever reaches this channel in the first place; an
/// `Entry` does not even carry a `Sensitivity` to re-check. Documented
/// here rather than left implicit, because this is the one place in the
/// daemon where a password manager's copy either does or does not end up
/// in a file on disk.
fn record(history: &mut History, entry: Entry, now: u64) -> bool {
    let Some(recordable) = Recordable::new(entry.content, Sensitivity::Recordable) else {
        unreachable!(
            "Recordable::new only refuses Sensitivity::Secret, which is never passed here"
        );
    };
    history.record(recordable, now)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Loads the default on-disk history, returning it together with
/// whether saving over it is safe — see the module doc.
fn load_history() -> (History, bool) {
    load_history_from(
        &hyprforge_paths::clipboard_index_path(),
        &hyprforge_paths::clipboard_images_dir(),
    )
}

fn load_history_from(index_path: &Path, images_dir: &Path) -> (History, bool) {
    match History::load_from(index_path, images_dir) {
        Ok(history) => (history, true),
        Err(e) => {
            tracing::error!(
                path = %index_path.display(),
                error = %e,
                "clipboard history exists but could not be parsed"
            );
            (History::new(), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_clipboard::{Content, EntryId};

    fn entry(text: &str) -> Entry {
        let content = Content::Text(text.to_string());
        Entry {
            id: EntryId::of(&content),
            content,
            copied_at: 0,
            pinned: false,
        }
    }

    /// The property Part 3 of the task exists to pin: a normal copy is
    /// recorded.
    #[test]
    fn the_daemon_records_a_normal_entry() {
        let mut history = History::new();
        assert!(record(&mut history, entry("hello"), 1));
        assert_eq!(history.entries().len(), 1);
        assert_eq!(history.entries()[0].content, Content::Text("hello".into()));
    }

    /// The other half: nothing this daemon does can turn a secret into a
    /// recordable entry, regardless of what any future caller passes —
    /// pinned here even though `record` itself has no branch for it,
    /// because that absence *is* the guarantee (see `record`'s doc).
    #[test]
    fn a_secret_can_never_be_recorded_through_this_daemons_own_constructor() {
        assert!(Recordable::new(Content::Text("hunter2".into()), Sensitivity::Secret).is_none());
    }

    /// An empty or whitespace-only copy is refused by `History::record`
    /// itself (see `store.rs`) — `record` here must not paper over that
    /// with `unreachable!` or a panic.
    #[test]
    fn a_blank_copy_is_not_recorded_and_does_not_panic() {
        let mut history = History::new();
        assert!(!record(&mut history, entry("   \n\t"), 1));
        assert!(history.entries().is_empty());
    }

    /// A missing index is first run: safe to save, starts empty.
    #[test]
    fn a_missing_index_yields_an_empty_savable_history() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("history.toml");
        let images = dir.path().join("images");
        let (history, can_save) = load_history_from(&index, &images);
        assert!(history.entries().is_empty());
        assert!(can_save);
    }

    /// The property Part 3's hardest requirement pins: a history that
    /// exists and will not parse must never be saved over, and the
    /// daemon must be able to keep going (an empty in-memory history,
    /// not a panic) rather than treating it as fatal.
    #[test]
    fn an_unreadable_history_is_not_overwritten_and_the_daemon_can_keep_going() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("history.toml");
        let images = dir.path().join("images");
        let broken = "entries = not a list at all\n";
        std::fs::write(&index, broken).unwrap();

        let (history, can_save) = load_history_from(&index, &images);
        assert!(
            history.entries().is_empty(),
            "the daemon must still have something usable to watch and record into"
        );
        assert!(
            !can_save,
            "a history that failed to parse must never be saved over"
        );

        // Simulate what the daemon's own loop would do next: record a
        // new copy, and — because `can_save` is false — never call
        // `save`. The broken file on disk must be untouched afterward.
        let mut history = history;
        assert!(record(&mut history, entry("new copy"), 1));
        if can_save {
            history.save_to(&index, &images).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&index).unwrap(),
            broken,
            "the broken file must survive exactly as it was found"
        );
    }
}
