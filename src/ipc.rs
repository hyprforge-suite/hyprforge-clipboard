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
//! `notif-ipc` (`crates/hyprforge-notif/crates/notif-ipc/src/lib.rs`) so this workspace
//! has one socket convention rather than two:
//!
//! | Request                                    | Response                                   |
//! |---------------------------------------------|---------------------------------------------|
//! | `{"cmd":"pin","id":"<entry id>"}`           | `{"ok":true}`                                |
//! | `{"cmd":"unpin","id":"<entry id>"}`         | `{"ok":true}`                                |
//! | `{"cmd":"remove","id":"<entry id>"}`        | `{"ok":true}`                                |
//! | `{"cmd":"set-clipboard","id":"<entry id>"}` | `{"ok":true}`                                |
//! | `{"cmd":"set-clipboard-text","text":"<s>"}` | `{"ok":true}`                                |
//! | `{"cmd":"list"}`                            | `{"ok":true,"entries":[…]}`                  |
//! | `{"cmd":"status"}`                          | `{"ok":true,"status":{…}}`                   |
//! | unknown / malformed                         | `{"ok":false,"error":"…"}`                   |
//!
//! # `set-clipboard`: the daemon becomes the selection's owner
//!
//! `pin`/`unpin`/`remove` change what is *recorded*; `set-clipboard` puts
//! an already-recorded entry's content back on the compositor's
//! clipboard, with this daemon as the source that serves it. On Wayland
//! the selection is served by whichever client set it — see
//! `bin/clipd.rs`'s `SharedHistory` doc for the "the source has to stay
//! alive" rule `write.rs` documents on `ClipboardWriter::set_selection`
//! — so a popup that sets the selection and then exits loses the
//! content the moment it does. `hyprforge-clipd` is already resident for
//! as long as anyone can paste, so it — not the short-lived popup — is
//! the thing that should hold the source open: see [`Shared::selection`]
//! for how the guard from `set_selection` is kept, and this module's
//! `handle_line` for where the swap happens.
//!
//! `set-clipboard` does **not** touch `can_save` or call
//! [`History::save`] at all: it reads an already-loaded entry out of
//! memory and hands its content to the compositor. Nothing about it
//! writes the index file, so refusing it when `can_save` is `false`
//! would withhold a feature (repeat-paste) that has nothing to do with
//! the thing that actually failed (parsing the on-disk file). The two
//! stay independent: a broken history file still blocks *saving*
//! mutations, but it never blocks reading an entry that already made it
//! into memory and putting it back on the clipboard.
//!
//! # `set-clipboard-text`: the same, for content this daemon never recorded
//!
//! `set-clipboard` only ever reaches content this daemon already has in
//! memory, keyed by an id from its own history — there was no caller
//! that needed anything else until `hyprforge-emojimenu`. An emoji a
//! person just picked was never copied through the clipboard watcher, so
//! it has no history entry and no id to name; asking this daemon to hold
//! it open for repeat-paste needs a way in that does not go through
//! [`History`] at all. `set-clipboard-text` is that: it carries the text
//! itself rather than a reference to it, skips [`History`] entirely (no
//! lookup, no `can_save` gate — there is nothing here that could ever be
//! "unparsed"), and otherwise takes over `*selection` exactly the way
//! `set-clipboard` does, for exactly the same reason (see
//! [`Shared::selection`]'s doc). Text only, deliberately: an emoji is
//! never an image, and adding an image variant with no caller to
//! exercise it would be an untested path the moment it landed.
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
//! `IDLE_TIMEOUT` rather than left to `read_line` forever — the same
//! "nothing waits without a bound" rule as everywhere else in this
//! workspace. Each connection also runs as its own task, so one slow or
//! silent client cannot block another, or the clipboard watcher loop.

use crate::store::History;
use crate::types::{Content, EntryId};
use crate::write::ClipboardWriter;
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
    /// Put an already-recorded entry's content back on the clipboard,
    /// with this daemon as the source that serves it — see the module
    /// doc's "`set-clipboard`: the daemon becomes the selection's
    /// owner".
    SetClipboard { id: String },
    /// Put `text` itself back on the clipboard, with this daemon as the
    /// source that serves it — the same ownership `SetClipboard` gives
    /// an already-recorded entry, for content (an emoji, say) this
    /// daemon never recorded and has no id for. See the module doc's
    /// "`set-clipboard-text`" section.
    SetClipboardText { text: String },
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
/// `writer` and `selection` only matter for [`Request::SetClipboard`] —
/// every other request ignores them. The real server (`handle_connection`
/// below) does **not** call this for `set-clipboard`: it needs to release
/// the history lock before making the (unbounded) Wayland round trip
/// `writer.set_selection` makes, and this function holds `history` for
/// its whole body. This is still the entry point the tests use, with
/// `crate::write::mock::MockWriter` (behind the `mock` feature) standing in for a real writer —
/// the mock never blocks, so calling it from here is exactly as pure as
/// every other request already is.
pub fn handle_line<W: ClipboardWriter>(
    history: &mut History,
    can_save: bool,
    writer: &W,
    selection: &mut Option<W::Guard>,
    line: &str,
) -> (String, bool) {
    let request: Result<Request, _> = serde_json::from_str(line);
    let (response, mutated) = match request {
        Err(e) => (err(format!("malformed request: {e}")), false),
        Ok(request) => dispatch(history, can_save, writer, selection, request),
    };
    (to_json(&response), mutated)
}

fn dispatch<W: ClipboardWriter>(
    history: &mut History,
    can_save: bool,
    writer: &W,
    selection: &mut Option<W::Guard>,
    request: Request,
) -> (Response, bool) {
    match request {
        Request::Pin { id } => apply_pin(history, can_save, id, true),
        Request::Unpin { id } => apply_pin(history, can_save, id, false),
        Request::Remove { id } => remove(history, can_save, id),
        Request::SetClipboard { id } => apply_set_clipboard(history, writer, selection, id),
        Request::SetClipboardText { text } => apply_set_clipboard_text(writer, selection, text),
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

/// Looks `id` up in `history` and returns its content, or the response to
/// send back when no entry has that id. No I/O, and no `writer` involved
/// — shared by [`apply_set_clipboard`] below and by the real server's own
/// two-phase path (`set_clipboard_on_daemon`), so both agree on exactly
/// what "no such id" reports.
fn find_content(history: &History, id: &str) -> Result<Content, Response> {
    let entry_id = EntryId::from_raw(id);
    history
        .entries()
        .iter()
        .find(|e| e.id == entry_id)
        .map(|e| e.content.clone())
        .ok_or_else(|| err(format!("no clipboard entry with id {id}")))
}

/// `set-clipboard`: puts `id`'s content on the clipboard through
/// `writer`, replacing `*selection` with the new guard.
///
/// Deliberately **not** gated on `can_save`, unlike `apply_pin`/`remove`.
/// Those refuse when the on-disk history could not be parsed because
/// applying them anyway would only ever be kept in memory, never saved —
/// see `unsavable_message`. This request never calls [`History::save`]
/// at all: it reads an entry already sitting in memory and hands its
/// content to the compositor, so whether the *index file* parsed has no
/// bearing on whether that read-and-serve can happen. Gating this on
/// `can_save` would withhold "paste this again" — which has nothing to
/// do with the file on disk — for exactly the situation (a history that
/// failed to load) where the user most needs the entries that *did* make
/// it into memory to still be usable.
///
/// Replacing `*selection` drops whatever guard was there before it is
/// overwritten (a `Mutex<Option<T>>` assignment runs the old `Option`'s
/// `Drop` before storing the new one) — which drops that source and lets
/// its dispatch thread end, per `write.rs`'s `ClipboardWriter::set_selection`
/// doc. Exactly one guard is ever held at a time.
///
/// Never itself a `History` mutation (`mutated` is always `false`): the
/// caller must never call `History::save` for this request.
fn apply_set_clipboard<W: ClipboardWriter>(
    history: &History,
    writer: &W,
    selection: &mut Option<W::Guard>,
    id: String,
) -> (Response, bool) {
    match find_content(history, &id) {
        Err(response) => (response, false),
        Ok(content) => match writer.set_selection(content) {
            Ok(guard) => {
                *selection = Some(guard);
                (Response::Ok, false)
            }
            // Never the content — `e` is a connection/protocol error
            // from `ClipboardWriter::set_selection`, never anything
            // derived from what was copied.
            Err(e) => (err(format!("failed to set the clipboard: {e}")), false),
        },
    }
}

/// `set-clipboard-text`: puts `text` on the clipboard through `writer`,
/// replacing `*selection` with the new guard — the same mechanics as
/// [`apply_set_clipboard`], minus the [`History`] lookup, since there is
/// no entry to look up (see the module doc). Never a `History` mutation
/// either, for the same reason.
fn apply_set_clipboard_text<W: ClipboardWriter>(
    writer: &W,
    selection: &mut Option<W::Guard>,
    text: String,
) -> (Response, bool) {
    match writer.set_selection(Content::Text(text)) {
        Ok(guard) => {
            *selection = Some(guard);
            (Response::Ok, false)
        }
        // Never the content — same rule `apply_set_clipboard` follows.
        Err(e) => (err(format!("failed to set the clipboard: {e}")), false),
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
/// `id`. See `request_at` for what actually happens on the wire, and
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

/// Asks `hyprforge-clipd` at the default socket path to forget `id` —
/// the popup's Delete key. Like [`set_pinned`], this only ever *asks*:
/// the daemon is the one writer of the history file, and a refusal (an
/// id it no longer has, or a history it cannot save) comes back as
/// [`ClientError::Refused`] rather than as a row that looks deleted and
/// is not.
pub fn remove_entry(id: &str) -> Result<(), ClientError> {
    let path = socket_path().map_err(|_| ClientError::NoRuntimeDir)?;
    remove_entry_at(&path, id, CLIENT_TIMEOUT)
}

/// [`remove_entry`] against an explicit socket `path` and `timeout` — the
/// seam the tests below use to talk to a throwaway daemon instead of
/// `$XDG_RUNTIME_DIR`'s real one.
pub fn remove_entry_at(path: &Path, id: &str, timeout: Duration) -> Result<(), ClientError> {
    request_at(path, &Request::Remove { id: id.to_string() }, timeout)
}

/// Asks `hyprforge-clipd` at the default socket path to put entry `id`
/// back on the clipboard, with the daemon itself holding the selection
/// open afterward — see this module's doc on why that is the whole
/// point of routing this through the daemon rather than setting the
/// selection from the caller's own process.
///
/// `hyprforge-clipmenu` is not required to use this: nothing about
/// [`crate::ClipboardWriter`]/[`crate::SelectionGuard`] goes away.
/// [`ClientError::Unreachable`] coming back means there was nobody to
/// ask (the daemon is not running) rather than that the request was
/// refused — CLAUDE.md's "every component runs alone" — so a caller can
/// treat it as the signal to fall back to setting the selection itself
/// (and waiting on the guard, the way `hyprforge-clipmenu`'s chooser
/// does today) instead of treating a missing daemon as an outright
/// failure to paste.
pub fn set_clipboard(id: &str) -> Result<(), ClientError> {
    let path = socket_path().map_err(|_| ClientError::NoRuntimeDir)?;
    set_clipboard_at(&path, id, CLIENT_TIMEOUT)
}

/// [`set_clipboard`] against an explicit socket `path` and `timeout` —
/// the seam the tests below use to talk to a throwaway daemon instead of
/// `$XDG_RUNTIME_DIR`'s real one.
pub fn set_clipboard_at(path: &Path, id: &str, timeout: Duration) -> Result<(), ClientError> {
    request_at(path, &Request::SetClipboard { id: id.to_string() }, timeout)
}

/// Asks `hyprforge-clipd` at the default socket path to put `text`
/// itself back on the clipboard, with the daemon holding the selection
/// open afterward — [`set_clipboard`] for content that was never copied
/// through this daemon's own history (an emoji a picker just chose, with
/// no id to name), so there is nothing to look up here at all. See the
/// module doc's "`set-clipboard-text`" section.
///
/// [`ClientError::Unreachable`]/[`ClientError::NoRuntimeDir`] mean the
/// same thing [`set_clipboard`]'s doc already gives them: nobody to ask,
/// not a refusal — the caller's own fallback (owning the selection
/// itself, as `hyprforge-clipmenu`'s `chooser::Wired` already does for
/// the identical case) is exactly as appropriate here.
pub fn set_clipboard_text(text: &str) -> Result<(), ClientError> {
    let path = socket_path().map_err(|_| ClientError::NoRuntimeDir)?;
    set_clipboard_text_at(&path, text, CLIENT_TIMEOUT)
}

/// [`set_clipboard_text`] against an explicit socket `path` and
/// `timeout` — the seam the tests below use to talk to a throwaway
/// daemon instead of `$XDG_RUNTIME_DIR`'s real one.
pub fn set_clipboard_text_at(path: &Path, text: &str, timeout: Duration) -> Result<(), ClientError> {
    request_at(path, &Request::SetClipboardText { text: text.to_string() }, timeout)
}

// ── The socket layer ────────────────────────────────────────────────────

/// State shared between the clipboard watcher loop and every IPC
/// connection: one [`History`] behind one lock, so a pin request and an
/// incoming copy can never interleave into two half-applied writes. See
/// `bin/clipd.rs`.
///
/// `writer` and `selection` are the new half, for `set-clipboard`:
/// `writer` is what actually talks to the compositor, and `selection`
/// holds the guard from this daemon's own most recent successful
/// `set_selection` call — kept alive for as long as this process runs,
/// which is the entire reason `set-clipboard` exists (see the module
/// doc). It is a lock **separate** from `history`'s: a `set-clipboard`
/// request only needs `history` briefly, to read an entry's content (see
/// `set_clipboard_on_daemon`), and must never hold it across the actual
/// (unbounded) Wayland round trip `writer.set_selection` makes — that
/// would stall the watcher loop's own `history.lock().await` for as long
/// as the compositor takes to answer, which per CLAUDE.md is a thing
/// nothing here may wait on without a bound.
pub struct Shared<W: ClipboardWriter> {
    pub history: Mutex<History>,
    /// Set once at startup from whether the on-disk file parsed; never
    /// flipped at runtime — see `clipd.rs`'s module doc on why fixing it
    /// requires a restart rather than a live retry.
    pub can_save: bool,
    pub writer: W,
    pub selection: Mutex<Option<W::Guard>>,
}

/// Runs the control socket at `$XDG_RUNTIME_DIR/clipd.sock` until the
/// process exits. Removes a stale socket file before binding and removes
/// its own socket file again on return.
pub async fn run<W: ClipboardWriter + 'static>(shared: Arc<Shared<W>>) -> Result<(), IpcError>
where
    W::Guard: 'static,
{
    run_at(&socket_path()?, shared).await
}

/// Runs the control socket at an explicit `path` — the seam tests use to
/// avoid `$XDG_RUNTIME_DIR` and the real socket entirely.
pub async fn run_at<W: ClipboardWriter + 'static>(
    path: &Path,
    shared: Arc<Shared<W>>,
) -> Result<(), IpcError>
where
    W::Guard: 'static,
{
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
                    handle_connection(stream, shared).await;
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept error on clipboard control socket");
            }
        }
    }
}

async fn handle_connection<W: ClipboardWriter + 'static>(stream: UnixStream, shared: Arc<Shared<W>>)
where
    W::Guard: 'static,
{
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

        let response_json = process_line(&shared, trimmed).await;

        if writer
            .write_all(format!("{response_json}\n").as_bytes())
            .await
            .is_err()
        {
            return; // client gone
        }
    }
}

/// Parses one request line and answers it, routing `set-clipboard` to
/// [`set_clipboard_on_daemon`] (never through the same lock scope as the
/// other requests — see [`Shared`]'s doc) and everything else through
/// the same [`dispatch`]/[`History::save`] pattern this socket has always
/// used.
async fn process_line<W: ClipboardWriter + 'static>(shared: &Arc<Shared<W>>, line: &str) -> String
where
    W::Guard: 'static,
{
    match serde_json::from_str::<Request>(line) {
        Err(e) => to_json(&err(format!("malformed request: {e}"))),
        Ok(Request::SetClipboard { id }) => {
            to_json(&set_clipboard_on_daemon(Arc::clone(shared), id).await)
        }
        Ok(Request::SetClipboardText { text }) => {
            to_json(&set_clipboard_text_on_daemon(Arc::clone(shared), text).await)
        }
        Ok(request) => {
            let mut history = shared.history.lock().await;
            let mut no_writer_needed = None; // none of these variants touch `writer`/`selection`
            let (response, mutated) = dispatch(
                &mut history,
                shared.can_save,
                &shared.writer,
                &mut no_writer_needed,
                request,
            );
            if mutated {
                // Still the only writer: this is the same `History::save`
                // `clipd.rs`'s watcher loop calls, taken under the same
                // lock, never a second path to the file.
                if let Err(e) = history.save() {
                    tracing::error!(error = %e, "failed to save clipboard history after a control request");
                }
            }
            to_json(&response)
        }
    }
}

/// The real, async two-phase `set-clipboard`: reads the entry's content
/// with `shared.history` locked only long enough to clone it, then makes
/// the actual (unbounded) Wayland round trip on a blocking thread with
/// *no* lock held at all, and finally stores the resulting guard under
/// `shared.selection`'s own lock — never `shared.history`'s. This is the
/// shape [`Shared`]'s doc promises: the watcher loop's own
/// `history.lock().await` is never made to wait on the compositor
/// answering a `set-clipboard` request.
async fn set_clipboard_on_daemon<W: ClipboardWriter + 'static>(
    shared: Arc<Shared<W>>,
    id: String,
) -> Response
where
    W::Guard: 'static,
{
    let content = {
        let history = shared.history.lock().await;
        match find_content(&history, &id) {
            Ok(content) => content,
            Err(response) => return response,
        }
    };
    // `history`'s lock is dropped here, before the compositor round trip
    // below — see this function's doc.

    let blocking_shared = Arc::clone(&shared);
    let result = tokio::task::spawn_blocking(move || blocking_shared.writer.set_selection(content))
        .await;

    match result {
        Ok(Ok(guard)) => {
            // Assigning over `*selection` drops whatever guard was there
            // before the new value is stored — dropping the old source
            // and letting its dispatch thread end. Exactly one guard is
            // ever held at a time; see `apply_set_clipboard`'s doc for
            // the same point on the test-only synchronous path.
            let mut selection = shared.selection.lock().await;
            *selection = Some(guard);
            Response::Ok
        }
        Ok(Err(e)) => err(format!("failed to set the clipboard: {e}")),
        Err(_join_error) => {
            // The blocking task panicked — this daemon must keep running
            // regardless (see `bin/clipd.rs`'s "fatal only for the
            // socket" reasoning for the control task as a whole), so this
            // is reported to the one caller who asked rather than taken
            // as a reason to do anything more drastic.
            err("internal error setting the clipboard".to_string())
        }
    }
}

/// The `set-clipboard-text` counterpart to [`set_clipboard_on_daemon`]:
/// the same unbounded Wayland round trip on a blocking thread with no
/// lock held across it, but with no [`History`] lookup at all — there is
/// no id, only `text` itself (see the module doc's "`set-clipboard-text`"
/// section and [`apply_set_clipboard_text`]'s doc for why).
async fn set_clipboard_text_on_daemon<W: ClipboardWriter + 'static>(shared: Arc<Shared<W>>, text: String) -> Response
where
    W::Guard: 'static,
{
    let blocking_shared = Arc::clone(&shared);
    let result =
        tokio::task::spawn_blocking(move || blocking_shared.writer.set_selection(Content::Text(text))).await;

    match result {
        Ok(Ok(guard)) => {
            let mut selection = shared.selection.lock().await;
            *selection = Some(guard);
            Response::Ok
        }
        Ok(Err(e)) => err(format!("failed to set the clipboard: {e}")),
        Err(_join_error) => err("internal error setting the clipboard".to_string()),
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

    /// [`handle_line`] with a throwaway [`crate::write::mock::MockWriter`]
    /// and no selection state — what every test that only cares about
    /// `pin`/`unpin`/`remove`/`list`/`status` uses, so those tests read
    /// exactly as they did before `set-clipboard` existed.
    fn handle_line_test(history: &mut History, can_save: bool, line: &str) -> (String, bool) {
        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;
        handle_line(history, can_save, &writer, &mut selection, line)
    }

    /// A [`Shared`] over [`crate::write::mock::MockWriter`], for the
    /// real-socket tests below — never a real compositor, and never
    /// `$XDG_RUNTIME_DIR`.
    fn test_shared(history: History, can_save: bool) -> Arc<Shared<crate::write::mock::MockWriter>> {
        Arc::new(Shared {
            history: Mutex::new(history),
            can_save,
            writer: crate::write::mock::MockWriter::new(),
            selection: Mutex::new(None),
        })
    }

    #[test]
    fn ok_response_is_exactly_ok_true_with_no_other_fields() {
        let (mut history, id) = history_with_one_entry();
        let line = format!(r#"{{"cmd":"pin","id":"{}"}}"#, id.as_str());
        let (response, mutated) = handle_line_test(&mut history, true, &line);
        assert_eq!(response, r#"{"ok":true}"#);
        assert!(mutated);
    }

    #[test]
    fn pin_happy_path_sets_pinned_and_reports_mutated() {
        let (mut history, id) = history_with_one_entry();
        let line = format!(r#"{{"cmd":"pin","id":"{}"}}"#, id.as_str());
        let (_response, mutated) = handle_line_test(&mut history, true, &line);
        assert!(mutated);
        assert!(history.entries()[0].pinned);
    }

    #[test]
    fn unpin_happy_path_clears_pinned() {
        let (mut history, id) = history_with_one_entry();
        history.set_pinned(&id, true);
        let line = format!(r#"{{"cmd":"unpin","id":"{}"}}"#, id.as_str());
        let (_response, mutated) = handle_line_test(&mut history, true, &line);
        assert!(mutated);
        assert!(!history.entries()[0].pinned);
    }

    #[test]
    fn remove_happy_path_deletes_the_entry_and_reports_mutated() {
        let (mut history, id) = history_with_one_entry();
        let line = format!(r#"{{"cmd":"remove","id":"{}"}}"#, id.as_str());
        let (_response, mutated) = handle_line_test(&mut history, true, &line);
        assert!(mutated);
        assert!(history.entries().is_empty());
    }

    #[test]
    fn list_returns_every_entry_without_leaking_raw_content_fields() {
        let (mut history, id) = history_with_one_entry();
        let (response, mutated) = handle_line_test(&mut history, true, r#"{"cmd":"list"}"#);
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
        let (response, mutated) = handle_line_test(&mut history, false, r#"{"cmd":"status"}"#);
        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["status"]["entries"], 1);
        assert_eq!(value["status"]["can_save"], false);
    }

    #[test]
    fn an_unknown_command_is_an_error_not_a_closed_connection() {
        let mut history = History::new();
        let (response, mutated) = handle_line_test(&mut history, true, r#"{"cmd":"frobnicate"}"#);
        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().is_some());
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        let mut history = History::new();
        let (response, mutated) = handle_line_test(&mut history, true, "not json at all { { {");
        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn an_id_that_does_not_exist_is_an_error_and_never_mutates() {
        let (mut history, _id) = history_with_one_entry();
        let (response, mutated) =
            handle_line_test(&mut history, true, r#"{"cmd":"pin","id":"not-a-real-id"}"#);
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
        let (response, mutated) = handle_line_test(&mut history, false, &line);
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
        let (response, mutated) = handle_line_test(&mut history, false, &line);
        assert!(!mutated);
        assert_eq!(history.entries().len(), 1, "entry must still be present");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn list_and_status_still_work_when_the_history_could_not_be_saved() {
        let (mut history, _id) = history_with_one_entry();
        let (response, _) = handle_line_test(&mut history, false, r#"{"cmd":"list"}"#);
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
        let shared = test_shared(history, true);

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
        let shared = test_shared(history, true);

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

    /// The same round trip for the popup's Delete key: the client's
    /// `remove` and the server's `Remove` agree on the wire, and the
    /// entry is actually gone from the daemon's own history afterwards.
    #[tokio::test]
    async fn the_client_removes_an_entry_against_a_real_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipd-remove-test.sock");
        let (history, id) = history_with_one_entry();
        let shared = test_shared(history, true);

        let serve_path = path.clone();
        let serve_shared = Arc::clone(&shared);
        let server = tokio::spawn(async move {
            let _ = run_at(&serve_path, serve_shared).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client_path = path.clone();
        let client_id = id.as_str().to_string();
        tokio::task::spawn_blocking(move || remove_entry_at(&client_path, &client_id, Duration::from_secs(2)))
            .await
            .unwrap()
            .expect("remove must succeed against a running daemon");

        {
            let history = shared.history.lock().await;
            assert!(history.entries().is_empty(), "the entry must be gone from the daemon's own History");
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
        let shared = test_shared(history, false);

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

    // ── set-clipboard ────────────────────────────────────────────────

    /// The happy path: an existing entry's content reaches the writer,
    /// and the resulting guard is kept in `selection` — this is the
    /// property Part 2 of the task exists for. `MockWriter` never
    /// touches a real compositor.
    #[test]
    fn set_clipboard_happy_path_hands_the_entrys_content_to_the_writer_and_keeps_the_guard() {
        let (mut history, id) = history_with_one_entry();
        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;
        let line = format!(r#"{{"cmd":"set-clipboard","id":"{}"}}"#, id.as_str());

        let (response, mutated) = handle_line(&mut history, true, &writer, &mut selection, &line);

        assert_eq!(response, r#"{"ok":true}"#);
        assert!(
            !mutated,
            "set-clipboard never writes the index file, so it must never ask the caller to save"
        );
        assert_eq!(writer.calls(), vec![Content::Text("hello".into())]);
        assert!(
            selection.is_some(),
            "the guard from set_selection must be kept, not dropped"
        );
    }

    /// Replacing an old guard must drop it — otherwise a source (and its
    /// dispatch thread) from a previous `set-clipboard` would outlive its
    /// usefulness. `MockGuard` carries no observable drop signal on its
    /// own, so this pins the behaviour through `Option::replace`'s
    /// documented semantics instead: assigning `*selection = Some(..)`
    /// again must still leave exactly one guard held, never two.
    #[test]
    fn setting_the_clipboard_a_second_time_replaces_rather_than_accumulates_the_guard() {
        let mut history = History::new();
        history.record(
            Recordable::new(Content::Text("first".into()), Sensitivity::Recordable).unwrap(),
            1,
        );
        history.record(
            Recordable::new(Content::Text("second".into()), Sensitivity::Recordable).unwrap(),
            2,
        );
        let first_id = history
            .entries()
            .iter()
            .find(|e| e.content == Content::Text("first".into()))
            .unwrap()
            .id
            .clone();
        let second_id = history
            .entries()
            .iter()
            .find(|e| e.content == Content::Text("second".into()))
            .unwrap()
            .id
            .clone();

        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;

        let line_one = format!(r#"{{"cmd":"set-clipboard","id":"{}"}}"#, first_id.as_str());
        handle_line(&mut history, true, &writer, &mut selection, &line_one);
        assert!(selection.is_some());

        let line_two = format!(r#"{{"cmd":"set-clipboard","id":"{}"}}"#, second_id.as_str());
        handle_line(&mut history, true, &writer, &mut selection, &line_two);

        // Still exactly one guard — the assignment in `apply_set_clipboard`
        // replaced (and thereby dropped) the first rather than being
        // additive.
        assert!(selection.is_some());
        assert_eq!(
            writer.calls(),
            vec![Content::Text("first".into()), Content::Text("second".into())]
        );
    }

    /// An id that names no entry is refused, exactly like `pin`/`remove`
    /// — and, crucially, nothing is ever handed to the writer for it.
    #[test]
    fn set_clipboard_with_an_unknown_id_is_refused_and_never_touches_the_writer() {
        let (mut history, _id) = history_with_one_entry();
        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;

        let (response, mutated) = handle_line(
            &mut history,
            true,
            &writer,
            &mut selection,
            r#"{"cmd":"set-clipboard","id":"not-a-real-id"}"#,
        );

        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().unwrap().contains("not-a-real-id"));
        assert!(writer.calls().is_empty());
        assert!(selection.is_none());
    }

    /// The property the module doc's "`set-clipboard` does not touch
    /// `can_save`" section exists to pin: unlike `pin`/`unpin`/`remove`,
    /// this request succeeds even when the on-disk history could not be
    /// parsed, because it never calls `History::save` — it only reads an
    /// entry already in memory and hands it to the writer.
    #[test]
    fn set_clipboard_works_even_when_the_history_could_not_be_saved() {
        let (mut history, id) = history_with_one_entry();
        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;
        let line = format!(r#"{{"cmd":"set-clipboard","id":"{}"}}"#, id.as_str());

        let (response, mutated) =
            handle_line(&mut history, /* can_save */ false, &writer, &mut selection, &line);

        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value["ok"], true,
            "a broken index file must not block putting an in-memory entry back on the clipboard"
        );
        assert!(selection.is_some());
    }

    /// A failure from the writer itself (the mock configured to error,
    /// standing in for a real compositor round trip that failed) must be
    /// reported, and must not leave a stale guard behind or claim
    /// success.
    #[test]
    fn a_writer_failure_is_reported_and_leaves_no_guard() {
        struct FailingWriter;
        impl ClipboardWriter for FailingWriter {
            type Guard = crate::write::mock::MockGuard;

            fn set_selection(&self, _content: Content) -> anyhow::Result<Self::Guard> {
                anyhow::bail!("no clipboard protocol available")
            }
        }

        let (mut history, id) = history_with_one_entry();
        let writer = FailingWriter;
        let mut selection = None;
        let line = format!(r#"{{"cmd":"set-clipboard","id":"{}"}}"#, id.as_str());

        let (response, mutated) = handle_line(&mut history, true, &writer, &mut selection, &line);

        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
        assert!(selection.is_none());
    }

    // ── set-clipboard-text ───────────────────────────────────────────

    /// The whole point: `text` reaches the writer with no `History`
    /// lookup involved at all — an empty history still succeeds, which
    /// `set-clipboard` (by id) never could.
    #[test]
    fn set_clipboard_text_hands_the_text_to_the_writer_with_no_history_entry_needed() {
        let mut history = History::new();
        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;

        let (response, mutated) =
            handle_line(&mut history, true, &writer, &mut selection, r#"{"cmd":"set-clipboard-text","text":"👋"}"#);

        assert_eq!(response, r#"{"ok":true}"#);
        assert!(!mutated, "set-clipboard-text never touches the index file either");
        assert_eq!(writer.calls(), vec![Content::Text("👋".into())]);
        assert!(selection.is_some(), "the guard must be kept, exactly like set-clipboard");
    }

    /// Same replace-not-accumulate guarantee `set-clipboard` has, and the
    /// same underlying `Option` assignment providing it — pinned
    /// separately because `apply_set_clipboard_text` is its own function
    /// with its own call to `writer.set_selection`, not a thin wrapper
    /// that reuses `apply_set_clipboard`'s.
    #[test]
    fn setting_clipboard_text_a_second_time_replaces_rather_than_accumulates_the_guard() {
        let mut history = History::new();
        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;

        handle_line(&mut history, true, &writer, &mut selection, r#"{"cmd":"set-clipboard-text","text":"🔥"}"#);
        assert!(selection.is_some());
        handle_line(&mut history, true, &writer, &mut selection, r#"{"cmd":"set-clipboard-text","text":"❤️"}"#);

        assert!(selection.is_some());
        assert_eq!(writer.calls(), vec![Content::Text("🔥".into()), Content::Text("❤️".into())]);
    }

    /// Works even when the on-disk history could not be parsed — there
    /// is no `History` read here at all, so `can_save` (which only ever
    /// gates a *save*) cannot possibly be the reason this fails.
    #[test]
    fn set_clipboard_text_works_even_when_the_history_could_not_be_saved() {
        let mut history = History::new();
        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;

        let (response, mutated) = handle_line(
            &mut history,
            /* can_save */ false,
            &writer,
            &mut selection,
            r#"{"cmd":"set-clipboard-text","text":"🎉"}"#,
        );

        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], true);
        assert!(selection.is_some());
    }

    /// A writer failure is reported and leaves no stale guard — the same
    /// property `a_writer_failure_is_reported_and_leaves_no_guard` pins
    /// for `set-clipboard`.
    #[test]
    fn a_writer_failure_setting_clipboard_text_is_reported_and_leaves_no_guard() {
        struct FailingWriter;
        impl ClipboardWriter for FailingWriter {
            type Guard = crate::write::mock::MockGuard;

            fn set_selection(&self, _content: Content) -> anyhow::Result<Self::Guard> {
                anyhow::bail!("no clipboard protocol available")
            }
        }

        let mut history = History::new();
        let writer = FailingWriter;
        let mut selection = None;

        let (response, mutated) =
            handle_line(&mut history, true, &writer, &mut selection, r#"{"cmd":"set-clipboard-text","text":"🎉"}"#);

        assert!(!mutated);
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["ok"], false);
        assert!(selection.is_none());
    }

    /// The decision Part 3 of the task asks to pin, with a comment
    /// explaining it: the daemon's own watcher will see the clipboard
    /// change it just made through `set-clipboard`, and will try to
    /// `record` it like any other copy. Because `EntryId` is derived from
    /// content (see `types::EntryId::of`), re-recording the very entry
    /// that was just set is recognised as the *same* entry — it moves to
    /// the top and its timestamp updates (arguably desirable: choosing
    /// an old entry is exactly what should bump it back to the top of a
    /// clipboard manager's list) — but it is never duplicated, and
    /// nothing about it causes a second `set_selection` call, so there is
    /// no loop: `record` only ever appends to `History`, it never talks
    /// to a `ClipboardWriter`.
    #[test]
    fn the_watcher_seeing_its_own_set_clipboard_change_moves_the_entry_up_without_duplicating_or_looping(
    ) {
        let mut history = History::new();
        history.record(
            Recordable::new(Content::Text("older".into()), Sensitivity::Recordable).unwrap(),
            1,
        );
        history.record(
            Recordable::new(Content::Text("chosen".into()), Sensitivity::Recordable).unwrap(),
            2,
        );
        let writer = crate::write::mock::MockWriter::new();
        let mut selection = None;
        // The user picks the older entry, well after "chosen" — bumping
        // it back to the top is exactly what set-clipboard is for.
        let older_id = history
            .entries()
            .iter()
            .find(|e| e.content == Content::Text("older".into()))
            .unwrap()
            .id
            .clone();
        let line = format!(r#"{{"cmd":"set-clipboard","id":"{}"}}"#, older_id.as_str());
        let (_response, mutated) = handle_line(&mut history, true, &writer, &mut selection, &line);
        assert!(!mutated, "set-clipboard itself never touches History::save");

        // The watcher loop now "sees" this daemon's own selection change
        // and does exactly what it does for any other copy: calls
        // `record` on it (see `bin/clipd.rs::record`). Simulated directly
        // here since that requires no writer at all — `record` never
        // takes one.
        let recorded_again = history.record(
            Recordable::new(Content::Text("older".into()), Sensitivity::Recordable).unwrap(),
            3,
        );
        assert!(recorded_again, "the same content is still a valid record");

        assert_eq!(
            history.entries().len(),
            2,
            "recognised as the same entry, not appended as a duplicate"
        );
        assert_eq!(
            history.entries()[0].content,
            Content::Text("older".into()),
            "choosing it moved it back to the top, same as any other re-copy"
        );
        assert_eq!(history.entries()[0].copied_at, 3);

        // No loop: nothing above ever called `writer.set_selection` a
        // second time. The only call recorded is the original
        // `set-clipboard` request.
        assert_eq!(writer.calls(), vec![Content::Text("older".into())]);
    }

    /// End-to-end over a real (throwaway) socket: `set-clipboard` against
    /// a running server actually reaches the writer and the client sees
    /// success, proving the client and server agree on this request's
    /// wire shape the same way the existing pin round trip does.
    #[tokio::test]
    async fn the_client_sets_the_clipboard_against_a_real_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipd-set-clipboard-test.sock");
        let (history, id) = history_with_one_entry();
        let shared = test_shared(history, true);

        let serve_path = path.clone();
        let serve_shared = Arc::clone(&shared);
        let server = tokio::spawn(async move {
            let _ = run_at(&serve_path, serve_shared).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client_path = path.clone();
        let client_id = id.as_str().to_string();
        tokio::task::spawn_blocking(move || {
            set_clipboard_at(&client_path, &client_id, Duration::from_secs(2))
        })
        .await
        .unwrap()
        .expect("set-clipboard must succeed against a running daemon");

        {
            let selection = shared.selection.lock().await;
            assert!(
                selection.is_some(),
                "the daemon must have kept the guard from its own set_selection call"
            );
        }

        server.abort();
    }

    /// `set-clipboard` against an unknown id, over the real socket, must
    /// come back as a refusal the client can see — not a silent success
    /// and not a hang.
    #[tokio::test]
    async fn the_client_sees_a_refusal_for_an_unknown_id_over_the_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipd-set-clipboard-unknown-test.sock");
        let (history, _id) = history_with_one_entry();
        let shared = test_shared(history, true);

        let serve_path = path.clone();
        let serve_shared = Arc::clone(&shared);
        let server = tokio::spawn(async move {
            let _ = run_at(&serve_path, serve_shared).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client_path = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            set_clipboard_at(&client_path, "not-a-real-id", Duration::from_secs(2))
        })
        .await
        .unwrap();

        assert!(matches!(result, Err(ClientError::Refused(_))), "got {result:?}");

        server.abort();
    }
}
