---
name: verify
description: Build and drive the wtm binary (CLI and TUI) against a scratch git repo to observe a change at its real surface.
---

# Verifying wtm

`cargo build --release` → `target/release/wtm`. Two surfaces worth driving: the
CLI (`wtm <subcommand>`) and the TUI (`wtm` with no args inside a repo).

## Scratch repo

Every worktree command refuses until `.wtm.toml` exists. `wtm init` is an
interactive wizard, so **write an empty `.wtm.toml` instead** — that's enough to
enable the commands and is what the unit tests do.

The subcommand to make a worktree is `create`, not `new`.

To exercise remote branch behavior, clone from a local bare repo so `origin/*`
refs exist with no local counterparts:

```sh
git init -q --bare origin.git      # stands in for the remote
git init -q -b main seed           # publish branches into it, then
git clone -q origin.git proj       # clone: origin/* with no local branches
```

Faking refs with `update-ref refs/remotes/origin/x` alone is **not** enough for
anything using `--track`: git only treats a ref as a remote-tracking branch when
its remote is configured (`git remote add`). A real clone avoids the trap.

Worktrees are addressed by **branch name**, so a worktree's name changes after
you switch it onto another branch — re-address it in later commands.

## Driving the TUI

Use tmux on an isolated socket so you don't touch the user's sessions:

```sh
tmux -L wtmverify new-session -d -s t -x 100 -y 32 -c <repo> <path-to-wtm>
tmux -L wtmverify send-keys -t t b        # then capture
tmux -L wtmverify capture-pane -t t -p
tmux -L wtmverify kill-server
```

Put the send-keys/capture loop in a **script file**, not a compound Bash
command: the shell wrapper mangles some compound invocations (`rm -rf` loses its
flags, `$T`-style command aliases don't resolve). Sleep ~0.4s between keys and
~1s before capturing, since ops run on a background thread and land via `tick()`.

Success lands in the header status line; failures pop a modal `error` box.
