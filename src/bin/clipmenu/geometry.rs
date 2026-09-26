//! Where everything in the clipboard popup is, and what a pointer
//! position lands on.
//!
//! The popup builds a fresh iced `UserInterface` every frame and throws
//! it away (see `hyprforge_popup::popup::Popup::draw`), so there is no
//! live layout tree to ask "what's under the cursor". This module has to
//! already agree with `view.rs` about where everything is — and it does
//! so by being the *only* place any of it is decided. `view.rs` sizes
//! every region with the numbers in [`Layout`] and never picks one of its
//! own; [`Layout::hit`] measures with the same numbers. CLAUDE.md's rule
//! that the thing drawn and the thing hit-tested must never be two
//! different numbers is the whole contract. If a region's drawn geometry
//! changes, it changes here, and both sides move together.
//!
//! The list's rows are no longer one uniform run — "Pinned" and "Recent"
//! labels sit between them at a different height — so where each line of
//! the list is comes from a [`hyprforge_popup::Stack`] built by the model
//! (see `model::Model::stack`), the same way.
//!
//! # The shape, top to bottom
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │ [ search field                                          ]│  header
//! │ [ All | Text | Images | Links | Files                   ]│
//! ├──────────────────────────┬───────────────────────────────┤
//! │ PINNED                   │ ┌───────────────────────────┐ │
//! │ TXT ssh adam@…    pinned │ │ preview                   │ │  body
//! │ RECENT                   │ └───────────────────────────┘ │
//! │ URL wiki.hyprland…    6m │ Type / Copied / Size          │
//! │ …                        │ [ Paste              Enter ]  │
//! │                          │ [ Pin Ctrl P ][ Delete Del ]  │
//! ├──────────────────────────┴───────────────────────────────┤
//! │ Ctrl P pin  Del delete  Tab filter        Clear history… │  footer
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! Pure arithmetic over points and sizes — no `hyprctl`, Wayland or iced —
//! so the piece most likely to be wrong (an off-by-one at an edge, a hit
//! in the gap between two rows) is the piece a unit test can pin.

use hyprforge_popup::kit::{Rect, Tabs};
use hyprforge_popup::Stack;

/// The popup's width, fixed: the design's 600 — a 330 list and a preview
/// pane beside it.
pub const POPUP_WIDTH: f64 = 600.0;

/// See the module doc. Every field is a logical-pixel value `view.rs`
/// sets explicitly on the widget it describes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub width: f64,
    /// Padding around the header block, all four sides.
    pub padding: f64,
    pub search: Rect,
    pub tabs: Tabs,
    /// Where the body starts: below the header block and the one-pixel
    /// divider under it.
    pub body_top: f64,
    pub body_height: f64,
    /// The list pane's width, including its own inset. The divider
    /// between the panes is the pixel after it.
    pub list_width: f64,
    /// The list's inset from its pane's left, right and bottom edges.
    pub list_inset: f64,
    /// A "Pinned"/"Recent" label line.
    pub header_height: f64,
    /// One entry row.
    pub row_height: f64,
    pub row_spacing: f64,
    /// The preview pane's own padding.
    pub pane_padding: f64,
    pub preview: Rect,
    /// Between the preview well and the Type/Copied/Size lines.
    pub pane_gap: f64,
    /// One line of the Type/Copied/Size block under the preview.
    pub meta_line: f64,
    pub meta_gap: f64,
    pub paste_button: Rect,
    pub pin_button: Rect,
    pub delete_button: Rect,
    pub footer_top: f64,
    pub footer_height: f64,
    /// "Clear history…" at the footer's right-hand end — a fixed box, so
    /// a click is measured against the same rectangle the label is drawn
    /// in rather than against however wide the string happens to render.
    pub clear_button: Rect,
    pub height: f64,
}

/// The design's figures, at the 13px body size it was drawn at. The
/// layout grows past them when the theme's font would not fit, never
/// shrinks below them.
mod design {
    pub const PADDING: f64 = 10.0;
    pub const SEARCH: f64 = 30.0;
    pub const SEARCH_GAP: f64 = 8.0;
    pub const TABS: f64 = 26.0;
    pub const BODY: f64 = 318.0;
    pub const LIST: f64 = 330.0;
    pub const LIST_INSET: f64 = 6.0;
    pub const HEADER: f64 = 26.0;
    pub const ROW: f64 = 34.0;
    pub const PANE_PADDING: f64 = 12.0;
    pub const PREVIEW: f64 = 110.0;
    pub const PANE_GAP: f64 = 10.0;
    pub const BUTTON: f64 = 30.0;
    pub const BUTTON_GAP: f64 = 6.0;
    pub const FOOTER: f64 = 33.0;
    pub const CLEAR: f64 = 150.0;
}

impl Layout {
    /// The one place the popup's geometry is derived — from the theme's
    /// font size alone, the one number this module and `view.rs` both
    /// have.
    pub fn for_font_size(font_size: f32) -> Layout {
        let fs = font_size as f64;
        let line = |size: f64| size * 1.2;
        let width = POPUP_WIDTH;
        let padding = design::PADDING;

        let search_height = design::SEARCH.max(line(fs) + 12.0);
        let search = Rect { x: padding, y: padding, width: width - padding * 2.0, height: search_height };
        let tabs_height = design::TABS.max(line(fs * 0.96) + 10.0);
        let tabs = Tabs {
            x: padding,
            y: search.bottom() + design::SEARCH_GAP,
            width: width - padding * 2.0,
            height: tabs_height,
            count: crate::kind::Filter::ALL.len(),
        };
        let body_top = tabs.y + tabs.height + padding + 1.0;

        let row_height = design::ROW.max(line(fs) + 14.0);
        let grow = row_height / design::ROW;
        let body_height = (design::BODY * grow).round();
        let header_height = design::HEADER.max(line(fs * 0.81) + 13.0);

        let list_width = design::LIST;
        let pane_padding = design::PANE_PADDING;
        let pane_x = list_width + 1.0 + pane_padding;
        let pane_width = width - pane_x - pane_padding;
        let preview = Rect { x: pane_x, y: body_top + pane_padding, width: pane_width, height: design::PREVIEW };

        let button = design::BUTTON.max(line(fs) + 12.0);
        let pane_bottom = body_top + body_height - pane_padding;
        let half = (pane_width - design::BUTTON_GAP) / 2.0;
        let pin_button = Rect { x: pane_x, y: pane_bottom - button, width: half, height: button };
        let delete_button = Rect { x: pane_x + half + design::BUTTON_GAP, y: pin_button.y, width: half, height: button };
        let paste_button = Rect { x: pane_x, y: pin_button.y - design::BUTTON_GAP - button, width: pane_width, height: button };

        let footer_top = body_top + body_height + 1.0;
        let footer_height = design::FOOTER.max(line(fs * 0.88) + 16.0);
        let clear_button = Rect { x: width - 12.0 - design::CLEAR, y: footer_top, width: design::CLEAR, height: footer_height };

        Layout {
            width,
            padding,
            search,
            tabs,
            body_top,
            body_height,
            list_width,
            list_inset: design::LIST_INSET,
            header_height,
            row_height,
            row_spacing: 0.0,
            pane_padding,
            preview,
            pane_gap: design::PANE_GAP,
            meta_line: (fs * 0.88 * 1.3).ceil(),
            meta_gap: 6.0,
            paste_button,
            pin_button,
            delete_button,
            footer_top,
            footer_height,
            clear_button,
            height: footer_top + footer_height,
        }
    }

    /// How tall the list's own scrolling area is: the body, less the
    /// list's bottom inset.
    pub fn viewport_height(&self) -> f64 {
        (self.body_height - self.list_inset).max(0.0)
    }

    /// The list's scrollbar — the one place its track is computed, read
    /// by both the drawing and the thumb drag. It sits in the list's
    /// right-hand inset, clear of the rows' text.
    pub fn scrollbar(&self) -> hyprforge_popup::Scrollbar {
        let track_x = self.list_width - hyprforge_popup::Scrollbar::WIDTH - 1.0;
        hyprforge_popup::Scrollbar::new(track_x, self.body_top, self.viewport_height())
    }

    /// What `position` lands on. `stack` is the list's current lines and
    /// `offset` how far it is scrolled — the same two values `view.rs`
    /// draws the list from.
    pub fn hit(&self, position: (f64, f64), stack: &Stack, offset: f64) -> Option<Hit> {
        if let Some(tab) = self.tabs.tab_at(position) {
            return Some(Hit::Tab(tab));
        }
        for (rect, hit) in [
            (self.paste_button, Hit::Paste),
            (self.pin_button, Hit::Pin),
            (self.delete_button, Hit::Delete),
            (self.clear_button, Hit::Clear),
        ] {
            if rect.contains(position) {
                return Some(hit);
            }
        }
        let (x, y) = position;
        let inside_list = x >= self.list_inset
            && x <= self.list_width - self.list_inset
            && y >= self.body_top
            && y <= self.body_top + self.viewport_height();
        if !inside_list {
            return None;
        }
        stack.line_at(y - self.body_top + offset).map(Hit::Line)
    }

    /// How many characters of a one-line preview fit in `available`
    /// pixels at `font_size`. An estimate — there is no shaped-text
    /// measurement to ask before drawing — with the row's own clip as the
    /// backstop for anything it undershoots. `0.6` is a plain average
    /// glyph width for a proportional face, generous enough that ordinary
    /// text fits inside it.
    pub fn chars_that_fit(font_size: f32, available: f64) -> usize {
        let average = (font_size as f64 * 0.6).max(1.0);
        ((available / average).floor() as usize).max(1)
    }
}

/// What a pointer position resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// One of the filter tabs, by index into `kind::Filter::ALL`.
    Tab(usize),
    /// A line of the list, by index into the model's stack — a row or a
    /// section label; the caller asks the model which.
    Line(usize),
    Paste,
    Pin,
    Delete,
    Clear,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::for_font_size(13.0)
    }

    #[test]
    fn at_the_designs_own_font_size_the_layout_is_the_designs() {
        let l = layout();
        assert_eq!(l.search.height, 30.0);
        assert_eq!(l.tabs.height, 26.0);
        assert_eq!(l.row_height, 34.0);
        assert_eq!(l.header_height, 26.0);
        assert_eq!(l.body_height, 318.0);
        assert_eq!(l.list_width, 330.0);
        assert_eq!(l.preview.height, 110.0);
    }

    #[test]
    fn a_bigger_font_grows_the_rows_rather_than_overflowing_them() {
        let big = Layout::for_font_size(24.0);
        assert!(big.row_height >= 24.0 * 1.2, "a row must hold a line of its own text");
        assert!(big.height > layout().height);
    }

    /// Every region sits inside the popup and none overlaps the next.
    #[test]
    fn the_regions_stack_without_overlapping_and_fit_the_popup() {
        for fs in [9.0, 13.0, 15.0, 20.0] {
            let l = Layout::for_font_size(fs);
            assert!(l.tabs.y >= l.search.bottom());
            assert!(l.body_top >= l.tabs.y + l.tabs.height);
            assert!(l.preview.bottom() <= l.paste_button.y, "the preview must end above the buttons at {fs}");
            assert!(l.paste_button.bottom() <= l.pin_button.y);
            assert!(l.pin_button.right() <= l.delete_button.x);
            assert!(l.delete_button.right() <= l.width - l.pane_padding + 0.001);
            assert!(l.delete_button.bottom() <= l.footer_top);
            assert!(l.clear_button.bottom() <= l.height + 0.001);
        }
    }

    #[test]
    fn a_click_on_each_control_is_that_control() {
        let l = layout();
        let stack = Stack::new([], 0.0);
        let middle = |r: Rect| (r.x + r.width / 2.0, r.y + r.height / 2.0);
        assert_eq!(l.hit(middle(l.paste_button), &stack, 0.0), Some(Hit::Paste));
        assert_eq!(l.hit(middle(l.pin_button), &stack, 0.0), Some(Hit::Pin));
        assert_eq!(l.hit(middle(l.delete_button), &stack, 0.0), Some(Hit::Delete));
        assert_eq!(l.hit(middle(l.clear_button), &stack, 0.0), Some(Hit::Clear));
        assert_eq!(l.hit((l.tabs.x + 20.0, l.tabs.y + 10.0), &stack, 0.0), Some(Hit::Tab(0)));
    }

    #[test]
    fn a_list_line_is_hit_where_it_is_drawn_scrolled_or_not() {
        let l = layout();
        let stack = Stack::new([l.header_height, l.row_height, l.row_height, l.row_height], l.row_spacing);
        let x = l.list_width / 2.0;
        let at = |line: usize, offset: f64| l.body_top + stack.top(line) - offset + stack.height(line) / 2.0;
        assert_eq!(l.hit((x, at(0, 0.0)), &stack, 0.0), Some(Hit::Line(0)));
        assert_eq!(l.hit((x, at(2, 0.0)), &stack, 0.0), Some(Hit::Line(2)));
        assert_eq!(l.hit((x, at(3, 20.0)), &stack, 20.0), Some(Hit::Line(3)));
    }

    #[test]
    fn the_preview_pane_and_the_header_are_not_the_list() {
        let l = layout();
        let stack = Stack::new([l.row_height; 20], 0.0);
        assert_eq!(l.hit((l.list_width + 50.0, l.body_top + 40.0), &stack, 0.0), None);
        assert_eq!(l.hit((50.0, l.body_top - 2.0), &stack, 0.0), None);
    }

    #[test]
    fn past_the_last_line_nothing_is_hit() {
        let l = layout();
        let stack = Stack::new([l.row_height; 2], 0.0);
        assert_eq!(l.hit((50.0, l.body_top + 3.0 * l.row_height), &stack, 0.0), None);
    }

    #[test]
    fn the_scrollbar_sits_in_the_lists_own_gutter() {
        let l = layout();
        let bar = l.scrollbar();
        assert!(bar.track_x + bar.width <= l.list_width, "the bar must not cross into the preview pane");
        assert!(bar.track_x >= l.list_width - l.list_inset - 1.0, "and must not sit over the rows' text");
    }

    #[test]
    fn more_room_fits_more_characters_and_never_zero() {
        assert!(Layout::chars_that_fit(13.0, 300.0) > Layout::chars_that_fit(13.0, 60.0));
        assert_eq!(Layout::chars_that_fit(13.0, 0.0), 1);
    }
}
