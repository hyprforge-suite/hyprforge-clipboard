//! What kind of thing an entry is — plain text, a colour, a link, a file,
//! an image — worked out from the content alone.
//!
//! The history records text or image bytes and nothing else: no MIME
//! list for text, no source application. So every badge the popup draws
//! (`TXT`, `HEX`, `URL`, `FILE`, `IMG`) and every filter tab it offers
//! is a reading of the content, and this module is the one place that
//! reading happens — the badge and the tab that shows it can never
//! disagree about what an entry is, because both ask [`Kind::of`].
//!
//! The rules are deliberately conservative. A link is one token with a
//! scheme (or a bare `www.`), never "anything with a dot in it"; a file
//! is a path or a `file://` URI on its own, never prose that happens to
//! mention one. Guessing wrong in the generous direction puts a sentence
//! under Links; guessing wrong in the stingy direction only leaves a
//! URL under Text, where it is still found by searching.

use hyprforge_clipboard::Content;
use hyprforge_look::Color;

/// See the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// A colour value on its own — `#bd93f9`, or Hyprland's own
    /// `rgba(bd93f9ff)` — carrying the colour so the row can show a
    /// swatch of it.
    Colour(Color),
    Link,
    File,
    Image,
}

impl Kind {
    pub fn of(content: &Content) -> Kind {
        let text = match content {
            Content::Image { .. } => return Kind::Image,
            Content::Text(text) => text.trim(),
        };
        if let Some(colour) = parse_colour(text) {
            return Kind::Colour(colour);
        }
        if is_link(text) {
            return Kind::Link;
        }
        if is_file(text) {
            return Kind::File;
        }
        Kind::Text
    }

    /// The three- or four-letter badge at the start of a row.
    pub fn badge(self) -> &'static str {
        match self {
            Kind::Text => "TXT",
            Kind::Colour(_) => "HEX",
            Kind::Link => "URL",
            Kind::File => "FILE",
            Kind::Image => "IMG",
        }
    }

    /// What the preview pane's "Type" line says. Not a MIME type for
    /// text: the history does not record which of an application's text
    /// offers was kept, and printing `text/plain` would be a claim.
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Colour(_) => "colour",
            Kind::Link => "link",
            Kind::File => "file path",
            Kind::Image => "image",
        }
    }
}

/// The filter tabs across the top of the popup, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Text,
    Images,
    Links,
    Files,
}

impl Filter {
    pub const ALL: [Filter; 5] = [Filter::All, Filter::Text, Filter::Images, Filter::Links, Filter::Files];

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Text => "Text",
            Filter::Images => "Images",
            Filter::Links => "Links",
            Filter::Files => "Files",
        }
    }

    pub fn index(self) -> usize {
        Filter::ALL.iter().position(|&f| f == self).unwrap_or(0)
    }

    /// The next tab along, wrapping — Tab steps through them, and Tab on
    /// the last one comes back to All rather than stopping.
    pub fn next(self, forward: bool) -> Filter {
        let n = Filter::ALL.len();
        let i = self.index();
        Filter::ALL[if forward { (i + 1) % n } else { (i + n - 1) % n }]
    }

    /// Whether an entry of `kind` belongs under this tab. A colour is
    /// text you can read, so it is under Text too.
    pub fn admits(self, kind: Kind) -> bool {
        match self {
            Filter::All => true,
            Filter::Text => matches!(kind, Kind::Text | Kind::Colour(_)),
            Filter::Images => kind == Kind::Image,
            Filter::Links => kind == Kind::Link,
            Filter::Files => kind == Kind::File,
        }
    }
}

/// `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, or Hyprland's `rgb(rrggbb)` /
/// `rgba(rrggbbaa)` — nothing looser. `#abc` in the middle of a sentence
/// is not a colour; only a copy that is *just* the value is.
fn parse_colour(text: &str) -> Option<Color> {
    if let Ok(colour) = Color::parse(text) {
        return Some(colour);
    }
    let hex = text.strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|d| d * 17);
    let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        3 => Some(Color::rgba(digit(0)?, digit(1)?, digit(2)?, 0xff)),
        4 => Some(Color::rgba(digit(0)?, digit(1)?, digit(2)?, digit(3)?)),
        6 => Some(Color::rgba(pair(0)?, pair(2)?, pair(4)?, 0xff)),
        8 => Some(Color::rgba(pair(0)?, pair(2)?, pair(4)?, pair(6)?)),
        _ => None,
    }
}

const LINK_SCHEMES: [&str; 5] = ["http://", "https://", "ftp://", "mailto:", "www."];

fn is_link(text: &str) -> bool {
    !text.is_empty()
        && !text.contains(char::is_whitespace)
        && LINK_SCHEMES.iter().any(|scheme| text.len() > scheme.len() && text.to_ascii_lowercase().starts_with(scheme))
}

/// A path or `file://` URI alone, or several `file://` URIs one per line
/// — which is what a file manager's copy looks like as text.
fn is_file(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    match lines.as_slice() {
        [] => false,
        [one] => one.starts_with("file://") || ((one.starts_with('/') || one.starts_with("~/")) && one.len() > 1 && !one.contains("  ")),
        many => many.iter().all(|l| l.starts_with("file://")),
    }
}

/// How a link or a file reads in a one-line row: a link without its
/// `https://`, a file URI as the path it names, with `$HOME` as `~`.
/// Only for display — what is pasted is always the entry exactly as
/// copied.
pub fn display(kind: Kind, text: &str, home: Option<&str>) -> String {
    let text = text.trim();
    match kind {
        Kind::Link => text.strip_prefix("https://").or_else(|| text.strip_prefix("http://")).unwrap_or(text).to_string(),
        Kind::File => {
            let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(text);
            let path = match first.strip_prefix("file://") {
                Some(rest) => percent_decode(rest.strip_prefix("localhost").unwrap_or(rest)),
                None => first.to_string(),
            };
            let path = match home {
                Some(home) if !home.is_empty() && (path == home || path.starts_with(&format!("{home}/"))) => {
                    format!("~{}", &path[home.len()..])
                }
                _ => path,
            };
            let more = text.lines().filter(|l| !l.trim().is_empty()).count().saturating_sub(1);
            if more > 0 {
                format!("{path} +{more}")
            } else {
                path
            }
        }
        _ => text.to_string(),
    }
}

/// `%20` back to a space, and so on — a URI's escapes, decoded as bytes
/// and then read as UTF-8 so a multi-byte name survives. A malformed
/// escape is left exactly as written rather than guessed at.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Bytes, not `&s[i + 1..i + 3]`: a multi-byte character right
        // after a `%` would put that slice mid-character and panic.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(text: &str) -> Kind {
        Kind::of(&Content::Text(text.to_string()))
    }

    #[test]
    fn a_colour_value_on_its_own_is_a_colour_and_carries_it() {
        assert_eq!(kind("#bd93f9"), Kind::Colour(Color::rgba(0xbd, 0x93, 0xf9, 0xff)));
        assert_eq!(kind("  #fff\n"), Kind::Colour(Color::rgba(0xff, 0xff, 0xff, 0xff)));
        assert_eq!(kind("rgba(26263aff)"), Kind::Colour(Color::rgba(0x26, 0x26, 0x3a, 0xff)));
    }

    #[test]
    fn a_hash_that_is_not_just_a_colour_is_text() {
        assert_eq!(kind("#bd93f9 is the accent"), Kind::Text);
        assert_eq!(kind("#12345"), Kind::Text, "five digits is no colour format");
        assert_eq!(kind("#include"), Kind::Text);
    }

    #[test]
    fn a_single_token_with_a_scheme_is_a_link_and_prose_mentioning_one_is_not() {
        assert_eq!(kind("https://wiki.hyprland.org/Configuring/Binds"), Kind::Link);
        assert_eq!(kind("www.example.org"), Kind::Link);
        assert_eq!(kind("see https://example.org for more"), Kind::Text);
        assert_eq!(kind("https://"), Kind::Text, "a scheme with nothing after it is not a link");
    }

    #[test]
    fn a_path_or_file_uri_on_its_own_is_a_file() {
        assert_eq!(kind("/home/adam/Pictures/IMG_2052.jpg"), Kind::File);
        assert_eq!(kind("~/notes.txt"), Kind::File);
        assert_eq!(kind("file:///home/adam/a.txt\nfile:///home/adam/b.txt"), Kind::File);
        assert_eq!(kind("/"), Kind::Text, "a lone slash is not a path worth a badge");
        assert_eq!(kind("/home/a\nsomething else"), Kind::Text);
    }

    #[test]
    fn ordinary_text_and_code_are_text() {
        assert_eq!(kind("cargo build --release"), Kind::Text);
        assert_eq!(kind("bind = SUPER, period, exec, emoji-menu"), Kind::Text);
    }

    #[test]
    fn an_image_is_an_image_whatever_its_bytes_say() {
        let content = Content::Image { bytes: b"#fff".to_vec(), mime: hyprforge_clipboard::Mime::new("image/png") };
        assert_eq!(Kind::of(&content), Kind::Image);
    }

    /// The badge and the tab must agree: whatever a row's badge says, the
    /// tab named for it shows that row.
    #[test]
    fn every_kind_appears_under_all_and_under_its_own_tab() {
        let kinds = [Kind::Text, Kind::Colour(Color::BLACK), Kind::Link, Kind::File, Kind::Image];
        for kind in kinds {
            assert!(Filter::All.admits(kind));
            assert_eq!(Filter::ALL[1..].iter().filter(|f| f.admits(kind)).count(), 1, "{kind:?} belongs under exactly one tab besides All");
        }
    }

    #[test]
    fn tab_steps_through_the_filters_and_wraps() {
        assert_eq!(Filter::All.next(true), Filter::Text);
        assert_eq!(Filter::Files.next(true), Filter::All);
        assert_eq!(Filter::All.next(false), Filter::Files);
    }

    #[test]
    fn a_link_reads_without_its_scheme_but_only_on_screen() {
        assert_eq!(display(Kind::Link, "https://wiki.hyprland.org/x", None), "wiki.hyprland.org/x");
        assert_eq!(display(Kind::Link, "mailto:a@b.c", None), "mailto:a@b.c");
    }

    #[test]
    fn a_file_uri_reads_as_the_path_it_names_with_home_as_a_tilde() {
        assert_eq!(
            display(Kind::File, "file:///home/adam/My%20Pictures/IMG_2052.jpg", Some("/home/adam")),
            "~/My Pictures/IMG_2052.jpg"
        );
        assert_eq!(display(Kind::File, "file:///a\nfile:///b\nfile:///c", None), "/a +2");
        assert_eq!(display(Kind::File, "/home/adamant/x", Some("/home/adam")), "/home/adamant/x", "a prefix is not a parent");
    }

    #[test]
    fn a_malformed_escape_is_left_as_written() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        assert_eq!(percent_decode("%é"), "%é", "a multi-byte character after % must not panic");
    }
}
