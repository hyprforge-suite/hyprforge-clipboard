//! The clipboard popup's own row geometry — which row a pointer position
//! lands on, and what part of it.
//!
//! `Point`/`Size`/`Monitor` and the placement math (`clamp_popup`,
//! `monitor_at`) that used to live in this file moved to
//! `hyprforge-popup::geometry` and `hyprforge-popup::placement`: they
//! know nothing about a clipboard's rows, and a future grid-shaped
//! picker needs the exact same placement, so it is shared machinery now
//! rather than something this crate happens to also define. `RowLayout`
//! stayed here, deliberately not shared as-is — see
//! `hyprforge-popup::popup`'s module doc for why a list's row geometry
//! and a grid's cell geometry are not the same shape, and for the seam
//! (`PopupApp`) this crate's own `surface::ClipApp` implements instead.
//!
//! Everything here is deliberately free of `hyprctl`, Wayland or iced —
//! it only knows about points and sizes. That is what makes it testable
//! without a compositor: the piece most likely to be wrong (an
//! off-by-one at an edge, a hit that lands in the gap between two rows)
//! is exactly the piece a unit test can pin directly, without a monitor
//! to click on.

/// Where the popup's rows are, in the same logical-pixel space `view.rs`
/// draws into — so a pointer position can be turned into a row index
/// without an iced runtime to ask, since this popup builds one
/// [`iced_runtime::user_interface::UserInterface`] per frame and throws it
/// away rather than keeping one running that could answer "what's under
/// the cursor" itself (see `surface.rs::draw`).
///
/// Every number here is a value `view.rs` sets explicitly on the widgets
/// it builds (a fixed row height, a fixed header height, a literal
/// spacing) rather than something read back from iced's own text
/// metrics. That is deliberate: a row's fixed height only ever depends on
/// the theme's font size, the one thing both this module and `view.rs`
/// already have, so the two cannot silently drift out of step the way
/// they could if this tried to reverse-engineer cosmic-text's line
/// metrics instead. If a row's drawn geometry ever changes in `view.rs`,
/// change the arithmetic here to match — that is the whole contract.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowLayout {
    /// Padding around the whole popup's content, all four sides —
    /// `container::padding` in `view::view`.
    pub padding: f64,
    /// Height of the header line ("Type to filter" / the current filter
    /// text) plus the gap under it, before the first row starts.
    pub header_height: f64,
    /// Height of one entry row, thumbnail or text alike — both are drawn
    /// into a container of this same fixed height (see
    /// `view::entry_row`), so an image row hit-tests identically to a
    /// text one.
    pub row_height: f64,
    /// Vertical gap between two rows — `column::spacing` in `view::view`.
    pub row_spacing: f64,
    /// Fixed width of the relative-age label at a row's right edge —
    /// `view::entry_row`'s `age_box` container. Fixed (not left to the
    /// text's own measured width) for the same reason the pin toggle's
    /// size is fixed: whatever sits at a deterministic offset from the
    /// row's right edge has to be at a pixel [`Self::hit_test`] can
    /// compute without asking iced how wide a string turned out to be.
    pub time_width: f64,
    /// Side length of the pin toggle — `view::entry_row`'s `pin_toggle`
    /// container. Same reasoning as `time_width`.
    pub pin_size: f64,
}

impl RowLayout {
    /// Popup padding, in logical pixels — `Padding::from(10)` in
    /// `view::view`.
    pub const PADDING: f64 = 10.0;
    /// Gap between the header and the first row —
    /// `Space::new().height(6)` in `view::view`. `pub` so `view.rs` can
    /// size the header text to exactly `header_height - HEADER_GAP` and
    /// let the literal `Space` widget account for the rest, rather than
    /// this module and `view.rs` each hard-coding `6.0` and hoping they
    /// stay equal.
    pub const HEADER_GAP: f64 = 6.0;
    /// Gap between rows — `column(rows).spacing(2)` in `view::view`.
    pub const ROW_SPACING: f64 = 2.0;
    /// Padding inside each row's own container, top and bottom —
    /// `Padding::from(6)` in `view::entry_row`.
    pub const ROW_PADDING: f64 = 6.0;
    /// Side length of an image row's thumbnail — `THUMBNAIL_SIZE` in
    /// `view::entry_row`. `pub` (rather than a second constant of the
    /// same value living in `view.rs`) because a row's height has to be
    /// tall enough for whichever of a thumbnail or a line of text is
    /// bigger, and this module is the one place that arithmetic happens.
    pub const THUMBNAIL_SIZE: f64 = 32.0;
    /// Gap on either side of the pin toggle, between it and the preview
    /// on one side and the time label on the other —
    /// `view::entry_row`'s two `Space::new().width(RowLayout::PIN_GAP)`
    /// calls.
    pub const PIN_GAP: f64 = 8.0;

    /// Derives the layout from the theme's font size, the one variable
    /// both sides of the hit-test already agree on.
    ///
    /// A row holds exactly one line — `view::entry_row`'s label sets
    /// `Wrapping::None` precisely so a row's height cannot depend on how
    /// much text happened to be in it — so a text row's content height is
    /// one line at the theme's font size, `LineHeight`'s default relative
    /// factor of `1.2` applied the same way iced applies it. An image row
    /// draws a fixed-size thumbnail instead of a line of text (see
    /// `view::entry_row`'s `Content::Image` branch), so the row height has
    /// to fit whichever of the two is taller — a small font with a large
    /// thumbnail must not clip the thumbnail, and a large font with the
    /// thumbnail's fixed size must not clip the text.
    pub fn for_font_size(font_size: f32) -> RowLayout {
        let font_size = font_size as f64;
        let line = |size: f64| size * 1.2;
        let content_height = line(font_size).max(Self::THUMBNAIL_SIZE);
        RowLayout {
            padding: Self::PADDING,
            // The search field is the same height as a row, not the
            // height of its own text. It is the one control in this
            // popup the user types into, and a field noticeably smaller
            // than every row under it reads as a label rather than
            // something to type in.
            //
            // `row_at` subtracts this, so the header and the rows cannot
            // drift apart by changing it — and `rows_that_fit` takes it
            // out of the available height, so a taller header simply
            // means one fewer row rather than a row that overflows.
            header_height: content_height + Self::ROW_PADDING * 2.0 + Self::HEADER_GAP,
            row_height: content_height + Self::ROW_PADDING * 2.0,
            row_spacing: Self::ROW_SPACING,
            // Wide enough for the longest strings `view::relative_age`
            // actually prints ("59m", "23h", "6d", …) at the label's own
            // 0.75x size, with a little slack — scaled with the font so
            // a bigger theme doesn't clip its own time label.
            time_width: (font_size * 2.0).max(28.0),
            // A small round toggle, big enough to read pinned-or-not at
            // a glance and to hit with a pointer, but never so big it
            // competes with the preview text for space. Clamped rather
            // than scaled without bound, the same reasoning
            // `sane_font_size` bounds the font itself for.
            pin_size: (font_size * 1.1).clamp(14.0, 22.0),
        }
    }

    /// How tall the popup's own scrollable content area is below the
    /// header — the viewport height a [`hyprforge_popup::Scrollbar`] and
    /// [`hyprforge_popup::clamp_offset`] both need, and the same
    /// "available" figure [`Self::rows_that_fit`] floors to a whole row
    /// count. Kept separate from that method because continuous
    /// scrolling wants the raw pixel figure — a partially visible row at
    /// the top or bottom is expected now, not floored away.
    pub fn viewport_height(&self, popup_height: f64) -> f64 {
        (popup_height - self.padding * 2.0 - self.header_height).max(0.0)
    }

    /// The total height of `row_count` rows stacked with their own
    /// spacing between them (but none trailing the last one) — the
    /// "content height" half of the scrollbar/offset arithmetic, read
    /// against [`Self::viewport_height`] by both `Model::max_scroll` and
    /// wherever a caller builds a [`hyprforge_popup::Scrollbar`].
    pub fn content_height(&self, row_count: usize) -> f64 {
        if row_count == 0 {
            return 0.0;
        }
        let stride = self.row_height + self.row_spacing;
        row_count as f64 * stride - self.row_spacing
    }

    /// The scrollbar's own track geometry for a popup `popup_width` wide
    /// showing a viewport `viewport_height` tall — the **one** place
    /// this crate computes it, read by both `surface.rs` (hit-testing a
    /// press or a drag against the thumb) and `view.rs` (drawing the
    /// track and thumb). Two independently-written copies of "where the
    /// scrollbar is" is exactly the class of drift CLAUDE.md's "the
    /// thing drawn, the thing hit-tested" rule warns about, applied here
    /// to the scrollbar rather than a row.
    pub fn scrollbar(&self, popup_width: f64, viewport_height: f64) -> hyprforge_popup::Scrollbar {
        let track_x = popup_width - self.padding - hyprforge_popup::Scrollbar::WIDTH;
        let track_top = self.padding + self.header_height;
        hyprforge_popup::Scrollbar::new(track_x, track_top, viewport_height)
    }

    /// How many rows actually fit in a popup `height` tall.
    ///
    /// The inverse of [`Self::row_at`], and it has to stay that way. The
    /// popup used to show a fixed twenty-four rows regardless of its own
    /// height: at a 44px row in a 420px popup that laid out 1104px of
    /// rows inside a surface a quarter that size, so the rows could not
    /// possibly be where `row_at` computed them, and the highlight
    /// landed nowhere near the pointer. Rows that are drawn and rows
    /// that are hit-tested have to be the same rows, and the count is
    /// as much a part of that as the positions are.
    pub fn rows_that_fit(&self, height: f64) -> usize {
        let available = height - self.padding * 2.0 - self.header_height;
        if available <= 0.0 {
            return 0;
        }
        let stride = self.row_height + self.row_spacing;
        if stride <= 0.0 {
            return 0;
        }
        // The last row needs no spacing under it, so a popup with room
        // for exactly N rows and N-1 gaps fits N.
        (((available + self.row_spacing) / stride).floor() as usize).max(1)
    }

    /// The row index under `local_y` — measured from the popup surface's
    /// own top-left corner, exactly the coordinate space
    /// `PointerEvent::position` reports — among `visible_count` rows
    /// currently built (see `Model::visible_range`).
    ///
    /// `scroll_offset` is how many pixels of content sit above the first
    /// *built* row (`Model::scroll_offset`, converted to "pixels within
    /// the built window" the same way `Model::window_remainder` does) —
    /// added to `local_y` before anything else so this and `view.rs`'s
    /// own vertical shift of the rendered rows can never disagree about
    /// where row 0 of the window actually is. Passing `0.0` reproduces
    /// this method's pre-scrolling behaviour exactly.
    ///
    /// `None` covers every way a position is not over a row: above the
    /// first row (still in the header or its padding), in the gap
    /// between two rows, or past the last row that is actually on
    /// screen. All three are "do nothing", the same as a keyboard press
    /// this popup does not recognise.
    pub fn row_at(&self, local_y: f64, visible_count: usize, scroll_remainder: f64) -> Option<usize> {
        if visible_count == 0 {
            return None;
        }
        let y = local_y - self.padding - self.header_height + scroll_remainder;
        if y < 0.0 {
            return None;
        }
        let stride = self.row_height + self.row_spacing;
        let index = (y / stride) as usize;
        if index >= visible_count {
            return None;
        }
        // Reject a hit that landed in the gap *after* this row's own
        // height, rather than clamping it to the row anyway — a gap that
        // silently selected whichever row was nearest would make the
        // dead zone between two rows pick one of them at random as far
        // as the user could tell.
        let within_row = y - (index as f64 * stride);
        if within_row > self.row_height {
            return None;
        }
        Some(index)
    }

    /// *What*, not just *which row*, a pointer position lands on: the
    /// pin toggle at a row's right edge, or the rest of that row (the
    /// preview, its padding, and the time label — everywhere a click
    /// means "choose", not "pin").
    ///
    /// `popup_width` is the surface's actual current width (`main.rs`
    /// keeps this fixed, but nothing here assumes that) — needed because
    /// the pin toggle and the time label are positioned from the row's
    /// *right* edge, not its left, so their pixel offsets move with the
    /// popup's width the same way `view::entry_row`'s layout does. That
    /// positioning has to match `view.rs`'s exactly, the same discipline
    /// `row_height` already keeps with `row_at`: the pin's rectangle
    /// here and the pin's drawn rectangle in `view::entry_row` are
    /// computed from the same four numbers (`padding`, `ROW_PADDING`,
    /// `time_width`, `PIN_GAP`, `pin_size`) for exactly that reason — two
    /// separate arithmetic expressions computing "the same" rectangle
    /// is how the original click-does-nothing bug happened once already.
    pub fn hit_test(&self, popup_width: f64, position: (f64, f64), visible_count: usize, scroll_remainder: f64) -> Option<Hit> {
        let index = self.row_at(position.1, visible_count, scroll_remainder)?;
        let content_right = popup_width - self.padding - Self::ROW_PADDING;
        let time_left = content_right - self.time_width;
        let pin_right = time_left - Self::PIN_GAP;
        let pin_left = pin_right - self.pin_size;
        if position.0 >= pin_left && position.0 <= pin_right {
            Some(Hit::Pin(index))
        } else {
            Some(Hit::Row(index))
        }
    }

    /// The pixel width left over for a row's preview text once the pin
    /// toggle, the time label and the gaps around them have taken their
    /// own space — computed from exactly the same numbers
    /// [`Self::hit_test`] uses for the pin's own rectangle, so a preview
    /// truncated to this width can never run into it.
    ///
    /// A popup narrower than what the pin, the time label and their gaps
    /// alone need floors this at `0.0` rather than going negative — the
    /// same "degenerate input, sane output" discipline `view::corner_radius`
    /// applies to a hostile theme.
    pub fn preview_width(&self, popup_width: f64) -> f64 {
        let content_right = popup_width - self.padding - Self::ROW_PADDING;
        let time_left = content_right - self.time_width;
        let pin_right = time_left - Self::PIN_GAP;
        let pin_left = pin_right - self.pin_size;
        let preview_left = self.padding + Self::ROW_PADDING;
        (pin_left - Self::PIN_GAP - preview_left).max(0.0)
    }
}

/// What a pointer position resolved to, from [`RowLayout::hit_test`] —
/// which row, and whether it was the pin toggle or the rest of the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Anywhere on the row except the pin toggle — the preview, its
    /// padding, or the time label. A click here chooses the row.
    Row(usize),
    /// The pin toggle. A click here pins or unpins the row instead of
    /// choosing it.
    Pin(usize),
}

impl Hit {
    /// The row index, regardless of which part of it was hit — what a
    /// caller wants to move the selection to before acting on whichever
    /// variant this is.
    pub fn row(self) -> usize {
        match self {
            Hit::Row(index) | Hit::Pin(index) => index,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_over_the_header_hits_no_row() {
        let layout = RowLayout::for_font_size(15.0);
        assert_eq!(layout.row_at(0.0, 5, 0.0), None);
        assert_eq!(layout.row_at(layout.padding, 5, 0.0), None);
    }

    #[test]
    fn the_first_row_starts_right_after_the_header() {
        let layout = RowLayout::for_font_size(15.0);
        let first_row_top = layout.padding + layout.header_height;
        assert_eq!(layout.row_at(first_row_top, 5, 0.0), Some(0));
        assert_eq!(
            layout.row_at(first_row_top + layout.row_height - 0.01, 5, 0.0),
            Some(0),
            "must still be row 0 right up to its own bottom edge"
        );
    }

    #[test]
    fn a_pointer_in_the_gap_between_two_rows_hits_neither() {
        let layout = RowLayout::for_font_size(15.0);
        let first_row_top = layout.padding + layout.header_height;
        let gap_middle = first_row_top + layout.row_height + layout.row_spacing / 2.0;
        assert_eq!(layout.row_at(gap_middle, 5, 0.0), None);
    }

    #[test]
    fn successive_rows_are_found_in_order() {
        let layout = RowLayout::for_font_size(15.0);
        let stride = layout.row_height + layout.row_spacing;
        let first_row_top = layout.padding + layout.header_height;
        for index in 0..5 {
            let middle = first_row_top + index as f64 * stride + layout.row_height / 2.0;
            assert_eq!(layout.row_at(middle, 5, 0.0), Some(index));
        }
    }

    #[test]
    fn a_pointer_past_the_last_visible_row_hits_nothing() {
        let layout = RowLayout::for_font_size(15.0);
        let stride = layout.row_height + layout.row_spacing;
        let first_row_top = layout.padding + layout.header_height;
        let past_the_end = first_row_top + 5.0 * stride;
        assert_eq!(layout.row_at(past_the_end, 5, 0.0), None);
    }

    #[test]
    fn an_empty_visible_window_hits_nothing_no_matter_where_the_pointer_is() {
        let layout = RowLayout::for_font_size(15.0);
        assert_eq!(layout.row_at(layout.padding + layout.header_height, 0, 0.0), None);
    }

    // --- `row_at` under a scroll remainder — the same rows shifted up by
    // whatever pixel amount `view.rs` shifted the rendered rows by, so a
    // hit-test and a drawing that agree on the offset must still agree on
    // which row is where.

    #[test]
    fn a_positive_scroll_remainder_shifts_which_row_a_position_hits() {
        let layout = RowLayout::for_font_size(15.0);
        let stride = layout.row_height + layout.row_spacing;
        let first_row_top = layout.padding + layout.header_height;
        // With no scroll, this point is inside row 0.
        assert_eq!(layout.row_at(first_row_top + 2.0, 5, 0.0), Some(0));
        // Scrolled by a whole stride, the same screen position now reads
        // as one row further into the (scrolled) window — exactly the
        // shift `view.rs` draws the rows with.
        assert_eq!(layout.row_at(first_row_top + 2.0, 5, stride), Some(1));
    }

    #[test]
    fn content_height_matches_the_pixels_view_rs_actually_stacks() {
        let layout = RowLayout::for_font_size(15.0);
        let stride = layout.row_height + layout.row_spacing;
        assert_eq!(layout.content_height(3), 3.0 * stride - layout.row_spacing);
        assert_eq!(layout.content_height(0), 0.0, "no rows is no content, not a negative spacing");
    }

    #[test]
    fn viewport_height_is_the_same_available_figure_rows_that_fit_floors() {
        let layout = RowLayout::for_font_size(15.0);
        let height = 420.0;
        let viewport = layout.viewport_height(height);
        let rows = layout.rows_that_fit(height);
        let stride = layout.row_height + layout.row_spacing;
        assert!((rows as f64) * stride - layout.row_spacing <= viewport + 0.001);
    }

    #[test]
    fn a_bigger_font_makes_taller_rows() {
        // Big enough that the *text* line, not the fixed-size thumbnail,
        // is what is driving `row_height` — otherwise two font sizes
        // that both fit under `THUMBNAIL_SIZE` would produce the same
        // row height and this test would prove nothing.
        let small = RowLayout::for_font_size(12.0);
        let large = RowLayout::for_font_size(40.0);
        assert!(large.row_height > small.row_height);
        assert!(large.header_height > small.header_height);
    }

    // --- `RowLayout::hit_test`: which part of a row a pointer position
    // resolved to — the pin toggle, or the rest of the row (which
    // chooses). These are the tests that would have caught the original
    // "clicking does nothing" bug: a click has to land on the same
    // rectangle this computes and `view::entry_row` draws.

    const HIT_POPUP_WIDTH: f64 = 360.0;

    #[test]
    fn a_click_on_the_pin_toggle_hits_the_pin_not_the_row() {
        let layout = RowLayout::for_font_size(15.0);
        let first_row_top = layout.padding + layout.header_height;
        let row_middle_y = first_row_top + layout.row_height / 2.0;
        let content_right = HIT_POPUP_WIDTH - layout.padding - RowLayout::ROW_PADDING;
        let pin_right = content_right - layout.time_width - RowLayout::PIN_GAP;
        let pin_left = pin_right - layout.pin_size;
        let pin_center_x = (pin_left + pin_right) / 2.0;
        assert_eq!(
            layout.hit_test(HIT_POPUP_WIDTH, (pin_center_x, row_middle_y), 5, 0.0),
            Some(Hit::Pin(0))
        );
    }

    #[test]
    fn a_click_on_the_preview_hits_the_row_not_the_pin() {
        let layout = RowLayout::for_font_size(15.0);
        let first_row_top = layout.padding + layout.header_height;
        let row_middle_y = first_row_top + layout.row_height / 2.0;
        assert_eq!(
            layout.hit_test(HIT_POPUP_WIDTH, (layout.padding + 4.0, row_middle_y), 5, 0.0),
            Some(Hit::Row(0))
        );
    }

    #[test]
    fn a_click_in_the_gap_between_rows_hits_neither() {
        let layout = RowLayout::for_font_size(15.0);
        let first_row_top = layout.padding + layout.header_height;
        let stride = layout.row_height + layout.row_spacing;
        let gap_middle = first_row_top + layout.row_height + layout.row_spacing / 2.0;
        assert!(gap_middle < first_row_top + stride);
        assert_eq!(layout.hit_test(HIT_POPUP_WIDTH, (layout.padding + 4.0, gap_middle), 5, 0.0), None);
    }

    #[test]
    fn hit_test_reports_the_row_a_click_landed_on_regardless_of_which_part_it_hit() {
        let layout = RowLayout::for_font_size(15.0);
        let stride = layout.row_height + layout.row_spacing;
        let first_row_top = layout.padding + layout.header_height;
        let second_row_middle = first_row_top + stride + layout.row_height / 2.0;
        let hit = layout.hit_test(HIT_POPUP_WIDTH, (layout.padding + 4.0, second_row_middle), 5, 0.0).unwrap();
        assert_eq!(hit.row(), 1);
    }

    // --- `RowLayout::preview_width`: the space left for a row's preview
    // text once the pin toggle and the time label have their own — this
    // is the number `view::entry_row` truncates the preview to, so it
    // stops running into the pin instead of merely clipping at the row's
    // own far edge.

    #[test]
    fn preview_width_leaves_room_for_the_pin_and_time_label() {
        let layout = RowLayout::for_font_size(15.0);
        let width = layout.preview_width(HIT_POPUP_WIDTH);
        // The preview's right edge (padding + preview_width) must land
        // no further right than where the pin toggle's own rectangle
        // starts (with its gap) — i.e. the two must never overlap.
        let preview_right = layout.padding + RowLayout::ROW_PADDING + width;
        let content_right = HIT_POPUP_WIDTH - layout.padding - RowLayout::ROW_PADDING;
        let pin_right = content_right - layout.time_width - RowLayout::PIN_GAP;
        let pin_left = pin_right - layout.pin_size;
        assert!(
            preview_right <= pin_left - RowLayout::PIN_GAP + 0.001,
            "preview_right {preview_right} must not run into the pin toggle starting at {pin_left}"
        );
    }

    #[test]
    fn a_popup_too_narrow_for_the_pin_and_time_floors_preview_width_at_zero() {
        let layout = RowLayout::for_font_size(15.0);
        assert_eq!(layout.preview_width(0.0), 0.0);
        assert_eq!(layout.preview_width(-100.0), 0.0);
    }

    #[test]
    fn a_small_font_still_leaves_room_for_the_thumbnail() {
        let layout = RowLayout::for_font_size(10.0);
        assert!(
            layout.row_height >= RowLayout::THUMBNAIL_SIZE + RowLayout::ROW_PADDING * 2.0,
            "row_height {} must fit a {}-pixel thumbnail plus its padding",
            layout.row_height,
            RowLayout::THUMBNAIL_SIZE,
        );
    }
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    /// The bug: a fixed twenty-four-row window in a 420px popup laid out
    /// more than twice the popup's height in rows, so nothing the
    /// pointer touched was where `row_at` said it was.
    #[test]
    fn the_row_count_and_the_popup_height_describe_the_same_rows() {
        let layout = RowLayout::for_font_size(14.0);
        let height = 420.0;
        let fits = layout.rows_that_fit(height);

        // Every row it claims fits must hit-test inside the popup.
        let stride = layout.row_height + layout.row_spacing;
        let last_row_bottom =
            layout.padding + layout.header_height + (fits as f64 - 1.0) * stride + layout.row_height;
        assert!(
            last_row_bottom <= height - layout.padding,
            "{fits} rows need {last_row_bottom}px inside {height}px"
        );

        // And the row after it must not, or we are leaving space unused.
        let next_bottom = last_row_bottom + stride;
        assert!(next_bottom > height - layout.padding, "one more row would still have fitted");
    }

    /// A popup with no room left after its own padding and header shows
    /// nothing, rather than dividing by a stride it has no space for.
    /// A popup with *some* room but less than a whole row shows that one
    /// row clipped — the alternative is an empty popup on a screen that
    /// plainly has a few pixels to spare, and `row_at` agrees with it
    /// either way, which is the property that matters.
    #[test]
    fn a_popup_too_short_for_a_row_shows_nothing_rather_than_dividing_by_no_space() {
        let layout = RowLayout::for_font_size(14.0);
        assert_eq!(layout.rows_that_fit(0.0), 0);
        assert_eq!(layout.rows_that_fit(1.0), 0, "1px is not room for a row");

        let barely = layout.padding * 2.0 + layout.header_height + 5.0;
        assert_eq!(layout.rows_that_fit(barely), 1, "a few pixels shows one clipped row");
    }
}
