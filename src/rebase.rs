use std::{
    collections::HashSet,
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::git::{GitOutput, Guard, Repo, Snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RebaseAction {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
    Edit,
}

impl RebaseAction {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
            Self::Edit => "edit",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub oid: String,
    pub subject: String,
    pub message: String,
    pub action: RebaseAction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub base: String,
    pub head: String,
    pub steps: Vec<Step>,
    pub published: bool,
    pub selected: usize,
    #[serde(skip)]
    pub guard: Option<Guard>,
}

impl Plan {
    pub fn load(repo: &Repo, snapshot: &Snapshot, base: &str) -> Result<Self> {
        repo.verify(&snapshot.guard)?;
        ensure!(
            snapshot.operation.is_none() && snapshot.files.is_empty(),
            "Rebase needs a clean tree and no operation in progress."
        );
        ensure!(
            !snapshot.branch.starts_with("detached @"),
            "Switch to a branch before starting an interactive rebase."
        );
        let base = repo.text(&["rev-parse", "--verify", &format!("{base}^{{commit}}")])?;
        repo.bytes(&["merge-base", "--is-ancestor", &base, "HEAD"])
            .context("Select an ancestor of HEAD as the rebase base.")?;
        let range = format!("{base}..HEAD");
        ensure!(
            repo.text(&["rev-list", "--min-parents=2", &range, "--"])?
                .is_empty(),
            "This range contains merge commits. Preserving merge topology is outside the MVP; choose a linear range or use git rebase --rebase-merges."
        );
        let oids = repo.text(&["rev-list", "--reverse", &range, "--"])?;
        let remote_oids = repo.text(&["rev-list", "--remotes", "--not", &base, "--"])?;
        let remote_oids: HashSet<_> = remote_oids.lines().collect();
        let mut published = false;
        let mut steps = Vec::new();
        ensure!(
            oids.lines().count() <= 300,
            "Limit the rebase to at most 300 commits."
        );
        for oid in oids.lines() {
            published |= remote_oids.contains(oid);
            let message = repo.text(&["show", "-s", "--format=%B", oid, "--"])?;
            steps.push(Step {
                oid: oid.into(),
                subject: message.lines().next().unwrap_or_default().into(),
                message,
                action: RebaseAction::Pick,
            });
        }
        ensure!(
            !steps.is_empty(),
            "There are no commits after the selected base. Select an older ancestor."
        );
        Ok(Self {
            base,
            head: snapshot.head.clone().context("No HEAD commit")?,
            steps,
            published,
            selected: 0,
            guard: Some(snapshot.guard.clone()),
        })
    }

    pub fn validate(&self) -> Result<()> {
        let mut previous = false;
        for step in &self.steps {
            ensure!(
                !matches!(step.action, RebaseAction::Squash | RebaseAction::Fixup) || previous,
                "Squash / fixup needs a preceding non-dropped commit."
            );
            ensure!(
                !step.message.trim().is_empty(),
                "Commit messages cannot be empty."
            );
            ensure!(
                step.oid.len() >= 40
                    && step
                        .oid
                        .chars()
                        .all(|character| character.is_ascii_hexdigit()),
                "Invalid commit ID in rebase plan."
            );
            if step.action != RebaseAction::Drop {
                previous = true;
            }
        }
        ensure!(!self.steps.is_empty(), "The rebase plan is empty.");
        Ok(())
    }

    pub fn todo(&self) -> Result<String> {
        self.validate()?;
        Ok(self
            .steps
            .iter()
            .map(|step| format!("{} {}\n", step.action.name(), step.oid))
            .collect())
    }

    pub fn execute(&self, repo: &Repo) -> Result<GitOutput> {
        self.validate()?;
        repo.verify(
            self.guard
                .as_ref()
                .context("Reopen the rebase editor before starting")?,
        )?;
        ensure!(
            repo.head()?.as_deref() == Some(&self.head),
            "HEAD changed. Reopen the rebase editor."
        );
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let backup = format!("refs/kae/backup/{nonce}");
        repo.bytes(&["update-ref", &backup, &self.head])?;
        let path = repo.git_path("kae-rebase-plan.json")?;
        ensure!(
            !path.is_symlink(),
            "Refusing to overwrite a symlinked Kae plan file."
        );
        fs::write(&path, serde_json::to_vec(self)?)?;
        let mut command = repo.command();
        command.args([
            "rebase",
            "--interactive",
            "--no-autostash",
            "--no-autosquash",
            "--no-update-refs",
            &self.base,
        ]);
        crate::editor::configure_git_editor(repo, &mut command)?;
        let mut output = repo.run_command(&mut command, None)?;
        output
            .stderr
            .extend_from_slice(format!("\nOriginal HEAD preserved at {backup}\n").as_bytes());
        if repo.operation()? != Some(crate::git::Operation::Rebase)
            && let Err(error) = crate::editor::cleanup_plan(repo)
        {
            output.stderr.extend_from_slice(
                format!("\nCould not remove completed Kae rebase plan: {error}\n").as_bytes(),
            );
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_squash_order_and_uses_only_oids_in_todo() {
        let mut plan = Plan {
            base: "a".repeat(40),
            head: "b".repeat(40),
            steps: vec![Step {
                oid: "c".repeat(40),
                subject: "exec malicious command".into(),
                message: "message".into(),
                action: RebaseAction::Squash,
            }],
            published: false,
            selected: 0,
            guard: None,
        };
        assert!(plan.validate().is_err());
        plan.steps[0].action = RebaseAction::Reword;
        assert_eq!(plan.todo().unwrap(), format!("reword {}\n", "c".repeat(40)));
    }
}
