//! Putting a chosen entry where the rest of the desktop can see it.
//!
//! This crate does not implement the clipboard write itself. That lives
//! in `hyprforge-clipboard` (`ClipboardWriter` for "set the selection",
//! `PasteSynthesizer` for "synthesise Ctrl+V") — built by another agent
//! in parallel with this one, so it did not exist yet when this module
//! was started. Rather than guess at that API up front, this crate
//! defined its own small seam, [`Chooser`], and drove every test in this
//! crate through [`mock::MockChooser`]. `hyprforge-clipboard`'s write
//! side landed before this was finished, so [`Wired`] below is the one
//! place it plugs in — see its doc comment.
use hyprforge_clipboard::{ClipboardWriter, Entry, Shortcut};
use std::sync::Mutex;
use std::time::Duration;

/// How long [`Wired::choose`] waits, after synthesizing the paste, for
/// the source it just created to be either read (a real paste
/// happened) or superseded (someone else now owns the clipboard)
/// before giving up and letting the process exit anyway.
///
/// A real paste target reads the clipboard within milliseconds of
/// receiving the keypress that asks it to — this is generous well past
/// that, covering a loaded machine or a target that is momentarily busy
/// — while still being short enough that a popup which, for whatever
/// reason, never gets read from does not sit resident (and holding
/// keyboard focus) for anything a person would call "stuck". See
/// `hyprforge_clipboard::write`'s module doc for why this wait exists
/// at all: without it, this process would tear down the very thread
/// serving the clipboard the instant the synthetic Ctrl+V was sent,
/// which is the bug this constant exists to close.
const SELECTION_WAIT: Duration = Duration::from_secs(2);

/// Whatever it takes to act on a chosen entry: put it on the clipboard
/// and paste it into whatever had focus before the popup opened.
///
/// Two calls, not one, and the order between them is load-bearing. This
/// popup takes `KeyboardInteractivity::Exclusive` while it is on screen
/// (see `surface.rs`'s comment on that) — precisely so its own
/// keystrokes reach it rather than whatever had focus before it opened.
/// That is exactly backwards for a *synthesized* paste: if
/// [`Self::finish_paste`] ran right after [`Self::set_clipboard`], the
/// synthetic Ctrl+V would be delivered back to this still-focused
/// popup, not to the window the user meant to paste into. So the two
/// are separate calls, with the popup's own surface torn down (and the
/// compositor given a chance to actually hand focus back) strictly
/// between them — see `surface::ClipMenu::run`, the only place that
/// happens, for why a flush alone is not enough to prove it. Recombining
/// these into one call reproduces the bug this split exists to close;
/// `surface.rs`'s
/// `choosing_sets_the_clipboard_without_synthesizing_the_paste_yet` test
/// is pinned against exactly that.
pub trait Chooser {
    /// Puts `entry` on the clipboard. `Err` is a message fit to print to
    /// stderr — this crate has no UI left to show it in by the time the
    /// choice has been made, since choosing is what ends the popup.
    ///
    /// Must not synthesize a paste itself — see this trait's own doc.
    fn set_clipboard(&self, entry: &Entry) -> Result<(), String>;

    /// Synthesizes `shortcut` and waits (bounded) for the clipboard
    /// source `set_clipboard` created to be read or superseded.
    ///
    /// `shortcut` is decided by the caller (`main.rs`, via
    /// `target::paste_shortcut`) from the window that had focus before
    /// this popup opened — this trait only ever sends whatever it is
    /// handed, the same seam `hyprforge-clipboard`'s own
    /// `PasteSynthesizer` keeps between "which combination" and "how to
    /// press it".
    ///
    /// Call this only after `set_clipboard` returned `Ok`, and only
    /// after the caller has released whatever kept the popup's own
    /// surface holding keyboard focus — see this trait's own doc for
    /// why the order matters. There is nothing left to report as an
    /// error by this point (a compositor with no virtual-keyboard
    /// protocol is reported to stderr, not returned, matching the
    /// original `choose`'s behaviour), so this cannot fail.
    fn finish_paste(&self, shortcut: Shortcut);
}

/// **The seam**: the real implementation, over `hyprforge-clipboard`'s
/// write-side traits.
///
/// `writer` and `paster` each open their own short-lived Wayland
/// connection on demand (see `WaylandWriter::new` and
/// `WaylandPaster::connect`) rather than sharing the layer-shell
/// connection this popup draws with — the same reasoning
/// `hyprforge-tray::TrayIcon::register` gives for opening its own
/// connection per registration, and it means a paste failure can never
/// take the popup's own surface down with it.
///
/// Failing to *set* the clipboard is the only thing that reports as
/// `Err` here — that is the step a person cannot work around. A
/// compositor with no virtual-keyboard protocol still gets the content
/// onto the clipboard; [`PasteSynthesizer::paste`] reporting
/// [`PasteOutcome::Unavailable`] is not a failure of *choosing*, only of
/// the one optional step after it, so it is reported to stderr and
/// still returns `Ok`.
pub struct Wired {
    writer: hyprforge_clipboard::WaylandWriter,
    paster: hyprforge_clipboard::WaylandPaster,
    /// The guard `set_clipboard` got back from `set_selection`, held
    /// here so `finish_paste` — a separate call, on the same `&self` —
    /// can wait on it. `Mutex` rather than `RefCell` only because
    /// `Chooser` requires `Send`-friendly interior mutability by
    /// convention with the rest of this crate's writer types; there is
    /// never any real contention, since a popup only ever chooses once
    /// (see `main.rs`'s module doc).
    guard: Mutex<Option<<hyprforge_clipboard::WaylandWriter as ClipboardWriter>::Guard>>,
}

impl Wired {
    /// Connects the paste-synthesis side now, once, rather than per
    /// choice — a popup only ever chooses once before exiting (see
    /// `main.rs`'s module doc), but constructing early means a
    /// compositor with no virtual-keyboard protocol is discovered
    /// before the user has picked anything, in case that is ever worth
    /// surfacing differently.
    pub fn connect() -> anyhow::Result<Self> {
        Ok(Wired {
            writer: hyprforge_clipboard::WaylandWriter::new(),
            paster: hyprforge_clipboard::WaylandPaster::connect()?,
            guard: Mutex::new(None),
        })
    }
}

impl Chooser for Wired {
    fn set_clipboard(&self, entry: &Entry) -> Result<(), String> {
        // The daemon first, because on Wayland the selection is served
        // by the client that set it — so a selection this popup owns
        // dies when this popup exits, and the chosen entry can be pasted
        // exactly once. `hyprforge-clipd` is already resident and is
        // already the only writer of the history; holding the selection
        // open is the same job, and it is what makes a chosen entry
        // pasteable over and over without reopening the menu.
        match hyprforge_clipboard::ipc::set_clipboard(entry.id.as_str()) {
            Ok(()) => return Ok(()),
            // Nobody to ask. Not an error, and not a reason to refuse to
            // work: CLAUDE.md's rule is that a component runs alone, so
            // the popup owns the selection itself instead — for as long
            // as it lives, which is the one-paste behaviour above.
            // `NoRuntimeDir` counts as nobody-to-ask for the same
            // reason `Unreachable` does: there is no socket to try.
            //
            // And so does *any* other answer, which is the part that had
            // to be learned rather than reasoned out. A daemon left
            // running from before this command existed replies
            // "unknown variant `set-clipboard`" — it is listening, it
            // answers immediately, and it cannot do the one thing being
            // asked. Treating that as "the daemon said no" and refusing
            // meant a chosen entry reached the clipboard not at all,
            // which is strictly worse than the local fallback this arm
            // was already written to provide. An installed binary is not
            // a restarted daemon, and version skew across a socket is
            // normal rather than exceptional.
            //
            // This is the opposite call from pinning, deliberately.
            // There, a refusal means the history could not be saved and
            // the user has to hear it. Here, falling back costs only
            // persistence — the selection lives as long as this popup
            // instead of as long as the daemon — and the user still gets
            // their paste. Silence about that is wrong too, so the
            // reason is printed; it just is not a reason to do nothing.
            Err(reason) => {
                eprintln!(
                    "hyprforge-clipd didn't take the clipboard ({reason}); \
                     holding it here instead, so it will last only until this popup exits"
                );
            }
        }

        let guard = self
            .writer
            .set_selection(entry.content.clone())
            .map_err(|e| e.to_string())?;
        *self.guard.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
        Ok(())
    }

    fn finish_paste(&self, shortcut: Shortcut) {
        use hyprforge_clipboard::{PasteOutcome, PasteSynthesizer, SelectionGuard, SelectionOutcome};

        if self.paster.paste(shortcut) == PasteOutcome::Unavailable {
            let keys = match shortcut {
                Shortcut::CtrlV => "Ctrl+V",
                Shortcut::CtrlShiftV => "Ctrl+Shift+V",
            };
            eprintln!(
                "copied to the clipboard — this compositor has no virtual-keyboard \
                 protocol, so press {keys} yourself to paste it"
            );
        }

        // Taken, not just locked and read: this source is only ever
        // waited on once (`Chooser`'s contract has `finish_paste` called
        // exactly once per `set_clipboard`), and taking it makes that
        // true structurally rather than by convention.
        let Some(guard) = self.guard.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            // `finish_paste` called without a preceding successful
            // `set_clipboard` — not a call `Chooser`'s contract allows,
            // but there is nothing to wait on if it happens anyway
            // rather than a reason to panic.
            return;
        };

        // The source `set_selection` created is served by a background
        // thread that dies the instant this process exits — see
        // `hyprforge_clipboard::write`'s module doc. This process must
        // not return from `finish_paste` (and therefore must not let
        // `main` exit) until that source has either been read from
        // (`Served`) or superseded by someone else
        // (`Superseded`), or until a bound on that wait elapses —
        // never wait unboundedly on another compositor client per
        // CLAUDE.md.
        //
        // This also answers "does a second invocation leave two
        // processes running": a second `hyprforge-clipmenu` choosing
        // something calls `set_selection` again, which makes the
        // compositor cancel *this* source — that `Cancelled` event
        // reaches this process's still-running dispatch thread
        // regardless of what this thread is doing, and wakes this
        // `wait` with `Superseded` immediately. At most one popup process
        // should ever be waiting here at a time.
        match guard.wait(SELECTION_WAIT) {
            SelectionOutcome::Served | SelectionOutcome::Superseded => {}
            SelectionOutcome::TimedOut => {
                // The clipboard is still set — `set_selection` already
                // made the compositor accept it before this wait began
                // — but nothing has read it in `SELECTION_WAIT`, and no
                // compositor keeps a source's content once its owning
                // process exits. Warn rather than silently leaving:
                // never the content itself, only that this happened.
                eprintln!(
                    "copied to the clipboard, but nothing pasted it within {SELECTION_WAIT:?} — \
                     if the paste didn't happen, copy it again before pasting by hand"
                );
            }
        }
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use hyprforge_clipboard::EntryId;
    use std::cell::RefCell;

    /// Records exactly what it was asked to choose, so a test can assert
    /// on it without a clipboard or a compositor.
    pub struct MockChooser {
        pub calls: RefCell<Vec<EntryId>>,
        pub result: Result<(), String>,
        /// Every `Chooser` method this mock was actually asked to run,
        /// in call order — `"set_clipboard"` / `"finish_paste"`. This is
        /// the property the two-call split exists for: a test can assert
        /// that `finish_paste` never appears before `set_clipboard`, and
        /// (see `surface.rs`'s
        /// `choosing_sets_the_clipboard_without_synthesizing_the_paste_yet`)
        /// that nothing calls `finish_paste` at all until the caller
        /// explicitly does so.
        pub log: RefCell<Vec<&'static str>>,
    }

    impl MockChooser {
        pub fn succeeding() -> Self {
            MockChooser { calls: RefCell::new(Vec::new()), result: Ok(()), log: RefCell::new(Vec::new()) }
        }

        pub fn failing(message: &str) -> Self {
            MockChooser {
                calls: RefCell::new(Vec::new()),
                result: Err(message.to_string()),
                log: RefCell::new(Vec::new()),
            }
        }
    }

    impl Chooser for MockChooser {
        fn set_clipboard(&self, entry: &Entry) -> Result<(), String> {
            self.calls.borrow_mut().push(entry.id.clone());
            self.log.borrow_mut().push("set_clipboard");
            self.result.clone()
        }

        fn finish_paste(&self, _shortcut: Shortcut) {
            self.log.borrow_mut().push("finish_paste");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockChooser;
    use super::*;
    use hyprforge_clipboard::{Content, EntryId};

    fn entry(text: &str) -> Entry {
        let content = Content::Text(text.to_string());
        Entry { id: EntryId::of(&content), content, copied_at: 0, pinned: false }
    }

    /// The whole point of the mock: choosing hands the mock *exactly*
    /// the entry that was chosen, not some other one and not a copy that
    /// differs in id.
    #[test]
    fn choosing_an_entry_hands_exactly_that_entry_to_the_chooser() {
        let chooser = MockChooser::succeeding();
        let picked = entry("pick me");
        chooser.set_clipboard(&picked).unwrap();
        assert_eq!(chooser.calls.borrow().as_slice(), std::slice::from_ref(&picked.id));
    }

    #[test]
    fn a_failing_chooser_reports_its_message() {
        let chooser = MockChooser::failing("no seat");
        let err = chooser.set_clipboard(&entry("x")).unwrap_err();
        assert_eq!(err, "no seat");
    }

    /// Pins the order the bug fix depends on: whoever drives a full
    /// choice (`surface::finish_choice`, in real use) must call
    /// `set_clipboard` before `finish_paste`, never the reverse and
    /// never with something else running between the two that isn't
    /// tearing down the popup's own surface. This test would still pass
    /// if the two were called back-to-back with nothing in between —
    /// see `surface.rs`'s
    /// `choosing_sets_the_clipboard_without_synthesizing_the_paste_yet`
    /// for the test that catches *that* regression, which this one
    /// cannot.
    #[test]
    fn finishing_a_choice_runs_set_clipboard_before_finish_paste() {
        let chooser = MockChooser::succeeding();
        chooser.set_clipboard(&entry("x")).unwrap();
        chooser.finish_paste(Shortcut::CtrlV);
        assert_eq!(chooser.log.borrow().as_slice(), &["set_clipboard", "finish_paste"]);
    }
}
