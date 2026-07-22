//! End-to-end tests running the real `wtm` binary against throwaway git repos.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// Creates a temp git repo with one commit and a `.wtm.toml` that copies
/// `.env` and runs a trivial setup command. Returns (tempdir, repo path).
fn setup_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(
        repo.join(".wtm.toml"),
        "[setup]\ncopy = [\".env\"]\nrun = [\"echo setup-ran > setup.log\"]\n",
    )
    .unwrap();
    std::fs::write(repo.join(".gitignore"), ".env\nsetup.log\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    (tmp, repo)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Runs the wtm binary in `dir` with the global config pointed at a
/// (normally nonexistent) file inside `dir`, so the developer's own global
/// config can't leak into tests.
fn wtm(dir: &Path, args: &[&str]) -> Output {
    wtm_global(dir, args, &dir.join(".wtm-test-global.toml"))
}

/// Runs the wtm binary with an explicit global config path.
fn wtm_global(dir: &Path, args: &[&str], global: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wtm"))
        .args(args)
        .current_dir(dir)
        .env("WTM_GLOBAL_CONFIG", global)
        .output()
        .unwrap()
}

/// Runs the wtm binary with `input` piped to stdin (for `wtm init`).
fn wtm_stdin(dir: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wtm"))
        .args(args)
        .current_dir(dir)
        .env("WTM_GLOBAL_CONFIG", dir.join(".wtm-test-global.toml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout_json(out: &Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "wtm failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout was not valid JSON")
}

#[test]
fn create_list_status_diff_remove_roundtrip() {
    let (_tmp, repo) = setup_repo();

    // create: runs setup steps and reports them
    let out = wtm(&repo, &["create", "feature-x", "--json"]);
    let created = stdout_json(&out);
    assert_eq!(created["branch"], "feature-x");
    assert_eq!(created["created_branch"], true);
    assert_eq!(created["setup_ok"], true);
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());
    assert!(wt_path.join(".env").exists(), "setup should copy .env");
    assert!(
        wt_path.join("setup.log").exists(),
        "setup should run commands"
    );

    // list: main + new worktree with status fields
    let list = stdout_json(&wtm(&repo, &["list", "--json"]));
    let items = list.as_array().unwrap();
    assert_eq!(items.len(), 2);
    let main = items.iter().find(|i| i["is_main"] == true).unwrap();
    assert_eq!(main["branch"], "main");
    let feat = items.iter().find(|i| i["name"] == "feature-x").unwrap();
    assert_eq!(feat["dirty"], 0);

    // path: prints the worktree path for cd
    let out = wtm(&repo, &["path", "feature-x"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        wt_path.to_string_lossy()
    );

    // status/diff: reflect an edit inside the worktree
    std::fs::write(wt_path.join("README.md"), "changed\n").unwrap();
    let status = stdout_json(&wtm(&repo, &["status", "feature-x", "--json"]));
    assert_eq!(status["changes"].as_array().unwrap().len(), 1);
    assert_eq!(status["changes"][0]["path"], "README.md");
    let diff = stdout_json(&wtm(&repo, &["diff", "feature-x", "--json"]));
    assert!(diff["diff"].as_str().unwrap().contains("-hello"));

    // remove: refuses while dirty, succeeds with --force
    let out = wtm(&repo, &["remove", "feature-x", "--json"]);
    assert!(
        !out.status.success(),
        "remove should refuse a dirty worktree"
    );
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("uncommitted"));

    let removed = stdout_json(&wtm(&repo, &["remove", "feature-x", "--force", "--json"]));
    assert_eq!(removed["removed"]["name"], "feature-x");
    assert!(!wt_path.exists());
    let list = stdout_json(&wtm(&repo, &["list", "--json"]));
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[test]
fn status_lists_files_inside_new_untracked_folders() {
    let (_tmp, repo) = setup_repo();
    let created = stdout_json(&wtm(&repo, &["create", "feature-x", "--json"]));
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());

    // A brand-new folder with files: `git status` collapses this to `newdir/`
    // by default, hiding the files. With --untracked-files=all each file is
    // listed individually so the diff view can show them.
    std::fs::create_dir_all(wt_path.join("newdir/sub")).unwrap();
    std::fs::write(wt_path.join("newdir/a.txt"), "a\n").unwrap();
    std::fs::write(wt_path.join("newdir/sub/b.txt"), "b\n").unwrap();

    let status = stdout_json(&wtm(&repo, &["status", "feature-x", "--json"]));
    let paths: Vec<&str> = status["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"newdir/a.txt"), "got {paths:?}");
    assert!(paths.contains(&"newdir/sub/b.txt"), "got {paths:?}");
    assert!(
        !paths.iter().any(|p| p.ends_with('/')),
        "no collapsed folder entries: {paths:?}"
    );
}

#[test]
fn create_checks_out_existing_branch_and_rejects_duplicates() {
    let (_tmp, repo) = setup_repo();
    git(&repo, &["branch", "existing"]);

    let created = stdout_json(&wtm(&repo, &["create", "existing", "--json"]));
    assert_eq!(created["created_branch"], false);

    // The same branch can't be checked out in two worktrees.
    let out = wtm(&repo, &["create", "existing", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("already checked out")
    );
}

#[test]
fn create_from_base_ref_and_slashed_branch_names() {
    let (_tmp, repo) = setup_repo();
    git(&repo, &["commit", "--allow-empty", "-m", "second"]);

    let created = stdout_json(&wtm(
        &repo,
        &["create", "feature/login", "--from", "HEAD~1", "--json"],
    ));
    let path = created["path"].as_str().unwrap();
    assert!(
        path.ends_with("feature-login"),
        "slash should be flattened: {path}"
    );

    // The worktree's HEAD must match the requested base commit.
    let repo_head_prev = Command::new("git")
        .args(["rev-parse", "HEAD~1"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let wt_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .unwrap();
    assert_eq!(repo_head_prev.stdout, wt_head.stdout);
}

#[test]
fn remove_can_delete_branch() {
    let (_tmp, repo) = setup_repo();
    stdout_json(&wtm(&repo, &["create", "doomed", "--json"]));
    let removed = stdout_json(&wtm(
        &repo,
        &["remove", "doomed", "--delete-branch", "--json"],
    ));
    assert_eq!(removed["deleted_branch"], true);

    let out = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/doomed"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(!out.status.success(), "branch should be deleted");
}

#[test]
fn rename_worktree_renames_branch_and_moves_directory() {
    let (_tmp, repo) = setup_repo();
    stdout_json(&wtm(&repo, &["create", "before", "--json"]));

    let renamed = stdout_json(&wtm(&repo, &["rename", "before", "after", "--json"]));
    assert_eq!(renamed["new_name"], "after");
    assert_eq!(renamed["renamed_branch"], true);
    let new_path = PathBuf::from(renamed["new_path"].as_str().unwrap());
    assert!(new_path.exists(), "the moved directory exists");
    assert!(new_path.ends_with("after"), "{new_path:?}");

    // The worktree is now addressable by its new name and branch.
    let list = stdout_json(&wtm(&repo, &["list", "--json"]));
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["branch"].as_str())
        .collect();
    assert!(names.contains(&"after"), "{names:?}");
    assert!(!names.contains(&"before"), "{names:?}");
}

#[test]
fn main_worktree_is_protected() {
    let (_tmp, repo) = setup_repo();
    let out = wtm(&repo, &["remove", "main", "--force", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("main worktree"));
}

#[test]
fn copied_files_in_subdirs_keep_their_relative_path() {
    let (_tmp, repo) = setup_repo();
    std::fs::create_dir_all(repo.join("config/secrets")).unwrap();
    std::fs::write(repo.join("config/secrets/.env.local"), "TOKEN=abc\n").unwrap();
    std::fs::write(
        repo.join(".wtm.toml"),
        "[setup]\ncopy = [\"config/secrets/.env.local\"]\n",
    )
    .unwrap();
    git(&repo, &["commit", "-am", "copy from a subdir"]);

    let created = stdout_json(&wtm(&repo, &["create", "subdir-copy", "--json"]));
    assert_eq!(created["setup_ok"], true);
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());
    let copied = wt_path.join("config/secrets/.env.local");
    assert!(
        copied.exists(),
        "file must land in the same subfolder of the new worktree"
    );
    assert_eq!(std::fs::read_to_string(copied).unwrap(), "TOKEN=abc\n");
}

#[test]
fn failing_setup_keeps_worktree_and_exits_2() {
    let (_tmp, repo) = setup_repo();
    std::fs::write(
        repo.join(".wtm.toml"),
        "[setup]\nrun = [\"exit 7\", \"echo never > never.log\"]\n",
    )
    .unwrap();
    git(&repo, &["commit", "-am", "break setup"]);

    let out = wtm(&repo, &["create", "broken-setup", "--json"]);
    assert_eq!(out.status.code(), Some(2), "setup failure should exit 2");
    let created: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(created["setup_ok"], false);
    let steps = created["setup"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["ok"], false);
    assert!(steps[1]["detail"].as_str().unwrap().contains("skipped"));

    // Worktree survives so the user can fix setup by hand.
    let path = PathBuf::from(created["path"].as_str().unwrap());
    assert!(path.exists());
    assert!(!path.join("never.log").exists());
}

#[test]
fn commands_from_inside_a_worktree_use_the_shared_repo() {
    let (_tmp, repo) = setup_repo();
    let created = stdout_json(&wtm(&repo, &["create", "inner", "--json"]));
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());

    // Running `wtm list` from inside the linked worktree sees all worktrees
    // and still resolves .wtm.toml from the main worktree.
    let list = stdout_json(&wtm(&wt_path, &["list", "--json"]));
    assert_eq!(list.as_array().unwrap().len(), 2);
}

#[test]
fn mcp_server_lists_and_calls_tools() {
    let (_tmp, repo) = setup_repo();
    let mut child = Command::new(env!("CARGO_BIN_EXE_wtm"))
        .arg("mcp")
        .current_dir(&repo)
        .env("WTM_GLOBAL_CONFIG", repo.join(".wtm-test-global.toml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Minimal MCP session over newline-delimited JSON-RPC; closing stdin
    // afterwards shuts the server down.
    let mut stdin = child.stdin.take().unwrap();
    for line in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_worktrees","arguments":{}}}"#,
    ] {
        writeln!(stdin, "{line}").unwrap();
    }
    drop(stdin);

    let out = child.wait_with_output().unwrap();
    let responses: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let by_id = |id: u64| {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .expect("missing response")
    };

    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "wtm");

    let tools: Vec<&str> = by_id(2)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "list_worktrees",
        "create_worktree",
        "remove_worktree",
        "worktree_status",
        "worktree_diff",
    ] {
        assert!(
            tools.contains(&expected),
            "missing tool {expected} in {tools:?}"
        );
    }

    // The call result content is the same JSON the CLI's `list --json` emits.
    let text = by_id(3)["result"]["content"][0]["text"].as_str().unwrap();
    let list: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["branch"], "main");
}

#[test]
fn inside_preset_places_worktrees_in_repo_and_hides_them_from_git() {
    let (_tmp, repo) = setup_repo();

    let out = wtm(&repo, &["config", "set", "worktree_dir", "inside"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(".worktrees"),
        "should preview the resolved path: {stdout}"
    );

    let created = stdout_json(&wtm(&repo, &["create", "tucked-away", "--json"]));
    let path = PathBuf::from(created["path"].as_str().unwrap());
    assert!(
        path.starts_with(repo.join(".worktrees").canonicalize().unwrap()),
        "worktree should live inside the repo: {}",
        path.display()
    );

    // The worktree directory must not pollute the main worktree's status.
    let status = stdout_json(&wtm(&repo, &["status", "main", "--json"]));
    let polluted = status["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"].as_str().unwrap().contains(".worktrees"));
    assert!(!polluted, "worktrees dir leaked into git status: {status}");
}

#[test]
fn global_config_applies_and_repo_config_overrides_it() {
    let (tmp, repo) = setup_repo();
    let global = tmp.path().join("global.toml");
    let global_wts = tmp.path().join("global-wts");

    // Settings written with --global work without a repo config entry.
    let out = wtm_global(
        &repo,
        &[
            "config",
            "set",
            "--global",
            "worktree_dir",
            global_wts.to_str().unwrap(),
        ],
        &global,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(global.exists(), "global config file should be created");

    let show = stdout_json(&wtm_global(&repo, &["config", "show", "--json"], &global));
    assert_eq!(show["worktree_dir"]["source"], "global");

    let created = stdout_json(&wtm_global(
        &repo,
        &["create", "from-global", "--json"],
        &global,
    ));
    let path = PathBuf::from(created["path"].as_str().unwrap());
    assert!(
        path.starts_with(global_wts.canonicalize().unwrap()),
        "worktree should follow the global setting: {}",
        path.display()
    );

    // A repo-level value wins over the global one.
    let out = wtm_global(
        &repo,
        &["config", "set", "worktree_dir", "../repo-wts"],
        &global,
    );
    assert!(out.status.success());
    let show = stdout_json(&wtm_global(&repo, &["config", "show", "--json"], &global));
    assert_eq!(show["worktree_dir"]["source"], "repo");
    assert_eq!(show["worktree_dir"]["value"], "../repo-wts");

    // Unsetting the repo value falls back to global again.
    let out = wtm_global(&repo, &["config", "unset", "worktree_dir"], &global);
    assert!(out.status.success());
    let show = stdout_json(&wtm_global(&repo, &["config", "show", "--json"], &global));
    assert_eq!(show["worktree_dir"]["source"], "global");
}

#[test]
fn config_show_get_and_unknown_keys() {
    let (_tmp, repo) = setup_repo();

    // Human-readable show explains values, sources, and file locations.
    let out = wtm(&repo, &["config"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("worktree_dir"), "{text}");
    assert!(text.contains("new worktrees go in"), "{text}");
    assert!(text.contains("repo config"), "{text}");

    // get prints the effective value (default preset when unset).
    let out = wtm(&repo, &["config", "get", "worktree_dir"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "sibling");
    let out = wtm(&repo, &["config", "get", "setup.copy"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), ".env");

    // Unknown keys fail with a list of valid ones.
    let out = wtm(&repo, &["config", "set", "worktreedir", "x"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown setting"), "{err}");
    assert!(err.contains("worktree_dir"), "{err}");

    // config path lists both files.
    let out = wtm(&repo, &["config", "path"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(".wtm.toml"), "{text}");
    assert!(text.contains("global config"), "{text}");
}

#[test]
fn config_set_edits_lists_and_preserves_comments() {
    let (_tmp, repo) = setup_repo();
    std::fs::write(
        repo.join(".wtm.toml"),
        "# keep me\n[setup]\ncopy = [\".env\"]\n",
    )
    .unwrap();

    let out = wtm(
        &repo,
        &["config", "set", "setup.run", "npm ci, npm run build"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(repo.join(".wtm.toml")).unwrap();
    assert!(text.contains("# keep me"), "comment lost: {text}");

    let run = stdout_json(&wtm(&repo, &["config", "get", "setup.run", "--json"]));
    assert_eq!(run, serde_json::json!(["npm ci", "npm run build"]));
}

#[test]
fn init_wizard_creates_config_and_respects_existing_files() {
    let (_tmp, repo) = setup_repo();

    // setup_repo already wrote .wtm.toml, so plain init refuses.
    let out = wtm_stdin(&repo, &["init"], "");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already exists"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // --force runs the wizard: skip cloning, choose "inside", copy .env,
    // one command.
    let out = wtm_stdin(
        &repo,
        &["init", "--force"],
        "\n2\n.env\necho ran > init.log\n\n",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(transcript.contains("Clone settings"), "{transcript}");
    assert!(
        transcript.contains("Where should new worktrees be created?"),
        "{transcript}"
    );
    assert!(transcript.contains("Wrote"), "{transcript}");

    // The written config drives create end to end.
    let created = stdout_json(&wtm(&repo, &["create", "wizard-made", "--json"]));
    assert_eq!(created["setup_ok"], true);
    let path = PathBuf::from(created["path"].as_str().unwrap());
    assert!(path.starts_with(repo.join(".worktrees").canonicalize().unwrap()));
    assert!(path.join(".env").exists());
    assert!(path.join("init.log").exists());
}

/// Creates a temp git repo with one commit but no `.wtm.toml`.
fn setup_uninitialized_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    (tmp, repo)
}

#[test]
fn uninitialized_repo_gates_worktree_commands_until_init() {
    let (_tmp, repo) = setup_uninitialized_repo();

    // Worktree commands refuse with a pointer to `wtm init`.
    for args in [
        vec!["list", "--json"],
        vec!["create", "nope", "--json"],
        vec!["status", "main", "--json"],
    ] {
        let out = wtm(&repo, &args);
        assert!(!out.status.success(), "wtm {args:?} should be gated");
        let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        let msg = err["error"].as_str().unwrap();
        assert!(msg.contains("not initialized"), "{msg}");
        assert!(msg.contains("wtm init"), "{msg}");
    }

    // Config inspection still works so setup itself isn't blocked.
    assert!(wtm(&repo, &["config"]).status.success());
    assert!(wtm(&repo, &["config", "path"]).status.success());

    // Running init (all defaults) unblocks everything.
    let out = wtm_stdin(&repo, &["init"], "\n\n\n\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = stdout_json(&wtm(&repo, &["list", "--json"]));
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[test]
fn global_config_alone_does_not_satisfy_the_init_gate() {
    let (tmp, repo) = setup_uninitialized_repo();
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, "worktree_dir = \"inside\"\n").unwrap();

    let out = wtm_global(&repo, &["list", "--json"], &global);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("not initialized"));
}

#[test]
fn mcp_gates_tool_calls_but_still_lists_tools() {
    let (_tmp, repo) = setup_uninitialized_repo();

    let run_session = |repo: &Path| -> Vec<serde_json::Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wtm"))
            .arg("mcp")
            .current_dir(repo)
            .env("WTM_GLOBAL_CONFIG", repo.join(".wtm-test-global.toml"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for line in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_worktrees","arguments":{}}}"#,
        ] {
            writeln!(stdin, "{line}").unwrap();
        }
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    let by_id = |responses: &[serde_json::Value], id: u64| -> serde_json::Value {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .expect("missing response")
            .clone()
    };

    // Uninitialized: the server starts and lists tools, but calls fail.
    let responses = run_session(&repo);
    assert!(
        !by_id(&responses, 2)["result"]["tools"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let call = by_id(&responses, 3);
    let error_text = call.to_string();
    assert!(error_text.contains("not initialized"), "{error_text}");
    assert!(error_text.contains("wtm init"), "{error_text}");

    // Writing .wtm.toml makes calls work; the gate is checked per call.
    std::fs::write(repo.join(".wtm.toml"), "worktree_dir = \"inside\"\n").unwrap();
    let responses = run_session(&repo);
    let text = by_id(&responses, 3)["result"]["content"][0]["text"]
        .as_str()
        .expect("tool call should succeed once initialized")
        .to_string();
    let list: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[test]
fn init_clones_settings_from_another_repo() {
    let (_src_tmp, source) = setup_repo();
    let (_tmp, repo) = setup_uninitialized_repo();

    // Clone by directory path, accept as-is.
    let script = format!("{}\ny\n", source.display());
    let out = wtm_stdin(&repo, &["init"], &script);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let transcript = String::from_utf8_lossy(&out.stdout);
    assert!(transcript.contains("Cloned settings:"), "{transcript}");

    let copy = stdout_json(&wtm(&repo, &["config", "get", "setup.copy", "--json"]));
    assert_eq!(copy, serde_json::json!([".env"]));

    // Clone by direct file path into a second repo.
    let (_tmp2, repo2) = setup_uninitialized_repo();
    let script = format!("{}\ny\n", source.join(".wtm.toml").display());
    let out = wtm_stdin(&repo2, &["init"], &script);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = stdout_json(&wtm(&repo2, &["config", "get", "setup.run", "--json"]));
    assert_eq!(run, serde_json::json!(["echo setup-ran > setup.log"]));
}

#[test]
fn outside_a_repo_fails_cleanly() {
    let tmp = TempDir::new().unwrap();
    let out = wtm(tmp.path(), &["list", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("not inside a git repository")
    );
}

/// Creates a bare repo (a fake "origin") under `tmp`, usable as a git remote.
fn bare_repo(tmp: &Path) -> PathBuf {
    let bare = tmp.join("origin.git");
    git(
        tmp,
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    bare
}

#[test]
fn commit_stages_and_reports_result_and_refuses_when_clean() {
    let (_tmp, repo) = setup_repo();
    let created = stdout_json(&wtm(&repo, &["create", "feat", "--json"]));
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());
    std::fs::write(wt_path.join("README.md"), "changed\n").unwrap();
    std::fs::write(wt_path.join("new.txt"), "new file\n").unwrap();

    let result = stdout_json(&wtm(
        &repo,
        &["commit", "feat", "-m", "do the thing", "--json"],
    ));
    assert_eq!(result["name"], "feat");
    assert_eq!(result["summary"], "do the thing");
    assert_eq!(result["files_changed"], 2);
    assert!(result["hash"].as_str().unwrap().len() >= 7);

    // A clean worktree has nothing to commit.
    let out = wtm(&repo, &["commit", "feat", "-m", "again", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("nothing to commit"));

    // Human output doesn't error.
    std::fs::write(wt_path.join("README.md"), "changed again\n").unwrap();
    let out = wtm(&repo, &["commit", "feat", "-m", "more"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("file(s) changed"), "{text}");
}

#[test]
fn commit_paths_flag_stages_only_selected_files() {
    let (_tmp, repo) = setup_repo();
    let created = stdout_json(&wtm(&repo, &["create", "feat", "--json"]));
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());
    std::fs::write(wt_path.join("README.md"), "changed\n").unwrap();
    std::fs::write(wt_path.join("other.txt"), "other\n").unwrap();

    let result = stdout_json(&wtm(
        &repo,
        &[
            "commit",
            "feat",
            "-m",
            "only readme",
            "--paths",
            "README.md",
            "--json",
        ],
    ));
    assert_eq!(result["files_changed"], 1);

    // other.txt is left uncommitted.
    let status = stdout_json(&wtm(&repo, &["status", "feat", "--json"]));
    let changes = status["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["path"], "other.txt");
}

#[test]
fn stash_push_list_apply_pop_drop_roundtrip() {
    let (_tmp, repo) = setup_repo();
    let created = stdout_json(&wtm(&repo, &["create", "feat", "--json"]));
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());
    std::fs::write(wt_path.join("README.md"), "changed\n").unwrap();
    std::fs::write(wt_path.join("scratch.txt"), "untracked\n").unwrap();

    let pushed = stdout_json(&wtm(
        &repo,
        &["stash", "push", "feat", "-m", "wip work", "--json"],
    ));
    assert_eq!(pushed["action"], "push");
    assert!(
        !wt_path.join("scratch.txt").exists(),
        "stash push should include untracked files"
    );
    let status = stdout_json(&wtm(&repo, &["status", "feat", "--json"]));
    assert_eq!(status["changes"].as_array().unwrap().len(), 0);

    let list = stdout_json(&wtm(&repo, &["stash", "list", "feat", "--json"]));
    let entries = list["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0]["message"].as_str().unwrap().contains("wip work"));

    // apply restores the changes but keeps the entry
    let applied = stdout_json(&wtm(&repo, &["stash", "apply", "feat", "--json"]));
    assert_eq!(applied["action"], "apply");
    assert!(wt_path.join("scratch.txt").exists());
    let list = stdout_json(&wtm(&repo, &["stash", "list", "feat", "--json"]));
    assert_eq!(
        list["entries"].as_array().unwrap().len(),
        1,
        "apply should keep the entry"
    );

    // Undo the applied changes so pop can cleanly reapply the same stash.
    git(&wt_path, &["checkout", "--", "README.md"]);
    std::fs::remove_file(wt_path.join("scratch.txt")).unwrap();

    let popped = stdout_json(&wtm(&repo, &["stash", "pop", "feat", "--json"]));
    assert_eq!(popped["status"], "applied");
    assert!(wt_path.join("scratch.txt").exists());
    let list = stdout_json(&wtm(&repo, &["stash", "list", "feat", "--json"]));
    assert_eq!(
        list["entries"].as_array().unwrap().len(),
        0,
        "pop should drop the entry"
    );

    // Push again then drop it explicitly.
    stdout_json(&wtm(&repo, &["stash", "push", "feat", "--json"]));
    let dropped = stdout_json(&wtm(&repo, &["stash", "drop", "feat", "--json"]));
    assert_eq!(dropped["action"], "drop");
    let list = stdout_json(&wtm(&repo, &["stash", "list", "feat", "--json"]));
    assert_eq!(list["entries"].as_array().unwrap().len(), 0);

    // Human output doesn't error, even with no entries.
    let out = wtm(&repo, &["stash", "list", "feat"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no stash entries"));
}

#[test]
fn stash_index_selects_a_specific_entry() {
    let (_tmp, repo) = setup_repo();
    let created = stdout_json(&wtm(&repo, &["create", "feat", "--json"]));
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());

    std::fs::write(wt_path.join("a.txt"), "a\n").unwrap();
    stdout_json(&wtm(
        &repo,
        &["stash", "push", "feat", "-m", "first", "--json"],
    ));
    std::fs::write(wt_path.join("b.txt"), "b\n").unwrap();
    stdout_json(&wtm(
        &repo,
        &["stash", "push", "feat", "-m", "second", "--json"],
    ));

    let list = stdout_json(&wtm(&repo, &["stash", "list", "feat", "--json"]));
    let entries = list["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries[0]["message"].as_str().unwrap().contains("second"));
    assert!(entries[1]["message"].as_str().unwrap().contains("first"));

    // Drop the older ("first") entry at index 1, keeping "second" behind.
    stdout_json(&wtm(
        &repo,
        &["stash", "drop", "feat", "--index", "1", "--json"],
    ));
    let list = stdout_json(&wtm(&repo, &["stash", "list", "feat", "--json"]));
    let entries = list["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0]["message"].as_str().unwrap().contains("second"));
}

#[test]
fn push_with_no_upstream_publishes_to_origin() {
    let (tmp, repo) = setup_repo();
    let bare = bare_repo(tmp.path());
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&repo, &["push", "-u", "origin", "main"]);

    stdout_json(&wtm(&repo, &["create", "feat", "--json"]));
    let pushed = stdout_json(&wtm(&repo, &["push", "feat", "--json"]));
    assert_eq!(pushed["branch"], "feat");
    assert_eq!(pushed["set_upstream"], true);
    assert_eq!(pushed["remote"], "origin");

    let out = Command::new("git")
        .args(["ls-remote", "--heads", bare.to_str().unwrap(), "feat"])
        .output()
        .unwrap();
    assert!(
        !out.stdout.is_empty(),
        "feat should have been published to origin"
    );

    // Pushing again (already up to date, upstream already set) is a no-op
    // human-output path that must not error.
    let out = wtm(&repo, &["push", "feat"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn pull_fast_forwards_and_rebase_recovers_from_divergence() {
    let (tmp, repo) = setup_repo();
    let bare = bare_repo(tmp.path());
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&repo, &["push", "-u", "origin", "main"]);

    // Advance the remote from an independent clone.
    let second = tmp.path().join("second");
    git(
        tmp.path(),
        &["clone", bare.to_str().unwrap(), second.to_str().unwrap()],
    );
    git(&second, &["config", "user.email", "test@example.com"]);
    git(&second, &["config", "user.name", "Test"]);
    std::fs::write(second.join("upstream.txt"), "new\n").unwrap();
    git(&second, &["add", "."]);
    git(&second, &["commit", "-m", "advance"]);
    git(&second, &["push"]);

    // main is behind; a plain pull fast-forwards.
    let pulled = stdout_json(&wtm(&repo, &["pull", "main", "--json"]));
    assert_eq!(pulled["already_up_to_date"], false);
    assert_eq!(pulled["ahead_behind"]["ahead"], 0);
    assert_eq!(pulled["ahead_behind"]["behind"], 0);
    assert!(repo.join("upstream.txt").exists());

    // Pulling again with nothing new reports already up to date.
    let pulled = stdout_json(&wtm(&repo, &["pull", "main", "--json"]));
    assert_eq!(pulled["already_up_to_date"], true);

    // Diverge: a local commit plus another remote commit.
    std::fs::write(repo.join("local.txt"), "mine\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "local work"]);

    std::fs::write(second.join("upstream2.txt"), "more\n").unwrap();
    git(&second, &["add", "."]);
    git(&second, &["commit", "-m", "advance again"]);
    git(&second, &["push"]);

    // Plain (ff-only) pull refuses to pull a diverged branch.
    let out = wtm(&repo, &["pull", "main", "--json"]);
    assert!(!out.status.success());

    // --rebase replays the local commit on top and succeeds.
    let pulled = stdout_json(&wtm(&repo, &["pull", "main", "--rebase", "--json"]));
    assert_eq!(pulled["ahead_behind"]["ahead"], 1);
    assert_eq!(pulled["ahead_behind"]["behind"], 0);
    assert!(repo.join("upstream2.txt").exists());
    assert!(repo.join("local.txt").exists());
}

#[test]
fn pull_without_upstream_fails_clearly() {
    let (_tmp, repo) = setup_repo();
    stdout_json(&wtm(&repo, &["create", "lonely", "--json"]));
    let out = wtm(&repo, &["pull", "lonely", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("no upstream"));
}

#[test]
fn fetch_reports_configured_remotes() {
    let (tmp, repo) = setup_repo();
    let bare = bare_repo(tmp.path());
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&repo, &["push", "-u", "origin", "main"]);

    let result = stdout_json(&wtm(&repo, &["fetch", "--json"]));
    assert_eq!(result["remotes"], serde_json::json!(["origin"]));

    let out = wtm(&repo, &["fetch"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("origin"));

    // A repo with no remotes still succeeds and reports none.
    let (_tmp2, repo2) = setup_repo();
    let result = stdout_json(&wtm(&repo2, &["fetch", "--json"]));
    assert_eq!(result["remotes"], serde_json::json!([]));
    let out = wtm(&repo2, &["fetch"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no remotes"));
}

#[test]
fn branch_create_list_delete_rename() {
    let (_tmp, repo) = setup_repo();

    // create makes a branch without a worktree.
    let created = stdout_json(&wtm(&repo, &["branch", "create", "topic", "--json"]));
    assert_eq!(created["name"], "topic");
    assert_eq!(created["from"], "HEAD");
    let out = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/topic"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "branch should exist");
    let list = stdout_json(&wtm(&repo, &["list", "--json"]));
    assert_eq!(
        list.as_array().unwrap().len(),
        1,
        "branch create must not add a worktree"
    );

    // Duplicate names are rejected.
    let out = wtm(&repo, &["branch", "create", "topic", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("already exists"));

    // list shows checkout location and last-commit info.
    let checked = stdout_json(&wtm(&repo, &["create", "checked-out", "--json"]));
    let wt_path = PathBuf::from(checked["path"].as_str().unwrap());
    let list = stdout_json(&wtm(&repo, &["branch", "list", "--json"]));
    let branches = list["branches"].as_array().unwrap();
    let co = branches
        .iter()
        .find(|b| b["name"] == "checked-out")
        .unwrap();
    assert_eq!(
        co["checked_out_path"],
        wt_path.to_string_lossy().to_string()
    );
    assert!(!co["subject"].as_str().unwrap().is_empty());
    assert!(!co["date"].as_str().unwrap().is_empty());
    let topic = branches.iter().find(|b| b["name"] == "topic").unwrap();
    assert!(topic["checked_out_path"].is_null());

    // Human output doesn't error.
    let out = wtm(&repo, &["branch", "list"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("checked-out"));

    // delete refuses for a branch checked out in a worktree.
    let out = wtm(&repo, &["branch", "delete", "checked-out", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("checked out"));

    // delete works for a plain branch.
    let deleted = stdout_json(&wtm(&repo, &["branch", "delete", "topic", "--json"]));
    assert_eq!(deleted["forced"], false);
    let out = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/topic"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(!out.status.success(), "branch should be gone");

    // rename works, including for a branch checked out elsewhere.
    let renamed = stdout_json(&wtm(
        &repo,
        &["branch", "rename", "checked-out", "renamed", "--json"],
    ));
    assert_eq!(renamed["old"], "checked-out");
    assert_eq!(renamed["new"], "renamed");
    let out = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/renamed"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(out.status.success());
    let list = stdout_json(&wtm(&repo, &["list", "--json"]));
    let renamed_wt = list
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["path"] == wt_path.to_string_lossy().to_string())
        .unwrap();
    assert_eq!(renamed_wt["branch"], "renamed");
}

#[test]
fn switch_changes_a_worktrees_branch() {
    let (_tmp, repo) = setup_repo();
    let created = stdout_json(&wtm(&repo, &["create", "feat", "--json"]));
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());
    // A spare local branch that isn't checked out anywhere.
    git(&repo, &["branch", "spare"]);

    // Switching the worktree onto the spare branch reports the new branch.
    let switched = stdout_json(&wtm(&repo, &["switch", "feat", "spare", "--json"]));
    assert_eq!(switched["branch"], "spare");
    assert_eq!(switched["name"], "spare");
    let list = stdout_json(&wtm(&repo, &["list", "--json"]));
    let wt = list
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["path"] == wt_path.to_string_lossy().to_string())
        .unwrap();
    assert_eq!(wt["branch"], "spare");

    // Switching onto a branch already checked out elsewhere (main) fails cleanly.
    let out = wtm(&repo, &["switch", "spare", "main", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("already checked out")
    );

    // Human output confirms the switch.
    git(&repo, &["branch", "spare2"]);
    let out = wtm(&repo, &["switch", "spare", "spare2"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("spare2"));
}

#[test]
fn log_shows_recent_commits_with_limit() {
    let (_tmp, repo) = setup_repo();
    let created = stdout_json(&wtm(&repo, &["create", "feat", "--json"]));
    let wt_path = PathBuf::from(created["path"].as_str().unwrap());
    for i in 1..=3 {
        std::fs::write(wt_path.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        git(&wt_path, &["add", "."]);
        git(&wt_path, &["commit", "-m", &format!("commit {i}")]);
    }

    let log = stdout_json(&wtm(&repo, &["log", "feat", "-n", "2", "--json"]));
    let entries = log["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["subject"], "commit 3");
    assert_eq!(entries[1]["subject"], "commit 2");

    // The default count covers everything reachable from HEAD (init + 3).
    let log_all = stdout_json(&wtm(&repo, &["log", "feat", "--json"]));
    assert_eq!(log_all["entries"].as_array().unwrap().len(), 4);

    // Human output doesn't error.
    let out = wtm(&repo, &["log", "feat", "-n", "1"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("commit 3"));
}

#[test]
fn create_tracks_a_branch_that_exists_only_on_origin() {
    let (tmp, repo) = setup_repo();
    let bare = bare_repo(tmp.path());
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&repo, &["push", "-u", "origin", "main"]);

    // Publish a branch to origin, then remove the local copy so it only
    // exists as a remote-tracking ref (as if a teammate pushed it).
    git(&repo, &["branch", "remote-only"]);
    git(&repo, &["push", "origin", "remote-only"]);
    git(&repo, &["branch", "-D", "remote-only"]);

    let created = stdout_json(&wtm(&repo, &["create", "remote-only", "--json"]));
    assert_eq!(created["created_branch"], true);
    assert_eq!(created["tracked_remote"], "origin/remote-only");

    let wt_path = PathBuf::from(created["path"].as_str().unwrap());
    let upstream = Command::new("git")
        .args([
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ])
        .current_dir(&wt_path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&upstream.stdout).trim(),
        "origin/remote-only"
    );
}

#[test]
fn cherry_pick_commits_from_a_branch_into_a_worktree() {
    let (_tmp, repo) = setup_repo();

    // A feature branch (in its own worktree) with a commit adding feat.txt.
    let feat = stdout_json(&wtm(&repo, &["create", "feature", "--json"]));
    let feat_path = PathBuf::from(feat["path"].as_str().unwrap());
    std::fs::write(feat_path.join("feat.txt"), "feature work\n").unwrap();
    git(&feat_path, &["add", "feat.txt"]);
    git(&feat_path, &["commit", "-m", "add feat.txt"]);

    // The commit shows up via `branch log` without checking the branch out,
    // and its hash is what we cherry-pick.
    let log = stdout_json(&wtm(&repo, &["branch", "log", "feature", "--json"]));
    assert_eq!(log["entries"][0]["subject"], "add feat.txt");
    let hash = log["entries"][0]["hash"].as_str().unwrap().to_string();

    // A separate target worktree (branched off main) to receive the commit.
    let target = stdout_json(&wtm(&repo, &["create", "target", "--json"]));
    let target_path = PathBuf::from(target["path"].as_str().unwrap());

    // Cherry-pick and commit: the file lands and the target's HEAD advances.
    let picked = stdout_json(&wtm(
        &repo,
        &["cherry-pick", "--into", "target", &hash, "--json"],
    ));
    assert_eq!(picked["committed"], true);
    assert_eq!(picked["count"], 1);
    assert!(target_path.join("feat.txt").exists());
    let target_log = stdout_json(&wtm(&repo, &["log", "target", "--json"]));
    assert_eq!(target_log["entries"][0]["subject"], "add feat.txt");

    // A second feature commit, this time loaded without committing.
    std::fs::write(feat_path.join("feat2.txt"), "more work\n").unwrap();
    git(&feat_path, &["add", "feat2.txt"]);
    git(&feat_path, &["commit", "-m", "add feat2.txt"]);
    let log2 = stdout_json(&wtm(&repo, &["branch", "log", "feature", "--json"]));
    let hash2 = log2["entries"][0]["hash"].as_str().unwrap().to_string();

    let loaded = stdout_json(&wtm(
        &repo,
        &[
            "cherry-pick",
            "--into",
            "target",
            "--no-commit",
            &hash2,
            "--json",
        ],
    ));
    assert_eq!(loaded["committed"], false);
    // The change is in the working tree but not committed: HEAD is unchanged.
    assert!(target_path.join("feat2.txt").exists());
    let target_log2 = stdout_json(&wtm(&repo, &["log", "target", "--json"]));
    assert_eq!(target_log2["entries"][0]["subject"], "add feat.txt");
}

#[test]
fn merge_merges_a_branch_cleanly() {
    let (_tmp, repo) = setup_repo();

    // A feature branch with a new file, and a separate target worktree
    // branched off the same commit, so the merge is a clean fast-forward.
    let feat = stdout_json(&wtm(&repo, &["create", "feature", "--json"]));
    let feat_path = PathBuf::from(feat["path"].as_str().unwrap());
    std::fs::write(feat_path.join("feat.txt"), "feature work\n").unwrap();
    git(&feat_path, &["add", "feat.txt"]);
    git(&feat_path, &["commit", "-m", "add feat.txt"]);

    let target = stdout_json(&wtm(&repo, &["create", "target", "--json"]));
    let target_path = PathBuf::from(target["path"].as_str().unwrap());

    let merged = stdout_json(&wtm(
        &repo,
        &["merge", "feature", "--into", "target", "--json"],
    ));
    assert_eq!(merged["status"], "clean");
    assert!(!merged["commit"].as_str().unwrap().is_empty());
    assert!(target_path.join("feat.txt").exists());

    // Merging the same branch again reports up to date.
    let again = stdout_json(&wtm(
        &repo,
        &["merge", "feature", "--into", "target", "--json"],
    ));
    assert_eq!(again["status"], "up_to_date");
}

#[test]
fn merge_conflict_lists_files_then_resolve_and_continue() {
    let (_tmp, repo) = setup_repo();

    // Two branches that both edit README.md differently from the same base.
    let feat = stdout_json(&wtm(&repo, &["create", "feature", "--json"]));
    let feat_path = PathBuf::from(feat["path"].as_str().unwrap());
    std::fs::write(feat_path.join("README.md"), "from feature\n").unwrap();
    git(&feat_path, &["commit", "-am", "feature edit"]);

    let target = stdout_json(&wtm(&repo, &["create", "target", "--json"]));
    let target_path = PathBuf::from(target["path"].as_str().unwrap());
    std::fs::write(target_path.join("README.md"), "from target\n").unwrap();
    git(&target_path, &["commit", "-am", "target edit"]);

    // The merge stops on the conflicting file.
    let merged = stdout_json(&wtm(
        &repo,
        &["merge", "feature", "--into", "target", "--json"],
    ));
    assert_eq!(merged["status"], "conflicted");
    let files: Vec<&str> = merged["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert_eq!(files, vec!["README.md"]);

    // `conflicts` confirms it, and reading the file returns parsed hunks.
    let conflicts = stdout_json(&wtm(&repo, &["conflicts", "target", "--json"]));
    assert_eq!(conflicts["files"].as_array().unwrap().len(), 1);
    let read = stdout_json(&wtm(&repo, &["conflicts", "target", "README.md", "--json"]));
    assert_eq!(read["path"], "README.md");
    assert!(!read["segments"].as_array().unwrap().is_empty());

    // Resolve by keeping "ours" (the target's edit), then finish the merge.
    let resolved = stdout_json(&wtm(
        &repo,
        &["resolve", "target", "README.md", "--ours", "--json"],
    ));
    assert_eq!(resolved["action"], "ours");
    assert_eq!(
        std::fs::read_to_string(target_path.join("README.md")).unwrap(),
        "from target\n"
    );

    let completed = stdout_json(&wtm(
        &repo,
        &["merge", "--into", "target", "--continue", "--json"],
    ));
    assert_eq!(completed["target"], "target");
    assert!(!completed["commit"].as_str().unwrap().is_empty());

    // No conflicts remain.
    let conflicts_after = stdout_json(&wtm(&repo, &["conflicts", "target", "--json"]));
    assert!(conflicts_after["files"].as_array().unwrap().is_empty());
}

#[test]
fn cherry_pick_conflict_lists_files_then_resolve_and_continue() {
    let (_tmp, repo) = setup_repo();

    // A feature branch with a commit editing README.md (the one we cherry-pick).
    let feat = stdout_json(&wtm(&repo, &["create", "feature", "--json"]));
    let feat_path = PathBuf::from(feat["path"].as_str().unwrap());
    std::fs::write(feat_path.join("README.md"), "from feature\n").unwrap();
    git(&feat_path, &["commit", "-am", "feature edit"]);
    let log = stdout_json(&wtm(&repo, &["branch", "log", "feature", "--json"]));
    let hash = log["entries"][0]["hash"].as_str().unwrap().to_string();

    // A target worktree that edits the same line differently, so the pick
    // conflicts.
    let target = stdout_json(&wtm(&repo, &["create", "target", "--json"]));
    let target_path = PathBuf::from(target["path"].as_str().unwrap());
    std::fs::write(target_path.join("README.md"), "from target\n").unwrap();
    git(&target_path, &["commit", "-am", "target edit"]);

    // The cherry-pick stops on the conflicting file and leaves it mid-pick.
    let picked = stdout_json(&wtm(
        &repo,
        &["cherry-pick", "--into", "target", &hash, "--json"],
    ));
    assert_eq!(picked["status"], "conflicted");
    let files: Vec<&str> = picked["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert_eq!(files, vec!["README.md"]);

    // Resolve by keeping "theirs" (the cherry-picked commit's edit), then finish
    // the cherry-pick via the shared `merge --continue`, which auto-detects it.
    let resolved = stdout_json(&wtm(
        &repo,
        &["resolve", "target", "README.md", "--theirs", "--json"],
    ));
    assert_eq!(resolved["action"], "theirs");
    assert_eq!(
        std::fs::read_to_string(target_path.join("README.md")).unwrap(),
        "from feature\n"
    );

    let completed = stdout_json(&wtm(
        &repo,
        &["merge", "--into", "target", "--continue", "--json"],
    ));
    assert_eq!(completed["target"], "target");
    assert!(!completed["commit"].as_str().unwrap().is_empty());

    // No conflicts remain and the cherry-picked commit's message is recorded.
    let conflicts_after = stdout_json(&wtm(&repo, &["conflicts", "target", "--json"]));
    assert!(conflicts_after["files"].as_array().unwrap().is_empty());
    let target_log = stdout_json(&wtm(&repo, &["log", "target", "--json"]));
    assert_eq!(target_log["entries"][0]["subject"], "feature edit");
}
