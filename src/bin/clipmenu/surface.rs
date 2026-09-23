//! What a keypress or a click *means* for this clipboard popup, and the
//! [`ClipApp`] that plugs that into `hyprforge-popup`'s generic
//! layer-shell machinery.
//!
//! The Wayland/iced plumbing this file used to contain — the
//! `smithay-client-toolkit` setup, the `calloop` event loop, the
//! `iced_tiny_skia` draw, the pointer and keyboard handlers, the
//! teardown-then-paste ordering — all moved to `hyprforge-popup::popup`
//! unchanged; see that module's own doc for why, and for the
//! [`hyprforge_popup::PopupApp`] seam [`ClipApp`] below implements. What
//! stayed is everything specific to a clipboard history: which
//! [`Action`] a key or a click means, and what happens when one is
//! dispatched against this popup's own [`Model`], [`Chooser`] and
//! [`Pinner`].

use crate::chooser::Chooser;
use crate::geometry::{Hit, RowLayout};
use crate::model::Model;
use crate::pinner::Pinner;
use crate::thumbnail;
use crate::view;
use hyprforge_look::Theme;
use hyprforge_popup::Keysym;
use iced_runtime::core::Element;
use std::convert::Infallible;

/// However the popup ended, as far as this crate's own logic is
/// concerned — everything else (the compositor closing the surface, the
/// connection dying) is `hyprforge_popup::Outcome::Closed`/`Disconnected`
/// and never reaches this type at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceOutcome {
    /// An entry was chosen and handed to the [`Chooser`] successfully.
    Chosen,
    /// Escape was pressed, or the [`Chooser`] failed and there was
    /// nothing more useful to do than close.
    Cancelled,
}

/// The paste shortcut this popup was already committed to before it ever
/// drew a frame — decided by `main.rs` (`target::paste_shortcut`) from
/// whatever window had focus before this popup's own surface stole it.
/// Threaded through as plain data so [`ClipApp::finish`] can hand it to
/// `Chooser::finish_paste` without knowing anything about window classes
/// or `hyprctl` itself.
pub use hyprforge_clipboard::Shortcut;

/// The clipboard popup's own [`hyprforge_popup::PopupApp`]: a live
/// [`Model`], a lazily-decoded thumbnail cache, and the two seams
/// (`Chooser`, `Pinner`) that reach the clipd daemon and the compositor.
pub struct ClipApp<C: Chooser> {
    model: Model,
    thumbnails: thumbnail::Cache,
    chooser: C,
    /// Boxed rather than a second generic parameter alongside `C`: this
    /// popup has exactly one production `Pinner` (`pinner::Wired`) and
    /// one mock, both zero-sized or nearly so, called at most once per
    /// invocation — the dynamic dispatch costs nothing worth avoiding,
    /// and it keeps `ClipApp<C>` from growing a second type parameter it
    /// would otherwise need to thread through everywhere.
    pinner: Box<dyn Pinner>,
    shortcut: Shortcut,
    /// The popup's own fixed width — needed for the scrollbar's track,
    /// which sits at a fixed offset from the *right* edge (see
    /// `geometry::RowLayout::scrollbar`), the same reason
    /// `hyprforge-emojimenu::popup_app::EmojiApp` keeps its own copy.
    /// [`Self::pointer_drag_start`]/[`Self::pointer_drag_move`] need it
    /// and are not handed one directly the way [`Self::pointer_click`]
    /// is (a click can afford the redundancy; a drag's own trait methods
    /// do not carry a width parameter at all).
    width: f64,
    /// The pointer's own y position as of the last drag event — `None`
    /// whenever no scrollbar-thumb drag is in progress. Set by
    /// [`Self::pointer_drag_start`], updated by every
    /// [`Self::pointer_drag_move`], cleared by [`Self::pointer_drag_end`]
    /// — what turns a drag's *absolute* pointer position into the
    /// *delta* `hyprforge_popup::Scrollbar::drag_delta_to_offset_delta`
    /// wants.
    drag_last_y: Option<f64>,
}

impl<C: Chooser> ClipApp<C> {
    pub fn new(model: Model, chooser: C, pinner: impl Pinner + 'static, shortcut: Shortcut, width: f64) -> ClipApp<C> {
        ClipApp { model, thumbnails: thumbnail::Cache::new(), chooser, pinner: Box::new(pinner), shortcut, width, drag_last_y: None }
    }
}

impl<C: Chooser + 'static> hyprforge_popup::PopupApp for ClipApp<C> {
    type Outcome = ChoiceOutcome;

    fn view<'a>(&'a mut self, theme: &'a Theme, now: u64, width: f64) -> Element<'a, Infallible, iced_widget::Theme, iced_tiny_skia::Renderer> {
        view::view(&self.model, theme, &mut self.thumbnails, now, width)
    }

    fn rows_that_fit(&self, theme: &Theme, height: f64) -> usize {
        RowLayout::for_font_size(theme.font_size).rows_that_fit(height)
    }

    /// See `hyprforge_popup::PopupApp::pointer_move`'s own doc for the
    /// coordinate space this reads. Landing outside every row (the
    /// header, the padding, a gap between rows) does nothing rather than
    /// clearing the selection — the same reasoning `move_selection` uses
    /// at the ends of the list: leaving via the header does not mean
    /// "select nothing".
    fn pointer_move(&mut self, theme: &Theme, position: (f64, f64)) -> bool {
        let layout = RowLayout::for_font_size(theme.font_size);
        let range = self.model.visible_range();
        let row = layout.row_at(position.1, range.len(), self.model.scroll_remainder());
        if let Some(row) = row {
            dispatch_action(&mut self.model, &self.chooser, self.pinner.as_ref(), Action::Select(range.start + row));
        }
        row.is_some()
    }

    /// Re-hit-tests at the click position first (rather than trusting
    /// the last hover): a click that lands between a hover event and a
    /// redraw must still be honest about which row, and which *part* of
    /// that row, it is actually over.
    ///
    /// [`RowLayout::hit_test`] answers that "which part": a hit on the
    /// pin toggle selects the row and toggles its pin — the exact same
    /// [`Action::TogglePin`] F2 already sends, never a second route to
    /// pinning — and a hit anywhere else on the row selects and chooses
    /// it.
    fn pointer_click(&mut self, theme: &Theme, width: f64, position: (f64, f64)) -> Option<ChoiceOutcome> {
        let layout = RowLayout::for_font_size(theme.font_size);
        let range = self.model.visible_range();
        let hit = layout.hit_test(width, position, range.len(), self.model.scroll_remainder())?;
        dispatch_action(&mut self.model, &self.chooser, self.pinner.as_ref(), Action::Select(range.start + hit.row()));
        let action = match hit {
            Hit::Pin(_) => Action::TogglePin,
            Hit::Row(_) => Action::Choose,
        };
        dispatch_action(&mut self.model, &self.chooser, self.pinner.as_ref(), action)
    }

    /// Scrolls the *view*, not the selection — continuous pixels rather
    /// than snapping the selection (and the window with it) a whole row
    /// at a time the way this used to route through [`Action::Move`].
    /// `rows` is already turned into whole notches by
    /// `hyprforge_popup::scroll_rows`; each notch moves the view by one
    /// row's own stride, which reads as smooth continuous motion under a
    /// touchpad's many small notches even though any single notch is
    /// still row-sized.
    fn pointer_scroll(&mut self, rows: i32) {
        let stride = self.model.row_stride();
        self.model.scroll_by(rows as f64 * stride);
    }

    /// A left-button press landed at `position` — starts a scrollbar-thumb
    /// drag if it landed on the thumb, otherwise leaves the press to
    /// resolve as an ordinary click exactly as it always has (this popup
    /// never opts into long-press detection either).
    fn pointer_drag_start(&mut self, theme: &Theme, position: (f64, f64)) -> bool {
        let layout = RowLayout::for_font_size(theme.font_size);
        let bar = layout.scrollbar(self.width, self.model.viewport_height());
        let content_height = layout.content_height(self.model.filtered().len());
        if bar.hit_thumb(position, content_height, self.model.scroll_offset()) {
            self.drag_last_y = Some(position.1);
            true
        } else {
            false
        }
    }

    fn pointer_drag_move(&mut self, theme: &Theme, position: (f64, f64)) {
        let Some(last_y) = self.drag_last_y else { return };
        self.drag_last_y = Some(position.1);
        let layout = RowLayout::for_font_size(theme.font_size);
        let bar = layout.scrollbar(self.width, self.model.viewport_height());
        let content_height = layout.content_height(self.model.filtered().len());
        let delta = bar.drag_delta_to_offset_delta(position.1 - last_y, content_height);
        self.model.scroll_by(delta);
    }

    fn pointer_drag_end(&mut self) {
        self.drag_last_y = None;
    }

    fn key(&mut self, keysym: Keysym, utf8: Option<String>) -> Option<ChoiceOutcome> {
        dispatch_key(&mut self.model, &self.chooser, self.pinner.as_ref(), keysym, utf8)
    }

    /// Only a completed choice has a paste left to synthesize — see
    /// [`Self::finish`].
    fn needs_finish(outcome: ChoiceOutcome) -> bool {
        outcome == ChoiceOutcome::Chosen
    }

    /// Runs only after `hyprforge-popup` has torn this popup's own
    /// surface down and proven the compositor processed that — see
    /// `hyprforge_popup::PopupApp::finish`'s own doc for why the ordering
    /// matters. `Action::Choose` (see [`dispatch_action`]) only put the
    /// entry on the clipboard; synthesizing the paste is this method's
    /// job, and this method's alone.
    fn finish(&mut self, outcome: ChoiceOutcome) {
        if outcome == ChoiceOutcome::Chosen {
            self.chooser.finish_paste(self.shortcut);
        }
    }
}

/// What the user asked the popup to do, independent of whether a key or a
/// pointer produced it. The seam that keeps mouse and keyboard handling
/// from growing two different sets of rules for the same outcome:
/// `dispatch_key` below turns a keysym into one of these, `ClipApp`'s
/// pointer handling turns a hit-tested row or a click into another, and
/// [`dispatch_action`] is the one place either ends up.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    /// Move the selection by this many rows (negative is up) — Up/Down,
    /// or the wheel.
    Move(i32),
    /// Select exactly this row of the filtered list — the pointer
    /// entering or moving over it.
    Select(usize),
    /// Choose whatever is currently selected — Enter, or a click.
    Choose,
    /// Close without choosing anything — Escape.
    Cancel,
    Backspace,
    Type(char),
    /// Pin the selected entry if it is not pinned, or unpin it if it
    /// is — F2, the one keybind this popup has beyond filtering and
    /// choosing.
    TogglePin,
}

/// The one place any [`Action`] takes effect on a [`Model`] and a
/// [`Chooser`]. Split out so every rule about what a row selection, a
/// click or a keystroke *means* can be tested without a Wayland
/// connection — the same split `hyprforge-lock::surface::dispatch_key`
/// uses for the same reason, generalised here to cover the pointer too.
fn dispatch_action<C: Chooser>(
    model: &mut Model,
    chooser: &C,
    pinner: &dyn Pinner,
    action: Action,
) -> Option<ChoiceOutcome> {
    // Any action at all supersedes a stale pin notice from an earlier
    // attempt — see `Model::pin_notice`'s own doc for why this is
    // cleared this aggressively rather than left to linger until the
    // next successful pin. `Action::TogglePin` below still gets the
    // final say: if this new attempt also fails, it sets its own notice
    // right back.
    model.set_pin_notice(None);
    match action {
        Action::Cancel => Some(ChoiceOutcome::Cancelled),
        Action::Choose => {
            // Only puts the entry on the clipboard. Synthesizing the
            // paste is `ClipApp::finish`'s job, called by
            // `hyprforge-popup` — never from here — once this popup's
            // own surface has been torn down; see both docs for why the
            // split exists. `ChoiceOutcome::Chosen` ends the event loop
            // this returns into, which is what makes that ordering
            // possible in the first place.
            let entry = model.selected_entry()?;
            match chooser.set_clipboard(&entry) {
                Ok(()) => Some(ChoiceOutcome::Chosen),
                Err(message) => {
                    eprintln!("couldn't put the chosen entry on the clipboard: {message}");
                    Some(ChoiceOutcome::Cancelled)
                }
            }
        }
        Action::Move(delta) => {
            model.move_selection(delta);
            None
        }
        Action::Select(index) => {
            model.select(index);
            None
        }
        Action::Backspace => {
            model.backspace();
            None
        }
        Action::Type(c) => {
            model.type_char(c);
            None
        }
        Action::TogglePin => {
            // Nothing selected (an empty filtered list) is nothing to
            // pin — the same "no-op, not an error" `Action::Choose`
            // above uses for the same situation.
            let entry = model.selected_entry()?;
            let new_pinned = !entry.pinned;
            match pinner.set_pinned(entry.id.as_str(), new_pinned) {
                // The daemon is still the only writer of the history
                // file (see `hyprforge_clipboard::ipc`'s module doc) —
                // this only updates the popup's own in-memory copy,
                // once the daemon has already agreed to the change.
                Ok(()) => model.set_entry_pinned(&entry.id, new_pinned),
                // Never applied locally on failure: doing so would make
                // the row look pinned when it is not, on disk or in the
                // daemon's own memory — exactly the "must not look like
                // it worked" failure this popup has to avoid.
                Err(message) => model.set_pin_notice(Some(message)),
            }
            None
        }
    }
}

/// The keystroke rules: which [`Action`] each key produces.
///
/// Nothing typed here is ever logged, matched on for its value, or
/// otherwise inspected beyond being appended to the filter — a
/// clipboard history search is not a password, but this crate follows
/// the same rule regardless of what the field means.
fn dispatch_key<C: Chooser>(
    model: &mut Model,
    chooser: &C,
    pinner: &dyn Pinner,
    keysym: Keysym,
    utf8: Option<String>,
) -> Option<ChoiceOutcome> {
    match keysym {
        Keysym::Escape => dispatch_action(model, chooser, pinner, Action::Cancel),
        Keysym::Return | Keysym::KP_Enter => dispatch_action(model, chooser, pinner, Action::Choose),
        Keysym::Up => dispatch_action(model, chooser, pinner, Action::Move(-1)),
        Keysym::Down => dispatch_action(model, chooser, pinner, Action::Move(1)),
        Keysym::BackSpace => dispatch_action(model, chooser, pinner, Action::Backspace),
        // F2 rather than a printable character or a modifier combo:
        // every printable keystroke already means "append to the
        // filter" (see the fallback arm below), and modifiers are not
        // even tracked here, so a Ctrl+P-style binding cannot be
        // recognised without wiring that up. F2 is free, conventional
        // for "rename/toggle a property of the selected row" elsewhere,
        // and cannot collide with typing a search term.
        Keysym::F2 => dispatch_action(model, chooser, pinner, Action::TogglePin),
        _ => {
            if let Some(text) = utf8 {
                for c in text.chars().filter(|c| !c.is_control()) {
                    dispatch_action(model, chooser, pinner, Action::Type(c));
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chooser::mock::MockChooser;
    use crate::model::HistoryState;
    use crate::pinner::mock::MockPinner;
    use hyprforge_clipboard::{Content, Entry, EntryId};

    fn entry(text: &str) -> Entry {
        let content = Content::Text(text.to_string());
        Entry { id: EntryId::of(&content), content, copied_at: 0, pinned: false }
    }

    fn model_with(texts: &[&str]) -> Model {
        Model::new(HistoryState::Loaded(texts.iter().map(|s| entry(s)).collect()))
    }

    #[test]
    fn escape_cancels_without_choosing_anything() {
        let mut model = model_with(&["a"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        let outcome = dispatch_key(&mut model, &chooser, &pinner, Keysym::Escape, None);
        assert_eq!(outcome, Some(ChoiceOutcome::Cancelled));
        assert!(chooser.calls.borrow().is_empty());
    }

    #[test]
    fn enter_chooses_the_selected_entry_and_ends_the_popup() {
        let mut model = model_with(&["a", "b"]);
        model.move_selection(1);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        let outcome = dispatch_key(&mut model, &chooser, &pinner, Keysym::Return, None);
        assert_eq!(outcome, Some(ChoiceOutcome::Chosen));
        assert_eq!(chooser.calls.borrow().as_slice(), &[EntryId::of(&Content::Text("b".into()))]);
    }

    /// Regression test for the bug this fix closes. `Action::Choose`
    /// must only put the entry on the clipboard — synthesizing the
    /// paste is a separate step (`ClipApp::finish`, via
    /// `hyprforge-popup`) that only runs after this popup's own layer
    /// surface has been torn down. If this test ever sees
    /// `"finish_paste"` in the log, `dispatch_key`/`dispatch_action`
    /// started calling it directly again — which welds the two calls
    /// back together while this popup still holds
    /// `KeyboardInteractivity::Exclusive`, delivering the synthesized
    /// Ctrl+V back to the popup instead of the window the user meant to
    /// paste into.
    #[test]
    fn choosing_sets_the_clipboard_without_synthesizing_the_paste_yet() {
        let mut model = model_with(&["a"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        let outcome = dispatch_key(&mut model, &chooser, &pinner, Keysym::Return, None);
        assert_eq!(outcome, Some(ChoiceOutcome::Chosen));
        assert_eq!(chooser.log.borrow().as_slice(), &["set_clipboard"]);
    }

    #[test]
    fn enter_with_an_empty_list_does_nothing() {
        let mut model = Model::new(HistoryState::Loaded(Vec::new()));
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        assert_eq!(dispatch_key(&mut model, &chooser, &pinner, Keysym::Return, None), None);
        assert!(chooser.calls.borrow().is_empty());
    }

    #[test]
    fn a_failing_choose_still_ends_the_popup_rather_than_hanging_open() {
        let mut model = model_with(&["a"]);
        let chooser = MockChooser::failing("no seat");
        let pinner = MockPinner::succeeding();
        let outcome = dispatch_key(&mut model, &chooser, &pinner, Keysym::Return, None);
        assert_eq!(outcome, Some(ChoiceOutcome::Cancelled));
    }

    #[test]
    fn up_and_down_move_the_selection_through_the_key_dispatcher() {
        let mut model = model_with(&["a", "b", "c"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        dispatch_key(&mut model, &chooser, &pinner, Keysym::Down, None);
        assert_eq!(model.selected_index(), 1);
        dispatch_key(&mut model, &chooser, &pinner, Keysym::Up, None);
        assert_eq!(model.selected_index(), 0);
    }

    #[test]
    fn typing_reaches_the_filter() {
        let mut model = model_with(&["alpha", "beta"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        dispatch_key(&mut model, &chooser, &pinner, Keysym::NoSymbol, Some("a".to_string()));
        dispatch_key(&mut model, &chooser, &pinner, Keysym::NoSymbol, Some("l".to_string()));
        assert_eq!(model.filter_text(), "al");
        assert_eq!(model.filtered().len(), 1);
    }

    // --- Pointer input, routed through the same `Action`/`dispatch_action`
    // seam the keyboard uses (see `dispatch_key` above and `ClipApp`'s
    // own `PopupApp` impl, which are thin wrappers over exactly this).
    // The hit-test that turns a pointer position into a row index is
    // `geometry::RowLayout::row_at`, pinned by its own tests in
    // `geometry.rs`; what belongs here is what an already-resolved row
    // or scroll *does*.

    #[test]
    fn hovering_a_row_selects_it_exactly_like_landing_on_it_with_the_keyboard() {
        let mut model = model_with(&["a", "b", "c"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        assert_eq!(dispatch_action(&mut model, &chooser, &pinner, Action::Select(2)), None);
        assert_eq!(model.selected_index(), 2);
        assert!(chooser.calls.borrow().is_empty(), "hovering must never choose anything");
    }

    /// Mirrors `ClipApp::pointer_click`'s `Hit::Pin` branch: select, then
    /// `TogglePin` rather than `Choose` — the same one-action, two-ways-
    /// to-trigger-it property `hovering_a_row_selects_it_exactly_like_landing_on_it_with_the_keyboard`
    /// pins for selection, applied to the pin toggle instead of choosing.
    #[test]
    fn clicking_the_pin_toggle_pins_the_row_instead_of_choosing_it() {
        let mut model = model_with(&["a", "b"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        dispatch_action(&mut model, &chooser, &pinner, Action::Select(1));
        let outcome = dispatch_action(&mut model, &chooser, &pinner, Action::TogglePin);
        assert_eq!(outcome, None, "pinning must never end the popup the way choosing does");
        assert!(chooser.calls.borrow().is_empty(), "a pin click must never choose the row");
        assert_eq!(
            pinner.calls.borrow().as_slice(),
            &[(EntryId::of(&Content::Text("b".into())).as_str().to_string(), true)]
        );
    }

    #[test]
    fn a_click_chooses_whatever_row_the_hit_test_resolved_exactly_as_enter_does() {
        let mut model = model_with(&["a", "b", "c"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        // The pointer's own path always selects the hit-tested row first
        // (see `ClipApp::pointer_click`), then chooses — mirrored here
        // as two actions so this test does not need a live surface.
        dispatch_action(&mut model, &chooser, &pinner, Action::Select(1));
        let outcome = dispatch_action(&mut model, &chooser, &pinner, Action::Choose);
        assert_eq!(outcome, Some(ChoiceOutcome::Chosen));
        assert_eq!(chooser.calls.borrow().as_slice(), &[EntryId::of(&Content::Text("b".into()))]);
    }

    #[test]
    fn scrolling_moves_the_selection_through_the_same_action_up_and_down_use() {
        let mut model = model_with(&["a", "b", "c"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        dispatch_action(&mut model, &chooser, &pinner, Action::Move(1));
        assert_eq!(model.selected_index(), 1);
        dispatch_action(&mut model, &chooser, &pinner, Action::Move(-1));
        assert_eq!(model.selected_index(), 0);
    }

    // --- Pinning, via the F2 key and `Action::TogglePin`. `Pinner` is the
    // same kind of seam `Chooser` is, so these are tested the same way:
    // no socket, no daemon, just `MockPinner` recording what it was
    // asked and handing back whatever result the test configured.

    #[test]
    fn f2_pins_the_selected_unpinned_entry() {
        let mut model = model_with(&["a", "b"]);
        model.move_selection(1); // "b"
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        let outcome = dispatch_key(&mut model, &chooser, &pinner, Keysym::F2, None);
        assert_eq!(outcome, None, "pinning does not end the popup");
        assert_eq!(
            pinner.calls.borrow().as_slice(),
            &[(EntryId::of(&Content::Text("b".into())).as_str().to_string(), true)]
        );
        assert!(
            model.filtered().iter().find(|e| e.content == Content::Text("b".into())).unwrap().pinned,
            "the popup's own copy must reflect the pin once the daemon confirmed it"
        );
    }

    #[test]
    fn f2_unpins_an_already_pinned_entry() {
        let mut model = model_with(&["a"]);
        let id = model.filtered()[0].id.clone();
        model.set_entry_pinned(&id, true);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        dispatch_key(&mut model, &chooser, &pinner, Keysym::F2, None);
        assert_eq!(pinner.calls.borrow().as_slice(), &[(id.as_str().to_string(), false)]);
        assert!(!model.filtered()[0].pinned);
    }

    #[test]
    fn f2_with_nothing_selected_does_not_call_the_pinner_at_all() {
        let mut model = Model::new(HistoryState::Loaded(Vec::new()));
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::succeeding();
        assert_eq!(dispatch_key(&mut model, &chooser, &pinner, Keysym::F2, None), None);
        assert!(pinner.calls.borrow().is_empty());
    }

    /// The failure case that matters most: no daemon to ask. The popup
    /// must not crash, must not end, and — the property this test
    /// exists to pin — must not apply the pin locally, which would make
    /// the row look pinned when nothing on disk or in the daemon agrees.
    #[test]
    fn a_failed_pin_leaves_the_entry_unpinned_and_sets_a_notice() {
        let mut model = model_with(&["a"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::no_daemon();
        let outcome = dispatch_key(&mut model, &chooser, &pinner, Keysym::F2, None);
        assert_eq!(outcome, None);
        assert!(
            !model.filtered()[0].pinned,
            "a failed pin must not look like it worked"
        );
        assert_eq!(
            model.pin_notice(),
            Some("hyprforge-clipd isn't running, so pinning isn't available right now")
        );
    }

    /// Any other action clears a stale notice from an earlier failed
    /// pin — see `Model::pin_notice`'s doc for why it must not linger.
    #[test]
    fn a_pin_notice_does_not_survive_the_next_unrelated_action() {
        let mut model = model_with(&["a", "b"]);
        let chooser = MockChooser::succeeding();
        let failing_pinner = MockPinner::no_daemon();
        dispatch_key(&mut model, &chooser, &failing_pinner, Keysym::F2, None);
        assert!(model.pin_notice().is_some());

        let succeeding_pinner = MockPinner::succeeding();
        dispatch_key(&mut model, &chooser, &succeeding_pinner, Keysym::Down, None);
        assert_eq!(model.pin_notice(), None, "an unrelated action must clear the notice");
    }

    /// The daemon's own refusal (an id it does not recognise, or
    /// `can_save == false`) is shown verbatim, distinct from the generic
    /// "isn't running" wording `MockPinner::no_daemon` produces —
    /// proving the two are not collapsed into the same message at this
    /// layer either.
    #[test]
    fn a_refusal_from_the_daemon_shows_its_own_message_not_a_generic_one() {
        let mut model = model_with(&["a"]);
        let chooser = MockChooser::succeeding();
        let pinner = MockPinner::failing("no clipboard entry with id abc123");
        dispatch_key(&mut model, &chooser, &pinner, Keysym::F2, None);
        assert_eq!(model.pin_notice(), Some("no clipboard entry with id abc123"));
    }

    /// A successful pin clears any notice left over from a previous
    /// failed attempt at the same entry.
    #[test]
    fn a_successful_pin_clears_a_previous_notice() {
        let mut model = model_with(&["a"]);
        let chooser = MockChooser::succeeding();
        let failing_pinner = MockPinner::no_daemon();
        dispatch_key(&mut model, &chooser, &failing_pinner, Keysym::F2, None);
        assert!(model.pin_notice().is_some());

        let succeeding_pinner = MockPinner::succeeding();
        dispatch_key(&mut model, &chooser, &succeeding_pinner, Keysym::F2, None);
        assert_eq!(model.pin_notice(), None);
        assert!(model.filtered()[0].pinned);
    }
}
