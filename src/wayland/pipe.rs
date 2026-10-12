//! Reading one MIME type's bytes off a data-control offer, with a bound
//! on both time and memory.
//!
//! Shared by both protocol backends (`ext.rs` and `wlr.rs`): `receive`
//! opens a pipe, hands the write end to the compositor, and the source
//! application on the other side of that offer writes into it and
//! closes it. That source is not this process, is not necessarily even
//! well-behaved, and CLAUDE.md is explicit that nothing here may wait on
//! another process without a bound.

use std::io::{Read, Write};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::time::Duration;
use wayland_client::Connection;

/// How much of one MIME type's content this crate will ever hold in
/// memory for one clipboard entry.
///
/// Far more than any plaintext paste needs, and enough for a screenshot
/// PNG off an ordinary display; a source offering more than this reads
/// as misbehaving rather than as something worth remembering in full.
/// The cap is what keeps a misbehaving source from growing this
/// process's memory without bound — CLAUDE.md: "Test the resource, not
/// just the result."
pub const MAX_READ_BYTES: usize = 16 * 1024 * 1024;

/// How long this process waits for the source application to write to
/// and close the pipe before giving up on that one read.
///
/// The data crosses a local pipe with no network and no daemon in
/// between, so a source that behaves at all answers in milliseconds;
/// this is generous only to survive a loaded machine, not a slow peer.
/// CLAUDE.md: "Never wait on another process without a bound."
pub const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Opens a pipe, hands its write end to `do_receive` (the protocol's own
/// `offer.receive(mime, fd)` request), flushes the connection so that
/// request actually reaches the compositor, and reads the other end
/// back — capped at [`MAX_READ_BYTES`] and [`READ_TIMEOUT`].
///
/// Returns `None` on any failure (a pipe that could not be created, a
/// flush that failed, a timeout, or an I/O error while reading), having
/// logged what happened. A failed read must never bring the watcher
/// down: one uncooperative source is not a reason to stop watching
/// every copy after it.
///
/// The actual read runs on a spawned thread so *this* call can never
/// block longer than `READ_TIMEOUT`, even in the pathological case of a
/// source that opened its end of the pipe and then never writes or
/// closes it. That thread is then left blocked on a read that may never
/// return — a deliberate trade: a leaked thread against a watcher that
/// can itself hang forever on somebody else's clipboard source. It costs
/// nothing while idle and the case is rare enough that it is not worth a
/// second abstraction to reclaim it.
pub fn receive(
    connection: &Connection,
    do_receive: impl FnOnce(BorrowedFd<'_>),
) -> Option<Vec<u8>> {
    let (mut reader, writer) = match std::io::pipe() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = %e, "could not create a pipe to read a clipboard offer");
            return None;
        }
    };

    // `receive` requests borrow the fd rather than take it: like
    // libwayland's C API, sending it over the socket (via `SCM_RIGHTS`)
    // does not close this process's own copy, and this process must
    // close it explicitly once the request has actually gone out —
    // otherwise the source's own end is not the *only* writer left, and
    // a source that legitimately never writes anything would still
    // leave the pipe open forever from our side.
    use std::os::fd::AsFd;
    do_receive(writer.as_fd());

    // The `receive` request (including the fd) only actually reaches the
    // compositor once flushed — without this, the source client is
    // never told to start writing, and closing our copy below would
    // race the send.
    if let Err(e) = connection.flush() {
        tracing::warn!(error = %e, "could not flush a clipboard receive request");
        return None;
    }
    drop(writer);

    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("hyprforge-clip-read".to_string())
        .spawn(move || {
            let result: std::io::Result<Vec<u8>> = (|| {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    if buf.len() >= MAX_READ_BYTES {
                        break;
                    }
                    let n = reader.read(&mut chunk)?;
                    if n == 0 {
                        break;
                    }
                    let take = n.min(MAX_READ_BYTES - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                }
                Ok(buf)
            })();
            // The receiver may already be gone (we timed out and moved on);
            // that is not this thread's problem to report.
            let _ = tx.send(result);
        });
    if spawned.is_err() {
        tracing::warn!("could not spawn a thread to read a clipboard offer");
        return None;
    }

    match rx.recv_timeout(READ_TIMEOUT) {
        Ok(Ok(bytes)) => Some(bytes),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "reading a clipboard offer failed");
            None
        }
        Err(_) => {
            tracing::warn!("timed out reading a clipboard offer; the source may be stuck");
            None
        }
    }
}

/// How long a write side (this crate acting as a clipboard *source*)
/// waits for the paste target to drain what is written to it.
///
/// The mirror image of [`READ_TIMEOUT`], for the same reason: the reader
/// on the other end of a `send` event's fd is some other, possibly
/// misbehaving, application, and CLAUDE.md is just as explicit here —
/// "never wait on another process without a bound" applies to writing as
/// much as reading.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Writes `bytes` to `fd` (as handed to us by a `send` event on a data
/// source we own) and closes it, bounded by [`WRITE_TIMEOUT`].
///
/// Like [`receive`], the actual write runs on a spawned thread so this
/// call can never block longer than the timeout even against a reader
/// that opened its end and then never reads — that thread is left
/// writing into a pipe nobody drains, the same leaked-thread trade
/// `receive` documents. Failures (a reader that closed early, a stuck
/// reader that times out) are logged and otherwise swallowed: one
/// uncooperative paste target must not bring down the source that is
/// still serving the clipboard to everyone else.
pub fn send(fd: OwnedFd, bytes: Vec<u8>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("hyprforge-clip-write".to_string())
        .spawn(move || {
            let mut file = std::fs::File::from(fd);
            let result = file.write_all(&bytes);
            let _ = tx.send(result);
        });
    if spawned.is_err() {
        tracing::warn!("could not spawn a thread to write a clipboard offer");
        return;
    }
    match rx.recv_timeout(WRITE_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "writing a clipboard offer failed"),
        Err(_) => {
            tracing::warn!("timed out writing a clipboard offer; the reader may be stuck")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `receive()` needs a live `Connection` only to call `.flush()` on
    /// — no compositor-specific object is ever touched, so any
    /// compositor answering the socket does. But tier 1 promises to run
    /// with none at all, so a machine with no `$WAYLAND_DISPLAY` must
    /// say so rather than silently passing zero assertions: libtest has
    /// no skipped state (CLAUDE.md, and `hyprforge-bluetooth`'s
    /// `live_bluez.rs` for the same convention), so an unrunnable check
    /// announces itself with this marker instead of returning early.
    const SKIP_MARKER: &str = "HYPRFORGE-SKIP:";

    fn connection_or_skip() -> Option<Connection> {
        match Connection::connect_to_env() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!(
                    "{SKIP_MARKER} no Wayland compositor to flush a Connection against ({e})"
                );
                None
            }
        }
    }

    /// `send()` needs no compositor at all — the fd it writes to is
    /// already ours, handed over by whatever bound object received the
    /// `send` event — so this runs under tier 1 unconditionally, unlike
    /// the `receive()` tests above.
    #[test]
    fn a_reader_that_drains_the_pipe_gets_everything_written() {
        use std::os::fd::OwnedFd;

        let (mut reader, writer) = std::io::pipe().unwrap();
        let owned: OwnedFd = writer.into();
        let handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).unwrap();
            buf
        });
        send(owned, b"hello paste target".to_vec());
        assert_eq!(handle.join().unwrap(), b"hello paste target");
    }

    /// A reader that never drains the pipe must not hang `send()`
    /// forever — the exact resource CLAUDE.md's "never wait on another
    /// process without a bound" is about, mirrored from the read side.
    #[test]
    fn a_reader_that_never_drains_does_not_hang_the_write() {
        use std::os::fd::OwnedFd;

        let (reader, writer) = std::io::pipe().unwrap();
        let owned: OwnedFd = writer.into();
        // Keep the read end open (without reading) on a background
        // thread, standing in for a stuck paste target. A pipe's default
        // buffer is a handful of pages, so a write larger than that
        // blocks until timeout rather than completing instantly.
        let _keep_open = std::thread::spawn(move || {
            let _reader = reader;
            std::thread::sleep(Duration::from_secs(30));
        });
        let started = std::time::Instant::now();
        send(owned, vec![0u8; 8 * 1024 * 1024]);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "send() must return around WRITE_TIMEOUT, not wait out a stuck reader"
        );
    }

    /// Exercises the real pipe/thread/timeout plumbing end to end,
    /// standing in for the compositor with a plain `receive` that writes
    /// to the fd itself — the part that is actually testable without a
    /// real data-control offer.
    ///
    /// `#[ignore]`d rather than run under tier 1: it needs a live
    /// `Connection`, which tier 1 explicitly does not have. Run with
    /// `cargo test -p hyprforge-clipboard -- --ignored`.
    #[test]
    #[ignore = "live (the Wayland compositor's protocols): changes nothing"]
    fn a_source_that_writes_and_closes_is_read_back_in_full() {
        use std::io::Write;
        use std::os::fd::AsFd;

        let Some(connection) = connection_or_skip() else {
            return;
        };
        let bytes = receive(&connection, |fd| {
            // Standing in for the source application on the other end
            // of a real offer: `receive()` only lends us the fd for the
            // duration of this call (the real compositor would instead
            // duplicate it across the socket to an unrelated process),
            // so a clone is what lets a background thread write to it
            // after this closure returns.
            let owned = fd.as_fd().try_clone_to_owned().unwrap();
            std::thread::spawn(move || {
                let mut writer = std::fs::File::from(owned);
                writer.write_all(b"hello clipboard").unwrap();
                // Dropped here, closing this thread's copy — combined
                // with `receive()` closing its own after flush, that
                // gives the reader its EOF.
            });
        });
        assert_eq!(bytes, Some(b"hello clipboard".to_vec()));
    }

    #[test]
    #[ignore = "live (the Wayland compositor's protocols): changes nothing"]
    fn a_source_that_never_writes_times_out_rather_than_hanging() {
        use std::os::fd::AsFd;

        let Some(connection) = connection_or_skip() else {
            return;
        };
        let started = std::time::Instant::now();
        let bytes = receive(&connection, |fd| {
            // Keeps a cloned write end open on a background thread
            // instead of ever closing it, standing in for a source that
            // hung — the read side must still return, not block until
            // this process ends.
            let owned = fd.as_fd().try_clone_to_owned().unwrap();
            std::thread::spawn(move || {
                let _keep_open = owned;
                std::thread::sleep(Duration::from_secs(30));
            });
        });
        assert_eq!(bytes, None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "receive() must return around READ_TIMEOUT, not wait out the stuck source"
        );
    }
}
