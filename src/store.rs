//! The clipboard history: what is kept, in what order, how much, and how
//! it survives a restart.
//!
//! # Never a secret
//!
//! The only way into a [`History`] is [`History::record`], and the only
//! thing it accepts is a [`Recordable`] — which can only be constructed
//! from a [`Sensitivity`] that is not [`Sensitivity::Secret`]
//! ([`Recordable::new`] returns `None` for one that is). There is no
//! other public constructor and no other way to append an entry, so a
//! marked password cannot reach this store through its own API; the
//! caller (the Wayland layer) still has to classify the offer, but it has
//! no way to skip that step and still get something this type will
//! accept.
//!
//! # Persistence shape
//!
//! The index — ids, timestamps, pins, text content and image
//! *references* — is one TOML file at [`hyprforge_paths::clipboard_index_path`].
//! Image bytes live one file per entry under
//! [`hyprforge_paths::clipboard_images_dir`], named by the entry's id.
//! Inlining a screenshot into the index as base64 would make that file
//! unreadable by eye and enormous, and would mean rewriting every text
//! entry's row just to add or evict one image.

use crate::types::{Content, Entry, EntryId, Mime, Sensitivity};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Entry-count cap. copyq on this machine held on the order of a
/// thousand rows after 20 hours of ordinary use before its *byte* size
/// became the practical limit (see [`MAX_TOTAL_BYTES`]) — a cap well
/// past that still leaves room for a long day's copying, while keeping
/// "how far back does this go" bounded rather than open-ended. Past a
/// few hundred rows nobody is scrolling to find one anyway; the oldest
/// unpinned entries are what falls off first.
const MAX_ENTRIES: usize = 500;

/// Total-bytes cap, independent of the entry count above. copyq reached
/// 28 MB in that same 20 hours, almost entirely from images — a handful
/// of screenshots outweigh thousands of text entries, so a count-only
/// cap would let a handful of them consume hundreds of megabytes while
/// reporting a history that "looks small". 64 MB is chosen as room for
/// a full ordinary day of copying (including a handful of screenshots)
/// without tripping eviction on a normal session, while still bounding
/// how much a runaway one — a burst of repeated large screenshots — can
/// pile up on disk.
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Cap on any single entry, a quarter of [`MAX_TOTAL_BYTES`]. Without
/// this, one very large image (a multi-monitor screenshot) could alone
/// exceed the total cap — and since eviction only ever removes *other*
/// entries to make room, admitting it would evict the entire rest of the
/// history just to keep this one new item. Refusing it instead leaves
/// every existing entry untouched: the user loses one over-sized copy,
/// not everything they had already kept. A quarter leaves room for a few
/// large images side by side, but not one on its own.
const MAX_SINGLE_ENTRY_BYTES: u64 = MAX_TOTAL_BYTES / 4;

/// The only way to hand a copy to [`History::record`].
///
/// Constructing one requires the [`Sensitivity`] the compositor layer
/// classified the offer as; it simply does not exist when that
/// classification was [`Sensitivity::Secret`]. This is the type-level
/// half of "never store a secret" — see the module doc for the other
/// half (there is no other entry point into the store).
pub struct Recordable(Content);

impl Recordable {
    /// `None` for a secret offer. The caller cannot unwrap its way past
    /// this — there is no field to read the content back out of a
    /// `Recordable` other than by handing it to [`History::record`].
    pub fn new(content: Content, sensitivity: Sensitivity) -> Option<Self> {
        match sensitivity {
            Sensitivity::Secret => None,
            Sensitivity::Recordable => Some(Recordable(content)),
        }
    }
}

/// The clipboard history, newest first with pinned entries sorted above
/// unpinned ones.
#[derive(Debug, Default)]
pub struct History {
    entries: Vec<Entry>,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// The index exists and will not parse. **Not** the same as absent —
    /// see [`History::load_from`]. Never silently replaced with an empty
    /// history, which would mean the next save overwrites what was
    /// actually there.
    #[error("{path} could not be read as clipboard history: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path} could not be opened: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} could not be written: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl History {
    pub fn new() -> Self {
        History::default()
    }

    /// The entries, in display order: pinned first, then newest first
    /// within each group.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Records a copy, or updates one already present.
    ///
    /// Returns `false` for a copy this store will never hold: empty (or
    /// whitespace-only) content, or a single item over
    /// `MAX_SINGLE_ENTRY_BYTES` — the two cases where there is nothing
    /// useful to add and, for the size case, admitting it would come at
    /// the cost of everything already kept (see the constant's doc).
    ///
    /// Re-recording content already present does not add a duplicate —
    /// `EntryId` is a content hash precisely so this can be recognised —
    /// it moves the existing entry to the top and updates its timestamp,
    /// keeping whatever pin state it already had.
    pub fn record(&mut self, recordable: Recordable, now: u64) -> bool {
        let content = recordable.0;
        if content.is_empty() {
            return false;
        }
        if content.size() as u64 > MAX_SINGLE_ENTRY_BYTES {
            return false;
        }

        let id = EntryId::of(&content);
        if let Some(existing) = self.entries.iter_mut().find(|e| e.id == id) {
            existing.copied_at = now;
        } else {
            self.entries.push(Entry {
                id,
                content,
                copied_at: now,
                pinned: false,
            });
        }
        self.sort();
        self.enforce_caps();
        true
    }

    /// Pins or unpins an entry. A pinned entry is never evicted by
    /// either cap in `Self::enforce_caps` — a pin is the user saying
    /// "keep this", and a cap silently dropping it anyway would break
    /// that promise. Returns `false` if no entry has this id.
    pub fn set_pinned(&mut self, id: &EntryId, pinned: bool) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| &e.id == id) else {
            return false;
        };
        entry.pinned = pinned;
        self.sort();
        true
    }

    /// Removes an entry outright (the user asking to delete a row, not a
    /// cap). Returns `false` if no entry has this id.
    pub fn remove(&mut self, id: &EntryId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| &e.id != id);
        self.entries.len() != before
    }

    /// Pinned entries first, newest first within each group. The id
    /// tie-break makes the order deterministic when two entries share a
    /// timestamp (a fast paste-then-copy, or a restored history where
    /// two rows were recorded in the same second), which matters for the
    /// tests below and for the popup not visibly reordering two
    /// same-second rows between one draw and the next.
    fn sort(&mut self) {
        self.entries.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.copied_at.cmp(&a.copied_at))
                .then(a.id.cmp(&b.id))
        });
    }

    /// Total bytes across every kept entry — the same accounting
    /// `Self::enforce_caps` uses against `MAX_TOTAL_BYTES`. Exposed
    /// for a `status` query so a caller can see how close the history is
    /// to that cap without duplicating the accounting itself.
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.content.size() as u64).sum()
    }

    /// Drops oldest-first until both caps are satisfied, but never a
    /// pinned entry. Because [`Self::sort`] keeps unpinned entries
    /// ordered newest-first after every pinned one, the oldest unpinned
    /// entry is always the *last* unpinned entry in the vector — so the
    /// last matching position is exactly the one to remove.
    ///
    /// If every remaining entry is pinned, both loops stop rather than
    /// looping forever or touching a pin: the cap stays exceeded, which
    /// is the documented cost of pinning past it.
    fn enforce_caps(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
            match self.entries.iter().rposition(|e| !e.pinned) {
                Some(idx) => {
                    self.entries.remove(idx);
                }
                None => break,
            }
        }
        while self.total_bytes() > MAX_TOTAL_BYTES {
            match self.entries.iter().rposition(|e| !e.pinned) {
                Some(idx) => {
                    self.entries.remove(idx);
                }
                None => break,
            }
        }
    }

    /// Loads the default on-disk history: [`hyprforge_paths::clipboard_index_path`]
    /// for the index, [`hyprforge_paths::clipboard_images_dir`] for image bytes.
    pub fn load() -> Result<History, HistoryError> {
        History::load_from(
            &hyprforge_paths::clipboard_index_path(),
            &hyprforge_paths::clipboard_images_dir(),
        )
    }

    /// Reads the index at `index_path`, resolving image entries against
    /// files under `images_dir`.
    ///
    /// A **missing** index is first run and yields an empty history. An
    /// index that **exists and will not parse** is reported as
    /// [`HistoryError::Unreadable`] rather than treated as empty — the
    /// same distinction `hyprforge-tray`'s `prefs::load_from` makes, and
    /// for the same reason: silently defaulting it would mean the very
    /// next save overwrites whatever the user actually had.
    ///
    /// An entry whose index row names an image, but whose file is
    /// missing from `images_dir` (deleted by hand, or a write that
    /// landed the index but not the image), is dropped rather than
    /// failing the whole load or panicking — a `id` in the log is a
    /// content hash, not the content, so noting which entry was dropped
    /// does not repeat the mistake this crate exists to avoid.
    pub fn load_from(index_path: &Path, images_dir: &Path) -> Result<History, HistoryError> {
        let text = match std::fs::read_to_string(index_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(History::default()),
            Err(source) => {
                return Err(HistoryError::Io {
                    path: index_path.to_path_buf(),
                    source,
                })
            }
        };
        let index: IndexFile =
            toml::from_str(&text).map_err(|source| HistoryError::Unreadable {
                path: index_path.to_path_buf(),
                source,
            })?;

        let mut entries = Vec::with_capacity(index.entries.len());
        for row in index.entries {
            let content = match row.content {
                IndexContent::Text { text } => Content::Text(text),
                IndexContent::Image { mime } => {
                    let image_path = images_dir.join(format!("{}.bin", row.id));
                    match std::fs::read(&image_path) {
                        Ok(bytes) => Content::Image {
                            bytes,
                            mime: Mime::new(mime),
                        },
                        Err(err) => {
                            tracing::warn!(
                                id = %row.id,
                                path = %image_path.display(),
                                error = %err,
                                "clipboard image file missing on disk; dropping entry from history"
                            );
                            continue;
                        }
                    }
                }
            };
            // Recomputed rather than trusting the stored id: the id is a
            // content hash, so this is also a free consistency check
            // against a partially-written or hand-edited row, and it
            // means this module never needs a raw "parse this string as
            // an id" constructor from `types.rs`.
            let id = EntryId::of(&content);
            entries.push(Entry {
                id,
                content,
                copied_at: row.copied_at,
                pinned: row.pinned,
            });
        }

        let mut history = History { entries };
        history.sort();
        Ok(history)
    }

    /// Saves to the default on-disk location. See [`Self::load`].
    pub fn save(&self) -> Result<(), HistoryError> {
        self.save_to(
            &hyprforge_paths::clipboard_index_path(),
            &hyprforge_paths::clipboard_images_dir(),
        )
    }

    /// Writes the index to `index_path` and any not-yet-written image
    /// bytes under `images_dir`, then removes image files that no longer
    /// belong to any entry.
    ///
    /// The orphan cleanup matters as much as the write does: without it,
    /// an entry evicted by a cap (or removed, or superseded) leaves its
    /// image file behind forever, which is exactly the unbounded disk
    /// growth `MAX_TOTAL_BYTES` exists to prevent — the index would
    /// stay small while the images directory kept growing regardless.
    pub fn save_to(&self, index_path: &Path, images_dir: &Path) -> Result<(), HistoryError> {
        std::fs::create_dir_all(images_dir).map_err(|source| HistoryError::Write {
            path: images_dir.to_path_buf(),
            source,
        })?;

        let mut keep = std::collections::HashSet::new();
        for entry in &self.entries {
            if let Content::Image { bytes, .. } = &entry.content {
                let path = images_dir.join(format!("{}.bin", entry.id.as_str()));
                keep.insert(path.clone());
                // Content is immutable given the id (the id *is* a hash
                // of it), so a file that already exists needs no rewrite
                // — skipping it is what keeps a save cheap when nothing
                // about the images changed.
                if !path.exists() {
                    hyprforge_paths::write_atomic_bytes(&path, bytes).map_err(|source| {
                        HistoryError::Write {
                            path: path.clone(),
                            source,
                        }
                    })?;
                }
            }
        }

        if let Ok(read_dir) = std::fs::read_dir(images_dir) {
            for dir_entry in read_dir.flatten() {
                let path = dir_entry.path();
                if !keep.contains(&path) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        let index = IndexFile {
            entries: self
                .entries
                .iter()
                .map(|e| IndexEntry {
                    id: e.id.as_str().to_string(),
                    copied_at: e.copied_at,
                    pinned: e.pinned,
                    content: match &e.content {
                        Content::Text(text) => IndexContent::Text { text: text.clone() },
                        Content::Image { mime, .. } => IndexContent::Image {
                            mime: mime.as_str().to_string(),
                        },
                    },
                })
                .collect(),
        };
        let text = toml::to_string_pretty(&index)
            .expect("clipboard index is plain data and always serialises");
        hyprforge_paths::write_atomic(index_path, &text).map_err(|source| HistoryError::Write {
            path: index_path.to_path_buf(),
            source,
        })
    }
}

/// On-disk shape of the index. Image bytes are never in here — see the
/// module doc — only enough to find them again: the id, which is also
/// the image file's name.
#[derive(Serialize, Deserialize)]
struct IndexFile {
    #[serde(default)]
    entries: Vec<IndexEntry>,
}

#[derive(Serialize, Deserialize)]
struct IndexEntry {
    id: String,
    copied_at: u64,
    #[serde(default)]
    pinned: bool,
    #[serde(flatten)]
    content: IndexContent,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum IndexContent {
    Text { text: String },
    Image { mime: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Recordable {
        Recordable::new(Content::Text(s.to_string()), Sensitivity::Recordable).unwrap()
    }

    fn image(bytes: Vec<u8>) -> Recordable {
        Recordable::new(
            Content::Image {
                bytes,
                mime: Mime::new("image/png"),
            },
            Sensitivity::Recordable,
        )
        .unwrap()
    }

    #[test]
    fn recording_a_secret_is_not_possible_at_all() {
        assert!(Recordable::new(Content::Text("hunter2".into()), Sensitivity::Secret).is_none());
    }

    #[test]
    fn an_entry_whose_content_is_empty_or_whitespace_only_is_not_stored() {
        let mut history = History::new();
        assert!(!history.record(text(""), 1));
        assert!(!history.record(text("   \n\t"), 1));
        assert!(history.entries().is_empty());
    }

    #[test]
    fn recopying_an_existing_entry_moves_it_to_the_top_instead_of_duplicating() {
        let mut history = History::new();
        history.record(text("first"), 1);
        history.record(text("second"), 2);
        assert_eq!(history.entries().len(), 2);
        assert_eq!(history.entries()[0].content, Content::Text("second".into()));

        // Re-copy "first" later: it moves to the top, no duplicate, and
        // the timestamp updates.
        history.record(text("first"), 3);
        assert_eq!(history.entries().len(), 2, "must not duplicate");
        assert_eq!(history.entries()[0].content, Content::Text("first".into()));
        assert_eq!(history.entries()[0].copied_at, 3);
    }

    #[test]
    fn recopying_a_pinned_entry_keeps_it_pinned() {
        let mut history = History::new();
        history.record(text("keep me"), 1);
        let id = history.entries()[0].id.clone();
        history.set_pinned(&id, true);

        history.record(text("something else"), 2);
        history.record(text("keep me"), 3);

        let entry = history.entries().iter().find(|e| e.id == id).unwrap();
        assert!(entry.pinned, "pin must survive the re-copy");
        assert_eq!(entry.copied_at, 3, "timestamp still updates");
    }

    #[test]
    fn a_pinned_entry_is_never_evicted_by_the_count_cap() {
        let mut history = History::new();
        history.record(text("pin me"), 0);
        let id = history.entries()[0].id.clone();
        history.set_pinned(&id, true);

        for i in 1..(MAX_ENTRIES as u64 + 50) {
            history.record(text(&format!("entry {i}")), i);
        }

        assert!(
            history.entries().iter().any(|e| e.id == id),
            "pinned entry must survive even though the count cap was exceeded many times over"
        );
        assert!(history.entries().len() <= MAX_ENTRIES + 1);
    }

    #[test]
    fn a_pinned_entry_is_never_evicted_by_the_byte_cap() {
        let mut history = History::new();
        // One pinned image, then push the byte cap far past its limit
        // with more images.
        history.record(image(vec![1u8; 1024]), 0);
        let id = history.entries()[0].id.clone();
        history.set_pinned(&id, true);

        let chunk = (MAX_SINGLE_ENTRY_BYTES / 2) as usize;
        for i in 1..20u64 {
            history.record(image(vec![i as u8; chunk]), i);
        }

        assert!(
            history.entries().iter().any(|e| e.id == id),
            "pinned entry must survive even though the byte cap was exceeded"
        );
    }

    #[test]
    fn the_byte_cap_evicts_the_oldest_unpinned_entry_first_when_images_push_it_over() {
        let mut history = History::new();
        // Each chunk sits right at the single-entry cap (never over it,
        // so none is individually refused), and four of them exactly
        // fill the total cap.
        let chunk = MAX_SINGLE_ENTRY_BYTES as usize;
        history.record(image(vec![1u8; chunk]), 1);
        let oldest_id = history.entries()[0].id.clone();
        history.record(image(vec![2u8; chunk]), 2);
        history.record(image(vec![3u8; chunk]), 3);
        history.record(image(vec![4u8; chunk]), 4);
        // A fifth pushes the total over, and the oldest (first recorded)
        // must be the one dropped, not the newest.
        history.record(image(vec![5u8; chunk]), 5);

        assert!(
            !history.entries().iter().any(|e| e.id == oldest_id),
            "the oldest unpinned entry should have been evicted"
        );
        assert!(history.total_bytes() <= MAX_TOTAL_BYTES);
        assert_eq!(
            history.entries()[0].content,
            Content::Image {
                bytes: vec![5u8; chunk],
                mime: Mime::new("image/png")
            },
            "the newest entry must still be present"
        );
    }

    #[test]
    fn a_single_entry_larger_than_the_cap_is_refused_rather_than_evicting_everything() {
        let mut history = History::new();
        history.record(text("keep me"), 1);
        history.record(text("and me"), 2);

        let huge = vec![0u8; (MAX_SINGLE_ENTRY_BYTES + 1) as usize];
        assert!(
            !history.record(image(huge), 3),
            "an entry that alone exceeds the cap must be refused"
        );

        assert_eq!(
            history.entries().len(),
            2,
            "existing history must be untouched by the refusal"
        );
    }

    #[test]
    fn a_missing_index_is_first_run_and_yields_an_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("history.toml");
        let images = dir.path().join("images");
        let history = History::load_from(&index, &images).unwrap();
        assert!(history.entries().is_empty());
    }

    #[test]
    fn an_index_that_exists_and_will_not_parse_is_reported_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("history.toml");
        let images = dir.path().join("images");
        std::fs::write(&index, "entries = not a list at all\n").unwrap();

        let err = History::load_from(&index, &images)
            .expect_err("a malformed index is an error, not an empty history");
        assert!(matches!(err, HistoryError::Unreadable { .. }));

        // And the malformed file must still be sitting there afterward —
        // nothing about loading it may have overwritten it.
        assert_eq!(
            std::fs::read_to_string(&index).unwrap(),
            "entries = not a list at all\n"
        );
    }

    #[test]
    fn an_image_file_missing_from_disk_does_not_fail_the_load_or_panic() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("history.toml");
        let images = dir.path().join("images");
        std::fs::create_dir_all(&images).unwrap();

        let mut history = History::new();
        history.record(text("survives"), 1);
        history.record(image(vec![9u8; 32]), 2);
        history.save_to(&index, &images).unwrap();

        // Delete the image file backing the second entry, simulating a
        // user tidying their config directory by hand.
        let missing_id = history
            .entries()
            .iter()
            .find(|e| matches!(e.content, Content::Image { .. }))
            .unwrap()
            .id
            .clone();
        std::fs::remove_file(images.join(format!("{}.bin", missing_id.as_str()))).unwrap();

        let loaded = History::load_from(&index, &images).unwrap();
        assert_eq!(
            loaded.entries().len(),
            1,
            "the broken entry is dropped, not the whole load"
        );
        assert_eq!(
            loaded.entries()[0].content,
            Content::Text("survives".into())
        );
    }

    #[test]
    fn what_is_saved_is_what_comes_back_including_pins_and_image_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("history.toml");
        let images = dir.path().join("images");

        let mut history = History::new();
        history.record(text("plain text"), 10);
        history.record(image(vec![7u8; 4096]), 20);
        let image_id = history.entries()[0].id.clone();
        history.set_pinned(&image_id, true);

        history.save_to(&index, &images).unwrap();
        let loaded = History::load_from(&index, &images).unwrap();

        assert_eq!(loaded.entries().len(), 2);
        let reloaded_image = loaded.entries().iter().find(|e| e.id == image_id).unwrap();
        assert!(reloaded_image.pinned);
        assert_eq!(
            reloaded_image.content,
            Content::Image {
                bytes: vec![7u8; 4096],
                mime: Mime::new("image/png")
            }
        );
        let reloaded_text = loaded
            .entries()
            .iter()
            .find(|e| e.content == Content::Text("plain text".into()))
            .unwrap();
        assert_eq!(reloaded_text.copied_at, 10);
    }

    #[test]
    fn saving_removes_image_files_left_behind_by_evicted_entries() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("history.toml");
        let images = dir.path().join("images");

        let mut history = History::new();
        history.record(image(vec![1u8; 16]), 1);
        history.save_to(&index, &images).unwrap();
        let files_before: Vec<_> = std::fs::read_dir(&images).unwrap().collect();
        assert_eq!(files_before.len(), 1);

        history.remove(&history.entries()[0].id.clone());
        history.save_to(&index, &images).unwrap();

        let files_after: Vec<_> = std::fs::read_dir(&images).unwrap().collect();
        assert!(
            files_after.is_empty(),
            "orphaned image file must be cleaned up"
        );
    }
}
