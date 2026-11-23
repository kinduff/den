//! `den home`: the picker in tmux window 0. The UI thread only draws and
//! handles keys; the index, the project tree, git and the tab list come in
//! over one channel from background threads.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::{cursor, execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use rayon::prelude::*;

use crate::app::{Action, App};
use crate::config::{Config, config_path, contract_tilde, load_or_init};
use crate::git::{self, GitStatus, RepoInfo};
use crate::index::{self, Index};
use crate::project::{Kind, MemoProbe, Probe, Project, RealProbe, Tree, build_tree};
use crate::scan::scan;
use crate::session::SessionMeta;
use crate::tmux::{Tab, Tmux};

const REFRESH_EVERY: Duration = Duration::from_secs(30);
const GIT_FRESH_FOR: Duration = Duration::from_secs(60);
/// Index temp files older than this are leftovers of a killed save.
const STALE_TEMP_AFTER: Duration = Duration::from_secs(60);
const OPEN_DIRS_EVERY: Duration = Duration::from_secs(2);
const GIT_THREADS: usize = 4;

/// Set by the panic hook: the input thread stops reading keys.
static STOP: AtomicBool = AtomicBool::new(false);
/// Set by the panic hook: some thread panicked.
static PANICKED: AtomicBool = AtomicBool::new(false);

enum Msg {
    Input(Event),
    InputError(String),
    Progress(usize, usize),
    /// A tree and the sessions its `SessionRef`s point into. `provisional`
    /// when it was built without git.
    Data {
        tree: Tree,
        sessions: Vec<SessionMeta>,
        provisional: bool,
    },
    Git(PathBuf, Option<GitStatus>),
    Modified(PathBuf, i64),
    Tabs(Vec<Tab>),
    /// Whether window 0 is the active window with a client attached.
    HomeActive(bool),
    Notice(String),
    /// After each read of the config: why it failed, or None when it read.
    ConfigError(Option<String>),
}

/// Runs the picker. Returns the process exit code. On an error or a panic
/// the message stays on screen until a key is pressed, so the respawn loop
/// in window 0 cannot scroll it away.
pub fn main() -> i32 {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        STOP.store(true, Ordering::SeqCst);
        PANICKED.store(true, Ordering::SeqCst);
        restore_terminal();
        default_hook(info);
    }));
    let result = std::panic::catch_unwind(run);
    STOP.store(true, Ordering::SeqCst);
    restore_terminal();
    let message = match result {
        Ok(Ok(())) => return 0,
        Ok(Err(e)) => Some(format!("den home: {e:#}")),
        // The hook already printed it.
        Err(_) => None,
    };
    if let Some(message) = message {
        eprintln!("{message}");
    }
    wait_for_key();
    1
}

fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        terminal::LeaveAlternateScreen,
        cursor::Show
    );
}

fn wait_for_key() {
    eprintln!("press any key to start den home again");
    // Let the input thread see STOP and let go of the terminal.
    thread::sleep(Duration::from_millis(200));
    if terminal::enable_raw_mode().is_err() {
        return;
    }
    while let Ok(ev) = event::read() {
        if matches!(ev, Event::Key(k) if k.kind == KeyEventKind::Press) {
            break;
        }
    }
    let _ = terminal::disable_raw_mode();
}

/// Where `den home` reads and writes, all from `dirs` (so `HOME` and the
/// XDG variables move them).
struct Paths {
    cache: PathBuf,
    config: PathBuf,
    projects: PathBuf,
    home: PathBuf,
}

impl Paths {
    fn real() -> Paths {
        Paths {
            cache: index::cache_path(),
            config: config_path(),
            projects: index::projects_dir(),
            home: dirs::home_dir().unwrap_or_default(),
        }
    }
}

fn run() -> anyhow::Result<()> {
    let paths = Paths::real();
    let tmux = Tmux::real();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());

    terminal::enable_raw_mode().context("terminal raw mode")?;
    execute!(
        std::io::stdout(),
        terminal::EnterAlternateScreen,
        cursor::Hide
    )
    .context("terminal setup")?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let mut ui = Ui::new(&paths);
    term.draw(|f| crate::view::draw(f, &ui.app))?;

    let (tx, rx) = mpsc::channel();
    spawn_input(tx.clone());
    let home_active = Arc::new(AtomicBool::new(true));
    let (wake_indexer, indexer_woken) = mpsc::channel();
    {
        let tx = tx.clone();
        let active = Arc::clone(&home_active);
        thread::spawn(move || indexer(tx, paths, &active, &indexer_woken));
    }
    let wake = spawn_tabs(tx.clone(), tmux.clone(), home_active, wake_indexer);
    let pool = GitPool::start(tx);

    loop {
        let screen = term
            .size()
            .map(|s| Rect::new(0, 0, s.width, s.height))
            .unwrap_or(Rect::new(0, 0, 100, 50));
        ui.ask_git(&pool, screen);
        let first = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => bail!("every worker stopped"),
        };
        for msg in first.into_iter().chain(drain(&rx)) {
            match ui.handle(msg)? {
                Action::None => {}
                Action::Open(req) => match tmux.open(&req, &shell) {
                    Ok(()) => {
                        let _ = wake.send(());
                    }
                    Err(e) => ui.error = Some(format!("{e:#}")),
                },
                Action::Detach => {
                    if let Err(e) = tmux.run(&["detach-client"]) {
                        ui.error = Some(format!("{e:#}"));
                    }
                }
            }
        }
        ui.app.now = unix_now();
        ui.app.utc_offset = local_offset();
        ui.app.notice = ui.notice();
        // The hook has restored the terminal and printed the panic: drawing
        // now would cover it.
        if PANICKED.load(Ordering::SeqCst) {
            bail!("a background thread panicked");
        }
        term.draw(|f| crate::view::draw(f, &ui.app))?;
    }
}

fn drain(rx: &Receiver<Msg>) -> impl Iterator<Item = Msg> + '_ {
    std::iter::from_fn(|| rx.try_recv().ok())
}

/// The local zone in seconds east of UTC: where a day starts in the lists.
fn local_offset() -> i64 {
    i64::from(chrono::Local::now().offset().local_minus_utc())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The UI thread's state around `App`.
struct Ui {
    app: App,
    /// Whether any tree has arrived yet.
    loaded: bool,
    /// Largest `done` seen for the refresh in progress, and its total.
    progress: Option<(usize, usize)>,
    /// Shown until the next key: the first-run root, config trouble.
    info: Option<String>,
    /// Shown while the config file does not read.
    config_error: Option<String>,
    /// Shown until the next key: a failed open or detach.
    error: Option<String>,
    /// When git was last asked about a folder.
    asked: HashMap<PathBuf, Instant>,
    /// Whether someone is looking at the home tab; git waits otherwise.
    home_active: bool,
}

impl Ui {
    fn new(paths: &Paths) -> Ui {
        let mut app = App::new(unix_now());
        app.utc_offset = local_offset();
        app.home = paths.home.clone();
        let mut ui = Ui {
            app,
            loaded: false,
            progress: None,
            info: None,
            config_error: None,
            error: None,
            asked: HashMap::new(),
            home_active: true,
        };
        ui.app.notice = ui.notice();
        ui
    }

    fn handle(&mut self, msg: Msg) -> anyhow::Result<Action> {
        match msg {
            Msg::Input(Event::Key(key)) => {
                if key.kind == KeyEventKind::Press {
                    self.error = None;
                    self.info = None;
                }
                return Ok(self.app.on_key(key, &RealProbe));
            }
            Msg::Input(_) => {}
            Msg::InputError(e) => bail!("reading keys: {e}"),
            Msg::Progress(done, total) => {
                let best = match self.progress {
                    Some((d, t)) if t == total => d.max(done),
                    _ => done,
                };
                self.progress = Some((best, total));
            }
            Msg::Data {
                tree,
                sessions,
                provisional,
            } => {
                self.app.set_data(tree, sessions);
                self.app.provisional = provisional;
                self.loaded = true;
                self.progress = None;
            }
            Msg::Git(path, status) => match status {
                Some(s) => {
                    self.app.status.insert(path, s);
                }
                None => {
                    self.app.status.remove(&path);
                }
            },
            Msg::Modified(path, t) => {
                self.app.modified.insert(path, t);
            }
            Msg::Tabs(tabs) => self.app.tabs = tabs,
            Msg::HomeActive(active) => {
                if active && !self.home_active {
                    // Back on the home tab: ask again at once.
                    self.asked.clear();
                }
                self.home_active = active;
            }
            Msg::Notice(text) => self.info = Some(text),
            Msg::ConfigError(e) => self.config_error = e,
        }
        Ok(Action::None)
    }

    /// Error first, then the first run's progress, then a broken config,
    /// then the info line.
    fn notice(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(e.clone());
        }
        if !self.loaded || self.app.sessions.is_empty() {
            if let Some((done, total)) = self.progress {
                return Some(format!("indexing sessions {done}/{total}"));
            }
            if !self.loaded {
                return Some("loading".into());
            }
        }
        self.config_error.clone().or_else(|| self.info.clone())
    }

    /// Hands the git pool the folders of the rows on a `screen`-sized
    /// screen that have no answer younger than `GIT_FRESH_FOR`. Nothing
    /// while nobody looks at the home tab.
    fn ask_git(&mut self, pool: &GitPool, screen: Rect) {
        if !self.home_active {
            return;
        }
        let rows = self.app.rows();
        let on = crate::view::project_rows_on_screen(&self.app, screen);
        let jobs: Vec<Job> = rows[on]
            .iter()
            .filter_map(|r| job_for(&self.app.tree.projects[r.project], self.app.provisional))
            .collect();
        let on_screen: HashSet<PathBuf> = jobs.iter().map(|j| j.path.clone()).collect();
        let fresh: Vec<Job> = jobs
            .into_iter()
            .filter(|j| {
                self.asked
                    .get(&j.path)
                    .is_none_or(|t| t.elapsed() >= GIT_FRESH_FOR)
            })
            .collect();
        for j in &fresh {
            self.asked.insert(j.path.clone(), Instant::now());
        }
        for dropped in pool.submit(fresh, &on_screen) {
            self.asked.remove(&dropped);
        }
    }
}

/// Reads terminal events until `STOP`.
fn spawn_input(tx: Sender<Msg>) {
    thread::spawn(move || {
        while !STOP.load(Ordering::SeqCst) {
            let msg = match event::poll(Duration::from_millis(100)) {
                Ok(false) => continue,
                Ok(true) => match event::read() {
                    Ok(ev) => Msg::Input(ev),
                    Err(e) => Msg::InputError(e.to_string()),
                },
                Err(e) => Msg::InputError(e.to_string()),
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
}

/// Lists the open tabs and whether the home tab is watched every
/// `OPEN_DIRS_EVERY`, or at once when woken. Keeps `home_active` up to date
/// and wakes the indexer when the home tab is watched again.
fn spawn_tabs(
    tx: Sender<Msg>,
    tmux: Tmux,
    home_active: Arc<AtomicBool>,
    wake_indexer: Sender<()>,
) -> Sender<()> {
    let (wake, woken) = mpsc::channel::<()>();
    thread::spawn(move || {
        loop {
            let (tabs, active) = tmux.poll_tabs();
            if tx.send(Msg::Tabs(tabs)).is_err() || tx.send(Msg::HomeActive(active)).is_err() {
                return;
            }
            if active && !home_active.swap(active, Ordering::SeqCst) {
                let _ = wake_indexer.send(());
            }
            home_active.store(active, Ordering::SeqCst);
            if let Err(RecvTimeoutError::Disconnected) = woken.recv_timeout(OPEN_DIRS_EVERY) {
                return;
            }
        }
    });
    wake
}

/// Loads the cache, then refreshes the index and reads the config again
/// every `REFRESH_EVERY` while the home tab is watched, sending a tree
/// built without git first (so the list never waits on git) and the real
/// one after, with every project's change time.
fn indexer(tx: Sender<Msg>, paths: Paths, active: &AtomicBool, woken: &Receiver<()>) {
    index::remove_stale_temps(&paths.cache, STALE_TEMP_AFTER);
    let cached = index::load_cache(&paths.cache);
    // With neither a config nor a cache, the root is picked from the first
    // refresh instead of from nothing.
    let mut cfg = (paths.config.exists() || !cached.sessions.is_empty())
        .then(|| load_config(&tx, &paths, &cached, None));
    if let Some(cfg) = &cfg {
        let _ = tx.send(data(&cached, cfg, &paths.home, &NoGitProbe, true));
    }
    let mut current = cached;
    let mut real_sent = false;
    loop {
        let progress_tx = tx.clone();
        let progress = move |done: usize, total: usize| {
            let _ = progress_tx.send(Msg::Progress(done, total));
        };
        let fresh = index::refresh(&paths.projects, &current, &progress);
        if (fresh != current || !paths.cache.exists())
            && let Err(e) = index::save_cache(&paths.cache, &fresh)
        {
            let _ = tx.send(Msg::Notice(format!("could not save the index: {e:#}")));
        }
        // Read again on every refresh, so an edit is picked up and a broken
        // file is retried.
        let cfg = &*cfg.insert(load_config(&tx, &paths, &fresh, cfg.as_ref()));
        if !real_sent
            && tx
                .send(data(&fresh, cfg, &paths.home, &NoGitProbe, true))
                .is_err()
        {
            return;
        }
        for msg in real_data(&fresh, cfg, &paths.home) {
            if tx.send(msg).is_err() {
                return;
            }
        }
        real_sent = true;
        current = fresh;
        if !wait_turn(active, woken, REFRESH_EVERY) {
            return;
        }
    }
}

/// Waits `every`, or less when woken, then for as long as the home tab is
/// not watched. False when the waker is gone.
fn wait_turn(active: &AtomicBool, woken: &Receiver<()>, every: Duration) -> bool {
    if let Err(RecvTimeoutError::Disconnected) = woken.recv_timeout(every) {
        return false;
    }
    while !active.load(Ordering::SeqCst) {
        if woken.recv().is_err() {
            return false;
        }
    }
    true
}

/// The real tree, then the change time of every existing project, so the
/// sort by last change is right for rows never on screen.
fn real_data(index: &Index, cfg: &Config, home: &Path) -> Vec<Msg> {
    let msg = data(index, cfg, home, &RealProbe, false);
    let times = match &msg {
        Msg::Data { tree, .. } => modified_times(tree),
        _ => Vec::new(),
    };
    std::iter::once(msg)
        .chain(times.into_iter().map(|(p, t)| Msg::Modified(p, t)))
        .collect()
}

/// Last commit time for repos and worktrees (folder time without one),
/// folder time for plain folders.
fn modified_times(tree: &Tree) -> Vec<(PathBuf, i64)> {
    tree.projects
        .par_iter()
        .filter(|p| p.exists)
        .filter_map(|p| {
            let git = match p.kind {
                Kind::Folder => None,
                Kind::Repo | Kind::Worktree => git::last_commit_time(&p.path),
            };
            git.or_else(|| folder_mtime(&p.path))
                .map(|t| (p.path.clone(), t))
        })
        .collect()
}

fn typed_homes(index: &Index) -> Vec<PathBuf> {
    index
        .sessions
        .iter()
        .filter(|s| s.is_typed())
        .filter_map(|s| s.home_cwd.clone())
        .collect()
}

fn data(index: &Index, cfg: &Config, home: &Path, probe: &dyn Probe, provisional: bool) -> Msg {
    let homes = typed_homes(index);
    // The scan and the tree ask git about the same folders: once is enough.
    let probe = MemoProbe::new(probe);
    let scanned = scan(cfg, &homes, &probe);
    Msg::Data {
        tree: build_tree(&index.sessions, &scanned, cfg, home, &probe),
        sessions: index.sessions.clone(),
        provisional,
    }
}

/// `load_or_init`, telling the user which root was picked on a first run.
/// A config that cannot be read, or one with no folder to scan, leaves a
/// lasting notice.
fn load_config(tx: &Sender<Msg>, paths: &Paths, index: &Index, prev: Option<&Config>) -> Config {
    let loaded = load_or_init(&paths.config, &typed_homes(index));
    let (cfg, problem, info) = choose_config(prev, loaded, &paths.config, &paths.home);
    let _ = tx.send(Msg::ConfigError(problem));
    if let Some(info) = info {
        let _ = tx.send(Msg::Notice(info));
    }
    cfg
}

/// The config to use after reading the file again: the new one, or on an
/// error the last good one (the default when there never was one). Also
/// the lasting problem to show (unreadable file, no folder to scan) and
/// the one-time info line. The file itself is never rewritten here.
fn choose_config(
    prev: Option<&Config>,
    loaded: anyhow::Result<(Config, bool)>,
    config: &Path,
    home: &Path,
) -> (Config, Option<String>, Option<String>) {
    let shown = contract_tilde(config, home);
    match loaded {
        Ok((cfg, _)) if cfg.roots.is_empty() => {
            let problem = format!(
                "no projects folder found: add one to roots in {}",
                shown.display()
            );
            (cfg, Some(problem), None)
        }
        Ok((cfg, written)) => {
            let info = written.then(|| first_run_notice(&cfg, config, home));
            (cfg, None, info)
        }
        Err(e) => {
            let problem = format!("{}: {e:#}", shown.display());
            let cfg = prev.cloned().unwrap_or_else(|| Config {
                roots: Vec::new(),
                depth: 2,
                hide: Vec::new(),
            });
            (cfg, Some(problem), None)
        }
    }
}

/// "scanning ~/www (edit ~/.config/den/config.toml to change)".
fn first_run_notice(cfg: &Config, config: &Path, home: &Path) -> String {
    let root = cfg.roots.first().map(|r| contract_tilde(r, home));
    format!(
        "scanning {} (edit {} to change)",
        root.unwrap_or_default().display(),
        contract_tilde(config, home).display()
    )
}

/// The file system without git: every folder looks like a plain folder.
/// Good enough to paint the list before git answers.
struct NoGitProbe;

impl Probe for NoGitProbe {
    fn exists(&self, p: &Path) -> bool {
        RealProbe.exists(p)
    }

    fn repo_info(&self, _: &Path) -> Option<RepoInfo> {
        None
    }

    fn subdirs(&self, p: &Path) -> Vec<PathBuf> {
        RealProbe.subdirs(p)
    }

    fn worktrees(&self, _: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// `GIT_THREADS` threads answering `Msg::Git` and `Msg::Modified` for
/// folders. A job nobody waits on: the columns stay blank until it answers.
struct GitPool {
    shared: Arc<(Mutex<Jobs>, Condvar)>,
}

#[derive(Default)]
struct Jobs {
    queue: VecDeque<Job>,
    running: HashSet<PathBuf>,
}

/// A folder to look at; `git` when it may be a repo of its own.
#[derive(Clone, Debug, PartialEq)]
struct Job {
    path: PathBuf,
    git: bool,
}

/// The job for a project row: git for repos and worktrees, only the folder
/// time for plain folders. In a git-free tree every folder may be a repo.
fn job_for(p: &Project, provisional: bool) -> Option<Job> {
    p.exists.then(|| Job {
        path: p.path.clone(),
        git: provisional || p.kind != Kind::Folder,
    })
}

/// Status and change time for `path`. Git answers only when `path` is the
/// top of its own repo: a folder inside a repo is a plain folder, never the
/// enclosing repo.
fn git_job(path: &Path, asked_git: bool) -> (Option<GitStatus>, Option<i64>) {
    let own_repo =
        asked_git && git::repo_info(path).is_some_and(|info| same_folder(&info.root, path));
    if own_repo {
        let modified = git::last_commit_time(path).or_else(|| folder_mtime(path));
        (git::status(path), modified)
    } else {
        (None, folder_mtime(path))
    }
}

fn same_folder(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl GitPool {
    fn start(tx: Sender<Msg>) -> GitPool {
        let shared = Arc::new((Mutex::new(Jobs::default()), Condvar::new()));
        for _ in 0..GIT_THREADS {
            let shared = Arc::clone(&shared);
            let tx = tx.clone();
            thread::spawn(move || git_worker(&shared, &tx));
        }
        GitPool { shared }
    }

    /// Drops waiting jobs no longer on screen, queues `fresh`, and returns
    /// the dropped ones.
    fn submit(&self, fresh: Vec<Job>, on_screen: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let (lock, cv) = &*self.shared;
        let Ok(mut jobs) = lock.lock() else {
            return fresh.into_iter().map(|j| j.path).collect();
        };
        let mut dropped = Vec::new();
        jobs.queue.retain(|j| {
            let keep = on_screen.contains(&j.path);
            if !keep {
                dropped.push(j.path.clone());
            }
            keep
        });
        for job in fresh {
            if !jobs.running.contains(&job.path) && !jobs.queue.iter().any(|q| q.path == job.path) {
                jobs.queue.push_back(job);
            }
        }
        cv.notify_all();
        dropped
    }
}

fn git_worker(shared: &(Mutex<Jobs>, Condvar), tx: &Sender<Msg>) {
    let (lock, cv) = shared;
    loop {
        let job = {
            let Ok(mut jobs) = lock.lock() else { return };
            loop {
                if let Some(job) = jobs.queue.pop_front() {
                    jobs.running.insert(job.path.clone());
                    break job;
                }
                jobs = match cv.wait(jobs) {
                    Ok(j) => j,
                    Err(_) => return,
                };
            }
        };
        let (status, modified) = git_job(&job.path, job.git);
        let path = job.path;
        if let Ok(mut jobs) = lock.lock() {
            jobs.running.remove(&path);
        }
        if let Some(t) = modified
            && tx.send(Msg::Modified(path.clone(), t)).is_err()
        {
            return;
        }
        if tx.send(Msg::Git(path, status)).is_err() {
            return;
        }
    }
}

fn folder_mtime(p: &Path) -> Option<i64> {
    std::fs::metadata(p)
        .ok()
        .map(|md| crate::session::mtime_secs(&md))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_notice_names_the_picked_root() {
        let cfg = Config {
            roots: vec!["/h/www".into()],
            depth: 2,
            hide: Vec::new(),
        };
        let conf = Path::new("/h/.config/den/config.toml");
        assert_eq!(
            first_run_notice(&cfg, conf, Path::new("/h")),
            "scanning ~/www (edit ~/.config/den/config.toml to change)"
        );
    }

    fn project(path: &str, kind: Kind, exists: bool) -> Project {
        Project {
            path: path.into(),
            name: "x".into(),
            kind,
            exists,
            parent: None,
            children: Vec::new(),
            sessions: Vec::new(),
            last_activity: None,
        }
    }

    #[test]
    fn only_repos_and_worktrees_go_to_git_once_the_tree_is_real() {
        let git = |p: &Project, provisional| job_for(p, provisional).map(|j| j.git);
        assert_eq!(git(&project("/r", Kind::Repo, true), false), Some(true));
        assert_eq!(git(&project("/w", Kind::Worktree, true), false), Some(true));
        assert_eq!(git(&project("/f", Kind::Folder, true), false), Some(false));
        assert_eq!(
            git(&project("/f", Kind::Folder, true), true),
            Some(true),
            "a git-free tree does not know which folders are repos"
        );
        assert!(job_for(&project("/g", Kind::Repo, false), false).is_none());
    }

    #[test]
    fn a_subfolder_never_gets_the_enclosing_repos_status() {
        use crate::git::testgit::{canon, init_repo};
        let tmp = tempfile::tempdir().unwrap();
        let repo = canon(tmp.path()).join("repo");
        init_repo(&repo);
        let sub = repo.join("plain");
        std::fs::create_dir_all(&sub).unwrap();

        let (status, modified) = git_job(&repo, true);
        assert_eq!(status.unwrap().branch.as_deref(), Some("main"));
        assert_eq!(modified, git::last_commit_time(&repo));

        for asked_git in [true, false] {
            let (status, modified) = git_job(&sub, asked_git);
            assert_eq!(status, None, "asked_git {asked_git}");
            assert_eq!(modified, folder_mtime(&sub), "asked_git {asked_git}");
        }
    }

    #[test]
    fn config_reload_keeps_the_last_good_config() {
        let conf = Path::new("/h/.config/den/config.toml");
        let home = Path::new("/h");
        let good = Config {
            roots: vec!["/h/www".into()],
            depth: 3,
            hide: Vec::new(),
        };
        let newer = Config {
            depth: 1,
            ..good.clone()
        };

        let (cfg, problem, info) = choose_config(None, Ok((good.clone(), true)), conf, home);
        assert_eq!((cfg, problem), (good.clone(), None));
        assert_eq!(
            info.as_deref(),
            Some("scanning ~/www (edit ~/.config/den/config.toml to change)")
        );
        let (cfg, problem, info) =
            choose_config(Some(&good), Ok((newer.clone(), false)), conf, home);
        assert_eq!(
            (cfg, problem, info),
            (newer.clone(), None, None),
            "an edit is picked up"
        );

        let broken = || Err(anyhow::anyhow!("expected `]`"));
        let (cfg, problem, info) = choose_config(Some(&newer), broken(), conf, home);
        assert_eq!(cfg, newer, "the last good config stays");
        assert_eq!(
            problem.as_deref(),
            Some("~/.config/den/config.toml: expected `]`")
        );
        assert_eq!(info, None);
        let (cfg, problem, _) = choose_config(None, broken(), conf, home);
        assert_eq!(
            cfg,
            Config {
                roots: Vec::new(),
                depth: 2,
                hide: Vec::new(),
            }
        );
        assert!(problem.is_some());
    }

    #[test]
    fn no_projects_folder_is_a_lasting_notice() {
        let conf = Path::new("/h/.config/den/config.toml");
        let home = Path::new("/h");
        let empty = Config {
            roots: Vec::new(),
            depth: 2,
            hide: Vec::new(),
        };
        let want = "no projects folder found: add one to roots in ~/.config/den/config.toml";
        for written in [true, false] {
            let (_, problem, info) = choose_config(None, Ok((empty.clone(), written)), conf, home);
            assert_eq!(problem.as_deref(), Some(want), "written {written}");
            assert_eq!(info, None);
        }
    }

    fn idle_pool() -> GitPool {
        GitPool {
            shared: Arc::new((Mutex::new(Jobs::default()), Condvar::new())),
        }
    }

    fn queued(pool: &GitPool) -> Vec<PathBuf> {
        let mut jobs = pool.shared.0.lock().unwrap();
        jobs.queue.drain(..).map(|j| j.path).collect()
    }

    fn test_paths() -> Paths {
        Paths {
            cache: "/c".into(),
            config: "/f".into(),
            projects: "/p".into(),
            home: "/h".into(),
        }
    }

    #[test]
    fn git_is_asked_only_while_the_home_tab_is_watched() {
        let (app, _) = crate::app::fixture::app();
        let mut ui = Ui::new(&test_paths());
        ui.app = app;
        let pool = idle_pool();
        let screen = Rect::new(0, 0, 100, 30);

        ui.handle(Msg::HomeActive(false)).unwrap();
        ui.ask_git(&pool, screen);
        assert!(queued(&pool).is_empty(), "nothing while away");

        ui.handle(Msg::HomeActive(true)).unwrap();
        ui.ask_git(&pool, screen);
        assert!(!queued(&pool).is_empty(), "asked on return");
        ui.ask_git(&pool, screen);
        assert!(queued(&pool).is_empty(), "answers are fresh");

        ui.handle(Msg::HomeActive(false)).unwrap();
        ui.handle(Msg::HomeActive(true)).unwrap();
        ui.ask_git(&pool, screen);
        assert!(!queued(&pool).is_empty(), "coming back asks again at once");
    }

    #[test]
    fn git_is_asked_only_for_rows_on_screen() {
        let (app, _) = crate::app::fixture::app();
        let mut ui = Ui::new(&test_paths());
        ui.app = app;
        let pool = idle_pool();
        let screen = Rect::new(0, 0, 100, 12);
        let on_screen = crate::view::project_rows_on_screen(&ui.app, screen);
        assert!(on_screen.len() < ui.app.rows().len());
        ui.ask_git(&pool, screen);
        let asked = queued(&pool);
        assert!(
            !asked.is_empty() && asked.len() <= on_screen.len(),
            "{asked:?}"
        );
    }

    #[test]
    fn refresh_waits_while_away_and_starts_on_return() {
        let active = Arc::new(AtomicBool::new(false));
        let (wake, woken) = mpsc::channel();
        let flag = Arc::clone(&active);
        let start = Instant::now();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::SeqCst);
            let _ = wake.send(());
        });
        assert!(wait_turn(&active, &woken, Duration::from_millis(10)));
        assert!(start.elapsed() >= Duration::from_millis(200));

        let (_wake, woken) = mpsc::channel::<()>();
        let start = Instant::now();
        assert!(wait_turn(&active, &woken, Duration::from_millis(50)));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "active: one period"
        );
    }

    #[test]
    fn real_pass_sends_change_times_for_every_project() {
        use crate::git::testgit::{canon, init_repo};
        let tmp = tempfile::tempdir().unwrap();
        let base = canon(tmp.path());
        let repo = base.join("repo");
        init_repo(&repo);
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let cfg = Config {
            roots: vec![base.clone()],
            depth: 2,
            hide: Vec::new(),
        };
        let msgs = real_data(&Index::default(), &cfg, Path::new("/nonexistent-home"));
        assert!(matches!(
            msgs.first(),
            Some(Msg::Data {
                provisional: false,
                ..
            })
        ));
        let times: HashMap<PathBuf, i64> = msgs
            .iter()
            .filter_map(|m| match m {
                Msg::Modified(p, t) => Some((p.clone(), *t)),
                _ => None,
            })
            .collect();
        assert_eq!(times.get(&repo).copied(), git::last_commit_time(&repo));
        assert_eq!(times.get(&plain).copied(), folder_mtime(&plain));
    }

    #[test]
    fn config_error_stays_until_the_file_reads_again() {
        let paths = Paths {
            cache: "/c".into(),
            config: "/f".into(),
            projects: "/p".into(),
            home: "/h".into(),
        };
        let mut ui = Ui::new(&paths);
        ui.handle(Msg::Data {
            tree: Tree::default(),
            sessions: Vec::new(),
            provisional: false,
        })
        .unwrap();
        ui.handle(Msg::ConfigError(Some("/f: bad".into()))).unwrap();
        assert_eq!(ui.notice().as_deref(), Some("/f: bad"));
        let j = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('j'));
        ui.handle(Msg::Input(Event::Key(j))).unwrap();
        assert_eq!(
            ui.notice().as_deref(),
            Some("/f: bad"),
            "a key does not hide it"
        );
        ui.handle(Msg::ConfigError(None)).unwrap();
        assert_eq!(ui.notice(), None, "fixed file, gone notice");
    }

    #[test]
    fn progress_shows_the_largest_done_seen() {
        let paths = Paths {
            cache: "/c".into(),
            config: "/f".into(),
            projects: "/p".into(),
            home: "/h".into(),
        };
        let mut ui = Ui::new(&paths);
        for done in [3, 1, 5, 4] {
            ui.handle(Msg::Progress(done, 9)).unwrap();
        }
        assert_eq!(ui.notice().as_deref(), Some("indexing sessions 5/9"));
        ui.handle(Msg::Data {
            tree: Tree::default(),
            sessions: Vec::new(),
            provisional: false,
        })
        .unwrap();
        ui.handle(Msg::Progress(1, 9)).unwrap();
        assert_eq!(
            ui.notice().as_deref(),
            Some("indexing sessions 1/9"),
            "a new refresh starts over"
        );
        assert_eq!(ui.app.home, Path::new("/h"));
    }
}
