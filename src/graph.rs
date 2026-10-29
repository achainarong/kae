use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Commit {
    pub oid: String,
    pub short: String,
    pub refs: String,
    pub subject: String,
    pub author: String,
    pub age: String,
    pub graph: String,
    pub connectors: Vec<String>,
}

/// Git owns graph lane calculation; retain its connector rows without making
/// them separate selectable commits.
pub fn parse(bytes: &[u8]) -> Result<Vec<Commit>> {
    let text = String::from_utf8_lossy(bytes);
    let mut commits: Vec<Commit> = Vec::new();
    for line in text.lines() {
        if let Some((graph, fields)) = line.split_once('\u{1e}') {
            let fields: Vec<_> = fields.split('\0').collect();
            anyhow::ensure!(fields.len() == 6, "Malformed Git history record");
            commits.push(Commit {
                oid: fields[0].to_owned(),
                short: fields[1].to_owned(),
                refs: fields[2].to_owned(),
                subject: fields[3].to_owned(),
                author: fields[4].to_owned(),
                age: fields[5].to_owned(),
                graph: graph.to_owned(),
                connectors: Vec::new(),
            });
        } else if !line.is_empty() {
            commits
                .last_mut()
                .context("Graph connector without a commit")?
                .connectors
                .push(line.to_owned());
        }
    }
    Ok(commits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_merge_lanes_and_connectors() {
        let graph = parse(b"*   \x1eabc\0abc\0 (main)\0merge\0A\0now\n|\\\n| * \x1edef\0def\0\0change\0B\0yesterday\n").unwrap();
        assert_eq!(graph.len(), 2);
        assert_eq!(graph[0].connectors, ["|\\"]);
        assert_eq!(graph[1].subject, "change");
    }
}
