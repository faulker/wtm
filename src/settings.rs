//! The `wtm config` and `wtm init` commands: view and change settings without
//! editing TOML by hand.
//!
//! Settings live in two layers: a global file (`~/.config/wtm/config.toml`)
//! that applies to every repo, and the repo's own `.wtm.toml` which overrides
//! it per field. `wtm config set` edits either file in place, preserving any
//! comments and formatting; `wtm init` walks through creating `.wtm.toml`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;
use toml_edit::{Array, DocumentMut, value as toml_value};

use crate::cli::ConfigAction;
use crate::config::{self, CONFIG_FILE, Config, DEFAULT_LOCATION, FileConfig, LOCATION_PRESETS};
use crate::git;
use crate::output;

/// Every setting `wtm config` understands, with a short description.
const KEYS: &[(&str, &str)] = &[
    (
        "worktree_dir",
        "where new worktrees go: sibling, inside, home, or a path",
    ),
    (
        "open_command",
        "command the TUI's open key runs in a worktree (e.g. `cursor .`)",
    ),
    (
        "auto_update_check",
        "check GitHub for a newer wtm when the TUI starts (true/false)",
    ),
    (
        "setup.copy",
        "files copied into each new worktree, comma separated",
    ),
    (
        "setup.run",
        "commands run in each new worktree, comma separated",
    ),
];

/// Answers collected by an init wizard (CLI or TUI), ready to be written as
/// the repo's `.wtm.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDraft {
    /// Preset name (`sibling`, `inside`, `home`) or a path.
    pub worktree_dir: String,
    pub copy: Vec<String>,
    pub run: Vec<String>,
}

impl Default for ConfigDraft {
    fn default() -> ConfigDraft {
        ConfigDraft {
            worktree_dir: DEFAULT_LOCATION.to_string(),
            copy: Vec::new(),
            run: Vec::new(),
        }
    }
}

impl ConfigDraft {
    /// Builds a draft from a parsed config file, filling in defaults for
    /// anything the file doesn't set.
    fn from_file_config(cfg: FileConfig) -> ConfigDraft {
        let setup = cfg.setup.unwrap_or_default();
        ConfigDraft {
            worktree_dir: cfg
                .worktree_dir
                .unwrap_or_else(|| DEFAULT_LOCATION.to_string()),
            copy: setup
                .copy
                .unwrap_or_default()
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
            run: setup.run.unwrap_or_default(),
        }
    }
}

/// Loads settings to clone from `raw`: a repo directory containing `.wtm.toml`
/// or a direct path to a TOML file. A leading `~` is expanded.
pub fn load_clone_source(raw: &str) -> Result<ConfigDraft> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("no path given");
    }
    let path = config::expand_user_path(raw)?;
    let file = if path.is_dir() {
        let candidate = path.join(CONFIG_FILE);
        if !candidate.is_file() {
            bail!("no {CONFIG_FILE} found in {}", path.display());
        }
        candidate
    } else if path.is_file() {
        path
    } else {
        bail!("{} does not exist", path.display());
    };
    Ok(ConfigDraft::from_file_config(FileConfig::load(&file)?))
}

/// Writes `draft` as the repo's `.wtm.toml` (with explanatory comments) and
/// returns the written path.
pub fn write_draft(repo_root: &Path, draft: &ConfigDraft) -> Result<PathBuf> {
    let file = repo_root.join(CONFIG_FILE);
    let content = render_config(&draft.worktree_dir, &draft.copy, &draft.run);
    std::fs::write(&file, &content)
        .with_context(|| format!("failed to write {}", file.display()))?;
    Ok(file)
}

/// The values the repo's own `.wtm.toml` sets (ignoring the global layer), as
/// strings for the TUI config editor. Unset keys come back empty; `copy` and
/// `run` are comma-joined.
///
/// `auto_update_check` is the exception: it governs wtm itself rather than one
/// repo, so it is read from `global_config` (the caller's resolved global
/// config path, or `None` when there isn't one) instead of the repo file.
pub fn repo_config_fields(
    repo_root: &Path,
    global_config: Option<&Path>,
) -> Result<RepoConfigFields> {
    let cfg = FileConfig::load(&repo_root.join(CONFIG_FILE))?;
    let setup = cfg.setup.unwrap_or_default();
    let copy = setup
        .copy
        .unwrap_or_default()
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let run = setup.run.unwrap_or_default().join(", ");
    Ok(RepoConfigFields {
        worktree_dir: cfg.worktree_dir.unwrap_or_default(),
        open_command: cfg.open_command.unwrap_or_default(),
        auto_update_check: global_auto_update_check(global_config),
        copy,
        run,
    })
}

/// The global config's `auto_update_check` as an editor string: `""` when
/// unset, otherwise `"true"`/`"false"`. A missing or unreadable global config
/// reads as unset rather than failing the whole Settings tab.
fn global_auto_update_check(global_config: Option<&Path>) -> String {
    global_config
        .and_then(|path| FileConfig::load(path).ok())
        .and_then(|cfg| cfg.auto_update_check)
        .map(|v| v.to_string())
        .unwrap_or_default()
}

/// The settings the TUI config editor shows, each empty when unset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoConfigFields {
    pub worktree_dir: String,
    pub open_command: String,
    /// `""`, `"true"`, or `"false"`; lives in the global config.
    pub auto_update_check: String,
    pub copy: String,
    pub run: String,
}

/// Applies edits from the TUI config editor, preserving comments and the
/// surrounding TOML. An empty value unsets the key so the default (or global
/// value) applies again. Returns the repo file's path.
///
/// Repo settings go to the repo's `.wtm.toml`; `auto_update_check` goes to
/// `global_config`, since it is about wtm rather than about this repository.
pub fn save_config_edits(
    repo_root: &Path,
    global_config: Option<&Path>,
    fields: &RepoConfigFields,
) -> Result<PathBuf> {
    let file = repo_root.join(CONFIG_FILE);
    let mut doc = load_doc(&file)?;
    set_or_unset(&mut doc, "worktree_dir", &fields.worktree_dir)?;
    set_or_unset(&mut doc, "open_command", &fields.open_command)?;
    set_or_unset(&mut doc, "setup.copy", &fields.copy)?;
    set_or_unset(&mut doc, "setup.run", &fields.run)?;
    save_doc(&file, &doc)?;
    if let Some(path) = global_config {
        save_global_auto_update_check(path, &fields.auto_update_check)?;
    }
    Ok(file)
}

/// Writes (or clears) `auto_update_check` in the global config at `path`,
/// leaving the file untouched when nothing about that key changed.
fn save_global_auto_update_check(path: &Path, raw: &str) -> Result<()> {
    let mut doc = load_doc(path)?;
    let changed = if raw.trim().is_empty() {
        apply_unset(&mut doc, "auto_update_check")?
    } else {
        apply_set(&mut doc, "auto_update_check", raw)?;
        true
    };
    if changed {
        save_doc(path, &doc)?;
    }
    Ok(())
}

/// Sets `key` to `raw`, or unsets it when `raw` is blank.
fn set_or_unset(doc: &mut DocumentMut, key: &str, raw: &str) -> Result<()> {
    if raw.trim().is_empty() {
        apply_unset(doc, key)?;
    } else {
        apply_set(doc, key, raw)?;
    }
    Ok(())
}

/// Entry point for `wtm config`; no subcommand means `show`.
pub fn config_command(cwd: &Path, action: Option<ConfigAction>, json: bool) -> Result<()> {
    match action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Show => show(cwd, json),
        ConfigAction::Get { key } => get(cwd, &key, json),
        ConfigAction::Set { key, value, global } => set(cwd, &key, &value, global, json),
        ConfigAction::Unset { key, global } => unset(cwd, &key, global, json),
        ConfigAction::Path => paths(cwd, json),
    }
}

/// Shows every effective setting, its value, and which file it came from.
fn show(cwd: &Path, json: bool) -> Result<()> {
    let repo_root = git::repo_root(cwd)?;
    let cfg = Config::load(&repo_root)?;
    let raw_dir = cfg
        .worktree_dir
        .clone()
        .unwrap_or_else(|| DEFAULT_LOCATION.to_string());
    let resolved = cfg.worktree_base(&repo_root)?;
    let repo_file = repo_root.join(CONFIG_FILE);
    let global_file = config::global_config_path();

    if json {
        return output::print_json(&json!({
            "worktree_dir": {
                "value": raw_dir,
                "resolved": resolved,
                "source": cfg.worktree_dir_source,
            },
            "open_command": {
                "value": cfg.open_command,
                "source": cfg.open_command_source,
            },
            "auto_update_check": {
                "value": cfg.auto_update_check(),
                "source": cfg.auto_update_check_source,
            },
            "version": crate::update::CURRENT_VERSION,
            "setup": {
                "copy": { "value": cfg.setup.copy, "source": cfg.copy_source },
                "run": { "value": cfg.setup.run, "source": cfg.run_source },
            },
            "files": {
                "repo": { "path": repo_file, "exists": repo_file.exists() },
                "global": global_file.as_ref().map(|p| json!({ "path": p, "exists": p.exists() })),
            },
        }));
    }

    println!("settings for {}", repo_root.display());
    println!();
    println!(
        "  worktree_dir = {raw_dir:?}   ({})",
        cfg.worktree_dir_source
    );
    println!("      new worktrees go in {}", resolved.display());
    println!(
        "  open_command = {:?}   ({})",
        cfg.open_command.clone().unwrap_or_default(),
        cfg.open_command_source
    );
    println!(
        "  auto_update_check = {}   ({})",
        cfg.auto_update_check(),
        cfg.auto_update_check_source
    );
    println!(
        "  setup.copy   = {:?}   ({})",
        cfg.setup.copy, cfg.copy_source
    );
    println!(
        "  setup.run    = {:?}   ({})",
        cfg.setup.run, cfg.run_source
    );
    println!();
    println!("  wtm version    {}", crate::update::CURRENT_VERSION);
    println!("  repo config    {}", file_status(&repo_file));
    match &global_file {
        Some(path) => println!("  global config  {}", file_status(path)),
        None => println!("  global config  (unavailable: HOME is not set)"),
    }
    println!();
    println!("  wtm config set <key> <value>     change a setting for this repo");
    println!("  wtm config set -g <key> <value>  change it for every repo");
    println!("  wtm init                         guided setup");
    Ok(())
}

/// Prints one setting's effective value.
fn get(cwd: &Path, key: &str, json: bool) -> Result<()> {
    known_key(key)?;
    let repo_root = git::repo_root(cwd)?;
    let cfg = Config::load(&repo_root)?;
    let value = match key {
        "worktree_dir" => json!(
            cfg.worktree_dir
                .clone()
                .unwrap_or_else(|| DEFAULT_LOCATION.to_string())
        ),
        "open_command" => json!(cfg.open_command.clone().unwrap_or_default()),
        "auto_update_check" => json!(cfg.auto_update_check()),
        "setup.copy" => json!(cfg.setup.copy),
        "setup.run" => json!(cfg.setup.run),
        _ => unreachable!("known_key checked"),
    };
    if json {
        return output::print_json(&value);
    }
    match &value {
        serde_json::Value::String(s) => println!("{s}"),
        serde_json::Value::Array(items) => {
            for item in items {
                println!("{}", item.as_str().unwrap_or_default());
            }
        }
        _ => println!("{value}"),
    }
    Ok(())
}

/// Changes one setting in the repo's `.wtm.toml` or the global config.
fn set(cwd: &Path, key: &str, raw: &str, global: bool, json: bool) -> Result<()> {
    known_key(key)?;
    if key == "worktree_dir" && raw.trim().is_empty() {
        bail!("empty value; use `wtm config unset worktree_dir` to go back to the default");
    }
    let file = target_file(cwd, global)?;
    let mut doc = load_doc(&file)?;
    apply_set(&mut doc, key, raw)?;
    save_doc(&file, &doc)?;

    if json {
        return output::print_json(&json!({ "set": key, "value": raw, "file": file }));
    }
    println!("set {key} = {raw:?} in {}", file.display());
    if key == "worktree_dir" {
        if let Ok(repo_root) = git::repo_root(cwd)
            && let Ok(resolved) = config::resolve_worktree_dir(raw, &repo_root)
        {
            println!(
                "new worktrees for this repo will go in {}",
                resolved.display()
            );
        }
        maybe_preset_note(raw);
    }
    Ok(())
}

/// Removes one setting so the default (or the global value) applies again.
fn unset(cwd: &Path, key: &str, global: bool, json: bool) -> Result<()> {
    known_key(key)?;
    let file = target_file(cwd, global)?;
    let mut doc = load_doc(&file)?;
    let removed = apply_unset(&mut doc, key)?;
    if removed {
        save_doc(&file, &doc)?;
    }
    if json {
        return output::print_json(&json!({ "unset": key, "removed": removed, "file": file }));
    }
    if removed {
        println!("removed {key} from {}", file.display());
    } else {
        println!("{key} was not set in {} (nothing to do)", file.display());
    }
    Ok(())
}

/// Prints the config file locations wtm reads.
fn paths(cwd: &Path, json: bool) -> Result<()> {
    let repo_file = git::repo_root(cwd).ok().map(|root| root.join(CONFIG_FILE));
    let global_file = config::global_config_path();
    if json {
        return output::print_json(&json!({
            "repo": repo_file.as_ref().map(|p| json!({ "path": p, "exists": p.exists() })),
            "global": global_file.as_ref().map(|p| json!({ "path": p, "exists": p.exists() })),
        }));
    }
    match &repo_file {
        Some(path) => println!("repo config    {}", file_status(path)),
        None => println!("repo config    (not inside a git repository)"),
    }
    match &global_file {
        Some(path) => println!("global config  {}", file_status(path)),
        None => println!("global config  (unavailable: HOME is not set)"),
    }
    Ok(())
}

/// Interactive `.wtm.toml` setup. Answers come from `input` (stdin in
/// production, a script in tests); blank answers and EOF pick the defaults.
pub fn init(
    repo_root: &Path,
    force: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    let file = repo_root.join(CONFIG_FILE);
    if file.exists() && !force {
        bail!(
            "{} already exists; use `wtm config set` to change individual settings, \
             or `wtm init --force` to start over",
            file.display()
        );
    }

    writeln!(out, "Setting up wtm for {}", repo_root.display())?;
    writeln!(out)?;

    // Offer to clone an existing config before asking questions from scratch.
    let cloned = loop {
        let answer = ask(
            input,
            out,
            "Clone settings from another repo or .wtm.toml file? (path, blank to skip): ",
        )?;
        if answer.is_empty() {
            break None;
        }
        match load_clone_source(&answer) {
            Ok(draft) => break Some(draft),
            Err(e) => writeln!(out, "cannot clone from there: {e:#}")?,
        }
    };
    if let Some(draft) = cloned {
        writeln!(out)?;
        writeln!(out, "Cloned settings:")?;
        writeln!(out, "  worktree_dir = {:?}", draft.worktree_dir)?;
        writeln!(out, "  setup.copy   = {:?}", draft.copy)?;
        writeln!(out, "  setup.run    = {:?}", draft.run)?;
        let answer = ask(input, out, "Use these settings? [Y/n]: ")?;
        if answer.is_empty()
            || answer.eq_ignore_ascii_case("y")
            || answer.eq_ignore_ascii_case("yes")
        {
            return write_and_report(repo_root, &draft, out);
        }
        writeln!(out, "OK, starting from scratch instead.")?;
        writeln!(out)?;
    }

    writeln!(out, "Where should new worktrees be created?")?;
    for (i, (name, label)) in LOCATION_PRESETS.iter().enumerate() {
        let preview = config::resolve_worktree_dir(name, repo_root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "(needs HOME set)".to_string());
        let default_marker = if i == 0 { "  (default)" } else { "" };
        writeln!(out, "  {}. {label}: {preview}{default_marker}", i + 1)?;
    }
    writeln!(out, "  4. somewhere else: type a path")?;
    let worktree_dir = loop {
        let answer = ask(input, out, "Choose 1-4 [1]: ")?;
        match answer.as_str() {
            "" | "1" => break DEFAULT_LOCATION.to_string(),
            "2" => break "inside".to_string(),
            "3" => break "home".to_string(),
            "4" => {
                let path = ask(
                    input,
                    out,
                    "Path (absolute, ~/..., or relative to the repo; {repo} = repo name): ",
                )?;
                if path.is_empty() {
                    writeln!(out, "no path given; using the default")?;
                    break DEFAULT_LOCATION.to_string();
                }
                break path;
            }
            other => writeln!(out, "'{other}' is not one of 1-4, try again")?,
        }
    };

    writeln!(out)?;
    let copy_answer = ask(
        input,
        out,
        "Files to copy into each new worktree (comma separated, e.g. .env, .env.local) [none]: ",
    )?;
    let copy = split_list(&copy_answer);

    writeln!(out)?;
    writeln!(
        out,
        "Commands to run in each new worktree (e.g. npm install)."
    )?;
    let mut run = Vec::new();
    loop {
        let cmd = ask(
            input,
            out,
            &format!("Command {} (blank to finish): ", run.len() + 1),
        )?;
        if cmd.is_empty() {
            break;
        }
        run.push(cmd);
    }

    let draft = ConfigDraft {
        worktree_dir,
        copy,
        run,
    };
    write_and_report(repo_root, &draft, out)
}

/// Writes the draft as `.wtm.toml` and prints the closing summary.
fn write_and_report(repo_root: &Path, draft: &ConfigDraft, out: &mut dyn Write) -> Result<()> {
    let file = write_draft(repo_root, draft)?;
    let resolved = config::resolve_worktree_dir(&draft.worktree_dir, repo_root)?;
    writeln!(out)?;
    writeln!(out, "Wrote {}", file.display())?;
    writeln!(out, "New worktrees will go in {}", resolved.display())?;
    writeln!(
        out,
        "Try it: wtm create my-branch  (or run `wtm` for the interactive UI)"
    )?;
    writeln!(out, "Change settings anytime with `wtm config`.")?;
    Ok(())
}

/// Prompts for one line of input; EOF yields the empty string.
fn ask(input: &mut dyn BufRead, out: &mut dyn Write, prompt: &str) -> Result<String> {
    write!(out, "{prompt}")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Renders a fresh `.wtm.toml` with explanatory comments.
fn render_config(worktree_dir: &str, copy: &[String], run: &[String]) -> String {
    let mut doc = DocumentMut::new();
    doc["worktree_dir"] = toml_value(worktree_dir);
    if !copy.is_empty() || !run.is_empty() {
        let mut setup = toml_edit::Table::new();
        if !copy.is_empty() {
            setup["copy"] = toml_value(to_array(copy));
        }
        if !run.is_empty() {
            setup["run"] = toml_value(to_array(run));
        }
        doc["setup"] = toml_edit::Item::Table(setup);
    }
    format!(
        "# wtm settings for this repo. Edit by hand or use `wtm config set`.\n\
         # worktree_dir: \"sibling\", \"inside\", \"home\", or a path; {{repo}} = repo name.\n\
         # [setup] copy = files copied into new worktrees, run = commands run in them.\n\n{doc}"
    )
}

/// Errors on settings `wtm config` doesn't know, listing the ones it does.
fn known_key(key: &str) -> Result<()> {
    if KEYS.iter().any(|(name, _)| *name == key) {
        return Ok(());
    }
    let known = KEYS
        .iter()
        .map(|(name, desc)| format!("  {name}: {desc}"))
        .collect::<Vec<_>>()
        .join("\n");
    bail!("unknown setting '{key}'; available settings:\n{known}");
}

/// The config file a change should go to: the repo's `.wtm.toml`, or the
/// global file with `--global`.
fn target_file(cwd: &Path, global: bool) -> Result<PathBuf> {
    if global {
        config::global_config_path()
            .context("cannot locate the global config; set HOME or WTM_GLOBAL_CONFIG")
    } else {
        Ok(git::repo_root(cwd)?.join(CONFIG_FILE))
    }
}

/// Parses an existing config file for editing; a missing file starts empty.
fn load_doc(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    text.parse()
        .with_context(|| format!("invalid TOML in {}", path.display()))
}

/// Writes the edited document back, refusing to write anything wtm itself
/// couldn't load again.
fn save_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    let text = doc.to_string();
    toml::from_str::<FileConfig>(&text)
        .with_context(|| format!("refusing to write invalid config to {}", path.display()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Applies one `set` to the TOML document.
fn apply_set(doc: &mut DocumentMut, key: &str, raw: &str) -> Result<()> {
    match key {
        "worktree_dir" => {
            doc["worktree_dir"] = toml_value(raw);
        }
        "open_command" => {
            doc["open_command"] = toml_value(raw);
        }
        "auto_update_check" => {
            doc["auto_update_check"] = toml_value(parse_bool(raw)?);
        }
        "setup.copy" | "setup.run" => {
            let sub = key.strip_prefix("setup.").unwrap();
            let setup = doc
                .entry("setup")
                .or_insert(toml_edit::table())
                .as_table_mut()
                .context("'setup' in the config file is not a table")?;
            setup[sub] = toml_value(to_array(&split_list(raw)));
        }
        _ => unreachable!("known_key checked"),
    }
    Ok(())
}

/// Applies one `unset`; returns whether the key was present.
fn apply_unset(doc: &mut DocumentMut, key: &str) -> Result<bool> {
    let removed = match key {
        "worktree_dir" => doc.remove("worktree_dir").is_some(),
        "open_command" => doc.remove("open_command").is_some(),
        "auto_update_check" => doc.remove("auto_update_check").is_some(),
        "setup.copy" | "setup.run" => {
            let sub = key.strip_prefix("setup.").unwrap();
            let removed = doc
                .get_mut("setup")
                .and_then(|item| item.as_table_mut())
                .map(|table| table.remove(sub).is_some())
                .unwrap_or(false);
            // Drop an emptied [setup] section rather than leaving a stub.
            if doc
                .get("setup")
                .and_then(|item| item.as_table())
                .is_some_and(|table| table.is_empty())
            {
                doc.remove("setup");
            }
            removed
        }
        _ => unreachable!("known_key checked"),
    };
    Ok(removed)
}

/// A bare word that isn't a known preset is more likely a typo'd preset than
/// an intentional relative directory; point that out instead of failing.
fn maybe_preset_note(raw: &str) {
    let looks_like_path = raw.contains(['/', '\\', '.', '~', '{']);
    if !looks_like_path && !LOCATION_PRESETS.iter().any(|(name, _)| *name == raw) {
        println!(
            "note: {raw:?} is not a preset (sibling, inside, home), so it is treated as a \
             directory called {raw:?} in the repo root"
        );
    }
}

/// Parses a boolean setting, accepting the spellings people actually type.
pub(crate) fn parse_bool(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "y" | "on" | "1" => Ok(true),
        "false" | "no" | "n" | "off" | "0" => Ok(false),
        other => bail!("expected true or false, got {other:?}"),
    }
}

/// Splits a comma-separated value into trimmed, non-empty items.
pub(crate) fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

fn to_array(items: &[String]) -> Array {
    let mut arr = Array::new();
    for item in items {
        arr.push(item.as_str());
    }
    arr
}

fn file_status(path: &Path) -> String {
    let status = if path.exists() {
        "exists"
    } else {
        "not created yet"
    };
    format!("{} ({status})", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_preserves_comments_and_other_keys() {
        let mut doc: DocumentMut =
            "# my notes\nworktree_dir = \"sibling\"\n\n[setup]\nrun = [\"npm install\"]\n"
                .parse()
                .unwrap();
        apply_set(&mut doc, "worktree_dir", "inside").unwrap();
        apply_set(&mut doc, "setup.copy", ".env, .env.local").unwrap();
        let text = doc.to_string();
        assert!(text.contains("# my notes"), "comment lost: {text}");
        assert!(text.contains("worktree_dir = \"inside\""));
        assert!(text.contains("run = [\"npm install\"]"));
        let parsed: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed.setup.unwrap().copy.unwrap(),
            vec![PathBuf::from(".env"), PathBuf::from(".env.local")]
        );
    }

    #[test]
    fn unset_removes_key_and_empty_setup_table() {
        let mut doc: DocumentMut = "worktree_dir = \"home\"\n\n[setup]\ncopy = [\".env\"]\n"
            .parse()
            .unwrap();
        assert!(apply_unset(&mut doc, "setup.copy").unwrap());
        assert!(apply_unset(&mut doc, "worktree_dir").unwrap());
        assert!(!apply_unset(&mut doc, "setup.run").unwrap());
        assert_eq!(doc.to_string().trim(), "");
    }

    #[test]
    fn unknown_keys_are_rejected_with_help() {
        let err = known_key("worktreedir").unwrap_err().to_string();
        assert!(err.contains("unknown setting"));
        assert!(err.contains("worktree_dir"));
        assert!(err.contains("setup.run"));
    }

    #[test]
    fn splits_comma_lists() {
        assert_eq!(
            split_list(" .env , .env.local ,"),
            vec![".env", ".env.local"]
        );
        assert!(split_list("  ").is_empty());
    }

    #[test]
    fn rendered_init_config_is_loadable_and_escaped() {
        let content = render_config(
            "~/wt/{repo}",
            &[".env".to_string()],
            &["echo \"hi\"".to_string()],
        );
        let parsed: FileConfig = toml::from_str(&content).unwrap();
        assert_eq!(parsed.worktree_dir.as_deref(), Some("~/wt/{repo}"));
        let setup = parsed.setup.unwrap();
        assert_eq!(setup.run.unwrap(), vec!["echo \"hi\"".to_string()]);
    }

    #[test]
    fn init_wizard_scripted_run_writes_config() {
        let dir = tempfile::tempdir().unwrap();
        // Skip cloning, choose "inside", copy .env, one setup command, finish.
        let mut input = std::io::Cursor::new("\n2\n.env\nnpm install\n\n");
        let mut out = Vec::new();
        init(dir.path(), false, &mut input, &mut out).unwrap();
        let cfg = FileConfig::load(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(cfg.worktree_dir.as_deref(), Some("inside"));
        let setup = cfg.setup.unwrap();
        assert_eq!(setup.copy.unwrap(), vec![PathBuf::from(".env")]);
        assert_eq!(setup.run.unwrap(), vec!["npm install".to_string()]);
        let transcript = String::from_utf8(out).unwrap();
        assert!(transcript.contains("Where should new worktrees be created?"));
        assert!(transcript.contains("Wrote"));
    }

    #[test]
    fn init_wizard_defaults_on_eof() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = std::io::Cursor::new("");
        let mut out = Vec::new();
        init(dir.path(), false, &mut input, &mut out).unwrap();
        let cfg = FileConfig::load(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(cfg.worktree_dir.as_deref(), Some("sibling"));
        assert!(cfg.setup.is_none());
    }

    #[test]
    fn load_clone_source_reads_dir_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CONFIG_FILE);
        std::fs::write(
            &file,
            "worktree_dir = \"home\"\n[setup]\ncopy = [\".env\"]\nrun = [\"make\"]\n",
        )
        .unwrap();

        // A repo directory resolves to its .wtm.toml.
        let draft = load_clone_source(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(draft.worktree_dir, "home");
        assert_eq!(draft.copy, vec![".env"]);
        assert_eq!(draft.run, vec!["make"]);

        // A direct file path works too, even under another name.
        let other = dir.path().join("shared.toml");
        std::fs::write(&other, "worktree_dir = \"inside\"\n").unwrap();
        let draft = load_clone_source(other.to_str().unwrap()).unwrap();
        assert_eq!(draft.worktree_dir, "inside");
        assert!(draft.copy.is_empty());
    }

    #[test]
    fn load_clone_source_rejects_bad_paths_and_bad_toml() {
        let dir = tempfile::tempdir().unwrap();

        // Directory without a config file names the directory.
        let err = load_clone_source(dir.path().to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains(".wtm.toml"), "{err}");

        // Nonexistent path.
        let missing = dir.path().join("nope");
        let err = load_clone_source(missing.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");

        // Invalid TOML is a hard error, not a silent default.
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "not [valid").unwrap();
        assert!(load_clone_source(bad.to_str().unwrap()).is_err());

        // Blank input.
        assert!(load_clone_source("   ").is_err());
    }

    #[test]
    fn write_draft_round_trips_through_file_config() {
        let dir = tempfile::tempdir().unwrap();
        let draft = ConfigDraft {
            worktree_dir: "~/wt/{repo}".to_string(),
            copy: vec![".env".to_string()],
            run: vec!["echo \"hi\"".to_string()],
        };
        let file = write_draft(dir.path(), &draft).unwrap();
        let cfg = FileConfig::load(&file).unwrap();
        assert_eq!(cfg.worktree_dir.as_deref(), Some("~/wt/{repo}"));
        let setup = cfg.setup.unwrap();
        assert_eq!(setup.copy.unwrap(), vec![PathBuf::from(".env")]);
        assert_eq!(setup.run.unwrap(), vec!["echo \"hi\"".to_string()]);
    }

    #[test]
    fn init_wizard_clones_settings_from_path() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join(CONFIG_FILE),
            "worktree_dir = \"inside\"\n[setup]\nrun = [\"npm ci\"]\n",
        )
        .unwrap();
        let target = tempfile::tempdir().unwrap();

        // Give the source path, accept the cloned settings.
        let script = format!("{}\ny\n", source.path().display());
        let mut input = std::io::Cursor::new(script);
        let mut out = Vec::new();
        init(target.path(), false, &mut input, &mut out).unwrap();

        let cfg = FileConfig::load(&target.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(cfg.worktree_dir.as_deref(), Some("inside"));
        assert_eq!(cfg.setup.unwrap().run.unwrap(), vec!["npm ci".to_string()]);
        let transcript = String::from_utf8(out).unwrap();
        assert!(transcript.contains("Cloned settings:"), "{transcript}");
        assert!(transcript.contains("Wrote"), "{transcript}");
    }

    #[test]
    fn init_wizard_bad_clone_path_retries_then_declining_falls_through() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join(CONFIG_FILE), "worktree_dir = \"home\"\n").unwrap();
        let target = tempfile::tempdir().unwrap();

        // Bad path -> retry with a good one -> decline -> normal wizard picks
        // "inside" with no copy/run steps.
        let script = format!(
            "/definitely/not/there\n{}\nn\n2\n\n\n",
            source.path().display()
        );
        let mut input = std::io::Cursor::new(script);
        let mut out = Vec::new();
        init(target.path(), false, &mut input, &mut out).unwrap();

        let cfg = FileConfig::load(&target.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(cfg.worktree_dir.as_deref(), Some("inside"));
        let transcript = String::from_utf8(out).unwrap();
        assert!(
            transcript.contains("cannot clone from there"),
            "{transcript}"
        );
        assert!(transcript.contains("starting from scratch"), "{transcript}");
    }

    /// Editor fields with everything unset, for building one-field cases.
    fn fields(worktree_dir: &str, open_command: &str, copy: &str, run: &str) -> RepoConfigFields {
        RepoConfigFields {
            worktree_dir: worktree_dir.to_string(),
            open_command: open_command.to_string(),
            auto_update_check: String::new(),
            copy: copy.to_string(),
            run: run.to_string(),
        }
    }

    #[test]
    fn repo_config_fields_reads_current_values() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "worktree_dir = \"home\"\n[setup]\ncopy = [\".env\", \"config/.env\"]\n",
        )
        .unwrap();
        let fields = repo_config_fields(dir.path(), None).unwrap();
        assert_eq!(fields.worktree_dir, "home");
        assert_eq!(fields.open_command, "");
        assert_eq!(fields.copy, ".env, config/.env");
        assert_eq!(fields.run, "");
        // With no global config to read, the toggle shows as unset (default).
        assert_eq!(fields.auto_update_check, "");
    }

    #[test]
    fn save_and_read_open_command() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CONFIG_FILE);
        save_config_edits(dir.path(), None, &fields("", "cursor .", "", "")).unwrap();
        let cfg = FileConfig::load(&file).unwrap();
        assert_eq!(cfg.open_command.as_deref(), Some("cursor ."));
        // Clearing it unsets the key again.
        save_config_edits(dir.path(), None, &fields("", "", "", "")).unwrap();
        let cfg = FileConfig::load(&file).unwrap();
        assert_eq!(cfg.open_command, None);
    }

    #[test]
    fn save_config_edits_preserves_comments_and_unsets_blanks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CONFIG_FILE);
        std::fs::write(
            &file,
            "# keep me\nworktree_dir = \"home\"\n\n[setup]\ncopy = [\".env\"]\n",
        )
        .unwrap();

        // Change worktree_dir, add a run command, and clear copy (unset it).
        save_config_edits(dir.path(), None, &fields("inside", "", "", "npm install")).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# keep me"), "comment lost: {text}");
        let cfg = FileConfig::load(&file).unwrap();
        assert_eq!(cfg.worktree_dir.as_deref(), Some("inside"));
        let setup = cfg.setup.unwrap();
        assert!(setup.copy.is_none(), "copy should have been unset");
        assert_eq!(setup.run.unwrap(), vec!["npm install".to_string()]);
    }

    #[test]
    fn auto_update_check_round_trips_through_the_global_config() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        std::fs::write(&global, "# global notes\nworktree_dir = \"home\"\n").unwrap();

        let mut edits = fields("", "", "", "");
        edits.auto_update_check = "false".to_string();
        save_config_edits(dir.path(), Some(&global), &edits).unwrap();

        let text = std::fs::read_to_string(&global).unwrap();
        assert!(text.contains("# global notes"), "comment lost: {text}");
        assert_eq!(
            FileConfig::load(&global).unwrap().auto_update_check,
            Some(false)
        );
        // The repo file must not pick up the app-level setting.
        let repo = FileConfig::load(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(repo.auto_update_check, None);
        // Reading back shows the value the editor should display.
        let read = repo_config_fields(dir.path(), Some(&global)).unwrap();
        assert_eq!(read.auto_update_check, "false");

        // Clearing it removes the key so the built-in default applies again.
        edits.auto_update_check = String::new();
        save_config_edits(dir.path(), Some(&global), &edits).unwrap();
        assert_eq!(FileConfig::load(&global).unwrap().auto_update_check, None);
        assert_eq!(
            repo_config_fields(dir.path(), Some(&global))
                .unwrap()
                .auto_update_check,
            ""
        );
    }

    #[test]
    fn auto_update_check_accepts_the_spellings_people_type() {
        for raw in ["true", "TRUE", "yes", "on", "1"] {
            assert!(parse_bool(raw).unwrap(), "{raw} should parse as true");
        }
        for raw in ["false", "No", "off", "0"] {
            assert!(!parse_bool(raw).unwrap(), "{raw} should parse as false");
        }
        let err = parse_bool("maybe").unwrap_err().to_string();
        assert!(err.contains("expected true or false"), "{err}");
    }

    #[test]
    fn setting_auto_update_check_writes_a_toml_boolean() {
        let mut doc = DocumentMut::new();
        apply_set(&mut doc, "auto_update_check", "off").unwrap();
        assert_eq!(doc.to_string().trim(), "auto_update_check = false");
        // It must round-trip through the strict FileConfig parser.
        let cfg: FileConfig = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.auto_update_check, Some(false));
        assert!(apply_unset(&mut doc, "auto_update_check").unwrap());
        assert_eq!(doc.to_string().trim(), "");
    }

    #[test]
    fn init_refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "worktree_dir = \"home\"\n").unwrap();
        let mut input = std::io::Cursor::new("");
        let err = init(dir.path(), false, &mut input, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // --force starts over.
        init(
            dir.path(),
            true,
            &mut std::io::Cursor::new(""),
            &mut Vec::new(),
        )
        .unwrap();
        let cfg = FileConfig::load(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(cfg.worktree_dir.as_deref(), Some("sibling"));
    }
}
