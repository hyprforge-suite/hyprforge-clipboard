//! Putting content back on the clipboard — the write side of the boundary
//! [`crate::backend`] draws for reading it.
//!
//! Same shape as `backend.rs`: a trait plus a mock so a popup can be
//! tested without a compositor, and the real implementation
//! ([`crate::wayland::WaylandWriter`]) lives behind it. The two pure
//! functions here ([`mimes_to_offer`] and [`bytes_for`]) are what decide
//! *what* to advertise and *which* bytes answer which offer — the part
//! most worth testing without any Wayland involved at all — and both the
//! real writer and the mock call them, so a test on either is a test of
//! the same decision.
//!
//! # Never the primary selection
//!
//! `ext-data-control-v1` and `zwlr-data-control-v1` can each also set the
//! X11-style "whatever is currently highlighted" selection
//! (`set_primary_selection`). Nothing in this module or
//! `crate::wayland::write_ext`/`write_wlr` ever calls it — only
//! `set_selection`, the ordinary clipboard. Setting the primary selection
//! from a clipboard manager would mean every popup selection also
//! silently overwrites whatever text the user has merely highlighted
//! elsewhere, which is not what "paste this" was asking for.

use crate::types::{Content, Mime};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Puts `content` on the clipboard.
///
/// Implementations own a data source that must **stay alive** for as
/// long as this remains the current selection — see
/// `crate::wayland::write_ext`'s module doc for why a source dropped
/// right after this call returns would leave the clipboard empty. This
/// method itself only has to make the compositor accept the new
/// selection; keeping the source alive afterward is the implementation's
/// job — it does that by handing the caller a [`SelectionGuard`] rather
/// than by keeping the *process* alive on its own, since only the
/// caller knows when it is done needing the source (typically: after
/// synthesizing a paste). **The corollary is the caller's job, not this
/// trait's**: a process that calls `set_selection` and then exits
/// without waiting on the guard destroys the source out from under
/// whoever tries to paste a moment later — see
/// `crates/hyprforge-clipmenu/src/chooser.rs` for where that wait
/// happens for the real popup.
pub trait ClipboardWriter: Send + Sync {
    type Guard: SelectionGuard;

    fn set_selection(&self, content: Content) -> anyhow::Result<Self::Guard>;
}

/// What became of a selection after [`ClipboardWriter::set_selection`]
/// returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOutcome {
    /// The source answered at least one `Send` request — some client
    /// actually read the clipboard. This is what a paste, synthetic or
    /// manual, looks like from here.
    Served,
    /// The compositor reported `Cancelled`: another data source has
    /// become the selection instead (most commonly the *next*
    /// invocation of this same popup calling `set_selection` again).
    /// Whatever this source would have served no longer matters to
    /// anyone.
    Superseded,
    /// Neither happened before the caller's bound elapsed. The content
    /// is still nominally the selection — nothing here un-set it — but
    /// no compositor-cached copy exists once the owning process exits,
    /// so a caller that gives up here and exits risks the next paste
    /// attempt finding an empty clipboard.
    TimedOut,
}

/// The other half of the promise `set_selection` used to make on its
/// own: a way for the caller to learn when the source it just created is
/// no longer needed, so the process can exit without either abandoning
/// a paste in flight or lingering forever waiting for one that will
/// never come.
pub trait SelectionGuard: Send {
    /// Blocks the calling thread until [`SelectionOutcome::Served`] or
    /// [`SelectionOutcome::Superseded`] is known, or until `timeout`
    /// elapses (reported as [`SelectionOutcome::TimedOut`]). Always
    /// returns — never blocks forever, per CLAUDE.md's rule against
    /// waiting on another process (here, another compositor client)
    /// without a bound.
    fn wait(&self, timeout: Duration) -> SelectionOutcome;
}

/// Shared between a source's background dispatch thread (which calls
/// [`Self::signal`] once it knows the outcome) and the [`SelectionGuard`]
/// handed back to the caller (which calls [`Self::wait`]). Pure
/// synchronization over a `Condvar` — no Wayland involved — which is
/// exactly what makes it testable without a compositor; see the tests
/// below.
///
/// `pub` rather than `pub(crate)` only because [`crate::wayland::WaylandWriter`]
/// names `Arc<Waiter>` as [`ClipboardWriter::Guard`], which an
/// associated type cannot expose as anything less visible than the
/// trait impl itself; nothing outside this crate constructs one.
#[derive(Default)]
pub struct Waiter {
    outcome: Mutex<Option<SelectionOutcome>>,
    condvar: Condvar,
}

impl Waiter {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records `outcome`, waking anyone blocked in [`Self::wait`].
    ///
    /// Only the *first* signal counts: `Served` can fire more than once
    /// (a target can ask for more than one MIME type in the same
    /// paste) and a `Cancelled` can in principle arrive after a `Send`
    /// this same source already answered. Whichever happened first is
    /// the one a caller waiting on this needs to hear — overwriting it
    /// with whatever comes after would let a late `Cancelled` mask an
    /// already-successful `Served` that a caller may already have acted
    /// on having been woken by it.
    pub(crate) fn signal(&self, outcome: SelectionOutcome) {
        let mut guard = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(outcome);
            self.condvar.notify_all();
        }
    }
}

impl SelectionGuard for Arc<Waiter> {
    fn wait(&self, timeout: Duration) -> SelectionOutcome {
        let guard = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _timed_out) = self
            .condvar
            .wait_timeout_while(guard, timeout, |outcome| outcome.is_none())
            .unwrap_or_else(|e| e.into_inner());
        guard.unwrap_or(SelectionOutcome::TimedOut)
    }
}

/// Which MIME types to advertise for a piece of content.
///
/// Text offers both `text/plain;charset=utf-8` (what most modern
/// applications look for first) and plain `text/plain` (what the rest
/// still look for) — this crate's own `resolve::select_mime` prefers the
/// former over the latter for exactly the same reason a paste target
/// might. An image offers only the one MIME type it was stored with:
/// re-encoding it as anything else is not something this crate does
/// anywhere, on the read side either.
pub fn mimes_to_offer(content: &Content) -> Vec<Mime> {
    match content {
        Content::Text(_) => vec![
            Mime::new("text/plain;charset=utf-8"),
            Mime::new("text/plain"),
        ],
        Content::Image { mime, .. } => vec![mime.clone()],
    }
}

/// The bytes to answer a `send` request for `requested`, or `None` when
/// `requested` is not one of [`mimes_to_offer`]'s own list for this
/// content — which should not happen against a well-behaved compositor,
/// since nothing else was ever advertised, but a source must still
/// answer *something* rather than write nothing at all to a paste
/// target's pipe.
pub fn bytes_for(content: &Content, requested: &Mime) -> Option<Vec<u8>> {
    match content {
        Content::Text(text) => mimes_to_offer(content)
            .contains(requested)
            .then(|| text.clone().into_bytes()),
        Content::Image { bytes, mime } => (requested == mime).then(|| bytes.clone()),
    }
}

/// Exactly what a data source advertises, and the bytes it answers each
/// type with.
///
/// [`mimes_to_offer`] and [`bytes_for`] decide this for clipboard
/// [`Content`] — text and images. Some callers have a different shape of
/// thing to put on the clipboard: a file manager's copied files are a
/// `text/uri-list`, an `x-special/gnome-copied-files` and a plain-text
/// fallback, all at once, none of which is `Content`. `Offers` is the
/// level both meet at, so the writers only ever serve this.
///
/// `Debug` shows the types and byte counts and never the bytes, for the
/// reason this crate's module doc gives: what is on a clipboard is
/// nobody's business in a log.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Offers {
    entries: Vec<(Mime, Vec<u8>)>,
}

impl Offers {
    /// These types, in this order — the order a paste target sees them
    /// offered, which some targets treat as a preference.
    pub fn new(entries: Vec<(Mime, Vec<u8>)>) -> Self {
        Offers { entries }
    }

    /// What [`ClipboardWriter::set_selection`] offers for `content`.
    pub fn of(content: &Content) -> Self {
        Offers::new(
            mimes_to_offer(content)
                .into_iter()
                .map(|mime| {
                    let bytes = bytes_for(content, &mime).unwrap_or_default();
                    (mime, bytes)
                })
                .collect(),
        )
    }

    pub fn mimes(&self) -> impl Iterator<Item = &Mime> {
        self.entries.iter().map(|(mime, _)| mime)
    }

    /// The bytes for `requested`, or nothing for a type that was never
    /// offered — a source must still answer *something* rather than
    /// write nothing at all to a paste target's pipe.
    pub fn bytes_for(&self, requested: &Mime) -> Vec<u8> {
        self.entries
            .iter()
            .find(|(mime, _)| mime == requested)
            .map(|(_, bytes)| bytes.clone())
            .unwrap_or_default()
    }
}

impl std::fmt::Debug for Offers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.entries.iter().map(|(mime, bytes)| format!("{} ({} bytes)", mime.as_str(), bytes.len())))
            .finish()
    }
}

#[cfg(any(test, feature = "mock"))]
pub mod mock {
    use super::*;

    /// Records every call rather than talking to a compositor — for the
    /// popup's own tests. Never actually touches the machine's real
    /// clipboard.
    ///
    /// Reports [`SelectionOutcome::Served`] from every guard it hands
    /// out by default — a test exercising the "nothing ever asked for
    /// it" path can override that with [`Self::with_outcome`].
    pub struct MockWriter {
        calls: Mutex<Vec<Content>>,
        outcome: SelectionOutcome,
    }

    impl Default for MockWriter {
        fn default() -> Self {
            MockWriter {
                calls: Mutex::new(Vec::new()),
                outcome: SelectionOutcome::Served,
            }
        }
    }

    impl MockWriter {
        pub fn new() -> Self {
            Self::default()
        }

        /// A mock whose guards report `outcome` instead of the default
        /// `Served` — for testing a caller's reaction to `Superseded`
        /// or `TimedOut` without a compositor.
        pub fn with_outcome(outcome: SelectionOutcome) -> Self {
            MockWriter {
                calls: Mutex::new(Vec::new()),
                outcome,
            }
        }

        /// Every `set_selection` call so far, in order.
        pub fn calls(&self) -> Vec<Content> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        /// The MIME types the *last* call would have offered — what a
        /// test asserts to check the right types are advertised, without
        /// re-deriving `mimes_to_offer` itself.
        pub fn last_offered(&self) -> Option<Vec<Mime>> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .map(mimes_to_offer)
        }
    }

    /// A guard that reports a fixed, already-known outcome — standing in
    /// for a real [`Waiter`] without any thread or timing involved.
    pub struct MockGuard(SelectionOutcome);

    impl SelectionGuard for MockGuard {
        fn wait(&self, _timeout: Duration) -> SelectionOutcome {
            self.0
        }
    }

    impl ClipboardWriter for MockWriter {
        type Guard = MockGuard;

        fn set_selection(&self, content: Content) -> anyhow::Result<Self::Guard> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(content);
            Ok(MockGuard(self.outcome))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_for_text_are_what_set_selection_always_offered() {
        let content = Content::Text("hello".into());
        let offers = Offers::of(&content);
        let mimes: Vec<Mime> = offers.mimes().cloned().collect();
        assert_eq!(mimes, mimes_to_offer(&content));
        for mime in &mimes {
            assert_eq!(offers.bytes_for(mime), b"hello");
        }
    }

    #[test]
    fn a_type_that_was_not_offered_answers_with_nothing() {
        let offers = Offers::new(vec![(Mime::new("text/uri-list"), b"file:///a".to_vec())]);
        assert!(offers.bytes_for(&Mime::new("image/png")).is_empty());
    }

    /// What is on a clipboard never reaches a log through `Debug`.
    #[test]
    fn debugging_offers_shows_sizes_not_content() {
        let offers = Offers::new(vec![(Mime::new("text/plain"), b"hunter2".to_vec())]);
        let shown = format!("{offers:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("text/plain (7 bytes)"), "{shown}");
    }

    /// The property Part 1 of the task exists to pin: text offers both
    /// plain-text MIME types.
    #[test]
    fn setting_text_offers_both_plain_text_mime_types() {
        let offered = mimes_to_offer(&Content::Text("hello".to_string()));
        assert_eq!(
            offered,
            vec![
                Mime::new("text/plain;charset=utf-8"),
                Mime::new("text/plain"),
            ]
        );
    }

    /// An image offers exactly the type it was stored with — never
    /// re-typed as something else.
    #[test]
    fn setting_an_image_offers_only_the_type_it_was_stored_with() {
        let offered = mimes_to_offer(&Content::Image {
            bytes: vec![1, 2, 3],
            mime: Mime::new("image/png"),
        });
        assert_eq!(offered, vec![Mime::new("image/png")]);
    }

    #[test]
    fn bytes_for_answers_either_text_mime_with_the_same_bytes() {
        let content = Content::Text("hello".to_string());
        assert_eq!(
            bytes_for(&content, &Mime::new("text/plain;charset=utf-8")),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            bytes_for(&content, &Mime::new("text/plain")),
            Some(b"hello".to_vec())
        );
        assert_eq!(bytes_for(&content, &Mime::new("text/html")), None);
    }

    #[test]
    fn bytes_for_an_image_only_answers_its_own_mime() {
        let content = Content::Image {
            bytes: vec![9, 9, 9],
            mime: Mime::new("image/png"),
        };
        assert_eq!(
            bytes_for(&content, &Mime::new("image/png")),
            Some(vec![9, 9, 9])
        );
        assert_eq!(bytes_for(&content, &Mime::new("image/jpeg")), None);
    }

    /// The mock exists so the popup can assert what would have been
    /// offered without a compositor — this pins that it actually tracks
    /// calls and reports the right offer list per the shared function
    /// above, rather than a hand-rolled duplicate of the decision.
    #[test]
    fn the_mock_reports_what_the_last_call_would_have_offered() {
        let writer = mock::MockWriter::new();
        writer
            .set_selection(Content::Text("first".to_string()))
            .unwrap();
        writer
            .set_selection(Content::Image {
                bytes: vec![1],
                mime: Mime::new("image/png"),
            })
            .unwrap();
        assert_eq!(writer.calls().len(), 2);
        assert_eq!(writer.last_offered(), Some(vec![Mime::new("image/png")]));
    }

    /// A mock configured with a non-default outcome hands it out from
    /// every guard — this is what lets a caller's timeout-handling be
    /// tested without a compositor or a real timer.
    #[test]
    fn a_mock_writer_can_be_configured_to_report_timing_out() {
        let writer = mock::MockWriter::with_outcome(SelectionOutcome::TimedOut);
        let guard = writer
            .set_selection(Content::Text("x".to_string()))
            .unwrap();
        assert_eq!(guard.wait(Duration::from_secs(0)), SelectionOutcome::TimedOut);
    }

    /// The property the whole `Waiter` type exists for: a signal that
    /// arrives well before the bound wakes `wait` immediately rather
    /// than making it sit out the full timeout.
    #[test]
    fn a_waiter_wakes_as_soon_as_it_is_signalled() {
        let waiter = Waiter::new();
        let signaller = waiter.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            signaller.signal(SelectionOutcome::Served);
        });
        let started = std::time::Instant::now();
        let outcome = waiter.wait(Duration::from_secs(10));
        assert_eq!(outcome, SelectionOutcome::Served);
        // Generous margin over the 20ms sleep — this only has to prove
        // the wait ended long before the 10s bound, not pin an exact
        // wakeup latency.
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// With nobody ever signalling, `wait` still returns — the whole
    /// point of a bounded wait — rather than hanging forever.
    #[test]
    fn an_unsignalled_waiter_times_out_rather_than_blocking_forever() {
        let waiter = Waiter::new();
        let outcome = waiter.wait(Duration::from_millis(20));
        assert_eq!(outcome, SelectionOutcome::TimedOut);
    }

    /// Pins the "first signal wins" rule: a late `Cancelled` must not
    /// overwrite a `Served` a caller may already have been woken by and
    /// acted on.
    #[test]
    fn only_the_first_signal_is_kept() {
        let waiter = Waiter::new();
        waiter.signal(SelectionOutcome::Served);
        waiter.signal(SelectionOutcome::Superseded);
        assert_eq!(waiter.wait(Duration::from_millis(0)), SelectionOutcome::Served);
    }
}
