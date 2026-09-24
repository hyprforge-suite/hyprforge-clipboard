//! What is on screen, kept apart from how it is drawn or how it got
//! there.
//!
//! Everything here is plain data and pure functions over it — no
//! Wayland, no iced, no `hyprctl`. That is what makes filtering,
//! selection movement and "which rows are on screen right now" testable
//! directly, the same split `hyprforge-bluetooth`'s and
//! `hyprforge-network`'s D-Bus-backed modules use for their own models.

use hyprforge_clipboard::{Entry, EntryId, HistoryError};

/// How the history read from disk turned out.
///
/// Kept distinct from an *empty* history — CLAUDE.md is explicit that
/// "this file could not be read" must never collapse into "there is
/// nothing configured". `hyprforge_clipboard::HistoryError::Unreadable`
/// means the index exists and will not parse; that is worth telling the
/// user about, on a screen that shows nothing else, rather than quietly
/// rendering the same blank list a first run would show.
pub enum HistoryState {
    /// Read cleanly. An empty `Vec` here is the sorted history genuinely
    /// having nothing in it — a still-distinct case the view has to
    /// handle by saying so, not by showing a blank box.
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

/// The popup's state: what it knows, what the user has typed, and which
/// row is selected.
pub struct Model {
    history: HistoryState,
    filter: String,
    /// Index into the *filtered* list, not the full history. Always
    /// within bounds of a non-empty filtered list; meaningless (and
    /// never read) when the filtered list is empty.
    selected: usize,
    /// The message from the most recent pin/unpin attempt that did not
    /// succeed — shown in the header in place of the filter text until
    /// the next action of any kind (see `surface::dispatch_action`).
    /// Never carries clipboard content, only an id (a hash) and the
    /// daemon's own reason, or a note that there was nobody to ask.
    ///
    /// Deliberately not "sticky": CLAUDE.md is explicit that a failed
    /// connection must never be cached as a reason to stop trying, and
    /// a notice that lingered after the user had moved on would start to
    /// look exactly like that — this field is cleared the moment
    /// anything else happens, not just on the next successful pin.
    pin_notice: Option<String>,
    /// How tall (in logical pixels) the popup's own scrollable content
    /// area is — the viewport a continuous [`Model::scroll_offset`]
    /// scrolls within. Set once from `geometry::RowLayout::viewport_height`
    /// via [`Model::set_viewport`], the same "derive it, never guess it"
    /// discipline the old row-count `window` field's own doc explained:
    /// a hardcoded figure here could again silently disagree with what
    /// the popup's fixed size actually has room for.
    viewport_height: f64,
    /// One row's own height — `geometry::RowLayout::row_height` at the
    /// current theme, needed here (rather than kept purely in `view.rs`
    /// and `geometry.rs`) so this module can convert "row index" to
    /// "pixels" for its own scroll-into-view arithmetic.
    row_height: f64,
    /// The gap between two rows — `geometry::RowLayout::row_spacing`.
    row_spacing: f64,
    /// How far, in pixels, the visible window has scrolled down into the
    /// filtered list's own stacked rows — **state**, not a value derived
    /// fresh from `selected` on every call, for the same reason the old
    /// `window_start` field's doc gave: `visible_range` used to recompute
    /// a window centred on `selected` every time it was called, which
    /// reads fine for keyboard movement but is exactly wrong for the
    /// mouse — hovering a row calls `Model::select` on it (see
    /// `surface::pointer_move`), and a window that recentres on whatever
    /// was just selected puts a *different* row under a pointer that
    /// never moved, which is indistinguishable from the list scrolling on
    /// its own. Keeping this as state and only ever nudging it just far
    /// enough to keep the selection inside (see
    /// [`hyprforge_popup::scroll_into_view`]) is what makes "the row
    /// already under the pointer is already visible" a no-op instead of a
    /// re-centre — continuous pixels instead of whole rows, but the same
    /// rule.
    scroll_offset: f64,
    /// A short label for where a chosen entry will be pasted — "Ghostty",
    /// say — shown in the header before anything is chosen. Cosmetic
    /// only: nothing here reads this to decide *how* to paste; that
    /// decision (`target::paste_shortcut`) is made once in `main.rs` from
    /// the same focused-window lookup this label came from, and handed to
    /// `ClipMenu::run` separately. `None` when `main.rs` could not work
    /// out what had focus (an empty desktop, or `hyprctl` not answering)
    /// — the header falls back to its plain "Type to filter" text rather
    /// than naming a window that was never found.
    paste_target: Option<String>,
}

/// [`Model::viewport_height`]'s value (and a plausible row size) until
/// [`Model::set_viewport`] is called. Every test in this crate that does
/// not care about scrolling leaves it at this default — big enough for
/// several dozen ordinary rows, so filtering and selection tests never
/// have to think about a window at all; `main.rs` always overrides it
/// with the popup's real, theme-derived geometry before showing anything.
const DEFAULT_VIEWPORT_HEIGHT: f64 = 1000.0;
const DEFAULT_ROW_HEIGHT: f64 = 40.0;
const DEFAULT_ROW_SPACING: f64 = 2.0;

impl Model {
    pub fn new(history: HistoryState) -> Model {
        Model {
            history,
            filter: String::new(),
            selected: 0,
            pin_notice: None,
            viewport_height: DEFAULT_VIEWPORT_HEIGHT,
            row_height: DEFAULT_ROW_HEIGHT,
            row_spacing: DEFAULT_ROW_SPACING,
            scroll_offset: 0.0,
            paste_target: None,
        }
    }

    /// Sets the popup's own scrollable geometry — the viewport height and
    /// one row's height/spacing at the current theme, all three read from
    /// `geometry::RowLayout` (`viewport_height`, `row_height`,
    /// `row_spacing`) rather than guessed here, the same "derive it, never
    /// duplicate it" discipline the old `window` field's own doc
    /// described. Floors both height figures at something positive so a
    /// degenerate popup size cannot leave this dividing by, or scrolling
    /// through, zero.
    ///
    /// Re-syncs [`Model::scroll_offset`] afterward: a viewport that just
    /// shrank could otherwise leave the selection outside it until the
    /// next unrelated action nudged things back into range.
    pub fn set_viewport(&mut self, viewport_height: f64, row_height: f64, row_spacing: f64) {
        self.viewport_height = viewport_height.max(0.0);
        self.row_height = row_height.max(1.0);
        self.row_spacing = row_spacing.max(0.0);
        self.sync_scroll();
    }

    /// One row's own height plus the spacing after it — the pixel unit
    /// [`Model::scroll_by`] moves the view by one wheel notch (see
    /// `surface::ClipApp::pointer_scroll`), and the unit every other
    /// pixel/row conversion in this module uses.
    pub fn row_stride(&self) -> f64 {
        self.row_height + self.row_spacing
    }

    /// The total height, in pixels, of every filtered row stacked with
    /// its own spacing — the "content height" half of the scrollbar and
    /// offset-clamping arithmetic in [`hyprforge_popup::scrollbar`].
    pub fn content_height(&self) -> f64 {
        let len = self.filtered().len();
        if len == 0 {
            return 0.0;
        }
        len as f64 * self.row_stride() - self.row_spacing
    }

    pub fn viewport_height(&self) -> f64 {
        self.viewport_height
    }

    /// How far the view has scrolled, in pixels — what
    /// [`hyprforge_popup::Scrollbar`] positions its thumb from, and what
    /// `view.rs` shifts the rendered rows up by (via
    /// [`Model::scroll_remainder`]).
    pub fn scroll_offset(&self) -> f64 {
        self.scroll_offset
    }

    /// Scrolls by `delta` pixels (negative is up) — the wheel's own
    /// path, distinct from [`Model::move_selection`]: scrolling moves the
    /// *view*, not the selection, so a wheel notch no longer snaps the
    /// selection (and the window) a whole row at a time the way it used
    /// to.
    ///
    /// Also what a scrollbar-thumb drag applies through — `surface.rs`
    /// turns a drag's pointer motion into an offset *delta* via
    /// `hyprforge_popup::Scrollbar::drag_delta_to_offset_delta` and hands
    /// it here the same way a wheel notch does, rather than this module
    /// needing a second, absolute-offset setter.
    pub fn scroll_by(&mut self, delta: f64) {
        self.scroll_offset = hyprforge_popup::clamp_offset(self.scroll_offset + delta, self.content_height(), self.viewport_height);
    }

    /// The row index (into the filtered list) of the first row actually
    /// built into a widget right now — the single source both this
    /// module's own [`Model::visible_range`] and `view.rs`'s rendering
    /// read, so "which row is first" can never drift between the two.
    fn first_visible_row(&self) -> usize {
        let stride = self.row_stride();
        if stride <= 0.0 {
            return 0;
        }
        (self.scroll_offset / stride).floor().max(0.0) as usize
    }

    /// How many pixels of the first built row are already scrolled past
    /// — the amount `view.rs` shifts the rendered rows up by so a
    /// partially-visible row at the top reads as partially visible
    /// rather than snapping to a row boundary. [`crate::geometry::RowLayout::row_at`]
    /// reads the exact same number (as `scroll_remainder`) before
    /// hit-testing, which is what keeps drawn and hit-tested rows from
    /// disagreeing under a scroll offset — see that method's own doc.
    pub fn scroll_remainder(&self) -> f64 {
        self.scroll_offset - self.first_visible_row() as f64 * self.row_stride()
    }

    /// The label [`Model::set_paste_target`] set, if any — see that
    /// field's own doc.
    pub fn paste_target(&self) -> Option<&str> {
        self.paste_target.as_deref()
    }

    pub fn set_paste_target(&mut self, target: Option<String>) {
        self.paste_target = target;
    }

    /// Moves [`Model::scroll_offset`] the minimum amount needed to bring
    /// the current selection back inside the viewport — see
    /// [`hyprforge_popup::scroll_into_view`]'s own doc for the rule.
    /// Called after anything that can move `selected` or shrink/grow the
    /// filtered list out from under it: typing, backspace, an explicit
    /// selection, or a changed viewport.
    fn sync_scroll(&mut self) {
        let len = self.filtered().len();
        if len == 0 {
            self.scroll_offset = 0.0;
            return;
        }
        let selected = self.selected.min(len - 1);
        let stride = self.row_stride();
        let item_top = selected as f64 * stride;
        let item_bottom = item_top + self.row_height;
        let offset = hyprforge_popup::scroll_into_view(self.scroll_offset, item_top, item_bottom, self.viewport_height);
        self.scroll_offset = hyprforge_popup::clamp_offset(offset, self.content_height(), self.viewport_height);
    }

    /// The entries matching the current filter, newest-first /
    /// pinned-first exactly as the store ordered them — filtering only
    /// removes rows, it never reorders what is left.
    ///
    /// Matching is case-insensitive substring matching against the same
    /// one-line preview the row renders, so what the user sees is what
    /// they can search for. An empty filter matches everything, which is
    /// also the state the popup opens in.
    pub fn filtered(&self) -> Vec<&Entry> {
        let HistoryState::Loaded(entries) = &self.history else {
            return Vec::new();
        };
        if self.filter.is_empty() {
            return entries.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        entries
            .iter()
            .filter(|e| e.content.preview(usize::MAX).to_lowercase().contains(&needle))
            .collect()
    }

    pub fn history(&self) -> &HistoryState {
        &self.history
    }

    pub fn filter_text(&self) -> &str {
        &self.filter
    }

    /// Appends to the filter, as a keystroke does. Resets the selection
    /// to the top of the new (generally shorter, and always different)
    /// filtered list — keeping an index into a list that just changed
    /// underneath it would either select the wrong row or nothing.
    pub fn type_char(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
        self.sync_scroll();
    }

    pub fn backspace(&mut self) {
        self.filter.pop();
        self.selected = 0;
        self.sync_scroll();
    }

    /// Moves the selection by `delta` rows (negative is up), clamped to
    /// the ends of the filtered list rather than wrapping — the
    /// behaviour the task asks for by name, and the one a Windows user
    /// actually expects: Up at the top does nothing, it does not jump to
    /// the bottom.
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

    /// Sets the selection to exactly this row of the filtered list —
    /// what the pointer entering or moving over a row means, as opposed
    /// to [`Model::move_selection`]'s relative Up/Down. Out-of-range is
    /// clamped rather than ignored: a hit-test racing a list that just
    /// got shorter (the user typed a filter character between the
    /// pointer motion and this call landing) should still select
    /// *something* sane rather than silently do nothing.
    pub fn select(&mut self, index: usize) {
        let len = self.filtered().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = index.min(len - 1);
        // The whole point of `scroll_offset` being persisted state rather
        // than derived from `selected`: a hover that lands on a row
        // already inside the current viewport (which every hover does,
        // since a hover only ever targets a row `view.rs` actually drew)
        // leaves the offset exactly where it was. See `sync_scroll`'s own
        // doc.
        self.sync_scroll();
    }

    pub fn selected_entry(&self) -> Option<Entry> {
        self.filtered().get(self.selected).map(|e| (*e).clone())
    }

    /// The range of the filtered list worth building real widgets for
    /// right now: a window of [`Model::window`] rows centred on the
    /// selection, clamped to the list's own bounds.
    ///
    /// This is the piece that keeps an image entry from being decoded
    /// (see `thumbnail.rs`) until it is actually one of the rows a
    /// person could be looking at — a history of 500 entries must not
    /// mean 500 decode attempts on every keystroke.
    ///
    /// Under continuous scrolling this is no longer a fixed-size window:
    /// it is every row from [`Model::first_visible_row`] through enough
    /// further rows to cover the viewport (plus one on each end for a
    /// partially visible row at the top or bottom), clamped to the
    /// filtered list's own length. `view.rs` builds exactly this range and
    /// shifts it up by [`Model::scroll_remainder`] — the same range this
    /// module's own doc says must never drift from what is drawn.
    pub fn visible_range(&self) -> std::ops::Range<usize> {
        let len = self.filtered().len();
        if len == 0 {
            return 0..0;
        }
        let start = self.first_visible_row().min(len - 1);
        let stride = self.row_stride();
        let rows_needed = if stride <= 0.0 { 1 } else { (self.viewport_height / stride).ceil() as usize + 2 };
        let end = (start + rows_needed.max(1)).min(len);
        start..end
    }

    /// Reflects a pin/unpin the daemon has already confirmed, in this
    /// popup's own copy of the history — this never writes anything to
    /// disk and never talks to the daemon itself; that already happened
    /// by the time `surface::dispatch_action` calls this. A no-op if
    /// `id` is not in the current history (it was removed or the popup
    /// is showing stale state some other way) rather than a panic.
    ///
    /// Re-sorts with exactly the same rule
    /// `hyprforge_clipboard::store::History::sort` uses — pinned first,
    /// then newest first, ties broken by id — so the row lands where a
    /// fresh load would put it next time the popup opens, instead of
    /// drifting from what the daemon's own file now says. Then keeps
    /// the *same entry* selected across the reorder: the id, not the
    /// index, is what the user cared about, and pinning something is
    /// supposed to move it, not lose the selection.
    pub fn set_entry_pinned(&mut self, id: &EntryId, pinned: bool) {
        let HistoryState::Loaded(entries) = &mut self.history else { return };
        let Some(entry) = entries.iter_mut().find(|e| &e.id == id) else { return };
        entry.pinned = pinned;
        sort_entries(entries);
        if let Some(new_index) = self.filtered().iter().position(|e| &e.id == id) {
            self.selected = new_index;
            self.sync_scroll();
        }
    }

    /// The reason the last pin/unpin attempt did not succeed, if any —
    /// see the field's own doc for why this is cleared so aggressively.
    pub fn pin_notice(&self) -> Option<&str> {
        self.pin_notice.as_deref()
    }

    pub fn set_pin_notice(&mut self, notice: Option<String>) {
        self.pin_notice = notice;
    }
}

/// The same ordering `History::sort` applies on the daemon side, kept as
/// a free function so `set_entry_pinned` can call it without punching a
/// hole in `HistoryState` for the sort itself. Duplicated rather than
/// shared because there is nothing to share *from*: `History::sort` is
/// a private method on a type this crate does not construct (it only
/// ever reads a `Vec<Entry>` out of one) — sharing it would mean making
/// it public API of `hyprforge-clipboard` for exactly one caller outside
/// the daemon.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(b.copied_at.cmp(&a.copied_at))
            .then(a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_clipboard::{Content, EntryId};

    fn text_entry(s: &str) -> Entry {
        let content = Content::Text(s.to_string());
        Entry { id: EntryId::of(&content), content, copied_at: 0, pinned: false }
    }

    fn model_with(texts: &[&str]) -> Model {
        Model::new(HistoryState::Loaded(texts.iter().map(|s| text_entry(s)).collect()))
    }

    #[test]
    fn an_empty_filter_shows_everything() {
        let model = model_with(&["alpha", "beta", "gamma"]);
        assert_eq!(model.filtered().len(), 3);
    }

    #[test]
    fn typing_filters_to_what_was_typed() {
        let mut model = model_with(&["alpha", "beta", "gamma"]);
        for c in "et".chars() {
            model.type_char(c);
        }
        let filtered = model.filtered();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].content, Content::Text("beta".into()));
    }

    #[test]
    fn filtering_is_case_insensitive() {
        let mut model = model_with(&["Alpha", "beta"]);
        model.type_char('A');
        model.type_char('L');
        assert_eq!(model.filtered().len(), 1);
    }

    #[test]
    fn backspace_widens_the_filter_back_out() {
        let mut model = model_with(&["alpha", "beta"]);
        model.type_char('z'); // matches nothing
        assert!(model.filtered().is_empty());
        model.backspace();
        assert_eq!(model.filtered().len(), 2);
    }

    #[test]
    fn moving_down_advances_the_selection() {
        let mut model = model_with(&["a", "b", "c"]);
        model.move_selection(1);
        assert_eq!(model.selected_index(), 1);
    }

    #[test]
    fn the_selection_stops_at_the_bottom_rather_than_wrapping() {
        let mut model = model_with(&["a", "b"]);
        model.move_selection(1);
        model.move_selection(1);
        model.move_selection(1);
        assert_eq!(model.selected_index(), 1, "must stop at the last row, not wrap to 0");
    }

    #[test]
    fn the_selection_stops_at_the_top_rather_than_wrapping() {
        let mut model = model_with(&["a", "b"]);
        model.move_selection(-1);
        model.move_selection(-1);
        assert_eq!(model.selected_index(), 0, "must stop at the first row, not wrap to the end");
    }

    #[test]
    fn selecting_a_row_directly_lands_on_that_row() {
        let mut model = model_with(&["a", "b", "c"]);
        model.select(2);
        assert_eq!(model.selected_index(), 2);
    }

    #[test]
    fn selecting_past_the_end_of_the_list_clamps_to_the_last_row() {
        let mut model = model_with(&["a", "b"]);
        model.select(50);
        assert_eq!(model.selected_index(), 1);
    }

    #[test]
    fn choosing_hands_back_the_selected_entry() {
        let mut model = model_with(&["a", "b", "c"]);
        model.move_selection(1);
        assert_eq!(model.selected_entry().unwrap().content, Content::Text("b".into()));
    }

    #[test]
    fn an_empty_history_shows_a_message_rather_than_a_blank_list() {
        let model = Model::new(HistoryState::Loaded(Vec::new()));
        assert!(model.filtered().is_empty());
        assert!(matches!(model.history(), HistoryState::Loaded(entries) if entries.is_empty()));
    }

    #[test]
    fn an_unreadable_history_is_distinct_from_an_empty_one() {
        let model = Model::new(HistoryState::Unreadable("bad toml".into()));
        assert!(model.filtered().is_empty());
        assert!(matches!(model.history(), HistoryState::Unreadable(_)));
    }

    #[test]
    fn pinning_an_entry_moves_it_above_unpinned_ones() {
        let mut model = model_with(&["a", "b", "c"]);
        let b_id = model.filtered()[1].id.clone();
        model.set_entry_pinned(&b_id, true);
        assert_eq!(model.filtered()[0].id, b_id, "the pinned entry must sort first");
    }

    #[test]
    fn unpinning_lets_an_entry_fall_back_out_of_the_pinned_group() {
        let mut model = model_with(&["a", "b", "c"]);
        let b_id = model.filtered()[1].id.clone();
        model.set_entry_pinned(&b_id, true);
        model.set_entry_pinned(&b_id, false);
        assert!(
            model.filtered().iter().all(|e| !e.pinned),
            "nothing should still be pinned"
        );
    }

    #[test]
    fn pinning_the_selected_entry_keeps_it_selected_after_it_moves() {
        let mut model = model_with(&["a", "b", "c"]);
        model.select(2); // "c"
        let c_id = model.filtered()[2].id.clone();
        model.set_entry_pinned(&c_id, true);
        assert_eq!(
            model.filtered()[model.selected_index()].id,
            c_id,
            "the same entry must still be selected even though its row moved"
        );
    }

    #[test]
    fn pinning_an_id_that_is_not_in_the_history_does_nothing() {
        let mut model = model_with(&["a"]);
        let bogus = EntryId::from_raw("not-a-real-id".to_string());
        model.set_entry_pinned(&bogus, true);
        assert!(model.filtered().iter().all(|e| !e.pinned));
    }

    #[test]
    fn a_fresh_model_has_no_pin_notice() {
        let model = model_with(&["a"]);
        assert_eq!(model.pin_notice(), None);
    }

    #[test]
    fn a_pin_notice_can_be_set_and_cleared() {
        let mut model = model_with(&["a"]);
        model.set_pin_notice(Some("hyprforge-clipd isn't running".to_string()));
        assert_eq!(model.pin_notice(), Some("hyprforge-clipd isn't running"));
        model.set_pin_notice(None);
        assert_eq!(model.pin_notice(), None);
    }

    // --- Continuous scrolling. The old `clamp_window`/`scroll_start` free
    // functions (row-index arithmetic) moved to `hyprforge_popup::scrollbar`
    // as `clamp_offset`/`scroll_into_view` (pixel arithmetic) and are
    // pinned by that crate's own tests; what belongs here is that `Model`
    // actually uses them the way this popup needs — the properties below
    // are the same ones the old `scroll_start`/`clamp_window` tests pinned,
    // reshaped from row indices to pixels.

    const STRIDE: f64 = DEFAULT_ROW_HEIGHT + DEFAULT_ROW_SPACING; // 42.0

    fn model_with_viewport(texts: &[&str], viewport_height: f64) -> Model {
        let mut model = model_with(texts);
        model.set_viewport(viewport_height, DEFAULT_ROW_HEIGHT, DEFAULT_ROW_SPACING);
        model
    }

    /// The property `main.rs` depends on: telling the model a new
    /// viewport size actually changes how many rows `visible_range`
    /// builds, rather than the field being write-only.
    #[test]
    fn set_viewport_changes_how_many_rows_are_built() {
        let mut model = model_with(&["a", "b", "c", "d", "e"]);
        model.set_viewport(50.0, DEFAULT_ROW_HEIGHT, DEFAULT_ROW_SPACING);
        let small = model.visible_range().len();
        model.set_viewport(500.0, DEFAULT_ROW_HEIGHT, DEFAULT_ROW_SPACING);
        let big = model.visible_range().len();
        assert!(big > small, "a taller viewport must build more rows ({small} vs {big})");
    }

    /// A degenerate (zero-height) viewport must still leave
    /// `visible_range` able to build at least one row for a non-empty
    /// list, the pixel-offset version of the old `set_window`'s floor of
    /// one.
    #[test]
    fn a_zero_height_viewport_still_builds_at_least_one_row() {
        let mut model = model_with(&["a", "b"]);
        model.set_viewport(0.0, DEFAULT_ROW_HEIGHT, DEFAULT_ROW_SPACING);
        assert!(!model.visible_range().is_empty());
    }

    // --- The auto-scroll regression, exercised through `Model` itself —
    // these are the properties the owner reported by name, now asserted
    // against `scroll_offset` (continuous pixels) rather than a row-index
    // window.

    /// The bug: hovering a row that is already on screen re-centred the
    /// window under it, since the old rule derived the window fresh from
    /// `selected` on every call. A hover must be a true no-op when the
    /// row it lands on is already visible.
    #[test]
    fn hovering_a_row_that_is_already_visible_does_not_move_the_offset() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 3.0 * STRIDE - DEFAULT_ROW_SPACING);
        model.move_selection(2); // selected = 2, still inside the initial viewport
        let before = model.scroll_offset();
        model.select(1); // hover row 1 — already visible
        assert_eq!(model.scroll_offset(), before, "hovering an already-visible row must not scroll");
    }

    #[test]
    fn moving_past_the_bottom_edge_scrolls_by_exactly_one_rows_worth_of_pixels() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 3.0 * STRIDE - DEFAULT_ROW_SPACING);
        model.move_selection(2); // selected = 2, the viewport's own last visible row
        assert_eq!(model.scroll_offset(), 0.0);
        model.move_selection(1); // selected = 3, one row past the viewport
        assert_eq!(model.scroll_offset(), STRIDE, "the viewport must slide down by exactly one row's stride");
    }

    #[test]
    fn moving_past_the_top_edge_scrolls_by_exactly_one_rows_worth_of_pixels() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 3.0 * STRIDE - DEFAULT_ROW_SPACING);
        model.select(4); // jump to the end
        let scrolled = model.scroll_offset();
        assert!(scrolled > 0.0);
        model.move_selection(-2); // selected = 2 — the viewport's own top edge, not past it yet
        assert_eq!(model.scroll_offset(), scrolled, "the viewport's own top edge is still inside it");
        model.move_selection(-1); // selected = 1, now past the top edge
        assert_eq!(model.scroll_offset(), scrolled - STRIDE, "the viewport must slide up by exactly one row's stride");
    }

    #[test]
    fn changing_the_filter_keeps_the_offset_valid_for_the_new_shorter_list() {
        let mut model = model_with_viewport(&["aa", "ab", "cc", "dd", "ee"], 3.0 * STRIDE - DEFAULT_ROW_SPACING);
        model.select(4); // jump to the end of the full 5-item list — scrolls down
        assert!(model.scroll_offset() > 0.0);
        model.type_char('a'); // filters down to ["aa", "ab"] — 2 rows, which fit the viewport whole
        assert_eq!(model.filtered().len(), 2);
        assert_eq!(
            model.scroll_offset(),
            0.0,
            "a filtered-down list shorter than the viewport must clamp back to the top"
        );
    }

    // --- The rest of the pixel-offset arithmetic `Model` itself owns:
    // clamping (never above the top, never past the point where the last
    // row sits at the bottom of the viewport) and the remainder `view.rs`
    // and `geometry::RowLayout::row_at` both read to agree on where a
    // partially-scrolled first row actually is.

    #[test]
    fn scrolling_up_past_the_top_clamps_at_zero() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 100.0);
        model.scroll_by(-500.0);
        assert_eq!(model.scroll_offset(), 0.0);
    }

    #[test]
    fn scrolling_down_clamps_at_the_point_the_last_row_reaches_the_bottom() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 100.0);
        model.scroll_by(1_000_000.0);
        assert_eq!(model.scroll_offset(), model.content_height() - model.viewport_height());
    }

    #[test]
    fn a_short_list_that_already_fits_cannot_be_scrolled_into_empty_space() {
        let mut model = model_with_viewport(&["a", "b"], 1000.0);
        model.scroll_by(500.0);
        assert_eq!(model.scroll_offset(), 0.0, "content shorter than the viewport must not scroll at all");
    }

    #[test]
    fn scroll_remainder_is_how_far_the_offset_sits_into_the_first_built_row() {
        let mut model = model_with_viewport(&["a", "b", "c", "d", "e"], 100.0);
        model.scroll_by(50.0); // one whole stride (42) plus 8px into the next row
        assert_eq!(model.scroll_remainder(), 8.0);
    }

    // --- `paste_target`: purely cosmetic label state, never consulted
    // for anything but the header text `view.rs` shows.

    #[test]
    fn a_fresh_model_has_no_paste_target() {
        let model = model_with(&["a"]);
        assert_eq!(model.paste_target(), None);
    }

    #[test]
    fn a_paste_target_can_be_set_and_read_back() {
        let mut model = model_with(&["a"]);
        model.set_paste_target(Some("Ghostty".to_string()));
        assert_eq!(model.paste_target(), Some("Ghostty"));
    }
}
