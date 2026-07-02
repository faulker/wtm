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
mod tui;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};
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
            let ctx = Ctx::discover(&cwd)?;
            let progress: fn(&str) = if cli.json {
                |_| {}
            } else {
                |msg| println!("{msg}")
            };
            let result = ops::create(&ctx, &branch, from.as_deref(), progress)?;
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
            let ctx = Ctx::discover(&cwd)?;
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
            let ctx = Ctx::discover(&cwd)?;
            let info = ops::remove(&ctx, &name, force, delete_branch)?;
            if cli.json {
                output::print_json(&output::remove_json(&info, delete_branch))?;
            } else {
                println!("removed worktree '{}' ({})", info.name, info.path);
            }
        }
        Command::Status { name } => {
            let ctx = Ctx::discover(&cwd)?;
            let (info, entries) = ops::status(&ctx, &name)?;
            if cli.json {
                output::print_json(&output::status_json(&info, &entries))?;
            } else {
                output::print_status(&info, &entries);
            }
        }
        Command::Diff { name } => {
            let ctx = Ctx::discover(&cwd)?;
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
            let ctx = Ctx::discover(&cwd)?;
            let path = ops::path(&ctx, &name)?;
            if cli.json {
                output::print_json(&serde_json::json!({ "path": path }))?;
            } else {
                println!("{path}");
            }
        }
        Command::Mcp => {
            let ctx = Ctx::discover(&cwd)?;
            mcp::serve(ctx)?;
        }
    }
    Ok(())
}
