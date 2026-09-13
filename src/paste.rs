//! Synthesising a paste (Ctrl+V) after the clipboard has been set.
//!
//! Setting the clipboard is only half of "act like Windows": the popup's
//! job is to land the selected entry in whatever application had focus,
//! and only a synthesized keypress can do that. This is a distinct
//! capability from [`crate::write::ClipboardWriter`] on purpose — a
//! compositor that has no `zwp_virtual_keyboard_manager_v1` can still
//! have a perfectly good `ext-data-control-v1`, and setting the
//! clipboard must succeed regardless of whether the keypress can follow
//! it. See [`crate::wayland::WaylandPaster`] for the real implementation
//! and why it can never fail to *construct*, only to *act*.

/// A modifier-plus-key combination this crate knows how to synthesize.
///
/// Deliberately just two fixed combinations rather than an arbitrary
/// modifier/keysym pair: this crate stays compositor- and
/// protocol-flavoured only, never Hyprland-flavoured — see the module
/// doc. *Which* combination a given focused window wants (a terminal
/// class wants Ctrl+Shift+V, everything else wants Ctrl+V) is a decision
/// about window classes, and window classes come from `hyprctl`, which
/// this crate must never shell out to. That decision lives in
/// `hyprforge-clipmenu` (`target::paste_shortcut`); this crate only
/// knows how to *send* whichever one it is handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    /// Ctrl+V — what almost every non-terminal application uses.
    CtrlV,
    /// Ctrl+Shift+V — what terminal emulators use instead, since Ctrl+V
    /// is already claimed (historically for SIGQUIT-adjacent control
    /// codes) by the terminal itself.
    CtrlShiftV,
}

/// Whether a synthesized paste actually happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteOutcome {
    /// The requested [`Shortcut`] was queued and flushed to the
    /// compositor.
    ///
    /// This is **dispatched**, not **delivered** — it says only that the
    /// request left this process, nothing about whether the focused
    /// client actually received or acted on the keystrokes. Proven live:
    /// a synthesized combination reported `Dispatched` while the focused
    /// window received zero bytes. Do not read this as "the paste
    /// happened"; nothing in this crate can observe that.
    Dispatched,
    /// No virtual keyboard was available (or sending failed), so nothing
    /// was pressed. The clipboard is still set; the caller (the popup)
    /// is expected to tell the user to press the shortcut themselves.
    Unavailable,
}

/// Presses and releases a [`Shortcut`] in whatever surface currently has
/// keyboard focus.
///
/// Never returns an error: a compositor that does not support this is
/// not a failure of the paste popup, only of this one optional step, so
/// the trait itself has no way to fail — only to report
/// [`PasteOutcome::Unavailable`].
pub trait PasteSynthesizer: Send + Sync {
    fn paste(&self, shortcut: Shortcut) -> PasteOutcome;
}

#[cfg(any(test, feature = "mock"))]
pub mod mock {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Reports a fixed outcome and counts calls (and remembers the last
    /// [`Shortcut`] it was asked for) — for the popup's own tests,
    /// standing in for a compositor with or without the protocol.
    pub struct MockPaster {
        outcome: PasteOutcome,
        calls: AtomicUsize,
        last_shortcut: Mutex<Option<Shortcut>>,
    }

    impl MockPaster {
        pub fn available() -> Self {
            MockPaster {
                outcome: PasteOutcome::Dispatched,
                calls: AtomicUsize::new(0),
                last_shortcut: Mutex::new(None),
            }
        }

        /// Stands in for a compositor with no virtual-keyboard protocol.
        pub fn unavailable() -> Self {
            MockPaster {
                outcome: PasteOutcome::Unavailable,
                calls: AtomicUsize::new(0),
                last_shortcut: Mutex::new(None),
            }
        }

        pub fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        pub fn last_shortcut(&self) -> Option<Shortcut> {
            *self.last_shortcut.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    impl PasteSynthesizer for MockPaster {
        fn paste(&self, shortcut: Shortcut) -> PasteOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_shortcut.lock().unwrap_or_else(|e| e.into_inner()) = Some(shortcut);
            self.outcome
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn an_unavailable_mock_reports_unavailable_and_still_counts_the_call() {
            let paster = MockPaster::unavailable();
            assert_eq!(paster.paste(Shortcut::CtrlV), PasteOutcome::Unavailable);
            assert_eq!(paster.call_count(), 1);
        }

        #[test]
        fn an_available_mock_reports_dispatched() {
            let paster = MockPaster::available();
            assert_eq!(paster.paste(Shortcut::CtrlV), PasteOutcome::Dispatched);
        }

        #[test]
        fn the_mock_remembers_which_shortcut_it_was_asked_for() {
            let paster = MockPaster::available();
            paster.paste(Shortcut::CtrlShiftV);
            assert_eq!(paster.last_shortcut(), Some(Shortcut::CtrlShiftV));
        }
    }
}
