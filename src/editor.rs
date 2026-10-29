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

/// Textarea owns cursor movement, Unicode editing and per-keystroke undo.
/// Keep line endings outside the widget, whose lines contain no terminators.
pub struct ResultEditor {
    pub area: ratatui_textarea::TextArea<'static>,
    eol: &'static str,
}

impl ResultEditor {
    pub fn new(text: &str) -> Result<Self> {
        ensure!(
            safe_editor_text(text),
            "This file contains terminal control characters. Use the external editor (E) instead."
        );
        let crlf = text.matches("\r\n").count();
        let lf = text.bytes().filter(|byte| *byte == b'\n').count();
        ensure!(
            (crlf == 0 || crlf == lf) && !text.replace("\r\n", "").contains('\r'),
            "Mixed line endings require the external editor (E) to avoid normalizing your file."
        );
        let eol = if crlf > 0 { "\r\n" } else { "\n" };
        let lines = text.split(eol).map(str::to_owned).collect();
        let area = ratatui_textarea::TextArea::new(lines);
        let mut editor = Self { area, eol };
        editor.set_theme(&crate::theme::Theme::default());
        Ok(editor)
    }

    pub fn text(&self) -> String {
        self.area.lines().join(self.eol)
    }

    pub fn set_theme(&mut self, theme: &crate::theme::Theme) {
        use ratatui::{style::Style, widgets::Block};
        self.area
            .set_style(Style::default().fg(theme.text).bg(theme.background));
        self.area.set_block(
            Block::bordered()
                .title(" Result editor · Ctrl-S review save · Esc keep draft ")
                .border_style(Style::default().fg(theme.accent)),
        );
        self.area
            .set_cursor_line_style(Style::default().bg(theme.selection));
        self.area
            .set_line_number_style(Style::default().fg(theme.muted));
        self.area
            .set_selection_style(Style::default().bg(theme.selection).fg(theme.text));
        self.area
            .set_cursor_style(Style::default().bg(theme.accent).fg(theme.search_text));
    }

    pub fn paste(&mut self, text: &str) -> Result<()> {
        ensure!(
            safe_editor_text(text),
            "Paste contains unsupported control characters."
        );
        ensure!(
            self.text().len() + text.len() <= crate::git::PREVIEW_LIMIT,
            "Result exceeds the 256 KiB inline editing limit."
        );
        let normalized = text.replace("\r\n", "\n");
        ensure!(
            !normalized.contains('\r'),
            "Paste contains standalone carriage returns; use the external editor to preserve them."
        );
        self.area.insert_str(normalized);
        Ok(())
    }
}

pub fn safe_editor_text(text: &str) -> bool {
    text.chars().all(|character| {
        (!character.is_control() || matches!(character, '\r' | '\n' | '\t'))
            && !matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    })
}

pub struct PasteMode;
impl PasteMode {
    pub fn enable() -> Result<Self> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;
        Ok(Self)
    }
    pub fn suspend(&self) -> Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste)?;
        Ok(())
    }
    pub fn resume(&self) -> Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;
        Ok(())
    }
}
impl Drop for PasteMode {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_eol_and_eof_and_supports_widget_undo() {
        for text in ["", "a", "a\n", "a\r\nb\r\n", "a\r\nb"] {
            let mut editor = ResultEditor::new(text).unwrap();
            assert_eq!(editor.text(), text);
            editor.area.insert_str("inserted");
            editor.area.undo();
            assert_eq!(editor.text(), text);
        }
        assert!(ResultEditor::new("a\r\nb\n").is_err());
        assert!(ResultEditor::new("unsafe\x1b[31m").is_err());
        let mut editor = ResultEditor::new("safe").unwrap();
        assert!(editor.paste("\x1b[31m").is_err());
        assert!(editor.paste("\rhidden").is_err());
        let mut editor = ResultEditor::new("").unwrap();
        editor.paste("a\r\n").unwrap();
        assert_eq!(editor.text(), "a\n");
    }
}
