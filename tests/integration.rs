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

/// Runs the wtm binary in `dir` and returns its output.
fn wtm(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wtm"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
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
fn main_worktree_is_protected() {
    let (_tmp, repo) = setup_repo();
    let out = wtm(&repo, &["remove", "main", "--force", "--json"]);
    assert!(!out.status.success());
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("main worktree"));
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
