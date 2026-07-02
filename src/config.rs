//! Layered configuration: a user-wide config file plus the per-repo
//! `.wtm.toml`, merged so repo settings win over global ones.
//!
//! The `worktree_dir` setting decides where new worktrees go. It accepts a
//! predefined rule (`sibling`, `inside`, `home`) or a manual path (absolute,
//! `~/...`, or relative to the repo root) where `{repo}` expands to the repo
//! directory name.

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = ".wtm.toml";

/// The rule used when `worktree_dir` isn't set anywhere.
pub const DEFAULT_LOCATION: &str = "sibling";

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

/// Raw contents of one config file. Every field is optional so a file can set
/// only what it cares about and inherit the rest.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub worktree_dir: Option<String>,
    pub setup: Option<FileSetup>,
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
        let global_setup = global.setup.unwrap_or_default();
        let repo_setup = repo.setup.unwrap_or_default();
        let (copy, copy_source) = pick(global_setup.copy, repo_setup.copy);
        let (run, run_source) = pick(global_setup.run, repo_setup.run);
        Config {
            worktree_dir,
            worktree_dir_source,
            setup: Setup {
                copy: copy.unwrap_or_default(),
                run: run.unwrap_or_default(),
            },
            copy_source,
            run_source,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn merge_of_nothing_is_default() {
        let cfg = Config::merge(FileConfig::default(), FileConfig::default());
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.worktree_dir_source, Source::Default);
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
