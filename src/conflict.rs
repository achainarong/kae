use std::{fs, io::Write, path::PathBuf};

use anyhow::{Context, Result, bail, ensure};
use tempfile::NamedTempFile;

use crate::git::{Guard, PREVIEW_LIMIT, Repo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Current,
    Incoming,
    CurrentThenIncoming,
    IncomingThenCurrent,
}

#[derive(Clone, Debug)]
pub struct Region {
    pub current: String,
    pub base: Option<String>,
    pub incoming: String,
    original: String,
    pub choice: Option<Choice>,
    current_no_eol: bool,
    incoming_no_eol: bool,
}

impl Region {
    fn resolved(&self) -> String {
        let side = |text: &str, no_eol: bool| {
            if no_eol {
                text.strip_suffix("\r\n")
                    .or_else(|| text.strip_suffix('\n'))
                    .unwrap_or(text)
                    .to_owned()
            } else {
                text.to_owned()
            }
        };
        let current = side(&self.current, self.current_no_eol);
        let incoming = side(&self.incoming, self.incoming_no_eol);
        let join = |mut first: String, second: String, crlf: bool| {
            if !first.is_empty() && !second.is_empty() && !first.ends_with('\n') {
                first.push_str(if crlf { "\r\n" } else { "\n" });
            }
            first.push_str(&second);
            first
        };
        match self.choice {
            None => self.original.clone(),
            Some(Choice::Current) => current,
            Some(Choice::Incoming) => incoming,
            Some(Choice::CurrentThenIncoming) => {
                join(current, incoming, self.current.ends_with("\r\n"))
            }
            Some(Choice::IncomingThenCurrent) => {
                join(incoming, current, self.incoming.ends_with("\r\n"))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Segment {
    Text(String),
    Conflict(Region),
}

#[derive(Clone, Debug)]
pub struct Conflict {
    pub path: PathBuf,
    pub current_label: String,
    pub incoming_label: String,
    pub segments: Vec<Segment>,
    pub selected: usize,
    pub scroll: u16,
    pub horizontal: u16,
    pub show_base: bool,
    pub base_file: Option<String>,
    pub confirm_close: bool,
    pub guard: Guard,
    pub current_file: String,
    pub incoming_file: String,
    pub parse_error: Option<String>,
    source_no_eol: (bool, bool),
    loaded: Vec<u8>,
    undo: Vec<Undo>,
}

#[derive(Clone, Debug)]
enum Undo {
    Choice {
        region: usize,
        previous: Option<Choice>,
    },
    Edit {
        segments: Vec<Segment>,
        selected: usize,
        parse_error: Option<String>,
    },
}

impl Conflict {
    pub fn load(repo: &Repo, path: PathBuf, guard: Guard, branch: &str) -> Result<Self> {
        repo.verify(&guard)?;
        let file = repo.safe_file(&path)?;
        let metadata = file.metadata().context(
            "File is deleted. Choose a whole-file side or delete it using command search.",
        )?;
        ensure!(
            metadata.is_file() && metadata.len() <= PREVIEW_LIMIT as u64,
            "Use whole-file side choices or an external editor for non-regular / large files."
        );
        let bytes = fs::read(&file)?;
        ensure!(
            !bytes.contains(&0),
            "Binary conflict: use whole-file current / incoming choices in command search."
        );
        let text = std::str::from_utf8(&bytes)
            .context("Non-UTF-8 conflict: use whole-file choices or an external editor.")?;
        let mut segments = parse(text)?;
        let unmerged = repo.checked(
            &[
                "ls-files".into(),
                "--unmerged".into(),
                "-z".into(),
                "--".into(),
                path.as_os_str().to_owned(),
            ],
            None,
        )?;
        let mut base_file = None;
        let mut current_no_eol = false;
        let mut incoming_no_eol = false;
        let mut current_file = String::new();
        let mut incoming_file = String::new();
        for record in unmerged.split(|byte| *byte == 0) {
            let metadata = record
                .split(|byte| *byte == b'\t')
                .next()
                .unwrap_or_default();
            let metadata = String::from_utf8_lossy(metadata);
            let fields: Vec<_> = metadata.split_whitespace().collect();
            if fields.len() == 3 && fields[2] == "1" {
                base_file = repo
                    .bytes(&["cat-file", "blob", fields[1]])
                    .ok()
                    .map(|bytes| crate::git::preview_text(&bytes));
            }
            if fields.len() == 3 && matches!(fields[2], "2" | "3") {
                ensure!(
                    matches!(fields[0], "100644" | "100755"),
                    "File-type conflict: use whole-file side choices instead of text-region resolution."
                );
                let bytes = repo.bytes(&["cat-file", "blob", fields[1]])?;
                let no_eol = !bytes.is_empty() && !bytes.ends_with(b"\n");
                if fields[2] == "2" {
                    current_no_eol = no_eol;
                    current_file = crate::git::preview_text(&bytes);
                } else {
                    incoming_no_eol = no_eol;
                    incoming_file = crate::git::preview_text(&bytes);
                }
            }
        }
        // Conflict markers introduce a separator newline even when an EOF
        // variant had none. Restore the chosen side's original EOF semantics.
        if let Some(Segment::Conflict(region)) = segments.last_mut() {
            region.current_no_eol = current_no_eol;
            region.incoming_no_eol = incoming_no_eol;
        }
        let replay = repo.operation()?.is_some_and(|operation| {
            matches!(
                operation,
                crate::git::Operation::Rebase | crate::git::Operation::CherryPick
            )
        });
        let incoming = if replay {
            repo.text(&["show", "-s", "--format=%h %s", "REBASE_HEAD", "--"])
                .or_else(|_| repo.text(&["show", "-s", "--format=%h %s", "CHERRY_PICK_HEAD", "--"]))
                .unwrap_or_else(|_| "replayed change".into())
        } else {
            repo.text(&["show", "-s", "--format=%h %s", "MERGE_HEAD", "--"])
                .unwrap_or_else(|_| "incoming change / stash".into())
        };
        let count = segments
            .iter()
            .filter(|segment| matches!(segment, Segment::Conflict(_)))
            .count();
        ensure!(
            count > 0,
            "No standard conflict markers found. Edit manually, or use a whole-file choice, then explicitly stage the resolved file."
        );
        Ok(Self {
            path,
            current_label: if replay {
                format!("Current tree / onto · {branch}")
            } else {
                format!("Current · {branch}")
            },
            incoming_label: format!("Incoming · {incoming}"),
            segments,
            selected: 0,
            scroll: 0,
            horizontal: 0,
            show_base: false,
            base_file,
            confirm_close: false,
            guard,
            current_file,
            incoming_file,
            parse_error: None,
            source_no_eol: (current_no_eol, incoming_no_eol),
            loaded: bytes,
            undo: Vec::new(),
        })
    }

    pub fn regions(&self) -> Vec<&Region> {
        self.segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Conflict(region) => Some(region),
                _ => None,
            })
            .collect()
    }

    pub fn unresolved(&self) -> usize {
        if self.parse_error.is_some() {
            return 1;
        }
        self.regions()
            .iter()
            .filter(|region| region.choice.is_none())
            .count()
    }

    pub fn dirty(&self) -> bool {
        self.result().as_bytes() != self.loaded
    }

    pub fn choose(&mut self, choice: Choice) {
        let Some(region) = self.regions().get(self.selected).copied() else {
            return;
        };
        if self
            .regions()
            .get(self.selected)
            .is_some_and(|region| region.choice == Some(choice))
        {
            return;
        }
        self.undo.push(Undo::Choice {
            region: self.selected,
            previous: region.choice,
        });
        self.trim_undo();
        let mut index = 0;
        for segment in &mut self.segments {
            if let Segment::Conflict(region) = segment {
                if index == self.selected {
                    region.choice = Some(choice);
                    break;
                }
                index += 1;
            }
        }
    }

    pub fn undo(&mut self) {
        match self.undo.pop() {
            Some(Undo::Choice { region, previous }) => {
                if let Some(Segment::Conflict(target)) = self
                    .segments
                    .iter_mut()
                    .filter(|segment| matches!(segment, Segment::Conflict(_)))
                    .nth(region)
                {
                    target.choice = previous;
                }
            }
            Some(Undo::Edit {
                segments,
                selected,
                parse_error,
            }) => {
                self.segments = segments;
                self.selected = selected;
                self.parse_error = parse_error;
            }
            None => {}
        }
    }

    fn trim_undo(&mut self) {
        while self.undo.len() > 256
            || self
                .undo
                .iter()
                .filter(|undo| matches!(undo, Undo::Edit { .. }))
                .count()
                > 16
        {
            self.undo.remove(0);
        }
    }

    pub fn edit_result(&mut self, text: String) -> Result<()> {
        ensure!(
            text.len() <= PREVIEW_LIMIT,
            "Result exceeds the 256 KiB editing limit."
        );
        if text == self.result() {
            return Ok(());
        }
        self.undo.push(Undo::Edit {
            segments: self.segments.clone(),
            selected: self.selected,
            parse_error: self.parse_error.clone(),
        });
        self.trim_undo();
        match parse(&text) {
            Ok(mut segments) => {
                if let Some(Segment::Conflict(region)) = segments.last_mut() {
                    region.current_no_eol = self.source_no_eol.0;
                    region.incoming_no_eol = self.source_no_eol.1;
                }
                self.segments = segments;
                self.parse_error = None;
            }
            Err(error) => {
                self.segments = vec![Segment::Text(text)];
                self.parse_error = Some(error.to_string());
            }
        }
        self.selected = self.selected.min(self.regions().len().saturating_sub(1));
        Ok(())
    }

    pub fn result(&self) -> String {
        let mut result = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => result.push_str(text),
                Segment::Conflict(region) => result.push_str(&region.resolved()),
            }
        }
        result
    }

    pub fn result_offset(&self) -> u16 {
        let mut index = 0;
        let mut line = 0usize;
        for segment in &self.segments {
            let text = match segment {
                Segment::Text(text) => text.clone(),
                Segment::Conflict(region) => {
                    if index == self.selected {
                        break;
                    }
                    index += 1;
                    region.resolved()
                }
            };
            line += text.bytes().filter(|byte| *byte == b'\n').count();
        }
        line.min(u16::MAX as usize) as u16
    }

    pub fn save(&mut self, repo: &Repo) -> Result<()> {
        repo.verify(&self.guard)?;
        let path = repo.safe_file(&self.path)?;
        ensure!(
            fs::read(&path)? == self.loaded,
            "File changed outside Kae. Close and reopen the conflict to avoid overwriting it."
        );
        let permissions = path.metadata()?.permissions();
        let mut temporary = NamedTempFile::new_in(path.parent().context("File has no parent")?)?;
        temporary.as_file().set_permissions(permissions)?;
        let result = self.result();
        temporary.write_all(result.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
        self.loaded = result.into_bytes();
        self.guard = repo.guard()?;
        Ok(())
    }
}

fn marker(line: &str, symbol: char, size: usize) -> bool {
    let line = line.trim_end_matches(['\r', '\n']);
    let count = line
        .chars()
        .take_while(|character| *character == symbol)
        .count();
    count == size && (line.len() == size || line.as_bytes().get(size) == Some(&b' '))
}

pub fn contains_markers(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes).lines().any(|line| {
        let size = line
            .chars()
            .take_while(|character| *character == '<')
            .count();
        size >= 7 && marker(line, '<', size)
    })
}

pub fn parse(text: &str) -> Result<Vec<Segment>> {
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let mut segments = Vec::new();
    let mut plain = String::new();
    let mut index = 0;
    while index < lines.len() {
        let size = lines[index]
            .chars()
            .take_while(|character| *character == '<')
            .count();
        if size < 7 || !marker(lines[index], '<', size) {
            plain.push_str(lines[index]);
            index += 1;
            continue;
        }
        if !plain.is_empty() {
            segments.push(Segment::Text(std::mem::take(&mut plain)));
        }
        let start = index;
        index += 1;
        let mut current = String::new();
        let mut base = None;
        let mut incoming = String::new();
        let mut phase = 0;
        let mut closed = false;
        while index < lines.len() {
            let line = lines[index];
            if marker(line, '<', size) {
                bail!("Nested conflict markers: resolve manually in your editor.");
            }
            if phase == 0 && marker(line, '|', size) {
                phase = 1;
                base = Some(String::new());
            } else if phase <= 1 && marker(line, '=', size) {
                phase = 2;
            } else if phase == 2 && marker(line, '>', size) {
                index += 1;
                closed = true;
                break;
            } else {
                match phase {
                    0 => current.push_str(line),
                    1 => base
                        .as_mut()
                        .context("Missing conflict base")?
                        .push_str(line),
                    _ => incoming.push_str(line),
                }
            }
            index += 1;
        }
        ensure!(
            closed,
            "Incomplete conflict markers: resolve manually in your editor."
        );
        segments.push(Segment::Conflict(Region {
            current,
            base,
            incoming,
            original: lines[start..index].concat(),
            choice: None,
            current_no_eol: false,
            incoming_no_eol: false,
        }));
    }
    if !plain.is_empty() {
        segments.push(Segment::Text(plain));
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_diff3_crlf_and_custom_marker_sizes_without_losing_bytes() {
        let text = "prefix\r\n<<<<<<<< current\r\nleft\r\n|||||||| base\r\nbase\r\n========\r\nright\r\n>>>>>>>> incoming\r\nsuffix";
        let segments = parse(text).unwrap();
        let Segment::Conflict(region) = &segments[1] else {
            panic!()
        };
        assert_eq!(region.current, "left\r\n");
        assert_eq!(region.base.as_deref(), Some("base\r\n"));
        assert_eq!(region.incoming, "right\r\n");
        assert_eq!(
            segments
                .iter()
                .map(|segment| match segment {
                    Segment::Text(text) => text.as_str(),
                    Segment::Conflict(region) => &region.original,
                })
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn rejects_malformed_markers() {
        assert!(parse("<<<<<<< left\na\n=======\nb\n").is_err());
        assert!(contains_markers(b"<<<<<<< left\n"));
        assert!(!contains_markers(b"<<<<<<<<literal\n"));
    }
}
