//! Finds the folders under the scan roots that are projects even without a
//! session: every direct child of a root, and below that, down to `depth`,
//! git repos plus the direct children of a non-git project with sessions
//! (when one of those children is a repo or a typed home). Non-git folders
//! with build or tooling names and no typed session are never projects.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::project::Probe;

/// Names of non-git folders that are never projects unless a typed session
/// lives there.
const JUNK: &[&str] = &[
    "bin",
    "lib",
    "lib64",
    "share",
    "include",
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "tmp",
    "venv",
    "__pycache__",
];

/// The scanned folders, sorted, without the roots themselves.
pub fn scan(cfg: &Config, typed_homes: &[PathBuf], probe: &dyn Probe) -> Vec<PathBuf> {
    let walk = Walk {
        cfg,
        typed_homes: typed_homes.iter().map(PathBuf::as_path).collect(),
        probe,
    };
    let mut out = Vec::new();
    for root in &cfg.roots {
        if !walk.hidden(root) {
            walk.visit(root, 1, true, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

struct Walk<'a> {
    cfg: &'a Config,
    typed_homes: HashSet<&'a Path>,
    probe: &'a dyn Probe,
}

impl Walk<'_> {
    fn hidden(&self, p: &Path) -> bool {
        self.cfg.hide.iter().any(|h| p.starts_with(h))
    }

    fn is_repo(&self, p: &Path) -> bool {
        self.probe.repo_info(p).is_some_and(|ri| ri.root == p)
    }

    /// Looks at the children of `dir`, which sit at `level`. `keep_all` says
    /// every child is a project (children of a root, or of a non-git project
    /// with sessions that has a repo or typed home among its children).
    fn visit(&self, dir: &Path, level: u8, keep_all: bool, out: &mut Vec<PathBuf>) {
        if level > self.cfg.depth {
            return;
        }
        for child in self.probe.subdirs(dir) {
            let name = child
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.starts_with('.') || self.hidden(&child) {
                continue;
            }
            let is_repo = self.is_repo(&child);
            let typed_home = self.typed_homes.contains(child.as_path());
            if !is_repo && !typed_home && JUNK.contains(&name.as_str()) {
                continue;
            }
            let keep = keep_all || is_repo;
            if keep {
                out.push(child.clone());
            }
            if !is_repo {
                let keep_children = keep
                    && typed_home
                    && level < self.cfg.depth
                    && self.probe.subdirs(&child).iter().any(|c| {
                        !self.hidden(c)
                            && (self.typed_homes.contains(c.as_path()) || self.is_repo(c))
                    });
                self.visit(&child, level + 1, keep_children, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::testkit::FakeProbe;

    fn cfg(depth: u8) -> Config {
        Config {
            roots: vec!["/w".into()],
            depth,
            hide: Vec::new(),
        }
    }

    fn paths(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    fn has(v: &[PathBuf], p: &str) -> bool {
        v.iter().any(|x| x == Path::new(p))
    }

    #[test]
    fn every_direct_child_of_a_root_is_scanned() {
        let probe = FakeProbe::new()
            .dir("/w/plain")
            .repo("/w/git")
            .dir("/w/.hidden");
        let found = scan(&cfg(2), &[], &probe);
        assert!(has(&found, "/w/plain"));
        assert!(has(&found, "/w/git"));
        assert!(!has(&found, "/w/.hidden"));
        assert!(!has(&found, "/w"), "the root itself is not in the result");
    }

    #[test]
    fn deeper_levels_keep_only_git_repos() {
        let probe = FakeProbe::new()
            .repo("/w/a/sub")
            .dir("/w/a/plain")
            .repo("/w/a/plain/deep")
            .repo("/w/a/.hid");
        let found = scan(&cfg(2), &[], &probe);
        assert!(has(&found, "/w/a"));
        assert!(has(&found, "/w/a/sub"));
        assert!(!has(&found, "/w/a/plain"));
        assert!(!has(&found, "/w/a/plain/deep"), "level 3 is below depth 2");
        assert!(!has(&found, "/w/a/.hid"));
        let deeper = scan(&cfg(3), &[], &probe);
        assert!(has(&deeper, "/w/a/plain/deep"));
        assert!(!has(&deeper, "/w/a/plain"));
    }

    #[test]
    fn children_of_a_non_git_project_with_sessions_are_all_kept() {
        let probe = FakeProbe::new()
            .dir("/w/lab/x")
            .dir("/w/lab/y")
            .repo("/w/lab/r")
            .dir("/w/other/z");
        let found = scan(&cfg(2), &[PathBuf::from("/w/lab")], &probe);
        assert!(has(&found, "/w/lab/x"));
        assert!(has(&found, "/w/lab/y"));
        assert!(has(&found, "/w/other"));
        assert!(!has(&found, "/w/other/z"));
    }

    #[test]
    fn children_kept_only_when_one_is_a_repo_or_session_home() {
        let probe = FakeProbe::new()
            .dir("/w/film/v2")
            .dir("/w/film/v3")
            .dir("/w/notes/a")
            .dir("/w/notes/b");
        let homes = paths(&["/w/film", "/w/notes", "/w/notes/a"]);
        let found = scan(&cfg(2), &homes, &probe);
        assert!(has(&found, "/w/film"));
        assert!(
            !has(&found, "/w/film/v2"),
            "no repo or typed home among film's children"
        );
        assert!(!has(&found, "/w/film/v3"));
        assert!(has(&found, "/w/notes/a"));
        assert!(
            has(&found, "/w/notes/b"),
            "a typed home among them keeps all"
        );
    }

    #[test]
    fn junk_named_folders_are_not_projects() {
        let probe = FakeProbe::new()
            .dir("/w/bin")
            .dir("/w/lib64")
            .dir("/w/node_modules/pkg")
            .repo("/w/target")
            .dir("/w/build")
            .dir("/w/wallet/bin")
            .dir("/w/wallet/downloads")
            .repo("/w/wallet/vendor")
            .repo("/w/wallet/core");
        let homes = paths(&["/w/build", "/w/wallet"]);
        let found = scan(&cfg(2), &homes, &probe);
        assert!(!has(&found, "/w/bin"));
        assert!(!has(&found, "/w/lib64"));
        assert!(!has(&found, "/w/node_modules"));
        assert!(!has(&found, "/w/node_modules/pkg"));
        assert!(
            has(&found, "/w/target"),
            "a git repo is a project whatever its name"
        );
        assert!(
            has(&found, "/w/build"),
            "a typed home is a project whatever its name"
        );
        assert!(!has(&found, "/w/wallet/bin"), "junk at level 2 too");
        assert!(has(&found, "/w/wallet/downloads"));
        assert!(has(&found, "/w/wallet/vendor"));
    }

    #[test]
    fn scan_does_not_descend_into_repos() {
        let probe = FakeProbe::new().repo("/w/r").repo("/w/r/inner");
        let found = scan(&cfg(2), &[PathBuf::from("/w/r")], &probe);
        assert!(has(&found, "/w/r"));
        assert!(!has(&found, "/w/r/inner"));
    }

    #[test]
    fn hidden_paths_are_not_scanned() {
        let mut c = cfg(2);
        c.hide = vec!["/w/secret".into()];
        let probe = FakeProbe::new().dir("/w/secret").repo("/w/a/secret");
        let found = scan(&c, &[], &probe);
        assert!(!has(&found, "/w/secret"));
        assert!(has(&found, "/w/a/secret"));
    }
}
