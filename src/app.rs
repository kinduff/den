//! The picker's state and key handling. No terminal, clock, environment or
//! tmux here: `now` is passed in and the file system is reached only through
//! the `Probe` given to `on_key`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::config::contract_tilde;

use crate::git::GitStatus;
use crate::project::{Probe, Project, SessionRef, Tree, claude_worktree_repo};
use crate::session::SessionMeta;
use crate::tmux::{OpenReq, What};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Projects,
    Sessions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Activity,
    Modified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Open(OpenReq),
    Detach,
}

/// Where the user is: kept across a refresh, a sort or a filter change.
struct Place {
    project: Option<PathBuf>,
    session: Option<String>,
    /// The projects of the rows above the selected one, nearest last.
    above: Vec<PathBuf>,
}

/// One line of the projects list. `header` is "elsewhere" or "gone" on the
/// first row of those lists, drawn as a separator above the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub project: usize,
    pub depth: u8,
    pub header: Option<&'static str>,
}

pub struct App {
    pub tree: Tree,
    pub sessions: Vec<SessionMeta>,
    pub status: HashMap<PathBuf, GitStatus>,
    pub modified: HashMap<PathBuf, i64>,
    pub open_dirs: Vec<PathBuf>,
    pub expanded: HashSet<PathBuf>,
    pub focus: Focus,
    pub sort: Sort,
    pub show_all: bool,
    pub filter: String,
    pub filtering: bool,
    pub project_cursor: usize,
    pub session_cursor: usize,
    /// Progress line, "root picked: ~/www", errors.
    pub notice: Option<String>,
    pub now: i64,
    /// The user's home folder, drawn as `~`. Empty draws paths in full.
    pub home: PathBuf,
    /// The tree was built without git: repo facts are not known yet.
    pub provisional: bool,
}

impl App {
    pub fn new(now: i64) -> App {
        App {
            tree: Tree::default(),
            sessions: Vec::new(),
            status: HashMap::new(),
            modified: HashMap::new(),
            open_dirs: Vec::new(),
            expanded: HashSet::new(),
            focus: Focus::Projects,
            sort: Sort::Activity,
            show_all: false,
            filter: String::new(),
            filtering: false,
            project_cursor: 0,
            session_cursor: 0,
            notice: None,
            now,
            home: PathBuf::new(),
            provisional: false,
        }
    }

    /// Replaces the data, keeping the selected project by path and the
    /// selected session by id.
    pub fn set_data(&mut self, tree: Tree, sessions: Vec<SessionMeta>) {
        let place = self.place();
        self.tree = tree;
        self.sessions = sessions;
        self.restore(place);
    }

    pub fn rows(&self) -> Vec<Row> {
        let visible = self.filter_visible();
        let mut out = Vec::new();
        let lists: [(&[usize], Option<&'static str>); 3] = [
            (&self.tree.top, None),
            (&self.tree.elsewhere, Some("elsewhere")),
            (&self.tree.gone, Some("gone")),
        ];
        for (list, header) in lists {
            let start = out.len();
            for i in self.sorted(list) {
                self.walk(i, 0, visible.as_ref(), &mut out);
            }
            if let Some(first) = out.get_mut(start) {
                first.header = header;
            }
        }
        out
    }

    /// Sessions of the selected project, newest first; headless ones left
    /// out unless `show_all`. A filter that the project itself does not
    /// match narrows the list to sessions whose label matches.
    pub fn session_rows(&self) -> Vec<SessionRef> {
        let Some(p) = self.selected() else {
            return Vec::new();
        };
        let project = &self.tree.projects[p];
        let narrow = !self.filter.is_empty() && !self.project_text_matches(project);
        project
            .sessions
            .iter()
            .filter(|r| self.session_shown(r))
            .filter(|r| !narrow || self.label_matches(r))
            .cloned()
            .collect()
    }

    pub fn on_key(&mut self, key: KeyEvent, probe: &dyn Probe) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Detach;
        }
        if matches!(key.code, KeyCode::Char(_)) && !(key.modifiers - KeyModifiers::SHIFT).is_empty()
        {
            return Action::None;
        }
        if self.filtering {
            return self.on_filter_key(key);
        }
        match key.code {
            KeyCode::Char('q') => return Action::Detach,
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Projects => Focus::Sessions,
                    Focus::Sessions => Focus::Projects,
                };
            }
            KeyCode::Char('l') | KeyCode::Right => self.expand(),
            KeyCode::Char('h') | KeyCode::Left => self.collapse(),
            KeyCode::Enter => return self.enter(probe),
            KeyCode::Char('n') => return self.open_here(What::NewClaude, probe),
            KeyCode::Char('t') => return self.open_here(What::Shell, probe),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                let place = self.place();
                self.filter.clear();
                self.restore(place);
            }
            KeyCode::Char('s') => {
                let place = self.place();
                self.sort = match self.sort {
                    Sort::Activity => Sort::Modified,
                    Sort::Modified => Sort::Activity,
                };
                self.restore(place);
            }
            KeyCode::Char('a') => {
                let place = self.place();
                self.show_all = !self.show_all;
                self.restore(place);
            }
            _ => {}
        }
        Action::None
    }

    /// The selected project's index into `tree.projects`.
    pub fn selected(&self) -> Option<usize> {
        self.rows().get(self.project_cursor).map(|r| r.project)
    }

    /// Ancestors' names and the project's own, joined with '/'.
    pub fn display_name(&self, i: usize) -> String {
        let ps = &self.tree.projects;
        let mut parts = vec![ps[i].name.as_str()];
        let mut at = ps[i].parent;
        while let Some(p) = at {
            parts.push(ps[p].name.as_str());
            at = ps[p].parent;
        }
        parts.reverse();
        parts.join("/")
    }

    fn walk(&self, i: usize, depth: u8, visible: Option<&HashSet<usize>>, out: &mut Vec<Row>) {
        if visible.is_some_and(|v| !v.contains(&i)) {
            return;
        }
        out.push(Row {
            project: i,
            depth,
            header: None,
        });
        let p = &self.tree.projects[i];
        let open = match visible {
            Some(v) => p.children.iter().any(|c| v.contains(c)),
            None => self.expanded.contains(&p.path),
        };
        if open {
            for c in self.sorted(&p.children) {
                self.walk(c, depth.saturating_add(1), visible, out);
            }
        }
    }

    /// Siblings in the current sort order, newest first, then by name.
    fn sorted(&self, list: &[usize]) -> Vec<usize> {
        let ps = &self.tree.projects;
        let key = |i: usize| match self.sort {
            Sort::Activity => ps[i].last_activity,
            Sort::Modified => self.modified.get(&ps[i].path).copied(),
        };
        let mut v = list.to_vec();
        v.sort_by(|&a, &b| newest_first(key(a), key(b)).then_with(|| ps[a].name.cmp(&ps[b].name)));
        v
    }

    /// With a filter: the projects that match and their ancestors.
    fn filter_visible(&self) -> Option<HashSet<usize>> {
        if self.filter.is_empty() {
            return None;
        }
        let mut keep = HashSet::new();
        for (i, p) in self.tree.projects.iter().enumerate() {
            let hit = self.project_text_matches(p)
                || p.sessions
                    .iter()
                    .any(|r| self.session_shown(r) && self.label_matches(r));
            if hit {
                let mut at = Some(i);
                while let Some(j) = at {
                    if !keep.insert(j) {
                        break;
                    }
                    at = self.tree.projects[j].parent;
                }
            }
        }
        Some(keep)
    }

    fn needle(&self) -> String {
        self.filter.to_lowercase()
    }

    fn project_text_matches(&self, p: &Project) -> bool {
        let n = self.needle();
        p.name.to_lowercase().contains(&n) || p.path.to_string_lossy().to_lowercase().contains(&n)
    }

    fn label_matches(&self, r: &SessionRef) -> bool {
        self.sessions
            .get(r.idx)
            .is_some_and(|s| s.label().to_lowercase().contains(&self.needle()))
    }

    fn session_shown(&self, r: &SessionRef) -> bool {
        self.sessions
            .get(r.idx)
            .is_some_and(|s| self.show_all || s.is_typed())
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selected().map(|i| self.tree.projects[i].path.clone())
    }

    /// The visible row of the project at `path`.
    fn row_of(&self, path: &Path) -> Option<usize> {
        self.rows()
            .iter()
            .position(|r| self.tree.projects[r.project].path == path)
    }

    /// The selected project's path and the selected session's id.
    fn place(&self) -> Place {
        let session = self
            .session_rows()
            .get(self.session_cursor)
            .and_then(|r| self.sessions.get(r.idx))
            .map(|s| s.id.clone());
        let rows = self.rows();
        let above = rows[..self.project_cursor.min(rows.len())]
            .iter()
            .map(|r| self.tree.projects[r.project].path.clone())
            .collect();
        Place {
            project: self.selected_path(),
            session,
            above,
        }
    }

    /// Puts the cursors back on `place` after the rows changed. A project
    /// that moved under a collapsed parent has its ancestors opened. A
    /// different project ends up selected only when `place`'s is gone; its
    /// session cursor then starts at the top.
    fn restore(&mut self, place: Place) {
        if let Some(path) = &place.project {
            if self.filter.is_empty()
                && self.row_of(path).is_none()
                && let Some(i) = self.tree.projects.iter().position(|p| &p.path == path)
            {
                let mut at = self.tree.projects[i].parent;
                while let Some(j) = at {
                    self.expanded.insert(self.tree.projects[j].path.clone());
                    at = self.tree.projects[j].parent;
                }
            }
            if let Some(pos) = self.row_of(path) {
                self.project_cursor = pos;
            } else {
                // Gone from the list: the nearest surviving row above it,
                // with the projects pane focused so a queued `enter` cannot
                // open an unrelated session.
                self.project_cursor = place
                    .above
                    .iter()
                    .rev()
                    .find_map(|p| self.row_of(p))
                    .unwrap_or(0);
                self.focus = Focus::Projects;
            }
        }
        self.clamp();
        if self.selected_path() != place.project {
            self.session_cursor = 0;
        } else if let Some(id) = &place.session
            && let Some(pos) = self
                .session_rows()
                .iter()
                .position(|r| self.sessions.get(r.idx).is_some_and(|s| &s.id == id))
        {
            self.session_cursor = pos;
        }
    }

    fn clamp(&mut self) {
        let rows = self.rows().len();
        self.project_cursor = self.project_cursor.min(rows.saturating_sub(1));
        let sessions = self.session_rows().len();
        self.session_cursor = self.session_cursor.min(sessions.saturating_sub(1));
    }

    fn move_cursor(&mut self, by: isize) {
        match self.focus {
            Focus::Projects => {
                let before = self.project_cursor;
                self.project_cursor = self.project_cursor.saturating_add_signed(by);
                self.clamp();
                if self.project_cursor != before {
                    self.session_cursor = 0;
                }
            }
            Focus::Sessions => {
                self.session_cursor = self.session_cursor.saturating_add_signed(by);
                self.clamp();
            }
        }
    }

    fn expand(&mut self) {
        if self.focus != Focus::Projects {
            return;
        }
        if let Some(i) = self.selected()
            && !self.tree.projects[i].children.is_empty()
        {
            self.expanded.insert(self.tree.projects[i].path.clone());
        }
    }

    /// Collapses an open row; on a closed one moves to its parent.
    fn collapse(&mut self) {
        if self.focus != Focus::Projects {
            return;
        }
        let Some(i) = self.selected() else {
            return;
        };
        let p = &self.tree.projects[i];
        if self.expanded.remove(&p.path) && self.filter.is_empty() {
            self.clamp();
            return;
        }
        if let Some(parent) = p.parent
            && let Some(pos) = self.rows().iter().position(|r| r.project == parent)
        {
            self.project_cursor = pos;
            self.session_cursor = 0;
        }
        self.clamp();
    }

    fn enter(&mut self, probe: &dyn Probe) -> Action {
        let Some(p) = self.selected() else {
            return Action::None;
        };
        let session = match self.focus {
            Focus::Sessions => match self.session_rows().get(self.session_cursor) {
                Some(r) => r.idx,
                None => return Action::None,
            },
            Focus::Projects => {
                let typed =
                    |r: &&SessionRef| self.sessions.get(r.idx).is_some_and(|s| s.is_typed());
                // A filter can narrow the sessions pane: take the newest one
                // on screen, never one the filter hides. An `also` session
                // only when nothing else is shown.
                let newest = if self.filter.is_empty() {
                    self.tree.projects[p]
                        .sessions
                        .iter()
                        .filter(|r| !r.also)
                        .find(typed)
                        .map(|r| r.idx)
                } else {
                    let shown = self.session_rows();
                    let own = shown.iter().filter(|r| !r.also).find(typed);
                    own.or_else(|| shown.iter().find(typed)).map(|r| r.idx)
                };
                match newest {
                    Some(idx) => idx,
                    None => return self.open_here(What::NewClaude, probe),
                }
            }
        };
        let s = &self.sessions[session];
        let own = self.own_project(session).unwrap_or(p);
        Action::Open(OpenReq {
            dir: self.tree.open_dir(s, probe),
            name: self.tab_name(own),
            what: What::Resume(s.id.clone()),
        })
    }

    /// The deepest project that lists session `idx` as its own (a worktree
    /// rather than the repo its sessions also join).
    fn own_project(&self, idx: usize) -> Option<usize> {
        let depth = |mut i: usize| {
            let mut d = 0;
            while let Some(p) = self.tree.projects[i].parent {
                d += 1;
                i = p;
            }
            d
        };
        self.tree
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| p.sessions.iter().any(|r| r.idx == idx && !r.also))
            .map(|(i, _)| i)
            .max_by_key(|&i| depth(i))
    }

    /// The window name for project `i`: its display name, with a worktree
    /// that a git-free tree shows as `.claude/worktrees/<name>` written as
    /// `<repo>/<name>`.
    fn tab_name(&self, i: usize) -> String {
        let p = &self.tree.projects[i];
        if let Some(parent) = p.parent
            && claude_worktree_repo(&p.path) == Some(self.tree.projects[parent].path.as_path())
            && let Some(name) = p.path.file_name()
        {
            return format!("{}/{}", self.display_name(parent), name.to_string_lossy());
        }
        self.display_name(i)
    }

    /// Opens `what` in the selected project's folder, or for a folder that
    /// is gone, in the nearest existing one (as resuming would).
    fn open_here(&self, what: What, probe: &dyn Probe) -> Action {
        match self.selected() {
            Some(p) => Action::Open(OpenReq {
                dir: self.tree.dir_to_open(&self.tree.projects[p].path, probe),
                name: self.tab_name(p),
                what,
            }),
            None => Action::None,
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) -> Action {
        let place = self.place();
        match key.code {
            KeyCode::Char(c) => self.filter.push(c),
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
            }
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            _ => {}
        }
        self.restore(place);
        Action::None
    }
}

/// Where a session lives relative to project `p`: "." for its folder, a
/// worktree or subfolder name below it, else the home path itself, with
/// the user's home folder `user_home` written as `~`.
pub fn session_place(tree: &Tree, p: usize, s: &SessionMeta, user_home: &Path) -> String {
    let Some(home) = s.home_cwd.as_deref() else {
        return String::new();
    };
    let project = &tree.projects[p];
    for &c in &project.children {
        let child = &tree.projects[c];
        if let Ok(rest) = home.strip_prefix(&child.path) {
            return if rest.as_os_str().is_empty() {
                child.name.clone()
            } else {
                format!("{}/{}", child.name, rest.display())
            };
        }
    }
    match home.strip_prefix(&project.path) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".into(),
        Ok(rest) => rest.display().to_string(),
        Err(_) => contract_tilde(home, user_home).display().to_string(),
    }
}

/// "now" (<60s), "5m ago", "2h ago", "3d ago", "4w ago", "never".
pub fn ago(now: i64, then: Option<i64>) -> String {
    let Some(then) = then else {
        return "never".into();
    };
    let d = now.saturating_sub(then);
    match d {
        ..60 => "now".into(),
        60..3600 => format!("{}m ago", d / 60),
        3600..86_400 => format!("{}h ago", d / 3600),
        86_400..604_800 => format!("{}d ago", d / 86_400),
        _ => format!("{}w ago", d / 604_800),
    }
}

fn newest_first(a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
pub mod fixture {
    //! A small hand-built tree shared by the app and view tests.
    use std::path::{Path, PathBuf};

    use super::App;
    use crate::config::Config;
    use crate::project::build_tree;
    use crate::project::testkit::FakeProbe;
    use crate::session::SessionMeta;

    pub const NOW: i64 = 1_000_000_000;

    pub fn session(id: &str, home: &str, ago: i64, typed: bool, title: &str) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            file: PathBuf::from(format!("/p/{id}.jsonl")),
            size: 0,
            mtime: 0,
            entrypoint: Some(if typed { "cli" } else { "sdk-cli" }.into()),
            home_cwd: Some(PathBuf::from(home)),
            cwds: vec![PathBuf::from(home)],
            first_ts: Some(NOW - ago),
            last_ts: Some(NOW - ago),
            branch: None,
            title: Some(title.into()),
            last_prompt: None,
        }
    }

    pub const INV: &str = "/w/inventory";
    pub const SCHED: &str = "/w/inventory/.claude/worktrees/count-schedule";
    pub const LAST: &str = "/w/inventory/.claude/worktrees/count-last-scan";
    pub const GONE_WT: &str = "/w/inventory/.claude/worktrees/old";

    pub fn probe() -> FakeProbe {
        FakeProbe::new()
            .repo(INV)
            .worktree(SCHED, INV)
            .worktree(LAST, INV)
            .repo("/w/bongo-cli")
            .dir("/w/lab")
            .repo("/w/lab/stardew-zombies")
            .repo("/w/kpis")
            .dir("/opt/tool")
    }

    pub fn sessions() -> Vec<SessionMeta> {
        let mut also = session("also1", "/w", 5 * 86_400, true, "Reimpresión de etiquetas");
        also.cwds.push(PathBuf::from(INV));
        vec![
            session("sched1", SCHED, 2 * 3600, true, "Fix the schedule count"),
            session("inv1", INV, 3 * 86_400, true, "Why is last scan empty"),
            also,
            session("bongo1", "/w/bongo-cli", 86_400, true, "Cluster request"),
            session("lab1", "/w/lab", 30, true, "Lab notes"),
            session(
                "zomb1",
                "/w/lab/stardew-zombies",
                86_400,
                true,
                "Zombies spawn",
            ),
            session("auto1", INV, 60, false, "Headless run"),
            session("gone1", GONE_WT, 10 * 86_400, true, "Old worktree work"),
            session("tool1", "/opt/tool", 20 * 86_400, true, "Tool thing"),
            session("lost1", "/w/vanished", 30 * 86_400, true, "Vanished folder"),
        ]
    }

    pub fn app() -> (App, FakeProbe) {
        let probe = probe();
        let sessions = sessions();
        let cfg = Config {
            roots: vec!["/w".into()],
            depth: 2,
            hide: Vec::new(),
        };
        let scanned: Vec<PathBuf> = ["/w/kpis", "/w/lab", "/w/lab/stardew-zombies"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let tree = build_tree(&sessions, &scanned, &cfg, Path::new("/h"), &probe);
        let mut app = App::new(NOW);
        app.set_data(tree, sessions);
        (app, probe)
    }

    /// Moves the project cursor onto the row for `path`.
    pub fn select(app: &mut App, path: &str) {
        app.project_cursor = app
            .rows()
            .iter()
            .position(|r| app.tree.projects[r.project].path == Path::new(path))
            .unwrap_or_else(|| panic!("{path} not in rows"));
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use std::path::Path;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn names(app: &App) -> Vec<String> {
        app.rows()
            .iter()
            .map(|r| app.tree.projects[r.project].name.clone())
            .collect()
    }

    fn session_ids(app: &App) -> Vec<String> {
        app.session_rows()
            .iter()
            .map(|r| app.sessions[r.idx].id.clone())
            .collect()
    }

    fn open(dir: &str, name: &str, what: What) -> Action {
        Action::Open(OpenReq {
            dir: dir.into(),
            name: name.into(),
            what,
        })
    }

    #[test]
    fn enter_on_project_resumes_its_newest_typed_session() {
        let (mut app, probe) = app();
        select(&mut app, INV);
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &probe),
            open(
                SCHED,
                "inventory/count-schedule",
                What::Resume("sched1".into())
            ),
            "headless auto1 is newer but not typed; the tab names the worktree"
        );
    }

    #[test]
    fn enter_on_project_without_sessions_starts_a_new_one() {
        let (mut app, probe) = app();
        select(&mut app, "/w/kpis");
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &probe),
            open("/w/kpis", "kpis", What::NewClaude)
        );
    }

    #[test]
    fn enter_on_session_of_a_gone_folder_opens_in_the_parent() {
        let (mut app, probe) = app();
        select(&mut app, INV);
        app.on_key(key(KeyCode::Tab), &probe);
        let pos = session_ids(&app).iter().position(|s| s == "gone1").unwrap();
        for _ in 0..pos {
            app.on_key(ch('j'), &probe);
        }
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &probe),
            open(INV, "inventory/old", What::Resume("gone1".into()))
        );

        select(&mut app, "/w/vanished");
        app.session_cursor = 0;
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &probe),
            open("/w", "vanished", What::Resume("lost1".into())),
            "a project in gone still opens, from the nearest existing folder"
        );
    }

    #[test]
    fn n_and_t_open_claude_and_shell_in_the_project_folder() {
        let (mut app, probe) = app();
        select(&mut app, "/w/lab");
        app.on_key(ch('l'), &probe);
        select(&mut app, "/w/lab/stardew-zombies");
        let dir = "/w/lab/stardew-zombies";
        assert_eq!(
            app.on_key(ch('n'), &probe),
            open(dir, "lab/stardew-zombies", What::NewClaude)
        );
        assert_eq!(
            app.on_key(ch('t'), &probe),
            open(dir, "lab/stardew-zombies", What::Shell)
        );
    }

    #[test]
    fn enter_on_a_filtered_project_resumes_a_session_on_screen() {
        let (mut app, probe) = app();
        app.on_key(ch('/'), &probe);
        for c in "why is".chars() {
            app.on_key(ch(c), &probe);
        }
        app.on_key(key(KeyCode::Enter), &probe);
        select(&mut app, INV);
        assert_eq!(session_ids(&app), vec!["inv1"]);
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &probe),
            open(INV, "inventory", What::Resume("inv1".into()))
        );
    }

    #[test]
    fn n_and_t_on_a_gone_folder_open_in_the_nearest_existing_one() {
        let (mut app, probe) = app();
        select(&mut app, "/w/vanished");
        assert_eq!(
            app.on_key(ch('n'), &probe),
            open("/w", "vanished", What::NewClaude)
        );
        select(&mut app, INV);
        app.on_key(ch('l'), &probe);
        select(&mut app, GONE_WT);
        assert_eq!(
            app.on_key(ch('t'), &probe),
            open(INV, "inventory/old", What::Shell),
            "a gone worktree opens in its repo"
        );
    }

    #[test]
    fn tab_from_a_provisional_tree_names_the_worktree() {
        // Built without git: the worktree is a plain folder under the repo.
        let plain = crate::project::testkit::FakeProbe::new()
            .dir(INV)
            .dir(SCHED);
        let mut early = sessions();
        early.retain(|s| s.id == "sched1" || s.id == "inv1");
        let tree = fixture_tree(&early, &plain);
        let mut app = App::new(NOW);
        app.set_data(tree, early);
        select(&mut app, INV);
        app.on_key(ch('l'), &plain);
        select(&mut app, SCHED);
        assert_eq!(
            app.tree.projects[app.selected().unwrap()].name,
            ".claude/worktrees/count-schedule",
            "nested as a plain subfolder"
        );
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &plain),
            open(
                SCHED,
                "inventory/count-schedule",
                What::Resume("sched1".into())
            )
        );
    }

    #[test]
    fn esc_clears_a_kept_filter() {
        let (mut app, probe) = app();
        app.on_key(ch('/'), &probe);
        app.on_key(ch('z'), &probe);
        app.on_key(key(KeyCode::Enter), &probe);
        assert_eq!(app.filter, "z");
        app.on_key(key(KeyCode::Esc), &probe);
        assert!(app.filter.is_empty() && !app.filtering);
        assert_eq!(names(&app).len(), 7);
    }

    #[test]
    fn a_vanished_project_moves_the_cursor_to_the_row_above() {
        let (mut app, probe) = app();
        select(&mut app, "/w/kpis");
        let above = app.rows()[app.project_cursor - 1].project;
        let above = app.tree.projects[above].path.clone();
        app.on_key(key(KeyCode::Tab), &probe);
        let sessions = sessions();
        let cfg = crate::config::Config {
            roots: vec!["/w".into()],
            depth: 2,
            hide: Vec::new(),
        };
        let tree = crate::project::build_tree(
            &sessions,
            &["/w/lab".into(), "/w/lab/stardew-zombies".into()],
            &cfg,
            Path::new("/h"),
            &probe,
        );
        app.set_data(tree, sessions);
        assert_eq!(app.tree.projects[app.selected().unwrap()].path, above);
        assert_eq!(app.focus, Focus::Projects);
        assert_eq!(app.session_cursor, 0);
    }

    #[test]
    fn headless_sessions_hidden_until_a() {
        let (mut app, probe) = app();
        select(&mut app, INV);
        assert!(!session_ids(&app).contains(&"auto1".to_string()));
        app.on_key(ch('a'), &probe);
        assert_eq!(session_ids(&app)[0], "auto1");
        app.on_key(ch('a'), &probe);
        assert!(!session_ids(&app).contains(&"auto1".to_string()));
    }

    #[test]
    fn filter_keeps_matches_and_their_ancestors() {
        let (mut app, probe) = app();
        app.on_key(ch('/'), &probe);
        for c in "ZOMBIES".chars() {
            app.on_key(ch(c), &probe);
        }
        assert_eq!(app.filter, "ZOMBIES");
        assert_eq!(names(&app), vec!["lab", "stardew-zombies"]);
        assert_eq!(app.rows()[1].depth, 1, "ancestor shown expanded");
        app.on_key(key(KeyCode::Enter), &probe);
        assert!(!app.filtering);
        assert_eq!(names(&app), vec!["lab", "stardew-zombies"], "enter keeps");
        app.on_key(ch('/'), &probe);
        app.on_key(key(KeyCode::Backspace), &probe);
        assert_eq!(app.filter, "ZOMBIE");
        app.on_key(key(KeyCode::Esc), &probe);
        assert!(app.filter.is_empty() && !app.filtering);
        assert_eq!(
            names(&app).len(),
            7,
            "all collapsed again: {:?}",
            names(&app)
        );
    }

    #[test]
    fn filter_matches_session_titles() {
        let (mut app, probe) = app();
        app.on_key(ch('/'), &probe);
        for c in "reimpres".chars() {
            app.on_key(ch(c), &probe);
        }
        let shown = names(&app);
        assert!(shown.contains(&"inventory".to_string()), "{shown:?}");
        assert!(shown.contains(&"w".to_string()), "{shown:?}");
        assert!(!shown.contains(&"kpis".to_string()));
        select(&mut app, INV);
        assert_eq!(session_ids(&app), vec!["also1"]);
    }

    #[test]
    fn filtering_takes_every_printable_key() {
        let (mut app, probe) = app();
        app.on_key(ch('/'), &probe);
        for c in "qntasjk".chars() {
            assert_eq!(app.on_key(ch(c), &probe), Action::None);
        }
        assert_eq!(app.filter, "qntasjk");
        assert!(app.filtering && !app.show_all && app.sort == Sort::Activity);
    }

    #[test]
    fn collapse_hides_children_and_cursor_stays_in_range() {
        let (mut app, probe) = app();
        select(&mut app, INV);
        app.on_key(key(KeyCode::Right), &probe);
        let open = names(&app);
        let at = open.iter().position(|n| n == "inventory").unwrap();
        assert_eq!(
            &open[at + 1..at + 4],
            &["count-schedule", "old", "count-last-scan"]
        );
        for _ in 0..2 {
            app.on_key(ch('j'), &probe);
        }
        app.on_key(key(KeyCode::Left), &probe);
        assert_eq!(
            app.selected().map(|i| app.tree.projects[i].name.as_str()),
            Some("inventory")
        );
        app.on_key(ch('h'), &probe);
        assert_eq!(names(&app).len(), open.len() - 3);
        assert!(app.project_cursor < app.rows().len());

        select(&mut app, "/w/vanished");
        for _ in 0..3 {
            app.on_key(ch('j'), &probe);
        }
        assert_eq!(app.project_cursor, app.rows().len() - 1);
    }

    #[test]
    fn rows_start_collapsed_with_headers() {
        let (app, _) = app();
        let rows = app.rows();
        assert!(rows.iter().all(|r| r.depth == 0));
        let headers: Vec<_> = rows.iter().filter_map(|r| r.header).collect();
        assert_eq!(headers, vec!["elsewhere", "gone"]);
        let tool = rows
            .iter()
            .position(|r| r.header == Some("elsewhere"))
            .unwrap();
        assert_eq!(
            app.tree.projects[rows[tool].project].path,
            Path::new("/opt/tool")
        );
    }

    #[test]
    fn sort_modified_reorders_siblings_only() {
        let (mut app, probe) = app();
        app.modified.insert("/w/kpis".into(), NOW);
        app.on_key(ch('s'), &probe);
        let n = names(&app);
        assert_eq!(n[0], "kpis");
        assert_eq!(&n[n.len() - 2..], &["tool", "vanished"]);
    }

    #[test]
    fn set_data_keeps_the_selected_project_by_path() {
        let (mut app, probe) = app();
        select(&mut app, "/w/kpis");
        let before = app.project_cursor;
        let mut sessions = sessions();
        sessions.push(session("kp", "/w/kpis", 10, true, "new one"));
        let cfg = crate::config::Config {
            roots: vec!["/w".into()],
            depth: 2,
            hide: Vec::new(),
        };
        let tree = crate::project::build_tree(
            &sessions,
            &["/w/kpis".into(), "/w/lab".into()],
            &cfg,
            Path::new("/h"),
            &probe,
        );
        app.set_data(tree, sessions);
        assert_ne!(app.project_cursor, before, "kpis moved to the top");
        assert_eq!(
            app.tree.projects[app.selected().unwrap()].path,
            Path::new("/w/kpis")
        );
    }

    fn fixture_tree(sessions: &[SessionMeta], probe: &dyn Probe) -> Tree {
        let cfg = crate::config::Config {
            roots: vec!["/w".into()],
            depth: 2,
            hide: Vec::new(),
        };
        let scanned: Vec<PathBuf> = ["/w/kpis", "/w/lab", "/w/lab/stardew-zombies"]
            .iter()
            .map(PathBuf::from)
            .collect();
        crate::project::build_tree(sessions, &scanned, &cfg, Path::new("/h"), probe)
    }

    fn selected_session(app: &App) -> String {
        let rows = app.session_rows();
        app.sessions[rows[app.session_cursor].idx].id.clone()
    }

    #[test]
    fn set_data_keeps_the_selected_session_by_id() {
        let (mut app, probe) = app();
        select(&mut app, INV);
        app.on_key(key(KeyCode::Tab), &probe);
        app.on_key(ch('j'), &probe);
        assert_eq!(selected_session(&app), "inv1");
        let mut sessions = sessions();
        sessions.insert(0, session("inv2", INV, 10, true, "Newer one"));
        let tree = fixture_tree(&sessions, &probe);
        app.set_data(tree, sessions);
        assert_eq!(session_ids(&app)[0], "inv2", "the new session is on top");
        assert_eq!(selected_session(&app), "inv1");

        // Showing headless sessions inserts rows above; the highlight stays.
        app.on_key(ch('a'), &probe);
        assert_eq!(selected_session(&app), "inv1");
    }

    #[test]
    fn set_data_keeps_a_selected_project_that_moved_under_a_collapsed_parent() {
        // A git-free first tree: the worktree is a plain top-level folder.
        let plain = crate::project::testkit::FakeProbe::new().dir(SCHED);
        let mut early = sessions();
        early.retain(|s| s.id == "sched1");
        early.push(session("sched2", SCHED, 3 * 3600, true, "Older one"));
        let cfg = crate::config::Config {
            roots: vec!["/w".into()],
            depth: 2,
            hide: Vec::new(),
        };
        let tree = crate::project::build_tree(&early, &[], &cfg, Path::new("/h"), &plain);
        let probe = probe();
        let mut app = App::new(NOW);
        app.set_data(tree, early);
        select(&mut app, SCHED);
        app.on_key(key(KeyCode::Tab), &probe);
        app.on_key(ch('j'), &probe);
        assert_eq!(selected_session(&app), "sched2");

        // The real tree nests it under inventory, which is collapsed.
        let mut real = sessions();
        real.push(session("sched2", SCHED, 3 * 3600, true, "Older one"));
        let tree = fixture_tree(&real, &probe);
        app.set_data(tree, real);
        let at = app.selected().unwrap();
        assert_eq!(app.tree.projects[at].path, Path::new(SCHED));
        assert!(app.expanded.contains(Path::new(INV)), "its parent opened");
        assert_eq!(app.focus, Focus::Sessions);
        assert_eq!(selected_session(&app), "sched2");
    }

    #[test]
    fn changing_project_resets_the_session_cursor() {
        let (mut app, probe) = app();
        select(&mut app, INV);
        app.on_key(key(KeyCode::Tab), &probe);
        app.on_key(ch('j'), &probe);
        assert_eq!(app.session_cursor, 1);
        // inventory goes away; the cursor lands on another project with
        // two sessions, and its session cursor starts at the top.
        let mut sessions: Vec<SessionMeta> = sessions()
            .into_iter()
            .filter(|s| {
                ![INV, SCHED, GONE_WT].contains(&s.home_cwd.as_deref().unwrap().to_str().unwrap())
            })
            .filter(|s| s.id != "also1")
            .collect();
        for p in [
            "/w/lab",
            "/w/bongo-cli",
            "/w/kpis",
            "/w/lab/stardew-zombies",
        ] {
            sessions.push(session(&format!("{p}-x"), p, 100_000, true, "Extra"));
        }
        let tree = fixture_tree(&sessions, &probe);
        app.set_data(tree, sessions);
        let now = &app.tree.projects[app.selected().unwrap()];
        assert_ne!(now.path, Path::new(INV));
        assert!(app.session_rows().len() >= 2, "{}", now.name);
        assert_eq!(app.session_cursor, 0);
    }

    #[test]
    fn modified_and_released_keys_are_ignored() {
        use crossterm::event::KeyEventKind;
        let (mut app, probe) = app();
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(app.on_key(release, &probe), Action::None);
        let repeat =
            KeyEvent::new_with_kind(KeyCode::Char('q'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(app.on_key(repeat, &probe), Action::None);
        for m in [
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
        ] {
            assert_eq!(
                app.on_key(KeyEvent::new(KeyCode::Char('q'), m), &probe),
                Action::None,
                "{m:?}-q"
            );
            assert_eq!(
                app.on_key(KeyEvent::new(KeyCode::Char('n'), m), &probe),
                Action::None,
                "{m:?}-n"
            );
        }
        let shift_q = KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT);
        assert_eq!(app.on_key(shift_q, &probe), Action::None);
        app.on_key(ch('/'), &probe);
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), &probe);
        app.on_key(
            KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT),
            &probe,
        );
        assert_eq!(app.filter, "Z", "Alt-x typed nothing, Shift-z typed Z");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_c, &probe), Action::Detach);
    }

    #[test]
    fn empty_tree_does_not_panic_on_any_key() {
        let probe = crate::project::testkit::FakeProbe::new();
        let mut app = App::new(NOW);
        let keys = [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Tab,
            KeyCode::Right,
            KeyCode::Left,
            KeyCode::Char('l'),
            KeyCode::Char('h'),
            KeyCode::Enter,
            KeyCode::Char('n'),
            KeyCode::Char('t'),
            KeyCode::Char('s'),
            KeyCode::Char('a'),
            KeyCode::Char('/'),
            KeyCode::Char('x'),
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::Enter,
            KeyCode::Char('j'),
        ];
        for k in keys {
            let a = app.on_key(key(k), &probe);
            assert!(matches!(a, Action::None), "{k:?} gave {a:?}");
        }
        assert_eq!(app.on_key(ch('q'), &probe), Action::Detach);
    }

    #[test]
    fn q_detaches() {
        let (mut app, probe) = app();
        assert_eq!(app.on_key(ch('q'), &probe), Action::Detach);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_c, &probe), Action::Detach);
        app.on_key(ch('/'), &probe);
        assert_eq!(app.on_key(ctrl_c, &probe), Action::Detach);
    }

    #[test]
    fn ago_buckets() {
        let n = 10_000_000;
        assert_eq!(ago(n, Some(n - 30)), "now");
        assert_eq!(ago(n, Some(n - 300)), "5m ago");
        assert_eq!(ago(n, Some(n - 7200)), "2h ago");
        assert_eq!(ago(n, Some(n - 3 * 86_400)), "3d ago");
        assert_eq!(ago(n, Some(n - 28 * 86_400)), "4w ago");
        assert_eq!(ago(n, None), "never");
        assert_eq!(ago(n, Some(n + 50)), "now", "a clock skew is not negative");
    }

    #[test]
    fn session_place_is_relative_to_the_project() {
        let (app, _) = app();
        let inv = app
            .tree
            .projects
            .iter()
            .position(|p| p.path == Path::new(INV))
            .unwrap();
        let place = |id: &str| {
            let s = app.sessions.iter().find(|s| s.id == id).unwrap();
            session_place(&app.tree, inv, s, Path::new("/h"))
        };
        assert_eq!(place("sched1"), "count-schedule");
        assert_eq!(place("inv1"), ".");
        assert_eq!(place("also1"), "/w");
        assert_eq!(place("gone1"), "old");
        let outside = session("out", "/h/www", 0, true, "x");
        assert_eq!(
            session_place(&app.tree, inv, &outside, Path::new("/h")),
            "~/www"
        );
        assert_eq!(
            session_place(&app.tree, inv, &outside, Path::new("")),
            "/h/www"
        );
    }
}
