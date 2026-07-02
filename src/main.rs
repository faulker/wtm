//! wtm — a friendly manager for git worktrees.
//!
//! Entry point: with no subcommand the interactive TUI opens; subcommands run
//! the scriptable CLI; `wtm mcp` serves MCP over stdio for AI agents.

mod cli;
mod config;
mod git;
mod mcp;
mod ops;
mod output;
mod settings;
mod tui;

use anyhow::Result;
use clap::Parser;

use cli::{BranchAction, Cli, Command, StashAction};
use ops::Ctx;

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    if let Err(e) = run(cli) {
        if json {
            eprintln!("{}", serde_json::json!({ "error": format!("{e:#}") }));
        } else {
            eprintln!("error: {e:#}");
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Some(command) = cli.command else {
        return tui::run(Ctx::discover(&cwd)?);
    };
    match command {
        Command::Create { branch, from } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            // Human runs get live, interactive setup (prompts read from the
            // terminal); --json keeps output captured and machine-clean.
            let (mode, progress): (ops::RunMode, fn(&str)) = if cli.json {
                (ops::RunMode::Capture, |_| {})
            } else {
                (ops::RunMode::Inherit, |msg| println!("{msg}"))
            };
            let result = ops::create(&ctx, &branch, from.as_deref(), mode, progress)?;
            if cli.json {
                output::print_json(&result)?;
            } else {
                output::print_create(&result);
            }
            // Setup failures keep the worktree but should fail scripts loudly.
            if !result.setup_ok {
                std::process::exit(2);
            }
        }
        Command::List => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let infos = ops::list(&ctx)?;
            if cli.json {
                output::print_json(&infos)?;
            } else {
                output::print_list(&infos);
            }
        }
        Command::Remove {
            name,
            force,
            delete_branch,
        } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let info = ops::remove(&ctx, &name, force, delete_branch)?;
            if cli.json {
                output::print_json(&output::remove_json(&info, delete_branch))?;
            } else {
                println!("removed worktree '{}' ({})", info.name, info.path);
            }
        }
        Command::Status { name } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let (info, entries) = ops::status(&ctx, &name)?;
            if cli.json {
                output::print_json(&output::status_json(&info, &entries))?;
            } else {
                output::print_status(&info, &entries);
            }
        }
        Command::Diff { name } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let (info, diff) = ops::diff(&ctx, &name)?;
            if cli.json {
                output::print_json(&output::diff_json(&info, &diff))?;
            } else if diff.is_empty() {
                println!("{}: no uncommitted changes", info.name);
            } else {
                println!("{diff}");
            }
        }
        Command::Path { name } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let path = ops::path(&ctx, &name)?;
            if cli.json {
                output::print_json(&serde_json::json!({ "path": path }))?;
            } else {
                println!("{path}");
            }
        }
        Command::Commit {
            name,
            message,
            paths,
        } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let result = ops::commit(&ctx, &name, &message, paths.as_deref())?;
            if cli.json {
                output::print_json(&result)?;
            } else {
                output::print_commit(&result);
            }
        }
        Command::Stash { action } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            match action {
                StashAction::Push { name, message } => {
                    let result = ops::stash_push(&ctx, &name, message.as_deref())?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_stash(&result);
                    }
                }
                StashAction::List { name } => {
                    let result = ops::stash_list(&ctx, &name)?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_stash_list(&result);
                    }
                }
                StashAction::Pop { name, index } => {
                    let result = ops::stash_pop(&ctx, &name, index)?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_stash(&result);
                    }
                }
                StashAction::Apply { name, index } => {
                    let result = ops::stash_apply(&ctx, &name, index)?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_stash(&result);
                    }
                }
                StashAction::Drop { name, index } => {
                    let result = ops::stash_drop(&ctx, &name, index)?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_stash(&result);
                    }
                }
            }
        }
        Command::Pull { name, rebase } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let result = ops::pull(&ctx, &name, rebase)?;
            if cli.json {
                output::print_json(&result)?;
            } else {
                output::print_pull(&result);
            }
        }
        Command::Push {
            name,
            force_with_lease,
        } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let result = ops::push(&ctx, &name, force_with_lease)?;
            if cli.json {
                output::print_json(&result)?;
            } else {
                output::print_push(&result);
            }
        }
        Command::Fetch => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let result = ops::fetch(&ctx)?;
            if cli.json {
                output::print_json(&result)?;
            } else {
                output::print_fetch(&result);
            }
        }
        Command::Branch { action } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            match action {
                BranchAction::List => {
                    let result = ops::branch_list(&ctx)?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_branch_list(&result);
                    }
                }
                BranchAction::Create { name, from } => {
                    let result = ops::branch_create(&ctx, &name, from.as_deref())?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_branch_create(&result);
                    }
                }
                BranchAction::Delete { name, force } => {
                    let result = ops::branch_delete(&ctx, &name, force)?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_branch_delete(&result);
                    }
                }
                BranchAction::Rename { old, new } => {
                    let result = ops::branch_rename(&ctx, &old, &new)?;
                    if cli.json {
                        output::print_json(&result)?;
                    } else {
                        output::print_branch_rename(&result);
                    }
                }
            }
        }
        Command::Log { name, count } => {
            let ctx = Ctx::discover_initialized(&cwd)?;
            let result = ops::log(&ctx, &name, count)?;
            if cli.json {
                output::print_json(&result)?;
            } else {
                output::print_log(&result);
            }
        }
        Command::Init { force } => {
            let repo_root = git::repo_root(&cwd)?;
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            settings::init(&repo_root, force, &mut stdin.lock(), &mut stdout.lock())?;
        }
        Command::Config { action } => settings::config_command(&cwd, action, cli.json)?,
        Command::Mcp => {
            // Not gated at startup: the server should come up and report a
            // clear per-call error until the repo is initialized.
            let ctx = Ctx::discover(&cwd)?;
            mcp::serve(ctx)?;
        }
    }
    Ok(())
}
