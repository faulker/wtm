//! Loading and parsing of the per-repo `.wtm.toml` configuration file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const CONFIG_FILE: &str = ".wtm.toml";

/// Repo-level configuration read from `.wtm.toml` in the main worktree root.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Directory that new worktrees are created under, relative to the main
    /// worktree root. Defaults to `../<repo-name>-worktrees`.
    pub worktree_dir: Option<PathBuf>,
    #[serde(default)]
    pub setup: Setup,
}

/// Steps run after a new worktree is created.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    /// Files copied from the main worktree into the new one (if they exist).
    #[serde(default)]
    pub copy: Vec<PathBuf>,
    /// Shell commands run inside the new worktree, in order.
    #[serde(default)]
    pub run: Vec<String>,
}

impl Config {
    /// Loads `.wtm.toml` from `repo_root`; a missing file yields the default
    /// config, but a malformed file is a hard error.
    pub fn load(repo_root: &Path) -> Result<Config> {
        let path = repo_root.join(CONFIG_FILE);
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid config in {}", path.display()))
    }

    /// Absolute directory new worktrees go in for a repo rooted at `repo_root`.
    pub fn worktree_base(&self, repo_root: &Path) -> PathBuf {
        match &self.worktree_dir {
            Some(dir) if dir.is_absolute() => dir.clone(),
            Some(dir) => repo_root.join(dir),
            None => {
                let name = repo_root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "repo".to_string());
                repo_root.join("..").join(format!("{name}-worktrees"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let cfg: Config = toml::from_str(
            r#"
            worktree_dir = "../wt"
            [setup]
            copy = [".env", ".env.local"]
            run = ["npm install"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.worktree_dir, Some(PathBuf::from("../wt")));
        assert_eq!(
            cfg.setup.copy,
            vec![PathBuf::from(".env"), PathBuf::from(".env.local")]
        );
        assert_eq!(cfg.setup.run, vec!["npm install".to_string()]);
    }

    #[test]
    fn empty_config_is_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(toml::from_str::<Config>("nope = 1").is_err());
    }

    #[test]
    fn missing_file_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "not [valid").unwrap();
        assert!(Config::load(dir.path()).is_err());
    }

    #[test]
    fn worktree_base_default_uses_repo_name() {
        let cfg = Config::default();
        let base = cfg.worktree_base(Path::new("/home/me/proj"));
        assert_eq!(base, PathBuf::from("/home/me/proj/../proj-worktrees"));
    }

    #[test]
    fn worktree_base_respects_relative_and_absolute_overrides() {
        let rel = Config {
            worktree_dir: Some(PathBuf::from("../wt")),
            ..Default::default()
        };
        assert_eq!(
            rel.worktree_base(Path::new("/r")),
            PathBuf::from("/r/../wt")
        );
        let abs = Config {
            worktree_dir: Some(PathBuf::from("/abs/wt")),
            ..Default::default()
        };
        assert_eq!(abs.worktree_base(Path::new("/r")), PathBuf::from("/abs/wt"));
    }
}
