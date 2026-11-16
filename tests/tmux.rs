//! Integration tests against real, private tmux servers (`den-test-*`).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use den::tmux::{OpenReq, Tmux, What};

const SHELL: &str = "/bin/sh";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// One private server per test; killed when dropped.
struct Server {
    tmux: Tmux,
    tmp: tempfile::TempDir,
}

impl Server {
    /// The temp folder with symlinks resolved: tmux reports resolved paths,
    /// and on macOS the temp folder sits behind the `/var` symlink.
    fn root(&self) -> PathBuf {
        self.tmp.path().canonicalize().unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let socket = self.tmux.run(&["display", "-p", "#{socket_path}"]);
        let _ = self.tmux.run(&["kill-server"]);
        // kill-server leaves the socket file behind.
        if let Ok(socket) = socket {
            let _ = std::fs::remove_file(socket.trim_end());
        }
    }
}

fn have_tmux() -> bool {
    let found = Command::new("tmux").arg("-V").output().is_ok();
    if !found {
        eprintln!("tmux not on PATH, skipping");
    }
    found
}

/// A started server, or None when tmux is missing.
fn server(name: &str) -> Option<Server> {
    if !have_tmux() {
        return None;
    }
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let tmp = tempfile::tempdir().unwrap();
    let tmux = Tmux {
        socket: format!("den-test-{}-{n}-{name}", std::process::id()),
        conf: tmp.path().join("conf/tmux.conf"),
    };
    let server = Server { tmux, tmp };
    server
        .tmux
        .ensure_server(Path::new("/bin/true"), SHELL)
        .unwrap();
    Some(server)
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Writes a fake `claude` running `body` and returns a shell to open tabs
/// with: `/bin/sh` with the fake first on PATH. tmux gives a new window the
/// PATH of the client that asked for it (here the test process), so the
/// server's environment cannot carry it.
fn fake_claude(server: &Server, body: &str) -> String {
    let bin = server.root().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), body);
    let shell = bin.join("sh");
    let quoted = den::tmux::sh_quote(&bin.to_string_lossy());
    write_script(
        &shell,
        &format!("PATH={quoted}:\"$PATH\" exec /bin/sh \"$@\""),
    );
    shell.to_string_lossy().into_owned()
}

/// Retries `query` for up to 5 seconds until `want` holds.
fn poll<T: std::fmt::Debug>(mut query: impl FnMut() -> T, want: impl Fn(&T) -> bool) -> T {
    let start = Instant::now();
    loop {
        let value = query();
        if want(&value) {
            return value;
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!("gave up waiting, last value: {value:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn project_dir(server: &Server) -> PathBuf {
    let dir = server.root().join("my proj@x é");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn windows(tmux: &Tmux) -> Vec<String> {
    tmux.run(&[
        "list-windows",
        "-t",
        "den",
        "-F",
        "#{window_index} #{window_name}",
    ])
    .unwrap()
    .lines()
    .map(str::to_string)
    .collect()
}

#[test]
fn config_loads_without_errors() {
    let Some(s) = server("conf") else { return };
    let prefix = s.tmux.run(&["show", "-g", "prefix"]).unwrap();
    assert_eq!(prefix.trim(), "prefix None");
    // Reading the config again on the live server runs every line; any
    // error makes source-file fail and print it.
    let conf = s.tmux.conf.to_string_lossy().into_owned();
    let out = Command::new("tmux")
        .args(["-L", &s.tmux.socket, "source-file", &conf])
        .output()
        .unwrap();
    assert!(
        out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
        "source-file: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let keys = s.tmux.run(&["list-keys", "-T", "root"]).unwrap();
    for key in [
        "M-0", "M-9", "M-[", "M-]", "M-\\\\", "M--", "M-w", "M-z", "M-c",
    ] {
        assert!(
            keys.lines()
                .any(|l| l.split_whitespace().nth(3) == Some(key)),
            "{key} not bound:\n{keys}"
        );
    }
    let new_claude = s.tmux.run(&["list-keys", "-T", "root", "M-c"]).unwrap();
    assert!(
        new_claude
            .contains(r##"split-window -h -c "#{@den_dir}" "/bin/sh -ic 'claude; exec /bin/sh'""##),
        "{new_claude}"
    );
}

#[test]
fn server_has_home_window_zero() {
    let Some(s) = server("home") else { return };
    assert_eq!(windows(&s.tmux).first().map(String::as_str), Some("0 home"));
    s.tmux.ensure_server(Path::new("/bin/true"), SHELL).unwrap();
    let sessions = s.tmux.run(&["list-sessions", "-F", "#S"]).unwrap();
    assert_eq!(sessions.lines().collect::<Vec<_>>(), ["den"]);
    assert_eq!(windows(&s.tmux).len(), 1);
}

#[test]
fn open_shell_lands_in_the_folder() {
    let Some(s) = server("shell") else { return };
    let dir = project_dir(&s);
    let req = OpenReq {
        dir: dir.clone(),
        name: "my proj/x".into(),
        what: What::Shell,
    };
    s.tmux.open(&req, SHELL).unwrap();
    let want = format!("{}\n", dir.display());
    poll(
        || {
            s.tmux
                .run(&["display", "-p", "-t", "den:1", "#{pane_current_path}"])
                .unwrap()
        },
        |p| *p == want,
    );
    let name = s
        .tmux
        .run(&["display", "-p", "-t", "den:1", "#{window_name}"])
        .unwrap();
    assert_eq!(name.trim_end(), req.name);
    assert_eq!(s.tmux.open_dirs(), vec![dir]);
}

#[test]
fn split_starts_in_the_project_folder() {
    let Some(s) = server("split") else { return };
    let dir = project_dir(&s);
    let req = OpenReq {
        dir: dir.clone(),
        name: "proj".into(),
        what: What::Shell,
    };
    s.tmux.open(&req, SHELL).unwrap();
    let pane = s
        .tmux
        .run(&[
            "split-window",
            "-h",
            "-t",
            "den:1",
            "-c",
            "#{@den_dir}",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .unwrap();
    let pane = pane.trim();
    let want = format!("{}\n", dir.display());
    poll(
        || {
            s.tmux
                .run(&["display", "-p", "-t", pane, "#{pane_current_path}"])
                .unwrap()
        },
        |p| *p == want,
    );
}

#[test]
fn resume_is_tagged_and_not_opened_twice() {
    let Some(s) = server("resume") else { return };
    let marker = s.root().join("fake-claude-ran");
    let body = format!("touch '{}'\nsleep 30", marker.display());
    let shell = fake_claude(&s, &body);
    let req = OpenReq {
        dir: project_dir(&s),
        name: "proj".into(),
        what: What::Resume("abc".into()),
    };
    s.tmux.open(&req, &shell).unwrap();
    s.tmux.open(&req, &shell).unwrap();
    assert_eq!(windows(&s.tmux).len(), 2);
    assert!(s.tmux.window_of_session("abc").is_some());
    // The fake ran, not a real claude.
    poll(|| marker.exists(), |ran| *ran);
}

#[test]
fn tag_clears_when_claude_exits() {
    let Some(s) = server("clears") else { return };
    let shell = fake_claude(&s, "exit 0");
    let req = OpenReq {
        dir: project_dir(&s),
        name: "proj".into(),
        what: What::Resume("abc".into()),
    };
    s.tmux.open(&req, &shell).unwrap();
    poll(|| s.tmux.window_of_session("abc"), Option::is_none);
    let dead = s
        .tmux
        .run(&["display", "-p", "-t", "den:1", "#{pane_dead}"])
        .unwrap();
    assert_eq!(dead.trim(), "0");
}

#[test]
fn home_window_survives_alt_w_binding() {
    let Some(s) = server("alt-w") else { return };
    let binding = s.tmux.run(&["list-keys", "-T", "root", "M-w"]).unwrap();
    assert!(
        binding.contains(r##"if-shell -F "#{==:#{window_index},0}" '' kill-pane"##),
        "{binding}"
    );
    s.tmux.run(&["select-window", "-t", "den:0"]).unwrap();
    s.tmux
        .run(&["if", "-F", "#{==:#{window_index},0}", "", "kill-pane"])
        .unwrap();
    assert_eq!(windows(&s.tmux).first().map(String::as_str), Some("0 home"));
}

/// A temp world for the real `den home`: HOME, cache and config inside a
/// temp dir, one typed session whose folder exists, a fake `claude` (and
/// optionally a slow fake `git`) first on PATH. The server's window 0 runs
/// `wrapper`, which sets that environment and execs the built binary.
struct World {
    server: Server,
    proj: PathBuf,
    marker: PathBuf,
}

impl World {
    fn path(&self, rel: &str) -> PathBuf {
        self.server.root().join(rel)
    }

    fn window_zero(&self) -> String {
        self.server
            .tmux
            .run(&["capture-pane", "-p", "-t", "den:0"])
            .unwrap_or_default()
    }
}

/// The folder tmux is installed in. Homebrew keeps it outside /usr/bin.
fn tmux_dir() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v tmux"])
        .output()
        .unwrap();
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    path.parent().unwrap().to_path_buf()
}

fn den_world(name: &str, slow_git: bool) -> Option<World> {
    if !have_tmux() {
        return None;
    }
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let socket = format!("den-test-{}-{n}-{name}", std::process::id());
    let proj = root.join("work/proj");
    std::fs::create_dir_all(&proj).unwrap();
    let sessions = root.join("home/.claude/projects/-w-proj");
    std::fs::create_dir_all(&sessions).unwrap();
    let line = format!(
        r#"{{"parentUuid":null,"type":"user","entrypoint":"cli","cwd":"{}","sessionId":"s1","timestamp":"2026-11-14T10:00:00Z"}}"#,
        proj.display()
    );
    std::fs::write(sessions.join("s1.jsonl"), format!("{line}\n")).unwrap();

    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let marker = root.join("fake-claude-ran");
    write_script(
        &bin.join("claude"),
        &format!("printf '%s ' \"$@\" > '{}'\nsleep 30", marker.display()),
    );
    if slow_git {
        write_script(&bin.join("git"), "sleep 5\nexec /usr/bin/git \"$@\"");
    }
    let q = |p: PathBuf| den::tmux::sh_quote(&p.to_string_lossy());
    let wrapper = root.join("den-in-world");
    write_script(
        &wrapper,
        &format!(
            "export HOME={} XDG_CACHE_HOME={} XDG_CONFIG_HOME={} DEN_SOCKET={} SHELL=/bin/sh PATH={}:{}:/usr/bin:/bin\nexec {} \"$@\"",
            q(root.join("home")),
            q(root.join("cache")),
            q(root.join("config")),
            socket,
            q(bin),
            q(tmux_dir()),
            q(PathBuf::from(env!("CARGO_BIN_EXE_den"))),
        ),
    );
    let tmux = Tmux {
        socket,
        // Where Tmux::real() puts it under XDG_CONFIG_HOME.
        conf: root.join("config/den/tmux.conf"),
    };
    let server = Server { tmux, tmp };
    server.tmux.ensure_server(&wrapper, SHELL).unwrap();
    Some(World {
        server,
        proj,
        marker,
    })
}

#[test]
fn den_home_runs_in_window_zero_and_opens_a_tab() {
    let Some(w) = den_world("den-home", false) else {
        return;
    };
    let screen = poll(
        || w.window_zero(),
        |s| s.contains("PROJECTS") && s.lines().any(|l| l.contains(" proj ")),
    );
    println!("window 0:\n{screen}");
    w.server
        .tmux
        .run(&["send-keys", "-t", "den:0", "Enter"])
        .unwrap();
    let want = w.proj.to_string_lossy().into_owned();
    poll(
        || {
            w.server
                .tmux
                .run(&[
                    "list-windows",
                    "-t",
                    "den",
                    "-F",
                    "#{window_index}\t#{@den_dir}",
                ])
                .unwrap()
        },
        |out| {
            out.lines()
                .any(|l| l.split_once('\t') == Some(("1", want.as_str())))
        },
    );
    // The fake ran, not a real claude.
    let args = poll(
        || std::fs::read_to_string(&w.marker).unwrap_or_default(),
        |a| !a.is_empty(),
    );
    assert_eq!(args.trim(), "--resume s1");
    // Everything den wrote is inside the temp world.
    assert!(w.path("cache/den/index.json").is_file());
    assert!(w.path("config/den/config.toml").is_file());
    assert!(w.path("config/den/tmux.conf").is_file());
    let config = std::fs::read_to_string(w.path("config/den/config.toml")).unwrap();
    assert!(config.contains("~/") || config.contains("work"), "{config}");
}

#[test]
fn slow_git_does_not_block_the_list() {
    let Some(w) = den_world("slow-git", true) else {
        return;
    };
    let start = Instant::now();
    let screen = poll(
        || w.window_zero(),
        |s| s.contains("PROJECTS") && s.lines().any(|l| l.contains(" proj ")),
    );
    let took = start.elapsed();
    assert!(took < Duration::from_secs(2), "took {took:?}:\n{screen}");
    // Every git call sleeps 5 s, so this list was built without git: it
    // does not claim the folder has no git yet.
    assert!(!screen.contains("(no git)"), "{screen}");
}

#[test]
fn open_refuses_a_missing_folder() {
    let Some(s) = server("missing") else { return };
    let req = OpenReq {
        dir: s.root().join("deleted"),
        name: "gone".into(),
        what: What::Shell,
    };
    let err = s.tmux.open(&req, SHELL).unwrap_err();
    assert!(format!("{err:#}").contains("deleted"), "{err:#}");
    assert_eq!(windows(&s.tmux).len(), 1, "no tab opened in $HOME");
}

/// The command bound to `key` in the root table, as `list-keys` prints it.
fn bound_command(tmux: &Tmux, key: &str) -> String {
    let all = tmux.run(&["list-keys", "-T", "root"]).unwrap();
    let line = all
        .lines()
        .find(|l| l.split_whitespace().nth(3) == Some(key))
        .unwrap_or_else(|| panic!("{key} not bound:\n{all}"));
    let start = format!(" {key} ");
    let at = line.find(&start).unwrap_or_else(|| panic!("{key}: {line}"));
    line[at + start.len()..].trim().to_string()
}

fn panes(tmux: &Tmux, window: &str) -> usize {
    tmux.run(&["list-panes", "-t", window])
        .unwrap()
        .lines()
        .count()
}

/// Runs a bound command as if pressed in the current window.
fn press(s: &Server, key: &str) {
    let file = s.root().join("press.conf");
    std::fs::write(&file, bound_command(&s.tmux, key)).unwrap();
    s.tmux
        .run(&["source-file", &file.to_string_lossy()])
        .unwrap();
}

#[test]
fn splits_and_alt_c_do_nothing_in_the_home_tab() {
    let Some(s) = server("home-splits") else {
        return;
    };
    let shell = fake_claude(&s, "sleep 30");
    let req = OpenReq {
        dir: project_dir(&s),
        name: "proj".into(),
        what: What::Shell,
    };
    s.tmux.open(&req, &shell).unwrap();
    s.tmux.run(&["select-window", "-t", "den:0"]).unwrap();
    for key in ["M-\\\\", "M--", "M-c"] {
        press(&s, key);
    }
    assert_eq!(panes(&s.tmux, "den:0"), 1, "home tab still one pane");
    s.tmux.run(&["select-window", "-t", "den:1"]).unwrap();
    press(&s, "M--");
    assert_eq!(panes(&s.tmux, "den:1"), 2, "a tab still splits");
}

#[test]
fn a_running_server_picks_up_the_new_config() {
    let Some(s) = server("reload") else { return };
    s.tmux
        .ensure_server(Path::new("/bin/true"), "/opt/new/shell")
        .unwrap();
    let alt_c = bound_command(&s.tmux, "M-c");
    assert!(alt_c.contains("/opt/new/shell -ic"), "{alt_c}");
    let sessions = s.tmux.run(&["list-sessions", "-F", "#S"]).unwrap();
    assert_eq!(sessions.lines().collect::<Vec<_>>(), ["den"]);
    assert_eq!(windows(&s.tmux).len(), 1);
}

#[test]
fn odd_folder_names_open_in_the_folder() {
    let Some(s) = server("odd-dirs") else { return };
    let names = ["a b", "it's", "x#y", "x#[fg=red]y", "cost $HOME", "semi;"];
    for (k, name) in names.iter().enumerate() {
        let dir = s.root().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let req = OpenReq {
            dir: dir.clone(),
            name: (*name).into(),
            what: What::Shell,
        };
        s.tmux.open(&req, SHELL).unwrap();
        let target = format!("den:{}", k + 1);
        let want = format!("{}\n", dir.display());
        poll(
            || {
                s.tmux
                    .run(&["display", "-p", "-t", &target, "#{pane_current_path}"])
                    .unwrap()
            },
            |p| *p == want,
        );
        let tag = s
            .tmux
            .run(&["show", "-wv", "-t", &target, "@den_dir"])
            .unwrap();
        assert_eq!(tag, want, "@den_dir of {name:?}");
    }
}
