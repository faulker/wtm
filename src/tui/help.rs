//! The keybinding registry: one source of truth for both the footer hints and
//! the help panel.
//!
//! Every binding is declared once, as `const` data, with a short footer label
//! and a longer help description. `draw_footer` and `draw_help` both read from
//! here, so the two surfaces cannot drift apart the way the old pair of
//! hardcoded lists did.

use super::app::{Tab, View};

/// One key and what it does.
///
/// `short` is the footer label; `None` keeps the binding out of the footer,
/// where width is scarce, while still documenting it in the help panel. `long`
/// is the help panel's description.
pub struct Binding {
    pub key: &'static str,
    pub short: Option<&'static str>,
    pub long: &'static str,
}

/// Shorthand for a binding that appears in both the footer and the help panel.
const fn both(key: &'static str, short: &'static str, long: &'static str) -> Binding {
    Binding {
        key,
        short: Some(short),
        long,
    }
}

/// Shorthand for a binding the footer has no room for.
const fn help_only(key: &'static str, long: &'static str) -> Binding {
    Binding {
        key,
        short: None,
        long,
    }
}

/// A titled group of bindings within a help tab, plus any trailing prose that
/// isn't a keybinding (the status-code legend, the cherry-pick explanation).
pub struct Section {
    pub heading: &'static str,
    pub bindings: &'static [Binding],
    pub notes: &'static [&'static str],
}

/// The tabs of the help panel. `Basics` is both the landing tab for views with
/// no help of their own and the home of the global keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTab {
    Basics,
    Worktrees,
    Branches,
    Changes,
    Commits,
    Conflicts,
}

impl HelpTab {
    pub const ALL: [HelpTab; 6] = [
        HelpTab::Basics,
        HelpTab::Worktrees,
        HelpTab::Branches,
        HelpTab::Changes,
        HelpTab::Commits,
        HelpTab::Conflicts,
    ];

    pub fn title(self) -> &'static str {
        match self {
            HelpTab::Basics => "Basics",
            HelpTab::Worktrees => "Worktrees",
            HelpTab::Branches => "Branches",
            HelpTab::Changes => "Changes",
            HelpTab::Commits => "Commits",
            HelpTab::Conflicts => "Conflicts",
        }
    }

    /// The next tab, wrapping at the end.
    pub fn next(self) -> HelpTab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// The previous tab, wrapping at the start.
    pub fn prev(self) -> HelpTab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// The help tab that documents what the user is currently looking at, so
    /// opening help lands on the relevant page instead of a fixed one. Views
    /// with no help of their own (dialogs, wizards, confirmations) fall back to
    /// `Basics`.
    pub fn for_view(view: &View, tab: Tab) -> HelpTab {
        match view {
            View::List => match tab {
                Tab::Worktrees => HelpTab::Worktrees,
                Tab::Branches => HelpTab::Branches,
            },
            View::Diff { .. } | View::Commit { .. } | View::Stash { .. } => HelpTab::Changes,
            View::BranchCommits { .. }
            | View::CherryPick { .. }
            | View::Log { .. }
            | View::CommitDiff { .. } => HelpTab::Commits,
            View::MergePick { .. } => HelpTab::Branches,
            View::ConflictResolver { .. } => HelpTab::Conflicts,
            _ => HelpTab::Basics,
        }
    }
}

/// The sections shown on a help tab.
pub fn sections(tab: HelpTab) -> &'static [Section] {
    match tab {
        HelpTab::Basics => BASICS_SECTIONS,
        HelpTab::Worktrees => WORKTREES_SECTIONS,
        HelpTab::Branches => BRANCHES_SECTIONS,
        HelpTab::Changes => CHANGES_SECTIONS,
        HelpTab::Commits => COMMITS_SECTIONS,
        HelpTab::Conflicts => CONFLICTS_SECTIONS,
    }
}

// ---------------------------------------------------------------------------
// Bindings. The footer also renders these directly, so the order here is the
// order the hints appear in.
// ---------------------------------------------------------------------------

pub const GLOBAL: &[Binding] = &[
    help_only("⇥ Tab", "switch between the Worktrees and Branches tabs"),
    help_only("?", "help for whatever you're looking at"),
    help_only("F1", "the same help, but works while typing too"),
    help_only("r", "refresh"),
    help_only("q", "quit, or step back out of a sub-view"),
    help_only("Esc", "cancel or close the current dialog"),
];

pub const WORKTREES: &[Binding] = &[
    both("⇥", "branches", "switch to the Branches tab"),
    help_only("↑/↓ or j/k", "select worktree"),
    both(
        "Enter",
        "changes",
        "browse changes per file (diff, stash, revert)",
    ),
    both("n", "new", "new worktree (new branch or existing branch)"),
    both(
        "b",
        "switch branch",
        "switch the selected worktree to another branch (local or remote), or type a new name to create one",
    ),
    help_only("u", "update: merge the default branch into the worktree"),
    both(
        "c",
        "commit",
        "commit (pick files, all selected by default)",
    ),
    help_only("o", "edit repo settings"),
    help_only("e", "run the open command"),
    both("s", "stash", "stash manager (stash/pop/apply/drop)"),
    both("p", "pull", "pull (fast-forward) the worktree"),
    both("⇧P", "push", "push the worktree"),
    both("f", "fetch", "fetch all remotes"),
    both("l", "log", "commit log"),
    both(
        "d",
        "delete",
        "delete worktree (folder, or folder + branch)",
    ),
    both(
        "⇧R",
        "rename",
        "rename the worktree (renames its branch and moves the folder)",
    ),
    help_only("r", "refresh the list"),
    both("?", "help", "show this help"),
    both("q", "quit", "quit"),
];

pub const BRANCHES: &[Binding] = &[
    both("⇥", "worktrees", "switch to the Worktrees tab"),
    help_only("↑/↓ or j/k", "select branch"),
    both(
        "Enter",
        "commits / cherry-pick",
        "view the branch's commits (then cherry-pick)",
    ),
    help_only("m", "merge the branch into a worktree of your choosing"),
    both(
        "c",
        "check out in a worktree",
        "check the branch out in a new worktree",
    ),
    both(
        "n",
        "new branch (no worktree)",
        "create a branch only (no worktree)",
    ),
    both(
        "f",
        "fetch",
        "fetch all remotes (refreshes every ahead/behind)",
    ),
    both("p", "pull", "fast-forward the branch onto its upstream"),
    both("d", "delete", "delete the selected branch (f to force)"),
    both("⇧R", "rename", "rename the selected branch"),
    both("?", "help", "show this help"),
    both("q", "quit", "quit"),
];

pub const DIFF: &[Binding] = &[
    help_only("↑/↓ or j/k", "move the file cursor"),
    help_only("⇧↑/⇧↓", "scroll the diff (or mouse wheel)"),
    help_only("←/→ or h/l", "collapse/expand the folder (Enter toggles)"),
    both("Space", "mark", "mark a file (or folder) for commit"),
    help_only("a", "mark or unmark every file"),
    both("c", "commit", "commit the marked files"),
    both("s", "stash file", "stash the highlighted file"),
    both("⇧S", "stash marked", "stash every marked file"),
    both(
        "⇧R",
        "revert",
        "revert the file to its last committed state",
    ),
    both("d", "delete", "delete the file from the worktree"),
    both("i", "ignore", "add the file or a glob to .gitignore"),
    both("t", "tree/flat", "toggle folder tree vs. flat file list"),
    both("Tab", "commit", "jump to the commit dialog"),
    both("?", "help", "show this help"),
    both("q", "back", "back to the worktree list"),
];

pub const COMMIT_FILES: &[Binding] = &[
    both("↑/↓", "file", "move the file cursor"),
    both("Space", "toggle", "include or exclude the highlighted file"),
    both("a", "all/none", "include or exclude every file"),
    both("Tab", "message", "jump to the commit message"),
    both("Enter", "commit", "commit the included files"),
    both("Esc", "cancel", "cancel without committing"),
];

pub const STASH_LIST: &[Binding] = &[
    both("↑/↓", "select", "select a stash entry"),
    both("s", "stash", "stash the worktree's current changes"),
    both("p", "pop", "pop the selected stash (apply, then drop)"),
    both("a", "apply", "apply the selected stash, keeping it"),
    both("x", "drop", "drop the selected stash"),
    both("Esc", "close", "close the stash manager"),
];

pub const BRANCH_COMMITS: &[Binding] = &[
    both("↑/↓", "select", "move the commit cursor"),
    both("Space", "mark commit", "mark a commit for cherry-pick"),
    both("a", "all/none", "mark or unmark every commit"),
    both(
        "Enter",
        "cherry-pick",
        "cherry-pick marked commits into a worktree",
    ),
    both(
        "v / →",
        "browse files",
        "view the highlighted commit's changed files and diffs",
    ),
    both(
        "t",
        "tree/flat",
        "switch between the commit tree and a flat list",
    ),
    both("?", "help", "show this help"),
    both("q", "back", "back to the branches list"),
];

pub const COMMIT_DIFF: &[Binding] = &[
    both("↑/↓ or j/k", "file", "move between the commit's changed files"),
    help_only("⇧↑/⇧↓", "scroll the diff (or mouse wheel)"),
    help_only("←/→ or h/l", "collapse/expand the folder (Enter toggles)"),
    both("t", "tree/flat", "toggle folder tree vs. flat file list"),
    both("?", "help", "show this help"),
    both("q", "back", "back to the commit list"),
];

pub const RESOLVER: &[Binding] = &[
    both("←/→", "file", "move between conflicted files"),
    both("↑/↓", "hunk", "move between hunks in the file"),
    both(
        "o/t",
        "ours/theirs",
        "keep ours (current branch) / theirs (incoming) for the hunk",
    ),
    both("b/⇧B", "both", "keep both (ours first / theirs first)"),
    both(
        "⇧O/⇧T",
        "whole file",
        "take the whole file from ours (current) / theirs (incoming)",
    ),
    both("e", "edit", "edit the merged result by hand"),
    both("w", "stage", "stage the file with your chosen resolutions"),
    both("c", "complete", "complete the merge (commit)"),
    both("x", "abort", "abort the merge"),
    help_only("?", "show this help"),
    both("q", "back", "back to the list"),
];

// ---------------------------------------------------------------------------
// Sections per tab.
// ---------------------------------------------------------------------------

const BASICS_SECTIONS: &[Section] = &[
    Section {
        heading: "keys that work everywhere",
        bindings: GLOBAL,
        notes: &[],
    },
    Section {
        heading: "changes view status codes  (col 1 = staged · col 2 = working tree)",
        bindings: &[],
        notes: &[
            "M modified · A added · D deleted · R renamed · C copied",
            "?? untracked · UU conflict (both sides changed)",
            "e.g.  ' M' edited, unstaged · 'M ' staged · 'MM' staged + more edits · 'A ' new file staged",
        ],
    },
];

const WORKTREES_SECTIONS: &[Section] = &[Section {
    heading: "worktrees tab",
    bindings: WORKTREES,
    notes: &[],
}];

const BRANCHES_SECTIONS: &[Section] = &[Section {
    heading: "branches tab",
    bindings: BRANCHES,
    notes: &[],
}];

const CHANGES_SECTIONS: &[Section] = &[
    Section {
        heading: "changes (diff) view  (Worktrees tab → Enter)",
        bindings: DIFF,
        notes: &[],
    },
    Section {
        heading: "commit dialog  (c)",
        bindings: COMMIT_FILES,
        notes: &["while typing the message, F1 opens this help ('?' types a '?')"],
    },
    Section {
        heading: "stash manager  (s)",
        bindings: STASH_LIST,
        notes: &[],
    },
];

const COMMITS_SECTIONS: &[Section] = &[
    Section {
        heading: "commits view  (Branches tab → Enter)",
        bindings: BRANCH_COMMITS,
        notes: &[
            "cherry-pick: pick a target worktree, then choose to commit",
            "directly (keeping the messages) or just load the changes.",
        ],
    },
    Section {
        heading: "commit browser  (log or commits → Enter/v)",
        bindings: COMMIT_DIFF,
        notes: &[
            "read-only: browse the files a commit changed and their diffs.",
            "the log view (worktree 'l') opens it with Enter.",
        ],
    },
];

const CONFLICTS_SECTIONS: &[Section] = &[Section {
    heading: "conflict resolver  (after a merge/update conflict)",
    bindings: RESOLVER,
    notes: &[],
}];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tab_has_content() {
        for tab in HelpTab::ALL {
            let secs = sections(tab);
            assert!(!secs.is_empty(), "{} has no sections", tab.title());
            for s in secs {
                assert!(
                    !s.bindings.is_empty() || !s.notes.is_empty(),
                    "{} / {} is empty",
                    tab.title(),
                    s.heading
                );
            }
        }
    }

    #[test]
    fn tabs_cycle_both_ways() {
        assert_eq!(HelpTab::Basics.next(), HelpTab::Worktrees);
        assert_eq!(HelpTab::Basics.prev(), HelpTab::Conflicts);
        assert_eq!(HelpTab::Conflicts.next(), HelpTab::Basics);
        // Round trip through every tab lands back where it started.
        let mut t = HelpTab::Basics;
        for _ in 0..HelpTab::ALL.len() {
            t = t.next();
        }
        assert_eq!(t, HelpTab::Basics);
    }
}
