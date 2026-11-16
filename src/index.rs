use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::session::{SessionMeta, mtime_secs, parse_session};

pub const INDEX_VERSION: u32 = 1;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub version: u32,
    pub sessions: Vec<SessionMeta>,
}

pub fn load_cache(path: &Path) -> Index {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
        .filter(|idx| idx.version == INDEX_VERSION)
        .unwrap_or_default()
}

pub fn save_cache(path: &Path, index: &Index) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, serde_json::to_vec(index)?)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Removes `<cache>.tmp*` files older than `older_than`: what a killed
/// `save_cache` leaves behind.
pub fn remove_stale_temps(cache: &Path, older_than: std::time::Duration) {
    let (Some(dir), Some(name)) = (cache.parent(), cache.file_name()) else {
        return;
    };
    let prefix = format!("{}.tmp", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let stale = e
            .metadata()
            .and_then(|md| md.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > older_than);
        if stale && e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `projects_dir/*/*.jsonl`, one level deep. Anything unreadable is skipped.
fn list_session_files(projects_dir: &Path) -> Vec<(PathBuf, std::fs::Metadata)> {
    let Ok(folders) = std::fs::read_dir(projects_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for folder in folders.flatten() {
        let Ok(files) = std::fs::read_dir(folder.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && let Ok(md) = std::fs::metadata(&path)
                && md.is_file()
            {
                out.push((path, md));
            }
        }
    }
    out
}

pub fn refresh(
    projects_dir: &Path,
    cached: &Index,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Index {
    let by_path: HashMap<&Path, &SessionMeta> = cached
        .sessions
        .iter()
        .map(|s| (s.file.as_path(), s))
        .collect();
    let files = list_session_files(projects_dir);
    let total = files.len();
    let done = AtomicUsize::new(0);
    let mut sessions: Vec<SessionMeta> = files
        .into_par_iter()
        .filter_map(|(path, md)| {
            let hit = by_path
                .get(path.as_path())
                .filter(|c| c.size == md.len() && c.mtime == mtime_secs(&md))
                .map(|c| (*c).clone());
            let meta = hit.or_else(|| parse_session(&path).ok());
            progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
            meta
        })
        .collect();
    sessions.sort_by(|a, b| b.last_ts.cmp(&a.last_ts).then_with(|| a.file.cmp(&b.file)));
    Index {
        version: INDEX_VERSION,
        sessions,
    }
}

pub fn cache_path() -> PathBuf {
    crate::config::cache_dir().join("index.json")
}

pub fn projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude/projects")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn line(ts: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"type":"user","entrypoint":"cli","cwd":"/w","timestamp":"{ts}"}}"#
        )
    }

    fn write(path: &Path, ts: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{}\n", line(ts))).unwrap();
    }

    fn none(_: usize, _: usize) {}

    #[test]
    fn finds_top_level_sessions_only() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("a/s1.jsonl"), "2026-11-08T10:00:00Z");
        write(&d.path().join("a/memory/x.jsonl"), "2026-11-08T10:00:00Z");
        write(&d.path().join("a/s1/sub.jsonl"), "2026-11-08T10:00:00Z");
        write(&d.path().join("top.jsonl"), "2026-11-08T10:00:00Z");
        let idx = refresh(d.path(), &Index::default(), &none);
        let ids: Vec<_> = idx.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s1"]);
        assert_eq!(idx.version, INDEX_VERSION);
    }

    #[test]
    fn unchanged_file_is_not_reparsed() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("a/s1.jsonl"), "2026-11-08T10:00:00Z");
        let mut idx = refresh(d.path(), &Index::default(), &none);
        idx.sessions[0].title = Some("FROM CACHE".into());
        let again = refresh(d.path(), &idx, &none);
        assert_eq!(again.sessions[0].title.as_deref(), Some("FROM CACHE"));
    }

    #[test]
    fn changed_file_is_reparsed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/s1.jsonl");
        write(&p, "2026-11-08T10:00:00Z");
        let idx = refresh(d.path(), &Index::default(), &none);
        let before = idx.sessions[0].last_ts;
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{}", line("2026-11-09T10:00:00Z")).unwrap();
        drop(f);
        let again = refresh(d.path(), &idx, &none);
        assert!(again.sessions[0].last_ts > before);
    }

    #[test]
    fn deleted_file_drops_out() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("a/s1.jsonl"), "2026-11-08T10:00:00Z");
        write(&d.path().join("a/s2.jsonl"), "2026-11-08T11:00:00Z");
        let idx = refresh(d.path(), &Index::default(), &none);
        assert_eq!(idx.sessions.len(), 2);
        fs::remove_file(d.path().join("a/s1.jsonl")).unwrap();
        let again = refresh(d.path(), &idx, &none);
        let ids: Vec<_> = again.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s2"]);
    }

    #[test]
    fn folder_vanishing_midway_is_fine() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("a/ok.jsonl"), "2026-11-08T10:00:00Z");
        symlink(d.path().join("gone"), d.path().join("dangling")).unwrap();
        symlink(d.path().join("gone.jsonl"), d.path().join("a/broken.jsonl")).unwrap();
        let locked = d.path().join("a/locked.jsonl");
        write(&locked, "2026-11-08T10:00:00Z");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let idx = refresh(d.path(), &Index::default(), &none);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        let ids: Vec<_> = idx.sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"ok"));
        assert!(!ids.contains(&"broken"));
    }

    #[test]
    fn cache_round_trip_and_version() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("p/a/s1.jsonl"), "2026-11-08T10:00:00Z");
        let idx = refresh(&d.path().join("p"), &Index::default(), &none);
        let c = d.path().join("cache/den/index.json");
        save_cache(&c, &idx).unwrap();
        assert_eq!(load_cache(&c), idx);
        fs::write(&c, r#"{"version":999,"sessions":[]}"#).unwrap();
        assert_eq!(load_cache(&c), Index::default());
        fs::write(&c, "not json").unwrap();
        assert_eq!(load_cache(&c), Index::default());
        assert_eq!(load_cache(&d.path().join("nope.json")), Index::default());
    }

    #[test]
    fn stale_temp_files_are_removed() {
        let d = tempfile::tempdir().unwrap();
        let cache = d.path().join("index.json");
        let old = d.path().join("index.json.tmp123");
        let fresh = d.path().join("index.json.tmp456");
        let other = d.path().join("notes.tmp1");
        for p in [&cache, &old, &fresh, &other] {
            fs::write(p, "x").unwrap();
        }
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for p in [&cache, &old, &other] {
            fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(hour_ago)
                .unwrap();
        }
        remove_stale_temps(&cache, std::time::Duration::from_secs(60));
        assert!(!old.exists());
        assert!(fresh.exists() && cache.exists() && other.exists());
        remove_stale_temps(
            &d.path().join("missing/index.json"),
            std::time::Duration::ZERO,
        );
    }

    #[test]
    fn missing_projects_dir_gives_empty_index() {
        let d = tempfile::tempdir().unwrap();
        let idx = refresh(&d.path().join("nope"), &Index::default(), &none);
        assert!(idx.sessions.is_empty());
    }
}
