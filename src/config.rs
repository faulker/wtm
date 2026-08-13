//! Layered configuration: a user-wide config file plus the per-repo
//! `.wtm.toml`, merged so repo settings win over global ones.
//!
//! The `worktree_dir` setting decides where new worktrees go. It accepts a
//! predefined rule (`sibling`, `inside`, `home`) or a manual path (absolute,
//! `~/...`, or relative to the repo root) where `{repo}` expands to the repo
//! directory name.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use toml_edit::{DocumentMut, value as toml_value};

pub const CONFIG_FILE: &str = ".wtm.toml";

/// The rule used when `worktree_dir` isn't set anywhere.
pub const DEFAULT_LOCATION: &str = "sibling";

/// Whether the start-up update check runs when `auto_update_check` isn't set
/// anywhere. On by default: the check is backgrounded and never blocks the UI,
/// and a stale worktree manager is worth telling the user about.
pub const DEFAULT_AUTO_UPDATE_CHECK: bool = true;

/// Default short id for the diff syntax-highlight theme when `diff_theme`
/// isn't set. Kept in sync with `tui::highlight::DEFAULT_DIFF_THEME`.
pub const DEFAULT_DIFF_THEME: &str = "eighties";

/// How long the Branches tab keeps a loaded list before refreshing on its
/// own, in minutes. Manual `r` and mutations still refresh immediately.
pub const DEFAULT_BRANCHES_REFRESH_MINS: u64 = 10;

/// Whether the diff pane draws a line-number gutter when `diff_line_numbers`
/// isn't set anywhere. On by default: the numbers make it possible to talk
/// about a diff by line, and the gutter is narrow.
pub const DEFAULT_DIFF_LINE_NUMBERS: bool = true;

/// How the TUI's Worktrees tab is laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreesLayout {
    /// The worktree table on top, a read-only changed-file preview below it.
    #[default]
    TwoPanel,
    /// A compact worktree list on top, with the Changes tab's file list and
    /// diff filling the bottom half. The Changes tab is hidden in this layout.
    ThreePanel,
}

impl WorktreesLayout {
    /// The config-file spelling of this layout, matching the serde renaming.
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreesLayout::TwoPanel => "two_panel",
            WorktreesLayout::ThreePanel => "three_panel",
        }
    }
}

/// The `worktrees_layout` values `wtm config` accepts and the TUI's Settings
/// row cycles through, each with a human-readable label. The first entry is
/// the default.
pub const WORKTREES_LAYOUTS: &[(&str, &str)] =
    &[("two_panel", "two panels"), ("three_panel", "three panels")];

/// Human-readable label for a `worktrees_layout` value, falling back to the
/// raw value for anything unrecognised.
pub fn worktrees_layout_label(id: &str) -> &str {
    WORKTREES_LAYOUTS
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, label)| *label)
        .unwrap_or(id)
}

/// Predefined location rules accepted by `worktree_dir`, with a short
/// human-readable label for each.
pub const LOCATION_PRESETS: &[(&str, &str)] = &[
    ("sibling", "next to the repo"),
    ("inside", "inside the repo (kept out of git status)"),
    ("home", "in your home folder"),
];

/// Where an effective setting's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Default,
    Global,
    Repo,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Source::Default => "default",
            Source::Global => "global",
            Source::Repo => "repo",
        };
        f.write_str(s)
    }
}

/// How a configured open command takes over (or doesn't) when it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandMode {
    /// Spawned detached with its stdio discarded; the TUI stays open. The right
    /// mode for GUI tools (`cursor {path}`) that don't need this terminal.
    #[default]
    Background,
    /// The TUI shuts down and hands this terminal to the command, so an
    /// interactive program (`nvim {path}`, `claude`) can use it directly.
    Terminal,
}

impl CommandMode {
    /// The config-file spelling of this mode, matching the serde renaming.
    pub fn as_str(self) -> &'static str {
        match self {
            CommandMode::Background => "background",
            CommandMode::Terminal => "terminal",
        }
    }

    /// The other mode, for the editor's toggle.
    pub fn toggled(self) -> CommandMode {
        match self {
            CommandMode::Background => CommandMode::Terminal,
            CommandMode::Terminal => CommandMode::Background,
        }
    }
}

/// One configured open command: a shell template plus how it should be run.
///
/// `global` is not stored in the entry itself; it records which config file the
/// command came from (the user-wide one, so it is offered in every repo, or
/// this repo's `.wtm.toml`) and which file a save should write it back to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OpenCommand {
    /// Shell template, before `{path}`/`{name}`/`{branch}`/`{status}` expand.
    pub command: String,
    pub mode: CommandMode,
    /// True when this command lives in the user-wide config rather than in the
    /// repo's `.wtm.toml`.
    pub global: bool,
}

impl OpenCommand {
    /// A background command, the shape a bare TOML string loads as.
    pub fn new(command: impl Into<String>) -> OpenCommand {
        OpenCommand {
            command: command.into(),
            mode: CommandMode::Background,
            global: false,
        }
    }

    pub fn with_mode(mut self, mode: CommandMode) -> OpenCommand {
        self.mode = mode;
        self
    }

    pub fn global(mut self, global: bool) -> OpenCommand {
        self.global = global;
        self
    }
}

impl From<&str> for OpenCommand {
    /// A bare template is a background, repo-level command: the shape a plain
    /// TOML string loads as, and the default for a newly typed one.
    fn from(command: &str) -> OpenCommand {
        OpenCommand::new(command)
    }
}

/// The TOML table form of an entry: `{ command = "…", mode = "terminal" }`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenCommandTable {
    command: String,
    #[serde(default)]
    mode: CommandMode,
}

/// One or more open-command templates. A TOML string still loads as a
/// single-entry list so existing configs keep working; an array holds several
/// commands the TUI's `o` key can pick from, each either a bare string or a
/// `{ command = "…", mode = "…" }` table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenCommandList(pub Vec<OpenCommand>);

impl<'de> Deserialize<'de> for OpenCommandList {
    /// Accepts `"cursor ."`, `["open {path}", "cursor {path}"]`, or an array
    /// mixing strings with `{ command, mode }` tables.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OpenCommandVisitor;
        impl<'de> Visitor<'de> for OpenCommandVisitor {
            type Value = OpenCommandList;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string, or an array of strings and { command, mode } tables")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(OpenCommandList(vec![OpenCommand::new(v)]))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(OpenCommandList(vec![OpenCommand::new(v)]))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(item) = seq.next_element::<OpenCommandEntry>()? {
                    out.push(match item {
                        OpenCommandEntry::Plain(command) => OpenCommand::new(command),
                        OpenCommandEntry::Table(t) => OpenCommand::new(t.command).with_mode(t.mode),
                    });
                }
                Ok(OpenCommandList(out))
            }
        }
        /// One array element: a bare template or a table with a mode.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OpenCommandEntry {
            Plain(String),
            Table(OpenCommandTable),
        }
        deserializer.deserialize_any(OpenCommandVisitor)
    }
}

/// Raw contents of one config file. Every field is optional so a file can set
/// only what it cares about and inherit the rest.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub worktree_dir: Option<String>,
    /// Commands the TUI's open key (`o`) can run for a worktree. Each entry is
    /// a shell template; `{path}`, `{name}`, `{branch}`, and `{status}` expand
    /// before the command runs. A plain string in TOML is treated as one entry.
    pub open_command: Option<OpenCommandList>,
    /// Whether the TUI checks GitHub for a newer wtm on start. Unset means
    /// [`DEFAULT_AUTO_UPDATE_CHECK`].
    pub auto_update_check: Option<bool>,
    /// Diff syntax-highlight theme short id (e.g. `eighties`, `ocean`). Unset
    /// means [`DEFAULT_DIFF_THEME`].
    pub diff_theme: Option<String>,
    /// Layout of the TUI's Worktrees tab. Unset means
    /// [`WorktreesLayout::TwoPanel`].
    pub worktrees_layout: Option<WorktreesLayout>,
    /// Minutes the Branches tab may keep a cached list before refreshing.
    /// Unset means [`DEFAULT_BRANCHES_REFRESH_MINS`].
    pub branches_refresh_mins: Option<u64>,
    /// Whether the diff pane draws a line-number gutter. Unset means
    /// [`DEFAULT_DIFF_LINE_NUMBERS`].
    pub diff_line_numbers: Option<bool>,
    pub setup: Option<FileSetup>,
    /// Runtime map of worktree branch → creation base ref, written by
    /// `wtm create`. Not a user-facing setting; ignored by [`Config::merge`].
    #[serde(default)]
    pub created_from: Option<BTreeMap<String, String>>,
    /// Runtime list of branches the user has archived: they still exist in git
    /// but are hidden from the Branches tab unless "view archived" is on. Not a
    /// user-facing setting; ignored by [`Config::merge`].
    #[serde(default)]
    pub archived_branches: Option<Vec<String>>,
}

/// The `[setup]` section of one config file.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileSetup {
    pub copy: Option<Vec<PathBuf>>,
    pub run: Option<Vec<String>>,
}

impl FileConfig {
    /// Parses one config file; a missing file yields the empty config, but a
    /// malformed file is a hard error.
    pub fn load(path: &Path) -> Result<FileConfig> {
        if !path.exists() {
            return Ok(FileConfig::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid config in {}", path.display()))
    }
}

/// Effective configuration after merging the global config file and the
/// repo's `.wtm.toml` (repo values win). Each field records where its value
/// came from so `wtm config show` can explain the setup.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Raw `worktree_dir` setting; `None` means the `sibling` preset.
    pub worktree_dir: Option<String>,
    pub worktree_dir_source: Source,
    /// Commands the TUI runs from the open key (`o`); empty when unset. Unlike
    /// the other settings these layer additively: the global config's commands
    /// come first (they are offered in every repo), then this repo's own.
    pub open_command: Vec<OpenCommand>,
    pub open_command_source: Source,
    /// Raw `auto_update_check` setting; `None` means
    /// [`DEFAULT_AUTO_UPDATE_CHECK`].
    pub auto_update_check: Option<bool>,
    pub auto_update_check_source: Source,
    /// Raw `diff_theme` setting; `None` means [`DEFAULT_DIFF_THEME`].
    pub diff_theme: Option<String>,
    pub diff_theme_source: Source,
    /// Raw `worktrees_layout` setting; `None` means
    /// [`WorktreesLayout::TwoPanel`].
    pub worktrees_layout: Option<WorktreesLayout>,
    pub worktrees_layout_source: Source,
    /// Raw `branches_refresh_mins` setting; `None` means
    /// [`DEFAULT_BRANCHES_REFRESH_MINS`].
    pub branches_refresh_mins: Option<u64>,
    pub branches_refresh_mins_source: Source,
    /// Raw `diff_line_numbers` setting; `None` means
    /// [`DEFAULT_DIFF_LINE_NUMBERS`].
    pub diff_line_numbers: Option<bool>,
    pub diff_line_numbers_source: Source,
    pub setup: Setup,
    pub copy_source: Source,
    pub run_source: Source,
}

/// Steps run after a new worktree is created.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Setup {
    /// Files copied from the main worktree into the new one (if they exist).
    pub copy: Vec<PathBuf>,
    /// Shell commands run inside the new worktree, in order.
    pub run: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            worktree_dir: None,
            worktree_dir_source: Source::Default,
            open_command: Vec::new(),
            open_command_source: Source::Default,
            auto_update_check: None,
            auto_update_check_source: Source::Default,
            diff_theme: None,
            diff_theme_source: Source::Default,
            worktrees_layout: None,
            worktrees_layout_source: Source::Default,
            branches_refresh_mins: None,
            branches_refresh_mins_source: Source::Default,
            diff_line_numbers: None,
            diff_line_numbers_source: Source::Default,
            setup: Setup::default(),
            copy_source: Source::Default,
            run_source: Source::Default,
        }
    }
}

impl Config {
    /// Loads and merges the global config and `repo_root`'s `.wtm.toml`.
    pub fn load(repo_root: &Path) -> Result<Config> {
        let global = match global_config_path() {
            Some(path) => FileConfig::load(&path)?,
            None => FileConfig::default(),
        };
        let repo = FileConfig::load(&repo_root.join(CONFIG_FILE))?;
        Ok(Config::merge(global, repo))
    }

    /// Merges two config layers; any field set in `repo` wins over `global`.
    pub fn merge(global: FileConfig, repo: FileConfig) -> Config {
        fn pick<T>(global: Option<T>, repo: Option<T>) -> (Option<T>, Source) {
            match (global, repo) {
                (_, Some(v)) => (Some(v), Source::Repo),
                (Some(v), None) => (Some(v), Source::Global),
                (None, None) => (None, Source::Default),
            }
        }
        let (worktree_dir, worktree_dir_source) = pick(global.worktree_dir, repo.worktree_dir);
        // Open commands are the one additive setting: a command saved globally
        // is meant to be available in every repo, so a repo's own list adds to
        // it rather than replacing it. Each entry remembers which file it came
        // from so the editor can save it back to the right one.
        let global_cmds = global.open_command.map(|OpenCommandList(c)| c);
        let repo_cmds = repo.open_command.map(|OpenCommandList(c)| c);
        let open_command_source = match (&global_cmds, &repo_cmds) {
            (_, Some(_)) => Source::Repo,
            (Some(_), None) => Source::Global,
            (None, None) => Source::Default,
        };
        let open_command: Vec<OpenCommand> = global_cmds
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.global(true))
            .chain(repo_cmds.unwrap_or_default())
            .collect();
        let (auto_update_check, auto_update_check_source) =
            pick(global.auto_update_check, repo.auto_update_check);
        let (diff_theme, diff_theme_source) = pick(global.diff_theme, repo.diff_theme);
        let (worktrees_layout, worktrees_layout_source) =
            pick(global.worktrees_layout, repo.worktrees_layout);
        let (branches_refresh_mins, branches_refresh_mins_source) =
            pick(global.branches_refresh_mins, repo.branches_refresh_mins);
        let (diff_line_numbers, diff_line_numbers_source) =
            pick(global.diff_line_numbers, repo.diff_line_numbers);
        let global_setup = global.setup.unwrap_or_default();
        let repo_setup = repo.setup.unwrap_or_default();
        let (copy, copy_source) = pick(global_setup.copy, repo_setup.copy);
        let (run, run_source) = pick(global_setup.run, repo_setup.run);
        Config {
            worktree_dir,
            worktree_dir_source,
            open_command,
            open_command_source,
            auto_update_check,
            auto_update_check_source,
            diff_theme,
            diff_theme_source,
            worktrees_layout,
            worktrees_layout_source,
            branches_refresh_mins,
            branches_refresh_mins_source,
            diff_line_numbers,
            diff_line_numbers_source,
            setup: Setup {
                copy: copy.unwrap_or_default(),
                run: run.unwrap_or_default(),
            },
            copy_source,
            run_source,
        }
    }

    /// Whether the TUI should check GitHub for a newer wtm on start.
    pub fn auto_update_check(&self) -> bool {
        self.auto_update_check.unwrap_or(DEFAULT_AUTO_UPDATE_CHECK)
    }

    /// Diff syntax-highlight theme short id (e.g. `eighties`).
    pub fn diff_theme(&self) -> &str {
        self.diff_theme
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_DIFF_THEME)
    }

    /// Layout of the TUI's Worktrees tab; two-panel unless a config file says
    /// otherwise.
    pub fn worktrees_layout(&self) -> WorktreesLayout {
        self.worktrees_layout.unwrap_or_default()
    }

    /// How long the Branches tab may keep a cached list before refreshing.
    pub fn branches_refresh_mins(&self) -> u64 {
        self.branches_refresh_mins
            .unwrap_or(DEFAULT_BRANCHES_REFRESH_MINS)
    }

    /// Whether the diff pane draws its line-number gutter.
    pub fn diff_line_numbers(&self) -> bool {
        self.diff_line_numbers.unwrap_or(DEFAULT_DIFF_LINE_NUMBERS)
    }

    /// Absolute directory new worktrees are created under for a repo rooted
    /// at `repo_root`.
    pub fn worktree_base(&self, repo_root: &Path) -> Result<PathBuf> {
        resolve_worktree_dir(
            self.worktree_dir.as_deref().unwrap_or(DEFAULT_LOCATION),
            repo_root,
        )
    }
}

/// Turns a `worktree_dir` setting (preset name or path) into an absolute
/// directory for the repo at `repo_root`.
pub fn resolve_worktree_dir(raw: &str, repo_root: &Path) -> Result<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    resolve_with(raw, repo_root, home.as_deref())
}

fn resolve_with(raw: &str, repo_root: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let home_dir = || {
        home.map(Path::to_path_buf)
            .context("HOME is not set; cannot resolve the worktree location")
    };
    let repo = repo_name(repo_root);
    Ok(match raw {
        "sibling" => repo_root.join("..").join(format!("{repo}-worktrees")),
        "inside" => repo_root.join(".worktrees"),
        "home" => home_dir()?.join("worktrees").join(&repo),
        _ => {
            let expanded = raw.replace("{repo}", &repo);
            let path = expand_with_home(&expanded, home)?;
            if path.is_absolute() {
                path
            } else {
                repo_root.join(path)
            }
        }
    })
}

/// Expands a leading `~` in a user-supplied path using `$HOME`; paths without
/// one pass through unchanged.
pub fn expand_user_path(raw: &str) -> Result<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    expand_with_home(raw, home.as_deref())
}

fn expand_with_home(raw: &str, home: Option<&Path>) -> Result<PathBuf> {
    let home_dir = || {
        home.map(Path::to_path_buf)
            .with_context(|| format!("HOME is not set; cannot expand '~' in {raw:?}"))
    };
    Ok(if raw == "~" {
        home_dir()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_dir()?.join(rest)
    } else {
        PathBuf::from(raw)
    })
}

/// The repo's directory name, used for `{repo}` and the default location.
fn repo_name(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string())
}

/// Values substituted into an `open_command` template before it is spawned.
#[derive(Debug, Clone, Copy)]
pub struct OpenCommandVars<'a> {
    pub path: &'a str,
    pub name: &'a str,
    pub branch: &'a str,
    /// One of `"behind"`, `"ahead"`, `"merged"`, or `""` when none apply.
    pub status: &'a str,
}

/// Expands `{path}`, `{name}`, `{branch}`, and `{status}` in an open-command
/// template. Unknown `{…}` placeholders are left untouched.
pub fn expand_open_command(template: &str, vars: &OpenCommandVars<'_>) -> String {
    template
        .replace("{path}", vars.path)
        .replace("{name}", vars.name)
        .replace("{branch}", vars.branch)
        .replace("{status}", vars.status)
}

/// Path of the user-wide config file: `$WTM_GLOBAL_CONFIG` when set (mainly
/// for tests), otherwise `$XDG_CONFIG_HOME/wtm/config.toml`, falling back to
/// `~/.config/wtm/config.toml`. `None` when no relevant env var is set.
pub fn global_config_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("WTM_GLOBAL_CONFIG") {
        return Some(PathBuf::from(path));
    }
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("wtm").join("config.toml"));
    }
    env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("wtm")
            .join("config.toml")
    })
}

/// Reads the `[created_from]` map from the repo's `.wtm.toml` (branch → base
/// ref recorded at create time). Missing file or table yields an empty map.
pub fn load_created_from(repo_root: &Path) -> Result<BTreeMap<String, String>> {
    let file = FileConfig::load(&repo_root.join(CONFIG_FILE))?;
    Ok(file.created_from.unwrap_or_default())
}

/// Records that worktree branch `branch` was created from `base` in the
/// repo's `.wtm.toml` `[created_from]` table, preserving other settings.
pub fn set_created_from(repo_root: &Path, branch: &str, base: &str) -> Result<()> {
    edit_created_from(repo_root, |map| {
        map.insert(branch.to_string(), base.to_string());
    })
}

/// Drops `branch` from `[created_from]` when a worktree is removed.
pub fn unset_created_from(repo_root: &Path, branch: &str) -> Result<()> {
    edit_created_from(repo_root, |map| {
        map.remove(branch);
    })
}

/// Renames a `[created_from]` key when a worktree/branch is renamed.
pub fn rename_created_from(repo_root: &Path, old: &str, new: &str) -> Result<()> {
    if old == new {
        return Ok(());
    }
    edit_created_from(repo_root, |map| {
        if let Some(base) = map.remove(old) {
            map.insert(new.to_string(), base);
        }
    })
}

/// Reads the archived-branch list from the repo's `.wtm.toml`. Archiving only
/// hides a branch from the Branches tab; git never sees it, so the list lives
/// beside `[created_from]` as runtime state rather than as a setting.
pub fn load_archived_branches(repo_root: &Path) -> Result<BTreeSet<String>> {
    let file = FileConfig::load(&repo_root.join(CONFIG_FILE))?;
    Ok(file
        .archived_branches
        .unwrap_or_default()
        .into_iter()
        .collect())
}

/// Adds or removes `branch` from the archived list, preserving other settings.
pub fn set_branch_archived(repo_root: &Path, branch: &str, archived: bool) -> Result<()> {
    edit_archived_branches(repo_root, |set| {
        if archived {
            set.insert(branch.to_string());
        } else {
            set.remove(branch);
        }
    })
}

/// Follows a rename so an archived branch stays archived under its new name.
pub fn rename_archived_branch(repo_root: &Path, old: &str, new: &str) -> Result<()> {
    if old == new {
        return Ok(());
    }
    edit_archived_branches(repo_root, |set| {
        if set.remove(old) {
            set.insert(new.to_string());
        }
    })
}

/// Loads, mutates, and writes `archived_branches` via toml_edit so comments and
/// unrelated settings survive.
fn edit_archived_branches(
    repo_root: &Path,
    mutate: impl FnOnce(&mut BTreeSet<String>),
) -> Result<()> {
    let path = repo_root.join(CONFIG_FILE);
    let mut doc = load_doc(&path)?;
    let mut set: BTreeSet<String> = match doc.get("archived_branches").and_then(|i| i.as_array()) {
        Some(array) => array
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => BTreeSet::new(),
    };
    mutate(&mut set);
    if set.is_empty() {
        doc.remove("archived_branches");
    } else {
        let mut array = toml_edit::Array::new();
        for branch in &set {
            array.push(branch.as_str());
        }
        doc["archived_branches"] = toml_edit::value(array);
    }
    save_doc(&path, doc)
}

/// Reads `path` as a TOML document, or starts an empty one when it is missing.
fn load_doc(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    text.parse::<DocumentMut>()
        .with_context(|| format!("invalid TOML in {}", path.display()))
}

/// Writes `doc` back to `path`, refusing anything `FileConfig` couldn't load
/// again (same guard as `settings::save_doc`).
fn save_doc(path: &Path, doc: DocumentMut) -> Result<()> {
    let text = doc.to_string();
    toml::from_str::<FileConfig>(&text)
        .with_context(|| format!("refusing to write invalid config to {}", path.display()))?;
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Loads, mutates, and writes the `[created_from]` table via toml_edit so
/// comments and unrelated settings survive.
fn edit_created_from(
    repo_root: &Path,
    mutate: impl FnOnce(&mut BTreeMap<String, String>),
) -> Result<()> {
    let path = repo_root.join(CONFIG_FILE);
    let mut doc = load_doc(&path)?;
    let mut map = match doc.get("created_from").and_then(|item| item.as_table()) {
        Some(table) => table
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.to_string(), s.to_string())))
            .collect(),
        None => BTreeMap::new(),
    };
    mutate(&mut map);
    if map.is_empty() {
        doc.remove("created_from");
    } else {
        let mut table = toml_edit::Table::new();
        for (branch, base) in &map {
            table[branch] = toml_value(base.as_str());
        }
        doc["created_from"] = toml_edit::Item::Table(table);
    }
    save_doc(&path, doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_open_command_as_string_or_array() {
        let one: FileConfig = toml::from_str(r#"open_command = "cursor .""#).unwrap();
        assert_eq!(
            one.open_command,
            Some(OpenCommandList(vec!["cursor .".into()]))
        );
        let many: FileConfig =
            toml::from_str(r#"open_command = ["open {path}", "cursor {path}"]"#).unwrap();
        assert_eq!(
            many.open_command,
            Some(OpenCommandList(vec![
                "open {path}".into(),
                "cursor {path}".into(),
            ]))
        );
    }

    /// A `{ command, mode }` table carries its run mode; a bare string beside
    /// it still loads as a background command, so old configs keep working.
    #[test]
    fn parses_open_command_tables_with_a_run_mode() {
        let cfg: FileConfig = toml::from_str(
            r#"open_command = ["cursor {path}", { command = "nvim {path}", mode = "terminal" }]"#,
        )
        .unwrap();
        assert_eq!(
            cfg.open_command,
            Some(OpenCommandList(vec![
                OpenCommand::new("cursor {path}"),
                OpenCommand::new("nvim {path}").with_mode(CommandMode::Terminal),
            ]))
        );
    }

    /// A table with no `mode` falls back to the safe default rather than
    /// failing to parse.
    #[test]
    fn open_command_table_defaults_to_background() {
        let cfg: FileConfig =
            toml::from_str(r#"open_command = [{ command = "cursor {path}" }]"#).unwrap();
        let OpenCommandList(commands) = cfg.open_command.unwrap();
        assert_eq!(commands[0].mode, CommandMode::Background);
    }

    /// Open commands are the one additive setting: a repo's list adds to the
    /// global one instead of replacing it, and each entry remembers where it
    /// came from so a save can put it back in the right file.
    #[test]
    fn open_commands_from_both_layers_are_offered_together() {
        let global: FileConfig = toml::from_str(r#"open_command = "cursor {path}""#).unwrap();
        let repo: FileConfig = toml::from_str(r#"open_command = "npm run dev""#).unwrap();
        let merged = Config::merge(global, repo);
        assert_eq!(
            merged
                .open_command
                .iter()
                .map(|c| (c.command.as_str(), c.global))
                .collect::<Vec<_>>(),
            [("cursor {path}", true), ("npm run dev", false)],
            "global commands come first, each flagged with its layer"
        );
    }

    #[test]
    fn diff_line_numbers_defaults_to_on() {
        assert!(Config::default().diff_line_numbers());
        let repo: FileConfig = toml::from_str("diff_line_numbers = false").unwrap();
        let merged = Config::merge(FileConfig::default(), repo);
        assert!(!merged.diff_line_numbers());
        assert_eq!(merged.diff_line_numbers_source, Source::Repo);
    }

    #[test]
    fn expands_open_command_placeholders() {
        let expanded = expand_open_command(
            "open {path} # {name}@{branch} ({status})",
            &OpenCommandVars {
                path: "/tmp/wt",
                name: "feat",
                branch: "feat",
                status: "ahead",
            },
        );
        assert_eq!(expanded, "open /tmp/wt # feat@feat (ahead)");
    }

    #[test]
    fn parses_full_file_config() {
        let cfg: FileConfig = toml::from_str(
            r#"
            worktree_dir = "../wt"
            [setup]
            copy = [".env", ".env.local"]
            run = ["npm install"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.worktree_dir.as_deref(), Some("../wt"));
        let setup = cfg.setup.unwrap();
        assert_eq!(
            setup.copy.unwrap(),
            vec![PathBuf::from(".env"), PathBuf::from(".env.local")]
        );
        assert_eq!(setup.run.unwrap(), vec!["npm install".to_string()]);
    }

    #[test]
    fn empty_file_config_is_default() {
        let cfg: FileConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(toml::from_str::<FileConfig>("nope = 1").is_err());
    }

    #[test]
    fn missing_file_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = FileConfig::load(&dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, "not [valid").unwrap();
        assert!(FileConfig::load(&path).is_err());
    }

    #[test]
    fn merge_repo_wins_over_global_per_field() {
        let global: FileConfig = toml::from_str(
            r#"
            worktree_dir = "home"
            [setup]
            copy = [".env"]
            run = ["make deps"]
            "#,
        )
        .unwrap();
        let repo: FileConfig = toml::from_str(
            r#"
            [setup]
            run = ["npm install"]
            "#,
        )
        .unwrap();
        let cfg = Config::merge(global, repo);
        assert_eq!(cfg.worktree_dir.as_deref(), Some("home"));
        assert_eq!(cfg.worktree_dir_source, Source::Global);
        assert_eq!(cfg.setup.copy, vec![PathBuf::from(".env")]);
        assert_eq!(cfg.copy_source, Source::Global);
        assert_eq!(cfg.setup.run, vec!["npm install".to_string()]);
        assert_eq!(cfg.run_source, Source::Repo);
    }

    #[test]
    fn auto_update_check_defaults_on_and_merges_like_other_keys() {
        assert!(Config::default().auto_update_check());
        let global: FileConfig = toml::from_str("auto_update_check = false").unwrap();
        let cfg = Config::merge(global.clone(), FileConfig::default());
        assert!(!cfg.auto_update_check());
        assert_eq!(cfg.auto_update_check_source, Source::Global);
        // A repo can opt back in over a global opt-out.
        let repo: FileConfig = toml::from_str("auto_update_check = true").unwrap();
        let cfg = Config::merge(global, repo);
        assert!(cfg.auto_update_check());
        assert_eq!(cfg.auto_update_check_source, Source::Repo);
    }

    #[test]
    fn diff_theme_defaults_to_eighties_and_merges() {
        assert_eq!(Config::default().diff_theme(), DEFAULT_DIFF_THEME);
        let global: FileConfig = toml::from_str("diff_theme = \"ocean\"").unwrap();
        let cfg = Config::merge(global.clone(), FileConfig::default());
        assert_eq!(cfg.diff_theme(), "ocean");
        assert_eq!(cfg.diff_theme_source, Source::Global);
        let repo: FileConfig = toml::from_str("diff_theme = \"mocha\"").unwrap();
        let cfg = Config::merge(global, repo);
        assert_eq!(cfg.diff_theme(), "mocha");
        assert_eq!(cfg.diff_theme_source, Source::Repo);
    }

    #[test]
    fn worktrees_layout_defaults_to_two_panel_and_merges() {
        assert_eq!(
            Config::default().worktrees_layout(),
            WorktreesLayout::TwoPanel
        );
        let global: FileConfig = toml::from_str("worktrees_layout = \"three_panel\"").unwrap();
        let cfg = Config::merge(global.clone(), FileConfig::default());
        assert_eq!(cfg.worktrees_layout(), WorktreesLayout::ThreePanel);
        assert_eq!(cfg.worktrees_layout_source, Source::Global);
        // A repo can go back to the two-panel layout over a global opt-in.
        let repo: FileConfig = toml::from_str("worktrees_layout = \"two_panel\"").unwrap();
        let cfg = Config::merge(global, repo);
        assert_eq!(cfg.worktrees_layout(), WorktreesLayout::TwoPanel);
        assert_eq!(cfg.worktrees_layout_source, Source::Repo);
    }

    #[test]
    fn merge_of_nothing_is_default() {
        let cfg = Config::merge(FileConfig::default(), FileConfig::default());
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.worktree_dir_source, Source::Default);
    }

    /// `[created_from]` is accepted in `.wtm.toml` but is not a Config setting.
    #[test]
    fn created_from_parses_and_is_ignored_by_merge() {
        let file: FileConfig =
            toml::from_str("worktree_dir = \"inside\"\n\n[created_from]\nfeature = \"main\"\n")
                .unwrap();
        assert_eq!(
            file.created_from.as_ref().unwrap().get("feature"),
            Some(&"main".to_string())
        );
        let cfg = Config::merge(FileConfig::default(), file);
        assert_eq!(cfg.worktree_dir.as_deref(), Some("inside"));
    }

    /// set/unset/rename round-trip through the repo `.wtm.toml` without
    /// clobbering other keys.
    #[test]
    fn created_from_helpers_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(CONFIG_FILE), "worktree_dir = \"home\"\n").unwrap();
        set_created_from(root, "feature", "main").unwrap();
        set_created_from(root, "other", "develop").unwrap();
        let map = load_created_from(root).unwrap();
        assert_eq!(map.get("feature"), Some(&"main".to_string()));
        assert_eq!(map.get("other"), Some(&"develop".to_string()));
        rename_created_from(root, "feature", "renamed").unwrap();
        let map = load_created_from(root).unwrap();
        assert!(!map.contains_key("feature"));
        assert_eq!(map.get("renamed"), Some(&"main".to_string()));
        unset_created_from(root, "renamed").unwrap();
        unset_created_from(root, "other").unwrap();
        assert!(load_created_from(root).unwrap().is_empty());
        let text = std::fs::read_to_string(root.join(CONFIG_FILE)).unwrap();
        assert!(text.contains("worktree_dir"), "settings preserved: {text}");
        assert!(
            !text.contains("created_from"),
            "empty table removed: {text}"
        );
    }

    /// Archiving round-trips through `.wtm.toml` and, crucially, keeps writing
    /// valid TOML when `[created_from]` is already there: a top-level array
    /// emitted after a table header would silently become part of that table.
    #[test]
    fn archived_branch_helpers_round_trip_alongside_created_from() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(CONFIG_FILE), "worktree_dir = \"home\"\n").unwrap();
        set_created_from(root, "feature", "main").unwrap();
        set_branch_archived(root, "feature", true).unwrap();
        set_branch_archived(root, "old-thing", true).unwrap();

        let archived = load_archived_branches(root).unwrap();
        assert!(archived.contains("feature"));
        assert!(archived.contains("old-thing"));
        // Both kinds of runtime state survive together and stay readable.
        assert_eq!(
            load_created_from(root).unwrap().get("feature"),
            Some(&"main".to_string())
        );

        rename_archived_branch(root, "feature", "renamed").unwrap();
        let archived = load_archived_branches(root).unwrap();
        assert!(!archived.contains("feature"));
        assert!(archived.contains("renamed"));

        set_branch_archived(root, "renamed", false).unwrap();
        set_branch_archived(root, "old-thing", false).unwrap();
        assert!(load_archived_branches(root).unwrap().is_empty());
        let text = std::fs::read_to_string(root.join(CONFIG_FILE)).unwrap();
        assert!(text.contains("worktree_dir"), "settings preserved: {text}");
        assert!(
            !text.contains("archived_branches"),
            "empty list removed: {text}"
        );
    }

    #[test]
    fn resolves_presets() {
        let repo = Path::new("/home/me/proj");
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            resolve_with("sibling", repo, home).unwrap(),
            PathBuf::from("/home/me/proj/../proj-worktrees")
        );
        assert_eq!(
            resolve_with("inside", repo, home).unwrap(),
            PathBuf::from("/home/me/proj/.worktrees")
        );
        assert_eq!(
            resolve_with("home", repo, home).unwrap(),
            PathBuf::from("/home/me/worktrees/proj")
        );
    }

    #[test]
    fn resolves_manual_paths_and_placeholders() {
        let repo = Path::new("/r/proj");
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            resolve_with("../wt", repo, home).unwrap(),
            PathBuf::from("/r/proj/../wt")
        );
        assert_eq!(
            resolve_with("/abs/wt", repo, home).unwrap(),
            PathBuf::from("/abs/wt")
        );
        assert_eq!(
            resolve_with("~/wt/{repo}", repo, home).unwrap(),
            PathBuf::from("/home/me/wt/proj")
        );
        assert_eq!(
            resolve_with("/x/{repo}-wts", repo, home).unwrap(),
            PathBuf::from("/x/proj-wts")
        );
    }

    #[test]
    fn home_preset_without_home_is_an_error() {
        assert!(resolve_with("home", Path::new("/r/p"), None).is_err());
        assert!(resolve_with("~/wt", Path::new("/r/p"), None).is_err());
        // Presets that don't need HOME still work.
        assert!(resolve_with("sibling", Path::new("/r/p"), None).is_ok());
    }

    #[test]
    fn expands_leading_tilde_only() {
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            expand_with_home("~", home).unwrap(),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            expand_with_home("~/dev/proj", home).unwrap(),
            PathBuf::from("/home/me/dev/proj")
        );
        assert_eq!(
            expand_with_home("/abs/path", home).unwrap(),
            PathBuf::from("/abs/path")
        );
        assert!(expand_with_home("~/dev", None).is_err());
    }

    #[test]
    fn worktree_base_default_uses_repo_name() {
        let cfg = Config::default();
        let base = cfg.worktree_base(Path::new("/home/me/proj")).unwrap();
        assert_eq!(base, PathBuf::from("/home/me/proj/../proj-worktrees"));
    }
}
