mod app;
mod conflict;
mod editor;
mod git;
mod graph;
mod history;
mod loader;
mod rebase;
mod ui;

use std::{io::IsTerminal, path::PathBuf};

use anyhow::{Result, bail};
use clap::Parser;

use app::App;

/// A merge-first Git terminal workspace.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Repository directory (a subdirectory or linked worktree also works).
    #[arg(short = 'C', long, default_value = ".")]
    path: PathBuf,

    #[arg(long, hide = true, conflicts_with = "message_editor")]
    sequence_editor: Option<PathBuf>,

    #[arg(long, hide = true)]
    message_editor: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(path) = cli.sequence_editor {
        return editor::sequence_editor(&path);
    }
    if let Some(path) = cli.message_editor {
        return editor::message_editor(&path);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("Kae needs an interactive terminal. Run `kae --help` for usage.");
    }

    // Repository discovery and reads happen in the loader after the first frame.
    let mut app = App::new(&cli.path)?;
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    // ratatui also installs a panic hook that restores the terminal.
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_repository_path() {
        let cli = Cli::try_parse_from(["kae", "-C", "/some/repository"]).unwrap();
        assert_eq!(cli.path, PathBuf::from("/some/repository"));
    }

    #[test]
    fn cli_rejects_unknown_flags() {
        assert!(Cli::try_parse_from(["kae", "--force"]).is_err());
    }
}
