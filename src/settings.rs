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
use crate::config::{
    self, CONFIG_FILE, CommandMode, Config, DEFAULT_LOCATION, FileConfig, LOCATION_PRESETS,
    OpenCommand, OpenCommandList,
};
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
        "commands the TUI's o key runs for a worktree, comma separated; \
         supports {path}, {name}, {branch}, {status}",
    ),
    (
        "auto_update_check",
        "check GitHub for a newer wtm when the TUI starts (true/false)",
    ),
    (
        "diff_theme",
        "diff syntax-highlight theme: eighties, mocha, ocean, solarized, github",
    ),
    (
        "worktrees_layout",
        "layout of the TUI's Worktrees tab: two_panel or three_panel",
    ),
    (
        "branches_refresh_mins",
        "minutes the Branches tab keeps its list before refreshing (default 10)",
    ),
    (
        "diff_line_numbers",
        "show a line-number gutter in the diff pane (true/false, default true)",
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

/// Local config files that git normally ignores, so a fresh worktree won't have
/// them unless `setup.copy` brings them across. Only the ones actually present
/// in the repo root are suggested.
const COPY_CANDIDATES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.development.local",
    ".dev.vars",
    ".envrc",
    ".npmrc",
    ".tool-versions",
];

/// Marker file -> install command, checked in order. The first match wins per
/// ecosystem so a repo with both `package-lock.json` and `yarn.lock` doesn't get
/// two competing installs suggested.
const RUN_CANDIDATES: &[(&str, &str)] = &[
    ("pnpm-lock.yaml", "pnpm install"),
    ("yarn.lock", "yarn install"),
    ("bun.lockb", "bun install"),
    ("bun.lock", "bun install"),
    ("package-lock.json", "npm install"),
    ("uv.lock", "uv sync"),
    ("poetry.lock", "poetry install"),
    ("Gemfile.lock", "bundle install"),
    ("composer.lock", "composer install"),
    ("go.mod", "go mod download"),
    ("mix.lock", "mix deps.get"),
];

/// Files in `repo_root` worth copying into new worktrees: the known local-config
/// names that exist here. Used to pre-fill the setup wizard's answer so the user
/// edits a sensible list instead of starting from a blank line.
pub fn suggest_copy_files(repo_root: &Path) -> Vec<String> {
    COPY_CANDIDATES
        .iter()
        .filter(|name| repo_root.join(name).is_file())
        .map(|name| name.to_string())
        .collect()
}

/// Setup commands worth running in new worktrees, inferred from the lockfiles
/// and manifests in `repo_root`. At most one command per ecosystem.
pub fn suggest_run_commands(repo_root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (marker, command) in RUN_CANDIDATES {
        if !repo_root.join(marker).is_file() {
            continue;
        }
        // A repo can legitimately use two ecosystems (say Node and Go), but not
        // two package managers for the same one; dedupe on the command itself.
        if !out.iter().any(|c| c == command) {
            out.push(command.to_string());
        }
    }
    // Node lockfiles are mutually exclusive in practice, so keep only the first.
    let node = ["pnpm install", "yarn install", "bun install", "npm install"];
    let mut seen_node = false;
    out.retain(|cmd| {
        if !node.contains(&cmd.as_str()) {
            return true;
        }
        let first = !seen_node;
        seen_node = true;
        first
    });
    out
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
/// `auto_update_check`, `diff_theme`, `worktrees_layout`, and
/// `branches_refresh_mins` are the exceptions: they govern wtm itself rather
/// than one repo, so the editor shows the *effective* merged value (so a stale
/// repo override cannot make the row disagree with what the TUI is drawing)
/// and Settings save writes them to `global_config`.
pub fn repo_config_fields(
    repo_root: &Path,
    global_config: Option<&Path>,
) -> Result<RepoConfigFields> {
    let cfg = FileConfig::load(&repo_root.join(CONFIG_FILE))?;
    let auto_update_check = effective_auto_update_check(global_config, &cfg);
    let diff_theme = effective_diff_theme(global_config, &cfg);
    let worktrees_layout = effective_worktrees_layout(global_config, &cfg);
    let branches_refresh_mins = effective_branches_refresh_mins(global_config, &cfg);
    let diff_line_numbers = effective_diff_line_numbers(global_config, &cfg);
    let setup = cfg.setup.clone().unwrap_or_default();
    let copy = setup
        .copy
        .unwrap_or_default()
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let run = setup.run.unwrap_or_default().join(", ");
    // Both layers are shown in one list, each entry flagged with the file it
    // came from, so the editor can move a command between global and repo by
    // flipping that flag.
    let open_command: Vec<OpenCommand> = load_global_file(global_config)
        .open_command
        .map(|OpenCommandList(cmds)| cmds)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.global(true))
        .chain(
            cfg.open_command
                .clone()
                .map(|OpenCommandList(cmds)| cmds)
                .unwrap_or_default(),
        )
        .collect();
    Ok(RepoConfigFields {
        worktree_dir: cfg.worktree_dir.unwrap_or_default(),
        open_command,
        auto_update_check,
        diff_theme,
        worktrees_layout,
        branches_refresh_mins,
        diff_line_numbers,
        copy,
        run,
    })
}

/// Loads the optional global file, treating a missing/unreadable path as empty
/// so the Settings tab still opens.
fn load_global_file(global_config: Option<&Path>) -> FileConfig {
    global_config
        .and_then(|path| FileConfig::load(path).ok())
        .unwrap_or_default()
}

/// Effective `auto_update_check` for the editor: `""` when the built-in
/// default applies, otherwise `"true"`/`"false"`.
fn effective_auto_update_check(global_config: Option<&Path>, repo: &FileConfig) -> String {
    let merged = Config::merge(load_global_file(global_config), repo.clone());
    match merged.auto_update_check_source {
        config::Source::Default => String::new(),
        _ => merged
            .auto_update_check
            .unwrap_or(config::DEFAULT_AUTO_UPDATE_CHECK)
            .to_string(),
    }
}

/// Effective `diff_theme` for the editor: `""` when the built-in default
/// applies, otherwise the theme id in force.
fn effective_diff_theme(global_config: Option<&Path>, repo: &FileConfig) -> String {
    let merged = Config::merge(load_global_file(global_config), repo.clone());
    match merged.diff_theme_source {
        config::Source::Default => String::new(),
        _ => merged
            .diff_theme
            .unwrap_or_else(|| config::DEFAULT_DIFF_THEME.to_string()),
    }
}

/// Effective `worktrees_layout` for the editor: `""` when the two-panel
/// default applies, otherwise the layout id currently drawn by the TUI.
fn effective_worktrees_layout(global_config: Option<&Path>, repo: &FileConfig) -> String {
    let merged = Config::merge(load_global_file(global_config), repo.clone());
    match merged.worktrees_layout_source {
        config::Source::Default => String::new(),
        _ => merged.worktrees_layout().as_str().to_string(),
    }
}

/// Effective `branches_refresh_mins` for the editor: `""` when the built-in
/// default applies, otherwise the configured minutes as a string.
fn effective_branches_refresh_mins(global_config: Option<&Path>, repo: &FileConfig) -> String {
    let merged = Config::merge(load_global_file(global_config), repo.clone());
    match merged.branches_refresh_mins_source {
        config::Source::Default => String::new(),
        _ => merged.branches_refresh_mins().to_string(),
    }
}

/// Effective `diff_line_numbers` for the editor: `""` when the built-in default
/// applies, otherwise `"true"`/`"false"`.
fn effective_diff_line_numbers(global_config: Option<&Path>, repo: &FileConfig) -> String {
    let merged = Config::merge(load_global_file(global_config), repo.clone());
    match merged.diff_line_numbers_source {
        config::Source::Default => String::new(),
        _ => merged.diff_line_numbers().to_string(),
    }
}

/// The settings the TUI config editor shows, each empty when unset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoConfigFields {
    pub worktree_dir: String,
    /// One entry per configured open command, from both config layers. Kept as
    /// a list rather than a joined string so a command containing a comma
    /// survives a round trip through the TUI's list editor, and so each entry
    /// can carry its run mode and which file it belongs in.
    pub open_command: Vec<OpenCommand>,
    /// `""`, `"true"`, or `"false"`; lives in the global config.
    pub auto_update_check: String,
    /// Diff theme short id, or `""` for the default; lives in the global config.
    pub diff_theme: String,
    /// `""`, `"two_panel"`, or `"three_panel"`; lives in the global config.
    pub worktrees_layout: String,
    /// Minutes the Branches tab caches its list, or `""` for the default;
    /// lives in the global config.
    pub branches_refresh_mins: String,
    /// `""`, `"true"`, or `"false"`; lives in the global config.
    pub diff_line_numbers: String,
    pub copy: String,
    pub run: String,
}

/// Applies edits from the TUI config editor, preserving comments and the
/// surrounding TOML. An empty value unsets the key so the default (or global
/// value) applies again. Returns the repo file's path.
///
/// Repo settings go to the repo's `.wtm.toml`; `auto_update_check`,
/// `diff_theme`, `worktrees_layout`, and `branches_refresh_mins` go to
/// `global_config`, since they are about wtm rather than about this
/// repository. Any repo-level copies of those keys are cleared on save so a
/// prior `wtm config set` (without `-g`) cannot keep overriding the value the
/// user just chose in Settings.
pub fn save_config_edits(
    repo_root: &Path,
    global_config: Option<&Path>,
    fields: &RepoConfigFields,
) -> Result<PathBuf> {
    let file = repo_root.join(CONFIG_FILE);
    let mut doc = load_doc(&file)?;
    set_or_unset(&mut doc, "worktree_dir", &fields.worktree_dir)?;
    // Commands are split by their `global` flag: the ones marked global belong
    // in the user-wide file so they show up in every repo, the rest here.
    let (global_cmds, repo_cmds): (Vec<_>, Vec<_>) =
        fields.open_command.iter().partition(|c| c.global);
    set_or_unset_commands(&mut doc, &repo_cmds)?;
    set_or_unset(&mut doc, "setup.copy", &fields.copy)?;
    set_or_unset(&mut doc, "setup.run", &fields.run)?;
    // Drop repo overrides of UI prefs; Settings owns them at the global layer.
    apply_unset(&mut doc, "auto_update_check")?;
    apply_unset(&mut doc, "diff_theme")?;
    apply_unset(&mut doc, "worktrees_layout")?;
    apply_unset(&mut doc, "branches_refresh_mins")?;
    apply_unset(&mut doc, "diff_line_numbers")?;
    save_doc(&file, &doc)?;
    if let Some(path) = global_config {
        save_global_setting(path, "auto_update_check", &fields.auto_update_check)?;
        save_global_setting(path, "diff_theme", &fields.diff_theme)?;
        save_global_setting(path, "worktrees_layout", &fields.worktrees_layout)?;
        save_global_setting(path, "branches_refresh_mins", &fields.branches_refresh_mins)?;
        save_global_setting(path, "diff_line_numbers", &fields.diff_line_numbers)?;
        save_global_commands(path, &global_cmds)?;
    }
    Ok(file)
}

/// Writes the globally-saved open commands to the user-wide config at `path`,
/// clearing the key when none are marked global. A save with nothing to write
/// and nothing to clear leaves the file alone, so using Settings in a repo with
/// only repo-level commands never creates an empty global config.
fn save_global_commands(path: &Path, commands: &[&OpenCommand]) -> Result<()> {
    let mut doc = load_doc(path)?;
    if commands.is_empty() && doc.get("open_command").is_none() {
        return Ok(());
    }
    set_or_unset_commands(&mut doc, commands)?;
    save_doc(path, &doc)
}

/// Writes (or clears) one wtm-wide `key` in the global config at `path`,
/// leaving the file untouched when nothing about that key changed.
fn save_global_setting(path: &Path, key: &str, raw: &str) -> Result<()> {
    let mut doc = load_doc(path)?;
    let changed = if raw.trim().is_empty() {
        apply_unset(&mut doc, key)?
    } else {
        apply_set(&mut doc, key, raw)?;
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

/// Writes the `open_command` key for `commands`, or unsets it when there are
/// none. A single background command still writes as a bare TOML string (the
/// long-standing shape); anything else writes an array whose entries are bare
/// strings for background commands and `{ command, mode }` tables for the
/// rest, so a plain list stays readable. Commands are taken verbatim, so one
/// containing a comma stays a single entry.
fn set_or_unset_commands(doc: &mut DocumentMut, commands: &[&OpenCommand]) -> Result<()> {
    let commands: Vec<&OpenCommand> = commands
        .iter()
        .copied()
        .filter(|c| !c.command.trim().is_empty())
        .collect();
    match commands.as_slice() {
        [] => {
            apply_unset(doc, "open_command")?;
        }
        [one] if one.mode == CommandMode::Background => {
            doc["open_command"] = toml_value(one.command.trim());
        }
        many => {
            let mut array = toml_edit::Array::new();
            for cmd in many {
                let command = cmd.command.trim();
                if cmd.mode == CommandMode::Background {
                    array.push(command);
                } else {
                    let mut table = toml_edit::InlineTable::new();
                    table.insert("command", command.into());
                    table.insert("mode", cmd.mode.as_str().into());
                    array.push(table);
                }
            }
            doc["open_command"] = toml_value(array);
        }
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
            "diff_theme": {
                "value": cfg.diff_theme(),
                "source": cfg.diff_theme_source,
            },
            "worktrees_layout": {
                "value": cfg.worktrees_layout().as_str(),
                "source": cfg.worktrees_layout_source,
            },
            "branches_refresh_mins": {
                "value": cfg.branches_refresh_mins(),
                "source": cfg.branches_refresh_mins_source,
            },
            "diff_line_numbers": {
                "value": cfg.diff_line_numbers(),
                "source": cfg.diff_line_numbers_source,
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
    // Commands layer rather than override, and each carries a scope and a run
    // mode, so they get a line each instead of one squashed value.
    if cfg.open_command.is_empty() {
        println!("  open_command = []   ({})", cfg.open_command_source);
    } else {
        println!("  open_command:");
        for cmd in &cfg.open_command {
            let scope = if cmd.global { "global" } else { "repo" };
            println!("      {:?}   ({scope}, {})", cmd.command, cmd.mode.as_str());
        }
    }
    println!(
        "  auto_update_check = {}   ({})",
        cfg.auto_update_check(),
        cfg.auto_update_check_source
    );
    println!(
        "  diff_theme = {:?}   ({})",
        cfg.diff_theme(),
        cfg.diff_theme_source
    );
    println!(
        "  worktrees_layout = {:?}   ({})",
        cfg.worktrees_layout().as_str(),
        cfg.worktrees_layout_source
    );
    println!(
        "  branches_refresh_mins = {}   ({})",
        cfg.branches_refresh_mins(),
        cfg.branches_refresh_mins_source
    );
    println!(
        "  diff_line_numbers = {}   ({})",
        cfg.diff_line_numbers(),
        cfg.diff_line_numbers_source
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
        // `get` prints one value per line, so a command list reduces to its
        // templates; `--json` above keeps the full entries.
        "open_command" => json!(
            cfg.open_command
                .iter()
                .map(|c| c.command.clone())
                .collect::<Vec<_>>()
        ),
        "auto_update_check" => json!(cfg.auto_update_check()),
        "diff_theme" => json!(cfg.diff_theme()),
        "worktrees_layout" => json!(cfg.worktrees_layout().as_str()),
        "branches_refresh_mins" => json!(cfg.branches_refresh_mins()),
        "diff_line_numbers" => json!(cfg.diff_line_numbers()),
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
            let items = split_list(raw);
            if items.len() == 1 {
                doc["open_command"] = toml_value(&items[0]);
            } else {
                doc["open_command"] = toml_value(to_array(&items));
            }
        }
        "auto_update_check" => {
            doc["auto_update_check"] = toml_value(parse_bool(raw)?);
        }
        "diff_theme" => {
            doc["diff_theme"] = toml_value(parse_diff_theme(raw)?);
        }
        "worktrees_layout" => {
            doc["worktrees_layout"] = toml_value(parse_worktrees_layout(raw)?);
        }
        "branches_refresh_mins" => {
            let mins = parse_branches_refresh_mins(raw)?;
            doc["branches_refresh_mins"] = toml_value(mins as i64);
        }
        "diff_line_numbers" => {
            doc["diff_line_numbers"] = toml_value(parse_bool(raw)?);
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
        "diff_theme" => doc.remove("diff_theme").is_some(),
        "worktrees_layout" => doc.remove("worktrees_layout").is_some(),
        "branches_refresh_mins" => doc.remove("branches_refresh_mins").is_some(),
        "diff_line_numbers" => doc.remove("diff_line_numbers").is_some(),
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

/// Accepts a known diff-theme short id (case-insensitive).
fn parse_diff_theme(raw: &str) -> Result<&str> {
    const KNOWN: &[&str] = &["eighties", "mocha", "ocean", "solarized", "github"];
    let trimmed = raw.trim();
    if let Some(id) = KNOWN.iter().find(|id| trimmed.eq_ignore_ascii_case(id)) {
        return Ok(*id);
    }
    bail!(
        "unknown diff theme {trimmed:?}; choose one of: {}",
        KNOWN.join(", ")
    )
}

/// Accepts a known Worktrees-tab layout (case-insensitive), also allowing the
/// hyphenated and spaced spellings people reach for.
fn parse_worktrees_layout(raw: &str) -> Result<&'static str> {
    let trimmed = raw.trim().replace(['-', ' '], "_");
    if let Some((id, _)) = config::WORKTREES_LAYOUTS
        .iter()
        .find(|(id, _)| trimmed.eq_ignore_ascii_case(id))
    {
        return Ok(*id);
    }
    bail!(
        "unknown layout {:?}; choose one of: {}",
        raw.trim(),
        config::WORKTREES_LAYOUTS
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Parses a positive integer (minutes) for the Branches tab cache timeout.
fn parse_branches_refresh_mins(raw: &str) -> Result<u64> {
    let trimmed = raw.trim();
    let mins: u64 = trimmed
        .parse()
        .with_context(|| format!("expected a number of minutes, got {raw:?}"))?;
    if mins == 0 {
        bail!("branches_refresh_mins must be at least 1");
    }
    Ok(mins)
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

    /// Only the local-config files actually present are suggested, in the order
    /// the candidate list declares them.
    #[test]
    fn suggest_copy_files_lists_what_is_there() {
        let dir = tempfile::tempdir().unwrap();
        assert!(suggest_copy_files(dir.path()).is_empty());

        std::fs::write(dir.path().join(".env.local"), "").unwrap();
        std::fs::write(dir.path().join(".env"), "").unwrap();
        // Not a candidate, and a directory that happens to share a name.
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        std::fs::create_dir(dir.path().join(".envrc")).unwrap();

        assert_eq!(suggest_copy_files(dir.path()), vec![".env", ".env.local"]);
    }

    /// One install command per ecosystem: competing Node lockfiles collapse to
    /// the most specific one, while a genuinely second ecosystem is kept.
    #[test]
    fn suggest_run_commands_picks_one_per_ecosystem() {
        let dir = tempfile::tempdir().unwrap();
        assert!(suggest_run_commands(dir.path()).is_empty());

        std::fs::write(dir.path().join("package-lock.json"), "").unwrap();
        assert_eq!(suggest_run_commands(dir.path()), vec!["npm install"]);

        // pnpm outranks npm, and only one Node install is suggested.
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(suggest_run_commands(dir.path()), vec!["pnpm install"]);

        // A second ecosystem alongside it is additive.
        std::fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(
            suggest_run_commands(dir.path()),
            vec!["pnpm install", "go mod download"]
        );
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

    /// Just the templates of a fields value's commands, for asserting on the
    /// list without spelling out each entry's mode and scope.
    fn command_texts(fields: &RepoConfigFields) -> Vec<String> {
        fields
            .open_command
            .iter()
            .map(|c| c.command.clone())
            .collect()
    }

    /// Editor fields with everything unset, for building one-field cases.
    fn fields(
        worktree_dir: &str,
        open_command: &[&str],
        copy: &str,
        run: &str,
    ) -> RepoConfigFields {
        RepoConfigFields {
            worktree_dir: worktree_dir.to_string(),
            open_command: open_command.iter().map(|c| OpenCommand::new(*c)).collect(),
            auto_update_check: String::new(),
            diff_theme: String::new(),
            worktrees_layout: String::new(),
            branches_refresh_mins: String::new(),
            diff_line_numbers: String::new(),
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
        assert!(fields.open_command.is_empty());
        assert_eq!(fields.copy, ".env, config/.env");
        assert_eq!(fields.run, "");
        // With no global config to read, the toggle shows as unset (default).
        assert_eq!(fields.auto_update_check, "");
    }

    #[test]
    fn save_and_read_open_command() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CONFIG_FILE);
        save_config_edits(dir.path(), None, &fields("", &["cursor ."], "", "")).unwrap();
        let cfg = FileConfig::load(&file).unwrap();
        assert_eq!(
            cfg.open_command,
            Some(OpenCommandList(vec!["cursor .".into()]))
        );
        // Clearing it unsets the key again.
        save_config_edits(dir.path(), None, &fields("", &[], "", "")).unwrap();
        let cfg = FileConfig::load(&file).unwrap();
        assert_eq!(cfg.open_command, None);
    }

    #[test]
    fn save_open_command_writes_an_array_for_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CONFIG_FILE);
        save_config_edits(
            dir.path(),
            None,
            &fields("", &["open {path}", "cursor {path}"], "", ""),
        )
        .unwrap();
        let cfg = FileConfig::load(&file).unwrap();
        assert_eq!(
            cfg.open_command,
            Some(OpenCommandList(vec![
                "open {path}".into(),
                "cursor {path}".into(),
            ]))
        );
        let fields = repo_config_fields(dir.path(), None).unwrap();
        assert_eq!(command_texts(&fields), ["open {path}", "cursor {path}"]);
    }

    /// A command marked global is written to the user-wide config, not the
    /// repo's, and comes back marked global so the checkbox stays ticked.
    #[test]
    fn save_writes_global_commands_to_the_global_file() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let mut fields = fields("", &["npm run dev"], "", "");
        fields
            .open_command
            .push(OpenCommand::new("cursor {path}").global(true));
        save_config_edits(dir.path(), Some(&global), &fields).unwrap();

        let repo_cfg = FileConfig::load(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(
            repo_cfg.open_command,
            Some(OpenCommandList(vec![OpenCommand::new("npm run dev")])),
            "the repo file keeps only the repo-level command"
        );
        let global_cfg = FileConfig::load(&global).unwrap();
        assert_eq!(
            global_cfg.open_command,
            Some(OpenCommandList(vec![OpenCommand::new("cursor {path}")])),
            "the global command lands in the global file"
        );

        let reloaded = repo_config_fields(dir.path(), Some(&global)).unwrap();
        assert_eq!(
            reloaded
                .open_command
                .iter()
                .map(|c| (c.command.as_str(), c.global))
                .collect::<Vec<_>>(),
            [("cursor {path}", true), ("npm run dev", false)]
        );
    }

    /// A terminal-mode command round-trips as a `{ command, mode }` table
    /// while its background neighbours stay bare strings.
    #[test]
    fn save_writes_a_mode_table_only_for_terminal_commands() {
        let dir = tempfile::tempdir().unwrap();
        let mut fields = fields("", &["cursor {path}"], "", "");
        fields
            .open_command
            .push(OpenCommand::new("nvim {path}").with_mode(CommandMode::Terminal));
        save_config_edits(dir.path(), None, &fields).unwrap();

        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(text.contains(r#""cursor {path}""#), "{text}");
        assert!(text.contains(r#"mode = "terminal""#), "{text}");

        let reloaded = repo_config_fields(dir.path(), None).unwrap();
        assert_eq!(
            reloaded
                .open_command
                .iter()
                .map(|c| (c.command.as_str(), c.mode))
                .collect::<Vec<_>>(),
            [
                ("cursor {path}", CommandMode::Background),
                ("nvim {path}", CommandMode::Terminal),
            ]
        );
    }

    /// The editor path takes the list as-is, so a command with a comma in it
    /// (a shell one-liner, say) stays one entry instead of being split.
    #[test]
    fn save_open_command_keeps_commands_containing_commas_whole() {
        let dir = tempfile::tempdir().unwrap();
        let commands = ["sh -c 'cd {path}, npm start'", "code --goto {path}:1,1"];
        save_config_edits(dir.path(), None, &fields("", &commands, "", "")).unwrap();
        let fields = repo_config_fields(dir.path(), None).unwrap();
        assert_eq!(command_texts(&fields), commands);
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
        save_config_edits(dir.path(), None, &fields("inside", &[], "", "npm install")).unwrap();
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

        let mut edits = fields("", &[], "", "");
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
    fn worktrees_layout_round_trips_through_the_global_config() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        std::fs::write(&global, "# global notes\nworktree_dir = \"home\"\n").unwrap();

        let mut edits = fields("", &[], "", "");
        edits.worktrees_layout = "three_panel".to_string();
        save_config_edits(dir.path(), Some(&global), &edits).unwrap();

        let text = std::fs::read_to_string(&global).unwrap();
        assert!(text.contains("# global notes"), "comment lost: {text}");
        assert_eq!(
            FileConfig::load(&global).unwrap().worktrees_layout,
            Some(config::WorktreesLayout::ThreePanel)
        );
        // The layout is about wtm, not this repo, so the repo file stays clean.
        let repo = FileConfig::load(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(repo.worktrees_layout, None);
        let read = repo_config_fields(dir.path(), Some(&global)).unwrap();
        assert_eq!(read.worktrees_layout, "three_panel");

        // Clearing it removes the key so the two-panel default applies again.
        edits.worktrees_layout = String::new();
        save_config_edits(dir.path(), Some(&global), &edits).unwrap();
        assert_eq!(FileConfig::load(&global).unwrap().worktrees_layout, None);
        assert_eq!(
            repo_config_fields(dir.path(), Some(&global))
                .unwrap()
                .worktrees_layout,
            ""
        );
    }

    /// A prior `wtm config set worktrees_layout` (repo layer) must not keep
    /// winning after the user picks a layout in Settings and saves.
    #[test]
    fn settings_save_clears_a_repo_worktrees_layout_override() {
        let dir = tempfile::tempdir().unwrap();
        let repo_file = dir.path().join(CONFIG_FILE);
        std::fs::write(
            &repo_file,
            "worktree_dir = \"sibling\"\nworktrees_layout = \"two_panel\"\n",
        )
        .unwrap();
        let global = dir.path().join("global.toml");
        std::fs::write(&global, "worktrees_layout = \"three_panel\"\n").unwrap();

        // Effective value is the repo override, which is what the editor shows.
        let read = repo_config_fields(dir.path(), Some(&global)).unwrap();
        assert_eq!(read.worktrees_layout, "two_panel");

        let mut edits = fields("sibling", &[], "", "");
        edits.worktrees_layout = "three_panel".to_string();
        save_config_edits(dir.path(), Some(&global), &edits).unwrap();

        let repo = FileConfig::load(&repo_file).unwrap();
        assert_eq!(
            repo.worktrees_layout, None,
            "repo override must be cleared so Settings takes effect"
        );
        assert_eq!(
            FileConfig::load(&global).unwrap().worktrees_layout,
            Some(config::WorktreesLayout::ThreePanel)
        );
        let cfg = Config::merge(
            FileConfig::load(&global).unwrap(),
            FileConfig::load(&repo_file).unwrap(),
        );
        assert_eq!(cfg.worktrees_layout(), config::WorktreesLayout::ThreePanel);
        assert_eq!(cfg.worktrees_layout_source, config::Source::Global);
    }

    #[test]
    fn setting_worktrees_layout_writes_a_known_value() {
        let mut doc = DocumentMut::new();
        apply_set(&mut doc, "worktrees_layout", "Three-Panel").unwrap();
        assert_eq!(doc.to_string().trim(), "worktrees_layout = \"three_panel\"");
        // It must round-trip through the strict FileConfig parser.
        let cfg: FileConfig = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(
            cfg.worktrees_layout,
            Some(config::WorktreesLayout::ThreePanel)
        );
        assert!(apply_unset(&mut doc, "worktrees_layout").unwrap());
        assert_eq!(doc.to_string().trim(), "");

        let err = apply_set(&mut doc, "worktrees_layout", "four_panel")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown layout"), "{err}");
        assert!(err.contains("two_panel"), "{err}");
    }

    #[test]
    fn setting_branches_refresh_mins_writes_an_integer() {
        let mut doc = DocumentMut::new();
        apply_set(&mut doc, "branches_refresh_mins", "15").unwrap();
        assert_eq!(doc.to_string().trim(), "branches_refresh_mins = 15");
        let cfg: FileConfig = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.branches_refresh_mins, Some(15));
        assert!(apply_unset(&mut doc, "branches_refresh_mins").unwrap());
        assert_eq!(doc.to_string().trim(), "");

        let err = apply_set(&mut doc, "branches_refresh_mins", "0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least 1"), "{err}");
        let err = apply_set(&mut doc, "branches_refresh_mins", "nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("number of minutes"), "{err}");
    }

    #[test]
    fn branches_refresh_mins_round_trips_through_the_global_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "worktree_dir = \"sibling\"\n").unwrap();
        let global = dir.path().join("global.toml");

        let mut edits = fields("", &[], "", "");
        edits.branches_refresh_mins = "30".to_string();
        save_config_edits(dir.path(), Some(&global), &edits).unwrap();

        assert_eq!(
            FileConfig::load(&global).unwrap().branches_refresh_mins,
            Some(30)
        );
        let read = repo_config_fields(dir.path(), Some(&global)).unwrap();
        assert_eq!(read.branches_refresh_mins, "30");

        edits.branches_refresh_mins = String::new();
        save_config_edits(dir.path(), Some(&global), &edits).unwrap();
        assert_eq!(
            FileConfig::load(&global).unwrap().branches_refresh_mins,
            None
        );
        assert_eq!(
            repo_config_fields(dir.path(), Some(&global))
                .unwrap()
                .branches_refresh_mins,
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
