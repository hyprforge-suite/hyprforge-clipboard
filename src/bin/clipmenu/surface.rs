//! What a keypress or a click *means* for this clipboard popup, and the
//! [`ClipApp`] that plugs that into `hyprforge-popup`'s generic
//! layer-shell machinery.
//!
//! The Wayland/iced plumbing lives in `hyprforge-popup::popup`; see that
//! module's own doc, and the [`hyprforge_popup::PopupApp`] seam
//! [`ClipApp`] implements. What is here is everything specific to a
//! clipboard history: which [`Action`] a key or a click means, and what
//! happens when one is dispatched against this popup's own [`Model`],
//! [`Chooser`] and [`Editor`].

use crate::chooser::Chooser;
use crate::editor::Editor;
use crate::geometry::{Hit, Layout};
use crate::kind::Filter;
use crate::model::{Line, Model};
use crate::thumbnail;
use crate::view;
use hyprforge_look::Theme;
use hyprforge_popup::{Keysym, Modifiers};
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
pub use hyprforge_clipboard::Shortcut;

/// The clipboard popup's own [`hyprforge_popup::PopupApp`]: a live
/// [`Model`], a lazily-decoded thumbnail cache, and the two seams
/// (`Chooser`, `Editor`) that reach the clipd daemon and the compositor.
pub struct ClipApp<C: Chooser> {
    model: Model,
    thumbnails: thumbnail::Cache,
    chooser: C,
    /// Boxed rather than a second generic parameter: one production
    /// `Editor` and one mock, called a handful of times per invocation —
    /// the dynamic dispatch costs nothing worth a second type parameter.
    editor: Box<dyn Editor>,
    shortcut: Shortcut,
    /// The pointer's y as of the last drag event — `None` whenever no
    /// scrollbar-thumb drag is in progress. What turns a drag's absolute
    /// position into the delta `Scrollbar::drag_delta_to_offset_delta`
    /// wants.
    drag_last_y: Option<f64>,
    /// `$HOME`, read once, for showing a file path with `~`.
    home: Option<String>,
}

impl<C: Chooser> ClipApp<C> {
    pub fn new(model: Model, chooser: C, editor: impl Editor + 'static, shortcut: Shortcut) -> ClipApp<C> {
        ClipApp {
            model,
            thumbnails: thumbnail::Cache::new(),
            chooser,
            editor: Box::new(editor),
            shortcut,
            drag_last_y: None,
            home: std::env::var("HOME").ok(),
        }
    }

    /// What `position` is over, measured against the same stack and
    /// offset `view.rs` draws the list from.
    fn hit(&self, theme: &Theme, position: (f64, f64)) -> Option<Hit> {
        Layout::for_font_size(theme.font_size).hit(position, &self.model.stack(), self.model.scroll_offset())
    }

    /// A list line resolved to the entry on it — `None` for a section
    /// label, which is drawn but is not something to choose.
    fn entry_on(&self, line: usize) -> Option<usize> {
        match self.model.lines().get(line) {
            Some(Line::Entry(index)) => Some(*index),
            _ => None,
        }
    }
}

impl<C: Chooser + 'static> hyprforge_popup::PopupApp for ClipApp<C> {
    type Outcome = ChoiceOutcome;

    fn view<'a>(&'a mut self, theme: &'a Theme, now: u64, _width: f64) -> Element<'a, Infallible, iced_widget::Theme, iced_tiny_skia::Renderer> {
        view::view(&self.model, theme, &mut self.thumbnails, now, self.home.as_deref())
    }

    fn rows_that_fit(&self, theme: &Theme, _height: f64) -> usize {
        let layout = Layout::for_font_size(theme.font_size);
        (layout.viewport_height() / layout.row_height).floor() as usize
    }

    /// A hover over a row selects it; over any other control it changes
    /// nothing but still reports `true`, so the pointer becomes a hand
    /// over everything that can be clicked.
    fn pointer_move(&mut self, theme: &Theme, position: (f64, f64)) -> bool {
        match self.hit(theme, position) {
            Some(Hit::Line(line)) => match self.entry_on(line) {
                Some(index) => {
                    self.model.select(index);
                    true
                }
                None => false,
            },
            Some(_) => true,
            None => false,
        }
    }

    /// Re-hit-tests at the click position rather than trusting the last
    /// hover: a click landing between a hover and a redraw must still be
    /// honest about what it is actually over.
    fn pointer_click(&mut self, theme: &Theme, _width: f64, position: (f64, f64)) -> Option<ChoiceOutcome> {
        let action = match self.hit(theme, position)? {
            Hit::Tab(index) => Action::SetFilter(Filter::ALL[index.min(Filter::ALL.len() - 1)]),
            Hit::Line(line) => {
                let index = self.entry_on(line)?;
                dispatch_action(&mut self.model, &self.chooser, self.editor.as_ref(), Action::Select(index));
                Action::Choose
            }
            Hit::Paste => Action::Choose,
            Hit::Pin => Action::TogglePin,
            Hit::Delete => Action::Delete,
            Hit::Clear => Action::Clear,
        };
        dispatch_action(&mut self.model, &self.chooser, self.editor.as_ref(), action)
    }

    /// Scrolls the view, not the selection — one row per wheel notch.
    fn pointer_scroll(&mut self, rows: i32) {
        let stride = self.model.row_stride();
        self.model.scroll_by(rows as f64 * stride);
    }

    fn pointer_drag_start(&mut self, theme: &Theme, position: (f64, f64)) -> bool {
        let bar = Layout::for_font_size(theme.font_size).scrollbar();
        if bar.hit_thumb(position, self.model.stack().content_height(), self.model.scroll_offset()) {
            self.drag_last_y = Some(position.1);
            true
        } else {
            false
        }
    }

    fn pointer_drag_move(&mut self, theme: &Theme, position: (f64, f64)) {
        let Some(last_y) = self.drag_last_y else { return };
        self.drag_last_y = Some(position.1);
        let bar = Layout::for_font_size(theme.font_size).scrollbar();
        let delta = bar.drag_delta_to_offset_delta(position.1 - last_y, self.model.stack().content_height());
        self.model.scroll_by(delta);
    }

    fn pointer_drag_end(&mut self) {
        self.drag_last_y = None;
    }

    fn key(&mut self, keysym: Keysym, utf8: Option<String>, modifiers: Modifiers) -> Option<ChoiceOutcome> {
        dispatch_key(&mut self.model, &self.chooser, self.editor.as_ref(), keysym, utf8, modifiers)
    }

    fn needs_finish(outcome: ChoiceOutcome) -> bool {
        outcome == ChoiceOutcome::Chosen
    }

    /// Runs only after `hyprforge-popup` has torn this popup's own
    /// surface down and proven the compositor processed that — see
    /// `hyprforge_popup::PopupApp::finish`'s own doc for why the ordering
    /// matters. `Action::Choose` only put the entry on the clipboard;
    /// synthesizing the paste is this method's job alone.
    fn finish(&mut self, outcome: ChoiceOutcome) {
        if outcome == ChoiceOutcome::Chosen {
            self.chooser.finish_paste(self.shortcut);
        }
    }
}

/// What the user asked the popup to do, independent of whether a key or
/// a pointer produced it — the seam that keeps mouse and keyboard from
/// growing two sets of rules for the same outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    /// Move the selection by this many entries — Up/Down.
    Move(i32),
    /// Select exactly this entry of the filtered list — a hover.
    Select(usize),
    /// Choose whatever is selected — Enter, a click on a row, or Paste.
    Choose,
    /// Close without choosing — Escape.
    Cancel,
    Backspace,
    Type(char),
    /// Pin the selected entry, or unpin it — Ctrl+P, F2, or the Pin
    /// button.
    TogglePin,
    /// Forget the selected entry — Delete, or the Delete button.
    Delete,
    /// Show this tab — a click on it.
    SetFilter(Filter),
    /// The next (or previous) tab — Tab and Shift+Tab.
    StepFilter(bool),
    /// "Clear history…": the first press arms it, the second clears.
    Clear,
    /// Put an armed "Clear history…" back down — Escape while armed.
    Disarm,
}

/// The one place any [`Action`] takes effect. Split out so every rule
/// about what a keystroke or a click *means* can be tested without a
/// Wayland connection.
fn dispatch_action<C: Chooser>(model: &mut Model, chooser: &C, editor: &dyn Editor, action: Action) -> Option<ChoiceOutcome> {
    // Any action supersedes a stale notice — see `Model::notice` for why
    // it is cleared this aggressively — and puts an armed clear back
    // down: a destructive second click has to *follow* the first, not
    // arrive after the user went off and did something else.
    model.set_notice(None);
    if action != Action::Clear {
        model.set_clear_armed(false);
    }
    match action {
        Action::Cancel => Some(ChoiceOutcome::Cancelled),
        Action::Disarm => None,
        Action::Choose => {
            // Only puts the entry on the clipboard. The paste is
            // `ClipApp::finish`'s job, once this popup's surface is gone.
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
        Action::SetFilter(filter) => {
            model.set_filter(filter);
            None
        }
        Action::StepFilter(forward) => {
            model.set_filter(model.filter().next(forward));
            None
        }
        Action::TogglePin => {
            let entry = model.selected_entry()?;
            let pinned = !entry.pinned;
            match editor.set_pinned(entry.id.as_str(), pinned) {
                // The daemon is the only writer of the history file; this
                // only updates the popup's own copy once it has agreed.
                Ok(()) => model.set_entry_pinned(&entry.id, pinned),
                // Never applied locally on failure: a row that looks
                // pinned when nothing on disk agrees is the "must not
                // look like it worked" failure this popup has to avoid.
                Err(message) => model.set_notice(Some(message)),
            }
            None
        }
        Action::Delete => {
            let entry = model.selected_entry()?;
            match editor.remove(entry.id.as_str()) {
                Ok(()) => model.remove_entry(&entry.id),
                Err(message) => model.set_notice(Some(message)),
            }
            None
        }
        Action::Clear => {
            let ids = model.clearable();
            if ids.is_empty() {
                model.set_clear_armed(false);
                return None;
            }
            if !model.clear_armed() {
                model.set_clear_armed(true);
                return None;
            }
            model.set_clear_armed(false);
            // One at a time, stopping at the first refusal: what was
            // removed is removed here too, and what was not stays and
            // says why — never a list that looks cleared and is not.
            for id in ids {
                match editor.remove(id.as_str()) {
                    Ok(()) => model.remove_entry(&id),
                    Err(message) => {
                        model.set_notice(Some(message));
                        break;
                    }
                }
            }
            None
        }
    }
}

/// The keystroke rules: which [`Action`] each key produces.
///
/// Nothing typed here is ever logged, matched on for its value, or
/// otherwise inspected beyond being appended to the filter — a clipboard
/// search is not a password, but the rule is followed regardless.
///
/// Chords are recognised before typing: `Ctrl+P` arrives with `utf8` set
/// to the control character `U+0010`, and a held Ctrl never types into
/// the search at all.
fn dispatch_key<C: Chooser>(
    model: &mut Model,
    chooser: &C,
    editor: &dyn Editor,
    keysym: Keysym,
    utf8: Option<String>,
    modifiers: Modifiers,
) -> Option<ChoiceOutcome> {
    let action = match keysym {
        Keysym::Escape if model.clear_armed() => Action::Disarm,
        Keysym::Escape => Action::Cancel,
        Keysym::Return | Keysym::KP_Enter => Action::Choose,
        Keysym::Up => Action::Move(-1),
        Keysym::Down => Action::Move(1),
        Keysym::BackSpace => Action::Backspace,
        Keysym::Delete | Keysym::KP_Delete => Action::Delete,
        // F2 stays alongside Ctrl+P: it was this popup's pin key before
        // modifiers reached it, and muscle memory is not a bug.
        Keysym::F2 => Action::TogglePin,
        Keysym::p | Keysym::P if modifiers.ctrl => Action::TogglePin,
        Keysym::Tab => Action::StepFilter(!modifiers.shift),
        Keysym::ISO_Left_Tab => Action::StepFilter(false),
        _ if modifiers.ctrl || modifiers.alt || modifiers.logo => return None,
        _ => {
            if let Some(text) = utf8 {
                for c in text.chars().filter(|c| !c.is_control()) {
                    dispatch_action(model, chooser, editor, Action::Type(c));
                }
            }
            return None;
        }
    };
    dispatch_action(model, chooser, editor, action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chooser::mock::MockChooser;
    use crate::editor::mock::MockEditor;
    use crate::model::HistoryState;
    use hyprforge_clipboard::{Content, Entry, EntryId};

    fn entry(text: &str) -> Entry {
        let content = Content::Text(text.to_string());
        Entry { id: EntryId::of(&content), content, copied_at: 0, pinned: false }
    }

    fn model_with(texts: &[&str]) -> Model {
        Model::new(HistoryState::Loaded(texts.iter().map(|s| entry(s)).collect()))
    }

    fn id(text: &str) -> EntryId {
        EntryId::of(&Content::Text(text.into()))
    }

    const NONE: Modifiers = Modifiers { ctrl: false, alt: false, shift: false, caps_lock: false, logo: false, num_lock: false };
    const CTRL: Modifiers = Modifiers { ctrl: true, ..NONE };
    const SHIFT: Modifiers = Modifiers { shift: true, ..NONE };

    fn key(model: &mut Model, chooser: &MockChooser, editor: &MockEditor, keysym: Keysym) -> Option<ChoiceOutcome> {
        dispatch_key(model, chooser, editor, keysym, None, NONE)
    }

    #[test]
    fn escape_cancels_without_choosing_anything() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::succeeding());
        assert_eq!(key(&mut model, &chooser, &editor, Keysym::Escape), Some(ChoiceOutcome::Cancelled));
        assert!(chooser.calls.borrow().is_empty());
    }

    #[test]
    fn enter_chooses_the_selected_entry_and_ends_the_popup() {
        let (mut model, chooser, editor) = (model_with(&["a", "b"]), MockChooser::succeeding(), MockEditor::succeeding());
        model.move_selection(1);
        assert_eq!(key(&mut model, &chooser, &editor, Keysym::Return), Some(ChoiceOutcome::Chosen));
        assert_eq!(chooser.calls.borrow().as_slice(), &[id("b")]);
    }

    /// `Action::Choose` must only put the entry on the clipboard. If this
    /// ever sees `"finish_paste"`, the paste is being synthesized while
    /// this popup still holds exclusive keyboard focus — delivering the
    /// Ctrl+V back to the popup instead of the window it was meant for.
    #[test]
    fn choosing_sets_the_clipboard_without_synthesizing_the_paste_yet() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::succeeding());
        key(&mut model, &chooser, &editor, Keysym::Return);
        assert_eq!(chooser.log.borrow().as_slice(), &["set_clipboard"]);
    }

    #[test]
    fn enter_with_an_empty_list_does_nothing() {
        let (mut model, chooser, editor) = (Model::new(HistoryState::Loaded(Vec::new())), MockChooser::succeeding(), MockEditor::succeeding());
        assert_eq!(key(&mut model, &chooser, &editor, Keysym::Return), None);
    }

    #[test]
    fn a_failing_choose_still_ends_the_popup_rather_than_hanging_open() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::failing("no seat"), MockEditor::succeeding());
        assert_eq!(key(&mut model, &chooser, &editor, Keysym::Return), Some(ChoiceOutcome::Cancelled));
    }

    #[test]
    fn typing_reaches_the_filter() {
        let (mut model, chooser, editor) = (model_with(&["alpha", "beta"]), MockChooser::succeeding(), MockEditor::succeeding());
        dispatch_key(&mut model, &chooser, &editor, Keysym::a, Some("a".into()), NONE);
        dispatch_key(&mut model, &chooser, &editor, Keysym::l, Some("l".into()), NONE);
        assert_eq!(model.filter_text(), "al");
    }

    /// A held Ctrl is a chord, never typing — `Ctrl+A` must not put an
    /// `a` in the search.
    #[test]
    fn a_chord_never_types_into_the_search() {
        let (mut model, chooser, editor) = (model_with(&["alpha"]), MockChooser::succeeding(), MockEditor::succeeding());
        dispatch_key(&mut model, &chooser, &editor, Keysym::a, Some("a".into()), CTRL);
        assert_eq!(model.filter_text(), "");
    }

    #[test]
    fn tab_steps_through_the_filters_and_shift_tab_steps_back() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::succeeding());
        key(&mut model, &chooser, &editor, Keysym::Tab);
        assert_eq!(model.filter(), Filter::Text);
        dispatch_key(&mut model, &chooser, &editor, Keysym::ISO_Left_Tab, None, SHIFT);
        assert_eq!(model.filter(), Filter::All);
    }

    // --- pinning

    #[test]
    fn ctrl_p_and_f2_both_pin_the_selected_entry() {
        for (keysym, modifiers) in [(Keysym::p, CTRL), (Keysym::F2, NONE)] {
            let (mut model, chooser, editor) = (model_with(&["a", "b"]), MockChooser::succeeding(), MockEditor::succeeding());
            model.move_selection(1);
            assert_eq!(dispatch_key(&mut model, &chooser, &editor, keysym, None, modifiers), None, "pinning does not end the popup");
            assert_eq!(editor.calls.borrow().as_slice(), &[(id("b").as_str().to_string(), true)]);
            assert!(model.filtered().iter().find(|e| e.id == id("b")).unwrap().pinned);
        }
    }

    #[test]
    fn a_plain_p_is_typed_not_a_pin() {
        let (mut model, chooser, editor) = (model_with(&["pear"]), MockChooser::succeeding(), MockEditor::succeeding());
        dispatch_key(&mut model, &chooser, &editor, Keysym::p, Some("p".into()), NONE);
        assert!(editor.calls.borrow().is_empty());
        assert_eq!(model.filter_text(), "p");
    }

    /// No daemon to ask: the popup must not crash, must not end, and must
    /// not apply the pin locally, which would make the row look pinned
    /// when nothing on disk agrees.
    #[test]
    fn a_failed_pin_leaves_the_entry_unpinned_and_says_why() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::no_daemon());
        assert_eq!(key(&mut model, &chooser, &editor, Keysym::F2), None);
        assert!(!model.filtered()[0].pinned);
        assert_eq!(model.notice(), Some("hyprforge-clipd isn't running, so pinning isn't available right now"));
    }

    #[test]
    fn a_notice_does_not_survive_the_next_unrelated_action() {
        let (mut model, chooser) = (model_with(&["a", "b"]), MockChooser::succeeding());
        key(&mut model, &chooser, &MockEditor::no_daemon(), Keysym::F2);
        assert!(model.notice().is_some());
        key(&mut model, &chooser, &MockEditor::succeeding(), Keysym::Down);
        assert_eq!(model.notice(), None);
    }

    #[test]
    fn a_refusal_from_the_daemon_shows_its_own_message() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::failing("no clipboard entry with id abc123"));
        key(&mut model, &chooser, &editor, Keysym::F2);
        assert_eq!(model.notice(), Some("no clipboard entry with id abc123"));
    }

    // --- deleting

    #[test]
    fn delete_asks_the_daemon_and_drops_the_entry_once_it_agrees() {
        let (mut model, chooser, editor) = (model_with(&["a", "b"]), MockChooser::succeeding(), MockEditor::succeeding());
        key(&mut model, &chooser, &editor, Keysym::Delete);
        assert_eq!(editor.removed.borrow().as_slice(), &[id("a").as_str().to_string()]);
        assert_eq!(model.filtered().len(), 1);
    }

    #[test]
    fn a_refused_delete_keeps_the_entry_and_says_why() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::failing("history can't be saved"));
        key(&mut model, &chooser, &editor, Keysym::Delete);
        assert_eq!(model.filtered().len(), 1, "a delete the daemon refused must not look like it happened");
        assert_eq!(model.notice(), Some("history can't be saved"));
    }

    // --- clearing

    fn clear(model: &mut Model, chooser: &MockChooser, editor: &MockEditor) {
        dispatch_action(model, chooser, editor, Action::Clear);
    }

    #[test]
    fn the_first_clear_only_arms_it_and_the_second_clears_everything_unpinned() {
        let (mut model, chooser, editor) = (model_with(&["a", "b", "c"]), MockChooser::succeeding(), MockEditor::succeeding());
        let b = model.filtered()[1].id.clone();
        model.set_entry_pinned(&b, true);

        clear(&mut model, &chooser, &editor);
        assert!(model.clear_armed());
        assert!(editor.removed.borrow().is_empty(), "one click must never delete anything");

        clear(&mut model, &chooser, &editor);
        assert_eq!(editor.removed.borrow().len(), 2);
        assert_eq!(model.filtered().len(), 1, "only the pinned entry is left");
        assert!(model.filtered()[0].pinned);
        assert!(!model.clear_armed());
    }

    #[test]
    fn anything_between_the_two_clicks_disarms_the_clear() {
        let (mut model, chooser, editor) = (model_with(&["a", "b"]), MockChooser::succeeding(), MockEditor::succeeding());
        clear(&mut model, &chooser, &editor);
        key(&mut model, &chooser, &editor, Keysym::Down);
        assert!(!model.clear_armed());
        clear(&mut model, &chooser, &editor);
        assert!(editor.removed.borrow().is_empty(), "the click after a disarm arms again, it does not clear");
    }

    #[test]
    fn escape_puts_an_armed_clear_down_before_it_closes_anything() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::succeeding());
        clear(&mut model, &chooser, &editor);
        assert_eq!(key(&mut model, &chooser, &editor, Keysym::Escape), None);
        assert!(!model.clear_armed());
        assert_eq!(key(&mut model, &chooser, &editor, Keysym::Escape), Some(ChoiceOutcome::Cancelled));
    }

    #[test]
    fn a_clear_the_daemon_refuses_part_way_leaves_the_rest_and_says_why() {
        let (mut model, chooser, editor) = (model_with(&["a", "b"]), MockChooser::succeeding(), MockEditor::failing("history can't be saved"));
        clear(&mut model, &chooser, &editor);
        clear(&mut model, &chooser, &editor);
        assert_eq!(editor.removed.borrow().len(), 1, "it stops at the first refusal");
        assert_eq!(model.filtered().len(), 2);
        assert_eq!(model.notice(), Some("history can't be saved"));
    }

    #[test]
    fn with_nothing_unpinned_clear_does_not_even_arm() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::succeeding());
        let a = model.filtered()[0].id.clone();
        model.set_entry_pinned(&a, true);
        clear(&mut model, &chooser, &editor);
        assert!(!model.clear_armed());
    }

    // --- pointer actions through the same seam

    #[test]
    fn hovering_a_row_selects_it_and_never_chooses_it() {
        let (mut model, chooser, editor) = (model_with(&["a", "b", "c"]), MockChooser::succeeding(), MockEditor::succeeding());
        assert_eq!(dispatch_action(&mut model, &chooser, &editor, Action::Select(2)), None);
        assert_eq!(model.selected_index(), 2);
        assert!(chooser.calls.borrow().is_empty());
    }

    #[test]
    fn clicking_a_tab_shows_it() {
        let (mut model, chooser, editor) = (model_with(&["a"]), MockChooser::succeeding(), MockEditor::succeeding());
        dispatch_action(&mut model, &chooser, &editor, Action::SetFilter(Filter::Images));
        assert_eq!(model.filter(), Filter::Images);
    }
}
