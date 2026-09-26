//! The popup's widget tree, built fresh each frame from a [`Model`].
//!
//! Every region is sized from [`Layout`] — the same values
//! `geometry::Layout::hit` measures with — and the list's lines from the
//! model's [`hyprforge_popup::Stack`]. Nothing here picks a height of its
//! own: see `geometry.rs`'s module doc for why that is the whole
//! contract. Every colour comes from the theme through
//! [`hyprforge_popup::kit::Look`]; CLAUDE.md is explicit that no app may
//! define its own colour constant.
//!
//! Nothing here routes through iced's own click handling (every
//! `Element` is `Infallible`-messaged): `hyprforge-popup` draws with the
//! cursor unavailable, and `surface.rs` resolves every click from raw
//! pointer coordinates against `geometry.rs`.

use crate::geometry::Layout;
use crate::kind::{self, Filter, Kind};
use crate::model::{HistoryState, Line, Model};
use crate::thumbnail;
use hyprforge_clipboard::{Content, Entry};
use hyprforge_look::Theme;
use hyprforge_popup::kit::{self, Look};
use iced_runtime::core::alignment::{Horizontal, Vertical};
use iced_runtime::core::text::Wrapping;
use iced_runtime::core::{Border, Color, ContentFit, Element, Font, Length, Padding};
use iced_widget::{column, container, row, text, Column, Row, Space, Stack};

type El<'a, Message, Renderer> = Element<'a, Message, iced_widget::Theme, Renderer>;

pub fn view<'a, Message, Renderer>(
    model: &'a Model,
    theme: &'a Theme,
    thumbnails: &mut thumbnail::Cache,
    now: u64,
    home: Option<&str>,
) -> El<'a, Message, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = Font>
        + iced_runtime::core::image::Renderer<Handle = iced_runtime::core::image::Handle>
        + 'a,
{
    let layout = Layout::for_font_size(theme.font_size);
    let look = Look::new(theme);

    let labels: Vec<&str> = Filter::ALL.iter().map(|f| f.label()).collect();
    let header = container(column![
        kit::search_field(model.filter_text(), "Search clipboard", layout.search.height, &look),
        Space::new().height((layout.tabs.y - layout.search.bottom()) as f32),
        kit::tabs(&labels, model.filter().index(), layout.tabs.height, &look),
    ])
    .width(Length::Fill)
    .height(Length::Fixed((layout.body_top - 1.0) as f32))
    .padding(Padding { top: layout.padding as f32, right: layout.padding as f32, bottom: 0.0, left: layout.padding as f32 });

    let body: El<'a, Message, Renderer> = match model.history() {
        // Never collapse "could not be read" into "there is nothing
        // configured" — CLAUDE.md's rule, and the reason this is its own
        // branch rather than the empty-list one.
        HistoryState::Unreadable(reason) => {
            message(&format!("Couldn't read the clipboard history: {reason}"), look.error, &look, layout.body_height)
        }
        HistoryState::Loaded(all) => {
            let selected = model.selected_entry();
            let list = list_pane(model, &layout, &look, thumbnails, now, home, all.is_empty());
            let pane = preview_pane(model, selected.as_ref(), &layout, &look, thumbnails, now, home);
            row![list, kit::divider(false, &look), pane].height(Length::Fixed(layout.body_height as f32)).into()
        }
    };

    let content = column![
        header,
        kit::divider(true, &look),
        container(body).height(Length::Fixed(layout.body_height as f32)),
        kit::divider(true, &look),
        footer(model, &layout, &look),
    ];

    let mut layers: Vec<El<'a, Message, Renderer>> = vec![kit::frame(content, &look)];
    if matches!(model.history(), HistoryState::Loaded(_)) {
        if let Some(bar) = kit::scrollbar_layer(&layout.scrollbar(), model.stack().content_height(), model.scroll_offset(), &look) {
            layers.push(bar);
        }
    }
    Stack::with_children(layers).width(Length::Fill).height(Length::Fill).into()
}

/// The left pane: the visible lines of the list, drawn at exactly the
/// positions the model's stack gives them.
fn list_pane<'a, Message, Renderer>(
    model: &'a Model,
    layout: &Layout,
    look: &Look,
    thumbnails: &mut thumbnail::Cache,
    now: u64,
    home: Option<&str>,
    history_empty: bool,
) -> El<'a, Message, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = Font>
        + iced_runtime::core::image::Renderer<Handle = iced_runtime::core::image::Handle>
        + 'a,
{
    let filtered = model.filtered();
    let inner: El<'a, Message, Renderer> = if filtered.is_empty() {
        let words = if history_empty {
            "No clipboard history yet".to_string()
        } else if !model.filter_text().is_empty() {
            "No matches".to_string()
        } else {
            format!("Nothing under {}", model.filter().label())
        };
        message(&words, look.dim, look, layout.viewport_height())
    } else {
        let lines = model.lines();
        let stack = model.stack();
        let offset = model.scroll_offset();
        let visible = stack.visible(offset, layout.viewport_height());
        let selected = model.selected_index();
        let text_width = layout.list_width - layout.list_inset * 2.0;
        let drawn: Vec<El<'a, Message, Renderer>> = visible
            .clone()
            .map(|i| match lines[i] {
                Line::Header(section) => kit::section_label(section.title(), None, stack.height(i), false, look),
                Line::Entry(index) => entry_row(filtered[index], index == selected, layout, look, thumbnails, now, home, text_width),
            })
            .collect();
        // Shifted up by however far the first drawn line sits above the
        // viewport's top — the same offset `Layout::hit` adds back — and
        // clipped to the viewport, so a half-scrolled line reads as
        // half-scrolled rather than spilling into the header.
        let shift = offset - stack.top(visible.start);
        container(Column::with_children(drawn).spacing(stack.spacing() as f32))
            .padding(Padding { top: -(shift as f32), right: 0.0, bottom: 0.0, left: 0.0 })
            .width(Length::Fill)
            .height(Length::Fixed(layout.viewport_height() as f32))
            .clip(true)
            .into()
    };
    container(inner)
        .width(Length::Fixed(layout.list_width as f32))
        .height(Length::Fixed(layout.body_height as f32))
        .padding(Padding { top: 0.0, right: layout.list_inset as f32, bottom: layout.list_inset as f32, left: layout.list_inset as f32 })
        .into()
}

/// One entry: its kind's badge, a swatch or thumbnail where the kind has
/// one, a one-line preview, and "pinned" or its age at the right.
#[allow(clippy::too_many_arguments)]
fn entry_row<'a, Message, Renderer>(
    entry: &Entry,
    selected: bool,
    layout: &Layout,
    look: &Look,
    thumbnails: &mut thumbnail::Cache,
    now: u64,
    home: Option<&str>,
    width: f64,
) -> El<'a, Message, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = Font>
        + iced_runtime::core::image::Renderer<Handle = iced_runtime::core::image::Handle>
        + 'a,
{
    let look = *look;
    let kind = Kind::of(&entry.content);
    let mut parts: Vec<El<'a, Message, Renderer>> = vec![badge(kind, &look)];
    let mut used = 8.0 * 2.0 + BADGE_WIDTH + 10.0;

    match kind {
        Kind::Colour(colour) => {
            parts.push(swatch(kit::to_iced(colour), 14.0, 4.0));
            used += 14.0 + 10.0;
        }
        // Only rows actually built reach `thumbnails.get`, so a history
        // of hundreds of images costs nothing until scrolled to.
        Kind::Image => {
            if let Some(handle) = thumbnails.get(entry) {
                parts.push(iced_widget::image(handle).width(Length::Fixed(28.0)).height(Length::Fixed(18.0)).content_fit(ContentFit::Cover).into());
                used += 28.0 + 10.0;
            }
        }
        _ => {}
    }

    let right = if entry.pinned { "pinned".to_string() } else { relative_age(now, entry.copied_at) };
    let right_width = (look.small() as f64 * 0.62 * 6.5).ceil();
    used += right_width + 10.0;

    let (label, font, size) = match (&entry.content, kind) {
        (Content::Image { .. }, _) => (image_label(entry, thumbnails), Font::DEFAULT, look.font_size),
        (Content::Text(t), Kind::Text) => (t.clone(), look.mono, look.font_size * 0.96),
        (Content::Text(t), _) => (kind::display(kind, t, home), Font::DEFAULT, look.font_size),
    };
    // `preview` collapses whitespace and truncates on a character
    // boundary; `Wrapping::None` and the row's clip are what stop a
    // long line from growing the row or drawing past it.
    let chars = Layout::chars_that_fit(size, (width - used).max(0.0));
    let label = Content::Text(label).preview(chars);
    parts.push(
        container(text(label).size(size).font(font).color(look.text).wrapping(Wrapping::None))
            .width(Length::Fill)
            .clip(true)
            .into(),
    );
    parts.push(
        container(text(right).size(look.small()).font(look.mono).color(look.dim).wrapping(Wrapping::None))
            .width(Length::Fixed(right_width as f32))
            .align_x(Horizontal::Right)
            .into(),
    );

    let radius = look.radius_for(layout.row_height, 8.0);
    container(Row::with_children(parts).spacing(10).align_y(Vertical::Center))
        .width(Length::Fill)
        .height(Length::Fixed(layout.row_height as f32))
        .padding(Padding { top: 0.0, right: 8.0, bottom: 0.0, left: 8.0 })
        .align_y(Vertical::Center)
        .clip(true)
        .style(move |_: &iced_widget::Theme| container::Style {
            background: selected.then_some(look.selected.into()),
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

const BADGE_WIDTH: f64 = 34.0;

/// The kind's badge. Its letters take the kind's colour — the design's
/// one use of the state colours here, each read from the theme: a link
/// in the theme's "somewhere else" cyan, an image in its green, a file
/// in its orange, a colour in the accent it most likely came from.
fn badge<'a, Message: 'a, Renderer>(kind: Kind, look: &Look) -> El<'a, Message, Renderer>
where
    Renderer: iced_runtime::core::text::Renderer<Font = Font> + 'a,
{
    let colour = match kind {
        Kind::Text => look.text,
        Kind::Colour(_) => look.accent,
        Kind::Link => look.info,
        Kind::File => look.warning,
        Kind::Image => look.success,
    };
    let fill = Color { a: 0.45, ..look.chip };
    container(text(kind.badge()).size(look.font_size * 0.73).font(Font { weight: iced_runtime::core::font::Weight::Semibold, ..look.mono }).color(colour).wrapping(Wrapping::None))
        .width(Length::Fixed(BADGE_WIDTH as f32))
        .padding(Padding { top: 3.0, right: 0.0, bottom: 3.0, left: 0.0 })
        .align_x(Horizontal::Center)
        .style(move |_: &iced_widget::Theme| container::Style {
            background: Some(fill.into()),
            border: Border { radius: 5.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

fn swatch<'a, Message: 'a, Renderer>(colour: Color, side: f32, radius: f32) -> El<'a, Message, Renderer>
where
    Renderer: iced_runtime::core::Renderer + 'a,
{
    container(Space::new())
        .width(Length::Fixed(side))
        .height(Length::Fixed(side))
        .style(move |_: &iced_widget::Theme| container::Style {
            background: Some(colour.into()),
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

/// "Image 1440 × 900", or "Image · image/png · 212 KB" when the header
/// could not be read.
fn image_label(entry: &Entry, thumbnails: &mut thumbnail::Cache) -> String {
    match thumbnails.size(entry) {
        Some((w, h)) => format!("Image {w} × {h}"),
        None => entry.content.preview(usize::MAX),
    }
}

/// The right pane: the selected entry, larger; what it is; and the three
/// buttons, each at exactly the rectangle `Layout` gives it.
fn preview_pane<'a, Message, Renderer>(
    model: &Model,
    entry: Option<&Entry>,
    layout: &Layout,
    look: &Look,
    thumbnails: &mut thumbnail::Cache,
    now: u64,
    home: Option<&str>,
) -> El<'a, Message, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = Font>
        + iced_runtime::core::image::Renderer<Handle = iced_runtime::core::image::Handle>
        + 'a,
{
    let pane_background = Color { a: 0.3, ..look.inset };
    let content: El<'a, Message, Renderer> = match entry {
        None => container(text("Nothing selected").size(look.small()).color(look.dim)).center(Length::Fill).into(),
        Some(entry) => {
            let kind = Kind::of(&entry.content);
            let well = Color { a: 0.05, ..look.text };
            let radius = look.radius_for(layout.preview.height, 8.0);
            let preview = container(preview_content(entry, kind, layout, look, thumbnails, home))
                .width(Length::Fill)
                .height(Length::Fixed(layout.preview.height as f32))
                .padding(12)
                .clip(true)
                .style(move |_: &iced_widget::Theme| container::Style {
                    background: Some(well.into()),
                    border: Border { radius: radius.into(), ..Default::default() },
                    ..Default::default()
                });

            let meta = meta_block(entry, kind, layout, look, thumbnails, now);
            let paste_label = match model.paste_target() {
                Some(target) => format!("Paste into {target}"),
                None => "Paste".to_string(),
            };
            let pin_label = if entry.pinned { "Unpin" } else { "Pin" };
            let gap = (layout.delete_button.x - layout.pin_button.right()) as f32;
            column![
                preview,
                Space::new().height(layout.pane_gap as f32),
                meta,
                Space::new().height(Length::Fill),
                button(&paste_label, "Enter", true, layout.paste_button.height, look),
                Space::new().height((layout.pin_button.y - layout.paste_button.bottom()) as f32),
                row![
                    button(pin_label, "Ctrl P", false, layout.pin_button.height, look),
                    Space::new().width(gap),
                    button("Delete", "Del", false, layout.delete_button.height, look),
                ],
            ]
            .into()
        }
    };
    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(layout.pane_padding as f32)
        .style(move |_: &iced_widget::Theme| container::Style {
            background: Some(pane_background.into()),
            ..Default::default()
        })
        .into()
}

/// What goes in the preview well, by kind. Text is bounded before it is
/// laid out: the well shows a few lines, and handing a megabyte of copied
/// log to the text shaper to fill a 110-pixel box would be the popup
/// paying for what nobody can see.
fn preview_content<'a, Message, Renderer>(
    entry: &Entry,
    kind: Kind,
    layout: &Layout,
    look: &Look,
    thumbnails: &mut thumbnail::Cache,
    home: Option<&str>,
) -> El<'a, Message, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = Font>
        + iced_runtime::core::image::Renderer<Handle = iced_runtime::core::image::Handle>
        + 'a,
{
    match (&entry.content, kind) {
        (Content::Image { .. }, _) => match thumbnails.get(entry) {
            Some(handle) => iced_widget::image(handle).width(Length::Fill).height(Length::Fill).content_fit(ContentFit::Contain).into(),
            None => {
                let why = if thumbnails.over_cap(entry) { "Too large to preview" } else { "Not an image this popup can read" };
                container(text(why).size(look.small()).color(look.dim)).center(Length::Fill).into()
            }
        },
        (Content::Text(value), Kind::Colour(colour)) => row![
            swatch(kit::to_iced(colour), (layout.preview.height - 24.0) as f32, 6.0),
            column![
                text(value.trim().to_string()).size(look.font_size * 1.1).font(look.mono).color(look.text),
                text(channels(colour)).size(look.small()).font(look.mono).color(look.dim).wrapping(Wrapping::None),
            ]
            .spacing(6),
        ]
        .spacing(14)
        .align_y(Vertical::Center)
        .into(),
        (Content::Text(value), Kind::Text) => {
            text(bounded(value, 8, 400)).size(look.font_size * 0.92).font(look.mono).color(look.text).wrapping(Wrapping::WordOrGlyph).into()
        }
        (Content::Text(value), Kind::Link) => {
            text(bounded(value, 4, 400)).size(look.font_size).color(look.info).wrapping(Wrapping::Glyph).into()
        }
        (Content::Text(value), _) => {
            let shown: Vec<String> = value.lines().filter(|l| !l.trim().is_empty()).take(5).map(|l| kind::display(kind, l, home)).collect();
            text(shown.join("\n")).size(look.font_size).color(look.text).wrapping(Wrapping::Glyph).into()
        }
    }
}

/// A colour's channels as decimals — `189 147 249` — with its alpha only
/// when it is not opaque, since an alpha of 255 on every hex colour anyone
/// copies is noise.
fn channels(colour: hyprforge_look::Color) -> String {
    let rgb = format!("{} {} {}", colour.r, colour.g, colour.b);
    if colour.a == 0xff {
        rgb
    } else {
        format!("{rgb} · {}%", (colour.a as u32 * 100 + 127) / 255)
    }
}

/// At most `max_lines` lines and `max_chars` characters of `value`,
/// counted in characters so nothing is cut mid-character, with an
/// ellipsis when anything was left out.
fn bounded(value: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = value.lines().take(max_lines).collect();
    let mut out = lines.join("\n");
    let truncated_lines = value.lines().nth(max_lines).is_some();
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect();
        out.push('\u{2026}');
    } else if truncated_lines {
        out.push('\u{2026}');
    }
    out
}

/// Type, Copied, Size — the design's "From" line is left out because the
/// history does not record which application a copy came from, and a
/// line that is always blank is noise.
fn meta_block<'a, Message, Renderer>(
    entry: &Entry,
    kind: Kind,
    layout: &Layout,
    look: &Look,
    thumbnails: &mut thumbnail::Cache,
    now: u64,
) -> El<'a, Message, Renderer>
where
    Message: 'a,
    Renderer: iced_runtime::core::text::Renderer<Font = Font> + 'a,
{
    let kind_text = match &entry.content {
        Content::Image { mime, .. } => mime.to_string(),
        Content::Text(_) => kind.describe().to_string(),
    };
    let size_text = match &entry.content {
        Content::Image { bytes, .. } => match thumbnails.size(entry) {
            Some((w, h)) => format!("{w} × {h} · {}", human_bytes(bytes.len())),
            None => human_bytes(bytes.len()),
        },
        Content::Text(t) => {
            let chars = t.chars().count();
            let lines = t.lines().count();
            let unit = if chars == 1 { "char" } else { "chars" };
            if lines > 1 {
                format!("{} {unit} · {lines} lines", thousands(chars))
            } else {
                format!("{} {unit}", thousands(chars))
            }
        }
    };
    let rows = [("Type", kind_text), ("Copied", copied(now, entry.copied_at)), ("Size", size_text)];
    let line = layout.meta_line as f32;
    Column::with_children(rows.into_iter().map(|(label, value)| {
        row![
            container(text(label).size(look.small()).color(look.dim)).width(Length::Fixed(62.0)),
            text(value).size(look.small()).font(look.mono).color(look.text).wrapping(Wrapping::None),
        ]
        .height(Length::Fixed(line))
        .spacing(10)
        .align_y(Vertical::Center)
        .into()
    }))
    .spacing(layout.meta_gap as f32)
    .into()
}

/// A pane button: its label at the left and its key at the right. The
/// primary one is the accent with the popup's background as its text.
fn button<'a, Message: 'a, Renderer>(label: &str, key: &str, primary: bool, height: f64, look: &Look) -> El<'a, Message, Renderer>
where
    Renderer: iced_runtime::core::text::Renderer<Font = Font> + 'a,
{
    let look = *look;
    let (fill, ink) = if primary { (look.accent, look.on_accent) } else { (Color { a: 0.45, ..look.chip }, look.text) };
    let key_ink = Color { a: 0.75, ..ink };
    let radius = look.radius_for(height, 7.0);
    container(
        row![
            text(label.to_string()).size(look.font_size).font(if primary { kit::strong() } else { Font::DEFAULT }).color(ink).wrapping(Wrapping::None),
            Space::new().width(Length::Fill),
            text(key.to_string()).size(look.label()).font(look.mono).color(key_ink).wrapping(Wrapping::None),
        ]
        .align_y(Vertical::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(height as f32))
    .padding(Padding { top: 0.0, right: 10.0, bottom: 0.0, left: 10.0 })
    .align_y(Vertical::Center)
    .clip(true)
    .style(move |_: &iced_widget::Theme| container::Style {
        background: Some(fill.into()),
        border: Border { radius: radius.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

/// The strip along the bottom: the keys, or a notice about the last
/// thing that did not happen, and "Clear history…" at the right.
fn footer<'a, Message: 'a, Renderer>(model: &Model, layout: &Layout, look: &Look) -> El<'a, Message, Renderer>
where
    Renderer: iced_runtime::core::text::Renderer<Font = Font> + 'a,
{
    let look_copy = *look;
    let left: El<'a, Message, Renderer> = match model.notice() {
        Some(notice) => text(notice.to_string()).size(look.small()).color(look.error).wrapping(Wrapping::None).into(),
        None => {
            let pin = match model.selected_entry() {
                Some(e) if e.pinned => "unpin",
                _ => "pin",
            };
            row![kit::key_hint("Ctrl P", pin, look), kit::key_hint("Del", "delete", look), kit::key_hint("Tab", "filter", look)]
                .spacing(12)
                .align_y(Vertical::Center)
                .into()
        }
    };
    let clear_words = if model.clear_armed() {
        let n = model.clearable().len();
        format!("Click again to clear {n}")
    } else {
        "Clear history\u{2026}".to_string()
    };
    let clear = container(text(clear_words).size(look.font_size * 0.92).color(look.error).wrapping(Wrapping::None))
        .width(Length::Fixed(layout.clear_button.width as f32))
        .height(Length::Fill)
        .align_x(Horizontal::Right)
        .align_y(Vertical::Center);
    container(row![container(left).width(Length::Fill).clip(true), clear].align_y(Vertical::Center))
        .width(Length::Fill)
        .height(Length::Fixed(layout.footer_height as f32))
        .padding(Padding { top: 0.0, right: (layout.width - layout.clear_button.right()) as f32, bottom: 0.0, left: 12.0 })
        .align_y(Vertical::Center)
        .style(move |_: &iced_widget::Theme| container::Style {
            background: Some(look_copy.footer.into()),
            ..Default::default()
        })
        .into()
}

fn message<'a, Message: 'a, Renderer>(words: &str, colour: Color, look: &Look, height: f64) -> El<'a, Message, Renderer>
where
    Renderer: iced_runtime::core::text::Renderer<Font = Font> + 'a,
{
    container(text(words.to_string()).size(look.font_size).color(colour))
        .width(Length::Fill)
        .height(Length::Fixed(height as f32))
        .padding(12)
        .into()
}

/// A short, glanceable age for a row: "now", then whole minutes, hours
/// or days. A clipboard history is skimmed, not read for precision.
///
/// `saturating_sub` is what keeps a clock that jumped backwards between
/// the daemon recording a copy and this popup drawing it from wrapping to
/// an age of several hundred billion years: both sides of that glitch
/// read as "now", the least surprising thing to show.
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

/// The pane's longer form: the local wall-clock time it was copied, and
/// how long ago in words — "14:19 · 1 min ago". A copy from another day
/// gets its date as well, since "09:12" alone would read as this morning.
fn copied(now: u64, copied_at: u64) -> String {
    let when = chrono::DateTime::from_timestamp(copied_at as i64, 0).map(|t| t.with_timezone(&chrono::Local));
    let today = chrono::DateTime::from_timestamp(now as i64, 0).map(|t| t.with_timezone(&chrono::Local).date_naive());
    let clock = match (when, today) {
        (Some(when), Some(today)) if when.date_naive() == today => when.format("%H:%M").to_string(),
        (Some(when), _) => when.format("%-d %b %H:%M").to_string(),
        (None, _) => return long_age(now, copied_at),
    };
    format!("{clock} · {}", long_age(now, copied_at))
}

fn long_age(now: u64, copied_at: u64) -> String {
    let elapsed = now.saturating_sub(copied_at);
    let (n, unit) = match elapsed {
        e if e < 60 => return "just now".to_string(),
        e if e < 3600 => (e / 60, "min"),
        e if e < 86_400 => (e / 3600, "h"),
        e => (e / 86_400, if e / 86_400 == 1 { "day" } else { "days" }),
    };
    format!("{n} {unit} ago")
}

/// `1284` as `1,284`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn human_bytes(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    match bytes {
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.0} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anything_copied_less_than_a_minute_ago_reads_as_now() {
        assert_eq!(relative_age(1_000, 1_000), "now");
        assert_eq!(relative_age(1_000, 950), "now");
    }

    #[test]
    fn minutes_hours_and_days_use_the_narrow_vocabulary() {
        assert_eq!(relative_age(1_000 + 5 * 60, 1_000), "5m");
        assert_eq!(relative_age(1_000 + 2 * 3600, 1_000), "2h");
        assert_eq!(relative_age(1_000 + 3 * 86_400, 1_000), "3d");
    }

    #[test]
    fn a_boundary_belongs_to_the_unit_it_just_entered() {
        assert_eq!(relative_age(1_060, 1_000), "1m");
        assert_eq!(relative_age(1_000 + 3600, 1_000), "1h");
        assert_eq!(relative_age(1_000 + 86_400, 1_000), "1d");
    }

    #[test]
    fn a_copied_at_in_the_future_reads_as_now_rather_than_underflowing() {
        assert_eq!(relative_age(1_000, 5_000), "now");
        assert_eq!(relative_age(0, u64::MAX), "now");
        assert_eq!(long_age(1_000, 5_000), "just now");
    }

    #[test]
    fn the_long_age_says_it_in_words() {
        assert_eq!(long_age(1_000 + 60, 1_000), "1 min ago");
        assert_eq!(long_age(1_000 + 7200, 1_000), "2 h ago");
        assert_eq!(long_age(1_000 + 86_400, 1_000), "1 day ago");
        assert_eq!(long_age(1_000 + 3 * 86_400, 1_000), "3 days ago");
    }

    /// A timestamp chrono cannot represent must still produce words, not
    /// a panic — the history file is plain TOML a person can edit.
    #[test]
    fn an_absurd_timestamp_still_describes_itself() {
        assert!(!copied(u64::MAX, u64::MAX).is_empty());
    }

    #[test]
    fn an_opaque_colour_shows_three_channels_and_a_translucent_one_its_alpha() {
        assert_eq!(channels(hyprforge_look::Color::rgba(189, 147, 249, 255)), "189 147 249");
        assert_eq!(channels(hyprforge_look::Color::rgba(0, 0, 0, 128)), "0 0 0 · 50%");
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_284), "1,284");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    /// The well is a few lines tall: a huge copy is cut before the text
    /// shaper ever sees it, on a character boundary.
    #[test]
    fn a_huge_copy_is_bounded_before_it_is_laid_out() {
        let huge = "🎉".repeat(10_000);
        let shown = bounded(&huge, 8, 400);
        assert_eq!(shown.chars().count(), 401, "400 characters and an ellipsis");
        let many_lines = "line\n".repeat(50);
        assert!(bounded(&many_lines, 8, 400).ends_with('\u{2026}'));
        assert_eq!(bounded("short", 8, 400), "short");
    }

    #[test]
    fn the_corner_radius_comes_from_the_theme_rather_than_a_constant() {
        let square = Theme { rounding: 0, ..Theme::default() };
        assert_eq!(Look::new(&square).radius_for(34.0, 8.0), 0.0, "a square theme gets square rows");
        let round = Theme { rounding: 12, ..Theme::default() };
        assert_eq!(Look::new(&round).radius_for(34.0, 8.0), 8.0);
    }
}
