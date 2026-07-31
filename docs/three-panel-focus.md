# Three-panel Worktrees focus model (plan step 1)

Accepted decision for implementing the Worktrees three-panel layout.

## Product decisions

1. Layout switch via settings (repo `.wtm.toml` + global via `wtm config set` / `-g`).
2. Default = `two_panel` (current behavior).
3. When three-panel is on, hide the Changes tab (skip in tab cycle / tab bar).
4. Match existing keybindings; do not invent a third focus for the diff.

## Focus / navigation

Two-state focus only (`WorktreesFocus { List, Files }`). Diff never takes focus; remote-scroll with Shift-arrows / `J`/`K`/`H`/`L` and pointer-position wheel, same as Changes tab.

| Context | Key | Action |
| --- | --- | --- |
| List focused | Enter | Focus file list (replaces open Changes tab) |
| Files focused | q / Esc | Return focus to worktree list (quit only from List) |
| Files focused | other keys | Same as `on_changes_tab_key` |
| List focused | other keys | Same as `on_worktrees_tab_key` |
| Either | mouse click on panel | Steal focus to that panel |

## State

- `WorktreesFocus` on `App`, default `List`, reset on tab switch / refresh.
- Reuse `ChangesTab` + `draw_diff` for bottom panes.
- Background status reload on worktree cursor move with stale-drop token (like `preview_pending`).

## Settings (step 3)

- Key: `worktrees_layout`, values `"two_panel" | "three_panel"`, default `"two_panel"`.
- Mirror `diff_theme` patterns in `config.rs` / `settings.rs`; Settings-tab cycle row saves global like theme.

## Hide Changes tab

- Skip in `cycle_tab`, filter in `draw_tab_bar`.
- Redirect `open_changes_tab` / `open_changes_tab_at` / `select_tab(Changes)` to focus file panel.

## Ready-for-step-2 checklist

- [x] Focus model decided
- [x] Implement `WorktreesFocus` + key dispatch
- [x] Three-panel render in `draw_worktrees_tab`
- [x] Reuse Changes panes; stale-drop status load
- [x] Mouse / wheel / help / min-height fallback

Step 2 landed the `worktrees_layout` config key (repo/global `.wtm.toml`) but
not its `wtm config get/set/unset` or Settings-tab rows, nor a help-panel
section for the focus keys; those were step 3.

## Step 3 checklist

- [x] `wtm config get/set/unset worktrees_layout` (`src/settings.rs`)
- [x] Settings-tab row to cycle `worktrees_layout` (`src/tui/config_editor.rs`)
- [x] Footer hints for both focus states (`WORKTREES_THREE_PANEL` /
      `WORKTREE_FILES` in `src/tui/help.rs`)
- [x] Help-panel (`?`) section documenting the three-panel focus keys, and a
      note on the two-panel Worktrees section pointing to it, so the static
      help panel is accurate in both layouts (`src/tui/help.rs`)

Step 3 is done.
