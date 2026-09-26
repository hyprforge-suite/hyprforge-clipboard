//! Turning an image entry's bytes into something safe to hand the
//! renderer.
//!
//! CLAUDE.md's rule this module exists to obey: a 36-megapixel wallpaper
//! once peaked at 296MB in the lock screen, and an allocation failure
//! inside `iced_tiny_skia` caches as "no entry" and panics on the
//! *next* frame — not this one, which is what made it hard to find the
//! first time. So dimensions are read from the header alone, before any
//! pixel is decoded, exactly the way `hyprforge-authui::screen::renderable`
//! already checks a wallpaper. Anything over the cap is never handed to
//! the decoder at all; the row falls back to the same text preview an
//! unrecognised image would get.
//!
//! # What is capped, and why this number
//!
//! [`MAX_PIXELS`] is 16 megapixels (e.g. 4096x4096, or a 5MP photo at
//! roughly 4:3) — comfortably past anything a screenshot or a pasted
//! photo needs for a thumbnail a few hundred pixels across, and well
//! under the 36-megapixel image that produced a 296MB decode. Decoded
//! RGBA8 at the cap is 16,777,216 * 4 bytes = 64MB for one image; the
//! popup only ever builds a handle for rows in the visible window (see
//! `model::Model::stack`'s visible lines), so the worst case is that window's worth of
//! 64MB images, not the whole history's.

use hyprforge_clipboard::{Content, Entry};
use iced_runtime::core::image::Handle;
use std::collections::HashMap;
use std::io::Cursor;

/// See the module doc for why this number and not a smaller or larger
/// one.
const MAX_PIXELS: u64 = 16 * 1024 * 1024;

/// Reads only the image header to get its dimensions, without decoding
/// any pixels — the same header-only check `renderable` already does for
/// the lock screen's wallpaper.
///
/// `None` means either the format could not even be identified, or its
/// dimensions could not be read — both are treated as "cannot preview",
/// never as "assume it's small".
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Whether an image this size is worth decoding for a thumbnail at all.
///
/// Split out from [`decode`] so the cap itself can be pinned by a test
/// without needing a real encoded image just to exercise the arithmetic.
fn fits_within_cap(width: u32, height: u32) -> bool {
    (width as u64) * (height as u64) <= MAX_PIXELS
}

/// Decodes `bytes` into an iced image handle, or `None` if the image is
/// unreadable or over the pixel cap.
///
/// Decoding (not just measuring) only happens once dimensions have
/// already passed the cap, so the one allocation this function risks is
/// bounded by [`MAX_PIXELS`] regardless of how large the *encoded* bytes
/// claim to be.
fn decode(bytes: &[u8]) -> Option<Handle> {
    let (width, height) = dimensions(bytes)?;
    if !fits_within_cap(width, height) {
        return None;
    }
    let decoded = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = decoded.dimensions();
    Some(Handle::from_rgba(width, height, decoded.into_raw()))
}

/// A cache of decoded thumbnails, so scrolling past the same row twice
/// does not decode its image twice.
///
/// Populated lazily, only for entries [`Cache::get`] is actually asked
/// about — which the popup only calls for rows in the current visible
/// window (see `model::Model::stack`). An entry that never
/// scrolls into view is never decoded at all.
#[derive(Default)]
pub struct Cache {
    handles: HashMap<hyprforge_clipboard::EntryId, Option<Handle>>,
    sizes: HashMap<hyprforge_clipboard::EntryId, Option<(u32, u32)>>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache::default()
    }

    /// The thumbnail for `entry`, decoding and caching it on first ask.
    /// `None` for text entries, and for an image over the cap or one
    /// that would not decode.
    pub fn get(&mut self, entry: &Entry) -> Option<Handle> {
        let Content::Image { bytes, .. } = &entry.content else {
            return None;
        };
        self.handles.entry(entry.id.clone()).or_insert_with(|| decode(bytes)).clone()
    }

    /// An image entry's width and height, from its header alone — the
    /// preview pane's "1440 × 900", which must be answerable even for an
    /// image over the decode cap, since saying how big it is costs
    /// nothing and is exactly what someone wondering why there is no
    /// thumbnail wants to know.
    pub fn size(&mut self, entry: &Entry) -> Option<(u32, u32)> {
        let Content::Image { bytes, .. } = &entry.content else {
            return None;
        };
        *self.sizes.entry(entry.id.clone()).or_insert_with(|| dimensions(bytes))
    }

    /// Whether `entry` is an image too large to decode for a thumbnail,
    /// so the pane can say so rather than showing an empty box.
    pub fn over_cap(&mut self, entry: &Entry) -> bool {
        self.size(entry).is_some_and(|(w, h)| !fits_within_cap(w, h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_clipboard::{EntryId, Mime};

    fn tiny_png() -> Vec<u8> {
        // A real, tiny (1x1) PNG, so `decode` exercises the actual
        // decoder rather than a stub.
        let mut pixel = image::RgbaImage::new(1, 1);
        pixel.put_pixel(0, 0, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn image_entry(bytes: Vec<u8>) -> Entry {
        let content = Content::Image { bytes, mime: Mime::new("image/png") };
        Entry { id: EntryId::of(&content), content, copied_at: 0, pinned: false }
    }

    #[test]
    fn a_small_real_image_decodes_to_a_handle() {
        assert!(decode(&tiny_png()).is_some());
    }

    #[test]
    fn bytes_that_are_not_an_image_at_all_never_produce_a_handle() {
        assert!(decode(b"not an image").is_none());
    }

    #[test]
    fn an_image_over_the_pixel_cap_is_refused_before_decoding() {
        assert!(!fits_within_cap(5000, 5000), "25 megapixels must exceed the cap");
        assert!(fits_within_cap(4096, 4096), "16 megapixels must fit exactly at the cap");
    }

    #[test]
    fn a_text_entry_has_no_thumbnail() {
        let mut cache = Cache::new();
        let entry = Entry {
            id: EntryId::of(&Content::Text("hi".into())),
            content: Content::Text("hi".into()),
            copied_at: 0,
            pinned: false,
        };
        assert!(cache.get(&entry).is_none());
    }

    #[test]
    fn an_images_size_is_read_from_its_header_and_text_has_none() {
        let mut cache = Cache::new();
        assert_eq!(cache.size(&image_entry(tiny_png())), Some((1, 1)));
        assert!(!cache.over_cap(&image_entry(tiny_png())));
        let text = Entry { id: EntryId::of(&Content::Text("hi".into())), content: Content::Text("hi".into()), copied_at: 0, pinned: false };
        assert_eq!(cache.size(&text), None);
    }

    #[test]
    fn decoding_the_same_entry_twice_only_decodes_once() {
        let mut cache = Cache::new();
        let entry = image_entry(tiny_png());
        let first = cache.get(&entry);
        let second = cache.get(&entry);
        assert!(first.is_some());
        assert_eq!(first, second);
        assert_eq!(cache.handles.len(), 1, "must not decode the same id twice");
    }
}
