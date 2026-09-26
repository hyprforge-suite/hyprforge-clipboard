//! Asking `hyprforge-clipd` to change the history: pin or unpin an entry,
//! or delete one.
//!
//! Split out the same way `chooser::Chooser` is, and for the same
//! reason: a small trait plus a mock means `surface.rs`'s dispatch logic
//! — what a keypress *means* — can be tested with no daemon, and no
//! socket, involved at all. [`Wired`] below is the one place this
//! crate's `hyprforge_clipboard::ipc` client is actually called.
//!
//! `hyprforge-clipd` is deliberately the only writer of the history file
//! (see `hyprforge_clipboard::ipc`'s own module doc) — nothing here, or
//! in `hyprforge_clipboard::ipc`, ever touches that file. This only ever
//! asks the daemon to, and the popup changes its own copy only once the
//! daemon has said yes.

use hyprforge_clipboard::ipc;

/// Whatever it takes to ask the daemon to change one entry.
///
/// `Err` is a message fit to show the user — see [`describe`] for what it
/// can say and, just as importantly, what it never says: an id (a content
/// hash) is safe to name, but nothing here ever carries a clipboard
/// entry's actual content.
pub trait Editor {
    /// Pins or unpins `id`.
    fn set_pinned(&self, id: &str, pinned: bool) -> Result<(), String>;

    /// Forgets `id`.
    fn remove(&self, id: &str) -> Result<(), String>;
}

/// **The seam**: the real implementation, over
/// `hyprforge_clipboard::ipc`'s blocking client.
pub struct Wired;

impl Editor for Wired {
    fn set_pinned(&self, id: &str, pinned: bool) -> Result<(), String> {
        ipc::set_pinned(id, pinned).map_err(|e| describe(e, "pinning"))
    }

    fn remove(&self, id: &str) -> Result<(), String> {
        ipc::remove_entry(id).map_err(|e| describe(e, "deleting"))
    }
}

/// Turns a [`ipc::ClientError`] into a message fit for the popup's own
/// notice line — see `view.rs`. `doing` names what could not happen
/// ("pinning", "deleting"), so a failed delete does not claim that
/// pinning is what broke.
///
/// The three "couldn't even ask" variants collapse into one message
/// naming the daemon rather than three subtly different ones a user has
/// no way to act on differently: CLAUDE.md's rule is that a service
/// which is not running is a state with *a* message, not that every
/// distinct way of not reaching it needs its own wording. [`ipc::ClientError::Refused`]
/// is the one case with something more specific to say, so it is passed
/// through as the daemon wrote it — still never the clipboard content,
/// only an id and a reason, per that module's own rule.
fn describe(err: ipc::ClientError, doing: &str) -> String {
    match err {
        ipc::ClientError::NoRuntimeDir | ipc::ClientError::Unreachable(_) | ipc::ClientError::Timeout => {
            format!("hyprforge-clipd isn't running, so {doing} isn't available right now")
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

    /// Records exactly what it was asked to change, so a test can assert
    /// on it without a socket or a daemon.
    pub struct MockEditor {
        pub calls: RefCell<Vec<(String, bool)>>,
        pub removed: RefCell<Vec<String>>,
        pub result: Result<(), String>,
    }

    impl MockEditor {
        pub fn succeeding() -> Self {
            MockEditor { calls: RefCell::new(Vec::new()), removed: RefCell::new(Vec::new()), result: Ok(()) }
        }

        /// Simulates the daemon not being reachable at all — the
        /// message a real [`describe`] would have produced for
        /// [`ipc::ClientError::Unreachable`] while pinning, so a test
        /// exercising this mock is exercising the same wording a user
        /// would actually see.
        pub fn no_daemon() -> Self {
            MockEditor {
                calls: RefCell::new(Vec::new()),
                removed: RefCell::new(Vec::new()),
                result: Err("hyprforge-clipd isn't running, so pinning isn't available right now".to_string()),
            }
        }

        pub fn failing(message: &str) -> Self {
            MockEditor {
                calls: RefCell::new(Vec::new()),
                removed: RefCell::new(Vec::new()),
                result: Err(message.to_string()),
            }
        }
    }

    impl Editor for MockEditor {
        fn set_pinned(&self, id: &str, pinned: bool) -> Result<(), String> {
            self.calls.borrow_mut().push((id.to_string(), pinned));
            self.result.clone()
        }

        fn remove(&self, id: &str) -> Result<(), String> {
            self.removed.borrow_mut().push(id.to_string());
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
            assert_eq!(describe(err, "pinning"), "hyprforge-clipd isn't running, so pinning isn't available right now");
        }
    }

    /// A failed delete must say deleting failed. One shared message for
    /// both would tell someone who pressed Delete that *pinning* is
    /// unavailable, which is true and useless.
    #[test]
    fn the_message_names_what_could_not_happen() {
        let message = describe(ipc::ClientError::Timeout, "deleting");
        assert_eq!(message, "hyprforge-clipd isn't running, so deleting isn't available right now");
    }

    #[test]
    fn a_refusal_from_the_daemon_is_passed_through_verbatim() {
        let message = "no clipboard entry with id abc123".to_string();
        assert_eq!(describe(ipc::ClientError::Refused(message.clone()), "pinning"), message);
    }

    #[test]
    fn a_malformed_response_says_so_rather_than_naming_the_daemon_as_down() {
        assert_eq!(
            describe(ipc::ClientError::Malformed, "pinning"),
            "hyprforge-clipd sent a response this popup couldn't understand"
        );
    }
}
