#![allow(dead_code)]

// The binary's modules are exercised against disposable real repositories.
// Keeping the CLI as a single binary avoids publishing a premature library API.
#[path = "../src/app.rs"]
mod app;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/conflict.rs"]
mod conflict;
#[path = "../src/editor.rs"]
mod editor;
#[path = "../src/git.rs"]
mod git;
#[path = "../src/graph.rs"]
mod graph;
#[path = "../src/history.rs"]
mod history;
#[path = "../src/loader.rs"]
mod loader;
#[path = "../src/rebase.rs"]
mod rebase;
#[path = "../src/theme.rs"]
mod theme;
#[path = "../src/tree.rs"]
mod tree;
#[path = "../src/ui.rs"]
mod ui;

use std::{
    ffi::OsString,
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tempfile::TempDir;

use conflict::{Choice, Conflict};
use git::{Action, Hunks, Operation, Repo};
use rebase::{Plan, RebaseAction};

struct Fixture {
    _dir: TempDir,
    repo: Repo,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo {
            root: dir.path().into(),
        };
        repo.bytes(&["init", "-b", "main"]).unwrap();
        configure(&repo);
        Self { _dir: dir, repo }
    }

    fn write(&self, name: &str, content: &str) {
        let path = self.repo.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn commit(&self, name: &str, content: &str, message: &str) -> String {
        self.write(name, content);
        self.repo.bytes(&["add", "--all"]).unwrap();
        self.repo.bytes(&["commit", "-m", message]).unwrap();
        self.repo.head().unwrap().unwrap()
    }

    fn base(&self) -> String {
        self.commit("base", "base\n", "base")
    }

    fn action(&self, args: &[&str]) -> git::GitOutput {
        let snapshot = self.repo.snapshot().unwrap();
        self.repo
            .execute(&Action::new(&snapshot, "test action", args))
            .unwrap()
    }

    fn success(&self, args: &[&str]) {
        let output = self.action(args);
        assert!(output.success, "{}", output.text());
    }

    fn conflict(&self) {
        self.commit("file", "base\n", "base");
        self.repo.bytes(&["switch", "-c", "incoming"]).unwrap();
        self.commit("file", "incoming\n", "incoming");
        self.repo.bytes(&["switch", "main"]).unwrap();
        self.commit("file", "current\n", "current");
        let snapshot = self.repo.snapshot().unwrap();
        let action = Action::new(&snapshot, "merge", &["merge", "--no-edit", "incoming"]).clean();
        let output = self.repo.execute(&action).unwrap();
        assert!(!output.success, "expected conflict: {}", output.text());
        assert_eq!(self.repo.operation().unwrap(), Some(Operation::Merge));
    }
}

fn configure(repo: &Repo) {
    for (key, value) in [
        ("user.name", "Kae Tests"),
        ("user.email", "kae@example.invalid"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
        ("core.autocrlf", "false"),
        ("core.editor", "false"),
        ("rerere.enabled", "false"),
        ("merge.conflictStyle", "diff3"),
        ("rebase.updateRefs", "false"),
    ] {
        repo.bytes(&["config", key, value]).unwrap();
    }
}

#[test]
fn cli_help_version_and_noninteractive_errors() {
    let executable = env!("CARGO_BIN_EXE_kae");
    for arg in ["--help", "--version"] {
        let output = Command::new(executable).arg(arg).output().unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
    }
    let output = Command::new(executable)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert_eq!(
        Command::new(executable)
            .arg("--bad-flag")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn unborn_status_literal_names_and_staged_unstaged_diffs() {
    let fixture = Fixture::new();
    fixture.write(":(glob)*.rs", "base\n");
    fixture.write("space and\nnewline", "other\n");
    let snapshot = fixture.repo.snapshot().unwrap();
    assert_eq!(snapshot.branch, "main");
    assert!(snapshot.head.is_none());
    assert_eq!(snapshot.files.len(), 2);
    assert!(
        fixture
            .repo
            .file_preview(&snapshot.files[0])
            .unwrap()
            .contains("Untracked")
    );
    fixture.success(&["add", "--all"]);
    fixture.success(&["rm", "--cached", "-r", "-f", "--", "."]);
    assert!(fixture.repo.bytes(&["ls-files"]).unwrap().is_empty());
    assert!(fixture.repo.root.join(":(glob)*.rs").exists());
    fixture.success(&["add", "--all"]);
    fixture.success(&["commit", "-m", "initial"]);
    fixture.write(":(glob)*.rs", "staged\n");
    fixture.success(&["add", "--", ":(glob)*.rs"]);
    fixture.write(":(glob)*.rs", "unstaged\n");
    let snapshot = fixture.repo.snapshot().unwrap();
    assert_eq!(snapshot.files[0].status, "MM");
    let preview = fixture.repo.file_preview(&snapshot.files[0]).unwrap();
    assert!(preview.contains("+staged") && preview.contains("+unstaged"));
    fixture.success(&["restore", "--staged", "--", "."]);
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join(":(glob)*.rs")).unwrap(),
        "unstaged\n"
    );
}

#[test]
fn stale_views_reject_content_ref_and_index_changes() {
    let fixture = Fixture::new();
    fixture.base();
    fixture.write("base", "first edit\n");
    let snapshot = fixture.repo.snapshot().unwrap();
    let discard = Action::new(
        &snapshot,
        "discard",
        &["restore", "--worktree", "--", "base"],
    );
    fixture.write("base", "second edit\n");
    assert!(
        fixture
            .repo
            .execute(&discard)
            .unwrap_err()
            .to_string()
            .contains("Repository changed")
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("base")).unwrap(),
        "second edit\n"
    );
    let snapshot = fixture.repo.snapshot().unwrap();
    fixture.repo.bytes(&["branch", "other"]).unwrap();
    assert!(fixture.repo.verify(&snapshot.guard).is_err());
    let snapshot = fixture.repo.snapshot().unwrap();
    fixture.repo.bytes(&["add", "."]).unwrap();
    assert!(fixture.repo.verify(&snapshot.guard).is_err());
}

#[test]
fn guards_include_branch_identity_and_remote_destination() {
    let fixture = Fixture::new();
    fixture.base();
    fixture.repo.bytes(&["branch", "other"]).unwrap();
    let snapshot = fixture.repo.snapshot().unwrap();
    fixture.repo.bytes(&["switch", "other"]).unwrap();
    assert!(
        fixture.repo.verify(&snapshot.guard).is_err(),
        "Equal commit IDs do not mean the same current branch"
    );
    fixture
        .repo
        .bytes(&["remote", "add", "origin", "/first/remote.git"])
        .unwrap();
    let snapshot = fixture.repo.snapshot().unwrap();
    fixture
        .repo
        .bytes(&["remote", "set-url", "origin", "/different/remote.git"])
        .unwrap();
    assert!(fixture.repo.verify(&snapshot.guard).is_err());
}

#[test]
fn operation_logs_redact_url_credentials_and_query_tokens() {
    let output = git::GitOutput {
        success: false,
        stdout: b"https://user:password@example.invalid/repo?access_token=abc&other=ok".to_vec(),
        stderr: b"ssh://secret@example.invalid/repo?private_token=def".to_vec(),
        truncated: false,
        stdout_fingerprint: 0,
    };
    let text = output.text();
    for secret in [
        "password",
        "access_token=abc",
        "secret@",
        "private_token=def",
    ] {
        assert!(!text.contains(secret), "{text}");
    }
    assert!(text.contains("other=ok") && text.contains("[redacted]"));
}

#[test]
fn output_capping_drains_and_fingerprints_the_complete_stream() {
    let fixture = Fixture::new();
    let mut bytes = vec![b'x'; git::OUTPUT_LIMIT + 100];
    let mut outputs = Vec::new();
    for tail in [b'a', b'b'] {
        *bytes.last_mut().unwrap() = tail;
        let oid = fixture
            .repo
            .checked(
                &git::strings(&["hash-object", "-w", "--stdin"]),
                Some(&bytes),
            )
            .unwrap();
        let oid = String::from_utf8(oid).unwrap();
        let output = fixture
            .repo
            .run(&git::strings(&["cat-file", "blob", oid.trim()]), None)
            .unwrap();
        assert!(output.success && output.truncated);
        assert_eq!(output.stdout.len(), git::OUTPUT_LIMIT);
        outputs.push(output);
    }
    assert_eq!(outputs[0].stdout, outputs[1].stdout);
    assert_ne!(outputs[0].stdout_fingerprint, outputs[1].stdout_fingerprint);
}

#[test]
fn orphan_branch_still_shows_other_branch_history() {
    let fixture = Fixture::new();
    fixture.base();
    fixture.success(&["switch", "--orphan", "new-root"]);
    let snapshot = fixture.repo.snapshot().unwrap();
    assert!(snapshot.head.is_none());
    assert_eq!(snapshot.branch, "new-root");
    assert_eq!(snapshot.commits.len(), 1);
}

#[test]
fn clean_guard_preserves_uncommitted_and_untracked_work() {
    let fixture = Fixture::new();
    fixture.base();
    fixture.repo.bytes(&["branch", "other"]).unwrap();
    fixture.write("untracked", "precious\n");
    let snapshot = fixture.repo.snapshot().unwrap();
    let action = Action::new(&snapshot, "switch", &["switch", "other"]).clean();
    assert!(
        fixture
            .repo
            .execute(&action)
            .unwrap_err()
            .to_string()
            .contains("clean working tree")
    );
    assert_eq!(
        fixture.repo.text(&["branch", "--show-current"]).unwrap(),
        "main"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("untracked")).unwrap(),
        "precious\n"
    );
}

#[test]
fn stages_and_unstages_a_single_hunk_without_changing_worktree() {
    let fixture = Fixture::new();
    let original = (0..40)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    fixture.commit("file", &original, "base");
    let changed = original
        .replace("line 2\n", "changed 2\n")
        .replace("line 35\n", "changed 35\n");
    fixture.write("file", &changed);
    let snapshot = fixture.repo.snapshot().unwrap();
    let hunks = Hunks::load(&fixture.repo, &snapshot, &snapshot.files[0], false).unwrap();
    assert_eq!(hunks.bodies.len(), 2);
    assert!(hunks.apply(&fixture.repo).unwrap().success);
    let staged = fixture.repo.text(&["diff", "--cached"]).unwrap();
    assert!(staged.contains("+changed 2"));
    assert!(!staged.contains("+changed 35"));
    let snapshot = fixture.repo.snapshot().unwrap();
    let hunks = Hunks::load(&fixture.repo, &snapshot, &snapshot.files[0], true).unwrap();
    assert!(hunks.apply(&fixture.repo).unwrap().success);
    assert!(
        fixture
            .repo
            .bytes(&["diff", "--cached"])
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        changed
    );
}

#[test]
fn conflict_choices_undo_save_stage_and_merge_continue_are_separate() {
    let fixture = Fixture::new();
    fixture.conflict();
    let snapshot = fixture.repo.snapshot().unwrap();
    let before = fixture.repo.bytes(&["ls-files", "--stage"]).unwrap();
    let mut conflict = Conflict::load(
        &fixture.repo,
        "file".into(),
        snapshot.guard.clone(),
        &snapshot.branch,
    )
    .unwrap();
    assert_eq!(conflict.regions()[0].base.as_deref(), Some("base\n"));
    for (choice, expected) in [
        (Choice::Current, "current\n"),
        (Choice::Incoming, "incoming\n"),
        (Choice::CurrentThenIncoming, "current\nincoming\n"),
        (Choice::IncomingThenCurrent, "incoming\ncurrent\n"),
    ] {
        conflict.choose(choice);
        assert_eq!(conflict.result(), expected);
        conflict.undo();
        assert_eq!(conflict.unresolved(), 1);
    }
    let stage = Action::new(&snapshot, "stage", &["add", "--", "file"]);
    assert!(
        fixture
            .repo
            .execute(&stage)
            .unwrap_err()
            .to_string()
            .contains("Conflict markers")
    );
    conflict.choose(Choice::CurrentThenIncoming);
    conflict.save(&fixture.repo).unwrap();
    assert!(!conflict.dirty());
    assert_eq!(
        before,
        fixture.repo.bytes(&["ls-files", "--stage"]).unwrap()
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "current\nincoming\n"
    );
    assert!(fixture.repo.ensure_resolved().is_err());
    fixture.success(&["add", "--", "file"]);
    fixture.success(&["merge", "--continue"]);
    assert_eq!(fixture.repo.operation().unwrap(), None);
    assert_eq!(
        fixture
            .repo
            .text(&["rev-list", "--parents", "-1", "HEAD"])
            .unwrap()
            .split_whitespace()
            .count(),
        3
    );
}

#[test]
fn conflict_save_refuses_external_edits_and_abort_restores_original_head() {
    let fixture = Fixture::new();
    fixture.conflict();
    let head = fixture.repo.head().unwrap();
    let snapshot = fixture.repo.snapshot().unwrap();
    let mut conflict = Conflict::load(
        &fixture.repo,
        "file".into(),
        snapshot.guard,
        &snapshot.branch,
    )
    .unwrap();
    conflict.choose(Choice::Incoming);
    fixture.write("file", "external edit\n");
    assert!(conflict.save(&fixture.repo).is_err());
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "external edit\n"
    );
    fixture.success(&["merge", "--abort"]);
    assert_eq!(fixture.repo.head().unwrap(), head);
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "current\n"
    );
}

#[test]
fn eof_conflict_choices_preserve_missing_final_newlines() {
    let fixture = Fixture::new();
    fixture.commit("file", "a", "base");
    fixture.success(&["switch", "-c", "incoming"]);
    fixture.commit("file", "incoming", "incoming");
    fixture.success(&["switch", "main"]);
    fixture.commit("file", "current", "current");
    assert!(!fixture.action(&["merge", "--no-edit", "incoming"]).success);
    let snapshot = fixture.repo.snapshot().unwrap();
    let mut conflict = Conflict::load(
        &fixture.repo,
        "file".into(),
        snapshot.guard,
        &snapshot.branch,
    )
    .unwrap();
    conflict.choose(Choice::Current);
    assert_eq!(conflict.result(), "current");
    conflict.choose(Choice::Incoming);
    assert_eq!(conflict.result(), "incoming");
    conflict.choose(Choice::CurrentThenIncoming);
    assert_eq!(conflict.result(), "current\nincoming");
    conflict.choose(Choice::IncomingThenCurrent);
    assert_eq!(conflict.result(), "incoming\ncurrent");
    conflict.save(&fixture.repo).unwrap();
    assert_eq!(
        fs::read(fixture.repo.root.join("file")).unwrap(),
        b"incoming\ncurrent"
    );
}

#[test]
fn conflict_regions_can_be_resolved_independently() {
    let fixture = Fixture::new();
    let middle = (0..30)
        .map(|line| format!("context {line}\n"))
        .collect::<String>();
    fixture.commit("file", &format!("base top\n{middle}base bottom\n"), "base");
    fixture.success(&["switch", "-c", "incoming"]);
    fixture.commit(
        "file",
        &format!("incoming top\n{middle}incoming bottom\n"),
        "incoming",
    );
    fixture.success(&["switch", "main"]);
    fixture.commit(
        "file",
        &format!("current top\n{middle}current bottom\n"),
        "current",
    );
    assert!(!fixture.action(&["merge", "--no-edit", "incoming"]).success);
    let snapshot = fixture.repo.snapshot().unwrap();
    let mut conflict = Conflict::load(
        &fixture.repo,
        "file".into(),
        snapshot.guard,
        &snapshot.branch,
    )
    .unwrap();
    assert_eq!(conflict.regions().len(), 2);
    conflict.choose(Choice::Current);
    conflict.selected = 1;
    conflict.choose(Choice::Incoming);
    assert_eq!(
        conflict.result(),
        format!("current top\n{middle}incoming bottom\n")
    );
    conflict.undo();
    assert_eq!(conflict.unresolved(), 1);
    assert!(conflict.result().starts_with("current top\n"));
    assert!(conflict.result_offset() > 20);
}

#[test]
fn binary_and_modify_delete_conflicts_have_whole_file_recovery() {
    let fixture = Fixture::new();
    fs::write(fixture.repo.root.join("binary"), b"base\0").unwrap();
    fixture.repo.bytes(&["add", "."]).unwrap();
    fixture.repo.bytes(&["commit", "-m", "base"]).unwrap();
    fixture.repo.bytes(&["switch", "-c", "other"]).unwrap();
    fs::write(fixture.repo.root.join("binary"), b"incoming\0").unwrap();
    fixture.repo.bytes(&["commit", "-am", "incoming"]).unwrap();
    fixture.repo.bytes(&["switch", "main"]).unwrap();
    fs::write(fixture.repo.root.join("binary"), b"current\0").unwrap();
    fixture.repo.bytes(&["commit", "-am", "current"]).unwrap();
    assert!(!fixture.action(&["merge", "--no-edit", "other"]).success);
    let snapshot = fixture.repo.snapshot().unwrap();
    assert!(
        Conflict::load(
            &fixture.repo,
            "binary".into(),
            snapshot.guard,
            &snapshot.branch
        )
        .is_err()
    );
    fixture.success(&["checkout", "--theirs", "--", "binary"]);
    fixture.success(&["add", "--", "binary"]);
    fixture.success(&["merge", "--continue"]);
    assert_eq!(
        fs::read(fixture.repo.root.join("binary")).unwrap(),
        b"incoming\0"
    );

    let fixture = Fixture::new();
    fixture.commit("file", "base\n", "base");
    fixture.repo.bytes(&["switch", "-c", "other"]).unwrap();
    fixture.repo.bytes(&["rm", "file"]).unwrap();
    fixture.repo.bytes(&["commit", "-m", "delete"]).unwrap();
    fixture.repo.bytes(&["switch", "main"]).unwrap();
    fixture.commit("file", "modified\n", "modify");
    assert!(!fixture.action(&["merge", "--no-edit", "other"]).success);
    fixture.success(&["rm", "--", "file"]);
    fixture.success(&["merge", "--continue"]);
    assert!(!fixture.repo.root.join("file").exists());
}

#[test]
fn graph_branch_tag_stash_and_worktree_workflows() {
    let fixture = Fixture::new();
    fixture.base();
    fixture.success(&["switch", "-c", "feature"]);
    fixture.commit("feature", "feature\n", "feature");
    fixture.success(&["branch", "-m", "--", "feature", "renamed"]);
    fixture.success(&["tag", "--", "v0.1", "HEAD"]);
    fixture.write("untracked", "stash me\n");
    fixture.success(&["stash", "push", "--include-untracked", "-m", "work"]);
    assert!(!fixture.repo.root.join("untracked").exists());
    fixture.success(&["stash", "apply", "stash@{0}"]);
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("untracked")).unwrap(),
        "stash me\n"
    );
    fs::remove_file(fixture.repo.root.join("untracked")).unwrap();
    fixture.success(&["stash", "pop", "stash@{0}"]);
    fs::remove_file(fixture.repo.root.join("untracked")).unwrap();
    fixture.write("stash-again", "again\n");
    fixture.success(&["stash", "push", "--include-untracked", "-m", "drop me"]);
    fixture.success(&["stash", "drop", "stash@{0}"]);
    fixture.success(&["switch", "--", "main"]);
    fixture.success(&["merge", "--no-ff", "--no-edit", "renamed"]);
    let snapshot = fixture.repo.snapshot().unwrap();
    assert_eq!(snapshot.commits.len(), 3);
    assert!(
        snapshot
            .commits
            .iter()
            .any(|commit| !commit.connectors.is_empty())
    );
    assert_eq!(snapshot.tags, ["v0.1"]);
    assert!(snapshot.stashes.is_empty());
    fixture.success(&["branch", "-d", "--", "renamed"]);
    fixture.success(&["tag", "-d", "--", "v0.1"]);
    let target = fixture._dir.path().join("worktree with space");
    fixture.success(&[
        "worktree",
        "add",
        "--detach",
        "--",
        target.to_str().unwrap(),
        "HEAD",
    ]);
    let linked = Repo::discover(&target).unwrap();
    assert!(linked.snapshot().unwrap().branch.starts_with("detached @"));
    assert_eq!(fixture.repo.snapshot().unwrap().worktrees.len(), 2);
    fs::write(target.join("dirty"), "do not delete").unwrap();
    assert!(
        !fixture
            .action(&["worktree", "remove", "--", target.to_str().unwrap()])
            .success
    );
    fs::remove_file(target.join("dirty")).unwrap();
    fixture.success(&["worktree", "remove", "--", target.to_str().unwrap()]);
    assert!(!target.exists());
}

#[test]
fn cherry_pick_revert_amend_and_clean_merge() {
    let fixture = Fixture::new();
    fixture.base();
    fixture.success(&["switch", "-c", "feature"]);
    let pick = fixture.commit("feature", "feature\n", "feature");
    fixture.success(&["switch", "main"]);
    fixture.success(&["cherry-pick", "--", &pick]);
    fixture.success(&["commit", "--amend", "-m", "amended"]);
    assert_eq!(
        fixture.repo.text(&["log", "-1", "--format=%s"]).unwrap(),
        "amended"
    );
    let head = fixture.repo.head().unwrap().unwrap();
    fixture.success(&["revert", "--no-edit", "--", &head]);
    assert!(!fixture.repo.root.join("feature").exists());
}

#[test]
fn merge_preview_does_not_touch_index_or_working_tree() {
    let fixture = Fixture::new();
    fixture.conflict();
    fixture.success(&["merge", "--abort"]);
    let before = fixture.repo.guard().unwrap();
    let snapshot = fixture.repo.snapshot().unwrap();
    let action = Action::new(
        &snapshot,
        "preview",
        &["merge-tree", "--write-tree", "HEAD", "incoming"],
    );
    let output = fixture.repo.merge_preview(&action).unwrap();
    assert!(!output.success);
    assert!(output.text().contains("CONFLICT"));
    assert!(output.text().contains("PROPOSED RESULT AGAINST HEAD"));
    assert!(output.text().contains("+<<<<<<<"));
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn remote_fetch_pull_strategies_and_push_never_force() {
    let remote_dir = tempfile::tempdir().unwrap();
    let remote = remote_dir.path().join("remote.git");
    let probe = Repo {
        root: remote_dir.path().into(),
    };
    probe
        .bytes(&["init", "--bare", "-b", "main", remote.to_str().unwrap()])
        .unwrap();
    let fixture = Fixture::new();
    fixture.base();
    fixture
        .repo
        .bytes(&["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    fixture.success(&[
        "push",
        "--no-follow-tags",
        "--recurse-submodules=no",
        "--set-upstream",
        "--",
        "origin",
        "main",
    ]);
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_path = peer_dir.path().join("peer");
    probe
        .bytes(&[
            "clone",
            remote.to_str().unwrap(),
            peer_path.to_str().unwrap(),
        ])
        .unwrap();
    let peer = Repo::discover(&peer_path).unwrap();
    configure(&peer);
    let peer_commit = |name: &str| {
        fs::write(peer.root.join(name), format!("{name}\n")).unwrap();
        peer.bytes(&["add", "."]).unwrap();
        peer.bytes(&["commit", "-m", name]).unwrap();
        peer.bytes(&["push", "origin", "HEAD"]).unwrap();
    };
    peer_commit("remote1");
    fixture.success(&["fetch", "--", "origin"]);
    fixture.success(&[
        "pull",
        "--ff-only",
        "--no-rebase",
        "--no-autostash",
        "--",
        "origin",
    ]);
    assert!(fixture.repo.root.join("remote1").exists());
    fixture.commit("local1", "local\n", "local1");
    peer_commit("remote2");
    assert!(
        !fixture
            .action(&[
                "pull",
                "--ff-only",
                "--no-rebase",
                "--no-autostash",
                "--",
                "origin"
            ])
            .success
    );
    fixture.success(&[
        "pull",
        "--ff",
        "--no-rebase",
        "--no-edit",
        "--no-autostash",
        "--",
        "origin",
    ]);
    fixture.success(&[
        "push",
        "--no-follow-tags",
        "--recurse-submodules=no",
        "--",
        "origin",
        "HEAD",
    ]);
    peer.bytes(&["pull", "--ff-only"]).unwrap();
    fixture.commit("local2", "local\n", "local2");
    peer_commit("remote3");
    let before = fixture.repo.head().unwrap();
    fixture.success(&["pull", "--ff", "--rebase", "--no-autostash", "--", "origin"]);
    assert_ne!(fixture.repo.head().unwrap(), before);
    peer_commit("remote4");
    let rejected = fixture.action(&[
        "push",
        "--no-follow-tags",
        "--recurse-submodules=no",
        "--",
        "origin",
        "HEAD",
    ]);
    assert!(!rejected.success);
    assert!(rejected.text().contains("rejected"));
    assert!(peer.text(&["show", "HEAD:local2"]).is_err());
}

#[test]
fn interactive_rebase_reword_squash_fixup_drop_and_edit() {
    let fixture = Fixture::new();
    let base = fixture.base();
    for name in ["b", "c", "d", "e", "f"] {
        fixture.commit(name, &format!("{name}\n"), &format!("commit {name}"));
    }
    let snapshot = fixture.repo.snapshot().unwrap();
    let mut plan = Plan::load(&fixture.repo, &snapshot, &base).unwrap();
    plan.steps[0].action = RebaseAction::Reword;
    plan.steps[0].message = "renamed\n\nbody".into();
    plan.steps[1].action = RebaseAction::Squash;
    plan.steps[2].action = RebaseAction::Fixup;
    plan.steps[3].action = RebaseAction::Drop;
    plan.steps[4].action = RebaseAction::Edit;
    let original = fixture.repo.head().unwrap();
    let output = plan.execute(&fixture.repo).unwrap();
    assert!(output.success, "{}", output.text());
    assert_eq!(fixture.repo.operation().unwrap(), Some(Operation::Rebase));
    fixture.success(&["commit", "--amend", "-m", "edited"]);
    fixture.success(&["rebase", "--continue"]);
    assert_eq!(fixture.repo.operation().unwrap(), None);
    assert_ne!(fixture.repo.head().unwrap(), original);
    assert!(!fixture.repo.root.join("e").exists());
    assert!(
        fixture.repo.root.join("b").exists()
            && fixture.repo.root.join("c").exists()
            && fixture.repo.root.join("d").exists()
    );
    assert_eq!(
        fixture
            .repo
            .text(&["rev-list", "--count", &format!("{base}..HEAD")])
            .unwrap(),
        "2"
    );
    let message = fixture
        .repo
        .text(&["show", "-s", "--format=%B", "HEAD~1"])
        .unwrap();
    assert!(message.starts_with("renamed"), "{message}");
    assert!(message.contains("body") && message.contains("commit c"));
    assert!(!message.contains("commit d"));
    assert!(
        fixture
            .repo
            .text(&["for-each-ref", "--format=%(objectname)", "refs/kae/backup"])
            .unwrap()
            .contains(original.as_deref().unwrap())
    );
}

#[test]
fn reordered_rebase_conflicts_resume_with_saved_reword_message() {
    let fixture = Fixture::new();
    let base = fixture.commit("file", "a\n", "a");
    fixture.commit("file", "b\n", "b");
    fixture.commit("file", "c\n", "c");
    let snapshot = fixture.repo.snapshot().unwrap();
    let mut plan = Plan::load(&fixture.repo, &snapshot, &base).unwrap();
    plan.steps.swap(0, 1);
    plan.steps[0].action = RebaseAction::Reword;
    plan.steps[0].message = "reworded c".into();
    let output = plan.execute(&fixture.repo).unwrap();
    assert!(!output.success, "{}", output.text());
    for _ in 0..5 {
        if fixture.repo.operation().unwrap().is_none() {
            break;
        }
        let snapshot = fixture.repo.snapshot().unwrap();
        for file in snapshot.files.iter().filter(|file| file.is_conflict()) {
            let current = fixture.repo.snapshot().unwrap();
            let mut conflict = Conflict::load(
                &fixture.repo,
                file.path.clone(),
                current.guard,
                &current.branch,
            )
            .unwrap();
            for index in 0..conflict.regions().len() {
                conflict.selected = index;
                conflict.choose(Choice::Incoming);
            }
            conflict.save(&fixture.repo).unwrap();
            fixture.success(&["add", "--", file.path.to_str().unwrap()]);
        }
        let output = fixture.action(&["rebase", "--continue"]);
        if !output.success {
            assert!(
                fixture
                    .repo
                    .snapshot()
                    .unwrap()
                    .files
                    .iter()
                    .any(|file| file.is_conflict()),
                "{}",
                output.text()
            );
        }
    }
    assert_eq!(fixture.repo.operation().unwrap(), None);
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "b\n"
    );
    assert_eq!(
        fixture
            .repo
            .text(&["show", "-s", "--format=%s", "HEAD~1"])
            .unwrap(),
        "reworded c"
    );
}

#[test]
fn rebase_warns_for_published_commits_rejects_merge_ranges_and_can_abort() {
    let fixture = Fixture::new();
    let base = fixture.base();
    fixture.commit("one", "1", "one");
    let original = fixture.commit("two", "2", "two");
    fixture
        .repo
        .bytes(&["update-ref", "refs/remotes/origin/main", &original])
        .unwrap();
    let mut plan = Plan::load(&fixture.repo, &fixture.repo.snapshot().unwrap(), &base).unwrap();
    assert!(plan.published);
    plan.steps[0].action = RebaseAction::Edit;
    assert!(plan.execute(&fixture.repo).unwrap().success);
    fixture.success(&["rebase", "--abort"]);
    assert_eq!(
        fixture.repo.head().unwrap().as_deref(),
        Some(original.as_str())
    );
    fixture.success(&["switch", "-c", "other"]);
    fixture.commit("other", "other", "other");
    fixture.success(&["switch", "main"]);
    fixture.success(&["merge", "--no-ff", "--no-edit", "other"]);
    assert!(
        Plan::load(&fixture.repo, &fixture.repo.snapshot().unwrap(), &base)
            .unwrap_err()
            .to_string()
            .contains("merge commits")
    );
}

#[test]
fn rebase_skip_and_cherry_pick_revert_abort_recover_cleanly() {
    let fixture = Fixture::new();
    let base = fixture.commit("file", "a\n", "a");
    fixture.commit("file", "b\n", "b");
    fixture.commit("file", "c\n", "c");
    let mut plan = Plan::load(&fixture.repo, &fixture.repo.snapshot().unwrap(), &base).unwrap();
    plan.steps.swap(0, 1);
    assert!(!plan.execute(&fixture.repo).unwrap().success);
    fixture.success(&["rebase", "--skip"]);
    assert_eq!(fixture.repo.operation().unwrap(), None);
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "b\n"
    );
    fixture.success(&["switch", "-c", "other"]);
    let pick = fixture.commit("file", "other\n", "other");
    fixture.success(&["switch", "main"]);
    let main = fixture.commit("file", "main\n", "main");
    assert!(!fixture.action(&["cherry-pick", "--", &pick]).success);
    assert_eq!(
        fixture.repo.operation().unwrap(),
        Some(Operation::CherryPick)
    );
    fixture.success(&["cherry-pick", "--abort"]);
    assert_eq!(fixture.repo.head().unwrap().as_deref(), Some(main.as_str()));
    let older = fixture.repo.text(&["rev-parse", "HEAD~1"]).unwrap();
    assert!(
        !fixture
            .action(&["revert", "--no-edit", "--", &older])
            .success
    );
    assert_eq!(fixture.repo.operation().unwrap(), Some(Operation::Revert));
    fixture.success(&["revert", "--abort"]);
    assert_eq!(fixture.repo.head().unwrap().as_deref(), Some(main.as_str()));
    assert!(fixture.repo.snapshot().unwrap().files.is_empty());
}

#[test]
fn worktree_removal_rejects_an_in_progress_rebase_even_when_clean() {
    let fixture = Fixture::new();
    let base = fixture.base();
    fixture.commit("one", "one\n", "one");
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("linked");
    fixture.success(&[
        "worktree",
        "add",
        "-b",
        "work",
        "--",
        path.to_str().unwrap(),
        "HEAD",
    ]);
    let linked = Repo::discover(&path).unwrap();
    let mut plan = Plan::load(&linked, &linked.snapshot().unwrap(), &base).unwrap();
    plan.steps[0].action = RebaseAction::Edit;
    assert!(plan.execute(&linked).unwrap().success);
    assert!(linked.snapshot().unwrap().files.is_empty());
    let snapshot = fixture.repo.snapshot().unwrap();
    let remove = Action::new(&snapshot, "remove worktree", &["worktree", "remove"]).path(&path);
    assert!(
        fixture
            .repo
            .execute(&remove)
            .unwrap_err()
            .to_string()
            .contains("operation in progress")
    );
    assert!(path.exists());
    let snapshot = linked.snapshot().unwrap();
    assert!(
        linked
            .execute(&Action::new(&snapshot, "abort", &["rebase", "--abort"]))
            .unwrap()
            .success
    );
    fixture.success(&["worktree", "remove", "--", path.to_str().unwrap()]);
}

fn key(app: &mut app::App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn render_views(app: &app::App) {
    use ratatui::{Terminal, backend::TestBackend};
    for (width, height) in [(140, 40), (60, 24), (44, 12)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    }
}

fn wait(app: &mut app::App) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while (app.busy.is_some() || app.loading()) && Instant::now() < deadline {
        app.poll_worker();
        app.poll_loading();
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        app.busy.is_none() && !app.loading(),
        "worker / loader did not finish"
    );
}

#[test]
#[ignore = "Manual local profiling; uses only an isolated temporary repository"]
fn profile_index_update_refresh() {
    let fixture = Fixture::new();
    for index in 0..1000 {
        fixture.write(&format!("src/file-{index:04}"), "base\n");
    }
    fixture.commit("README", "base\n", "base");
    for index in 0..128 {
        fixture.write(
            &format!("src/file-{index:04}"),
            &"changed content\n".repeat(1024),
        );
    }
    let started = Instant::now();
    let snapshot = fixture.repo.snapshot().unwrap();
    let snapshot_time = started.elapsed();
    let started = Instant::now();
    let output = fixture
        .repo
        .execute(&Action::new(&snapshot, "Stage file", &["add"]).path(Path::new("src/file-0000")))
        .unwrap();
    assert!(output.success);
    let mutation_time = started.elapsed();
    let started = Instant::now();
    let refreshed = fixture.repo.refresh_index(&snapshot).unwrap();
    let refresh_time = started.elapsed();
    assert_eq!(refreshed.files.len(), 128);
    eprintln!(
        "PROFILE full snapshot={snapshot_time:?}; guarded stage={mutation_time:?}; post-stage refresh={refresh_time:?}"
    );
}

#[test]
fn ui_stage_commit_and_destructive_confirmation_flow() {
    let fixture = Fixture::new();
    fixture.write("file", "initial\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('s'));
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    key(&mut app, KeyCode::Char('c'));
    for character in "first commit".chars() {
        key(&mut app, KeyCode::Char(character));
    }
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert_eq!(
        fixture.repo.text(&["log", "-1", "--format=%s"]).unwrap(),
        "first commit"
    );
    fixture.write("file", "edited\n");
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    key(&mut app, KeyCode::Char(':'));
    for character in "discard".chars() {
        key(&mut app, KeyCode::Char(character));
    }
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(app::Modal::Confirm { .. })));
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "edited\n"
    );
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "edited\n"
    );
    key(&mut app, KeyCode::Char(':'));
    for character in "discard".chars() {
        key(&mut app, KeyCode::Char(character));
    }
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "initial\n"
    );
}

#[test]
fn commit_dialog_pastes_edits_and_submits_multiline_messages() {
    let fixture = Fixture::new();
    fixture.write("file", "content\n");
    fixture.success(&["add", "."]);
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('c'));
    assert!(matches!(app.modal, Some(app::Modal::CommitMessage { .. })));
    app.handle_paste("Résumé 🦀\r\n\r\nBody");
    assert!(app.busy.is_none());
    assert!(
        fixture.repo.head().unwrap().is_none(),
        "paste must not submit"
    );
    key(&mut app, KeyCode::Home);
    app.handle_paste("Edited ");
    key(&mut app, KeyCode::End);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    app.handle_paste("Another line");
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    app.handle_paste("Last line");
    let terminal = draw_at(&app, 140, 40);
    let buffer = terminal.backend().buffer();
    let text = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Commit message") && text.contains("Shift+Enter"));
    assert!(
        !(0..140).any(|x| buffer[(x, 0)].symbol() == "┌"),
        "dialog is not fullscreen"
    );
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    assert_eq!(
        fixture.repo.text(&["log", "-1", "--format=%B"]).unwrap(),
        "Résumé 🦀\n\nEdited Body\nAnother line\nLast line"
    );
}

#[test]
fn commit_dialog_empty_cancel_and_unsafe_paste_never_commit() {
    let fixture = Fixture::new();
    fixture.write("file", "content\n");
    fixture.success(&["add", "."]);
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('c'));
    app.handle_paste(" \n ");
    key(&mut app, KeyCode::Enter);
    assert!(app.error.is_some() && matches!(app.modal, Some(app::Modal::CommitMessage { .. })));
    app.handle_paste("\x1b[31munsafe");
    assert!(
        matches!(&app.modal, Some(app::Modal::CommitMessage { editor, .. }) if !editor.text().contains("unsafe"))
    );
    key(&mut app, KeyCode::Esc);
    assert!(app.modal.is_none() && app.busy.is_none());
    assert!(fixture.repo.head().unwrap().is_none());
    assert_eq!(fixture.repo.text(&["ls-files"]).unwrap(), "file");
}

#[test]
fn amend_dialog_prefills_message_and_retains_confirmation() {
    let fixture = Fixture::new();
    let head = fixture.commit("file", "content\n", "Original subject\n\nOriginal body");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char(':'));
    app.handle_paste("amend");
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::CommitMessage { editor, .. }) if editor.text().contains("Original body"))
    );
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(app::Modal::Confirm { .. })));
    key(&mut app, KeyCode::Esc);
    assert_eq!(fixture.repo.head().unwrap().unwrap(), head);
}

#[test]
fn fast_staging_uses_current_target_content_without_scanning_unrelated_changes() {
    let fixture = Fixture::new();
    fixture.write("other", "base other\n");
    fixture.commit("target", "base\n", "base");
    fixture.write("target", "seen\n");
    fixture.write("other", "seen other\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    let target = app
        .snapshot
        .files
        .iter()
        .position(|file| file.path == Path::new("target"))
        .unwrap();
    app.explorer.select_file(target);
    app.selected = target;
    fixture.write("target", "latest content\n");
    fixture.write("other", "unrelated external edit\n");
    fixture.success(&["branch", "unrelated-ref"]);
    key(&mut app, KeyCode::Char('s'));
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    let staged = fixture.repo.text(&["diff", "--cached"]).unwrap();
    assert!(staged.contains("+latest content") && !staged.contains("unrelated external edit"));
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("other")).unwrap(),
        "unrelated external edit\n"
    );
}

#[test]
fn root_stage_then_queued_file_unstage_preserves_order_and_working_files() {
    let fixture = Fixture::new();
    fixture.write("src/a", "base a\n");
    let head = fixture.commit("src/b", "base b\n", "base");
    fixture.write("src/a", "changed a\n");
    fixture.write("src/b", "changed b\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('S'));
    app.poll_worker(); // Start the root batch before the next user intent.
    let target = app
        .snapshot
        .files
        .iter()
        .position(|file| file.path == Path::new("src/a"))
        .unwrap();
    app.explorer.select_file(target);
    app.selected = target;
    key(&mut app, KeyCode::Char('u'));
    assert_eq!(app.pending_staging.get(Path::new("src/a")), Some(&false));
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    assert_eq!(
        fixture
            .repo
            .text(&["diff", "--cached", "--name-only"])
            .unwrap(),
        "src/b"
    );
    assert_eq!(fixture.repo.head().unwrap().unwrap(), head);
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("src/a")).unwrap(),
        "changed a\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("src/b")).unwrap(),
        "changed b\n"
    );
}

#[test]
fn fast_unstage_of_an_already_unstaged_literal_path_is_a_safe_noop() {
    let fixture = Fixture::new();
    let head = fixture.base();
    fixture.write("[folder]/line\nbreak", "keep\n");
    let request = git::IndexRequest {
        staged: false,
        bulk: false,
        scopes: vec!["[folder]/line\nbreak".into()],
        rename_sources: Vec::new(),
    };
    assert!(fixture.repo.update_index(&request).unwrap().success);
    assert_eq!(fixture.repo.head().unwrap().unwrap(), head);
    assert!(
        fixture
            .repo
            .bytes(&["diff", "--cached"])
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("[folder]/line\nbreak")).unwrap(),
        "keep\n"
    );
}

#[test]
fn queued_rename_toggles_resolve_sources_against_the_live_index() {
    let fixture = Fixture::new();
    let head = fixture.commit("old", &"rename content\n".repeat(20), "base");
    fixture.success(&["mv", "old", "new"]);
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('u'));
    app.poll_worker();
    key(&mut app, KeyCode::Char(' ')); // Re-stage while the unstage is still in flight.
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    assert_eq!(fixture.repo.text(&["ls-files"]).unwrap(), "new");
    assert!(
        app.snapshot
            .files
            .iter()
            .any(|file| file.path == Path::new("new")
                && file.original.as_deref() == Some(Path::new("old")))
    );
    key(&mut app, KeyCode::Char('u'));
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Char(' '));
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    assert_eq!(fixture.repo.text(&["ls-files"]).unwrap(), "old");
    assert_eq!(fixture.repo.head().unwrap().unwrap(), head);
    assert!(fixture.repo.root.join("new").exists() && !fixture.repo.root.join("old").exists());
}

#[test]
fn fast_stage_display_is_green_without_pending_dots_or_labels() {
    let fixture = Fixture::new();
    fixture.write("a", "a\n");
    fixture.write("b", "b\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('s'));
    let terminal = draw_at(&app, 140, 40);
    let buffer = terminal.backend().buffer();
    let rendered = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!rendered.contains("staging…") && !rendered.contains("unstaging…"));
    let row = (5..38)
        .find(|y| {
            (0..40)
                .any(|x| buffer[(x, *y)].symbol() == "a" && buffer[(x, *y)].fg == app.theme.added)
        })
        .unwrap();
    assert!(
        !(1..40).any(|x| buffer[(x, row)].symbol() == "·"),
        "no optimistic marker in the file row"
    );
    wait(&mut app);
}

#[test]
#[ignore = "Manual staging performance comparison in an isolated repository"]
fn profile_fast_index_batches() {
    let fixture = Fixture::new();
    for index in 0..1000 {
        fixture.write(&format!("src/file-{index:04}"), "base\n");
    }
    let head = fixture.commit("README", "base\n", "base");
    for index in 0..128 {
        fixture.write(&format!("src/file-{index:04}"), &"changed\n".repeat(1024));
    }
    let snapshot = fixture.repo.snapshot().unwrap();
    let started = Instant::now();
    assert!(
        fixture
            .repo
            .execute(&Action::new(
                &snapshot,
                "Guarded root stage",
                &["add", "--all"]
            ))
            .unwrap()
            .success
    );
    fixture.repo.refresh_index(&snapshot).unwrap();
    let guarded = started.elapsed();
    let mut request = git::IndexRequest {
        staged: false,
        bulk: true,
        scopes: vec![".".into()],
        rename_sources: Vec::new(),
    };
    assert!(fixture.repo.update_index(&request).unwrap().success);
    request.staged = true;
    let started = Instant::now();
    assert!(fixture.repo.update_index(&request).unwrap().success);
    let status = fixture.repo.index_status(&request.scopes).unwrap();
    let fast = started.elapsed();
    assert_eq!(status.len(), 128);
    assert!(status.iter().all(|file| file.status == "M "));
    assert_eq!(fixture.repo.head().unwrap().unwrap(), head);
    eprintln!(
        "PROFILE root staging (128 changes / 1,000 files): old guarded stage+refresh={guarded:?}; fast batch+status={fast:?}; speedup={:.1}x",
        guarded.as_secs_f64() / fast.as_secs_f64()
    );
}

#[test]
#[ignore = "Manual performance comparison; isolated temporary repository only"]
fn profile_batched_folder_previews_and_viewport_rendering() {
    let fixture = Fixture::new();
    for index in 0..64 {
        fixture.write(
            &format!("src/feature-{}/file-{index:04}", index % 8),
            "base\n",
        );
    }
    fixture.commit("README", "base\n", "base");
    for index in 0..64 {
        fixture.write(
            &format!("src/feature-{}/file-{index:04}", index % 8),
            &format!("changed-{index:04}\n"),
        );
    }
    let snapshot = fixture.repo.snapshot().unwrap();
    let started = Instant::now();
    for file in &snapshot.files {
        fixture.repo.file_preview(file).unwrap();
    }
    let sequential = started.elapsed();
    let started = Instant::now();
    let preview = fixture
        .repo
        .folder_preview("src", Path::new("src"), &snapshot.files)
        .unwrap();
    let batched = started.elapsed();
    assert!(preview.contains("changed-0000") && preview.contains("changed-0063"));
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    // Rendering cost should be independent of the number of offscreen files.
    app.snapshot.files = (0..10_000)
        .map(|index| git::FileChange {
            path: format!("src/feature-{}/file-{index:05}", index % 100).into(),
            status: " M".into(),
            original: None,
        })
        .collect();
    app.explorer = tree::explorer(&app.snapshot.files, &app.repo.root);
    app.folder_status = tree::folder_status(&app.snapshot.files, &app.pending_staging);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
    let started = Instant::now();
    for _ in 0..50 {
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    }
    let render = started.elapsed();
    eprintln!(
        "PROFILE folder files=64 sequential={sequential:?}; batched={batched:?}; speedup={:.1}x; 50 viewport renders with 10,000 changed files={render:?}",
        sequential.as_secs_f64() / batched.as_secs_f64()
    );
}

#[test]
fn batched_folder_paths_remain_literal_and_read_cancellation_never_leaks_into_mutations() {
    let fixture = Fixture::new();
    fixture.write("[group]/a", "base\n");
    fixture.commit("group/a", "base\n", "base");
    fixture.write("[group]/a", "literal change\n");
    fixture.write("group/a", "unrelated change\n");
    let snapshot = fixture.repo.snapshot().unwrap();
    let files = snapshot
        .files
        .iter()
        .filter(|file| file.path.starts_with("[group]"))
        .cloned()
        .collect::<Vec<_>>();
    let preview = fixture
        .repo
        .folder_preview("[group]", Path::new("[group]"), &files)
        .unwrap();
    assert!(preview.contains("+literal change") && !preview.contains("unrelated change"));
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    assert!(git::cancellable_read(cancel, || fixture.repo.text(&["status", "--short"])).is_err());
    // This runs on the normal mutation path, with all guard checks intact.
    let action = Action::new(&snapshot, "Stage literal", &["add"]).path(Path::new("[group]/a"));
    assert!(fixture.repo.execute(&action).unwrap().success);
    assert!(
        fixture
            .repo
            .text(&["diff", "--cached"])
            .unwrap()
            .contains("+literal change")
    );
}

#[test]
fn viewport_rendering_preserves_global_file_row_indices() {
    let fixture = Fixture::new();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    app.snapshot.files = (0..10_000)
        .map(|index| git::FileChange {
            path: format!("src/file-{index:05}").into(),
            status: " M".into(),
            original: None,
        })
        .collect();
    app.explorer = tree::explorer(&app.snapshot.files, &app.repo.root);
    app.folder_status = tree::folder_status(&app.snapshot.files, &app.pending_staging);
    app.explorer.select_file(9_999);
    app.selected = 9_999;
    draw_at(&app, 140, 40);
    let geometry = app.geometry.borrow();
    assert!(geometry.rows.len() <= usize::from(geometry.list.height.saturating_sub(2)));
    assert!(
        geometry
            .rows
            .last()
            .is_some_and(|index| app.explorer.rows[*index].file == Some(9_999))
    );
    assert!(app.explorer_state.borrow().offset() > 9_000);
}

#[test]
fn history_defaults_to_compact_head_scope_and_retains_toggles_and_selection() {
    let fixture = Fixture::new();
    let base = fixture.base();
    fixture.success(&["switch", "-c", "side"]);
    let side = fixture.commit("side-file", "side\n", "side-only commit");
    fixture.success(&["switch", "main"]);
    let main = fixture.commit("main-file", "main\n", "main-only commit");
    let before = fixture.repo.guard().unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('2'));
    wait(&mut app);
    assert!(!app.history_graph && !app.snapshot.all_history);
    assert_eq!(app.snapshot.commits.len(), 2);
    assert!(app.snapshot.commits.iter().all(|commit| commit.oid != side));
    assert!(app.items().iter().all(|item| !item.contains('\n')));
    assert!(app.items()[0].starts_with("main-only commit"));
    let selected = app.snapshot.commits[app.selected].oid.clone();
    key(&mut app, KeyCode::Char('g'));
    assert!(app.history_graph);
    assert!(app.items()[0].starts_with('*'));
    assert_eq!(app.snapshot.commits[app.selected].oid, selected);
    key(&mut app, KeyCode::Char('B'));
    wait(&mut app);
    assert!(app.snapshot.all_history);
    assert_eq!(app.snapshot.commits.len(), 3);
    assert_eq!(app.snapshot.commits[app.selected].oid, selected);
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert!(app.history_graph && app.snapshot.all_history);
    // Rapid scope changes must accept only the final reader generation.
    key(&mut app, KeyCode::Char('B'));
    key(&mut app, KeyCode::Char('B'));
    key(&mut app, KeyCode::Char('B'));
    wait(&mut app);
    assert!(!app.snapshot.all_history);
    fixture.repo.verify(&before).unwrap();
    fixture.success(&["switch", "--detach", &base]);
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert_eq!(app.snapshot.commits.len(), 1);
    assert_eq!(app.snapshot.commits[0].oid, base);
    fixture.success(&["switch", "--orphan", "empty"]);
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert!(app.snapshot.commits.is_empty());
    key(&mut app, KeyCode::Char('B'));
    wait(&mut app);
    assert!(app.snapshot.commits.iter().any(|commit| commit.oid == main));
    assert!(app.snapshot.commits.iter().any(|commit| commit.oid == side));
}

fn draw_at(
    app: &app::App,
    width: u16,
    height: u16,
) -> ratatui::Terminal<ratatui::backend::TestBackend> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    terminal
}

#[test]
fn parent_and_file_previews_follow_selection_without_changing_expansion() {
    use crossterm::event::{MouseButton::Left, MouseEventKind::Down};
    let fixture = Fixture::new();
    fixture.write("src/a", "base a\n");
    fixture.write("src/nested/b", "base b\n");
    fixture.commit("outside", "base\n", "base");
    fixture.write("src/a", "staged a\n");
    fixture.success(&["add", "src/a"]);
    fixture.write("src/a", "working a\n");
    fixture.write("src/nested/b", "working b\n");
    fixture.write("src/new", "untracked content\n");
    fixture.write("outside", "outside change\n");
    let before = fixture.repo.guard().unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    select_folder(&mut app, "src");
    draw_at(&app, 140, 40);
    let geometry = app.geometry.borrow().clone();
    let line = geometry
        .rows
        .iter()
        .position(|row| *row == app.explorer.cursor)
        .unwrap() as u16;
    let collapsed = app.explorer.collapsed.clone();
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 10,
        geometry.list.y + 1 + line,
    );
    wait(&mut app);
    assert_eq!(app.explorer.collapsed, collapsed);
    for content in [
        "STAGED",
        "WORKING TREE",
        "+staged a",
        "+working a",
        "+working b",
        "untracked content",
    ] {
        assert!(app.preview.contains(content), "{content}");
    }
    assert!(!app.preview.contains("outside change"));
    key(&mut app, KeyCode::Home);
    wait(&mut app);
    assert!(app.preview.contains("outside change"));
    draw_at(&app, 140, 40);
    let target = app
        .explorer
        .rows
        .iter()
        .position(|row| row.path == Path::new("src/nested/b"))
        .unwrap();
    let geometry = app.geometry.borrow().clone();
    let line = geometry.rows.iter().position(|row| *row == target).unwrap() as u16;
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 12,
        geometry.list.y + 1 + line,
    );
    wait(&mut app);
    assert!(app.preview.contains("+working b"));
    assert!(!app.preview.contains("staged a") && !app.preview.contains("outside change"));
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn combined_folder_preview_is_bounded_and_utf8_safe() {
    let fixture = Fixture::new();
    fixture.write("src/a", &"雪".repeat(100_000));
    fixture.write("src/b", "later file\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Home);
    wait(&mut app);
    assert!(app.preview.contains("Combined preview truncated"));
    assert!(app.preview.len() <= git::PREVIEW_LIMIT + 100);
}

#[test]
fn slash_filters_lists_searches_diffs_and_never_executes_commands() {
    let fixture = Fixture::new();
    fixture.commit("alpha", "first needle\nsecond needle\n", "wanted commit");
    fixture.commit("beta", "other\n", "another commit");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('/'));
    assert!(app.modal.is_none());
    app.handle_paste("wanted");
    assert_eq!(app.visible_items().len(), 1);
    assert_eq!(app.selected, 1);
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(app.modal.is_none() && app.preview.contains("wanted commit"));
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("needle");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.preview_search.matches.len(), 2);
    let first = app.preview_search.active;
    key(&mut app, KeyCode::Char('n'));
    assert_ne!(app.preview_search.active, first);
    let terminal = draw_at(&app, 140, 40);
    assert!(
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.bg == app.theme.search_active)
    );
    key(&mut app, KeyCode::Esc);
    assert!(
        app.filters[app::Workspace::History.index()]
            .query
            .is_empty()
    );
    key(&mut app, KeyCode::Char(':'));
    assert!(matches!(app.modal, Some(app::Modal::Palette { .. })));
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Char('3'));
    let before = fixture.repo.guard().unwrap();
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("no matching branch qsy");
    key(&mut app, KeyCode::Enter);
    assert!(app.visible_items().is_empty());
    key(&mut app, KeyCode::Enter);
    assert!(app.busy.is_none());
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn file_filter_restores_expansion_after_refresh_and_clear() {
    let fixture = Fixture::new();
    fixture.write("src/alpha", "alpha\n");
    fixture.write("src/nested/雪", "target\n");
    fixture.write("outside", "other\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    select_folder(&mut app, "src");
    key(&mut app, KeyCode::Enter);
    let collapsed = app.explorer.collapsed.clone();
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("雪");
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(
        app.explorer
            .rows
            .iter()
            .any(|row| row.path == Path::new("src/nested/雪"))
    );
    assert!(
        !app.explorer
            .rows
            .iter()
            .any(|row| row.path == Path::new("outside"))
    );
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert_eq!(app.explorer.filter, "雪");
    key(&mut app, KeyCode::Esc);
    wait(&mut app);
    assert_eq!(app.explorer.collapsed, collapsed);
}

#[test]
fn remote_tree_groups_filters_and_targets_network_confirmations_without_local_diffs() {
    use crossterm::event::{MouseButton::Left, MouseEventKind::Down};
    let fixture = Fixture::new();
    let base = fixture.base();
    for remote in ["origin", "upstream", "empty"] {
        fixture.success(&["remote", "add", remote, &format!("/not/contacted/{remote}")]);
    }
    for reference in [
        "refs/remotes/origin/main",
        "refs/remotes/origin/feature/card",
        "refs/remotes/upstream/main",
        "refs/remotes/upstream/release/v2",
    ] {
        fixture.success(&["update-ref", reference, &base]);
    }
    fixture.success(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    fixture.write("local-only-change", "never show me in remote view\n");
    let before = fixture.repo.guard().unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('4'));
    wait(&mut app);
    assert_eq!(app.snapshot.remote_branches.len(), 4);
    assert!(
        app.remote_tree
            .rows
            .iter()
            .any(|row| row.path == Path::new("empty") && row.count == 0)
    );
    let root = app
        .remote_tree
        .rows
        .iter()
        .position(|row| row.path == Path::new("origin"))
        .unwrap();
    draw_at(&app, 140, 40);
    let geometry = app.geometry.borrow().clone();
    let line = geometry.rows.iter().position(|row| *row == root).unwrap() as u16;
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 10,
        geometry.list.y + 1 + line,
    );
    wait(&mut app);
    assert!(app.preview.contains("Remote: origin") && app.preview.contains("origin/feature/card"));
    assert!(!app.preview.contains("local-only-change"));
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 3,
        geometry.list.y + 1 + line,
    );
    assert!(app.remote_tree.collapsed.contains(Path::new("origin")));
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert!(app.remote_tree.collapsed.contains(Path::new("origin")));
    key(&mut app, KeyCode::Right);
    assert!(!app.remote_tree.collapsed.contains(Path::new("origin")));
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("upstream/release/v2");
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(
        app.remote_tree
            .rows
            .iter()
            .all(|row| row.path.starts_with("upstream"))
    );
    draw_at(&app, 140, 40);
    let geometry = app.geometry.borrow().clone();
    let target = app
        .remote_tree
        .rows
        .iter()
        .position(|row| row.file.is_some())
        .unwrap();
    let line = geometry.rows.iter().position(|row| *row == target).unwrap() as u16;
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 12,
        geometry.list.y + 1 + line,
    );
    wait(&mut app);
    assert!(
        app.preview.contains("Remote: upstream")
            && app.preview.contains("Branch: release/v2")
            && app.preview.contains("Compared with current HEAD")
    );
    key(&mut app, KeyCode::Char('f'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Confirm { task: app::Task::Git(action), .. }) if action.args.last().is_some_and(|arg| arg == "upstream"))
    );
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Char('p'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Confirm { task: app::Task::Git(action), detail }) if action.args.last().is_some_and(|arg| arg == "release/v2") && detail.contains("upstream/release/v2"))
    );
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Char('P'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Confirm { task: app::Task::Git(action), detail }) if action.args.last().is_some_and(|arg| arg == "HEAD:refs/heads/release/v2") && detail.contains("upstream"))
    );
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn tabs_are_clickable_and_commit_tree_slash_filters_instead_of_searching_the_diff() {
    use crossterm::event::{MouseButton::Left, MouseEventKind::Down};
    let fixture = navigation_fixture();
    let before = fixture.repo.guard().unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    draw_at(&app, 140, 40);
    let (area, _) = app
        .tab_hits
        .borrow()
        .iter()
        .find(|(_, workspace)| *workspace == app::Workspace::History)
        .copied()
        .unwrap();
    mouse(&mut app, Down(Left), area.x + 2, area.y);
    assert_eq!(app.workspace, app::Workspace::History);
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("a.rs");
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.tree.filter == "a.rs" && view.history.tree.rows.iter().filter(|row| row.file.is_some()).count() == 1 && view.history.search.query.is_empty())
    );
    key(&mut app, KeyCode::Esc);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.tree.filter.is_empty())
    );
    fixture.repo.verify(&before).unwrap();
}

fn mouse(app: &mut app::App, kind: crossterm::event::MouseEventKind, x: u16, y: u16) {
    app.handle_mouse(crossterm::event::MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn folder_names_have_no_counts_in_working_and_commit_file_trees() {
    let fixture = Fixture::new();
    fixture.write("src/file-123", "base\n");
    fixture.commit("src/file-456", "base\n", "two files");
    fixture.write("src/file-123", "changed\n");
    fixture.write("src/file-456", "changed\n");
    let working = app::App::ready(&fixture.repo.root).unwrap();
    let historical = open_history(&fixture);
    for app in [&working, &historical] {
        let terminal = draw_at(app, 140, 40);
        let geometry = app.geometry.borrow().clone();
        let buffer = terminal.backend().buffer();
        let lines = (geometry.list.y..geometry.list.y + geometry.list.height)
            .map(|y| {
                (geometry.list.x..geometry.list.x + geometry.list.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let folder = lines
            .iter()
            .find(|line| line.contains("src/"))
            .expect("folder row");
        let suffix = folder.split_once("src/").unwrap().1;
        assert!(
            !suffix.chars().any(|ch| ch.is_ascii_digit() || ch == '('),
            "unexpected count: {folder}"
        );
        assert!(lines.iter().any(|line| line.contains("file-123")));
        assert!(lines.iter().any(|line| line.contains("file-456")));
    }
}

#[test]
fn clicks_select_scrolled_files_commits_and_graph_connector_rows_without_writes() {
    use crossterm::event::{MouseButton::Left, MouseEventKind::Down};
    let fixture = Fixture::new();
    for index in 0..35 {
        fixture.write(&format!("src/{index:02}.txt"), "content\n");
    }
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    for _ in 0..30 {
        key(&mut app, KeyCode::Down);
    }
    draw_at(&app, 140, 20);
    let geometry = app.geometry.borrow().clone();
    let row = geometry.rows[0];
    assert!(row > 0);
    let expected = app.explorer.rows[row].file.unwrap();
    let before = fixture.repo.guard().unwrap();
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 5,
        geometry.list.y + 1,
    );
    assert_eq!(app.selected, expected);
    wait(&mut app);
    assert!(app.preview.contains("content"));
    mouse(
        &mut app,
        Down(Left),
        geometry.preview.x + 3,
        geometry.preview.y + 3,
    );
    assert_eq!(app.focus, app::Focus::Preview);
    fixture.repo.verify(&before).unwrap();
    // Clicking a folder expands/collapses, but never stages it.
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Home);
    draw_at(&app, 140, 30);
    let geometry = app.geometry.borrow().clone();
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 5,
        geometry.list.y + 2,
    );
    assert!(app.explorer.collapsed.contains(Path::new("src")));
    assert!(app.busy.is_none());
    fixture.repo.verify(&before).unwrap();
    for index in 0..8 {
        fixture.commit(
            "history-file",
            &format!("{index}\n"),
            &format!("commit {index}"),
        );
    }
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('g'));
    app.snapshot.commits[0].connectors = vec!["|\\".into(), "| |".into()];
    draw_at(&app, 140, 20);
    let geometry = app.geometry.borrow().clone();
    assert_eq!(&geometry.rows[..3], &[0, 0, 0]);
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 5,
        geometry.list.y + 3,
    );
    assert_eq!(app.selected, 0, "connector row belongs to its commit");
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 5,
        geometry.list.y + 4,
    );
    assert_eq!(app.selected, 1);
    wait(&mut app);
    assert!(app.preview.contains("commit 6"));
}

#[test]
fn file_wheel_scrolls_viewport_without_selection_or_preview_changes() {
    use crossterm::event::{
        MouseButton::Left,
        MouseEventKind::{Down, ScrollDown, ScrollUp},
    };
    let fixture = Fixture::new();
    for index in 0..60 {
        fixture.write(&format!("file-{index:02}"), &format!("content {index}\n"));
    }
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    wait(&mut app);
    draw_at(&app, 140, 30);
    let geometry = app.geometry.borrow().clone();
    let selected = app.selected;
    let cursor = app.explorer.cursor;
    let preview = app.preview.clone();
    let before = fixture.repo.guard().unwrap();
    for _ in 0..25 {
        mouse(
            &mut app,
            ScrollDown,
            geometry.list.x + 5,
            geometry.list.y + 2,
        );
        draw_at(&app, 140, 30);
    }
    let scrolled = app.geometry.borrow().clone();
    assert!(
        scrolled.rows[0] > cursor,
        "selected row can scroll out of view"
    );
    assert_eq!(app.selected, selected);
    assert_eq!(app.explorer.cursor, cursor);
    assert_eq!(app.preview, preview);
    assert!(!app.loading(), "scrolling must not start a preview read");
    assert_eq!(scrolled.rows.last(), Some(&(app.explorer.rows.len() - 1)));
    mouse(&mut app, ScrollUp, geometry.list.x + 5, geometry.list.y + 2);
    draw_at(&app, 140, 30);
    assert_eq!(app.geometry.borrow().rows[0], scrolled.rows[0] - 3);
    // Hit-testing must use the scrolled viewport, not the selected row.
    let row = app.geometry.borrow().rows[0];
    let expected = app.explorer.rows[row].file.unwrap();
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 5,
        geometry.list.y + 1,
    );
    wait(&mut app);
    assert_eq!(app.selected, expected);
    assert_eq!(app.explorer.cursor, row);
    mouse(&mut app, ScrollUp, geometry.list.x + 5, geometry.list.y + 2);
    key(&mut app, KeyCode::Down);
    draw_at(&app, 140, 30);
    assert_eq!(app.explorer.cursor, row + 1);
    assert!(app.geometry.borrow().rows.contains(&app.explorer.cursor));
    for _ in 0..25 {
        mouse(&mut app, ScrollUp, geometry.list.x + 5, geometry.list.y + 2);
    }
    draw_at(&app, 140, 30);
    assert_eq!(app.geometry.borrow().rows[0], 0);
    draw_at(&app, 140, 60); // Resize while the selected row is offscreen.
    assert_eq!(app.selected, expected + 1);
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn commit_file_wheel_preserves_selected_file_and_preview() {
    use crossterm::event::{
        MouseButton::Left,
        MouseEventKind::{Down, ScrollDown, ScrollUp},
    };
    let fixture = Fixture::new();
    for index in 0..60 {
        fixture.write(&format!("file-{index:02}"), "content\n");
    }
    fixture.commit("last", "last\n", "many files");
    let mut app = open_history(&fixture);
    draw_at(&app, 140, 30);
    let geometry = app.geometry.borrow().clone();
    let (selected, cursor, preview) = match &app.modal {
        Some(app::Modal::Commit(view)) => (
            view.selected,
            view.history.tree.cursor,
            view.preview.clone(),
        ),
        _ => unreachable!(),
    };
    for _ in 0..25 {
        mouse(
            &mut app,
            ScrollDown,
            geometry.list.x + 5,
            geometry.list.y + 2,
        );
        draw_at(&app, 140, 30);
    }
    assert!(app.geometry.borrow().rows[0] > cursor);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.selected == selected && view.history.tree.cursor == cursor && view.preview == preview)
    );
    assert!(!app.loading());
    let offset = app.geometry.borrow().rows[0];
    mouse(&mut app, ScrollUp, geometry.list.x + 5, geometry.list.y + 2);
    draw_at(&app, 140, 30);
    assert_eq!(app.geometry.borrow().rows[0], offset - 3);
    let row = app.geometry.borrow().rows[0];
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 5,
        geometry.list.y + 1,
    );
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.tree.cursor == row && view.selected != selected)
    );
    key(&mut app, KeyCode::Down);
    draw_at(&app, 140, 30);
    assert!(app.geometry.borrow().rows.contains(&(row + 1)));
}

#[test]
fn dividers_drag_resize_with_shortcuts_and_restore_without_overwriting_other_settings() {
    use crossterm::event::{
        MouseButton::Left,
        MouseEventKind::{Down, Drag, Up},
    };
    let fixture = Fixture::new();
    fixture.write("a", "a\n");
    fixture.write("b", "b\n");
    let config = tempfile::tempdir().unwrap();
    fs::write(
        config.path().join("config.toml"),
        "# layout test\ntheme = 'nord'\n[other]\nvalue = 42\n",
    )
    .unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    app.config_root = Some(config.path().into());
    draw_at(&app, 140, 40);
    let geometry = app.geometry.borrow().clone();
    mouse(
        &mut app,
        Down(Left),
        geometry.divider.x,
        geometry.divider.y + 2,
    );
    mouse(&mut app, Drag(Left), 70, 10);
    mouse(&mut app, Up(Left), 70, 10);
    assert_eq!(app.config.panes.files_width, 50);
    assert_eq!(
        config::Config::load(config.path())
            .unwrap()
            .panes
            .files_width,
        50
    );
    draw_at(&app, 140, 40);
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
    assert_eq!(app.config.panes.files_width, 52);
    draw_at(&app, 60, 30);
    let geometry = app.geometry.borrow().clone();
    mouse(
        &mut app,
        Down(Left),
        geometry.divider.x + 2,
        geometry.divider.y,
    );
    mouse(
        &mut app,
        Drag(Left),
        10,
        geometry.area.y + geometry.area.height / 2,
    );
    mouse(&mut app, Up(Left), 10, 15);
    let saved = config::Config::load(config.path()).unwrap();
    assert_eq!(saved.panes.files_width, 52);
    assert!((45..=50).contains(&saved.panes.files_height));
    let mut reopened = app::App::ready(&fixture.repo.root).unwrap();
    reopened.config = saved;
    draw_at(&reopened, 140, 40);
    assert_eq!(reopened.geometry.borrow().list.width, 72);
    let text = fs::read_to_string(config.path().join("config.toml")).unwrap();
    assert!(text.contains("# layout test") && text.contains("42") && text.contains("'nord'"));
    key(&mut app, KeyCode::Char('2'));
    draw_at(&app, 140, 40);
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    assert_eq!(app.config.panes.history_width, 48);
    assert_eq!(app.config.panes.files_width, 52);
}

#[test]
fn optimistic_stage_unstage_and_failure_reconcile_without_mutating_confirmed_snapshot() {
    let fixture = Fixture::new();
    fixture.commit("file", "base\n", "base");
    fixture.write("file", "changed\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    let status = app.snapshot.files[0].status.clone();
    key(&mut app, KeyCode::Char('s'));
    assert_eq!(app.pending_staging.get(Path::new("file")), Some(&true));
    assert_eq!(app.snapshot.files[0].status, status);
    let terminal = draw_at(&app, 140, 40);
    assert!(
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.fg == app.theme.added)
    );
    wait(&mut app);
    assert!(app.pending_staging.is_empty());
    assert_eq!(app.snapshot.files[0].status, "M ");
    key(&mut app, KeyCode::Char('u'));
    assert_eq!(app.pending_staging.get(Path::new("file")), Some(&false));
    assert_eq!(app.snapshot.files[0].status, "M ");
    wait(&mut app);
    assert_eq!(app.snapshot.files[0].status, " M");
    assert!(app.pending_staging.is_empty());
    fs::write(fixture.repo.root.join(".git/index.lock"), "test lock").unwrap();
    key(&mut app, KeyCode::Char('s'));
    assert_eq!(app.pending_staging.get(Path::new("file")), Some(&true));
    wait(&mut app);
    assert!(app.pending_staging.is_empty());
    assert_eq!(app.snapshot.files[0].status, " M");
    assert!(app.last_output.contains("index.lock"));
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "changed\n"
    );
    assert!(
        fixture
            .repo
            .bytes(&["diff", "--cached"])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn single_file_views_skip_the_tree_and_tree_toggle_does_not_capture_search_input() {
    let fixture = Fixture::new();
    fixture.commit("src/only-file", "base\n", "base");
    fixture.write("src/only-file", "changed\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    draw_at(&app, 140, 40);
    assert!(!app.files_tree_visible());
    assert!(app.geometry.borrow().list.is_empty());
    assert_eq!(app.focus, app::Focus::Preview);
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('1'));
    assert_eq!(app.focus, app::Focus::Preview);
    key(&mut app, KeyCode::Up);
    assert!(
        app.file_selected(),
        "hidden folders must never become navigation targets"
    );
    key(&mut app, KeyCode::Char(' '));
    assert!(app.busy.is_some());
    wait(&mut app);
    key(&mut app, KeyCode::Char('T'));
    draw_at(&app, 140, 40);
    assert!(app.files_tree_visible() && !app.geometry.borrow().list.is_empty());
    key(&mut app, KeyCode::Char('T'));
    assert!(!app.files_tree_visible());
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    draw_at(&app, 140, 40);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if !view.show_tree && view.focus == app::Focus::Preview)
    );
    assert!(app.geometry.borrow().list.is_empty());
    key(&mut app, KeyCode::Char('/'));
    key(&mut app, KeyCode::Char('T'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.search.editing.as_deref() == Some("T") && !view.show_tree)
    );
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Char('T'));
    draw_at(&app, 140, 40);
    assert!(matches!(&app.modal, Some(app::Modal::Commit(view)) if view.show_tree));
    assert!(!app.geometry.borrow().list.is_empty());
}

#[test]
fn adaptive_tree_handles_one_many_and_clean_changes_and_failed_layout_saves() {
    use crossterm::event::{
        MouseButton::Left,
        MouseEventKind::{Down, Drag, Up},
    };
    let fixture = Fixture::new();
    fixture.commit("a", "base\n", "base");
    fixture.write("a", "changed\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    assert!(!app.files_tree_visible());
    fixture.write("b", "new\n");
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert!(app.files_tree_visible());
    let config = tempfile::tempdir().unwrap();
    fs::write(config.path().join("config.toml"), "broken = [").unwrap();
    app.config_root = Some(config.path().into());
    draw_at(&app, 140, 40);
    let geometry = app.geometry.borrow().clone();
    mouse(
        &mut app,
        Down(Left),
        geometry.divider.x,
        geometry.divider.y + 2,
    );
    mouse(&mut app, Drag(Left), 70, 10);
    mouse(&mut app, Up(Left), 70, 10);
    assert_eq!(app.config.panes.files_width, 50);
    assert!(
        app.error
            .as_deref()
            .is_some_and(|error| error.contains("could not be saved"))
    );
    assert_eq!(
        fs::read_to_string(config.path().join("config.toml")).unwrap(),
        "broken = ["
    );
    key(&mut app, KeyCode::Char('T'));
    assert!(!app.files_tree_visible());
    fixture.write("a", "base\n");
    fs::remove_file(fixture.repo.root.join("b")).unwrap();
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert!(
        app.files_tree_visible(),
        "clean state keeps its repository root"
    );
    assert_eq!(app.explorer.rows.len(), 1);
}

#[test]
fn commit_browser_clicks_and_dividers_work_but_other_dialogs_block_mouse_input() {
    use crossterm::event::{
        MouseButton::Left,
        MouseEventKind::{Down, Drag, Up},
    };
    let fixture = Fixture::new();
    fixture.write("src/a", "first file\n");
    fixture.commit("src/b", "second file\n", "two files");
    let config = tempfile::tempdir().unwrap();
    let mut app = open_history(&fixture);
    app.config_root = Some(config.path().into());
    draw_at(&app, 140, 40);
    let geometry = app.geometry.borrow().clone();
    let target = match &app.modal {
        Some(app::Modal::Commit(view)) => view
            .history
            .tree
            .rows
            .iter()
            .position(|row| row.file == Some(1))
            .unwrap(),
        _ => unreachable!(),
    };
    let visible = geometry.rows.iter().position(|row| *row == target).unwrap() as u16;
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 5,
        geometry.list.y + 1 + visible,
    );
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.selected == 1 && view.preview.contains("+second file"))
    );
    mouse(
        &mut app,
        Down(Left),
        geometry.divider.x,
        geometry.divider.y + 3,
    );
    mouse(&mut app, Drag(Left), 70, 15);
    mouse(&mut app, Up(Left), 70, 15);
    assert_eq!(
        config::Config::load(config.path())
            .unwrap()
            .panes
            .commit_width,
        50
    );
    draw_at(&app, 60, 30);
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
    assert_eq!(app.config.panes.commit_height, 34);
    key(&mut app, KeyCode::Esc);
    draw_at(&app, 140, 40);
    let before = app.selected;
    key(&mut app, KeyCode::Char('?'));
    mouse(
        &mut app,
        Down(Left),
        geometry.list.x + 5,
        geometry.list.y + 1,
    );
    assert_eq!(app.selected, before);
    assert!(matches!(&app.modal, Some(app::Modal::Help { .. })));
}

#[test]
fn all_file_and_hunk_staging_are_immediate_and_keep_working_content() {
    let fixture = Fixture::new();
    fixture.commit("file", "base\n", "base");
    fixture.write("file", "staged\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('S'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    fixture.write("file", "working\n");
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    key(&mut app, KeyCode::Char('S'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(
        fixture
            .action(&["diff", "--cached"])
            .text()
            .contains("+working")
    );
    key(&mut app, KeyCode::Char('U'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(fixture.action(&["diff", "--cached"]).text().is_empty());
    key(&mut app, KeyCode::Char('v'));
    wait(&mut app);
    assert!(matches!(app.modal, Some(app::Modal::Hunks(_))));
    key(&mut app, KeyCode::Char('s'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(
        fixture
            .action(&["diff", "--cached"])
            .text()
            .contains("+working")
    );
    key(&mut app, KeyCode::Char('v'));
    wait(&mut app);
    assert!(matches!(&app.modal, Some(app::Modal::Hunks(hunks)) if hunks.cached));
    key(&mut app, KeyCode::Char('s'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(fixture.action(&["diff", "--cached"]).text().is_empty());
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "working\n"
    );
    assert!(app.error.is_none(), "{:?}", app.error);
}

#[test]
fn path_tooltips_reveal_full_names_by_keyboard_and_hover_without_selecting() {
    use ratatui::{Terminal, backend::TestBackend};
    let fixture = Fixture::new();
    for index in 0..35 {
        fixture.write(
            &format!("deep/nested/{index:02}-long-unambiguous-filename-雪.txt"),
            "content\n",
        );
    }
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    let text = |terminal: &Terminal<TestBackend>| {
        // TestBackend retains continuation cells; real terminals display only
        // the leading cell of each wide character.
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for row in buffer.content.chunks(usize::from(buffer.area.width)) {
            let mut skip = 0;
            for cell in row {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                text.push_str(cell.symbol());
                skip = ratatui::text::Span::raw(cell.symbol())
                    .width()
                    .saturating_sub(1);
            }
        }
        text
    };
    key(&mut app, KeyCode::Char('w'));
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    assert!(text(&terminal).contains("Full path"));
    assert!(text(&terminal).contains("deep/nested/00-long-unambiguous-filename-雪.txt"));
    key(&mut app, KeyCode::Esc);
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    assert!(!text(&terminal).contains("Full path"));
    for _ in 0..30 {
        key(&mut app, KeyCode::Down);
    }
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    let offset = app.explorer_state.borrow().offset();
    assert!(offset > 0);
    let selected = app.explorer.cursor;
    let hovered = app.explorer.rows[offset]
        .path
        .to_string_lossy()
        .into_owned();
    // First visible tree row, accounting for the header, tabs and border.
    app.mouse_position = Some((5, 5));
    app.hover_started = Some(Instant::now() - Duration::from_millis(350));
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    assert!(text(&terminal).contains("Full path"));
    assert!(
        text(&terminal).contains(&hovered),
        "expected {hovered:?} at offset {offset}: {}",
        text(&terminal)
    );
    assert_eq!(app.explorer.cursor, selected);
    app.mouse_position = Some((139, 29));
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    assert!(!text(&terminal).contains("Full path"));
    for (width, height) in [(44, 12), (60, 24), (100, 40)] {
        key(&mut app, KeyCode::Char('w'));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        assert!(text(&terminal).contains("Full path"));
        key(&mut app, KeyCode::Esc);
    }
}

#[test]
fn destructive_confirmation_is_centered_over_the_workspace_and_escape_cancels() {
    use ratatui::{Terminal, backend::TestBackend};
    let fixture = Fixture::new();
    fixture.commit("file", "base\n", "base");
    fixture.write("file", "changed\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    let before = fixture.repo.guard().unwrap();
    key(&mut app, KeyCode::Char(':'));
    app.handle_paste("discard");
    key(&mut app, KeyCode::Enter);
    for (width, height) in [(140, 40), (60, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(0, 4)].symbol(),
            "┌",
            "workspace remains behind modal"
        );
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("file"));
        assert!(rendered.contains("Review action"));
        assert!(rendered.contains("[y] Proceed"));
    }
    app.handle_paste("y");
    assert!(app.busy.is_none());
    key(&mut app, KeyCode::Esc);
    assert!(app.modal.is_none() && app.busy.is_none());
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn quitting_during_a_worker_requests_orderly_completion() {
    let fixture = Fixture::new();
    fixture.write("file", "keep this content\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('s'));
    assert!(app.busy.is_some());
    key(&mut app, KeyCode::Char('q'));
    assert!(app.quit_requested);
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    assert_eq!(fixture.repo.text(&["ls-files"]).unwrap(), "file");
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "keep this content\n"
    );
}

#[test]
fn ui_stages_updates_to_a_staged_rename_and_unstages_both_paths() {
    let fixture = Fixture::new();
    let original = "base line\n".repeat(20);
    fixture.commit("old", &original, "base");
    fixture.repo.bytes(&["mv", "old", "new"]).unwrap();
    fixture.write("new", &format!("{original}additional line\n"));
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    assert!(app.snapshot.files[0].original.is_some());
    key(&mut app, KeyCode::Char('s'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    assert!(fixture.repo.bytes(&["diff", "--"]).unwrap().is_empty());
    key(&mut app, KeyCode::Char('u'));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert!(app.error.is_none(), "{:?}", app.error);
    assert_eq!(fixture.repo.text(&["ls-files"]).unwrap(), "old");
    assert!(fixture.repo.root.join("new").exists());
}

#[test]
fn ui_push_names_the_actual_upstream_destination() {
    let fixture = Fixture::new();
    fixture.base();
    fixture
        .repo
        .bytes(&["remote", "add", "origin", "/not/contacted/remote.git"])
        .unwrap();
    fixture
        .repo
        .bytes(&["update-ref", "refs/remotes/origin/trunk", "HEAD"])
        .unwrap();
    fixture
        .repo
        .bytes(&["config", "branch.main.remote", "origin"])
        .unwrap();
    fixture
        .repo
        .bytes(&["config", "branch.main.merge", "refs/heads/trunk"])
        .unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('P'));
    let Some(app::Modal::Confirm {
        task: app::Task::Git(action),
        detail,
    }) = &app.modal
    else {
        panic!("missing push confirmation")
    };
    assert_eq!(action.args.last().unwrap(), "HEAD:refs/heads/trunk");
    assert!(action.idle && detail.contains("Destination remote branch: trunk"));
    assert!(
        !action
            .args
            .iter()
            .any(|arg| arg == "--force" || arg == "--force-with-lease")
    );
    key(&mut app, KeyCode::Char('n'));
    assert!(app.busy.is_none());
}

#[test]
fn ui_conflict_and_rebase_workspaces_render_real_content_and_safe_cancel() {
    use ratatui::{Terminal, backend::TestBackend};
    let fixture = Fixture::new();
    fixture.conflict();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(matches!(app.modal, Some(app::Modal::Conflict(_))));
    key(&mut app, KeyCode::Char('3'));
    for (width, height) in [(140, 40), (60, 24), (44, 12)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        if height < 24 {
            assert!(screen.contains("Resize"));
        } else {
            assert!(screen.contains("Result"));
        }
        if width >= 100 {
            assert!(
                screen.contains("Current")
                    && screen.contains("Incoming")
                    && screen.contains("UNSAVED")
            );
        }
    }
    key(&mut app, KeyCode::Char('q'));
    assert!(matches!(&app.modal, Some(app::Modal::Conflict(conflict)) if conflict.confirm_close));
    key(&mut app, KeyCode::Char('n'));
    assert!(matches!(&app.modal, Some(app::Modal::Conflict(conflict)) if conflict.dirty()));
    key(&mut app, KeyCode::Char('q'));
    key(&mut app, KeyCode::Char('y'));
    assert!(conflict::contains_markers(
        &fs::read(fixture.repo.root.join("file")).unwrap()
    ));
    fixture.success(&["merge", "--abort"]);
    fixture.commit("another", "another\n", "another");
    let base = fixture.repo.text(&["rev-parse", "HEAD~1"]).unwrap();
    let plan = Plan::load(&fixture.repo, &fixture.repo.snapshot().unwrap(), &base).unwrap();
    app.modal = Some(app::Modal::Rebase(Box::new(plan)));
    for (width, height) in [(140, 40), (60, 24), (44, 12)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    }
    let head = fixture.repo.head().unwrap();
    key(&mut app, KeyCode::Esc);
    assert_eq!(fixture.repo.head().unwrap(), head);
    assert_eq!(fixture.repo.operation().unwrap(), None);
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_are_preserved_and_symlink_edits_refused() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let fixture = Fixture::new();
    let name = OsString::from_vec(b"invalid-\xff".to_vec());
    let parsed = git::parse_status(b"?? invalid-\xff\0").unwrap();
    assert_eq!(parsed[0].path.as_os_str(), &name);
    match fs::write(fixture.repo.root.join(&name), "content") {
        Ok(()) => {
            let snapshot = fixture.repo.snapshot().unwrap();
            assert_eq!(snapshot.files[0].path.as_os_str(), &name);
            let action = Action::new(&snapshot, "stage", &["add"]).path(Path::new(&name));
            assert!(fixture.repo.execute(&action).unwrap().success);
        }
        // APFS rejects illegal byte sequences; parsing is still checked above.
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {}
        Err(error) => panic!("Could not create non-UTF-8 fixture: {error}"),
    }
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("precious"), "do not overwrite").unwrap();
    symlink(outside.path(), fixture.repo.root.join("link")).unwrap();
    assert!(fixture.repo.safe_file(Path::new("link/precious")).is_err());
    assert_eq!(
        fs::read_to_string(outside.path().join("precious")).unwrap(),
        "do not overwrite"
    );
}

#[test]
fn asynchronous_refresh_preserves_selection_scroll_and_defers_confirmations() {
    let fixture = Fixture::new();
    let mut app = app::App::new(&fixture.repo.root).unwrap();
    assert!(!app.loaded);
    wait(&mut app);
    fixture.write("a", "alpha\n");
    fixture.write("b", "beta\nline2\nline3\nline4\n");
    app.auto_refresh(Instant::now() + Duration::from_secs(3));
    wait(&mut app);
    assert_eq!(app.snapshot.files.len(), 2);
    key(&mut app, KeyCode::Char('j'));
    wait(&mut app);
    assert_eq!(app.snapshot.files[app.selected].path, Path::new("b"));
    app.scroll = 2;
    app.horizontal = 4;
    fixture.write("0-new", "new\n");
    app.auto_refresh(Instant::now() + Duration::from_secs(3));
    wait(&mut app);
    assert_eq!(app.snapshot.files[app.selected].path, Path::new("b"));
    assert_eq!((app.scroll, app.horizontal), (2, 4));
    fixture.write("c", "later\n");
    key(&mut app, KeyCode::Char('r'));
    key(&mut app, KeyCode::Char(':'));
    for character in "discard".chars() {
        key(&mut app, KeyCode::Char(character));
    }
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(matches!(app.modal, Some(app::Modal::Confirm { .. })));
    assert_eq!(
        app.snapshot.files.len(),
        3,
        "an in-flight refresh replaced a confirmation's context"
    );
    app.handle_paste("y");
    assert!(app.busy.is_none(), "pasted confirmation must never execute");
    key(&mut app, KeyCode::Char('n'));
    app.poll_loading();
    wait(&mut app);
    assert_eq!(app.snapshot.files.len(), 4);
    assert!(fixture.repo.root.join("b").exists());
}

#[test]
fn rapid_navigation_ignores_stale_preview_results() {
    let fixture = Fixture::new();
    fixture.write("a", "UNIQUE_ALPHA\n");
    fixture.write("b", "UNIQUE_BETA\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    for _ in 0..30 {
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Char('k'));
    }
    key(&mut app, KeyCode::Char('j'));
    wait(&mut app);
    assert_eq!(app.snapshot.files[app.selected].path, Path::new("b"));
    assert!(app.preview.contains("UNIQUE_BETA") && !app.preview.contains("UNIQUE_ALPHA"));
}

#[test]
fn inline_editing_save_cancel_undo_and_stage_are_separate() {
    let fixture = Fixture::new();
    fixture.conflict();
    let original = fs::read(fixture.repo.root.join("file")).unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Char('e'));
    assert!(matches!(app.modal, Some(app::Modal::Inline { .. })));
    app.handle_paste("manual ");
    render_views(&app);
    app.auto_refresh(Instant::now() + Duration::from_secs(3));
    assert!(
        !app.loading(),
        "automatic refresh must not run over an editor draft"
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(matches!(
        app.modal,
        Some(app::Modal::Confirm {
            task: app::Task::SaveConflict(_),
            ..
        })
    ));
    assert_eq!(fs::read(fixture.repo.root.join("file")).unwrap(), original);
    key(&mut app, KeyCode::Char('n'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Conflict(conflict)) if conflict.dirty() && conflict.result() == "manual current\n")
    );
    key(&mut app, KeyCode::Char('u'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Conflict(conflict)) if conflict.result() == "current\n")
    );
    key(&mut app, KeyCode::Char('s'));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "current\n"
    );
    assert!(!fixture.repo.bytes(&["ls-files", "-u"]).unwrap().is_empty());
    key(&mut app, KeyCode::Char('a'));
    wait(&mut app);
    assert!(fixture.repo.bytes(&["ls-files", "-u"]).unwrap().is_empty());
    key(&mut app, KeyCode::Char('C'));
    assert!(matches!(app.modal, Some(app::Modal::Confirm { .. })));
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(fixture.repo.operation().unwrap(), Some(Operation::Merge));
    key(&mut app, KeyCode::Char('C'));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert_eq!(fixture.repo.operation().unwrap(), None);
}

#[test]
fn manually_resolved_or_malformed_drafts_remain_undoable_and_safe() {
    let fixture = Fixture::new();
    fixture.conflict();
    let snapshot = fixture.repo.snapshot().unwrap();
    let mut conflict = Conflict::load(
        &fixture.repo,
        "file".into(),
        snapshot.guard,
        &snapshot.branch,
    )
    .unwrap();
    conflict
        .edit_result("manual without final newline".into())
        .unwrap();
    assert_eq!(conflict.regions().len(), 0);
    assert_eq!(conflict.unresolved(), 0);
    conflict.choose(Choice::Incoming); // no regions must be a safe no-op
    assert_eq!(conflict.result(), "manual without final newline");
    conflict.edit_result("<<<<<<< unfinished\n".into()).unwrap();
    assert!(conflict.parse_error.is_some() && conflict.unresolved() > 0);
    conflict.undo();
    assert_eq!(conflict.result(), "manual without final newline");
    conflict.undo();
    assert_eq!(conflict.unresolved(), 1);
}

#[test]
fn confirmed_save_rechecks_external_edits_and_retains_the_draft_on_failure() {
    let fixture = Fixture::new();
    fixture.conflict();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Char('s'));
    assert!(matches!(
        app.modal,
        Some(app::Modal::Confirm {
            task: app::Task::SaveConflict(_),
            ..
        })
    ));
    fixture.write("file", "external editor content\n");
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert!(app.error.is_some());
    assert!(
        matches!(&app.modal, Some(app::Modal::Conflict(conflict)) if conflict.dirty() && conflict.result() == "current\n")
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("file")).unwrap(),
        "external editor content\n"
    );
}

#[test]
fn merge_commit_file_browser_can_compare_both_parents() {
    let fixture = Fixture::new();
    fixture.base();
    fixture.success(&["switch", "-c", "side"]);
    fixture.commit("side", "side\n", "side");
    fixture.success(&["switch", "main"]);
    fixture.commit("main", "main\n", "main");
    fixture.success(&["merge", "--no-ff", "--no-edit", "side"]);
    let first = fixture.repo.commit_files("HEAD", 0).unwrap();
    let second = fixture.repo.commit_files("HEAD", 1).unwrap();
    assert_eq!(first.parents.len(), 2);
    assert_eq!(first.files[0].path, Path::new("side"));
    assert_eq!(second.files[0].path, Path::new("main"));
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    key(&mut app, KeyCode::Char('P'));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.commit.parent == 1 && view.preview.contains("+main"))
    );
}

#[test]
fn historical_files_cover_root_rename_deletion_and_literal_paths() {
    let fixture = Fixture::new();
    fixture.write("alpha", &"line\n".repeat(20));
    fixture.write("deleted", "old content\n");
    let root = fixture.commit(":(glob)*.rs", "literal content\n", "root");
    let first = fixture.repo.commit_files(&root, 0).unwrap();
    assert!(first.parents.is_empty());
    assert_eq!(first.files.len(), 3);
    let literal = first
        .files
        .iter()
        .position(|file| file.path == Path::new(":(glob)*.rs"))
        .unwrap();
    let diff = fixture
        .repo
        .historical_preview(&first, literal, false)
        .unwrap();
    assert!(diff.contains("+literal content") && !diff.contains("old content"));
    fixture.repo.bytes(&["mv", "alpha", "renamed"]).unwrap();
    fixture.repo.bytes(&["rm", "deleted"]).unwrap();
    fixture
        .repo
        .bytes(&["commit", "-m", "rename and delete"])
        .unwrap();
    let commit = fixture.repo.commit_files("HEAD", 0).unwrap();
    let before = fixture.repo.guard().unwrap();
    let renamed = commit
        .files
        .iter()
        .position(|file| file.path == Path::new("renamed"))
        .unwrap();
    assert_eq!(
        commit.files[renamed].original.as_deref(),
        Some(Path::new("alpha"))
    );
    assert!(
        fixture
            .repo
            .historical_preview(&commit, renamed, false)
            .unwrap()
            .contains("rename from alpha")
    );
    let deleted = commit
        .files
        .iter()
        .position(|file| file.status == "D")
        .unwrap();
    assert_eq!(
        fixture
            .repo
            .historical_preview(&commit, deleted, true)
            .unwrap(),
        "old content\n"
    );
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn history_enter_drills_into_files_and_full_content_without_checkout() {
    let fixture = Fixture::new();
    fixture.write("a", "alpha\n");
    fixture.commit("b", "beta\n", "root");
    let before = fixture.repo.guard().unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.commit.files.len() == 2 && view.preview.contains("+alpha"))
    );
    key(&mut app, KeyCode::Char('j'));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.selected == 1 && view.preview.contains("+beta") && !view.preview.contains("+alpha"))
    );
    key(&mut app, KeyCode::Char('t'));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.full && view.preview == "beta\n")
    );
    render_views(&app);
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Down);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.focus == app::Focus::Preview && view.scroll == 0)
    );
    key(&mut app, KeyCode::Esc);
    wait(&mut app);
    assert!(app.modal.is_none() && app.workspace == app::Workspace::History);
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn historical_full_file_navigation_can_reach_beyond_u16_line_offsets() {
    let fixture = Fixture::new();
    fixture.commit("many-lines", &"x\n".repeat(70_000), "many lines");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    key(&mut app, KeyCode::Char('t'));
    wait(&mut app);
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::End);
    assert!(matches!(&app.modal, Some(app::Modal::Commit(view)) if view.scroll == 69_999));
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(screen.contains("70000 x"));
}

fn open_history(fixture: &Fixture) -> app::App {
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(matches!(app.modal, Some(app::Modal::Commit(_))));
    app
}

fn navigation_fixture() -> Fixture {
    let fixture = Fixture::new();
    let original = (0..70)
        .map(|index| format!("line {index}\n"))
        .collect::<String>();
    fixture.write("src/a.rs", &original);
    fixture.write("src/nested/b.rs", "old beta\n");
    fixture.commit("zz-root.txt", "old root\n", "base");
    fixture.write(
        "src/a.rs",
        &original
            .replace("line 2\n", "needle café\n")
            .replace("line 55\n", "needle 雪\n"),
    );
    fixture.write("src/nested/b.rs", "beta needle\n");
    fixture.commit("zz-root.txt", "new root\n", "changes");
    fixture
}

#[test]
fn history_tree_shortcuts_search_and_scroll_survive_view_switches() {
    let fixture = navigation_fixture();
    let guard = fixture.repo.guard().unwrap();
    let mut app = open_history(&fixture);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.commit.files[view.selected].path == Path::new("src/nested/b.rs"))
    );
    key(&mut app, KeyCode::Char(']'));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.commit.files[view.selected].path == Path::new("src/a.rs"))
    );
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("NEEDLE");
    key(&mut app, KeyCode::Enter);
    let first = match &app.modal {
        Some(app::Modal::Commit(view)) => {
            assert_eq!(view.history.search.matches.len(), 2);
            assert_eq!(view.history.search.active, Some(0));
            view.scroll
        }
        _ => panic!(),
    };
    key(&mut app, KeyCode::Char('d'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.scroll == first && view.history.split(140, false))
    );
    key(&mut app, KeyCode::Char('n'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.search.active == Some(1) && view.scroll > first)
    );
    key(&mut app, KeyCode::Char('N'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.search.active == Some(0) && view.scroll == first)
    );
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("missing");
    key(&mut app, KeyCode::Esc);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.search.query == "NEEDLE" && view.scroll == first)
    );
    key(&mut app, KeyCode::Char('/'));
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('}'));
    key(&mut app, KeyCode::Char('l'));
    let position = match &app.modal {
        Some(app::Modal::Commit(view)) => (view.scroll, view.horizontal),
        _ => panic!(),
    };
    key(&mut app, KeyCode::Char(']'));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.focus == app::Focus::Preview && view.commit.files[view.selected].path == Path::new("zz-root.txt"))
    );
    key(&mut app, KeyCode::Char('['));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if (view.scroll, view.horizontal) == position)
    );
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Char('h'));
    key(&mut app, KeyCode::Char('h'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.tree.collapsed.contains(Path::new("src")) && view.commit.files[view.selected].path == Path::new("src/a.rs"))
    );
    key(&mut app, KeyCode::Char(']'));
    wait(&mut app);
    key(&mut app, KeyCode::Char('['));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if !view.history.tree.collapsed.contains(Path::new("src")))
    );
    render_views(&app);
    fixture.repo.verify(&guard).unwrap();
}

#[test]
fn split_diffs_render_real_line_numbers_matches_and_narrow_fallback() {
    use ratatui::{Terminal, backend::TestBackend};
    let fixture = navigation_fixture();
    let mut app = open_history(&fixture);
    key(&mut app, KeyCode::Char(']'));
    wait(&mut app);
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Char('d'));
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("needle");
    key(&mut app, KeyCode::Enter);
    for (width, height) in [(140, 40), (100, 30), (60, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            text.contains("needle"),
            "match is not visible at width {width}"
        );
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.bg == app.theme.search_active)
        );
        if width >= 120 {
            assert!(text.contains("Old · parent") && text.contains("New · commit"));
        } else {
            assert!(!text.contains("Old · parent") && text.contains("unified"));
        }
    }
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.layout == history::DiffLayout::SideBySide)
    );
    key(&mut app, KeyCode::Char('t'));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.full && view.history.search.matches.len() == 2 && view.history.layout == history::DiffLayout::SideBySide)
    );
    key(&mut app, KeyCode::Char('t'));
    wait(&mut app);
    assert!(matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.split(140, false)));
}

#[test]
fn search_can_reach_long_utf8_lines_and_pasted_shortcuts_remain_literal() {
    use ratatui::{Terminal, backend::TestBackend};
    let fixture = Fixture::new();
    fixture.commit(
        "file",
        &format!("{}雪 café NEEDLE\n", "x".repeat(100_000)),
        "long line",
    );
    let before = fixture.repo.guard().unwrap();
    let mut app = open_history(&fixture);
    key(&mut app, KeyCode::Char('t'));
    wait(&mut app);
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("CAFÉ");
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.horizontal > usize::from(u16::MAX) && view.history.search.matches.len() == 1)
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .contains("café")
    );
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("q]d/y");
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.history.search.query == "CAFÉq]d/y" && view.history.search.matches.is_empty())
    );
    assert!(app.busy.is_none());
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn parent_switch_preserves_active_file_collapsed_folder_layout_and_search() {
    let fixture = Fixture::new();
    fixture.commit("src/shared", "base\n", "base");
    fixture.success(&["switch", "-c", "side"]);
    fixture.commit("src/shared", "side\n", "side");
    fixture.success(&["switch", "main"]);
    fixture.commit("src/shared", "main\n", "main");
    assert!(!fixture.action(&["merge", "--no-edit", "side"]).success);
    fixture.write("src/shared", "combined\n");
    fixture.success(&["add", "."]);
    fixture.success(&["commit", "-m", "merge"]);
    let mut app = open_history(&fixture);
    key(&mut app, KeyCode::Char('d'));
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("combined");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('T')); // Reveal the normally hidden single-file tree.
    key(&mut app, KeyCode::Char('h'));
    key(&mut app, KeyCode::Char('h'));
    key(&mut app, KeyCode::Char('P'));
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.show_tree && view.commit.parent == 1 && view.commit.files[view.selected].path == Path::new("src/shared") && view.history.tree.collapsed.contains(Path::new("src")) && view.history.layout == history::DiffLayout::SideBySide && view.history.search.query == "combined" && view.history.search.matches.len() == 1)
    );
}

#[test]
fn exact_historical_file_selection_excludes_directory_descendants() {
    let fixture = Fixture::new();
    fixture.commit("foo", &"old file\n".repeat(20), "file");
    fs::remove_file(fixture.repo.root.join("foo")).unwrap();
    fixture.write("foo/child", "unrelated child\n");
    fixture.commit("foo/other", "other child\n", "directory");
    let commit = fixture.repo.commit_files("HEAD", 0).unwrap();
    let deleted = commit
        .files
        .iter()
        .position(|file| file.path == Path::new("foo"))
        .unwrap();
    let preview = fixture
        .repo
        .historical_preview(&commit, deleted, false)
        .unwrap();
    assert!(
        preview.contains("-old file")
            && !preview.contains("+unrelated child")
            && !preview.contains("+other child")
    );

    let fixture = Fixture::new();
    fixture.commit("foo", "rename content\n", "base");
    fs::remove_file(fixture.repo.root.join("foo")).unwrap();
    fixture.write("foo/child", "rename content\n");
    fixture.commit(
        "foo/other",
        "unrelated sibling\n",
        "rename into old directory name",
    );
    let commit = fixture.repo.commit_files("HEAD", 0).unwrap();
    let renamed = commit
        .files
        .iter()
        .position(|file| file.original.as_deref() == Some(Path::new("foo")))
        .unwrap();
    let preview = fixture
        .repo
        .historical_preview(&commit, renamed, false)
        .unwrap();
    assert!(preview.contains("rename from foo") && preview.matches("diff --git").count() == 1);
    assert!(!preview.contains("unrelated sibling"));
}

#[test]
fn exact_historical_pathspecs_escape_globs_unicode_and_leading_special_characters() {
    let fixture = Fixture::new();
    let names = [
        "[a]*?.rs",
        "]leading.rs",
        "!bang.rs",
        "^caret.rs",
        "-dash.rs",
        "\\slash.rs",
        "☃.rs",
        ":(glob)*.rs",
        "nested/quoted[?].rs",
    ];
    for (index, name) in names.iter().enumerate() {
        fixture.write(name, &format!("unique-{index}\n"));
    }
    fixture.success(&["add", "."]);
    fixture.success(&["commit", "-m", "paths"]);
    let commit = fixture.repo.commit_files("HEAD", 0).unwrap();
    for (index, name) in names.iter().enumerate() {
        let selected = commit
            .files
            .iter()
            .position(|file| file.path == Path::new(name))
            .unwrap();
        let preview = fixture
            .repo
            .historical_preview(&commit, selected, false)
            .unwrap();
        assert!(
            preview.contains(&format!("+unique-{index}")),
            "wrong preview for {name}: {preview}"
        );
        assert_eq!(
            preview.matches("+unique-").count(),
            1,
            "unrelated file included for {name}"
        );
    }
}

#[test]
fn history_rapid_file_changes_keep_the_final_document_and_query() {
    let fixture = navigation_fixture();
    let before = fixture.repo.guard().unwrap();
    let mut app = open_history(&fixture);
    for _ in 0..12 {
        key(&mut app, KeyCode::Char(']'));
        key(&mut app, KeyCode::Char(']'));
        key(&mut app, KeyCode::Char('['));
        key(&mut app, KeyCode::Char('['));
    }
    key(&mut app, KeyCode::Char(']'));
    key(&mut app, KeyCode::Tab); // Search the diff, not the focused file tree.
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("needle");
    key(&mut app, KeyCode::Enter);
    wait(&mut app);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.commit.files[view.selected].path == Path::new("src/a.rs") && view.history.search.matches.len() == 2 && view.history.document.can_split() && !view.preview.contains("beta needle") && view.history.document.lines.join("\n") == view.preview.trim_end_matches('\n'))
    );
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn history_empty_and_binary_commits_have_safe_fallback_navigation() {
    let fixture = Fixture::new();
    fixture.base();
    fixture.success(&["commit", "--allow-empty", "-m", "empty"]);
    let before = fixture.repo.guard().unwrap();
    let mut app = open_history(&fixture);
    for code in [
        KeyCode::Char('d'),
        KeyCode::Char(']'),
        KeyCode::Char('['),
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Enter,
    ] {
        key(&mut app, code);
    }
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.commit.files.is_empty() && !view.history.document.can_split())
    );
    render_views(&app);
    fixture.repo.verify(&before).unwrap();

    fs::write(fixture.repo.root.join("binary"), b"binary\0payload").unwrap();
    fixture.success(&["add", "."]);
    fixture.success(&["commit", "-m", "binary"]);
    let mut app = open_history(&fixture);
    key(&mut app, KeyCode::Char('d'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if !view.history.document.can_split() && view.history.layout == history::DiffLayout::SideBySide && view.preview.contains("Binary files"))
    );
    render_views(&app);
}

#[cfg(unix)]
#[test]
fn exact_historical_pathspecs_preserve_non_utf8_bytes_where_the_filesystem_allows() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    let name = OsString::from_vec(b"\xff-first".to_vec());
    match fs::write(fixture.repo.root.join(&name), "raw byte content\n") {
        Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => return,
        result => result.unwrap(),
    }
    fixture.success(&["add", "."]);
    fixture.success(&["commit", "-m", "raw path"]);
    let commit = fixture.repo.commit_files("HEAD", 0).unwrap();
    assert_eq!(commit.files[0].path.as_os_str(), &name);
    assert!(
        fixture
            .repo
            .historical_preview(&commit, 0, false)
            .unwrap()
            .contains("+raw byte content")
    );
}

#[test]
fn searching_eof_annotations_keeps_the_match_visible_when_toggling_layouts() {
    use ratatui::{Terminal, backend::TestBackend};
    let fixture = Fixture::new();
    fixture.commit("file", &format!("{}old", "x".repeat(5000)), "base");
    fixture.commit("file", &format!("{}new", "x".repeat(5000)), "update");
    let mut app = open_history(&fixture);
    app.screen_width = 140;
    key(&mut app, KeyCode::Char('d'));
    key(&mut app, KeyCode::Char('/'));
    app.handle_paste("No newline");
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.horizontal > 4000 && view.history.search.matches.len() == 2)
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.bg == app.theme.search_active)
            .count()
            >= 10
    );
    key(&mut app, KeyCode::Char('n'));
    key(&mut app, KeyCode::Char('d'));
    assert!(
        matches!(&app.modal, Some(app::Modal::Commit(view)) if view.horizontal == 0 && view.history.search.active == Some(1))
    );
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.bg == app.theme.search_active)
            .count()
            >= 10
    );
}

fn select_folder(app: &mut app::App, path: &str) {
    app.focus = app::Focus::List;
    app.explorer.cursor = app
        .explorer
        .rows
        .iter()
        .position(|row| row.file.is_none() && row.path == Path::new(path))
        .unwrap();
}

#[test]
fn explorer_uses_detected_project_root_when_launched_inside_a_subdirectory() {
    use ratatui::{Terminal, backend::TestBackend};
    let fixture = Fixture::new();
    fixture.write("apps/portal/src/a", "base\n");
    fixture.commit("root-file", "base\n", "base");
    fixture.write("apps/portal/src/a", "modified\n");
    fixture.write("root-file", "modified\n");
    let mut app = app::App::ready(&fixture.repo.root.join("apps/portal/src")).unwrap();
    assert_eq!(
        fs::canonicalize(&app.repo.root).unwrap(),
        fs::canonicalize(&fixture.repo.root).unwrap()
    );
    let root = &app.explorer.rows[0];
    assert!(root.path.as_os_str().is_empty());
    assert_eq!(root.name, fixture.repo.root.file_name().unwrap());
    assert_eq!(root.count, 2);
    assert_eq!(root.depth, 0);
    assert_eq!(app.explorer.rows[1].name, "apps/portal/src");
    assert_eq!(app.explorer.rows[1].depth, 1);
    key(&mut app, KeyCode::Home);
    assert!(!app.file_selected());
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.explorer.rows.len(), 1);
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert_eq!(app.explorer.rows.len(), 1);
    assert_eq!(app.explorer.selected().unwrap().count, 2);
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains(&format!(
        "{}/",
        fixture.repo.root.file_name().unwrap().to_string_lossy()
    )));
    key(&mut app, KeyCode::Right);
    assert!(app.explorer.rows.len() > 1);
}

#[test]
fn project_root_supports_immediate_staging_and_remains_visible_when_clean() {
    let fixture = Fixture::new();
    fixture.write("src/a", "base\n");
    fixture.commit("root-file", "base\n", "base");
    fixture.write("src/a", "changed\n");
    fixture.write("root-file", "changed\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Char('s'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert_eq!(
        fixture
            .action(&["diff", "--cached", "--name-only"])
            .text()
            .lines()
            .count(),
        2
    );
    assert!(app.explorer.selected().unwrap().path.as_os_str().is_empty());
    key(&mut app, KeyCode::Char('u'));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert!(
        fixture
            .action(&["diff", "--cached", "--name-only"])
            .text()
            .is_empty()
    );
    fixture.write("src/a", "base\n");
    fixture.write("root-file", "base\n");
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert_eq!(app.explorer.rows.len(), 1);
    assert_eq!(app.explorer.rows[0].count, 0);
    assert!(app.preview.contains("Working tree is clean"));
    render_views(&app);
}

#[test]
fn staging_files_and_folders_with_spaces_preserves_literal_paths() {
    let fixture = Fixture::new();
    fixture.write("folder with spaces/a file.txt", "first\n");
    fixture.write("folder with spaces/another file.txt", "second\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    assert!(app.file_selected());
    key(&mut app, KeyCode::Char('s'));
    wait(&mut app);
    assert_eq!(fixture.action(&["ls-files"]).text().lines().count(), 1);
    key(&mut app, KeyCode::Char('u'));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert!(fixture.action(&["ls-files"]).text().is_empty());
    select_folder(&mut app, "folder with spaces");
    key(&mut app, KeyCode::Char('s'));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    let staged = fixture.action(&["ls-files"]).text();
    assert!(staged.contains("folder with spaces/a file.txt"));
    assert!(staged.contains("folder with spaces/another file.txt"));
    assert_eq!(staged.lines().count(), 2);
}

#[test]
fn space_toggles_file_staging_and_partial_staging_immediately() {
    let fixture = Fixture::new();
    fixture.commit("file with spaces", "base\n", "base");
    fixture.write("file with spaces", "changed\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    key(&mut app, KeyCode::Char(' '));
    wait(&mut app);
    assert!(
        fixture
            .action(&["diff", "--cached"])
            .text()
            .contains("+changed")
    );
    key(&mut app, KeyCode::Char(' '));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(
        fixture
            .action(&["diff", "--cached", "--name-only"])
            .text()
            .is_empty()
    );
    key(&mut app, KeyCode::Char(' '));
    wait(&mut app);
    fixture.write("file with spaces", "newer\n");
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    key(&mut app, KeyCode::Char(' '));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(
        fixture
            .action(&["diff", "--cached"])
            .text()
            .contains("+newer")
    );
}

#[test]
fn space_targets_folders_and_project_root_but_not_previews_prompts_or_paste() {
    let fixture = Fixture::new();
    fixture.write("src/a", "a\n");
    fixture.write("outside", "outside\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    select_folder(&mut app, "src");
    key(&mut app, KeyCode::Char(' '));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert_eq!(fixture.action(&["ls-files"]).text().trim(), "src/a");
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert!(fixture.action(&["ls-files"]).text().is_empty());
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Char(' '));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert_eq!(fixture.action(&["ls-files"]).text().lines().count(), 2);
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert!(fixture.action(&["ls-files"]).text().is_empty());
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Char(' '));
    assert!(app.modal.is_none() && app.busy.is_none());
    key(&mut app, KeyCode::Char('b'));
    key(&mut app, KeyCode::Char(' '));
    assert!(matches!(&app.modal, Some(app::Modal::Prompt { input, .. }) if input == " "));
    key(&mut app, KeyCode::Esc);
    assert!(fixture.action(&["ls-files"]).text().is_empty());
}

#[test]
fn index_refresh_reuses_metadata_only_until_refs_head_or_configuration_change() {
    let fixture = Fixture::new();
    fixture.commit("file", "base\n", "base");
    fixture.write("file", "changed\n");
    let snapshot = fixture.repo.snapshot().unwrap();
    let output = fixture
        .repo
        .execute(&Action::new(&snapshot, "Stage", &["add"]).path(Path::new("file")))
        .unwrap();
    assert!(output.success);
    let updated = fixture.repo.refresh_index(&snapshot).unwrap();
    assert_eq!(updated.files[0].status, "M ");
    assert_eq!(updated.commits[0].oid, snapshot.commits[0].oid);
    fixture.repo.verify(&updated.guard).unwrap();
    fixture.success(&["tag", "external-tag"]);
    let tagged = fixture.repo.refresh_index(&updated).unwrap();
    assert!(tagged.tags.contains(&"external-tag".into()));
    fixture.success(&["switch", "-c", "external-branch"]);
    let branched = fixture.repo.refresh_index(&tagged).unwrap();
    assert_eq!(branched.branch, "external-branch");
    fixture.success(&[
        "remote",
        "add",
        "external-remote",
        "https://example.invalid/repo",
    ]);
    let configured = fixture.repo.refresh_index(&branched).unwrap();
    assert!(configured.remotes.contains(&"external-remote".into()));
    fixture.repo.verify(&configured.guard).unwrap();
    fixture.write("file", "external change after refresh\n");
    assert!(
        fixture
            .repo
            .execute(&Action::new(&configured, "Stage stale", &["add"]).path(Path::new("file")))
            .is_err()
    );
}

#[test]
fn operation_markers_are_checked_in_the_linked_worktrees_own_git_directory() {
    let fixture = Fixture::new();
    fixture.base();
    let worktree = tempfile::tempdir().unwrap();
    fixture.success(&[
        "worktree",
        "add",
        "--detach",
        worktree.path().to_str().unwrap(),
        "HEAD",
    ]);
    let linked = Repo::discover(worktree.path()).unwrap();
    fs::write(
        linked.git_path("CHERRY_PICK_HEAD").unwrap(),
        fixture.repo.head().unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(linked.operation().unwrap(), Some(Operation::CherryPick));
    assert_eq!(fixture.repo.operation().unwrap(), None);
}

#[test]
fn working_tree_explorer_compacts_paths_and_stages_only_the_selected_folder() {
    let fixture = Fixture::new();
    fixture.write("apps/portal/src/feature/a.ts", "base a\n");
    fixture.write("apps/portal/src/feature/b.ts", "base b\n");
    fixture.commit("outside", "base\n", "base");
    fixture.write("apps/portal/src/feature/a.ts", "new a\n");
    fixture.write("apps/portal/src/feature/b.ts", "new b\n");
    fixture.write("outside", "outside change\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    assert!(
        app.explorer
            .rows
            .iter()
            .any(|row| row.name == "apps/portal/src/feature" && row.count == 2)
    );
    select_folder(&mut app, "apps/portal/src/feature");
    assert!(!app.file_selected());
    key(&mut app, KeyCode::Char('s'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    let staged = fixture.action(&["diff", "--cached", "--name-only"]).text();
    assert!(staged.contains("a.ts") && staged.contains("b.ts") && !staged.contains("outside"));
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("outside")).unwrap(),
        "outside change\n"
    );
    select_folder(&mut app, "apps/portal/src/feature");
    key(&mut app, KeyCode::Char('u'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(
        fixture
            .action(&["diff", "--cached", "--name-only"])
            .text()
            .is_empty()
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("apps/portal/src/feature/a.ts")).unwrap(),
        "new a\n"
    );
    render_views(&app);
}

#[test]
fn explorer_refresh_keeps_collapsed_folders_and_file_actions_cannot_target_folders() {
    let fixture = Fixture::new();
    fixture.write("src/nested/a", "a\n");
    fixture.commit("src/nested/b", "b\n", "base");
    fixture.write("src/nested/a", "changed\n");
    fixture.write("src/nested/b", "changed\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    select_folder(&mut app, "src/nested");
    key(&mut app, KeyCode::Enter);
    assert!(app.explorer.collapsed.contains(Path::new("src/nested")));
    for command in [
        app::CommandId::Discard,
        app::CommandId::Edit,
        app::CommandId::Hunks,
        app::CommandId::TakeCurrent,
    ] {
        assert!(!command.available(&app));
    }
    key(&mut app, KeyCode::Char('e'));
    assert!(app.modal.is_none());
    key(&mut app, KeyCode::Char('r'));
    wait(&mut app);
    assert!(app.explorer.collapsed.contains(Path::new("src/nested")));
    assert!(!app.file_selected());
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right);
    wait(&mut app);
    assert!(app.file_selected());
    assert_eq!(app.explorer.selected().unwrap().file, Some(app.selected));
    assert!(app.preview.contains("+changed"));
    render_views(&app);
}

#[test]
fn folder_unstage_handles_unborn_head_and_untracked_siblings() {
    let fixture = Fixture::new();
    fixture.write("src/a", "a\n");
    fixture.write("src/b", "b\n");
    fixture.success(&["add", "src/a"]);
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    select_folder(&mut app, "src");
    key(&mut app, KeyCode::Char('u'));
    key(&mut app, KeyCode::Char('y'));
    wait(&mut app);
    assert!(fixture.action(&["ls-files"]).text().is_empty());
    assert!(fixture.repo.root.join("src/a").exists() && fixture.repo.root.join("src/b").exists());
}

#[test]
fn folder_staging_refuses_unresolved_conflicts() {
    let fixture = Fixture::new();
    fixture.commit("src/file", "base\n", "base");
    fixture.success(&["switch", "-c", "side"]);
    fixture.commit("src/file", "side\n", "side");
    fixture.success(&["switch", "main"]);
    fixture.commit("src/file", "main\n", "main");
    assert!(!fixture.action(&["merge", "side"]).success);
    let before = fixture.repo.guard().unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    select_folder(&mut app, "src");
    key(&mut app, KeyCode::Char('s'));
    assert!(
        app.modal.is_none()
            && app
                .error
                .as_ref()
                .is_some_and(|error| error.contains("individually"))
    );
    fixture.repo.verify(&before).unwrap();
}

fn open_theme_picker(app: &mut app::App) {
    key(app, KeyCode::Char(':'));
    app.handle_paste("Change theme");
    key(app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(app::Modal::Theme { .. })));
}

#[test]
fn theme_picker_previews_cancels_and_persists_without_changing_git() {
    let fixture = Fixture::new();
    fixture.base();
    let before = fixture.repo.guard().unwrap();
    let root = tempfile::tempdir().unwrap();
    config::Config::initialize(root.path()).unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    app.config_root = Some(root.path().into());
    app.config = config::Config::default();
    app.theme = app.config.resolve(root.path(), &app.config.theme).unwrap();
    let original = app.theme.accent;
    assert_eq!(app.theme.background, ratatui::style::Color::Reset);
    open_theme_picker(&mut app);
    key(&mut app, KeyCode::Down);
    assert_ne!(app.theme.accent, original);
    assert_eq!(app.theme.background, ratatui::style::Color::Reset);
    render_views(&app);
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.theme.accent, original);
    assert_eq!(app.theme.background, ratatui::style::Color::Reset);
    assert_eq!(
        config::Config::load(root.path()).unwrap().theme,
        "catppuccin-mocha"
    );
    open_theme_picker(&mut app);
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Enter);
    assert!(app.modal.is_none());
    assert_eq!(
        config::Config::load(root.path()).unwrap().theme,
        app.config.theme
    );
    assert_ne!(app.theme.accent, original);
    assert_eq!(app.theme.background, ratatui::style::Color::Reset);
    fixture.repo.verify(&before).unwrap();
}

#[test]
fn theme_picker_rejects_broken_custom_themes_and_does_not_overwrite_config() {
    let fixture = Fixture::new();
    fixture.base();
    let root = tempfile::tempdir().unwrap();
    config::Config::initialize(root.path()).unwrap();
    fs::write(
        root.path().join("themes/zz-broken.toml"),
        "added = 'NOT A COLOR'",
    )
    .unwrap();
    let original = fs::read(root.path().join("config.toml")).unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    app.config_root = Some(root.path().into());
    app.config = config::Config::default();
    open_theme_picker(&mut app);
    for _ in 0..10 {
        key(&mut app, KeyCode::Down);
    }
    assert!(
        app.error
            .as_ref()
            .is_some_and(|error| error.contains("Invalid theme"))
    );
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(app::Modal::Theme { .. })));
    assert_eq!(fs::read(root.path().join("config.toml")).unwrap(), original);
    key(&mut app, KeyCode::Esc);
}

#[test]
fn terminal_background_survives_theme_changes_and_dialog_rendering() {
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    let fixture = Fixture::new();
    fixture.base();
    let root = tempfile::tempdir().unwrap();
    config::Config::initialize(root.path()).unwrap();
    let config = config::Config::load(root.path()).unwrap();
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    for name in theme::PRESETS {
        app.theme = config.resolve(root.path(), name).unwrap();
        for modal in [
            None,
            Some(app::Modal::Help { scroll: 0 }),
            Some(app::Modal::Palette {
                filter: String::new(),
                selected: 0,
            }),
            Some(app::Modal::Output {
                title: "test".into(),
                text: "+added\n-removed".into(),
                scroll: 0,
                horizontal: 0,
            }),
            Some(app::Modal::Theme {
                names: vec![name.to_string()],
                selected: 0,
                original: app.theme.clone(),
            }),
        ] {
            app.modal = modal;
            let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
            terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer[(139, 39)].bg,
                Color::Reset,
                "{name}: footer background"
            );
            assert_eq!(
                buffer[(139, 20)].bg,
                Color::Reset,
                "{name}: pane/dialog background"
            );
            assert_eq!(
                buffer[(139, 0)].bg,
                Color::Reset,
                "{name}: header background"
            );
        }
    }
}

#[test]
fn folder_unstage_includes_rename_sources_outside_the_folder() {
    let fixture = Fixture::new();
    fixture.commit("old", "rename me\n", "base");
    fs::create_dir(fixture.repo.root.join("target-dir")).unwrap();
    fixture.success(&["mv", "old", "target-dir/new"]);
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    select_folder(&mut app, "target-dir");
    key(&mut app, KeyCode::Char('u'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    assert!(
        fixture
            .action(&["diff", "--cached", "--name-only"])
            .text()
            .is_empty()
    );
    assert!(!fixture.repo.root.join("old").exists());
    assert_eq!(
        fs::read_to_string(fixture.repo.root.join("target-dir/new")).unwrap(),
        "rename me\n"
    );
}

#[test]
fn folder_staging_literal_names_and_partial_staging_are_immediate() {
    let fixture = Fixture::new();
    fixture.write("[folder]/a", "base\n");
    fixture.commit("folder/a", "base\n", "base");
    fixture.write("[folder]/a", "staged\n");
    fixture.success(&["add", "--", "[folder]/a"]);
    fixture.write("[folder]/a", "working\n");
    fixture.write("folder/a", "unrelated\n");
    let mut app = app::App::ready(&fixture.repo.root).unwrap();
    select_folder(&mut app, "[folder]");
    key(&mut app, KeyCode::Char('s'));
    assert!(app.modal.is_none() && app.busy.is_some());
    wait(&mut app);
    let staged = fixture.action(&["diff", "--cached"]).text();
    assert!(staged.contains("+working") && !staged.contains("unrelated"));
}

#[cfg(unix)]
#[test]
fn quick_installer_selects_platform_verifies_checksums_and_refuses_silent_overwrite() {
    use std::os::unix::fs::PermissionsExt;
    for (os, arch, target) in [
        ("Darwin", "arm64", "aarch64-apple-darwin"),
        ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
        ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mock = root.join("mock");
        fs::create_dir(&mock).unwrap();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("kae"), "verified test payload\n").unwrap();
        let archive = root.join("archive.tar.gz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&package)
                .arg("kae")
                .status()
                .unwrap()
                .success()
        );
        let digest = Command::new("sh").arg("-c").arg("if command -v sha256sum >/dev/null 2>&1; then sha256sum \"$1\"; else shasum -a 256 \"$1\"; fi").arg("checksum").arg(&archive).output().unwrap();
        assert!(digest.status.success());
        let hash = String::from_utf8(digest.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        fs::write(
            root.join("checksum"),
            format!("{hash}  kae-{target}.tar.gz\n"),
        )
        .unwrap();
        fs::write(mock.join("uname"), "#!/bin/sh\ncase $1 in -s) echo \"$TEST_OS\";; -m) echo \"$TEST_ARCH\";; *) exit 1;; esac\n").unwrap();
        fs::write(mock.join("curl"), "#!/bin/sh\nset -eu\nurl=\noutput=\nwhile [ \"$#\" -gt 0 ]; do\ncase $1 in -o) output=$2; shift 2;; https://*) url=$1; shift;; *) shift;; esac\ndone\nprintf '%s\\n' \"$url\" >> \"$TEST_ROOT/urls\"\ncase $url in *.sha256) cp \"$TEST_ROOT/checksum\" \"$output\";; *.tar.gz) cp \"$TEST_ROOT/archive.tar.gz\" \"$output\";; *) exit 1;; esac\n").unwrap();
        for name in ["uname", "curl"] {
            fs::set_permissions(mock.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let temp = root.join("tmp");
        fs::create_dir(&temp).unwrap();
        let install = root.join("install with spaces");
        let run = || {
            Command::new("sh")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install.sh"))
                .arg("v0.1.0")
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        mock.display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .env("TEST_ROOT", root)
                .env("TEST_OS", os)
                .env("TEST_ARCH", arch)
                .env("KAE_INSTALL_DIR", &install)
                .env("KAE_NONINTERACTIVE", "1")
                .env("TMPDIR", &temp)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let output = run();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(install.join("kae")).unwrap(),
            "verified test payload\n"
        );
        assert!(
            fs::read_to_string(root.join("urls"))
                .unwrap()
                .contains(&format!("/download/v0.1.0/kae-{target}.tar.gz"))
        );
        assert_eq!(
            fs::read_dir(&temp).unwrap().count(),
            0,
            "installer leaked temporary files"
        );
        fs::write(install.join("kae"), "keep existing binary\n").unwrap();
        assert!(!run().status.success());
        assert_eq!(
            fs::read_to_string(install.join("kae")).unwrap(),
            "keep existing binary\n"
        );
        fs::remove_file(install.join("kae")).unwrap();
        fs::write(
            root.join("checksum"),
            format!("{}  kae-{target}.tar.gz\n", "0".repeat(64)),
        )
        .unwrap();
        let output = run();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Checksum mismatch"));
        assert!(!install.join("kae").exists());
        assert_eq!(fs::read_dir(&temp).unwrap().count(), 0);
    }
}
