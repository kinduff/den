use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub file: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub entrypoint: Option<String>,
    pub home_cwd: Option<PathBuf>,
    pub cwds: Vec<PathBuf>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub branch: Option<String>,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
}

impl SessionMeta {
    pub fn is_typed(&self) -> bool {
        matches!(self.entrypoint.as_deref(), None | Some("cli"))
    }

    pub fn label(&self) -> String {
        fn flat(s: &str) -> Option<String> {
            let f = s.split_whitespace().collect::<Vec<_>>().join(" ");
            (!f.is_empty()).then_some(f)
        }
        if let Some(title) = self.title.as_deref().and_then(flat) {
            return title;
        }
        if let Some(prompt) = self.last_prompt.as_deref().and_then(flat) {
            let mut chars = prompt.chars();
            let head: String = chars.by_ref().take(60).collect();
            return if chars.next().is_some() {
                format!("{head}…")
            } else {
                head
            };
        }
        self.id.chars().take(8).collect()
    }
}

#[derive(Deserialize)]
struct MessageLine {
    cwd: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    entrypoint: Option<String>,
}

#[derive(Deserialize)]
struct MetaLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "lastPrompt")]
    last_prompt: Option<String>,
}

const META_PREFIX: &[u8] = b"{\"type\":\"";

fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

pub fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn parse_session(path: &Path) -> std::io::Result<SessionMeta> {
    let file = File::open(path)?;
    let md = file.metadata()?;
    let mtime = mtime_secs(&md);
    let mut meta = SessionMeta {
        id: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        file: path.to_path_buf(),
        size: md.len(),
        mtime,
        entrypoint: None,
        home_cwd: None,
        cwds: Vec::new(),
        first_ts: None,
        last_ts: None,
        branch: None,
        title: None,
        last_prompt: None,
    };

    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.starts_with(META_PREFIX) {
            let rest = &line[META_PREFIX.len()..];
            if (rest.starts_with(b"ai-title\"") || rest.starts_with(b"last-prompt\""))
                && let Ok(m) = serde_json::from_slice::<MetaLine>(&line)
            {
                match m.kind.as_str() {
                    "ai-title" if m.ai_title.is_some() => meta.title = m.ai_title,
                    "last-prompt" if m.last_prompt.is_some() => meta.last_prompt = m.last_prompt,
                    _ => {}
                }
            }
            continue;
        }
        let Ok(m) = serde_json::from_slice::<MessageLine>(&line) else {
            continue;
        };
        if meta.entrypoint.is_none() {
            meta.entrypoint = m.entrypoint;
        }
        if let Some(cwd) = m.cwd {
            let cwd = PathBuf::from(cwd);
            if meta.home_cwd.is_none() {
                meta.home_cwd = Some(cwd.clone());
            }
            if !meta.cwds.contains(&cwd) {
                meta.cwds.push(cwd);
            }
        }
        if let Some(ts) = m.timestamp.as_deref().and_then(parse_ts) {
            if meta.first_ts.is_none() {
                meta.first_ts = Some(ts);
            }
            meta.last_ts = Some(ts);
        }
        if m.git_branch.is_some() {
            meta.branch = m.git_branch;
        }
    }
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fx(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn typed_session_fields() {
        let m = parse_session(&fx("typed.jsonl")).unwrap();
        assert_eq!(m.id, "typed");
        assert_eq!(m.entrypoint.as_deref(), Some("cli"));
        assert_eq!(m.home_cwd, Some("/w/inventory".into()));
        assert_eq!(m.branch.as_deref(), Some("main"));
        assert_eq!(m.title.as_deref(), Some("Second title"));
        assert_eq!(m.last_prompt.as_deref(), Some("why is it empty"));
        assert_eq!(m.first_ts, Some(1794181130));
        assert!(m.last_ts > m.first_ts);
        assert!(m.is_typed());
    }

    #[test]
    fn headless_is_not_typed() {
        assert!(!parse_session(&fx("headless.jsonl")).unwrap().is_typed());
    }

    #[test]
    fn wander_keeps_every_cwd_once_in_order() {
        let m = parse_session(&fx("wander.jsonl")).unwrap();
        assert_eq!(m.home_cwd, Some("/w".into()));
        assert_eq!(
            m.cwds,
            vec![
                PathBuf::from("/w"),
                "/w/inventory".into(),
                "/w/lab/x".into()
            ]
        );
    }

    #[test]
    fn broken_lines_are_skipped() {
        let m = parse_session(&fx("broken.jsonl")).unwrap();
        assert_eq!(m.cwds.len(), 1);
        assert!(m.last_ts.is_some());
    }

    #[test]
    fn empty_file_gives_empty_meta() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e.jsonl");
        File::create(&p).unwrap();
        let m = parse_session(&p).unwrap();
        assert!(m.entrypoint.is_none() && m.home_cwd.is_none());
        assert!(m.first_ts.is_none() && m.last_ts.is_none());
        assert!(m.branch.is_none() && m.title.is_none() && m.last_prompt.is_none());
        assert!(m.cwds.is_empty());
        assert!(m.is_typed());
    }

    #[test]
    fn big_lines_are_not_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.jsonl");
        let mut f = File::create(&p).unwrap();
        writeln!(
            f,
            r#"{{"parentUuid":null,"type":"user","entrypoint":"cli","cwd":"/w","timestamp":"2026-11-08T23:38:50.074Z"}}"#
        )
        .unwrap();
        f.write_all(b"{\"type\":\"api-request-blob\",\"cwd\":\"/evil\",\"blob\":\"")
            .unwrap();
        f.write_all(&vec![b'x'; 5 * 1024 * 1024]).unwrap();
        f.write_all(b"\"}\n").unwrap();
        drop(f);
        let m = parse_session(&p).unwrap();
        assert_eq!(m.home_cwd, Some("/w".into()));
        assert_eq!(m.entrypoint.as_deref(), Some("cli"));
        assert_eq!(m.cwds, vec![PathBuf::from("/w")]);
    }

    #[test]
    fn label_skips_empty_text() {
        let mut m = parse_session(&fx("headless.jsonl")).unwrap();
        m.id = "abcdefghijkl".into();
        m.title = Some("  \n ".into());
        m.last_prompt = Some("".into());
        assert_eq!(m.label(), "abcdefgh");
        m.last_prompt = Some("real prompt".into());
        assert_eq!(m.label(), "real prompt");
        m.title = Some("".into());
        assert_eq!(m.label(), "real prompt");
    }

    #[test]
    fn label_flattens_newlines() {
        let mut m = parse_session(&fx("headless.jsonl")).unwrap();
        m.title = None;
        m.last_prompt = Some("a\n\n  b\tc\r\nd".into());
        assert_eq!(m.label(), "a b c d");
        m.title = Some("x\ny".into());
        assert_eq!(m.label(), "x y");
    }

    #[test]
    fn label_falls_back() {
        let mut m = parse_session(&fx("headless.jsonl")).unwrap();
        m.id = "abcdefghijkl".into();
        assert_eq!(m.label(), "abcdefgh");
        m.last_prompt = Some("é".repeat(70));
        assert_eq!(m.label(), format!("{}…", "é".repeat(60)));
        m.last_prompt = Some("short".into());
        assert_eq!(m.label(), "short");
        m.title = Some("T".into());
        assert_eq!(m.label(), "T");
    }
}
