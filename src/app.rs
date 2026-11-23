//! The picker's state and key handling. No terminal, clock, environment or
//! tmux here: `now` is passed in and the file system is reached only through
//! the `Probe` given to `on_key`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::activity;
use crate::config::contract_tilde;

use crate::git::GitStatus;
use crate::project::{Probe, Project, SessionRef, Tree, claude_worktree_repo};
use crate::session::SessionMeta;
use crate::tmux::{OpenReq, Tab, What};

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

/// One line of the projects list. `header` names the section the row
/// starts ("today", "elsewhere"), drawn as a separator above the row.
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
    /// The tabs open right now, window 0 left out.
    pub tabs: Vec<Tab>,
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
    /// A key has been pressed. Until then new data leaves the cursor on the
    /// top row instead of following whatever row was first in older data.
    pub touched: bool,
    /// The full list of keys is on screen; `?` toggles it.
    pub keys_shown: bool,
    /// The local zone, in seconds east of UTC: where a day starts.
    pub utc_offset: i64,
}

/// The numbers of the overview band.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overview {
    pub tabs: usize,
    /// Folders in the list that exist.
    pub projects: usize,
    /// Sessions a person typed in, and how many of those were used today
    /// and in the last seven days.
    pub sessions: usize,
    pub today: usize,
    pub week: usize,
    /// Sessions started by scripts and agents.
    pub auto: usize,
    /// Up to three top-level projects with the most sessions used today,
    /// their children's included.
    pub busiest: Vec<String>,
}

impl App {
    pub fn new(now: i64) -> App {
        App {
            tree: Tree::default(),
            sessions: Vec::new(),
            status: HashMap::new(),
            modified: HashMap::new(),
            tabs: Vec::new(),
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
            touched: false,
            keys_shown: false,
            utc_offset: 0,
        }
    }

    /// Replaces the data, keeping the selected project by path and the
    /// selected session by id.
    pub fn set_data(&mut self, tree: Tree, sessions: Vec<SessionMeta>) {
        let place = self.place();
        self.tree = tree;
        self.sessions = sessions;
        if self.touched {
            self.restore(place);
        } else {
            self.project_cursor = 0;
            self.session_cursor = 0;
        }
    }

    /// The visible lines of the projects list. The top list is cut into
    /// sections by the time it is sorted on; "elsewhere" and "gone" follow.
    pub fn rows(&self) -> Vec<Row> {
        let visible = self.filter_visible();
        let mut out = Vec::new();
        let mut section = None;
        for i in self.sorted(&self.tree.top) {
            let start = out.len();
            self.walk(i, 0, visible.as_ref(), &mut out);
            let name = self.section_of(i);
            if section != Some(name)
                && let Some(first) = out.get_mut(start)
            {
                first.header = Some(name);
                section = Some(name);
            }
        }
        let lists = [
            (&self.tree.elsewhere, "elsewhere"),
            (&self.tree.gone, "gone"),
        ];
        for (list, header) in lists {
            let start = out.len();
            for i in self.sorted(list) {
                self.walk(i, 0, visible.as_ref(), &mut out);
            }
            if let Some(first) = out.get_mut(start) {
                first.header = Some(header);
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
        self.touched = true;
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
            KeyCode::Char('?') => self.keys_shown = !self.keys_shown,
            KeyCode::Esc if self.keys_shown => self.keys_shown = false,
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

    /// The numbers of the tabs open in `path`, lowest first.
    pub fn tab_numbers(&self, path: &Path) -> Vec<u32> {
        let mut numbers: Vec<u32> = self
            .tabs
            .iter()
            .filter(|t| t.dir == path)
            .map(|t| t.index)
            .collect();
        numbers.sort_unstable();
        numbers
    }

    /// The number of the tab resuming session `id`.
    pub fn tab_of_session(&self, id: &str) -> Option<u32> {
        self.tabs
            .iter()
            .find(|t| t.sessions.iter().any(|s| s == id))
            .map(|t| t.index)
    }

    /// How many of project `p`'s sessions are listed, and how many are
    /// left out as started by scripts and agents.
    pub fn session_counts(&self, p: usize) -> (usize, usize) {
        let all = &self.tree.projects[p].sessions;
        let shown = all.iter().filter(|r| self.session_shown(r)).count();
        (shown, all.len() - shown)
    }

    /// Sessions of project `p` and of the projects below it, each once and
    /// only where the folder is the session's own: what the `last` column
    /// is made from.
    fn subtree_sessions(&self, p: usize) -> Vec<&SessionMeta> {
        let mut seen = HashSet::new();
        let mut todo = vec![p];
        let mut out = Vec::new();
        while let Some(i) = todo.pop() {
            let project = &self.tree.projects[i];
            todo.extend(&project.children);
            let own = project.sessions.iter().filter(|r| !r.also);
            out.extend(
                own.filter(|r| seen.insert(r.idx))
                    .filter_map(|r| self.sessions.get(r.idx)),
            );
        }
        out
    }

    /// Sessions per day over the last `days`, for project `p` and the
    /// projects below it. Scripts' and agents' count only when shown.
    pub fn activity(&self, p: usize, days: usize) -> Vec<u32> {
        let spans = self
            .subtree_sessions(p)
            .into_iter()
            .filter(|s| self.show_all || s.is_typed())
            .map(|s| (s.first_ts, s.last_ts));
        activity::per_day(spans, self.now, self.utc_offset, days)
    }

    /// Every typed session per day, over the last `days`.
    pub fn total_activity(&self, days: usize) -> Vec<u32> {
        let spans = self
            .sessions
            .iter()
            .filter(|s| s.is_typed())
            .map(|s| (s.first_ts, s.last_ts));
        activity::per_day(spans, self.now, self.utc_offset, days)
    }

    pub fn overview(&self) -> Overview {
        let days_ago = |s: &SessionMeta| {
            s.last_ts
                .map(|t| activity::days_ago(self.now, t, self.utc_offset))
        };
        let typed: Vec<&SessionMeta> = self.sessions.iter().filter(|s| s.is_typed()).collect();
        let used_within = |days: i64| {
            let recent = |s: &&&SessionMeta| days_ago(s).is_some_and(|d| d < days);
            typed.iter().filter(recent).count()
        };
        let used_today = |i: usize| {
            let today = |s: &&SessionMeta| s.is_typed() && days_ago(s) == Some(0);
            self.subtree_sessions(i).into_iter().filter(today).count()
        };
        let mut busiest: Vec<(usize, &str)> = self
            .tree
            .top
            .iter()
            .chain(&self.tree.elsewhere)
            .map(|&i| (used_today(i), self.tree.projects[i].name.as_str()))
            .filter(|(n, _)| *n > 0)
            .collect();
        busiest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        Overview {
            tabs: self.tabs.len(),
            projects: self.tree.projects.iter().filter(|p| p.exists).count(),
            sessions: typed.len(),
            today: used_within(1),
            week: used_within(7),
            auto: self.sessions.len() - typed.len(),
            busiest: busiest.iter().take(3).map(|b| b.1.to_string()).collect(),
        }
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

    /// The time project `i` is sorted on.
    fn sort_time(&self, i: usize) -> Option<i64> {
        let p = &self.tree.projects[i];
        match self.sort {
            Sort::Activity => p.last_activity,
            Sort::Modified => self.modified.get(&p.path).copied(),
        }
    }

    /// The section of the top list project `i` sits in.
    fn section_of(&self, i: usize) -> &'static str {
        let unknown = match self.sort {
            Sort::Activity => "no sessions yet",
            Sort::Modified => "no date",
        };
        activity::section(self.now, self.sort_time(i), self.utc_offset).unwrap_or(unknown)
    }

    /// Siblings in the current sort order, newest first, then by name.
    fn sorted(&self, list: &[usize]) -> Vec<usize> {
        let ps = &self.tree.projects;
        let mut v = list.to_vec();
        v.sort_by(|&a, &b| {
            newest_first(self.sort_time(a), self.sort_time(b))
                .then_with(|| ps[a].name.cmp(&ps[b].name))
        });
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
        app.touched = true;
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

    /// "header: path" for every row that starts a section.
    fn headers(app: &App) -> Vec<String> {
        app.rows()
            .iter()
            .filter_map(|r| {
                let path = app.tree.projects[r.project].path.display();
                Some(format!("{}: {path}", r.header?))
            })
            .collect()
    }

    fn tab(index: u32, dir: &str, sessions: &[&str]) -> Tab {
        Tab {
            index,
            dir: dir.into(),
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn project(app: &App, path: &str) -> usize {
        let at = |p: &Project| p.path == Path::new(path);
        app.tree.projects.iter().position(at).unwrap()
    }

    #[test]
    fn rows_start_collapsed_under_time_sections() {
        let (app, _) = app();
        assert!(app.rows().iter().all(|r| r.depth == 0));
        assert_eq!(
            headers(&app),
            [
                "today: /w/lab",
                "this week: /w/inventory",
                "no sessions yet: /w/kpis",
                "elsewhere: /opt/tool",
                "gone: /w/vanished",
            ]
        );
    }

    #[test]
    fn sections_follow_the_local_day() {
        let (mut app, _) = app();
        // Six hours west of UTC it is still the evening before, so the
        // session of two hours ago is today's.
        app.utc_offset = -6 * 3600;
        assert_eq!(
            headers(&app)[..2],
            ["today: /w/lab", "this week: /w/bongo-cli"]
        );
    }

    #[test]
    fn sorted_by_change_the_sections_follow_the_change_time() {
        let (mut app, probe) = app();
        app.modified.insert("/w/kpis".into(), NOW - 10);
        app.modified.insert("/w/lab".into(), NOW - 40 * 86_400);
        app.on_key(ch('s'), &probe);
        assert_eq!(
            headers(&app)[..3],
            ["today: /w/kpis", "older: /w/lab", "no date: /w/bongo-cli"]
        );
    }

    #[test]
    fn question_mark_shows_and_hides_the_keys() {
        let (mut app, probe) = app();
        assert!(!app.keys_shown);
        assert_eq!(app.on_key(ch('?'), &probe), Action::None);
        assert!(app.keys_shown);
        let shifted = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
        app.on_key(shifted, &probe);
        assert!(!app.keys_shown);
    }

    #[test]
    fn esc_hides_the_keys_before_it_clears_a_filter() {
        let (mut app, probe) = app();
        app.on_key(ch('/'), &probe);
        app.on_key(ch('z'), &probe);
        app.on_key(key(KeyCode::Enter), &probe);
        app.on_key(ch('?'), &probe);
        app.on_key(key(KeyCode::Esc), &probe);
        assert!(!app.keys_shown);
        assert_eq!(app.filter, "z", "the first esc only hides the keys");
        app.on_key(key(KeyCode::Esc), &probe);
        assert!(app.filter.is_empty());
    }

    #[test]
    fn question_mark_is_text_while_filtering() {
        let (mut app, probe) = app();
        app.on_key(ch('/'), &probe);
        app.on_key(ch('?'), &probe);
        assert_eq!(app.filter, "?");
        assert!(!app.keys_shown);
    }

    #[test]
    fn tabs_are_found_by_folder_and_by_session() {
        let (mut app, _) = app();
        app.tabs = vec![
            tab(4, INV, &["inv1"]),
            tab(1, INV, &[]),
            tab(2, "/w/kpis", &[]),
        ];
        assert_eq!(app.tab_numbers(Path::new(INV)), [1, 4]);
        assert!(app.tab_numbers(Path::new("/w/lab")).is_empty());
        assert_eq!(app.tab_of_session("inv1"), Some(4));
        assert_eq!(app.tab_of_session("sched1"), None);
    }

    #[test]
    fn a_projects_activity_is_its_own_sessions_and_its_childrens() {
        let (mut app, probe) = app();
        let inv = project(&app, INV);
        // Three days ago inv1, yesterday evening the worktree's. The `also`
        // session of five days ago belongs to another folder, like in the
        // `last` column; the one of ten days ago is too old.
        assert_eq!(app.activity(inv, 7), [0, 0, 0, 1, 0, 1, 0]);
        assert_eq!(app.session_counts(inv), (4, 1), "the list keeps `also`");
        app.on_key(ch('a'), &probe);
        assert_eq!(app.activity(inv, 7), [0, 0, 0, 1, 0, 1, 1]);
        assert_eq!(app.session_counts(inv), (5, 0));
        // A plain folder lists only its own session, but the repo inside
        // it was used yesterday.
        let lab = project(&app, "/w/lab");
        assert_eq!(app.session_counts(lab), (1, 0));
        assert_eq!(app.activity(lab, 3), [0, 1, 1]);
    }

    #[test]
    fn busiest_today_counts_a_projects_children() {
        let (mut app, _) = app();
        let at = |app: &App, id: &str| app.sessions.iter().position(|s| s.id == id).unwrap();
        let (lab1, zomb1) = (at(&app, "lab1"), at(&app, "zomb1"));
        app.sessions[lab1].last_ts = Some(NOW - 20 * 86_400);
        app.sessions[zomb1].last_ts = Some(NOW - 10);
        assert_eq!(app.overview().busiest, ["lab"]);
    }

    #[test]
    fn overview_counts_tabs_projects_and_sessions() {
        let (mut app, _) = app();
        app.tabs = vec![tab(1, INV, &[])];
        let o = app.overview();
        assert_eq!((o.tabs, o.projects), (1, 9), "two folders are gone");
        assert_eq!((o.sessions, o.auto), (9, 1));
        assert_eq!((o.today, o.week), (1, 6));
        assert_eq!(o.busiest, ["lab"]);
        assert_eq!(app.total_activity(7), [0, 1, 0, 1, 0, 3, 1]);

        app.utc_offset = -6 * 3600;
        let o = app.overview();
        assert_eq!(o.today, 2, "the session of two hours ago is today's");
        assert_eq!(o.busiest, ["inventory", "lab"]);
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
    fn new_data_before_any_key_leaves_the_cursor_on_the_top_row() {
        let (mut app, probe) = app();
        let top = names(&app)[0].clone();
        // Older data put another project first; the cursor sat on row 0.
        let mut sessions = sessions();
        sessions.push(session("fresh", "/w/kpis", 1, true, "Just now"));
        let tree = fixture_tree(&sessions, &probe);
        app.set_data(tree, sessions);
        assert_ne!(names(&app)[0], top, "the order changed");
        assert_eq!(app.project_cursor, 0);

        // After a key, the selected project is followed instead.
        app.on_key(ch('j'), &probe);
        let picked = app.selected_path().unwrap();
        let mut sessions = sessions_with("fresh2", "/w/lab");
        sessions.push(session("fresh", "/w/kpis", 1, true, "Just now"));
        let tree = fixture_tree(&sessions, &probe);
        app.set_data(tree, sessions);
        assert_eq!(app.selected_path().unwrap(), picked);
    }

    fn sessions_with(id: &str, cwd: &str) -> Vec<SessionMeta> {
        let mut all = sessions();
        all.push(session(id, cwd, 0, true, "Newest"));
        all
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
