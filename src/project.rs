//! Turns sessions plus scanned folders into the project tree.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::config::Config;
use crate::git::{self, RepoInfo};
use crate::session::SessionMeta;

/// What `build_tree` and `scan` ask of the file system and git.
pub trait Probe: Sync {
    fn exists(&self, p: &Path) -> bool;
    fn repo_info(&self, p: &Path) -> Option<RepoInfo>;
    fn subdirs(&self, p: &Path) -> Vec<PathBuf>;
    /// Linked worktrees of the repo at `main_root`, without the main one.
    fn worktrees(&self, main_root: &Path) -> Vec<PathBuf>;
}

/// The real file system and `git::repo_info`.
pub struct RealProbe;

impl Probe for RealProbe {
    fn exists(&self, p: &Path) -> bool {
        p.is_dir()
    }

    fn repo_info(&self, p: &Path) -> Option<RepoInfo> {
        git::repo_info(p)
    }

    /// Direct child folders, sorted, skipping names that start with '.'.
    fn subdirs(&self, p: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(p) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        out.sort();
        out
    }

    fn worktrees(&self, main_root: &Path) -> Vec<PathBuf> {
        git::worktrees(main_root)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Repo,
    Worktree,
    Folder,
}

/// `idx` points into the sessions slice given to `build_tree`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRef {
    pub idx: usize,
    pub also: bool,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub path: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub exists: bool,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub sessions: Vec<SessionRef>,
    pub last_activity: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub projects: Vec<Project>,
    pub top: Vec<usize>,
    pub elsewhere: Vec<usize>,
    pub gone: Vec<usize>,
}

pub fn build_tree(
    sessions: &[SessionMeta],
    scanned: &[PathBuf],
    cfg: &Config,
    home: &Path,
    probe: &dyn Probe,
) -> Tree {
    let memo = MemoProbe::new(probe);
    let probe: &dyn Probe = &memo;
    let mut b = Builder {
        cfg,
        home,
        probe,
        projects: Vec::new(),
        by_path: HashMap::new(),
        folder_project: HashMap::new(),
    };

    // 1. Folders that create projects: typed homes and scanned folders.
    let mut candidates: BTreeSet<&Path> = scanned.iter().map(PathBuf::as_path).collect();
    for s in sessions.iter().filter(|s| s.is_typed()) {
        if let Some(h) = &s.home_cwd {
            candidates.insert(h);
        }
    }
    let candidates: Vec<&Path> = candidates.into_iter().filter(|p| !b.hidden(p)).collect();
    let resolved: Vec<(&Path, bool, Option<RepoInfo>)> = candidates
        .par_iter()
        .map(|&p| {
            let exists = probe.exists(p);
            (p, exists, exists.then(|| probe.repo_info(p)).flatten())
        })
        .collect();
    let mut gone_homes = Vec::new();
    for (p, exists, info) in resolved {
        if exists {
            b.add_existing(p, info);
        } else {
            gone_homes.push(p);
        }
    }

    // 2. Every repo lists each existing checkout from `git worktree list`.
    let repos: Vec<(usize, PathBuf)> = b
        .projects
        .iter()
        .enumerate()
        .filter(|(_, p)| p.kind == Kind::Repo && p.exists)
        .map(|(i, p)| (i, p.path.clone()))
        .collect();
    let lists: Vec<(usize, Vec<PathBuf>)> = repos
        .par_iter()
        .map(|(i, p)| {
            let list = probe.worktrees(p);
            let live = list.into_iter().filter(|w| probe.exists(w)).collect();
            (*i, live)
        })
        .collect();
    for (main, list) in lists {
        for wt in list {
            if !b.hidden(&wt) {
                b.ensure(&wt, Kind::Worktree, true, Some(main));
            }
        }
    }

    // 3. Existing projects nest under the nearest project above them.
    for i in 0..b.projects.len() {
        if b.projects[i].kind != Kind::Worktree {
            b.projects[i].parent = b.nearest_group(&b.projects[i].path.clone());
        }
    }

    // 4. Gone home folders: known worktree shapes attach, the rest are gone.
    for p in gone_homes {
        b.add_gone(p);
    }
    break_parent_cycles(&mut b.projects);

    // 5. Sessions: own project, then `also` refs for wandered folders.
    for (idx, s) in sessions.iter().enumerate() {
        let Some(h) = s.home_cwd.as_deref() else {
            continue;
        };
        let own = if s.is_typed() {
            b.folder_project.get(h).copied()
        } else {
            b.locate_headless(h)
        };
        let Some(own) = own else {
            continue;
        };
        b.projects[own]
            .sessions
            .push(SessionRef { idx, also: false });
        let mut seen = vec![own];
        for cwd in s.cwds.iter().filter(|c| c.as_path() != h) {
            if let Some(t) = b.nearest_existing(cwd)
                && !seen.contains(&t)
            {
                seen.push(t);
                b.projects[t].sessions.push(SessionRef { idx, also: true });
            }
        }
    }

    b.finish(sessions)
}

/// Clears the parent link that closes a cycle (a repo inside its own
/// worktree's worktree), so every parent chain ends.
fn break_parent_cycles(projects: &mut [Project]) {
    for start in 0..projects.len() {
        let mut seen = HashSet::from([start]);
        let mut at = start;
        while let Some(p) = projects[at].parent {
            if !seen.insert(p) {
                projects[at].parent = None;
                break;
            }
            at = p;
        }
    }
}

/// A probe that asks `repo_info` once per folder; one per tree build.
pub struct MemoProbe<'a> {
    inner: &'a dyn Probe,
    repo_infos: Mutex<HashMap<PathBuf, Option<RepoInfo>>>,
}

impl<'a> MemoProbe<'a> {
    pub fn new(inner: &'a dyn Probe) -> Self {
        MemoProbe {
            inner,
            repo_infos: Mutex::new(HashMap::new()),
        }
    }
}

impl Probe for MemoProbe<'_> {
    fn exists(&self, p: &Path) -> bool {
        self.inner.exists(p)
    }

    fn repo_info(&self, p: &Path) -> Option<RepoInfo> {
        if let Some(known) = self.repo_infos.lock().ok().and_then(|m| m.get(p).cloned()) {
            return known;
        }
        let info = self.inner.repo_info(p);
        if let Ok(mut m) = self.repo_infos.lock() {
            m.insert(p.to_path_buf(), info.clone());
        }
        info
    }

    fn subdirs(&self, p: &Path) -> Vec<PathBuf> {
        self.inner.subdirs(p)
    }

    fn worktrees(&self, main_root: &Path) -> Vec<PathBuf> {
        self.inner.worktrees(main_root)
    }
}

struct Builder<'a> {
    cfg: &'a Config,
    home: &'a Path,
    probe: &'a dyn Probe,
    projects: Vec<Project>,
    by_path: HashMap<PathBuf, usize>,
    /// Folder that creates a project (typed home or scanned) -> its project.
    folder_project: HashMap<PathBuf, usize>,
}

impl Builder<'_> {
    fn hidden(&self, p: &Path) -> bool {
        self.cfg.hide.iter().any(|h| p.starts_with(h))
    }

    fn ensure(&mut self, path: &Path, kind: Kind, exists: bool, parent: Option<usize>) -> usize {
        if let Some(&i) = self.by_path.get(path) {
            return i;
        }
        let i = self.projects.len();
        self.projects.push(Project {
            path: path.to_path_buf(),
            name: String::new(),
            kind,
            exists,
            parent,
            children: Vec::new(),
            sessions: Vec::new(),
            last_activity: None,
        });
        self.by_path.insert(path.to_path_buf(), i);
        i
    }

    fn add_existing(&mut self, folder: &Path, info: Option<RepoInfo>) {
        let project = match info {
            Some(ri) if ri.root != ri.main_root => {
                if self.hidden(&ri.root) || self.hidden(&ri.main_root) {
                    return;
                }
                let main = self.ensure(&ri.main_root, Kind::Repo, true, None);
                self.ensure(&ri.root, Kind::Worktree, true, Some(main))
            }
            Some(ri) => {
                if self.hidden(&ri.root) {
                    return;
                }
                self.ensure(&ri.root, Kind::Repo, true, None)
            }
            None => self.ensure(folder, Kind::Folder, true, None),
        };
        self.folder_project.insert(folder.to_path_buf(), project);
    }

    /// The nearest existing project strictly above `path` that may be a
    /// group: never `home`, `/` or a scan root, and nothing above those.
    fn nearest_group(&self, path: &Path) -> Option<usize> {
        path.ancestors()
            .skip(1)
            .take_while(|a| {
                *a != self.home && a.parent().is_some() && !self.cfg.roots.iter().any(|r| r == a)
            })
            .find_map(|a| self.by_path.get(a).copied())
            .filter(|&i| self.projects[i].exists)
    }

    /// The nearest existing project at or above `path`.
    fn nearest_existing(&self, path: &Path) -> Option<usize> {
        if self.hidden(path) {
            return None;
        }
        path.ancestors()
            .filter_map(|a| self.by_path.get(a).copied())
            .find(|&i| self.projects[i].exists)
    }

    fn add_gone(&mut self, path: &Path) {
        let parent = self.gone_worktree_parent(path);
        let kind = if parent.is_some() {
            Kind::Worktree
        } else {
            Kind::Folder
        };
        let i = self.ensure(path, kind, false, parent);
        self.folder_project.insert(path.to_path_buf(), i);
    }

    /// `<repo>/.claude/worktrees/<name>` with `<repo>` a project, or
    /// `<home>/.herdr/worktrees/<repo-name>/<name>` with exactly one project
    /// named `<repo-name>`.
    fn gone_worktree_parent(&self, path: &Path) -> Option<usize> {
        if let Some(repo) = claude_worktree_repo(path)
            && let Some(&i) = self.by_path.get(repo)
            && self.projects[i].exists
        {
            return Some(i);
        }
        let rest = path.strip_prefix(self.home.join(".herdr/worktrees")).ok()?;
        let mut parts = rest.components();
        let (Some(repo_name), Some(_), None) = (parts.next(), parts.next(), parts.next()) else {
            return None;
        };
        let mut named = self.projects.iter().enumerate().filter(|(_, p)| {
            p.exists
                && p.kind != Kind::Worktree
                && p.path.file_name() == Some(repo_name.as_os_str())
        });
        match (named.next(), named.next()) {
            (Some((i, _)), None) => Some(i),
            _ => None,
        }
    }

    /// Headless sessions never create a project: the project of their home
    /// folder if one exists, else the nearest existing project above it.
    fn locate_headless(&self, home: &Path) -> Option<usize> {
        if self.hidden(home) {
            return None;
        }
        if let Some(&i) = self.folder_project.get(home) {
            return Some(i);
        }
        if self.probe.exists(home)
            && let Some(ri) = self.probe.repo_info(home)
            && let Some(&i) = self
                .by_path
                .get(&ri.root)
                .or_else(|| self.by_path.get(&ri.main_root))
        {
            return Some(i);
        }
        self.nearest_existing(home)
    }

    fn finish(mut self, sessions: &[SessionMeta]) -> Tree {
        let n = self.projects.len();
        let ts = |r: &SessionRef| sessions[r.idx].last_ts;
        let own_activity = |p: &Project| {
            p.sessions
                .iter()
                .filter(|r| !r.also && sessions[r.idx].is_typed())
                .filter_map(ts)
                .max()
        };

        for i in 0..n {
            if let Some(parent) = self.projects[i].parent {
                self.projects[parent].children.push(i);
            }
        }

        // A project's worktrees' sessions join its list.
        let own: Vec<Option<i64>> = self.projects.iter().map(own_activity).collect();
        for i in 0..n {
            if self.projects[i].kind != Kind::Worktree {
                continue;
            }
            let Some(parent) = self.projects[i].parent else {
                continue;
            };
            let refs = self.projects[i].sessions.clone();
            let list = &mut self.projects[parent].sessions;
            for r in refs {
                match list.iter_mut().find(|x| x.idx == r.idx) {
                    Some(x) => x.also &= r.also,
                    None => list.push(r),
                }
            }
        }
        // Activity includes every descendant's (children and worktrees).
        fn with_descendants(ps: &[Project], own: &[Option<i64>], i: usize) -> Option<i64> {
            ps[i]
                .children
                .iter()
                .map(|&c| with_descendants(ps, own, c))
                .fold(own[i], Option::max)
        }
        let activity: Vec<Option<i64>> = (0..n)
            .map(|i| with_descendants(&self.projects, &own, i))
            .collect();
        for (p, a) in self.projects.iter_mut().zip(activity) {
            p.last_activity = a;
            p.sessions
                .sort_by(|a, b| cmp_newest(ts(a), ts(b)).then(a.idx.cmp(&b.idx)));
        }

        for i in 0..n {
            let p = &self.projects[i];
            let name = match p.parent {
                Some(parent) if p.kind != Kind::Worktree => p
                    .path
                    .strip_prefix(&self.projects[parent].path)
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned()),
                _ => None,
            };
            let name = name.unwrap_or_else(|| folder_name(&p.path));
            self.projects[i].name = name;
        }

        let mut tree = Tree::default();
        for (i, p) in self.projects.iter().enumerate() {
            if p.parent.is_some() {
                continue;
            }
            if !p.exists {
                tree.gone.push(i);
            } else if self.cfg.roots.iter().any(|r| p.path.starts_with(r)) {
                tree.top.push(i);
            } else {
                tree.elsewhere.push(i);
            }
        }
        let projects = &self.projects;
        let order = |a: &usize, b: &usize| {
            let (pa, pb) = (&projects[*a], &projects[*b]);
            cmp_newest(pa.last_activity, pb.last_activity).then_with(|| pa.name.cmp(&pb.name))
        };
        tree.top.sort_by(order);
        tree.elsewhere.sort_by(order);
        tree.gone.sort_by(order);
        let mut children: Vec<Vec<usize>> = projects.iter().map(|p| p.children.clone()).collect();
        for c in &mut children {
            c.sort_by(order);
        }
        for (p, c) in self.projects.iter_mut().zip(children) {
            p.children = c;
        }
        tree.projects = self.projects;
        tree
    }
}

/// `<repo>` when `path` is shaped `<repo>/.claude/worktrees/<name>`.
pub fn claude_worktree_repo(path: &Path) -> Option<&Path> {
    let worktrees = path.parent()?;
    let claude = worktrees.parent()?;
    (worktrees.file_name()? == "worktrees" && claude.file_name()? == ".claude")
        .then(|| claude.parent())
        .flatten()
}

/// Newest first, `None` last.
fn cmp_newest(a: Option<i64>, b: Option<i64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn folder_name(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

impl Tree {
    /// The folder to resume `s` from: its home folder if it exists, the
    /// parent repo for a gone worktree, else the nearest existing ancestor.
    pub fn open_dir(&self, s: &SessionMeta, probe: &dyn Probe) -> PathBuf {
        match s.home_cwd.as_deref() {
            Some(home) => self.dir_to_open(home, probe),
            None => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
        }
    }

    /// `folder` if it exists, the parent repo for a gone worktree, else the
    /// nearest existing ancestor.
    pub fn dir_to_open(&self, folder: &Path, probe: &dyn Probe) -> PathBuf {
        if probe.exists(folder) {
            return folder.to_path_buf();
        }
        if let Some(parent) = self
            .projects
            .iter()
            .find(|p| p.path == folder && p.kind == Kind::Worktree)
            .and_then(|p| p.parent)
            .map(|i| &self.projects[i])
            .filter(|p| probe.exists(&p.path))
        {
            return parent.path.clone();
        }
        if let Some(repo) = claude_worktree_repo(folder).filter(|r| probe.exists(r)) {
            return repo.to_path_buf();
        }
        folder
            .ancestors()
            .find(|a| probe.exists(a))
            .unwrap_or(Path::new("/"))
            .to_path_buf()
    }
}

#[cfg(test)]
pub mod testkit {
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::Probe;
    use crate::git::RepoInfo;

    /// In-memory folders and repos. Adding a folder adds its ancestors.
    #[derive(Default)]
    pub struct FakeProbe {
        pub dirs: HashSet<PathBuf>,
        pub repos: HashMap<PathBuf, RepoInfo>,
        pub worktrees: HashMap<PathBuf, Vec<PathBuf>>,
        /// How often `repo_info` was asked about each folder.
        pub calls: Mutex<HashMap<PathBuf, usize>>,
    }

    impl FakeProbe {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn dir(mut self, p: impl AsRef<Path>) -> Self {
            for a in p.as_ref().ancestors() {
                self.dirs.insert(a.to_path_buf());
            }
            self
        }

        pub fn repo(self, p: impl AsRef<Path>) -> Self {
            let p = p.as_ref().to_path_buf();
            self.worktree(&p, &p)
        }

        /// A linked worktree at `p` whose main repo is `main`; it shows in
        /// `worktrees(main)` like `git worktree list`.
        pub fn worktree(mut self, p: impl AsRef<Path>, main: impl AsRef<Path>) -> Self {
            let p = p.as_ref().to_path_buf();
            self = self.dir(&p);
            if p != main.as_ref() {
                self.worktrees
                    .entry(main.as_ref().to_path_buf())
                    .or_default()
                    .push(p.clone());
            }
            self.repos.insert(
                p.clone(),
                RepoInfo {
                    root: p,
                    main_root: main.as_ref().to_path_buf(),
                },
            );
            self
        }
    }

    impl Probe for FakeProbe {
        fn exists(&self, p: &Path) -> bool {
            self.dirs.contains(p)
        }

        /// Like git: none for a missing folder, else the nearest repo at or above.
        fn repo_info(&self, p: &Path) -> Option<RepoInfo> {
            *self
                .calls
                .lock()
                .unwrap()
                .entry(p.to_path_buf())
                .or_default() += 1;
            if !self.exists(p) {
                return None;
            }
            p.ancestors().find_map(|a| self.repos.get(a).cloned())
        }

        fn subdirs(&self, p: &Path) -> Vec<PathBuf> {
            let mut out: Vec<PathBuf> = self
                .dirs
                .iter()
                .filter(|d| d.parent() == Some(p))
                .cloned()
                .collect();
            out.sort();
            out
        }

        fn worktrees(&self, main_root: &Path) -> Vec<PathBuf> {
            self.worktrees.get(main_root).cloned().unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::FakeProbe;
    use super::*;

    const HOME: &str = "/h";

    fn session(home: &str, cwds: &[&str], ts: i64, typed: bool) -> SessionMeta {
        let mut all: Vec<PathBuf> = vec![PathBuf::from(home)];
        for c in cwds {
            if !all.iter().any(|x| x == Path::new(c)) {
                all.push(PathBuf::from(c));
            }
        }
        SessionMeta {
            id: format!("s{ts}"),
            file: PathBuf::from(format!("/p/s{ts}.jsonl")),
            size: 0,
            mtime: 0,
            entrypoint: Some(if typed { "cli" } else { "sdk-cli" }.into()),
            home_cwd: Some(PathBuf::from(home)),
            cwds: all,
            first_ts: Some(ts),
            last_ts: Some(ts),
            branch: None,
            title: None,
            last_prompt: None,
        }
    }

    fn typed(home: &str, ts: i64) -> SessionMeta {
        session(home, &[], ts, true)
    }

    fn headless(home: &str, ts: i64) -> SessionMeta {
        session(home, &[], ts, false)
    }

    fn cfg() -> Config {
        Config {
            roots: vec!["/w".into()],
            depth: 2,
            hide: Vec::new(),
        }
    }

    fn paths(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    fn build(sessions: &[SessionMeta], scanned: &[&str], probe: &FakeProbe) -> Tree {
        build_tree(sessions, &paths(scanned), &cfg(), Path::new(HOME), probe)
    }

    fn find<'a>(t: &'a Tree, p: &str) -> Option<&'a Project> {
        t.projects.iter().find(|x| x.path == Path::new(p))
    }

    fn get<'a>(t: &'a Tree, p: &str) -> &'a Project {
        find(t, p).unwrap_or_else(|| panic!("no project {p}: {:#?}", t.projects))
    }

    fn idx(t: &Tree, p: &str) -> usize {
        t.projects
            .iter()
            .position(|x| x.path == Path::new(p))
            .unwrap_or_else(|| panic!("no project {p}"))
    }

    fn r(idx: usize, also: bool) -> SessionRef {
        SessionRef { idx, also }
    }

    #[test]
    fn repo_root_is_the_project_for_a_subfolder_session() {
        let probe = FakeProbe::new().repo("/w/inv").dir("/w/inv/app");
        let t = build(&[typed("/w/inv/app", 10)], &[], &probe);
        let inv = get(&t, "/w/inv");
        assert_eq!(inv.kind, Kind::Repo);
        assert_eq!(inv.sessions, vec![r(0, false)]);
        assert_eq!(inv.last_activity, Some(10));
        assert!(find(&t, "/w/inv/app").is_none());
        assert_eq!(t.projects.len(), 1);
    }

    #[test]
    fn worktree_nests_under_its_main_repo() {
        let wt = "/w/inv/.claude/worktrees/feat";
        let herdr = "/h/.herdr/worktrees/inv/other";
        let probe = FakeProbe::new()
            .repo("/w/inv")
            .worktree(wt, "/w/inv")
            .worktree(herdr, "/w/inv");
        let sessions = [typed(wt, 200), typed("/w/inv", 100), typed(herdr, 50)];
        let t = build(&sessions, &[], &probe);
        let inv_i = idx(&t, "/w/inv");
        let inv = &t.projects[inv_i];
        assert_eq!(inv.kind, Kind::Repo);
        assert_eq!(inv.parent, None);
        assert_eq!(inv.sessions, vec![r(0, false), r(1, false), r(2, false)]);
        assert_eq!(inv.last_activity, Some(200));
        for (p, name) in [(wt, "feat"), (herdr, "other")] {
            let w = get(&t, p);
            assert_eq!(w.kind, Kind::Worktree);
            assert!(w.exists);
            assert_eq!(w.parent, Some(inv_i));
            assert_eq!(w.name, name);
        }
        assert_eq!(get(&t, wt).sessions, vec![r(0, false)]);
        assert_eq!(inv.children, vec![idx(&t, wt), idx(&t, herdr)]);
        assert_eq!(t.top, vec![inv_i]);
    }

    #[test]
    fn non_git_home_folder_is_its_own_project() {
        let probe = FakeProbe::new().dir("/w/notes");
        let t = build(&[typed("/w/notes", 1)], &[], &probe);
        let p = get(&t, "/w/notes");
        assert_eq!(p.kind, Kind::Folder);
        assert_eq!(p.name, "notes");
        assert!(p.exists);
        assert_eq!(p.sessions, vec![r(0, false)]);
    }

    #[test]
    fn wandered_folder_never_creates_a_project() {
        let probe = FakeProbe::new().dir("/w/scratch");
        let s = session("/w", &["/w", "/w/scratch"], 1, true);
        let t = build(&[s], &[], &probe);
        assert!(find(&t, "/w/scratch").is_none());
        assert_eq!(t.projects.len(), 1);
        assert_eq!(get(&t, "/w").sessions, vec![r(0, false)]);
    }

    #[test]
    fn wandering_session_shows_as_also_under_the_project() {
        let probe = FakeProbe::new()
            .repo("/w/inventory")
            .dir("/w/inventory/app");
        let s = session(
            "/w",
            &["/w/inventory/app", "/w/inventory", "/w/nowhere"],
            5,
            true,
        );
        let t = build(&[s], &["/w/inventory"], &probe);
        let inv = get(&t, "/w/inventory");
        assert_eq!(inv.sessions, vec![r(0, true)]);
        assert_eq!(inv.last_activity, None, "an also ref does not count");
        assert_eq!(get(&t, "/w").sessions, vec![r(0, false)]);
    }

    #[test]
    fn group_lists_children_and_its_own_sessions() {
        let probe = FakeProbe::new()
            .dir("/w/lab")
            .repo("/w/lab/zombies")
            .repo("/w/lab/decomp/TLK");
        let sessions = [typed("/w/lab", 10), typed("/w/lab/zombies", 99)];
        let t = build(
            &sessions,
            &["/w/lab", "/w/lab/zombies", "/w/lab/decomp/TLK"],
            &probe,
        );
        let lab_i = idx(&t, "/w/lab");
        let lab = &t.projects[lab_i];
        assert_eq!(lab.sessions, vec![r(0, false)]);
        assert_eq!(
            lab.children,
            vec![idx(&t, "/w/lab/zombies"), idx(&t, "/w/lab/decomp/TLK")]
        );
        let z = get(&t, "/w/lab/zombies");
        assert_eq!(z.parent, Some(lab_i));
        assert_eq!(z.name, "zombies");
        assert_eq!(z.sessions, vec![r(1, false)]);
        assert_eq!(get(&t, "/w/lab/decomp/TLK").name, "decomp/TLK");
        assert_eq!(t.top, vec![lab_i]);
    }

    #[test]
    fn scan_root_is_never_a_group() {
        let probe = FakeProbe::new().repo("/w/a").dir("/w/b");
        let t = build(
            &[typed("/w", 3), typed("/w/a", 2)],
            &["/w/a", "/w/b"],
            &probe,
        );
        let w = idx(&t, "/w");
        let w_p = &t.projects[w];
        assert!(w_p.children.is_empty());
        assert_eq!(w_p.kind, Kind::Folder);
        assert_eq!(w_p.sessions, vec![r(0, false)]);
        assert_eq!(get(&t, "/w/a").parent, None);
        assert_eq!(get(&t, "/w/b").parent, None);
        assert_eq!(t.top, vec![w, idx(&t, "/w/a"), idx(&t, "/w/b")]);
    }

    #[test]
    fn repo_lists_all_git_worktrees_even_without_sessions() {
        let mut probe = FakeProbe::new()
            .repo("/w/inv")
            .worktree("/h/.herdr/worktrees/inv/a", "/w/inv")
            .worktree("/w/inv/.claude/worktrees/b", "/w/inv")
            .worktree("/w/secret/c", "/w/inv");
        probe
            .worktrees
            .get_mut(Path::new("/w/inv"))
            .unwrap()
            .push(PathBuf::from("/h/.herdr/worktrees/inv/pruned"));
        let mut c = cfg();
        c.hide = vec!["/w/secret".into()];
        let sessions = [typed("/w/inv/.claude/worktrees/b", 7)];
        let t = build_tree(&sessions, &paths(&["/w/inv"]), &c, Path::new(HOME), &probe);
        let inv = idx(&t, "/w/inv");
        let a = idx(&t, "/h/.herdr/worktrees/inv/a");
        let b = idx(&t, "/w/inv/.claude/worktrees/b");
        for i in [a, b] {
            assert_eq!(t.projects[i].kind, Kind::Worktree);
            assert_eq!(t.projects[i].parent, Some(inv));
            assert!(t.projects[i].exists);
        }
        assert_eq!(t.projects[inv].children, vec![b, a]);
        assert_eq!(t.projects[b].sessions, vec![r(0, false)]);
        assert!(t.projects[a].sessions.is_empty());
        assert!(find(&t, "/w/secret/c").is_none(), "hide still applies");
        assert!(
            find(&t, "/h/.herdr/worktrees/inv/pruned").is_none(),
            "only existing checkouts"
        );
        assert_eq!(
            t.projects.len(),
            3,
            "no duplicate of the session's worktree"
        );
    }

    #[test]
    fn last_activity_includes_descendants() {
        let probe = FakeProbe::new()
            .dir("/w/lab")
            .dir("/w/lab/deep")
            .repo("/w/lab/deep/z")
            .worktree("/w/lab/deep/z-wt", "/w/lab/deep/z")
            .dir("/w/quiet");
        let sessions = [
            typed("/w/lab", 10),
            typed("/w/lab/deep", 20),
            typed("/w/lab/deep/z-wt", 99),
            headless("/w/lab", 500),
            headless("/w/quiet", 600),
        ];
        let t = build(&sessions, &["/w/lab", "/w/quiet"], &probe);
        assert_eq!(get(&t, "/w/lab/deep/z-wt").last_activity, Some(99));
        assert_eq!(get(&t, "/w/lab/deep/z").last_activity, Some(99));
        assert_eq!(get(&t, "/w/lab/deep").last_activity, Some(99));
        assert_eq!(get(&t, "/w/lab").last_activity, Some(99));
        assert_eq!(
            get(&t, "/w/quiet").last_activity,
            None,
            "headless never counts"
        );
        assert_eq!(get(&t, "/w/lab").sessions, vec![r(3, false), r(0, false)]);
    }

    #[test]
    fn home_dir_is_never_a_group() {
        let probe = FakeProbe::new().dir("/h/notes").dir("/w");
        let t = build(&[typed("/h", 2), typed("/h/notes", 1)], &[], &probe);
        let h = get(&t, "/h");
        assert!(h.children.is_empty());
        assert_eq!(get(&t, "/h/notes").parent, None);
        assert_eq!(t.elsewhere, vec![idx(&t, "/h"), idx(&t, "/h/notes")]);
        assert!(t.top.is_empty());

        let probe = FakeProbe::new().dir("/x");
        let t = build(&[typed("/", 2), typed("/x", 1)], &[], &probe);
        assert!(get(&t, "/").children.is_empty());
        assert_eq!(get(&t, "/").name, "/");
        assert_eq!(get(&t, "/x").parent, None);
    }

    #[test]
    fn project_outside_roots_goes_to_elsewhere() {
        let probe = FakeProbe::new().dir("/opt/tool").dir("/w/a");
        let t = build(&[typed("/opt/tool", 1), typed("/w/a", 2)], &[], &probe);
        assert_eq!(t.elsewhere, vec![idx(&t, "/opt/tool")]);
        assert_eq!(t.top, vec![idx(&t, "/w/a")]);
        assert!(t.gone.is_empty());
    }

    #[test]
    fn gone_claude_worktree_attaches_to_its_repo() {
        let old = "/w/inv/.claude/worktrees/old";
        let stray = "/w/other/.claude/worktrees/x";
        let probe = FakeProbe::new().repo("/w/inv");
        let sessions = [
            typed(old, 30),
            typed("/w/inv", 10),
            typed(old, 20),
            typed(stray, 5),
        ];
        let t = build(&sessions, &[], &probe);
        let inv_i = idx(&t, "/w/inv");
        let w = get(&t, old);
        assert_eq!(w.kind, Kind::Worktree);
        assert!(!w.exists);
        assert_eq!(w.parent, Some(inv_i));
        assert_eq!(w.name, "old");
        assert_eq!(w.sessions, vec![r(0, false), r(2, false)]);
        assert_eq!(
            t.projects[inv_i].sessions,
            vec![r(0, false), r(2, false), r(1, false)]
        );
        assert_eq!(t.projects[inv_i].last_activity, Some(30));
        let s = idx(&t, stray);
        assert!(!t.projects[s].exists);
        assert_eq!(t.gone, vec![s]);
        assert_eq!(t.top, vec![inv_i]);
    }

    #[test]
    fn gone_herdr_worktree_attaches_by_unique_name_else_gone() {
        let probe = FakeProbe::new()
            .repo("/w/inv")
            .repo("/w/dup")
            .repo("/opt/dup");
        let a = "/h/.herdr/worktrees/inv/a";
        let b = "/h/.herdr/worktrees/dup/b";
        let c = "/h/.herdr/worktrees/none/c";
        let t = build(
            &[typed(a, 3), typed(b, 2), typed(c, 1)],
            &["/w/inv", "/w/dup", "/opt/dup"],
            &probe,
        );
        let wa = get(&t, a);
        assert_eq!(wa.kind, Kind::Worktree);
        assert_eq!(wa.parent, Some(idx(&t, "/w/inv")));
        assert!(!wa.exists);
        assert_eq!(t.gone, vec![idx(&t, b), idx(&t, c)]);
        assert_eq!(get(&t, b).parent, None);
    }

    #[test]
    fn headless_only_folder_creates_nothing() {
        let probe = FakeProbe::new()
            .dir("/w/notes")
            .repo("/w/inv")
            .dir("/w/inv/app");
        let t = build(&[headless("/w/notes", 1)], &[], &probe);
        assert!(t.projects.is_empty());

        let t = build(&[headless("/w/inv/app", 1)], &["/w/inv"], &probe);
        assert_eq!(t.projects.len(), 1);
        assert_eq!(get(&t, "/w/inv").sessions, vec![r(0, false)]);
    }

    #[test]
    fn headless_session_does_not_move_last_activity() {
        let probe = FakeProbe::new().repo("/w/inv");
        let t = build(
            &[typed("/w/inv", 100), headless("/w/inv", 500)],
            &[],
            &probe,
        );
        let inv = get(&t, "/w/inv");
        assert_eq!(inv.last_activity, Some(100));
        assert_eq!(inv.sessions, vec![r(1, false), r(0, false)]);
    }

    #[test]
    fn hidden_path_is_left_out() {
        let probe = FakeProbe::new()
            .dir("/w/secret")
            .repo("/w/secret/x")
            .repo("/w/inv");
        let mut c = cfg();
        c.hide = vec!["/w/secret".into()];
        let sessions = [
            typed("/w/secret/x", 1),
            session("/w/inv", &["/w/secret/x"], 2, true),
        ];
        let t = build_tree(
            &sessions,
            &paths(&["/w/secret", "/w/inv"]),
            &c,
            Path::new(HOME),
            &probe,
        );
        assert!(t.projects.iter().all(|p| !p.path.starts_with("/w/secret")));
        assert_eq!(t.projects.len(), 1);
        assert_eq!(get(&t, "/w/inv").sessions, vec![r(1, false)]);
    }

    #[test]
    fn lists_sort_by_last_activity_then_name() {
        let probe = FakeProbe::new()
            .dir("/w/b")
            .dir("/w/a")
            .dir("/w/c")
            .dir("/w/d");
        let t = build(
            &[typed("/w/c", 5), typed("/w/d", 9)],
            &["/w/a", "/w/b"],
            &probe,
        );
        let order: Vec<&str> = t.top.iter().map(|&i| t.projects[i].name.as_str()).collect();
        assert_eq!(order, vec!["d", "c", "a", "b"]);
        assert_eq!(get(&t, "/w/a").last_activity, None);
    }

    #[test]
    fn open_dir_falls_back_to_existing_ancestor() {
        let gone = "/w/repo/.claude/worktrees/x";
        let probe = FakeProbe::new()
            .repo("/w/repo")
            .dir("/w/repo/.claude/worktrees")
            .dir("/w/a");
        let sessions = [typed(gone, 1), typed("/w/a/b/c", 2), typed("/w/a", 3)];
        let t = build(&sessions, &[], &probe);
        assert_eq!(t.open_dir(&sessions[0], &probe), PathBuf::from("/w/repo"));
        assert_eq!(t.open_dir(&sessions[1], &probe), PathBuf::from("/w/a"));
        assert_eq!(t.open_dir(&sessions[2], &probe), PathBuf::from("/w/a"));
    }

    #[test]
    fn a_parent_cycle_is_broken() {
        // A's worktree holds repo B, and B's worktree holds repo A.
        let (a, b) = ("/w/wt-b/a", "/w/wt-a/b");
        let probe = FakeProbe::new()
            .repo(a)
            .repo(b)
            .worktree("/w/wt-a", a)
            .worktree("/w/wt-b", b);
        let t = build(&[typed(a, 2), typed(b, 1)], &[], &probe);
        for start in 0..t.projects.len() {
            let mut seen = vec![start];
            let mut at = t.projects[start].parent;
            while let Some(p) = at {
                assert!(!seen.contains(&p), "cycle through {:?}", t.projects[p].path);
                seen.push(p);
                at = t.projects[p].parent;
            }
        }
        assert_eq!(t.projects.len(), 4);
        let shown = t.top.len() + t.elsewhere.len() + t.gone.len();
        assert!(shown >= 1, "a project of the cycle is a top row");
    }

    #[test]
    fn repo_info_is_asked_once_per_folder() {
        let probe = FakeProbe::new().repo("/w/inv").dir("/w/inv/app");
        let sessions: Vec<SessionMeta> = (0..5).map(|i| headless("/w/inv/app", i)).collect();
        build(&sessions, &["/w/inv"], &probe);
        let calls = probe.calls.lock().unwrap();
        assert!(calls.values().all(|&n| n == 1), "{calls:?}");
    }

    #[test]
    fn real_probe_sees_a_linked_worktree() {
        use crate::git::testgit::{canon, git, init_repo};
        let tmp = tempfile::tempdir().unwrap();
        let base = canon(tmp.path());
        let repo = base.join("repo");
        init_repo(&repo);
        git(&repo, &["worktree", "add", "../wt", "-b", "feat"]);
        let wt = base.join("wt");
        let mut s = typed("/", 1);
        s.home_cwd = Some(wt.clone());
        s.cwds = vec![wt.clone()];
        let c = Config {
            roots: vec![base.clone()],
            depth: 2,
            hide: Vec::new(),
        };
        let homes = vec![wt.clone()];
        let scanned = crate::scan::scan(&c, &homes, &RealProbe);
        assert!(
            scanned.contains(&repo) && scanned.contains(&wt),
            "{scanned:?}"
        );
        let t = build_tree(
            &[s],
            &scanned,
            &c,
            Path::new("/nonexistent-home"),
            &RealProbe,
        );
        let ri = t
            .projects
            .iter()
            .position(|p| canon(&p.path) == repo)
            .unwrap();
        let wi = t
            .projects
            .iter()
            .position(|p| canon(&p.path) == wt)
            .unwrap();
        assert_eq!(t.projects[ri].kind, Kind::Repo);
        assert_eq!(t.projects[wi].kind, Kind::Worktree);
        assert_eq!(t.projects[wi].parent, Some(ri));
        assert_eq!(t.projects[ri].children, vec![wi]);
        assert_eq!(t.projects[ri].sessions, vec![r(0, false)]);
        assert_eq!(t.top, vec![ri]);
    }
}
