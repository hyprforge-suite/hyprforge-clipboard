//! Asking `hyprforge-clipd` to pin or unpin an entry.
//!
//! Split out the same way `chooser::Chooser` is, and for the same
//! reason: a small trait plus a mock means `surface.rs`'s dispatch logic
//! — what a keypress *means* — can be tested with no daemon, and no
//! socket, involved at all. [`Wired`] below is the one place this
//! crate's `hyprforge_clipboard::ipc` client is actually called.
//!
//! `hyprforge-clipd` is deliberately the only writer of the history file
//! (see `hyprforge_clipboard::ipc`'s own module doc) — nothing here, or
//! in `hyprforge_clipboard::ipc::set_pinned`, ever touches that file.
//! This only ever asks the daemon to.

use hyprforge_clipboard::ipc;

/// Whatever it takes to ask the daemon to change one entry's pin state.
pub trait Pinner {
    /// Pins or unpins `id`. `Err` is a message fit to show the user —
    /// see [`describe`] for what it can say and, just as importantly,
    /// what it never says: an id (a content hash) is safe to name, but
    /// nothing here ever carries a clipboard entry's actual content.
    fn set_pinned(&self, id: &str, pinned: bool) -> Result<(), String>;
}

/// **The seam**: the real implementation, over
/// `hyprforge_clipboard::ipc`'s blocking client.
pub struct Wired;

impl Pinner for Wired {
    fn set_pinned(&self, id: &str, pinned: bool) -> Result<(), String> {
        ipc::set_pinned(id, pinned).map_err(describe)
    }
}

/// Turns a [`ipc::ClientError`] into a message fit for the popup's own
/// header — see `view.rs`.
///
/// The three "couldn't even ask" variants collapse into one message
/// naming the daemon rather than three subtly different ones a user has
/// no way to act on differently: CLAUDE.md's rule is that a service
/// which is not running is a state with *a* message, not that every
/// distinct way of not reaching it needs its own wording. [`ipc::ClientError::Refused`]
/// is the one case with something more specific to say, so it is passed
/// through as the daemon wrote it — still never the clipboard content,
/// only an id and a reason, per that module's own rule.
fn describe(err: ipc::ClientError) -> String {
    match err {
        ipc::ClientError::NoRuntimeDir | ipc::ClientError::Unreachable(_) | ipc::ClientError::Timeout => {
            "hyprforge-clipd isn't running, so pinning isn't available right now".to_string()
        }
        ipc::ClientError::Malformed => {
            "hyprforge-clipd sent a response this popup couldn't understand".to_string()
        }
        ipc::ClientError::Refused(message) => message,
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::cell::RefCell;

    /// Records exactly what it was asked to pin/unpin, so a test can
    /// assert on it without a socket or a daemon.
    pub struct MockPinner {
        pub calls: RefCell<Vec<(String, bool)>>,
        pub result: Result<(), String>,
    }

    impl MockPinner {
        pub fn succeeding() -> Self {
            MockPinner { calls: RefCell::new(Vec::new()), result: Ok(()) }
        }

        /// Simulates the daemon not being reachable at all — the
        /// message a real [`describe`] would have produced for
        /// [`ipc::ClientError::Unreachable`], so a test exercising this
        /// mock is exercising the same wording a user would actually
        /// see.
        pub fn no_daemon() -> Self {
            MockPinner {
                calls: RefCell::new(Vec::new()),
                result: Err(
                    "hyprforge-clipd isn't running, so pinning isn't available right now".to_string(),
                ),
            }
        }

        pub fn failing(message: &str) -> Self {
            MockPinner { calls: RefCell::new(Vec::new()), result: Err(message.to_string()) }
        }
    }

    impl Pinner for MockPinner {
        fn set_pinned(&self, id: &str, pinned: bool) -> Result<(), String> {
            self.calls.borrow_mut().push((id.to_string(), pinned));
            self.result.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_way_of_not_reaching_the_daemon_reads_as_not_running() {
        for err in [
            ipc::ClientError::NoRuntimeDir,
            ipc::ClientError::Unreachable("connection refused".to_string()),
            ipc::ClientError::Timeout,
        ] {
            assert_eq!(
                describe(err),
                "hyprforge-clipd isn't running, so pinning isn't available right now"
            );
        }
    }

    #[test]
    fn a_refusal_from_the_daemon_is_passed_through_verbatim() {
        let message = "no clipboard entry with id abc123".to_string();
        assert_eq!(describe(ipc::ClientError::Refused(message.clone())), message);
    }

    #[test]
    fn a_malformed_response_says_so_rather_than_naming_the_daemon_as_down() {
        assert_eq!(
            describe(ipc::ClientError::Malformed),
            "hyprforge-clipd sent a response this popup couldn't understand"
        );
    }
}
