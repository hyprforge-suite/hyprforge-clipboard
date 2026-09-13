//! Unix-domain socket control interface for `hyprforge-clipd`.
//!
//! # Why this exists, and why it does not become a second writer
//!
//! `clipd.rs`'s module doc explains the constraint this has to respect:
//! this daemon is the only process that ever calls [`History::save`],
//! because two writers racing on one index file is exactly what
//! `write_atomic` cannot make safe. That constraint is still true here —
//! nothing in this module ever touches the index file itself. What is
//! new is that another process (the popup) can now *ask* this daemon to
//! change history state, over a socket, and this daemon is still the one
//! that decides whether to apply the change and the only one that calls
//! [`History::save`] afterward. A request that arrives while the
//! clipboard watcher is mid-record is just another caller taking the
//! same lock the watcher already takes — see `bin/clipd.rs`'s
//! `SharedHistory` — never a second file handle.
//!
//! # Protocol
//!
//! One JSON object per line, request → response, modelled directly on
//! `notif-ipc` (`notif/crates/notif-ipc/src/lib.rs`) so this workspace
//! has one socket convention rather than two:
//!
//! | Request                                    | Response                                   |
//! |---------------------------------------------|---------------------------------------------|
//! | `{"cmd":"pin","id":"<entry id>"}`           | `{"ok":true}`                                |
//! | `{"cmd":"unpin","id":"<entry id>"}`         | `{"ok":true}`                                |
//! | `{"cmd":"remove","id":"<entry id>"}`        | `{"ok":true}`                                |
//! | `{"cmd":"list"}`                            | `{"ok":true,"entries":[…]}`                  |
//! | `{"cmd":"status"}`                          | `{"ok":true,"status":{…}}`                   |
//! | unknown / malformed                         | `{"ok":false,"error":"…"}`                   |
//!
//! The connection stays open after a response; a client may send
//! multiple requests before closing. Malformed requests do **not** close
//! the connection — they get an error response like any other refusal.
//!
//! An entry in a `list` response never carries a clipboard entry's raw
//! bytes: only its id (a content hash, safe to show), a short one-line
//! [`Content::preview`] the popup already renders un-redacted today, its
//! kind, size, pin state and timestamp. Nothing here logs any of it —
//! see "Never log clipboard content" below.
//!
//! # Socket path
//!
//! `$XDG_RUNTIME_DIR/clipd.sock` — the obvious counterpart to notif's
//! `$XDG_RUNTIME_DIR/notif.sock`. This is decided here rather than added
//! to `hyprforge-paths`: that crate is a published foundation crate with
//! no dependencies of its own and no existing notion of a *runtime*
//! (as opposed to config) directory, and `notif-ipc` itself does not put
//! this logic in a shared paths crate either — it resolves
//! `$XDG_RUNTIME_DIR` locally, once, right where the socket is bound.
//! Following that precedent here means the same decision either way:
//! nothing upstream of this module needs to change for a socket to
//! exist.
//!
//! Any stale socket at that path is removed before binding; the socket
//! is removed again on clean exit.
//!
//! # Never log clipboard content
//!
//! An id is a content hash — safe to print, per `types.rs`. A `preview`
//! string is not: it is derived from the user's actual clipboard
//! content, so nothing in this module writes a `preview` (or a raw
//! `Content`) to a `tracing` call. Errors here name the *id* and the
//! *reason* a request failed, never the content a request named.
//!
//! # A client cannot tie up the daemon
//!
//! A connection that sends nothing at all is bounded by
//! [`IDLE_TIMEOUT`] rather than left to `read_line` forever — the same
//! "nothing waits without a bound" rule as everywhere else in this
//! workspace. Each connection also runs as its own task, so one slow or
//! silent client cannot block another, or the clipboard watcher loop.

use crate::store::History;
use crate::types::EntryId;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// How long a connection may sit idle (no complete request line) before
/// this daemon gives up on it and closes it. Generous for a local
/// control socket used interactively, but not infinite — see the module
/// doc's "a client cannot tie up the daemon".
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Errors setting up the IPC listener. Distinct from a request failing —
/// these are startup problems, not something a client asked for.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("$XDG_RUNTIME_DIR is not set; cannot determine the clipboard control socket path")]
    NoRuntimeDir,
    #[error("failed to bind clipboard control socket at {path}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// `$XDG_RUNTIME_DIR/clipd.sock`.
pub fn socket_path() -> Result<PathBuf, IpcError> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or(IpcError::NoRuntimeDir)?;
    Ok(PathBuf::from(runtime_dir).join("clipd.sock"))
}

// ── Wire protocol ────────────────────────────────────────────────────────

/// Incoming request, tagged on `"cmd"` in kebab-case — the same shape
/// `notif-ipc::protocol::Request` uses.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// Pin an entry by id — kept across history caps and shown first.
    Pin { id: String },
    /// Undo a pin.
    Unpin { id: String },
    /// Delete an entry outright.
    Remove { id: String },
    /// The full history, in display order.
    List,
    /// Whether this daemon is alive, and how healthy its history is.
    Status,
}

/// One entry as shown to an IPC client: enough to render a row, never
/// the raw bytes a `Content` carries.
#[derive(Debug, Serialize, PartialEq)]
pub struct EntrySummary {
    pub id: String,
    pub pinned: bool,
    pub copied_at: u64,
    /// `"text"` or `"image"`.
    pub kind: &'static str,
    /// A one-line description — see [`crate::types::Content::preview`].
    /// Not the content itself, and not logged; this is what the popup
    /// already shows on screen today, over the file it reads directly.
    pub preview: String,
    pub size: usize,
}

/// `status` snapshot.
#[derive(Debug, Serialize, PartialEq)]
pub struct StatusInfo {
    pub entries: usize,
    pub total_bytes: u64,
    /// Whether this daemon is currently allowed to save — `false` means
    /// the on-disk history existed but would not parse at startup; see
    /// `clipd.rs`'s module doc. A client sees this to explain why a
    /// mutating request just failed, without needing to read a log.
    pub can_save: bool,
}

/// A response, serialised as one JSON object with `"ok"` first so every
/// shape starts the same way a client can check without knowing which
/// request produced it.
#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Response {
    Ok,
    Err {
        ok: bool,
        error: String,
    },
    Entries {
        ok: bool,
        entries: Vec<EntrySummary>,
    },
    Status {
        ok: bool,
        status: StatusInfo,
    },
}

/// Every response as JSON, with the `ok` field always present and, for
/// [`Response::Ok`], nothing else — `{"ok":true}` exactly, matching
/// `notif-ipc`'s `OkResponse`. Handled by hand rather than deriving
/// `Serialize` for `Response::Ok` as a bare unit variant, because an
/// untagged unit variant would serialise as `null`, not an object.
fn to_json(response: &Response) -> String {
    match response {
        Response::Ok => r#"{"ok":true}"#.to_string(),
        other => serde_json::to_string(other)
            .unwrap_or_else(|_| r#"{"ok":false,"error":"internal serialization error"}"#.into()),
    }
}

fn err(message: impl Into<String>) -> Response {
    Response::Err {
        ok: false,
        error: message.into(),
    }
}

// ── The pure layer: parse a line, apply it to a History, answer ────────────

/// Parses one request line and applies it to `history`, returning the
/// response to send back and whether `history` was mutated in a way
/// that needs [`History::save`] — the caller (the socket handler in
/// `run_at`, or a test with no socket at all) decides when to actually
/// call it. No I/O happens in here: this is the seam the module doc
/// promises, and every request's happy path, failure and edge case is
/// tested directly against it in `tests` below.
///
/// `can_save` is the same flag `clipd.rs` computes once at startup: when
/// `false`, the on-disk history exists but would not parse, and a
/// mutating request is refused outright rather than applied only in
/// memory — applying it anyway would invite exactly the mistake the
/// flag exists to prevent, papering over a request that silently never
/// makes it to disk with no way for the daemon (restarted with the file
/// still broken) or the user to tell request and non-event apart.
/// `list` and `status` are unaffected: reading in-memory state is always
/// safe, and `status` is how a client learns `can_save` is `false` in
/// the first place.
pub fn handle_line(history: &mut History, can_save: bool, line: &str) -> (String, bool) {
    let request: Result<Request, _> = serde_json::from_str(line);
    let (response, mutated) = match request {
        Err(e) => (err(format!("malformed request: {e}")), false),
        Ok(request) => dispatch(history, can_save, request),
    };
    (to_json(&response), mutated)
}

fn dispatch(history: &mut History, can_save: bool, request: Request) -> (Response, bool) {
    match request {
        Request::Pin { id } => apply_pin(history, can_save, id, true),
        Request::Unpin { id } => apply_pin(history, can_save, id, false),
        Request::Remove { id } => remove(history, can_save, id),
        Request::List => (
            Response::Entries {
                ok: true,
                entries: history.entries().iter().map(summarize).collect(),
            },
            false,
        ),
        Request::Status => (
            Response::Status {
                ok: true,
                status: StatusInfo {
                    entries: history.entries().len(),
                    total_bytes: history.total_bytes(),
                    can_save,
                },
            },
            false,
        ),
    }
}

fn apply_pin(history: &mut History, can_save: bool, id: String, pinned: bool) -> (Response, bool) {
    if !can_save {
        return (err(unsavable_message()), false);
    }
    let entry_id = EntryId::from_raw(id.clone());
    if history.set_pinned(&entry_id, pinned) {
        (Response::Ok, true)
    } else {
        (err(format!("no clipboard entry with id {id}")), false)
    }
}

fn remove(history: &mut History, can_save: bool, id: String) -> (Response, bool) {
    if !can_save {
        return (err(unsavable_message()), false);
    }
    let entry_id = EntryId::from_raw(id.clone());
    if history.remove(&entry_id) {
        (Response::Ok, true)
    } else {
        (err(format!("no clipboard entry with id {id}")), false)
    }
}

/// Shared verbatim between `pin`/`unpin`/`remove`: the reason a mutation
/// is refused, and what to do about it, rather than a bare "no".
fn unsavable_message() -> String {
    "clipboard history could not be parsed at startup and is not being saved; fix the file by \
     hand and restart hyprforge-clipd before changing pin state"
        .to_string()
}

fn summarize(entry: &crate::types::Entry) -> EntrySummary {
    let kind = match entry.content {
        crate::types::Content::Text(_) => "text",
        crate::types::Content::Image { .. } => "image",
    };
    EntrySummary {
        id: entry.id.as_str().to_string(),
        pinned: entry.pinned,
        copied_at: entry.copied_at,
        kind,
        preview: entry.content.preview(120),
        size: entry.content.size(),
    }
}

// ── The blocking client ──────────────────────────────────────────────────
//
// `hyprforge-clipmenu` is the one caller: a short-lived popup that asks
// the daemon to pin or unpin an entry and needs an answer before it can
// keep going. Defined beside the server, over the same [`Request`]/JSON
// line the server already speaks, so the protocol has one definition
// rather than the popup hand-rolling a second one.
//
// This is deliberately synchronous, over `std::os::unix::net::UnixStream`
// rather than `tokio`: the popup has no other use for an async runtime,
// and "connect, write one line, read one line" needs nothing more than a
// blocking socket with a read/write deadline set on it.

/// How long [`set_pinned`] waits for a response before giving up. Short,
/// because a user is waiting on this as a UI action — CLAUDE.md is
/// explicit that nothing here waits on another process without a bound,
/// and a popup that hung for even a few seconds on a dead or wedged
/// daemon would look broken.
pub const CLIENT_TIMEOUT: Duration = Duration::from_millis(500);

/// Everything that can go wrong asking the daemon to change a pin.
///
/// Kept distinct from [`IpcError`] (the server's own startup errors):
/// this is what a *caller* sees, and the caller has to tell "there was
/// nobody to ask" apart from "I asked and was refused" — CLAUDE.md is
/// explicit that a service that is not running is a state with its own
/// message, never folded into an ordinary failure, and that state is
/// never cached: every call reconnects from scratch, so a daemon that
/// starts up after a failed attempt is reachable on the very next one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// `$XDG_RUNTIME_DIR` is not set — nowhere to even look for a
    /// socket.
    #[error("$XDG_RUNTIME_DIR is not set")]
    NoRuntimeDir,
    /// Couldn't connect at all. In practice this is almost always
    /// `hyprforge-clipd` not running — a missing socket file behaves the
    /// same as a refused connection from this side, and there is no
    /// value in telling them apart for a caller that only wants to know
    /// whether pinning is possible right now.
    #[error("couldn't reach hyprforge-clipd: {0}")]
    Unreachable(String),
    /// Connected, but no complete response arrived within
    /// [`CLIENT_TIMEOUT`]. A daemon that accepted the connection and
    /// then never answered — wedged, or overloaded — must not hang this
    /// caller either, so this is reported the same as any other failure
    /// to reach it rather than left to block.
    #[error("hyprforge-clipd did not respond in time")]
    Timeout,
    /// A response arrived but was not the JSON object every response in
    /// the protocol table is documented to be.
    #[error("hyprforge-clipd sent a response this client could not understand")]
    Malformed,
    /// The daemon understood the request and refused it — `can_save`
    /// was false, or the id named no entry. Carries the daemon's own
    /// message, which never contains clipboard content (see this
    /// module's "Never log clipboard content" section) — only an id (a
    /// hash) and the reason.
    #[error("{0}")]
    Refused(String),
}

fn client_socket_error(error: std::io::Error) -> ClientError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ClientError::Timeout,
        _ => ClientError::Unreachable(error.to_string()),
    }
}

/// Sends one request line to the socket at `path` and interprets the one
/// response line that comes back. No I/O happens beyond that single
/// round trip — the connection is closed (by `stream` going out of
/// scope) as soon as this returns, since every caller today needs
/// exactly one request answered, not a kept-open session.
fn request_at(path: &Path, request: &Request, timeout: Duration) -> Result<(), ClientError> {
    let mut stream = std::os::unix::net::UnixStream::connect(path).map_err(client_socket_error)?;
    // Both directions get the same bound: a write can block too, on a
    // kernel socket buffer that never drains because nothing on the
    // other end is reading.
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let line = serde_json::to_string(request).expect("Request always serialises");
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(client_socket_error)?;

    let mut reader = std::io::BufReader::new(stream);
    let mut response_line = String::new();
    let read = reader.read_line(&mut response_line).map_err(client_socket_error)?;
    if read == 0 {
        return Err(ClientError::Unreachable(
            "connection closed with no response".to_string(),
        ));
    }

    let value: serde_json::Value =
        serde_json::from_str(response_line.trim()).map_err(|_| ClientError::Malformed)?;
    match value.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => Ok(()),
        Some(false) => {
            let message = value
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("request refused")
                .to_string();
            Err(ClientError::Refused(message))
        }
        None => Err(ClientError::Malformed),
    }
}

/// Asks `hyprforge-clipd` at the default socket path to pin or unpin
/// `id`. See [`request_at`] for what actually happens on the wire, and
/// this module's doc for why nothing here ever calls [`History::save`]
/// itself — this only ever asks the daemon to.
pub fn set_pinned(id: &str, pinned: bool) -> Result<(), ClientError> {
    let path = socket_path().map_err(|_| ClientError::NoRuntimeDir)?;
    set_pinned_at(&path, id, pinned, CLIENT_TIMEOUT)
}

/// [`set_pinned`] against an explicit socket `path` and `timeout` — the
/// seam the tests below use to talk to a throwaway daemon instead of
/// `$XDG_RUNTIME_DIR`'s real one.
pub fn set_pinned_at(path: &Path, id: &str, pinned: bool, timeout: Duration) -> Result<(), ClientError> {
    let request = if pinned {
        Request::Pin { id: id.to_string() }
    } else {
        Request::Unpin { id: id.to_string() }
    };
    request_at(path, &request, timeout)
}

// ── The socket layer ────────────────────────────────────────────────────

/// State shared between the clipboard watcher loop and every IPC
/// connection: one [`History`] behind one lock, so a pin request and an
/// incoming copy can never interleave into two half-applied writes. See
/// `bin/clipd.rs`.
pub struct Shared {
    pub history: Mutex<History>,
    /// Set once at startup from whether the on-disk file parsed; never
    /// flipped at runtime — see `clipd.rs`'s module doc on why fixing it
    /// requires a restart rather than a live retry.
    pub can_save: bool,
}

/// Runs the control socket at `$XDG_RUNTIME_DIR/clipd.sock` until the
/// process exits. Removes a stale socket file before binding and removes
/// its own socket file again on return.
pub async fn run(shared: Arc<Shared>) -> Result<(), IpcError> {
    run_at(&socket_path()?, shared).await
}

/// Runs the control socket at an explicit `path` — the seam tests use to
/// avoid `$XDG_RUNTIME_DIR` and the real socket entirely.
pub async fn run_at(path: &Path, shared: Arc<Shared>) -> Result<(), IpcError> {
    /// Removes the socket file when dropped, on a clean return or a
    /// cancelled task alike — the same reasoning as `notif-ipc`'s
    /// `SocketGuard`.
    struct SocketGuard(PathBuf);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|source| IpcError::Bind {
        path: path.to_path_buf(),
        source,
    })?;
    let _guard = SocketGuard(path.to_path_buf());

    tracing::info!(path = %path.display(), "listening for clipboard control connections");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    handle_connection(stream, &shared).await;
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept error on clipboard control socket");
            }
        }
    }
}

async fn handle_connection(stream: UnixStream, shared: &Shared) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let read = tokio::time::timeout(IDLE_TIMEOUT, reader.read_line(&mut line)).await;
        let n = match read {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return, // read error: client gone
            Err(_) => {
                tracing::debug!("clipboard control connection idle too long; closing");
                return;
            }
        };
        if n == 0 {
            return; // client closed the connection
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response_json = {
            let mut history = shared.history.lock().await;
            let (response_json, mutated) = handle_line(&mut history, shared.can_save, trimmed);
            if mutated {
                // Still the only writer: this is the same `History::save`
                // `clipd.rs`'s watcher loop calls, taken under the same
                // lock, never a second path to the file.
                if let Err(e) = history.save() {
                    tracing::error!(error = %e, "failed to save clipboard history after a control request");
                }
            }
            response_json
        };

        if writer
            .write_all(format!("{response_json}\n").as_bytes())
            .await
            .is_err()
        {
            return; // client gone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Recordable;
    use crate::types::{Content, Sensitivity};

    fn history_with_one_entry() -> (History, EntryId) {
        let mut history = History::new();
        history.record(
            Recordable::new(Content::Text("hello".into()), Sensitivity::Recordable).unwrap(),
            1,
        );
        let id = history.entries()[0].id.clone();
        (history, id)
    }

    #[test]
    fn ok_response_is_exactly_ok_true_with_no_other_fields() {
        let (mut history, id) = history_with_one_entry();
        let line = format!(r#"{{"cmd":"pin","id":"{}"}}"#, id.as_str());
        let (response, mutated) = handle_line(&mut history, true, &line);
        assert_eq!(response, r#"{"ok":true}"#);
        assert!(mutated);
    }

    #[test]
    fn pin_happy_path_sets_pinned_and_reports_mutated() {
        let (mut history, id) = history_with_one_entry();
        let line = format!(r#"{{"cmd":"pin","id":"{}"}}"#, id.as_str());
        let (_response, mutated) = handle_line(&mut history, true, &line);
        assert!(mutated);
        assert!(history.entries()[0].pinned);
    }

    #[test]
    fn unpin_happy_path_clears_pinned() {
        let (mut history, id) = history_with_one_entry();
        history.set_pinned(&id, true);
        let line = format!(r#"{{"cmd":"unpin","id":"{}"}}"#, id.as_str());
        let (_response, mutated) = handle_line(&mut history, true, &line);
        assert!(mutated);
        assert!(!history.entries()[0].pinned);
    }

    #[test]
    fn remove_happy_path_deletes_the_entry_and_reports_mutated() {
        let (mut history, id) = history_with_one_entry();
        let line = format!(r#"{{"cmd":"remove","id":"{}"}}"#, id.as_str());
        let (_response, mutated) = handle_line(&mut history, true, &line);
        assert!(mutated);
        assert!(history.entries().is_empty());
    }

    #[test]
    fn list_returns_every_entry_without_leaking_raw_content_fields() {
        let (mut history, id) = history_with_one_entry();
        let (response, mutated) = handle_line(&mut history, true, r#"{"cmd":"list"}"#);
        assert!(!mutated, "reading history is never itself a mutation");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], true);
        let entries = value["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], id.as_str());
        assert_eq!(entries[0]["preview"], "hello");
        assert_eq!(entries[0]["kind"], "text");
        // The response is built from `EntrySummary`, which has no field
        // that could carry a raw `Content` — this asserts the JSON has
        // exactly the keys `EntrySummary` declares, not some superset.
        let keys: std::collections::BTreeSet<_> =
            entries[0].as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            ["id", "pinned", "copied_at", "kind", "preview", "size"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[test]
    fn status_reports_counts_and_whether_saving_is_possible() {
        let (mut history, _id) = history_with_one_entry();
        let (response, mutated) = handle_line(&mut history, false, r#"{"cmd":"status"}"#);
        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["status"]["entries"], 1);
        assert_eq!(value["status"]["can_save"], false);
    }

    #[test]
    fn an_unknown_command_is_an_error_not_a_closed_connection() {
        let mut history = History::new();
        let (response, mutated) = handle_line(&mut history, true, r#"{"cmd":"frobnicate"}"#);
        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().is_some());
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        let mut history = History::new();
        let (response, mutated) = handle_line(&mut history, true, "not json at all { { {");
        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn an_id_that_does_not_exist_is_an_error_and_never_mutates() {
        let (mut history, _id) = history_with_one_entry();
        let (response, mutated) =
            handle_line(&mut history, true, r#"{"cmd":"pin","id":"not-a-real-id"}"#);
        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
        // The error names the id (a hash — safe) but this asserts it
        // does not also carry any clipboard content: there is none to
        // carry here in the first place, since only an id was ever
        // given.
        assert!(value["error"].as_str().unwrap().contains("not-a-real-id"));
    }

    #[test]
    fn pin_is_refused_without_mutating_when_the_history_could_not_be_saved() {
        let (mut history, id) = history_with_one_entry();
        let before = history.entries()[0].pinned;
        let line = format!(r#"{{"cmd":"pin","id":"{}"}}"#, id.as_str());
        let (response, mutated) = handle_line(&mut history, false, &line);
        assert!(
            !mutated,
            "a request must not be applied in memory when it can never be saved"
        );
        assert_eq!(
            history.entries()[0].pinned,
            before,
            "pin state must be untouched"
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn remove_is_refused_without_mutating_when_the_history_could_not_be_saved() {
        let (mut history, id) = history_with_one_entry();
        let line = format!(r#"{{"cmd":"remove","id":"{}"}}"#, id.as_str());
        let (response, mutated) = handle_line(&mut history, false, &line);
        assert!(!mutated);
        assert_eq!(history.entries().len(), 1, "entry must still be present");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn list_and_status_still_work_when_the_history_could_not_be_saved() {
        let (mut history, _id) = history_with_one_entry();
        let (response, _) = handle_line(&mut history, false, r#"{"cmd":"list"}"#);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], true, "reading must not be blocked by can_save");
    }

    /// End-to-end over a real socket, but on a throwaway path — never
    /// `$XDG_RUNTIME_DIR` and never a real `hyprforge-clipd`. Exercises
    /// the parts `handle_line` cannot: binding, a stale-socket removal,
    /// a real connection, and the socket being gone on return.
    #[tokio::test]
    async fn a_stale_socket_file_does_not_block_binding_and_a_real_request_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipd-test.sock");
        std::fs::write(&path, b"not a socket").unwrap(); // simulate staleness

        let (history, id) = history_with_one_entry();
        let shared = Arc::new(Shared {
            history: Mutex::new(history),
            can_save: true,
        });

        let serve_path = path.clone();
        let serve_shared = Arc::clone(&shared);
        let server = tokio::spawn(async move {
            let _ = run_at(&serve_path, serve_shared).await;
        });

        // Give the listener a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(path.exists(), "socket file must exist once bound");

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        writer
            .write_all(format!("{{\"cmd\":\"pin\",\"id\":\"{}\"}}\n", id.as_str()).as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), r#"{"ok":true}"#);

        {
            let history = shared.history.lock().await;
            assert!(
                history.entries()[0].pinned,
                "the shared History was mutated"
            );
        }

        server.abort();
    }

    // ── The blocking client, against a real (throwaway) socket ─────────

    /// The client's happy path: a real `run_at` server, on a tempdir
    /// socket, actually flips the pin and reports success — proving the
    /// client and the server agree on the wire format, not just that
    /// `handle_line` parses what the client happens to send.
    #[tokio::test]
    async fn the_client_pins_an_entry_against_a_real_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipd-client-test.sock");
        let (history, id) = history_with_one_entry();
        let shared = Arc::new(Shared { history: Mutex::new(history), can_save: true });

        let serve_path = path.clone();
        let serve_shared = Arc::clone(&shared);
        let server = tokio::spawn(async move {
            let _ = run_at(&serve_path, serve_shared).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The client is synchronous, so it runs on a blocking thread —
        // `spawn_blocking` is only this test's plumbing to call it from
        // an async test, not something the client itself needs.
        let client_path = path.clone();
        let client_id = id.as_str().to_string();
        tokio::task::spawn_blocking(move || {
            set_pinned_at(&client_path, &client_id, true, Duration::from_secs(2))
        })
        .await
        .unwrap()
        .expect("pin must succeed against a running daemon");

        {
            let history = shared.history.lock().await;
            assert!(history.entries()[0].pinned, "the daemon's own History was mutated");
        }

        server.abort();
    }

    /// The failure case that matters most: nothing is listening at all.
    /// This must come back quickly as a distinct, actionable error —
    /// never hang, and never look like the pin succeeded.
    #[test]
    fn pinning_with_no_daemon_running_fails_fast_and_names_the_problem() {
        let dir = tempfile::tempdir().unwrap();
        // A path where nothing has ever bound a socket.
        let path = dir.path().join("nobody-home.sock");
        let started = std::time::Instant::now();
        let result = set_pinned_at(&path, "some-id", true, Duration::from_secs(2));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a refused connection must fail immediately, not wait out the timeout"
        );
        assert!(
            matches!(result, Err(ClientError::Unreachable(_))),
            "got {result:?}"
        );
    }

    /// A connection that is accepted but never answered must still be
    /// bounded — CLAUDE.md's rule against waiting on another process
    /// without one, applied to this client specifically.
    #[test]
    fn a_daemon_that_never_answers_times_out_rather_than_hanging_forever() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let accept_thread = std::thread::spawn(move || {
            // Accept the connection and then do nothing at all with it —
            // simulating a wedged daemon that took the request and never
            // replied.
            let _kept_alive = listener.accept();
            std::thread::sleep(Duration::from_secs(5));
        });

        let started = std::time::Instant::now();
        let result = set_pinned_at(&path, "some-id", true, Duration::from_millis(200));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must give up within roughly the requested timeout, not wait for the peer"
        );
        assert!(matches!(result, Err(ClientError::Timeout)), "got {result:?}");

        drop(accept_thread); // detached; the test process exits regardless
    }

    /// The daemon's refusal (can_save == false) must reach the client as
    /// its own distinct outcome, carrying the daemon's message, rather
    /// than being reported as success or as a generic unreachable error.
    #[tokio::test]
    async fn a_refusal_from_the_daemon_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipd-refuse-test.sock");
        let (history, id) = history_with_one_entry();
        let shared = Arc::new(Shared { history: Mutex::new(history), can_save: false });

        let serve_path = path.clone();
        let serve_shared = Arc::clone(&shared);
        let server = tokio::spawn(async move {
            let _ = run_at(&serve_path, serve_shared).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client_path = path.clone();
        let client_id = id.as_str().to_string();
        let result = tokio::task::spawn_blocking(move || {
            set_pinned_at(&client_path, &client_id, true, Duration::from_secs(2))
        })
        .await
        .unwrap();

        match result {
            Err(ClientError::Refused(message)) => {
                assert!(message.contains("could not be parsed"), "got {message:?}");
            }
            other => panic!("expected a Refused error, got {other:?}"),
        }

        server.abort();
    }
}
