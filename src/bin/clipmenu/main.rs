//! A clipboard history popup that appears where the mouse is, shows one
//! item once, and exits.
//!
//! This is a per-invocation program, launched by a keybind, not a
//! long-running daemon. Two reasons, not one:
//!
//! - A popup that lives forever is a window manager's problem: staying
//!   out of the way when unfocused, reappearing on the right output
//!   when the cursor has moved, surviving a monitor being unplugged.
//!   None of that is this program's job, and a daemon would have to
//!   solve all of it just to sit idle between invocations.
//! - A short-lived process cannot leak a stuck layer surface. It takes
//!   `KeyboardInteractivity::Exclusive` — CLAUDE.md is explicit that a
//!   keybind's compositor belongs to whoever is standing at the
//!   keyboard — and the one guarantee that has to hold no matter how
//!   this exits (chosen, cancelled, killed, panicked) is that the
//!   surface it made goes away with the process. A daemon that misjudged
//!   its own state and left the popup up, holding exclusive keyboard
//!   focus, would be a stuck keyboard on a compositor with no window
//!   manager left to blame.
//!
//! `hyprforge-clipboard` owns the history and its own daemon watches the
//! compositor's clipboard to fill it; this only ever reads it (see
//! `crates/hyprforge-clipboard/src/store.rs`) and, on Enter, writes the
//! chosen entry back out through `chooser::Chooser` — see `chooser::Wired`
//! for where that plugs into `hyprforge-clipboard`'s write side.
//!
//! The layer-shell surface, the event loop, pointer/keyboard handling,
//! placement and the single-instance lock all live in `hyprforge-popup`
//! now — see that crate's own module doc for why, and `surface::ClipApp`
//! for the seam this binary plugs a clipboard history into it through.

mod chooser;
mod editor;
mod geometry;
mod kind;
mod model;
mod surface;
mod target;
mod thumbnail;
mod view;

use geometry::Layout;
use hyprforge_popup::geometry::Size;
use model::{HistoryState, ListGeometry, Model};
use surface::{ChoiceOutcome, ClipApp};

/// The list's geometry for `layout` — what `Model` scrolls and stacks
/// with, taken from the same `Layout` the view draws and the hit-test
/// measures, never a second set of numbers. The bug that first motivated
/// deriving this: the model once built a hardcoded 24 rows regardless of
/// the popup's height, so nothing a pointer touched was where the drawn
/// rows were.
fn list_geometry(layout: &Layout) -> ListGeometry {
    ListGeometry {
        viewport_height: layout.viewport_height(),
        header_height: layout.header_height,
        row_height: layout.row_height,
        spacing: layout.row_spacing,
    }
}

/// The name this popup's single-instance lock is filed under — see
/// `hyprforge_popup::singleton`'s own doc for why a name, not a shared
/// lock, and why an `flock` rather than a name match or a PID file.
const LOCK_NAME: &str = "hyprforge-clipmenu.lock";


fn main() -> std::process::ExitCode {
    // Only one popup at a time: a keybind pressed twice while one is
    // already open must leave the first alone and exit quietly, not
    // start a second process.
    let lock_path = hyprforge_popup::singleton::lock_path(LOCK_NAME);
    let _lock = match hyprforge_popup::singleton::acquire(&lock_path) {
        Ok(Some(lock)) => Some(lock),
        // Someone already has it: this is a keybind pressed twice, not
        // an error — silent and successful, exactly as if this process
        // had never run.
        Ok(None) => return std::process::ExitCode::SUCCESS,
        // Couldn't even check — never let a broken lock lock out every
        // future popup; proceed without one.
        Err(e) => {
            eprintln!("couldn't set up the single-instance lock ({e}) — continuing anyway");
            None
        }
    };

    // Read *before* this popup's own layer surface ever exists: once it
    // takes `KeyboardInteractivity::Exclusive`, the focused window *is*
    // this popup, and asking `hyprctl` afterward would only ever answer
    // that. See `target::focused_window`'s own doc.
    let focused = target::focused_window();
    let paste_shortcut =
        focused.as_ref().map(|w| target::paste_shortcut(&w.class)).unwrap_or(hyprforge_clipboard::Shortcut::CtrlV);
    let paste_target_label = focused.as_ref().map(|w| target::display_name(&w.class));

    let monitors = hyprforge_popup::monitors();
    if monitors.is_empty() {
        eprintln!("couldn't read any monitors from hyprctl — is Hyprland running?");
        return std::process::ExitCode::FAILURE;
    }
    // Resolved before placing: the popup's height follows the theme's
    // font (see `geometry::Layout::for_font_size`), and placement needs
    // the real size to keep the whole popup on screen.
    let mut theme = hyprforge_appearance::look::resolve();
    theme.font_size = theme.drawable_font_size();
    let layout = Layout::for_font_size(theme.font_size);

    let popup_size = Size { width: layout.width, height: layout.height };
    let Some(placement) = hyprforge_popup::place(&monitors, hyprforge_popup::cursor_position(), popup_size) else {
        eprintln!("couldn't work out where to place the popup");
        return std::process::ExitCode::FAILURE;
    };

    let history = HistoryState::from_result(hyprforge_clipboard::History::load());
    let mut model = Model::new(history);
    model.set_paste_target(paste_target_label);
    model.set_geometry(list_geometry(&layout));

    let connection = match hyprforge_popup::Connection::connect_to_env() {
        Ok(connection) => connection,
        Err(e) => {
            eprintln!("couldn't connect to the compositor: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // **The seam**: `chooser::Wired` is where `hyprforge-clipboard`'s
    // write-side traits (`ClipboardWriter`, `PasteSynthesizer`) plug in —
    // see its doc comment. Every test in this crate instead drives
    // `chooser::mock::MockChooser`, so nothing here depends on a real
    // compositor to be checked.
    let chooser = match chooser::Wired::connect() {
        Ok(chooser) => chooser,
        Err(e) => {
            eprintln!("couldn't set up pasting: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let app = ClipApp::new(model, chooser, editor::Wired, paste_shortcut);

    match hyprforge_popup::Popup::run(connection, placement, app, theme) {
        Ok(hyprforge_popup::Outcome::App(ChoiceOutcome::Chosen | ChoiceOutcome::Cancelled)) => {
            std::process::ExitCode::SUCCESS
        }
        Ok(hyprforge_popup::Outcome::Closed | hyprforge_popup::Outcome::Disconnected) => {
            eprintln!("the popup closed unexpectedly");
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression test for the bug that started all of this: the
    /// model's viewport has to be *derived* from the popup's layout, not
    /// a separate hardcoded number that can drift out of step with it. A
    /// history far longer than the viewport must still build only about
    /// as many lines as physically fit — wired exactly the way `main`
    /// wires it, against a history long enough that a stale hardcoded
    /// window would show a different number.
    #[test]
    fn the_models_viewport_is_derived_from_the_popups_actual_layout() {
        let layout = Layout::for_font_size(15.0);
        let fit = (layout.viewport_height() / layout.row_height).floor() as usize;

        let history = model::HistoryState::Loaded((0..200).map(|i| test_entry(&format!("entry {i}"))).collect());
        let mut model = Model::new(history);
        model.set_geometry(list_geometry(&layout));

        let built = model.stack().visible(model.scroll_offset(), layout.viewport_height()).len();
        assert!(built >= fit, "must build at least the rows that fully fit ({fit}), got {built}");
        assert!(built <= fit + 2, "and only one partial row at each edge beyond them: {built} built for {fit}");
    }

    fn test_entry(text: &str) -> hyprforge_clipboard::Entry {
        let content = hyprforge_clipboard::Content::Text(text.to_string());
        hyprforge_clipboard::Entry {
            id: hyprforge_clipboard::EntryId::of(&content),
            content,
            copied_at: 0,
            pinned: false,
        }
    }

    #[test]
    fn a_non_finite_font_size_falls_back_rather_than_panicking_the_renderer() {
        let theme = hyprforge_look::Theme { font_size: f32::NAN, ..hyprforge_look::Theme::default() };
        assert_eq!(theme.drawable_font_size(), 15.0);
        let theme = hyprforge_look::Theme { font_size: 0.0, ..hyprforge_look::Theme::default() };
        assert!(theme.drawable_font_size() >= 6.0);
    }
}
