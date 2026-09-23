//! The popup's widget tree, built fresh each frame from a [`Model`].
//!
//! Renders two ways from one description, the same reason
//! `hyprforge_authui::screen::view` does: nothing here is Wayland-
//! specific, so the same tree could sit in an ordinary iced window for
//! testing if that were ever useful. Every colour comes from
//! [`hyprforge_look::Theme`] — CLAUDE.md is explicit that no app may
//! define its own colour constant.

use crate::geometry::RowLayout;
use crate::model::{HistoryState, Model};
use crate::thumbnail;
use hyprforge_clipboard::{Content, Entry};
use hyprforge_look::Theme;
use iced_runtime::core::text::Wrapping;
use iced_runtime::core::{Element, Length, Padding};
use iced_widget::{column, container, row, text, Space, Stack};

fn to_iced(c: hyprforge_look::Color) -> iced_runtime::core::Color {
    iced_runtime::core::Color::from_rgba8(c.r, c.g, c.b, c.a as f32 / 255.0)
}

pub fn view<'a, Message, Renderer>(
    model: &'a Model,
    theme: &'a Theme,
    thumbnails: &mut thumbnail::Cache,
    now: u64,
    popup_width: f64,
) -> Element<'a, Message, iced_widget::Theme, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = iced_runtime::core::Font>
        + iced_runtime::core::image::Renderer<Handle = iced_runtime::core::image::Handle>
        + 'a,
{
    let text_color = to_iced(theme.surfaces.text);
    let dim_color = to_iced(theme.surfaces.text_dim);

    // Both fixed to the theme's font size alone — see
    // `geometry::RowLayout`'s doc comment for why the hit-test in
    // `surface.rs` has to agree with these two numbers exactly, and why
    // that means they are never left to iced's own text metrics to
    // decide.
    let layout = RowLayout::for_font_size(theme.font_size);
    let header_text_height = layout.header_height - RowLayout::HEADER_GAP;

    // Copied out by value for the same reason the row's colours are: the
    // style closure outlives the borrow of `theme` this call holds.
    let root_background = theme.surfaces.root;
    // The accent, not `card_border` — the popup's own outline is meant
    // to read, not just separate it from the desktop behind it.
    let popup_border = theme.accent;
    let popup_radius = theme.corner_radius();

    // A failed pin/unpin takes over this same fixed-height slot rather
    // than adding a banner above the rows: `geometry::RowLayout` derives
    // where the first row starts from `header_height` alone, so growing
    // the header area by an extra element would move every row down on
    // screen without `RowLayout::row_at` knowing anything changed —
    // exactly the "hit-test disagrees with what was drawn" failure
    // CLAUDE.md warns about. Reusing the header's own slot means the
    // notice can appear and clear (see `Model::pin_notice`'s doc — any
    // other action clears it) without the row geometry ever moving.
    let header_inner: Element<'a, Message, iced_widget::Theme, Renderer> = if let Some(notice) =
        model.pin_notice()
    {
        text(notice.to_string()).size(theme.font_size).wrapping(Wrapping::None).color(to_iced(theme.error)).into()
    } else if model.filter_text().is_empty() {
        // Shows where a chosen entry will land, if `main.rs` was able to
        // work that out — confirmation that the popup picked up the
        // right window before anything is even chosen, per the owner's
        // "make it aware of where it's pasting" ask. Reuses the header's
        // own text slot rather than adding a widget, so `header_height`
        // (and therefore every row below it) never moves for this.
        let placeholder = match model.paste_target() {
            Some(target) => format!("Type to filter — pasting into {target}"),
            None => "Type to filter".to_string(),
        };
        text(placeholder).size(theme.font_size).wrapping(Wrapping::None).color(dim_color).into()
    } else {
        text(model.filter_text().to_string()).size(theme.font_size).wrapping(Wrapping::None).color(text_color).into()
    };
    // Full `font_size`, not the 0.85 this used to draw at: the field is
    // as tall as a row now, and text smaller than every row beneath it
    // read as a caption rather than as something you type into.
    //
    // A search field, not a plain line of text — the whole reason this
    // is a `container` around the text rather than the text itself:
    // `header_height` (and therefore where the first row starts) is
    // untouched, since the container's own height is fixed to exactly
    // `header_text_height`, the same number the text used to carry
    // directly. Only the horizontal padding and the visible field
    // (background plus an accent outline) are new.
    let field_background = theme.surfaces.card;
    let field_border = theme.accent;
    let field_radius = theme.corner_radius().min((header_text_height / 2.0) as f32);
    let header: Element<'a, Message, iced_widget::Theme, Renderer> = container(header_inner)
        .width(Length::Fill)
        .height(Length::Fixed(header_text_height as f32))
        .padding(Padding { top: 0.0, right: 8.0, bottom: 0.0, left: 8.0 })
        .align_y(iced_runtime::core::alignment::Vertical::Center)
        .style(move |_: &iced_widget::Theme| container::Style {
            background: Some(to_iced(field_background).into()),
            border: iced_runtime::core::Border { radius: field_radius.into(), width: 1.0, color: to_iced(field_border) },
            ..Default::default()
        })
        .into();

    let body: Element<'a, Message, iced_widget::Theme, Renderer> = match model.history() {
        // Never collapse "could not be read" into "there is nothing
        // configured" — CLAUDE.md's rule, and the reason this is a
        // distinct branch instead of falling into the empty-list one
        // below.
        HistoryState::Unreadable(reason) => message(
            &format!("Couldn't read the clipboard history: {reason}"),
            to_iced(theme.error),
            theme,
        ),
        HistoryState::Loaded(_) => {
            let filtered = model.filtered();
            if filtered.is_empty() {
                let msg = if model.filter_text().is_empty() {
                    "No clipboard history yet"
                } else {
                    "No matches"
                };
                message(msg, dim_color, theme)
            } else {
                let range = model.visible_range();
                let selected = model.selected_index();
                let window = &filtered[range.clone()];
                let rows = range
                    .zip(window.iter())
                    .map(|(index, entry)| {
                        entry_row(entry, index == selected, theme, &layout, thumbnails, now, popup_width)
                    })
                    .collect::<Vec<_>>();
                // Shifted up by `scroll_remainder` — the same number
                // `geometry::RowLayout::row_at` adds to a pointer's own
                // `y` before hit-testing (see that method's own doc), so
                // whatever this container draws at is exactly what a
                // click or a hover resolves against. Clipped to a fixed
                // `viewport_height` (rather than left to grow with
                // however many rows got built) is what makes a partially
                // visible row at the top or bottom look clipped instead
                // of spilling into the header or past the popup's own
                // edge — continuous scrolling's whole point.
                container(column(rows).spacing(RowLayout::ROW_SPACING as f32))
                    .padding(Padding { top: -(model.scroll_remainder() as f32), right: 0.0, bottom: 0.0, left: 0.0 })
                    .width(Length::Fill)
                    .height(Length::Fixed(model.viewport_height() as f32))
                    .clip(true)
                    .into()
            }
        }
    };

    let content: Element<'a, Message, iced_widget::Theme, Renderer> = container(
        column![header, Space::new().height(RowLayout::HEADER_GAP as f32), body]
            .spacing(0)
            .width(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(Padding::from(RowLayout::PADDING as f32))
    .style(move |_: &iced_widget::Theme| container::Style {
        background: Some(to_iced(root_background).into()),
        // The popup is a floating surface over the desktop, so it draws
        // its own edge the way a window would — Hyprland rounds and
        // borders real windows, and a layer-shell surface gets neither
        // for free. Without this it was a hard-edged rectangle whatever
        // the theme said.
        border: iced_runtime::core::Border {
            radius: popup_radius.into(),
            width: 1.0,
            color: to_iced(popup_border),
        },
        ..Default::default()
    })
    .into();

    // The scrollbar: drawn only when there is more content than the
    // viewport shows (`Scrollbar::is_needed`) — a scrollbar that cannot
    // scroll is noise. `layout.scrollbar` is the *one* place this crate
    // computes the track's geometry (see that method's own doc); reading
    // it here rather than recomputing the track's rectangle a second way
    // is what keeps a drawn thumb and a dragged thumb from disagreeing
    // about where it is.
    let bar = layout.scrollbar(popup_width, model.viewport_height());
    let content_height = layout.content_height(model.filtered().len());
    if !bar.is_needed(content_height) {
        return content;
    }
    let thumb_top = bar.thumb_top(content_height, model.scroll_offset());
    let thumb_height = bar.thumb_height(content_height);
    let thumb_color = to_iced(theme.accent);
    let scrollbar: Element<'a, Message, iced_widget::Theme, Renderer> = container(
        container(Space::new())
            .width(Length::Fixed(bar.width as f32))
            .height(Length::Fixed(thumb_height as f32))
            .style(move |_: &iced_widget::Theme| container::Style {
                background: Some(thumb_color.into()),
                border: iced_runtime::core::Border { radius: (bar.width as f32 / 2.0).into(), width: 0.0, color: thumb_color },
                ..Default::default()
            }),
    )
    .padding(Padding { top: thumb_top as f32, left: bar.track_x as f32, right: 0.0, bottom: 0.0 })
    .into();

    Stack::with_children([content, scrollbar]).width(Length::Fill).height(Length::Fill).into()
}


/// A short, glanceable age for a copy, both `now` and `copied_at` in
/// seconds since the Unix epoch — the same unit `Entry::copied_at` is
/// stored in.
///
/// The vocabulary is deliberately narrow: "now" under a minute old, then
/// whole minutes, hours, or days. A clipboard history is skimmed, not
/// read for precision — telling "the thing I just copied" apart from
/// "the thing from this morning" needs a category, not a stopwatch, and
/// a narrower vocabulary is also a narrower thing for a translator or a
/// future reader to get wrong.
///
/// `now.saturating_sub(copied_at)` is what keeps this from ever
/// underflowing. A `u64` subtraction that went negative would wrap to a
/// number near `u64::MAX` instead of panicking, which would silently
/// print an age of several hundred billion years — so the saturating
/// subtraction is not just tidiness, it is what turns two different real
/// failure modes into the same harmless "now": a `copied_at` that is
/// genuinely in the future (a clock that jumped backward between the
/// daemon recording the copy and this popup drawing it), and a `now`
/// that is itself behind `copied_at` for the same reason from the other
/// side. Both read as "just copied" rather than as an error, which is
/// the least surprising thing to show for a clock glitch neither side
/// caused.
fn relative_age(now: u64, copied_at: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let elapsed = now.saturating_sub(copied_at);
    if elapsed < MINUTE {
        "now".to_string()
    } else if elapsed < HOUR {
        format!("{}m", elapsed / MINUTE)
    } else if elapsed < DAY {
        format!("{}h", elapsed / HOUR)
    } else {
        format!("{}d", elapsed / DAY)
    }
}

/// How many characters of a preview fit in `available_width` pixels at
/// `font_size`, so a row's preview text ends before the pin toggle
/// rather than running into it.
///
/// This is necessarily an estimate: this crate builds a fresh
/// `UserInterface` from scratch every frame (see `surface.rs::draw`)
/// rather than keeping one running that could measure a string's actual
/// shaped width ahead of time, so there is no real text-metrics call to
/// make here. `0.6` is a plain average-glyph-width factor for a
/// proportional font — generous enough that ordinary text (which is
/// narrower on average, especially with `Content::preview`'s own
/// whitespace collapsing) reliably fits inside the estimate rather than
/// spilling past it, which is what matters here: the same "clip rather
/// than overflow" backstop `entry_row`'s own `.clip(true)` and
/// `Wrapping::None` already provide handles anything this estimate
/// slightly undershoots.
fn max_preview_chars(font_size: f32, available_width: f64) -> usize {
    let avg_char_width = (font_size as f64 * 0.6).max(1.0);
    ((available_width / avg_char_width).floor() as usize).max(1)
}

fn message<'a, Message, Renderer>(
    text_value: &str,
    color: iced_runtime::core::Color,
    theme: &Theme,
) -> Element<'a, Message, iced_widget::Theme, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = iced_runtime::core::Font> + 'a,
{
    container(text(text_value.to_string()).size(theme.font_size).color(color))
        .width(Length::Fill)
        .padding(Padding::from(12))
        .into()
}

fn entry_row<'a, Message, Renderer>(
    entry: &Entry,
    selected: bool,
    theme: &Theme,
    layout: &RowLayout,
    thumbnails: &mut thumbnail::Cache,
    now: u64,
    popup_width: f64,
) -> Element<'a, Message, iced_widget::Theme, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = iced_runtime::core::Font>
        + iced_runtime::core::image::Renderer<Handle = iced_runtime::core::image::Handle>
        + 'a,
{
    let text_color = if selected { to_iced(theme.surfaces.text) } else { to_iced(theme.surfaces.text_dim) };

    // `preview` already collapses whitespace and truncates on a
    // character boundary (`Content::preview`'s own doc comment) — that
    // handles a multi-line copy or an absurdly long single line in terms
    // of *characters*. `Wrapping::None` below is what stops the row
    // itself from growing: without it, iced wraps at word boundaries
    // regardless of how few characters got through, and a row full of
    // hyphen-free base64 or a URL would still lay out as several tall
    // lines instead of the one-line preview a clipboard history needs.
    //
    // The character cap itself used to be the flat `96` regardless of
    // how much room the row actually had, which is what let a long
    // preview run straight into the pin toggle: `Wrapping::None` stops
    // the row from *growing*, but does nothing to stop the text from
    // *drawing* past its own allotted space toward whatever is laid out
    // after it. `max_preview_chars` derives the cap from the same pixel
    // geometry `RowLayout::hit_test` uses for the pin's own rectangle, so
    // the preview is truncated to end where the pin toggle begins,
    // rather than merely being clipped at the row's far edge.
    let preview = entry.content.preview(max_preview_chars(theme.font_size, layout.preview_width(popup_width)));
    let label: Element<'a, Message, iced_widget::Theme, Renderer> = text(preview)
        .size(theme.font_size)
        .wrapping(Wrapping::None)
        .color(text_color)
        .into();

    let preview: Element<'a, Message, iced_widget::Theme, Renderer> = match &entry.content {
        // Only entries actually built into a row (see
        // `Model::visible_range`) ever reach `thumbnails.get`, so a
        // history of hundreds of images costs nothing until scrolled
        // to.
        Content::Image { .. } => match thumbnails.get(entry) {
            Some(handle) => row![
                iced_widget::image(handle)
                    .width(Length::Fixed(RowLayout::THUMBNAIL_SIZE as f32))
                    .height(Length::Fixed(RowLayout::THUMBNAIL_SIZE as f32)),
                Space::new().width(8),
                label,
            ]
            .into(),
            // Over the decode cap, or not a decodable image at all —
            // the row still shows the text preview rather than nothing.
            None => label,
        },
        Content::Text(_) => label,
    };
    // Fixed to `Length::Fill` rather than left to shrink to the text's
    // own width: everything after it (the pin toggle, the time label) is
    // positioned at a fixed offset from the row's *right* edge, and
    // `geometry::RowLayout::hit_test` computes that same offset
    // independently of whatever this widget tree actually measures out
    // to. If the preview were free to grow with an unusually long line,
    // it could push the pin toggle somewhere `hit_test` does not expect
    // it — precisely the "drawn" and "hit-tested" positions disagreeing
    // that caused the original bug this whole layout exists to avoid.
    let preview: Element<'a, Message, iced_widget::Theme, Renderer> = container(preview).width(Length::Fill).into();

    // The pin toggle: a small round indicator, filled with the accent
    // colour when pinned and merely outlined in it otherwise — readable
    // at a glance, and its own click target (see `surface::pointer_click`
    // and `geometry::RowLayout::hit_test`), not just the F2 keybind's
    // visual echo. Fixed-size for the same reason the thumbnail is: a
    // size read back from the renderer could disagree with what
    // `hit_test` was told to expect.
    let pinned = entry.pinned;
    let pin_color = if pinned { to_iced(theme.accent) } else { to_iced(theme.surfaces.text_dim) };
    let pin_size = layout.pin_size as f32;
    let pin_toggle: Element<'a, Message, iced_widget::Theme, Renderer> = container(Space::new())
        .width(Length::Fixed(pin_size))
        .height(Length::Fixed(pin_size))
        .style(move |_: &iced_widget::Theme| container::Style {
            background: if pinned { Some(pin_color.into()) } else { None },
            border: iced_runtime::core::Border { radius: (pin_size / 2.0).into(), width: 1.5, color: pin_color },
            ..Default::default()
        })
        .into();

    // The age sits at the row's right-hand end, in its own fixed-width
    // box for the same reason the pin toggle needs one: `hit_test`
    // computes this label's left edge to know where the pin toggle's
    // own box ends, so its width has to be a number both sides agree on
    // rather than whatever `relative_age`'s string happens to measure.
    let age_color = to_iced(theme.surfaces.text_dim);
    let age_label: Element<'a, Message, iced_widget::Theme, Renderer> = text(relative_age(now, entry.copied_at))
        .size(theme.font_size * 0.75)
        .wrapping(Wrapping::None)
        .color(age_color)
        .into();
    let age_box: Element<'a, Message, iced_widget::Theme, Renderer> = container(age_label)
        .width(Length::Fixed(layout.time_width as f32))
        .align_x(iced_runtime::core::alignment::Horizontal::Right)
        .into();

    // Left to right: the preview, the pin toggle, the time — exactly the
    // order `geometry::RowLayout::hit_test` assumes when it works
    // backward from the row's right edge.
    let content: Element<'a, Message, iced_widget::Theme, Renderer> = row![
        preview,
        Space::new().width(RowLayout::PIN_GAP as f32),
        pin_toggle,
        Space::new().width(RowLayout::PIN_GAP as f32),
        age_box,
    ]
    .align_y(iced_runtime::core::alignment::Vertical::Center)
    .into();

    // Copied out of `theme` as plain `Color`s rather than captured by
    // reference: the style closure below has to be `'static`-ish (bound
    // by `'a`, same as the `Element` it ends up in), and `theme` itself
    // only ever borrows for the length of one `view` call.
    let row_background = to_iced(if selected { theme.surfaces.row } else { theme.surfaces.card });
    // The accent, not `card_border`: this border is a selection
    // indicator rather than an edge — the popup's own outline is the one
    // that takes `card_border`. Pinning used to borrow this same border
    // at a thinner width, but now that the pin toggle itself shows
    // pinned-or-not (filled versus outlined, see `pin_toggle` above) that
    // reads as redundant clutter rather than a second signal — CLAUDE.md's
    // "must look different" is satisfied by the toggle alone now, so this
    // border is selection-only.
    let border_color = to_iced(theme.accent);
    let border_width = if selected { 1.5 } else { 0.0 };
    // Never more than half the row's height. A radius larger than that
    // is a degenerate shape, and degenerate shapes are the reason
    // `hyprforge-authui` bounds this field at all: `tiny_skia`'s path
    // builders return `None` for them and iced unwraps that.
    let row_radius = theme.corner_radius().min(layout.row_height as f32 / 2.0);

    container(content)
        .width(Length::Fill)
        // Fixed to `layout.row_height`, the exact number
        // `geometry::RowLayout::row_at` hit-tests against — not left to
        // shrink around whatever the label or thumbnail measure out to.
        // A row that grew or shrank with its content would still overflow
        // visually (an unusually tall glyph, a thumbnail bigger than
        // `THUMBNAIL_SIZE` somehow) *and* would silently invalidate the
        // hit-test's arithmetic at the same time.
        .height(Length::Fixed(layout.row_height as f32))
        .align_y(iced_runtime::core::alignment::Vertical::Center)
        .padding(Padding::from(RowLayout::ROW_PADDING as f32))
        // Belt and braces alongside `Wrapping::None`: a preview that is
        // still wider than the row (a long run of characters with no
        // word breaks at all, wider per-character than average) is
        // clipped at the row's own edge instead of overdrawing into the
        // padding or the row below it.
        .clip(true)
        .style(move |_: &iced_widget::Theme| container::Style {
            background: Some(row_background.into()),
            border: iced_runtime::core::Border {
                radius: row_radius.into(),
                width: border_width,
                color: border_color,
            },
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this fixes: the radius was the literal `4.0` and the
    /// popup had no border at all, so a theme saying `rounding = 12`
    /// drew square rows inside a hard-edged rectangle. Reading the field
    /// is the whole point, so assert it is actually read.
    #[test]
    fn the_corner_radius_comes_from_the_theme_rather_than_a_constant() {
        let theme = Theme { rounding: 12, ..Theme::default() };
        assert_eq!(theme.corner_radius(), 12.0);
        let square = Theme { rounding: 0, ..Theme::default() };
        assert_eq!(square.corner_radius(), 0.0, "a theme may legitimately ask for square corners");
    }

    /// `hyprforge-authui` bounds the same field for the lock screen, and
    /// the reason is not cosmetic: a degenerate radius reaches a
    /// `tiny_skia` path builder that answers `None`, and iced unwraps
    /// it. A popup that panics on a hostile theme file is a popup that
    /// panics on a typo.
    #[test]
    fn an_absurd_rounding_is_bounded_rather_than_handed_to_the_renderer() {
        let theme = Theme { rounding: u32::MAX, ..Theme::default() };
        let radius = theme.corner_radius();
        assert!(radius.is_finite());
        assert!(radius <= 64.0, "got {radius}");
    }

    // --- `max_preview_chars`: the estimate that keeps a row's preview
    // text from running into the pin toggle.

    #[test]
    fn a_wider_available_width_allows_more_characters() {
        let narrow = max_preview_chars(15.0, 60.0);
        let wide = max_preview_chars(15.0, 600.0);
        assert!(wide > narrow, "more room must allow more characters, not fewer");
    }

    #[test]
    fn zero_or_negative_width_still_allows_at_least_one_character() {
        // Never zero: a preview of zero characters would show an empty
        // row rather than the clipped-but-present text a degenerate popup
        // size should still manage.
        assert_eq!(max_preview_chars(15.0, 0.0), 1);
    }

    #[test]
    fn a_bigger_font_needs_more_width_per_character() {
        let small_font = max_preview_chars(12.0, 300.0);
        let large_font = max_preview_chars(40.0, 300.0);
        assert!(large_font < small_font, "a bigger font must fit fewer characters in the same width");
    }

    // --- `relative_age`: the vocabulary and the arithmetic behind it.

    #[test]
    fn anything_copied_less_than_a_minute_ago_reads_as_now() {
        assert_eq!(relative_age(1_000, 1_000), "now", "copied this instant");
        assert_eq!(relative_age(1_000, 950), "now", "copied 50 seconds ago");
    }

    #[test]
    fn minutes_hours_and_days_use_the_narrow_vocabulary() {
        assert_eq!(relative_age(1_000 + 5 * 60, 1_000), "5m");
        assert_eq!(relative_age(1_000 + 59 * 60, 1_000), "59m");
        assert_eq!(relative_age(1_000 + 2 * 3600, 1_000), "2h");
        assert_eq!(relative_age(1_000 + 23 * 3600, 1_000), "23h");
        assert_eq!(relative_age(1_000 + 3 * 86_400, 1_000), "3d");
    }

    /// The boundary between two units lands on the unit that just
    /// started, not the one that just ended — exactly 60 seconds is
    /// "1m", not "60s" and not "now".
    #[test]
    fn a_boundary_belongs_to_the_unit_it_just_entered() {
        assert_eq!(relative_age(1_060, 1_000), "1m");
        assert_eq!(relative_age(1_000 + 3600, 1_000), "1h");
        assert_eq!(relative_age(1_000 + 86_400, 1_000), "1d");
    }

    /// A `copied_at` in the future — a clock that jumped backward
    /// between the daemon recording the copy and this popup drawing it
    /// — must read as "now" rather than underflow the subtraction.
    #[test]
    fn a_copied_at_in_the_future_reads_as_now_rather_than_underflowing() {
        assert_eq!(relative_age(1_000, 5_000), "now");
    }

    /// The same clock-jump hazard from the other side: `now` itself
    /// behind where it should be. Still must not underflow or panic.
    #[test]
    fn now_is_saturating_even_at_the_extremes() {
        assert_eq!(relative_age(0, u64::MAX), "now");
        let age = relative_age(u64::MAX, 0);
        assert!(age.ends_with('d'), "got {age:?}");
    }
}
