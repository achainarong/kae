use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};

use crate::{
    git::Repo,
    rebase::{Plan, RebaseAction},
};

pub fn configure_git_editor(repo: &Repo, command: &mut Command) -> Result<()> {
    let executable = std::env::current_exe()?;
    // Integration tests run under a test harness; Git must launch the real CLI.
    #[cfg(test)]
    let executable = option_env!("CARGO_BIN_EXE_kae")
        .map(std::path::PathBuf::from)
        .unwrap_or(executable);
    let executable = executable
        .to_str()
        .context("Kae executable path must be UTF-8 for Git editor integration")?;
    let quoted = shell_words::quote(executable);
    command
        .env("GIT_SEQUENCE_EDITOR", format!("{quoted} --sequence-editor"))
        .env("GIT_EDITOR", format!("{quoted} --message-editor"))
        .env("KAE_REBASE_PLAN", repo.git_path("kae-rebase-plan.json")?);
    Ok(())
}

/// Invoked by Git in a separate Kae process, before any terminal setup.
pub fn sequence_editor(target: &Path) -> Result<()> {
    let plan: Plan = serde_json::from_slice(&fs::read(
        std::env::var_os("KAE_REBASE_PLAN").context("Missing Kae rebase plan")?,
    )?)?;
    fs::write(target, plan.todo()?)?;
    Ok(())
}

pub fn message_editor(target: &Path) -> Result<()> {
    // Non-reword operations keep Git's generated message. Reword uses the
    // original object ID in Git's completed todo row, never a duplicated subject.
    let Ok(path) = std::env::var("KAE_REBASE_PLAN") else {
        return Ok(());
    };
    let Ok(bytes) = fs::read(path) else {
        return Ok(());
    };
    let repo = Repo::discover(Path::new("."))?;
    if repo.operation()? != Some(crate::git::Operation::Rebase) {
        return Ok(());
    }
    let plan: Plan = serde_json::from_slice(&bytes)?;
    let done = fs::read_to_string(repo.git_path("rebase-merge/done")?).unwrap_or_default();
    let command = done
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'));
    let mut fields = command.unwrap_or_default().split_whitespace();
    let action = fields.next();
    let oid = fields.next();
    if action == Some("reword")
        && let Some(step) = plan
            .steps
            .iter()
            .find(|step| Some(step.oid.as_str()) == oid && step.action == RebaseAction::Reword)
    {
        fs::write(target, format!("{}\n", step.message.trim_end()))?;
    }
    Ok(())
}

pub fn cleanup_plan(repo: &Repo) -> Result<()> {
    let path = repo.git_path("kae-rebase-plan.json")?;
    if path.try_exists()? && !path.is_symlink() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn open(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let words = shell_words::split(&editor).context("Could not parse VISUAL / EDITOR. Use an executable followed by arguments, not shell expressions.")?;
    let (program, args) = words.split_first().context("VISUAL / EDITOR is empty")?;
    let status = Command::new(program)
        .args(args)
        .arg(path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("Could not start editor {program}. Set VISUAL or EDITOR."))?;
    ensure!(status.success(), "Editor exited unsuccessfully ({status})");
    Ok(())
}
