//! Parsing of Liquibase-style "formatted SQL" changelog files.
//!
//! ```sql
//! --liquibase formatted sql
//!
//! --changeset alice:create-users runOnChange:true
//! CREATE TABLE users (id INT PRIMARY KEY);
//! --rollback
//! DROP TABLE users;
//! ```
//!
//! Everything after a `--rollback` line (up to the next `--changeset`) is rollback SQL.
//! Text after `--rollback` on the same line is rollback SQL too, so the single-line
//! Liquibase form `--rollback DROP TABLE users;` also works.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The identity of a changeset: Liquibase identifies a changeset by `id`, `author`
/// and the file it lives in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeSetId {
    pub id: String,
    pub author: String,
    pub filename: String,
}

impl fmt::Display for ChangeSetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}::{}", self.filename, self.id, self.author)
    }
}

#[derive(Debug, Clone)]
pub struct ChangeSet {
    pub key: ChangeSetId,
    pub run_on_change: bool,
    pub run_always: bool,
    /// Forward SQL, whitespace-normalized. May contain several statements.
    pub sql: String,
    pub rollback: Option<String>,
    /// Hex SHA-256 of `sql`. Rollback SQL and attributes are not part of the checksum.
    pub checksum: String,
    /// 1-based line of the `--changeset` header.
    pub line: usize,
}

/// Loads a changelog from a single `.sql` file, or from every `.sql` file under a
/// directory (recursively, in lexicographic order of their relative paths).
///
/// The stored `filename` is the path relative to the directory, or the bare file name
/// when `path` is a file, so changesets keep their identity regardless of the cwd.
pub fn load_changelog(path: &Path) -> Result<Vec<ChangeSet>> {
    let io_err = |p: &Path| {
        let p = p.to_path_buf();
        move |source| Error::Io { path: p, source }
    };

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if fs::metadata(path).map_err(io_err(path))?.is_dir() {
        collect_sql_files(path, path, &mut files)?;
    } else {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        files.push((name, path.to_path_buf()));
    }
    files.sort();

    let mut seen = HashSet::new();
    let mut changesets = Vec::new();
    for (name, file) in files {
        let content = fs::read_to_string(&file).map_err(io_err(&file))?;
        for cs in parse(&name, &content)? {
            if !seen.insert(cs.key.clone()) {
                return Err(Error::DuplicateChangeSet(cs.key));
            }
            changesets.push(cs);
        }
    }
    Ok(changesets)
}

fn collect_sql_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|source| Error::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_sql_files(root, &path, out)?;
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
        {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((name, path));
        }
    }
    Ok(())
}

/// Parses the contents of one formatted-SQL file.
pub fn parse(filename: &str, content: &str) -> Result<Vec<ChangeSet>> {
    let mut changesets = Vec::new();
    let mut current: Option<Builder> = None;

    for (idx, raw) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();

        if let Some(rest) = directive(line, "changeset") {
            if let Some(b) = current.take() {
                changesets.push(b.finish(filename)?);
            }
            current = Some(Builder::from_header(filename, line_no, rest)?);
            continue;
        }

        if let Some(rest) = directive(line, "rollback") {
            let b = current.as_mut().ok_or_else(|| {
                parse_error(filename, line_no, "--rollback before any --changeset")
            })?;
            b.in_rollback = true;
            if !rest.is_empty() {
                b.rollback.push(rest.to_string());
            }
            continue;
        }

        match current.as_mut() {
            Some(b) if b.in_rollback => b.rollback.push(raw.to_string()),
            Some(b) => b.sql.push(raw.to_string()),
            None if line.is_empty() || line.starts_with("--") => {}
            None => {
                return Err(parse_error(
                    filename,
                    line_no,
                    "SQL found before the first --changeset",
                ));
            }
        }
    }

    if let Some(b) = current {
        changesets.push(b.finish(filename)?);
    }
    Ok(changesets)
}

/// Matches `--<name>` / `-- <name>` followed by end of line or whitespace, and returns
/// the trimmed remainder.
fn directive<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix("--")?.trim_start().strip_prefix(name)?;
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

fn parse_error(file: &str, line: usize, message: impl Into<String>) -> Error {
    Error::Parse {
        file: file.to_string(),
        line,
        message: message.into(),
    }
}

struct Builder {
    id: String,
    author: String,
    line: usize,
    run_on_change: bool,
    run_always: bool,
    sql: Vec<String>,
    rollback: Vec<String>,
    in_rollback: bool,
}

impl Builder {
    fn from_header(file: &str, line: usize, header: &str) -> Result<Self> {
        let mut tokens = header.split_whitespace();
        let ident = tokens
            .next()
            .ok_or_else(|| parse_error(file, line, "expected `--changeset <author>:<id>`"))?;
        let (author, id) = ident
            .split_once(':')
            .filter(|(a, i)| !a.is_empty() && !i.is_empty())
            .ok_or_else(|| {
                parse_error(
                    file,
                    line,
                    format!("expected `<author>:<id>`, found `{ident}`"),
                )
            })?;

        let mut builder = Builder {
            id: id.to_string(),
            author: author.to_string(),
            line,
            run_on_change: false,
            run_always: false,
            sql: Vec::new(),
            rollback: Vec::new(),
            in_rollback: false,
        };

        for token in tokens {
            let (key, value) = token.split_once(':').ok_or_else(|| {
                parse_error(
                    file,
                    line,
                    format!("expected `<attribute>:<value>`, found `{token}`"),
                )
            })?;
            let flag = match key {
                "runOnChange" => &mut builder.run_on_change,
                "runAlways" => &mut builder.run_always,
                // Other Liquibase attributes (context, labels, ...) are accepted and ignored.
                _ => continue,
            };
            *flag = match value {
                "true" => true,
                "false" => false,
                _ => {
                    return Err(parse_error(
                        file,
                        line,
                        format!("`{key}` must be true or false, found `{value}`"),
                    ));
                }
            };
        }
        Ok(builder)
    }

    fn finish(self, filename: &str) -> Result<ChangeSet> {
        let sql = normalize(&self.sql);
        if sql.is_empty() {
            return Err(parse_error(
                filename,
                self.line,
                format!("changeset {}:{} has no SQL", self.author, self.id),
            ));
        }
        let rollback = Some(normalize(&self.rollback)).filter(|r| !r.is_empty());
        Ok(ChangeSet {
            key: ChangeSetId {
                id: self.id,
                author: self.author,
                filename: filename.to_string(),
            },
            run_on_change: self.run_on_change,
            run_always: self.run_always,
            checksum: checksum(&sql),
            sql,
            rollback,
            line: self.line,
        })
    }
}

/// Strips trailing whitespace from each line and drops leading/trailing blank lines, so
/// line-ending and indentation-at-EOL changes do not alter the checksum.
fn normalize(lines: &[String]) -> String {
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    let start = lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(start, |i| i + 1);
    lines[start..end].join("\n")
}

pub fn checksum(sql: &str) -> String {
    hex::encode(Sha256::digest(sql.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
--liquibase formatted sql

--changeset alice:1
CREATE TABLE a (id INT);
INSERT INTO a VALUES (1);
--rollback
DROP TABLE a;

-- changeset bob:2 runOnChange:true runAlways:true context:dev
CREATE VIEW v AS SELECT * FROM a;
--rollback DROP VIEW v;
";

    #[test]
    fn parses_changesets_and_attributes() {
        let cs = parse("f.sql", SAMPLE).unwrap();
        assert_eq!(cs.len(), 2);

        assert_eq!(cs[0].key.author, "alice");
        assert_eq!(cs[0].key.id, "1");
        assert_eq!(
            cs[0].sql,
            "CREATE TABLE a (id INT);\nINSERT INTO a VALUES (1);"
        );
        assert_eq!(cs[0].rollback.as_deref(), Some("DROP TABLE a;"));
        assert!(!cs[0].run_on_change && !cs[0].run_always);
        assert_eq!(cs[0].line, 3);

        assert_eq!(cs[1].key.author, "bob");
        assert!(cs[1].run_on_change && cs[1].run_always);
        assert_eq!(cs[1].rollback.as_deref(), Some("DROP VIEW v;"));
    }

    #[test]
    fn checksum_ignores_trailing_whitespace_and_rollback() {
        let a = parse("f.sql", "--changeset a:1\nSELECT 1;\n--rollback SELECT 2;").unwrap();
        let b = parse(
            "f.sql",
            "--changeset a:1\r\nSELECT 1;   \r\n\r\n--rollback SELECT 3;",
        )
        .unwrap();
        assert_eq!(a[0].checksum, b[0].checksum);

        let c = parse("f.sql", "--changeset a:1\nSELECT 2;").unwrap();
        assert_ne!(a[0].checksum, c[0].checksum);
    }

    #[test]
    fn rejects_bad_input() {
        let err = |s: &str| parse("f.sql", s).unwrap_err().to_string();
        assert!(err("SELECT 1;").contains("before the first --changeset"));
        assert!(err("--changeset nobody\nSELECT 1;").contains("<author>:<id>"));
        assert!(err("--changeset a:1 runOnChange:yes\nSELECT 1;").contains("true or false"));
        assert!(err("--changeset a:1\n--rollback DROP x;").contains("has no SQL"));
        assert!(err("--rollback DROP x;").contains("before any --changeset"));
    }

    #[test]
    fn similar_comments_are_plain_sql() {
        let cs = parse(
            "f.sql",
            "--changeset a:1\n--changesets are cool\nSELECT 1;\n--rollbacks too",
        )
        .unwrap();
        assert_eq!(cs.len(), 1);
        assert!(cs[0].rollback.is_none());
        assert!(cs[0].sql.contains("--rollbacks too"));
    }
}
