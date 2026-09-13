//! What a clipboard entry is, with no Wayland in it.

use std::fmt;

/// A MIME type as the compositor offers it.
///
/// Kept as an owned string rather than an enum: an offer carries whatever
/// the source application chose, the list is open-ended, and a type this
/// crate does not recognise still has to be *seen* — both to detect the
/// sensitivity hint below and to decide which of several offers is the
/// most useful one to keep.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mime(String);

impl Mime {
    pub fn new(value: impl Into<String>) -> Self {
        Mime(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_text(&self) -> bool {
        let m = self.0.to_ascii_lowercase();
        m.starts_with("text/") || m == "utf8_string" || m == "string"
    }

    pub fn is_image(&self) -> bool {
        self.0.to_ascii_lowercase().starts_with("image/")
    }
}

impl fmt::Display for Mime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Mime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mime({})", self.0)
    }
}

/// The MIME type password managers put on an offer to say "do not keep
/// this".
///
/// A KDE convention that every serious clipboard manager honours, and
/// which KeePassXC, Bitwarden, 1Password and others emit. It is not a
/// standard, and it is the only signal there is.
pub const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

/// The value of [`PASSWORD_HINT`] that means "secret".
pub const PASSWORD_HINT_SECRET: &str = "secret";

/// Whether an offer may be recorded at all.
///
/// This is the single most consequential decision in this crate. A
/// clipboard manager keeps a history on disk; a password manager puts
/// your password on the clipboard. Without this, using both means your
/// vault ends up in a file in your home directory, and neither program
/// ever says so.
///
/// The same rule as `hyprforge-authui`'s password field, one layer out:
/// the value is never written, never logged, and never rendered. Here it
/// is never even *read* — a secret offer is dropped before its bytes are
/// requested from the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// Safe to record.
    Recordable,
    /// The source asked for this not to be kept.
    Secret,
}

impl Sensitivity {
    /// Classifies an offer from the MIME types it advertises, and the
    /// value of the password hint if it carried one.
    ///
    /// `hint_value` is `None` when the offer did not advertise
    /// [`PASSWORD_HINT`] at all. When it did, the *value* decides —
    /// applications use the same type to say "this is fine" as to say
    /// "this is a secret", so the presence of the type alone is not the
    /// signal and treating it as one would silently stop recording
    /// ordinary copies from those applications.
    pub fn classify(mimes: &[Mime], hint_value: Option<&str>) -> Sensitivity {
        let advertises_hint = mimes.iter().any(|m| m.as_str() == PASSWORD_HINT);
        if !advertises_hint {
            return Sensitivity::Recordable;
        }
        match hint_value {
            Some(value) if value.trim().eq_ignore_ascii_case(PASSWORD_HINT_SECRET) => {
                Sensitivity::Secret
            }
            // The hint is advertised but could not be read. Err toward
            // not recording: a password kept by mistake is far worse than
            // a copy missing from the history, and the user can always
            // copy again.
            None => Sensitivity::Secret,
            Some(_) => Sensitivity::Recordable,
        }
    }

    pub fn is_secret(self) -> bool {
        matches!(self, Sensitivity::Secret)
    }
}

/// What an entry holds.
#[derive(Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// Image bytes exactly as the source offered them, with the type it
    /// called them. Not decoded here — decoding a 36-megapixel PNG to
    /// measure it is how the lock screen once peaked at 296MB, and this
    /// crate may be handed anything.
    Image {
        bytes: Vec<u8>,
        mime: Mime,
    },
}

impl Content {
    pub fn is_empty(&self) -> bool {
        match self {
            Content::Text(text) => text.trim().is_empty(),
            Content::Image { bytes, .. } => bytes.is_empty(),
        }
    }

    /// Bytes on the wire, for size accounting.
    pub fn size(&self) -> usize {
        match self {
            Content::Text(text) => text.len(),
            Content::Image { bytes, .. } => bytes.len(),
        }
    }

    /// A one-line label for the list.
    ///
    /// Collapses whitespace so a copied block of code is one row rather
    /// than a paragraph, and truncates on a **character** boundary — a
    /// byte-truncated UTF-8 string is not a string, and this runs on
    /// whatever the user copied.
    pub fn preview(&self, max_chars: usize) -> String {
        match self {
            Content::Image { bytes, mime } => {
                format!("Image · {} · {}", mime, human_size(bytes.len()))
            }
            Content::Text(text) => {
                let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if collapsed.chars().count() <= max_chars {
                    return collapsed;
                }
                let cut: String = collapsed
                    .chars()
                    .take(max_chars.saturating_sub(1))
                    .collect();
                format!("{cut}\u{2026}")
            }
        }
    }
}

/// Renders a description, never the content.
///
/// A clipboard holds whatever the user copied, which routinely includes
/// things they would not want in a log — and in an agent session, a
/// `Debug` of an entry would reach the transcript. The same reasoning as
/// `Psk` in `hyprforge-network`, for a much wider range of content.
///
/// This is deliberately *not* built on `hyprforge_secret::Secret<T>`,
/// unlike `Psk` and the lock screen's typed password. `Secret` hides a
/// value entirely because the value itself is the whole risk — there is
/// nothing about a password worth describing. A clipboard entry is a
/// domain enum whose `Debug` needs to say *which kind* of thing this is
/// (`Text` vs. `Image`, and an image's MIME type) even while redacting
/// its content, and `Secret<T>` has no way to carry that: it is built to
/// render nothing but a count. Forcing `Content` into it would mean
/// either losing the variant/MIME information or reaching around the
/// wrapper to print it anyway, which defeats the point of sharing an
/// implementation. What *is* shared is the smaller piece both cases
/// need: rendering "a redacted count" the same way everywhere, via
/// `hyprforge_secret::chars`/`bytes` rather than a third hand-rolled
/// `format_args!("<{} ...>", ...)`.
impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Content::Text(text) => {
                write!(f, "Text({:?})", hyprforge_secret::chars(text.chars().count()))
            }
            Content::Image { bytes, mime } => {
                write!(f, "Image({mime}, {:?})", hyprforge_secret::bytes(bytes.len()))
            }
        }
    }
}

fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    match bytes {
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.0} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

/// One remembered clipboard entry.
#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    /// Content-derived, so the same thing copied twice is the same entry
    /// rather than two — which is what makes "move it to the top" mean
    /// something instead of growing a list of duplicates.
    pub id: EntryId,
    pub content: Content,
    /// Seconds since the Unix epoch. Plain, because this is written to a
    /// file a user may read, and a timestamp they cannot interpret is
    /// worse than one they can.
    pub copied_at: u64,
    /// Kept across a restart and shown first. Windows calls this pinning.
    pub pinned: bool,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("id", &self.id)
            .field("content", &self.content)
            .field("pinned", &self.pinned)
            .finish()
    }
}

/// A content hash, so identical copies collapse.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(String);

impl EntryId {
    /// Wraps an already-known id string, without hashing anything.
    ///
    /// For looking an entry up by an id a caller already has — an IPC
    /// request naming the entry to pin or remove — never for minting a
    /// *new* entry's id. [`EntryId::of`] stays the only way to do that,
    /// so an id can never silently drift out of sync with what it is
    /// supposed to be a hash of. An id is a content hash, not the
    /// content itself, so accepting one from a request and comparing it
    /// against stored entries carries none of the "never log a
    /// keystroke" risk the rest of this module protects against.
    pub fn from_raw(id: impl Into<String>) -> Self {
        EntryId(id.into())
    }

    /// Derived from the bytes, never from the time or a counter: the
    /// point is that copying the same thing again is recognised.
    pub fn of(content: &Content) -> Self {
        let mut hasher = blake3::Hasher::new();
        match content {
            Content::Text(text) => {
                hasher.update(b"text\0");
                hasher.update(text.as_bytes());
            }
            Content::Image { bytes, mime } => {
                hasher.update(b"image\0");
                hasher.update(mime.as_str().as_bytes());
                hasher.update(b"\0");
                hasher.update(bytes);
            }
        }
        EntryId(hasher.finalize().to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short form: an id is a hash of the content, and a full one in a
        // log is a fingerprint of what was copied.
        write!(f, "EntryId({}…)", &self.0[..8.min(self.0.len())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<Mime> {
        list.iter().map(|m| Mime::new(*m)).collect()
    }

    /// The property this crate exists to protect. A password manager
    /// marks its copy, and a marked copy is never recorded.
    #[test]
    fn an_offer_a_password_manager_marked_secret_is_never_recordable() {
        let offer = mimes(&["text/plain", PASSWORD_HINT]);
        assert_eq!(
            Sensitivity::classify(&offer, Some("secret")),
            Sensitivity::Secret
        );
    }

    /// The hint is advertised but its value could not be read. Keeping a
    /// password by mistake is far worse than missing a copy from the
    /// history, and the user can copy again.
    #[test]
    fn an_unreadable_hint_is_treated_as_secret_rather_than_assumed_safe() {
        let offer = mimes(&["text/plain", PASSWORD_HINT]);
        assert_eq!(Sensitivity::classify(&offer, None), Sensitivity::Secret);
    }

    /// Applications use the same MIME type to say "this one is fine", so
    /// treating its mere presence as a refusal would silently stop
    /// recording ordinary copies from every app that sets it.
    #[test]
    fn the_hint_being_present_is_not_itself_the_signal_its_value_is() {
        let offer = mimes(&["text/plain", PASSWORD_HINT]);
        assert_eq!(
            Sensitivity::classify(&offer, Some("not-secret")),
            Sensitivity::Recordable
        );
    }

    #[test]
    fn an_ordinary_offer_with_no_hint_is_recordable() {
        assert_eq!(
            Sensitivity::classify(&mimes(&["text/plain", "text/html"]), None),
            Sensitivity::Recordable
        );
    }

    /// A clipboard holds whatever was copied. A derived `Debug` would put
    /// all of it in any log line that rendered an entry — and in an agent
    /// session, in the transcript.
    #[test]
    fn an_entry_never_renders_its_own_content() {
        let secret = "correct-horse-battery-staple";
        let entry = Entry {
            id: EntryId::of(&Content::Text(secret.to_string())),
            content: Content::Text(secret.to_string()),
            copied_at: 0,
            pinned: false,
        };
        let rendered = format!("{entry:?}");
        assert!(!rendered.contains(secret), "leaked: {rendered}");
        assert!(
            rendered.contains(&format!("{} chars", secret.chars().count())),
            "expected a redacted count, got {rendered}"
        );
    }

    /// Copying the same thing twice is one entry, which is what lets the
    /// history move it to the top instead of growing.
    #[test]
    fn the_same_content_copied_twice_has_the_same_id() {
        let a = Content::Text("hello".to_string());
        let b = Content::Text("hello".to_string());
        assert_eq!(EntryId::of(&a), EntryId::of(&b));
        assert_ne!(EntryId::of(&a), EntryId::of(&Content::Text("other".into())));
    }

    /// Text and an image whose bytes happen to match are different
    /// things, and must not collapse into one entry.
    #[test]
    fn text_and_an_image_with_the_same_bytes_are_different_entries() {
        let text = Content::Text("hello".to_string());
        let image = Content::Image {
            bytes: b"hello".to_vec(),
            mime: Mime::new("image/png"),
        };
        assert_ne!(EntryId::of(&text), EntryId::of(&image));
    }

    /// A copied block of code is one row in the list, not a paragraph.
    #[test]
    fn a_preview_is_one_line_and_never_splits_a_character() {
        let text = Content::Text("line one\n\n   line two\tline three".to_string());
        assert_eq!(text.preview(80), "line one line two line three");

        // Multi-byte characters: truncating by bytes would produce
        // something that is not a string at all.
        let emoji = Content::Text("🎉".repeat(50));
        let preview = emoji.preview(10);
        assert_eq!(preview.chars().count(), 10);
        assert!(preview.ends_with('\u{2026}'));
    }

    #[test]
    fn an_image_preview_says_what_it_is_rather_than_showing_bytes() {
        let image = Content::Image {
            bytes: vec![0; 2048],
            mime: Mime::new("image/png"),
        };
        let preview = image.preview(80);
        assert!(preview.contains("image/png"));
        assert!(preview.contains("2 KB"));
    }

    #[test]
    fn a_mime_knows_whether_it_is_text_or_an_image() {
        assert!(Mime::new("text/plain;charset=utf-8").is_text());
        assert!(Mime::new("UTF8_STRING").is_text());
        assert!(Mime::new("image/png").is_image());
        assert!(!Mime::new("image/png").is_text());
    }

    /// Whitespace-only copies are noise — selecting a blank line should
    /// not push something useful out of the history.
    #[test]
    fn a_blank_copy_is_empty_even_when_it_has_bytes() {
        assert!(Content::Text("   \n\t ".to_string()).is_empty());
        assert!(!Content::Text("x".to_string()).is_empty());
    }
}
