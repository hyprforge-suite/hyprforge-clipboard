//! Which window the popup is about to paste into, and what that means
//! for the shortcut it should send.
//!
//! This is the one place in the popup that is allowed to know both
//! things at once: how to ask Hyprland what has focus (`hyprctl`), and
//! that a terminal emulator wants Ctrl+Shift+V instead of Ctrl+V.
//! `hyprforge-clipboard` is deliberately kept ignorant of both — see its
//! `paste` module doc — so this module is the seam between "Hyprland
//! knowledge" and "how to press a key", the same split every other
//! Hyprland-flavoured decision in this suite keeps (CLAUDE.md's "the
//! model is plain data, and the decisions are pure functions over it").

use hyprforge_clipboard::Shortcut;
use hyprforge_process::{output, TIMEOUT};
use std::process::Command;

/// The window that had keyboard focus, read *before* this popup's own
/// layer surface takes it away.
///
/// Only `class` is kept. `hyprctl activewindow -j` also reports a
/// `title`, but this crate has no use for it: `class` is a stable
/// application identifier a window manager assigns, which is all
/// [`paste_shortcut`] and [`display_name`] need, while a title is chosen
/// by whatever application set it and can contain anything at all — a
/// field this crate never reads is also a field nobody can accidentally
/// log.
pub struct FocusedWindow {
    pub class: String,
}

/// `hyprctl activewindow -j`, read once at startup — before the popup's
/// own surface exists and steals focus for itself. Calling this any
/// later would only ever answer "the popup", which is exactly the wrong
/// answer; see this crate's `main.rs` for where this is called relative
/// to `ClipMenu::run`.
///
/// `None` covers every way this can fail to answer: no `hyprctl`, no
/// compositor, unparsable JSON, or a genuinely empty desktop (`hyprctl`
/// reports `{}` with no `class` key when nothing has focus) — all of
/// them mean "don't know, and there may be nothing to know", so the
/// caller falls back to the ordinary shortcut rather than guessing.
pub fn focused_window() -> Option<FocusedWindow> {
    let result = output(Command::new("hyprctl").args(["activewindow", "-j"]), TIMEOUT).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).ok()?;
    let class = value.get("class")?.as_str()?.to_string();
    Some(FocusedWindow { class })
}

/// Known terminal-emulator window classes, matched after lower-casing
/// and taking the last `.`-separated segment (so a reverse-DNS class
/// like `com.mitchellh.ghostty` or `org.gnome.Terminal` is compared
/// against its own application name, not the whole reversed domain).
///
/// A class matches if it equals one of these outright, or starts with
/// one of them followed by `-` or `_` — which is what catches a variant
/// like `foot-extra` without also catching an unrelated class that
/// merely happens to contain the same letters somewhere in its middle.
const KNOWN_TERMINAL_NAMES: &[&str] =
    &["ghostty", "foot", "kitty", "alacritty", "konsole", "terminal", "urxvt", "rxvt", "rio", "contour", "st"];

/// Chooses the paste shortcut for a focused window's class.
///
/// Terminal emulators bind Ctrl+V to something else (historically a
/// control code) and use Ctrl+Shift+V for paste instead; everything else
/// uses plain Ctrl+V. This is a plain function over a string — no
/// `hyprctl`, no compositor — so the whole decision can be pinned by a
/// test without anything running.
///
/// # The matching rule, and what it will and will not catch
///
/// Two checks, either of which is enough to call `class` a terminal:
///
/// 1. The last `.`-separated segment, lower-cased, is exactly one of
///    [`KNOWN_TERMINAL_NAMES`], or starts with one of them followed by
///    `-`/`_` (catching `foot-extra`, a hypothetical `kitty_wayland`,
///    and so on).
/// 2. The lower-cased class contains the substring `"term"` anywhere at
///    all — this is what catches `xterm`, `org.wezfurlong.wezterm` /
///    `WezTerm`, `org.gnome.Terminal` (also caught by rule 1, since
///    `terminal` is in the known list), and `terminator`, without this
///    module having to name every terminal emulator that happens to spell
///    its name that way.
///
/// Rule 2 is the loose one, and it is loose on purpose: a class this
/// crate has never heard of but that contains "term" is more likely than
/// not to be a terminal, and sending Ctrl+Shift+V to a real Ctrl+V
/// application is a wrong keystroke, not a destructive one. It **will**
/// produce a false positive on any non-terminal class that happens to
/// contain "term" — a hypothetical `com.example.TermsOfService` viewer,
/// say — which would receive Ctrl+Shift+V instead of the Ctrl+V it
/// actually wants. Rule 1's short, generic-looking names (`st`, `rio`)
/// are deliberately matched by exact equality (or a `-`/`_`-prefixed
/// variant) rather than substring, precisely so they do **not** false-
/// positive on `startpage`, `trio`, `mario`, and the like.
///
/// Case-insensitive throughout: `Alacritty`, `ALACRITTY` and `alacritty`
/// all match.
pub fn paste_shortcut(class: &str) -> Shortcut {
    let lower = class.to_lowercase();
    let last_segment = lower.rsplit('.').next().unwrap_or(&lower);

    let known = KNOWN_TERMINAL_NAMES.iter().any(|&name| {
        last_segment == name
            || last_segment.starts_with(&format!("{name}-"))
            || last_segment.starts_with(&format!("{name}_"))
    });
    let generic_term = lower.contains("term");

    if known || generic_term {
        Shortcut::CtrlShiftV
    } else {
        Shortcut::CtrlV
    }
}

/// A short, human-readable name for a window class — "paste into
/// Ghostty" reads better than "paste into com.mitchellh.ghostty". Purely
/// cosmetic: nothing about *which shortcut* to send depends on this, only
/// what the popup's header shows.
///
/// Takes the class's last `.`-separated segment (matching how
/// [`paste_shortcut`] itself reads a reverse-DNS class) and capitalizes
/// its first character; a class with no letters at all is shown
/// unchanged rather than producing an empty label.
pub fn display_name(class: &str) -> String {
    let segment = class.rsplit('.').next().unwrap_or(class);
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => segment.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_terminal(class: &str) {
        assert_eq!(paste_shortcut(class), Shortcut::CtrlShiftV, "{class:?} should be treated as a terminal");
    }

    fn assert_not_terminal(class: &str) {
        assert_eq!(paste_shortcut(class), Shortcut::CtrlV, "{class:?} should not be treated as a terminal");
    }

    #[test]
    fn every_named_terminal_class_gets_ctrl_shift_v() {
        for class in [
            "com.mitchellh.ghostty",
            "foot",
            "kitty",
            "Alacritty",
            "org.wezfurlong.wezterm",
            "konsole",
            "xterm",
            "st",
            "WezTerm",
            "org.gnome.Terminal",
            "terminator",
            "urxvt",
            "rio",
            "contour",
        ] {
            assert_terminal(class);
        }
    }

    #[test]
    fn ordinary_applications_get_plain_ctrl_v() {
        for class in ["google-chrome", "org.mozilla.firefox", "code"] {
            assert_not_terminal(class);
        }
    }

    #[test]
    fn an_empty_class_falls_back_to_plain_ctrl_v() {
        assert_not_terminal("");
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_terminal("GHOSTTY");
        assert_terminal("Kitty");
        assert_terminal("ALACRITTY");
        assert_not_terminal("GOOGLE-CHROME");
    }

    /// A variant of a known terminal's class, suffixed rather than exact
    /// — the case the module doc calls out `foot-extra` for by name.
    #[test]
    fn a_hyphenated_variant_of_a_known_terminal_is_still_caught() {
        assert_terminal("foot-extra");
        assert_terminal("kitty_wayland");
    }

    /// The short, generic-looking names are matched exactly, not as a
    /// loose substring — otherwise `rio` would catch `trio` and `st`
    /// would catch `startpage`, exactly the false positives the module
    /// doc says this rule is built to avoid.
    #[test]
    fn short_terminal_names_do_not_loosely_match_unrelated_classes() {
        assert_not_terminal("trio");
        assert_not_terminal("startpage");
        assert_not_terminal("mario");
    }

    /// The documented false positive: a non-terminal class that happens
    /// to contain "term" is caught by the loose heuristic anyway. This is
    /// not a bug — it is the trade-off the module doc names explicitly —
    /// but it is worth pinning so nobody "fixes" rule 2 without reading
    /// why it is there.
    #[test]
    fn a_non_terminal_class_containing_term_is_a_known_false_positive() {
        assert_terminal("com.example.TermsOfService");
    }

    // --- `display_name`: cosmetic only, never consulted by `paste_shortcut`.

    #[test]
    fn a_reverse_dns_class_shows_only_its_last_segment_capitalized() {
        assert_eq!(display_name("com.mitchellh.ghostty"), "Ghostty");
    }

    #[test]
    fn a_plain_class_is_capitalized_as_is() {
        assert_eq!(display_name("foot"), "Foot");
        assert_eq!(display_name("Alacritty"), "Alacritty");
    }

    #[test]
    fn an_empty_class_produces_an_empty_name_rather_than_panicking() {
        assert_eq!(display_name(""), "");
    }
}
