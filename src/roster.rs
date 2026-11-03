//! `repos.tsv`: which repos live in `repos/`.
//!
//! ```text
//! # comment
//! <name><TAB><gitlab path>[<TAB>gitflow]
//! ```
//! Runs of tabs count as one separator, so columns can be aligned.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::branch::PrefixStyle;
use crate::error::{Error, InputError, WorkspaceError};
use crate::names::{GitlabPath, RepoName};

#[derive(Debug, Clone, Serialize)]
pub struct RepoEntry {
    pub name: RepoName,
    pub gitlab: GitlabPath,
    pub style: PrefixStyle,
}

/// Parsed roster. Names and paths are unique, so a lookup by name is final.
#[derive(Debug)]
pub struct Roster {
    path: PathBuf,
    entries: Vec<RepoEntry>,
}

impl Roster {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(Error::io(path))?;
        Ok(Self::parse(path, &text)?)
    }

    fn parse(path: &Path, text: &str) -> Result<Self, WorkspaceError> {
        let err = |line: usize, message: String| WorkspaceError::Roster {
            path: path.to_owned(),
            line,
            message,
        };
        let mut entries: Vec<RepoEntry> = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line_no = i + 1;
            let line = raw.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            let cols: Vec<&str> = line
                .split('\t')
                .filter(|c| !c.trim().is_empty())
                .map(str::trim)
                .collect();
            let (name, gitlab, style) = match cols.as_slice() {
                [n, p] | [n, p, "conventional"] => (*n, *p, PrefixStyle::Conventional),
                [n, p, "gitflow"] => (*n, *p, PrefixStyle::Gitflow),
                [_, _, other] => return Err(err(line_no, format!("unknown style '{other}'"))),
                _ => {
                    return Err(err(
                        line_no,
                        format!("expected 2 or 3 tab-separated columns, got {}", cols.len()),
                    ));
                }
            };
            let entry = RepoEntry {
                name: name
                    .parse()
                    .map_err(|e: InputError| err(line_no, e.to_string()))?,
                gitlab: gitlab
                    .parse()
                    .map_err(|e: InputError| err(line_no, e.to_string()))?,
                style,
            };
            if entries.iter().any(|e| e.name == entry.name) {
                return Err(err(line_no, format!("duplicate name '{}'", entry.name)));
            }
            if entries.iter().any(|e| e.gitlab == entry.gitlab) {
                return Err(err(line_no, format!("duplicate path '{}'", entry.gitlab)));
            }
            entries.push(entry);
        }
        Ok(Self {
            path: path.to_owned(),
            entries,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &RepoEntry> {
        self.entries.iter()
    }

    pub fn get(&self, name: &RepoName) -> Option<&RepoEntry> {
        self.entries.iter().find(|e| &e.name == name)
    }

    pub fn require(&self, name: &RepoName) -> Result<&RepoEntry, InputError> {
        self.get(name)
            .ok_or_else(|| InputError::UnknownRepo(name.to_string()))
    }

    pub fn by_path(&self, gitlab: &GitlabPath) -> Option<&RepoEntry> {
        self.entries.iter().find(|e| &e.gitlab == gitlab)
    }

    /// Picks a free local name for a new path: the last segment, or
    /// `<parent>-<last>` when that is taken. Mirrors how people name clones.
    pub fn free_name(&self, gitlab: &GitlabPath) -> Option<RepoName> {
        let mut segs = gitlab.as_str().rsplit('/');
        let last = segs.next()?;
        let candidates = [
            Some(last.to_owned()),
            segs.next().map(|p| format!("{p}-{last}")),
        ];
        candidates
            .into_iter()
            .flatten()
            .filter_map(|c| c.parse::<RepoName>().ok())
            .find(|n| self.get(n).is_none())
    }

    /// Appends an entry to the file and to this roster.
    pub fn append(&mut self, entry: RepoEntry) -> Result<(), Error> {
        use std::io::Write;
        let style = match entry.style {
            PrefixStyle::Conventional => "",
            PrefixStyle::Gitflow => "\tgitflow",
        };
        let existing = std::fs::read(&self.path).map_err(Error::io(&self.path))?;
        let sep = if existing.last().is_some_and(|b| *b != b'\n') {
            "\n"
        } else {
            ""
        };
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(Error::io(&self.path))?;
        writeln!(f, "{sep}{}\t{}{style}", entry.name, entry.gitlab)
            .map_err(Error::io(&self.path))?;
        self.entries.push(entry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The format of the roster that already exists, aligned with tab runs.
    #[test]
    fn parses_existing_format() -> Result<(), WorkspaceError> {
        let text = "# Roster\n#\n# local-name\t\tgitlab path\ndocs\tteam/docs\nCore\t\tteam/Core\tgitflow\n\n";
        let r = Roster::parse(Path::new("repos.tsv"), text)?;
        let names: Vec<_> = r
            .iter()
            .map(|e| (e.name.as_str(), e.gitlab.as_str(), e.style))
            .collect();
        assert_eq!(
            names,
            [
                ("docs", "team/docs", PrefixStyle::Conventional),
                ("Core", "team/Core", PrefixStyle::Gitflow)
            ]
        );
        Ok(())
    }

    #[test]
    fn refuses_duplicates() {
        let text = "a\tg/a\nb\tg/a\n";
        assert!(matches!(
            Roster::parse(Path::new("repos.tsv"), text),
            Err(WorkspaceError::Roster { line: 2, .. })
        ));
    }
}
