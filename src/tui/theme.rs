//! Semantic color tokens for the TUI.
//!
//! One flat palette, named by role rather than by literal color, so render
//! code reads `theme::SUCCESS` instead of scattering bare `Color::Green`
//! (etc.) calls that mean the same thing but don't say so.

use ratatui::style::Color;

/// Single accent used for titles, keys, and selection markers.
pub const ACCENT: Color = Color::Cyan;
/// Border color for all panels.
pub const BORDER: Color = Color::DarkGray;
/// Background of the selected row in lists and tables. Deliberately a dark
/// grey rather than `Color::DarkGray`, which many terminals render bright
/// enough to wash out the row's own colors (commit hashes, graph art).
pub const SELECTION_BG: Color = Color::Indexed(236);
/// Background band behind a commit that is the branch's own work (ahead of its
/// comparison base). Deliberately darker than `SELECTION_BG` so the cursor row
/// still reads as the brighter of the two when it lands on a marked commit.
pub const OWN_COMMIT_BG: Color = Color::Indexed(234);
/// Solid fill behind dialog/modal overlays so they read as a card over the UI.
pub const DIALOG_BG: Color = Color::Black;
/// Border color for dialog/modal overlays (stronger than panel chrome).
pub const DIALOG_BORDER: Color = Color::Cyan;
/// Cycled by graph column so parallel branch lines stay distinguishable as
/// they run down the commit tree.
pub const GRAPH_COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Red,
];

/// A positive/complete state: a clean worktree, a staged/marked file,
/// "ours" in a conflict resolution, a merged branch.
pub const SUCCESS: Color = Color::Green;
/// A state that needs attention but isn't an error: a dirty worktree, an
/// untracked file, an unresolved conflict, a transient status message.
pub const WARNING: Color = Color::Yellow;
/// An error, a blocking condition, or a destructive/irreversible action: a
/// locked worktree, a failed command, discarding uncommitted changes.
pub const DANGER: Color = Color::Red;
/// Secondary emphasis, reserved for diff headers and similar accents.
pub const INFO: Color = Color::Magenta;

/// The conflict resolver's FINAL column: the text that will actually be
/// written. Neutral on purpose, so the lines inside it keep the color of the
/// side they came from and a mixed resolution reads as exactly that.
pub const RESULT: Color = Color::White;
