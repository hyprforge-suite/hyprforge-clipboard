//! What is on screen, kept apart from how it is drawn or how it got
//! there.
//!
//! Everything here is plain data and pure functions over it — no
//! Wayland, no iced, no `hyprctl`. That is what makes filtering,
//! selection movement and "which lines are on screen right now" testable
//! directly, the same split `hyprforge-bluetooth`'s and
//! `hyprforge-network`'s D-Bus-backed modules use for their own models.
//!
//! # Lines, not rows
//!
//! The list is a stack of *lines*: a "Pinned" label, the pinned entries,
//! a "Recent" label, the rest. The selection is still an index into the
//! filtered entries — a label is never selected — and [`Model::lines`]
//! maps between the two. Where each line sits comes from one
//! [`hyprforge_popup::Stack`] ([`Model::stack`]), which `view.rs` draws
//! from and `geometry::Layout::hit` measures against.
//!
//! # Scrolling is state
//!
//! [`Model::scroll_offset`] is kept, not recomputed from the selection,
//! and only ever nudged far enough to keep the selection in view. A
//! hover selects the row under the pointer, and a view that recentred on
//! every selection would put a different row under a pointer that never
//! moved — indistinguishable from the list scrolling on its own.

use crate::kind::{Filter, Kind};
use hyprforge_clipboard::{Entry, EntryId, HistoryError};
use hyprforge_popup::Stack;

/// How the history read from disk turned out.
///
/// Kept distinct from an *empty* history — CLAUDE.md is explicit that
/// "this file could not be read" must never collapse into "there is
/// nothing configured". `hyprforge_clipboard::HistoryError::Unreadable`
/// means the index exists and will not parse; that is worth telling the
/// user about, on a screen that shows nothing else, rather than quietly
/// rendering the same blank list a first run would show.
pub enum HistoryState {
    /// Read cleanly. An empty `Vec` here is the history genuinely having
    /// nothing in it — a still-distinct case the view says out loud.
    Loaded(Vec<Entry>),
    /// The index exists and could not be parsed. Carries the message so
    /// the popup can show *why*, not just that something went wrong.
    Unreadable(String),
}

impl HistoryState {
    pub fn from_result(result: Result<hyprforge_clipboard::History, HistoryError>) -> HistoryState {
        match result {
            Ok(history) => HistoryState::Loaded(history.entries().to_vec()),
            Err(err) => HistoryState::Unreadable(err.to_string()),
        }
    }
}

/// The two groups the list is split into, pinned first — the order the
/// store already keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Pinned,
    Recent,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Pinned => "Pinned",
            Section::Recent => "Recent",
        }
    }
}

/// One line of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Header(Section),
    /// An entry, by index into [`Model::filtered`].
    Entry(usize),
}

/// Line heights and the viewport, from `geometry::Layout` — never
/// guessed here, the same "derive it, never duplicate it" discipline the
/// geometry module's own doc describes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListGeometry {
    pub viewport_height: f64,
    pub header_height: f64,
    pub row_height: f64,
    pub spacing: f64,
}

/// Plausible figures until `main.rs` sets the real ones — tall enough
/// that a filtering or selection test never has to think about
/// scrolling.
const DEFAULT_GEOMETRY: ListGeometry = ListGeometry { viewport_height: 1000.0, header_height: 26.0, row_height: 34.0, spacing: 0.0 };

/// The popup's state: what it knows, what the user has typed, which tab
/// and which row are selected.
pub struct Model {
    history: HistoryState,
    query: String,
    filter: Filter,
    /// Index into the *filtered* entries. Always within bounds of a
    /// non-empty filtered list; meaningless when it is empty.
    selected: usize,
    /// The message from the most recent pin or delete the daemon did not
    /// carry out — shown in place of the key hints until the next action
    /// of any kind. Never clipboard content: only an id (a hash) and the
    /// daemon's own reason, or a note that there was nobody to ask.
    ///
    /// Deliberately not sticky: CLAUDE.md is explicit that a failed
    /// connection must never be cached as a reason to stop trying, and a
    /// notice that lingered after the user had moved on would start to
    /// look exactly like that.
    notice: Option<String>,
    /// Whether "Clear history…" has been clicked once and is waiting for
    /// the second click that means it. Any other action disarms it.
    clear_armed: bool,
    geometry: ListGeometry,
    scroll_offset: f64,
    /// Where a chosen entry will be pasted — "Ghostty", say — shown on
    /// the Paste button. Cosmetic only: *how* to paste was decided once
    /// in `main.rs` from the same lookup, and nothing here reads this to
    /// decide it. `None` when nothing had focus that could be named.
    paste_target: Option<String>,
}

impl Model {
    pub fn new(history: HistoryState) -> Model {
        Model {
            history,
            query: String::new(),
            filter: Filter::All,
            selected: 0,
            notice: None,
            clear_armed: false,
            geometry: DEFAULT_GEOMETRY,
            scroll_offset: 0.0,
            paste_target: None,
        }
    }

    /// Sets the list's geometry from `geometry::Layout`. Re-syncs the
    /// scroll afterwards: a viewport that just shrank could otherwise
    /// leave the selection outside it.
    pub fn set_geometry(&mut self, geometry: ListGeometry) {
        self.geometry = ListGeometry {
            viewport_height: geometry.viewport_height.max(0.0),
            header_height: geometry.header_height.max(1.0),
            row_height: geometry.row_height.max(1.0),
            spacing: geometry.spacing.max(0.0),
        };
        self.sync_scroll();
    }

    /// One wheel notch: a row's own height.
    pub fn row_stride(&self) -> f64 {
        self.geometry.row_height + self.geometry.spacing
    }

    pub fn paste_target(&self) -> Option<&str> {
        self.paste_target.as_deref()
    }

    pub fn set_paste_target(&mut self, target: Option<String>) {
        self.paste_target = target;
    }

    pub fn history(&self) -> &HistoryState {
        &self.history
    }

    pub fn filter_text(&self) -> &str {
        &self.query
    }

    pub fn filter(&self) -> Filter {
        self.filter
    }

    /// The entries under the current tab that match the query, pinned
    /// first and then newest first exactly as the store ordered them —
    /// filtering only removes entries, it never reorders what is left.
    ///
    /// The query is a case-insensitive substring of the same text the row
    /// shows, so what the user sees is what they can search for.
    pub fn filtered(&self) -> Vec<&Entry> {
        let HistoryState::Loaded(entries) = &self.history else {
            return Vec::new();
        };
        let needle = self.query.to_lowercase();
        entries
            .iter()
            .filter(|e| self.filter.admits(Kind::of(&e.content)))
            .filter(|e| needle.is_empty() || e.content.preview(usize::MAX).to_lowercase().contains(&needle))
            .collect()
    }

    /// The list's lines: a label before each group that has anything in
    /// it, then that group's entries. "Recent" gets its label only when
    /// there is a pinned group to tell it apart from — a lone "Recent"
    /// over the only list there is labels nothing.
    pub fn lines(&self) -> Vec<Line> {
        let filtered = self.filtered();
        let pinned = filtered.iter().take_while(|e| e.pinned).count();
        let mut lines = Vec::with_capacity(filtered.len() + 2);
        if pinned > 0 {
            lines.push(Line::Header(Section::Pinned));
            lines.extend((0..pinned).map(Line::Entry));
            if pinned < filtered.len() {
                lines.push(Line::Header(Section::Recent));
            }
        }
        lines.extend((pinned..filtered.len()).map(Line::Entry));
        lines
    }

    /// Where every line sits — the one [`Stack`] both drawing and
    /// hit-testing read.
    pub fn stack(&self) -> Stack {
        let g = self.geometry;
        Stack::new(
            self.lines().iter().map(|line| match line {
                Line::Header(_) => g.header_height,
                Line::Entry(_) => g.row_height,
            }),
            g.spacing,
        )
    }

    pub fn scroll_offset(&self) -> f64 {
        self.scroll_offset
    }

    /// Scrolls the view by `delta` pixels (negative is up) — the wheel
    /// and a scrollbar drag. Moves the view, never the selection.
    pub fn scroll_by(&mut self, delta: f64) {
        let content = self.stack().content_height();
        self.scroll_offset = hyprforge_popup::clamp_offset(self.scroll_offset + delta, content, self.geometry.viewport_height);
    }

    /// Nudges the view the minimum needed to show the selected row — and
    /// the label above it, when it is the first row of its group, so
    /// moving up to the top of "Recent" shows the word "Recent" too.
    fn sync_scroll(&mut self) {
        let lines = self.lines();
        let Some(line) = lines.iter().position(|l| *l == Line::Entry(self.selected)) else {
            self.scroll_offset = 0.0;
            return;
        };
        let first = if line > 0 && matches!(lines[line - 1], Line::Header(_)) { line - 1 } else { line };
        self.scroll_offset = self.stack().reveal(first, line, self.scroll_offset, self.geometry.viewport_height);
    }

    /// Anything that changes *which* entries are listed: back to the top
    /// of the new list, since an index into the old one would select the
    /// wrong entry or nothing.
    fn refilter(&mut self) {
        self.selected = 0;
        self.scroll_offset = 0.0;
        self.sync_scroll();
    }

    pub fn type_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    /// Removes the last *character*, never a byte.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn set_filter(&mut self, filter: Filter) {
        if self.filter != filter {
            self.filter = filter;
            self.refilter();
        }
    }

    /// Moves the selection by `delta` entries, clamped at the ends rather
    /// than wrapping: Up at the top does nothing, it does not jump to the
    /// bottom. Labels are skipped, because they are not entries.
    pub fn move_selection(&mut self, delta: i32) {
        let len = self.filtered().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let current = self.selected.min(len - 1) as i32;
        self.selected = (current + delta).clamp(0, len as i32 - 1) as usize;
        self.sync_scroll();
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Selects exactly this entry of the filtered list — a hover or a
    /// click. Out of range is clamped rather than ignored: a hit racing a
    /// list that just got shorter should still select something sane.
    pub fn select(&mut self, index: usize) {
        let len = self.filtered().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = index.min(len - 1);
        self.sync_scroll();
    }

    pub fn selected_entry(&self) -> Option<Entry> {
        self.filtered().get(self.selected).map(|e| (*e).clone())
    }

    /// Reflects a pin or unpin the daemon has already confirmed, in this
    /// popup's own copy — this never writes anything and never talks to
    /// the daemon itself. A no-op for an id that is not here.
    ///
    /// Re-sorts with exactly the rule
    /// `hyprforge_clipboard::store::History::sort` uses, so the row lands
    /// where a fresh load would put it, then keeps the *same entry*
    /// selected: pinning is supposed to move it, not lose the selection.
    pub fn set_entry_pinned(&mut self, id: &EntryId, pinned: bool) {
        let HistoryState::Loaded(entries) = &mut self.history else { return };
        let Some(entry) = entries.iter_mut().find(|e| &e.id == id) else { return };
        entry.pinned = pinned;
        sort_entries(entries);
        if let Some(new_index) = self.filtered().iter().position(|e| &e.id == id) {
            self.selected = new_index;
        }
        self.sync_scroll();
    }

    /// Drops an entry the daemon has already confirmed it forgot. The
    /// selection stays at the same position, which is now the entry that
    /// was below — so pressing Delete repeatedly walks down the list the
    /// way it does in every file manager — clamped at a new end.
    pub fn remove_entry(&mut self, id: &EntryId) {
        let HistoryState::Loaded(entries) = &mut self.history else { return };
        entries.retain(|e| &e.id != id);
        let len = self.filtered().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        self.scroll_by(0.0);
        self.sync_scroll();
    }

    /// The ids "Clear history…" would remove: everything not pinned —
    /// under every tab, not only the one showing, because the button
    /// says "history". Pinned entries are what a person asked to keep.
    pub fn clearable(&self) -> Vec<EntryId> {
        match &self.history {
            HistoryState::Loaded(entries) => entries.iter().filter(|e| !e.pinned).map(|e| e.id.clone()).collect(),
            HistoryState::Unreadable(_) => Vec::new(),
        }
    }

    pub fn clear_armed(&self) -> bool {
        self.clear_armed
    }

    pub fn set_clear_armed(&mut self, armed: bool) {
        self.clear_armed = armed;
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }
}

/// The ordering `History::sort` applies on the daemon side. Duplicated
/// rather than shared because `History::sort` is private to a type this
/// crate only ever reads a `Vec<Entry>` out of — sharing it would make it
/// public API of `hyprforge-clipboard` for exactly one caller.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.copied_at.cmp(&a.copied_at)).then(a.id.cmp(&b.id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_clipboard::Content;

    fn text_entry(s: &str) -> Entry {
        let content = Content::Text(s.to_string());
        Entry { id: EntryId::of(&content), content, copied_at: 0, pinned: false }
    }

    fn model_with(texts: &[&str]) -> Model {
        Model::new(HistoryState::Loaded(texts.iter().map(|s| text_entry(s)).collect()))
    }

    const G: ListGeometry = ListGeometry { viewport_height: 100.0, header_height: 26.0, row_height: 34.0, spacing: 0.0 };

    fn model_with_viewport(texts: &[&str], viewport_height: f64) -> Model {
        let mut model = model_with(texts);
        model.set_geometry(ListGeometry { viewport_height, ..G });
        model
    }

    // --- filtering

    #[test]
    fn an_empty_query_under_all_shows_everything() {
        assert_eq!(model_with(&["alpha", "beta", "gamma"]).filtered().len(), 3);
    }

    #[test]
    fn typing_filters_case_insensitively_to_what_was_typed() {
        let mut model = model_with(&["Alpha", "beta", "gamma"]);
        model.type_char('A');
        model.type_char('L');
        let filtered = model.filtered();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].content, Content::Text("Alpha".into()));
    }

    #[test]
    fn backspace_widens_the_filter_back_out() {
        let mut model = model_with(&["alpha", "beta"]);
        model.type_char('z');
        assert!(model.filtered().is_empty());
        model.backspace();
        assert_eq!(model.filtered().len(), 2);
    }

    #[test]
    fn a_tab_shows_only_its_own_kind() {
        let mut model = model_with(&["https://example.org", "plain words", "/etc/hosts", "#ff0000"]);
        model.set_filter(Filter::Links);
        assert_eq!(model.filtered().len(), 1);
        model.set_filter(Filter::Files);
        assert_eq!(model.filtered()[0].content, Content::Text("/etc/hosts".into()));
        model.set_filter(Filter::Text);
        assert_eq!(model.filtered().len(), 2, "plain text and the colour");
    }

    #[test]
    fn changing_tab_goes_back_to_the_top_of_the_new_list() {
        let mut model = model_with(&["a", "b", "c", "https://x.org"]);
        model.select(2);
        model.set_filter(Filter::Text);
        assert_eq!(model.selected_index(), 0);
    }

    // --- sections

    #[test]
    fn with_nothing_pinned_there_are_no_labels_at_all() {
        let model = model_with(&["a", "b"]);
        assert_eq!(model.lines(), vec![Line::Entry(0), Line::Entry(1)]);
    }

    #[test]
    fn pinned_entries_get_their_own_label_and_the_rest_are_recent() {
        let mut model = model_with(&["a", "b", "c"]);
        let c = model.filtered()[2].id.clone();
        model.set_entry_pinned(&c, true);
        assert_eq!(
            model.lines(),
            vec![Line::Header(Section::Pinned), Line::Entry(0), Line::Header(Section::Recent), Line::Entry(1), Line::Entry(2)]
        );
    }

    #[test]
    fn when_everything_is_pinned_there_is_no_empty_recent_label() {
        let mut model = model_with(&["a"]);
        let a = model.filtered()[0].id.clone();
        model.set_entry_pinned(&a, true);
        assert_eq!(model.lines(), vec![Line::Header(Section::Pinned), Line::Entry(0)]);
    }

    /// The stack must describe exactly the lines: a label at the label's
    /// height, a row at a row's.
    #[test]
    fn the_stack_has_one_line_per_line_at_its_own_height() {
        let mut model = model_with(&["a", "b"]);
        model.set_geometry(G);
        let a = model.filtered()[1].id.clone();
        model.set_entry_pinned(&a, true);
        let stack = model.stack();
        assert_eq!(stack.len(), model.lines().len());
        assert_eq!(stack.height(0), G.header_height);
        assert_eq!(stack.height(1), G.row_height);
    }

    // --- selection

    #[test]
    fn the_selection_stops_at_both_ends_rather_than_wrapping() {
        let mut model = model_with(&["a", "b"]);
        model.move_selection(5);
        assert_eq!(model.selected_index(), 1);
        model.move_selection(-5);
        assert_eq!(model.selected_index(), 0);
    }

    #[test]
    fn selecting_past_the_end_clamps_to_the_last_entry() {
        let mut model = model_with(&["a", "b"]);
        model.select(50);
        assert_eq!(model.selected_index(), 1);
    }

    #[test]
    fn pinning_the_selected_entry_keeps_it_selected_after_it_moves() {
        let mut model = model_with(&["a", "b", "c"]);
        model.select(2);
        let c = model.filtered()[2].id.clone();
        model.set_entry_pinned(&c, true);
        assert_eq!(model.filtered()[model.selected_index()].id, c);
    }

    #[test]
    fn pinning_an_id_that_is_not_in_the_history_does_nothing() {
        let mut model = model_with(&["a"]);
        model.set_entry_pinned(&EntryId::from_raw("not-a-real-id"), true);
        assert!(model.filtered().iter().all(|e| !e.pinned));
    }

    // --- deleting

    #[test]
    fn deleting_the_selected_entry_selects_the_one_that_was_below_it() {
        let mut model = model_with(&["a", "b", "c"]);
        model.select(1);
        let b = model.filtered()[1].id.clone();
        model.remove_entry(&b);
        assert_eq!(model.filtered().len(), 2);
        assert_eq!(model.selected_entry().unwrap().content, Content::Text("c".into()));
    }

    #[test]
    fn deleting_the_last_entry_selects_the_new_last_one() {
        let mut model = model_with(&["a", "b"]);
        model.select(1);
        let b = model.filtered()[1].id.clone();
        model.remove_entry(&b);
        assert_eq!(model.selected_index(), 0);
    }

    #[test]
    fn clearing_would_remove_everything_except_what_is_pinned() {
        let mut model = model_with(&["a", "b", "c"]);
        let b = model.filtered()[1].id.clone();
        model.set_entry_pinned(&b, true);
        let clearable = model.clearable();
        assert_eq!(clearable.len(), 2);
        assert!(!clearable.contains(&b));
    }

    #[test]
    fn an_unreadable_history_offers_nothing_to_clear_and_is_not_empty() {
        let model = Model::new(HistoryState::Unreadable("bad toml".into()));
        assert!(model.clearable().is_empty());
        assert!(model.filtered().is_empty());
        assert!(matches!(model.history(), HistoryState::Unreadable(_)));
    }

    // --- scrolling

    #[test]
    fn hovering_a_row_that_is_already_visible_does_not_move_the_view() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 3.0 * 34.0);
        model.select(2);
        let before = model.scroll_offset();
        model.select(1);
        assert_eq!(model.scroll_offset(), before);
    }

    #[test]
    fn moving_past_the_bottom_scrolls_by_exactly_one_row() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 3.0 * 34.0);
        model.move_selection(2);
        assert_eq!(model.scroll_offset(), 0.0);
        model.move_selection(1);
        assert_eq!(model.scroll_offset(), 34.0);
    }

    /// Moving up onto the first pinned row must bring the "Pinned" label
    /// back into view with it, not leave the row flush against the top
    /// with its own heading scrolled away.
    #[test]
    fn reaching_the_first_row_of_a_group_shows_its_label_too() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e", "f"], 2.0 * 34.0);
        let a = model.filtered()[0].id.clone();
        model.set_entry_pinned(&a, true);
        model.select(5);
        assert!(model.scroll_offset() > 0.0);
        model.select(0);
        assert_eq!(model.scroll_offset(), 0.0, "the label above the first pinned row is at the very top");
    }

    #[test]
    fn scrolling_clamps_at_both_ends() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 100.0);
        model.scroll_by(-500.0);
        assert_eq!(model.scroll_offset(), 0.0);
        model.scroll_by(1_000_000.0);
        assert_eq!(model.scroll_offset(), model.stack().content_height() - 100.0);
    }

    #[test]
    fn a_short_list_that_already_fits_cannot_be_scrolled() {
        let mut model = model_with_viewport(&["a", "b"], 1000.0);
        model.scroll_by(500.0);
        assert_eq!(model.scroll_offset(), 0.0);
    }

    #[test]
    fn a_shorter_filtered_list_clamps_the_view_back_to_the_top() {
        let mut model = model_with_viewport(&["aa", "ab", "cc", "dd", "ee"], 2.0 * 34.0);
        model.select(4);
        assert!(model.scroll_offset() > 0.0);
        model.type_char('a');
        assert_eq!(model.scroll_offset(), 0.0);
    }

    // --- transient state

    #[test]
    fn a_notice_and_an_armed_clear_can_be_set_and_cleared() {
        let mut model = model_with(&["a"]);
        assert_eq!(model.notice(), None);
        model.set_notice(Some("hyprforge-clipd isn't running".to_string()));
        assert_eq!(model.notice(), Some("hyprforge-clipd isn't running"));
        model.set_clear_armed(true);
        assert!(model.clear_armed());
    }
}
