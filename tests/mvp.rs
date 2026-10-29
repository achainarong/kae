#![allow(dead_code)]

// The binary's modules are exercised against disposable real repositories.
// Keeping the CLI as a single binary avoids publishing a premature library API.
#[path = "../src/app.rs"]
mod app;
#[path = "../src/conflict.rs"]
mod conflict;
#[path = "../src/editor.rs"]
mod editor;
#[path = "../src/git.rs"]
mod git;
#[path = "../src/graph.rs"]
mod graph;
#[path = "../src/loader.rs"]
mod loader;
#[path = "../src/rebase.rs"]
mod rebase;
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
    assert!(matches!(app.modal, Some(app::Modal::Confirm { .. })));
    key(&mut app, KeyCode::Char('y'));
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
