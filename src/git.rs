//! Repo, worktree and status facts, read by running the `git` binary.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct RepoInfo {
    pub root: PathBuf,
    /// Equals `root` unless `dir` is inside a linked worktree.
    pub main_root: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub changed: u32,
    pub ahead: u32,
    pub behind: u32,
}

/// Runs `git -C dir args` and returns stdout on success, `None` on any failure.
fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    run_git_with(dir, args, GIT_DEADLINE, None)
}

/// How long one git call may take before it is killed and counts as failed.
const GIT_DEADLINE: Duration = Duration::from_secs(5);

/// `run_git` with its deadline and, for tests, the child's `PATH`. Stdout is
/// read on its own thread, so a large output cannot fill the pipe and stall
/// the child while this waits.
fn run_git_with(
    dir: &Path,
    args: &[&str],
    deadline: Duration,
    path_env: Option<&OsStr>,
) -> Option<String> {
    let mut cmd = Command::new("git");
    if let Some(p) = path_env {
        cmd.env("PATH", p);
    }
    let mut child = cmd
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    // Something git started may still hold the pipe open: wait no longer
    // than the deadline allows.
    let left = deadline
        .saturating_sub(start.elapsed())
        .max(Duration::from_millis(100));
    let out = rx.recv_timeout(left).ok()?;
    if !status.success() {
        return None;
    }
    String::from_utf8(out).ok()
}

pub fn repo_info(dir: &Path) -> Option<RepoInfo> {
    let out = run_git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
    )?;
    let mut lines = out.lines();
    let root = PathBuf::from(lines.next()?);
    let common = PathBuf::from(lines.next()?);
    let main_root = match common.file_name() {
        Some(name) if name == ".git" => common.parent().map(Path::to_path_buf)?,
        _ => root.clone(),
    };
    Some(RepoInfo { root, main_root })
}

pub fn status(dir: &Path) -> Option<GitStatus> {
    let out = run_git(dir, &["status", "--porcelain=v2", "--branch"])?;
    let mut st = GitStatus::default();
    for line in out.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            st.branch = (head != "(detached)").then(|| head.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    st.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    st.behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            st.changed += 1;
        }
    }
    Some(st)
}

pub fn worktrees(main_root: &Path) -> Vec<PathBuf> {
    let Some(out) = run_git(main_root, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .skip(1)
        .map(PathBuf::from)
        .collect()
}

pub fn last_commit_time(dir: &Path) -> Option<i64> {
    run_git(dir, &["log", "-1", "--format=%ct"])?
        .trim()
        .parse()
        .ok()
}

/// Temp-repo helpers for tests. Git runs with no global or system config,
/// so a test never triggers commit signing.
#[cfg(test)]
pub(crate) mod testgit {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    pub fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pub fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        git(dir, &["config", "tag.gpgsign", "false"]);
        fs::write(dir.join("f.txt"), "one\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "first"]);
    }

    pub fn canon(p: &Path) -> PathBuf {
        fs::canonicalize(p).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testgit::{canon, git, init_repo};
    use super::*;
    use std::fs;

    #[test]
    fn plain_folder_is_not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(repo_info(tmp.path()), None);
        assert_eq!(status(tmp.path()), None);
    }

    #[test]
    fn repo_root_from_a_subfolder() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = canon(tmp.path()).join("repo");
        init_repo(&repo);
        let sub = repo.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        let info = repo_info(&sub).unwrap();
        assert_eq!(canon(&info.root), canon(&repo));
        assert_eq!(canon(&info.main_root), canon(&repo));
    }

    #[test]
    fn linked_worktree_points_at_main() {
        let tmp = tempfile::tempdir().unwrap();
        let base = canon(tmp.path());
        let repo = base.join("repo");
        init_repo(&repo);
        let wt = base.join("wt");
        git(&repo, &["worktree", "add", "../wt", "-b", "feat"]);
        let info = repo_info(&wt).unwrap();
        assert_eq!(canon(&info.root), canon(&wt));
        assert_eq!(canon(&info.main_root), canon(&repo));
        let list: Vec<PathBuf> = worktrees(&repo).iter().map(|p| canon(p)).collect();
        assert_eq!(list, vec![canon(&wt)]);
    }

    #[test]
    fn status_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = canon(tmp.path()).join("repo");
        init_repo(&repo);
        let clean = status(&repo).unwrap();
        assert_eq!(clean.changed, 0);
        assert_eq!(clean.branch.as_deref(), Some("main"));
        assert_eq!((clean.ahead, clean.behind), (0, 0));
        fs::write(repo.join("f.txt"), "two\n").unwrap();
        fs::write(repo.join("new.txt"), "x\n").unwrap();
        assert_eq!(status(&repo).unwrap().changed, 2);
        assert!(last_commit_time(&repo).unwrap() > 0);
    }

    #[test]
    fn detached_head_has_no_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = canon(tmp.path()).join("repo");
        init_repo(&repo);
        git(&repo, &["checkout", "--detach"]);
        assert_eq!(status(&repo).unwrap().branch, None);
    }

    /// A fake `git` in a temp folder, found first through the child's PATH.
    fn fake_git(body: &str) -> (tempfile::TempDir, std::ffi::OsString) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let git = tmp.path().join("git");
        fs::write(&git, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", tmp.path().display());
        (tmp, path.into())
    }

    #[test]
    fn hung_git_is_killed_at_the_deadline() {
        let (tmp, path) = fake_git("exec sleep 30");
        let start = std::time::Instant::now();
        let out = run_git_with(
            tmp.path(),
            &["status"],
            Duration::from_millis(300),
            Some(&path),
        );
        assert_eq!(out, None);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn large_git_output_does_not_deadlock() {
        let (tmp, path) = fake_git("head -c 1000000 /dev/zero | tr '\\0' a");
        let out = run_git_with(tmp.path(), &["log"], Duration::from_secs(5), Some(&path));
        assert_eq!(out.map(|o| o.len()), Some(1_000_000));
    }

    #[test]
    fn missing_dir_is_none() {
        let missing = Path::new("/nonexistent/x");
        assert_eq!(repo_info(missing), None);
        assert_eq!(status(missing), None);
        assert_eq!(last_commit_time(missing), None);
        assert!(worktrees(missing).is_empty());
    }
}
