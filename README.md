# wtm — worktree manager

A friendly top-level interface for git, built for working with AI agents on multiple branches at once.

Git only lets you have one branch checked out in a folder. A **worktree** is a second folder for the same repo, sitting on a different branch, so you (or an agent) can work on several things without stashing or switching. wtm creates those folders, copies in the files git ignores (your `.env`, and so on), runs setup like `npm install`, and then lets you commit, pull, push, stash, merge, and resolve conflicts by **worktree name** instead of paths and flags.

[Project page on sleepymagpie.com](https://sleepymagpie.com/tools/wtm.html)

Three ways to use it:

- **TUI**: `wtm` with no arguments, inside a repo
- **CLI**: scriptable subcommands, all with `--json` for agents
- **MCP**: `wtm mcp` serves the same operations as tools over stdio

![The Worktrees tab: a table of worktrees with change counts, ahead/behind, and paths, over a preview of the selected worktree's changed files](docs/images/tui-worktrees.png)

## Quick start

Requires `git` on your PATH.

Install the latest release (macOS Apple Silicon/Intel, Linux x86_64/ARM64):

```sh
curl -fsSL https://raw.githubusercontent.com/faulker/wtm/main/install.sh | bash
```

That downloads the matching binary from [GitHub Releases](https://github.com/faulker/wtm/releases), verifies its SHA-256 checksum, and installs it to `~/.local/bin/wtm` (override with `WTM_INSTALL_DIR`). If `wtm` is not found after install, add `~/.local/bin` to your PATH.

Or unpack a release tarball by hand:

```sh
tar -xzf wtm-vX.Y.Z-aarch64-apple-darwin.tar.gz
mv wtm ~/.local/bin/
```

To build from source you also need Rust (edition 2024 toolchain):

```sh
cargo build --release
cp target/release/wtm ~/.local/bin/
```

Then, in any git repo:

```sh
cd your-repo
wtm
```

The first time, a setup wizard asks where worktree folders should live, which ignored files to copy into them, and what to run once they exist. After that you get the worktree list.

From there:

| Key | What it does |
| --- | --- |
| `n` | new worktree (type a branch name, or pick an existing one) |
| `o` | open the selected worktree in your editor |
| `c` | commit |
| `Enter` | see that worktree's changed files and diff |
| `?` | help for the screen you're on (`F1` while typing, since `?` is a character then) |
| `q` | quit |

Press `Tab` / `⇧Tab` to move between **Worktrees**, **Changes**, **Branches**, **Stash**, and **Settings**. The mouse works too: click a tab or a row to select it.

That's enough to start. The rest of this page is a reference.

## Settings

Every repo needs a `.wtm.toml` in its root before worktree commands work. Until that file exists, `create`, `list`, and friends refuse with a pointer to `wtm init` (MCP tool calls report the same error). Two ways to create it:

- **`wtm init`**: a guided wizard in the terminal. It first offers to clone settings from another repo (give a path to the repo or its `.wtm.toml`), otherwise it asks the three questions below.
- **run `wtm` with no arguments**: in an uninitialized repo the TUI opens the same wizard. It starts on a welcome screen, then offers two routes: answer three questions, or copy settings from a repo that already uses wtm (`Tab` on the path prompt opens a file browser).

The three questions are where worktree folders should live (each choice shows the path it resolves to), which files to copy into them, and what to run once they exist. The last two arrive **pre-filled from your repo**: a `.env` in the root is suggested for copying, and a lockfile (`pnpm-lock.yaml`, `package-lock.json`, `uv.lock`, `Gemfile.lock`, `go.mod`, and friends) suggests the matching install command. Every screen says why it's asking, `Esc` steps back one screen keeping your answers, and both routes end on a review screen where you can still edit everything before the file is written.

```sh
wtm init
```

![The Settings tab: worktree_dir, open_command, setup.copy, and setup.run, each with a hint line, plus a live preview of where worktrees will land](docs/images/tui-settings.png)

The Settings tab (`Tab` until you land there, or click **Settings**) edits the same fields, with a hint under each one and a live preview of where new worktrees will land. `Enter` edits the selected row and writes immediately (`Esc` cancels an in-progress edit). Below the location preview it shows a sample of the selected `diff_theme` (cycling the row updates the palette right away), then the running version and whether an update is waiting, with a `[ check for updates now ]` row.

To view or change settings later, no TOML editing required:

```sh
wtm config                       # show every setting, its value, and where it came from
wtm config get worktree_dir
wtm config set worktree_dir inside
wtm config set open_command "open {path}, cursor {path}"
wtm config set setup.copy ".env, .env.local"
wtm config set --global auto_update_check false   # stop checking for new releases
wtm config set --global diff_theme ocean          # Eighties (default), Mocha, Ocean, Solarized, GitHub
wtm config set --global worktrees_layout three_panel   # Worktrees tab: two_panel (default) or three_panel
wtm config set --global branches_refresh_mins 10       # Branches tab cache timeout in minutes (default 10)
wtm config set --global diff_line_numbers false        # hide the diff pane's line-number gutter (on by default)
wtm config unset setup.copy      # back to the default (or the global value)
wtm config path                  # where the config files live
```

`diff_theme` and `worktrees_layout` can also be cycled on the Settings tab. Both are saved globally, like `auto_update_check`. `branches_refresh_mins` (default 10) is how long the Branches tab keeps its cached list; `r` on that tab always reloads immediately. `diff_line_numbers` (on by default) toggles the line-number gutter in the diff pane.

### Where worktrees go: `worktree_dir`

Pick a predefined rule or give a path yourself:

| Value | Worktrees end up in |
| --- | --- |
| `sibling` (default) | `../<repo>-worktrees`, next to the repo |
| `inside` | `.worktrees/` inside the repo (kept out of `git status` automatically) |
| `home` | `~/worktrees/<repo>` |
| any path | absolute, `~/...`, or relative to the repo root; `{repo}` expands to the repo folder name, e.g. `~/wt/{repo}` |

### Two config layers

Settings resolve per field: repo, then global, then built-in default.

- **Repo**: `.wtm.toml` in the repo root, applies to this repo only.
- **Global**: `~/.config/wtm/config.toml` (or `$XDG_CONFIG_HOME/wtm/config.toml`), applies to all your repos. Write to it with `wtm config set --global <key> <value>`.

`wtm config` shows which layer each value came from.

### The config file

`wtm init` and `wtm config set` maintain this for you (comments are preserved), but it's plain TOML if you'd rather edit by hand:

```toml
# "sibling", "inside", "home", or a path ({repo} = repo folder name)
worktree_dir = "sibling"
# Commands the TUI's `o` key runs for a worktree. A string, or an array whose
# entries are either a bare template or a `{ command, mode }` table;
# `{path}`, `{name}`, `{branch}`, and `{status}` expand before it runs.
# mode = "background" (default) spawns it detached and leaves wtm up;
# mode = "terminal" closes wtm and hands this terminal to the command.
# Commands in the *global* config are offered in every repo, alongside a
# repo's own list rather than instead of it.
open_command = ["open {path}", { command = "nvim {path}", mode = "terminal" }]
# Check GitHub for a newer wtm when the TUI starts. Usually set globally.
auto_update_check = true
# Diff syntax-highlight palette: eighties (default), mocha, ocean, solarized, github.
diff_theme = "eighties"
# Worktrees tab layout: two_panel (default) or three_panel. Usually set globally.
worktrees_layout = "two_panel"
# Line-number gutter in the diff pane. Usually set globally.
diff_line_numbers = true

[setup]
# Files copied from the main worktree into the new one (if they exist).
# Files in subfolders (e.g. "config/.env") land in the same subfolder.
copy = [".env", ".env.local"]
# Commands run inside the new worktree, in order. Stops at the first failure.
run = ["npm install"]
```

If a setup command fails, the worktree is kept so you can fix things by hand; `wtm create` reports the failure and exits with code 2.

Setup commands are interactive: with `wtm create` in a terminal they attach to your terminal directly, and in the TUI their output streams live into the progress window, where you can type a line and press `Enter` to answer a prompt. If a command hangs, press `Ctrl+C` twice in the TUI to kill it (the worktree itself is kept).

## CLI

Worktrees are addressed by branch name (or directory name when detached). Every command accepts `--json` for machine-readable output. Errors go to stderr as `{"error": "..."}` with a non-zero exit code.

```sh
wtm init [--force]                    # guided setup, writes .wtm.toml
wtm create <branch> [--from <base>]   # new worktree; creates the branch if needed, runs setup
wtm list                              # all worktrees with dirty count and ahead/behind
wtm remove <name> [--force] [--delete-branch]
wtm rename <name> <new-name>          # rename a worktree: renames its branch and moves the folder
wtm status <name>                     # changed files in a worktree
wtm diff <name>                       # unified diff of uncommitted changes
wtm path <name>                       # prints the path, e.g. cd $(wtm path feature-x)
wtm config [show|get|set|unset|path]  # view and change settings
wtm upgrade [--check]                 # update wtm itself to the latest release
wtm mcp                               # MCP server over stdio
```

`wtm create` also pulls down remote branches: when the branch only exists on a remote, it creates a local tracking branch from it instead of branching off HEAD.

Everyday git, addressed by worktree name:

```sh
wtm commit <name> -m <msg> [-b <body>] [--paths a,b]   # stage (everything, or just --paths) and commit
wtm stash push <name> [-m <msg>]           # stash changes, untracked files included
wtm stash list|pop|apply|drop <name> [--index N]
wtm move-changes <from> <to>               # move uncommitted changes into another worktree (destination must be clean)
wtm pull <name> [--rebase]                 # fast-forward only unless --rebase
wtm push <name> [--force-with-lease]       # publishes with -u origin when no upstream yet
wtm switch <name> <branch> [--create]      # check a different branch out in the worktree; a remote-only
                                           # branch becomes a local branch tracking the remote.
                                           # --create makes a new branch off HEAD when it doesn't exist
wtm log <name> [-n <count>]                # recent commits (default 20)
wtm fetch                                  # fetch all remotes, prune deleted branches
```

Branches (repo-wide, not tied to one worktree):

```sh
wtm branch list                            # branches with checkout, tracking, last commit
wtm branch create <name> [--from <ref>]    # branch without a worktree
wtm branch delete <name> [--force]         # local only by default; refuses if checked out in a worktree
                                           # --remote also deletes it on the remote, --remote-only just there
wtm branch archive <name> [--undo]         # hide it from wtm's listings without deleting it
wtm branch rename <old> <new>
wtm branch upstream <name> <origin/ref>    # change which remote branch it tracks (--unset to stop tracking)
wtm branch log <name> [-n <count>]         # a branch's commits without checking it out
wtm cherry-pick --into <name> <commit>...  # apply commits into a worktree (--no-commit to load only)
```

Merging, rebasing, updating, and resolving conflicts:

```sh
wtm merge <source> --into <name> [--no-ff] # merge a branch into a worktree's branch
wtm rebase <name> --onto <branch>          # replay a worktree's commits on top of a branch
wtm rebase <name> --continue               # finish the rebase once conflicts are resolved
wtm rebase <name> --skip                   # drop the commit it stopped on and carry on
wtm rebase <name> --abort                  # abandon the rebase, restore the worktree
wtm update <name> [--autostash]            # refresh default from upstream, then merge it in
                                           #   (fast-forwards in place when already on default;
                                           #   --autostash stashes local edits first, reapplies after)
wtm conflicts <name>                       # list conflicted files in the worktree
wtm conflicts <name> <file>                # inspect one file's conflict hunks (ours/theirs, --json)
wtm resolve <name> <file> --ours           # take our side of the whole file
wtm resolve <name> <file> --theirs         # take their side
wtm resolve <name> <file> --both           # keep both, ours then theirs on separate lines
wtm resolve <name> <file> --both-reversed  # keep both, theirs then ours
wtm merge --into <name> --continue [-m ..] # finish the resolved merge, rebase, or cherry-pick
wtm merge --into <name> --abort            # abandon it, restoring the worktree
```

The same conflict flow covers five sources: `merge`, `rebase`, `update`, `cherry-pick`, and `stash pop` each report `conflicted` with the file list and leave the tree in place to resolve. `resolve` each file (or hand-edit it and `git add`), then finish: `merge --continue` completes a merge, rebase, or cherry-pick (it auto-detects which), while a resolved stash pop finishes with `wtm stash drop <name>` (the conflicting pop keeps the stash). `wtm list` reports `conflicted` and `in_progress` for a worktree stopped mid-operation. Every command takes `--json`, so an agent can drive the whole loop.

> **Mid-rebase, git swaps the two sides.** Rebasing replays *your* commits on top of another branch, so during a rebase `--ours` is the branch you are rebasing onto and `--theirs` is your own commit being replayed. This is the opposite of a merge, and it catches people out. The TUI's resolver labels both sides explicitly and flags the swap.

## TUI

Run `wtm` inside a repo. If the repo isn't initialized yet, the setup wizard opens first (see [Settings](#settings)); once `.wtm.toml` exists you get the worktree list.

Each worktree shows its change count, ahead/behind, and a **FLAGS** column: `unpushed` / `pushed` / `behind` for where the branch's commits stand against the remote, `same` / `changed` / `outdated` vs the comparison base (recorded `[created_from]` in `.wtm.toml` when present, otherwise the repo default branch, with a merge-base fallback when that tip is missing), `✓merged` when fully merged into the default branch (safe to clean up), and `locked` for a locked worktree. Worktrees and Branches both flag `✓merged`.

The panel underneath the list shows the changed files of whichever worktree you have selected, so you can see what an agent has been up to without leaving the list. When there are more files than fit, `⇧↑`/`⇧↓` or the mouse wheel over the panel scroll it, and the border shows your position (`10-18/27`). Lists that scroll (worktrees, branches, stashes) mark the overflow with `▲`/`▼` in the left border. Clicking a file there opens it on the Changes tab.

A worktree's path is click-to-copy in both places it appears: the one in the header (the selected worktree's root) and the **PATH** cell on any row. The cell copies the whole path even where it is shown front-trimmed to fit, and the click still selects that row.

`b` switches the selected worktree to another branch. `⇧R` renames a worktree.

The TUI is meant to be left open all day, so it paces itself: it redraws on your input, keeps a fast (10/s) tick only while something is actually moving — a spinner, a background load — and drops to a slow tick once it has been sitting untouched, when the changed-file refresh that re-runs `git status` also backs off from every second to every fifteen. It asks the terminal for button and wheel reporting only, not the pointer-motion tracking most TUIs enable by default, so moving the mouse across the window doesn't wake it at all. Idle in a conflict resolver, that is about a quarter of the CPU it used to burn.

### Three-panel layout

The default Worktrees tab is `two_panel` (list plus the changed-file preview). `wtm config set --global worktrees_layout three_panel` (or the `worktrees_layout` row on the Settings tab) swaps that for a **three-panel** layout: a compact scrollable worktree list on top, and the Changes tab's file list and syntax-highlighted diff filling the space below it.

When the highlighted worktree is clean, that bottom area shows the branch's commit list instead (navigate and open a commit the same way as Branches → Enter). The branch name in that panel's title is click-to-copy. The commits the branch added since it forked are drawn bold on a tinted band, and the panel title counts them (`3 on this branch`), so its own work reads apart from the history it inherited. The same marking is on the full-screen log (`l`) and the Branches tab's commit history.

`Enter` hands the keyboard to the panel below and `q`/`Esc` gives it back, so the app still only quits from the worktree list. The bottom panel is the selected worktree's, so the worktree keys (`p` pull, `P` push, `f` fetch, `c` commit, and the rest) keep working while it holds the keyboard. Two keys mean what the focused panel is showing rather than what the list above it is: over the files, `d` deletes the file under the cursor (delete-worktree stays on the list above); over the commits, `u` opens the undo wizard for the commit under the cursor.

The Changes tab is folded away while this layout is on (it's already on screen). A terminal too short for three panels falls back to two so the diff stays reachable.

### Changes

![The Changes tab: changed files grouped into a folder tree on the left, the selected file's syntax-highlighted diff on the right with added lines tinted green and removed lines red](docs/images/tui-changes.png)

`Enter` on a worktree opens the Changes tab. Files are grouped under their folders on the left (`[x]`/`[ ]`/`[~]` shows how much of a folder is marked), and the selected file's diff is syntax-highlighted on the right (`⇧←`/`⇧→` or `H`/`L` scroll it horizontally; `⇧↑`/`⇧↓` or `J`/`K` scroll vertically). Diffs load in the background, so switching files never freezes the UI. New files inside brand-new folders are listed too. Updates live as files change; `r` refreshes now. `t` switches the file list between the folder tree and a flat path list.

From here you can mark files with `Space`, commit them with `c`, pull/push with `p`/`⇧P`, stash one (`s`) or all marked (`⇧S`) files, undo a file's changes with `u` (a brand-new file has no committed version, so it points you at delete instead), delete it with `d`, or add it to `.gitignore` with `i` (exact path or a glob). `←`/`→` (or `h`/`l`) collapse and expand the folder under the cursor (`←` on a file jumps to its parent); `Enter` toggles a folder, and on a file row opens it in whatever app your OS opens that file type with. Double-clicking a row does the same. The mouse wheel scrolls whichever panel it's over.

Clicking the path in the diff panel's title copies it to the clipboard, asking first whether you want it relative to the worktree (`r`) or the full path (`f`).

### Commit

![The commit dialog over the worktree list: a checklist of the five changed files, all ticked, with a typed commit message underneath](docs/images/tui-commit.png)

`c` commits without leaving the list. Tick the files you want (everything is selected by default, `Space` toggles, `Tab` switches between the file list and the message), type a message, and `Enter` commits. The dialog opens right away and reads the changed files in the background, so a big changeset never makes you wait to start typing; the list shows `reading changes…` until it lands, and a commit submitted before then fires as soon as it does. If the commit fails (a pre-commit hook rejects it, signing goes wrong), the dialog comes back with your message and body intact rather than making you retype them. If the selection looks like one side of a renamed folder, you're asked whether to include the other side so git records the rename.

### New worktree

![The new worktree dialog: an empty name field, a row for creating a new branch off a chosen base, and rows for checking out existing local and remote-only branches](docs/images/tui-new-worktree.png)

`n` creates a worktree. The top row makes a new branch off a base you pick with `Tab`; the rows below check out an existing branch, including remote-only ones like a teammate's `origin/feature/webhooks`, which become local tracking branches. Typing filters that list and names the new branch at the same time. A long branch name is elided in the middle so it never crowds the field you are typing into. To make a branch *without* a worktree, use the Branches tab (`n` there) instead. If the target folder already exists you're asked to open it (when it's already a worktree), replace it, or cancel.

### Branches

![The Branches tab: every local branch with where it is checked out, its upstream, a ✓merged flag on release/1.4, and the last commit on each](docs/images/tui-branches.png)

The Branches tab shows every branch, where each one is checked out, and the same **FLAGS** vocabulary as the worktree list. Remote-only branches are marked with `☁` under a `REMOTE BRANCHES` heading. Local branches sit under `LOCAL BRANCHES`.

From here:

- `c` checks the branch out in a new worktree
- `n` creates a branch only (no worktree, from HEAD)
- `d` deletes, locally only by default, or locally and on its remote when it has one (`⇧F` for a force delete; cancel is always listed)
- `a` archives the branch (git keeps it; it just stops showing up) and `v` toggles viewing archived branches (the panel title says how many are hidden)
- `u` changes which remote branch it tracks, or stops tracking, from a type-to-filter picker
- `m` merges it into a worktree you pick
- `b` rebases a worktree you pick onto it
- `p` fast-forwards it onto its upstream (a branch checked out in a worktree is pulled there so its files move with it; one checked out nowhere is fast-forwarded in place). A branch that has diverged is reported rather than merged
- `f` fetches all remotes, refreshing every branch's ahead/behind
- `Enter` opens the branch's commit history: `Space` marks commits (`a` all/none), `Enter` / `v` / `→` browses into the highlighted commit, `p` cherry-picks the marked commits (or the highlighted one) into a worktree you pick, and `t` switches between the commit tree and a flat list

### Log

![The commit log drawn as a tree, with branch and tag names marked on the commits they point at and a fork and merge visible in the graph](docs/images/tui-log.png)

`l` draws the log as a commit tree, with branch and tag names on the commits they point at, so forks and merges are visible at a glance. Every row's graph art is padded to the widest, so the hashes and subjects sit in one column instead of stepping in and out with the topology, and each commit's hash takes the colour of the lane its dot is drawn in. `↑`/`↓` move between commits and `Enter` browses into one (a read-only view of the files it changed, same tree + diff layout as Changes). `t` switches between the tree and a flat list; the choice carries over to the Branches tab's commit history. `u` opens the [undo wizard](#undoing-a-commit) for the highlighted commit.

### Stash

![The Stash tab listing two stash entries for a worktree, each with its message and branch](docs/images/tui-stash.png)

`s` opens the Stash tab. Stashes are shared across the whole repo, so popping or applying one asks which worktree to put it into (defaulting to the worktree the tab was opened from). `s` stashes the selected worktree's current changes (optional message); `p`/`a` pop/apply the selected entry; `x` drops it. `Enter` on an entry browses the files it changed. A pop that conflicts opens the conflict resolver.

### Keys

On the worktree list:

| Key | Action |
| --- | --- |
| `↑`/`↓` or `j`/`k` | select a worktree (mouse wheel over the table does the same) |
| `⇧↑`/`⇧↓` | scroll the changed-file panel below the table |
| `Enter` | Changes tab for the selected worktree (or focus the bottom panel in three-panel layout) |
| `n` | new worktree |
| `d` | delete the selected worktree: folder-only (keeps the branch) or folder + branch. Uncommitted changes: stash them or discard. If the branch can't be safely deleted you're offered a force delete; forcing a branch that's checked out elsewhere first switches that worktree to the repo's default branch |
| `c` | commit |
| `o` / `e` | open: run a configured `open_command`. Any configured commands open a picker listing each one already expanded (`{path}`, `{name}`, `{branch}`, `{status}`). `mode = "terminal"` (marked `▶`) closes wtm and runs in this terminal; the default `background` mode spawns detached. With none configured it prompts for a one-off |
| `u` | update: refresh the default branch from its upstream, then merge it in (or fast-forward in place). Offers to stash local changes first. On conflict, opens the conflict resolver |
| `s` | Stash tab |
| `m` | move uncommitted changes into another worktree you pick; refuses if the destination isn't clean |
| `p` | pull (fast-forward only). If the branch has diverged, offers to retry with a rebase |
| `⇧P` | push; publishes with `-u` when there's no upstream |
| `f` | fetch all remotes and refresh |
| `t` | change which remote branch this worktree's branch tracks, or stop tracking (a detached HEAD has no branch, so it says so) |
| `b` | switch this worktree to another branch (type to filter; local first, then remote-only). With nothing matching, `Enter` tries the typed name anyway |
| `l` | commit log for this worktree |
| `x` | jump to the conflict resolver for a worktree stopped mid-operation or left with unmerged files. Flagged in the list as `⚠ 3 conflicts` / `rebasing`. `Enter` gets there too; `x` is the shortcut that says why there is nothing to resolve when there isn't |
| `⇧U` | discard all local changes (asks first; same key on the Changes tab) |
| `⇧R` | rename this worktree |
| `r` | refresh (the worktree and branch lists also refresh themselves every minute, keeping your place) |
| `Tab` / `⇧Tab` | cycle tabs. Tab never switches tabs while a dialog is open |
| `?` | help (works here and in the changes view; `Tab` moves between help pages, `↑`/`↓` scrolls). Any key closes it |
| `q` / `Ctrl+C` | quit |

### Undoing a commit

Git has two unrelated ways to undo a commit, and picking the wrong one is expensive, so wtm asks rather than guessing. Press `u` on a commit in a worktree's log (`l`), in the commits panel of the three-panel layout, or while browsing a commit's files, and a short wizard walks you through it.

The first question is which kind of undo:

- **revert** adds a *new* commit that reverses the one you picked. History is untouched, so a branch you have already pushed stays safe to push. The follow-up asks whether to commit the reversal now, or leave it staged in your working tree so you can review or trim it first.
- **reset** moves the branch back to the commit you picked and everything after it leaves the branch. This rewrites history, so a shared branch will need a force-push. The follow-up is the *keep the changes?* question: keep them as uncommitted changes (`--mixed`), keep them staged and ready to recommit as one commit (`--soft`), or discard them along with the commits (`--hard`, the only answer marked destructive).

Each screen spells out what it will do before you confirm it. A reset to a commit that isn't in that branch's history is refused rather than performed, since it would move the branch sideways onto unrelated work. If undoing the commit conflicts with later work, the conflict resolver opens on it just like a conflicting merge or cherry-pick.

### Conflicts

![The conflict resolver: two conflicted files on the left, and on the right one hunk showing the OURS side in green and the THEIRS side in blue, waiting for a side to be picked](docs/images/tui-conflicts.png)

When a merge, rebase, update, cherry-pick, or stash pop hits a conflict, the **conflict resolver** opens automatically, and it isn't a window you can lose. While a worktree has unmerged files (or is stopped mid-merge, mid-rebase, or mid-cherry-pick) the resolver **is** that worktree's changes pane: it takes the place of the file list and diff on the Changes tab, and of the bottom region under the three-panel layout, so there is no ordinary changes view until the conflicts are settled. The Changes tab is flagged `⚠` while that's true. Finish the conflicts and the same pane turns back into the normal changes view, commit and all.

It lists the conflicted files (each with a resolved/unresolved marker) and shows the selected file's hunks. On a pane with room for it, each hunk is laid out the way other merge tools do it — **THEIRS ▶ FINAL ◀ OURS** — with the incoming side on the left, the current side on the right, and **FINAL** in the middle showing the text that will actually be written. Pick a side and FINAL rewrites itself; keep both and FINAL stacks them in the order they'll land, each line still in its own side's colour, so a mixed resolution reads as exactly that. An undecided hunk says so in the middle instead of sitting empty. On a narrower pane the hunk falls back to stacked OURS/THEIRS blocks, each side numbered from 1 down its own gutter so line 2 of OURS reads against line 2 of THEIRS.

Every hunk names both of its sides in place: the branch each came from, the key that takes it, and a `✓ keep` / `✗ drop` verdict once you've decided. A pinned legend at the top of the pane says where each side comes from and stays put as you scroll, and the footer counts how many hunks are decided. During a **rebase** git swaps the two, so OURS is the branch you are rebasing onto and THEIRS is your own commit being replayed; the resolver flags that in the legend.

Switching files costs a file read and nothing more (the worktree is resolved once, when the resolver opens), and the read runs off-thread, so holding `←`/`→` through a long list of conflicts keeps up with the keys. The file being read is marked `⟳` in the list and named in the pane, so a keypress is visibly registered before its hunks are there to show.

`←`/`→` move between files, `↑`/`↓` between hunks; `o`/`t` keep ours/theirs for the current hunk, `b`/`⇧B` **keep both** (ours-then-theirs or reversed, on separate lines), `⇧O`/`⇧T` take the whole file's side. The pane scrolls on its own with `⇧↑`/`⇧↓`, `⇧J`/`⇧K`, `PgUp`/`PgDn`, `g`/`⇧G`, or the wheel, and `↑`/`↓` fall through to scrolling once the cursor is on the first or last hunk, so a hunk taller than the screen still reads to the end. The mouse works throughout: the wheel steps the file list and scrolls the hunks, a click picks a file or a hunk, and a click on a hunk's THEIRS or OURS column takes that side.

`e` and `⇧E` open the **whole file**, full screen, conflict markers and all, for the fixes per-hunk choices can't express (interleaving both sides, fixing an import list, deleting a stray line). `e` puts the cursor on the hunk you were looking at; `⇧E` opens at the top. Choices you have already made are written to disk first, so you edit the file as it really stands. The editor takes the entire frame, numbers every line down the left, highlights the conflict markers with each side in its resolver colour, and scrolls in both directions with the arrows, `PgUp`/`PgDn`, or the mouse wheel (a notch moves the cursor three lines, and the view follows it). `Ctrl+S` saves and re-reads the file, so any hunks the edit settled drop out of the list; `Esc` discards.

Choosing a side with `o`/`t`/`b` **records the choice, it does not write it**. The pinned legend counts how many choices are still unwritten, the status line says the same on every press, and `c` refuses in those words rather than passing along git's bare "unmerged paths". `w` (or `Enter`) is what saves the file and, once every hunk is decided, stages it as resolved. If you fixed a conflict **in your own editor** instead, `a` marks the file resolved using exactly what is on disk (it refuses while conflict markers remain), and `r` re-reads the conflicts from disk to pick up changes made outside wtm.

`c` completes the operation (commit the merge, continue the rebase or cherry-pick, or drop the popped stash), `s` **skips** the commit a rebase stopped on, and `x` then `y` aborts and restores the worktree. `Esc`/`q` backs out to the worktree list and leaves the operation in progress; `Enter` on that worktree (or the `⚠ Changes` tab) walks straight back in where you left off.

### Settings tab

Pressing `Tab` to the **Settings** tab opens an editor for the repo's `.wtm.toml`: pick a row with `↑`/`↓`, press `Enter` to edit or cycle it, and the change is written immediately (comments preserved). It shows a live preview of where worktrees will land and a colour sample for the selected `diff_theme`, and clearing a field unsets it so the default (or global value) applies again. The `auto_update_check`, `diff_theme`, and `worktrees_layout` rows cycle rather than edit free text (`Enter` or `Space`), and all three are saved in the global config since they apply to wtm rather than to one repo. Switching `worktrees_layout` redraws the Worktrees tab on the next frame, no restart needed.

`open_command` holds a list of commands, so `Enter` on that row opens a small list editor: `↑`/`↓` move, `Enter` edits the selected command, `a` adds one, `d` removes one, `Enter` on `[ done ]` saves the list, and `Esc` discards the edits. Each command row carries two toggles: `g` saves that command **globally** so every repo offers it, and `t` switches it between running in the **background** and taking over the **terminal**. Both are shown in fixed columns on the row. Because entries are edited one at a time, a command containing a comma stays a single command.

Every text field (the new-branch name, the commit message, stash and branch names, and the settings and setup-wizard inputs) supports cursor editing: `←`/`→` move, `Home`/`End` jump, and `Backspace`/`Delete` remove characters mid-string.

While setup runs, its output streams into the progress window. Type a line and press `Enter` to answer a prompting command; press `Ctrl+C` twice to kill a stuck setup.

## Staying up to date

When the TUI starts it looks up the latest [release](https://github.com/faulker/wtm/releases) on a background thread. This never delays startup: the first frame draws immediately, and if the network is slow or unreachable the check simply fails silently. If a newer version exists you get a prompt with the version, a link to the release notes, and two choices, update and restart, or not now. Postponing keeps the version visible on the Settings tab and doesn't ask again until the next launch.

The check uses the public release URLs, not `api.github.com`. `/releases/latest` redirects to the newest tag, so one lookup gives both the version and (via the release workflow's asset naming) every download URL. That avoids GitHub's anonymous API rate limit, which any shared or office network exhausts quickly. No token is needed or used.

Installing downloads the build for your platform, verifies it against the release's SHA-256 checksums, checks that the new binary runs and reports the expected version, and only then moves it over the old one. If wtm lives somewhere your user can't write (a `/usr/local` or Homebrew install owned by root) it says so up front instead of failing halfway through.

From the command line:

```sh
wtm upgrade --check    # report whether a newer release exists
wtm upgrade            # download, verify, and install it
```

To turn the automatic check off:

```sh
wtm config set --global auto_update_check false
```

Or set `WTM_NO_UPDATE_CHECK` in the environment, which skips the check without touching any config file (useful in CI or offline). Debug builds and anything launched by `cargo run` also skip the automatic check, so local development does not get update prompts against a published release. Explicit `wtm upgrade` runs and the Settings tab's check-now row always check regardless. Updates need `curl`, `tar`, and `shasum` or `sha256sum` on your PATH.

## MCP server

`wtm mcp` speaks MCP over stdio and exposes the same operations as the CLI. Results use the same JSON shapes as the CLI's `--json` output.

| Area | Tools |
| --- | --- |
| Worktrees | `list_worktrees`, `create_worktree`, `remove_worktree`, `worktree_status`, `worktree_diff` |
| Commits | `commit_changes`, `worktree_log`, `cherry_pick` |
| Merge/rebase/conflicts | `merge`, `rebase`, `update`, `list_conflicts`, `read_conflict`, `resolve_file`, `complete_merge`, `skip_rebase`, `abort_merge` |
| Stashes | `stash_push`, `stash_list`, `stash_pop`, `stash_apply`, `stash_drop`, `move_changes` |
| Remotes | `pull_worktree`, `push_worktree`, `fetch_remotes` |
| Branches | `list_branches`, `create_branch`, `delete_branch`, `archive_branch`, `rename_branch`, `branch_log` |

Register with [Claude Code](https://claude.com/claude-code) from inside your repo:

```sh
claude mcp add wtm -- wtm mcp
```

The server binds to the repo it was started in and reloads `.wtm.toml` on every call.

## Build and test

```sh
cargo build            # debug build
cargo test             # unit + integration tests (temp git repos, MCP stdio session)
cargo build --release  # optimized binary at target/release/wtm
```

## Project layout

```
src/git.rs      thin wrapper around the git binary (worktree/status/diff parsing)
src/config.rs   layered config: global file + repo .wtm.toml, location rules
src/settings.rs wtm config and wtm init commands
src/ops.rs      core operations shared by CLI, TUI, and MCP
src/conflict.rs conflict-marker parsing and hunk resolution (ours/theirs/both)
src/update.rs   GitHub release check and self-update
src/platform.rs opening files in the OS default app, system clipboard
src/cli.rs      clap definitions
src/output.rs   human vs JSON rendering
src/tui/        ratatui app (state, rendering, event loop)
src/mcp.rs      MCP stdio server (rmcp)
tests/          end-to-end tests against throwaway git repos
```

## Releasing

Pushing a `v*` tag (or running the Release workflow with a bump type) builds,
tests, and publishes all four binaries with a SHA-256 checksum file.

The macOS binaries are codesigned, and notarized, when the Apple secrets are
configured on the repository; without them the release still goes out, just
unsigned. See [docs/macos-signing.md](docs/macos-signing.md) for which
certificate to get and which secrets to set.
