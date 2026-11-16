//! The private tmux server den owns: socket `den`, session `den`, window 0
//! `home` running the picker, every other window a tab opened from it.
//! Every tmux call goes through `Tmux::run`, which always passes `-L`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

const SESSION: &str = "den";

/// What a new tab runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    Resume(String),
    NewClaude,
    Shell,
}

/// A tab to open: its folder, its window name and what it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenReq {
    pub dir: PathBuf,
    pub name: String,
    pub what: What,
}

/// A handle on one private tmux server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tmux {
    pub socket: String,
    pub conf: PathBuf,
}

/// Placeholder in `KEYS` for the command that starts a new Claude session.
const NEW_CLAUDE: &str = "{new_claude}";

/// Prefix-free key bindings: (key as written in the tmux config, command).
/// Splits and new panes do nothing in window 0, where `M-w` cannot close
/// them.
const KEYS: &[(&str, &str)] = &[
    ("M-0", "select-window -t :0"),
    ("M-1", "select-window -t :1"),
    ("M-2", "select-window -t :2"),
    ("M-3", "select-window -t :3"),
    ("M-4", "select-window -t :4"),
    ("M-5", "select-window -t :5"),
    ("M-6", "select-window -t :6"),
    ("M-7", "select-window -t :7"),
    ("M-8", "select-window -t :8"),
    ("M-9", "select-window -t :9"),
    ("M-[", "previous-window"),
    ("M-]", "next-window"),
    (
        "M-\\\\",
        r##"if -F "#{==:#{window_index},0}" "" { split-window -h -c "#{@den_dir}" }"##,
    ),
    (
        "M--",
        r##"if -F "#{==:#{window_index},0}" "" { split-window -v -c "#{@den_dir}" }"##,
    ),
    ("M-Left", "select-pane -L"),
    ("M-Right", "select-pane -R"),
    ("M-Up", "select-pane -U"),
    ("M-Down", "select-pane -D"),
    ("M-z", "resize-pane -Z"),
    ("M-w", r##"if -F "#{==:#{window_index},0}" "" "kill-pane""##),
    (
        "M-c",
        r##"if -F "#{==:#{window_index},0}" "" { split-window -h -c "#{@den_dir}" {new_claude} }"##,
    ),
];

impl Tmux {
    /// Socket `den` (or `$DEN_SOCKET`), config at `~/.config/den/tmux.conf`.
    pub fn real() -> Tmux {
        let conf = crate::config::config_dir().join("tmux.conf");
        Tmux {
            socket: socket_name(std::env::var("DEN_SOCKET").ok()),
            conf,
        }
    }

    /// Runs `tmux -L <socket> <args>` and returns its stdout.
    pub fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = self.command().args(args).output().context("running tmux")?;
        if !out.status.success() {
            bail!(
                "tmux {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Writes the config and starts the server, whose config creates session
    /// `den` with window 0 `home`. A running server reads the config again,
    /// so new keys arrive after an upgrade; the config's own guard keeps it
    /// from creating a second session.
    pub fn ensure_server(&self, den_bin: &Path, shell: &str) -> anyhow::Result<()> {
        if let Some(parent) = self.conf.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&self.conf, config_text(den_bin, shell))
            .with_context(|| format!("writing {}", self.conf.display()))?;
        let conf = self.conf.to_string_lossy();
        if self.has_session() {
            self.run(&["source-file", &conf])?;
            return Ok(());
        }
        self.run(&["-f", &conf, "start-server"])?;
        if !self.has_session() {
            // A server was already running without the session: the config
            // was not read, so read it now.
            self.run(&["source-file", &conf])?;
        }
        if !self.has_session() {
            bail!(
                "tmux did not create session {SESSION} from {}",
                self.conf.display()
            );
        }
        Ok(())
    }

    /// Attaches to session `den`, replacing this process. Inside den's own
    /// tmux it switches to window 0 instead.
    pub fn attach(&self) -> anyhow::Result<()> {
        let inside = std::env::var("TMUX")
            .map(|v| tmux_env_is_socket(&v, &self.socket))
            .unwrap_or(false);
        let args = self.attach_args(inside);
        if inside {
            let args: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();
            self.run(&args)?;
            return Ok(());
        }
        use std::os::unix::process::CommandExt;
        // TMUX may name the user's own server; attaching to ours is not nesting.
        let err = Command::new("tmux").args(&args).env_remove("TMUX").exec();
        Err(err).context("exec tmux attach")
    }

    /// The tmux arguments `attach` uses, `-L <socket>` included.
    pub fn attach_args(&self, inside_den: bool) -> Vec<String> {
        let rest: &[&str] = if inside_den {
            &["select-window", "-t", "den:0"]
        } else {
            &["attach-session", "-t", SESSION]
        };
        let mut args = vec!["-L".to_string(), self.socket.clone()];
        args.extend(rest.iter().map(|s| s.to_string()));
        args
    }

    /// Opens a tab for `req`, or switches to the tab already resuming that
    /// session. A folder that is not there is an error: tmux would quietly
    /// open the tab in `$HOME`.
    pub fn open(&self, req: &OpenReq, shell: &str) -> anyhow::Result<()> {
        if let What::Resume(id) = &req.what
            && let Some(window) = self.window_of_session(id)
        {
            self.run(&["select-window", "-t", &window])?;
            return Ok(());
        }
        if !req.dir.is_dir() {
            bail!("{} is not a folder", req.dir.display());
        }
        let dir = req.dir.to_string_lossy();
        let name = chain_safe(&escape_format(&req.name));
        let start_dir = chain_safe(&escape_format(&dir));
        // `-c` alone loses folders whose names tmux reads as formats.
        let command = chain_safe(&format!(
            "cd -- {} && {}",
            sh_quote(&dir),
            launch_command(&req.what, shell, &self.socket)
        ));
        let dir_value = chain_safe(&dir);
        let target = format!("{SESSION}:");
        // One invocation, so the tags are set before the pane's own
        // `tmux set -p -u` can reach the server.
        let mut args: Vec<&str> = vec![
            "new-window",
            "-t",
            &target,
            "-n",
            &name,
            "-c",
            &start_dir,
            &command,
            ";",
            "set",
            "-w",
            "@den_dir",
            &dir_value,
        ];
        let id_value;
        if let What::Resume(id) = &req.what {
            id_value = chain_safe(id);
            args.extend([";", "set", "-p", "@den_session", &id_value]);
        }
        self.run(&args)?;
        Ok(())
    }

    /// The id of the window whose pane is tagged with session `id`.
    pub fn window_of_session(&self, id: &str) -> Option<String> {
        let out = self
            .run(&[
                "list-panes",
                "-s",
                "-t",
                SESSION,
                "-F",
                "#{window_id}\t#{@den_session}",
            ])
            .ok()?;
        out.lines().find_map(|line| {
            let (window, tag) = line.split_once('\t')?;
            (tag == id).then(|| window.to_string())
        })
    }

    /// The project folder of every tab but window 0.
    pub fn open_dirs(&self) -> Vec<PathBuf> {
        self.poll_tabs().0
    }

    /// The project folder of every tab but window 0, and whether window 0
    /// is the active window with a client attached. When tmux cannot be
    /// asked, the home tab counts as watched.
    pub fn poll_tabs(&self) -> (Vec<PathBuf>, bool) {
        match self.run(&[
            "list-windows",
            "-t",
            SESSION,
            "-F",
            "#{window_index}\t#{window_active}\t#{session_attached}\t#{@den_dir}",
        ]) {
            Ok(out) => parse_tabs(&out),
            Err(_) => (Vec::new(), true),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.arg("-L").arg(&self.socket);
        cmd
    }

    fn has_session(&self) -> bool {
        self.run(&["has-session", "-t", SESSION]).is_ok()
    }
}

/// The socket name: `den_socket` when set and not empty, else `den`.
fn socket_name(den_socket: Option<String>) -> String {
    den_socket
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SESSION.to_string())
}

/// `poll_tabs` from `list-windows` lines `index\tactive\tattached\tdir`.
fn parse_tabs(out: &str) -> (Vec<PathBuf>, bool) {
    let mut dirs = Vec::new();
    let mut home_active = false;
    for line in out.lines() {
        let mut f = line.splitn(4, '\t');
        let (Some(index), Some(active), Some(attached), dir) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        if index == "0" {
            home_active = active == "1" && attached != "0";
        } else if let Some(dir) = dir.filter(|d| !d.is_empty()) {
            dirs.push(PathBuf::from(dir));
        }
    }
    (dirs, home_active)
}

/// (major, minor) from `tmux -V` output: "tmux 3.8", "tmux 3.2a",
/// "tmux next-3.5". None for a build without a number ("tmux master").
pub fn parse_tmux_version(out: &str) -> Option<(u32, u32)> {
    let word = out.split_whitespace().last()?;
    let word = word.strip_prefix("next-").unwrap_or(word);
    let (major, rest) = word.split_once('.')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, digits.parse().ok()?))
}

/// Whether a `TMUX` value (`<socket path>,<pid>,<session>`) names `socket`.
fn tmux_env_is_socket(tmux_env: &str, socket: &str) -> bool {
    let path = tmux_env.split(',').next().unwrap_or("");
    Path::new(path)
        .file_name()
        .is_some_and(|name| name == socket)
}

/// The tmux config den writes: options, the tab strip, prefix-free keys,
/// and session `den` with window 0 `home` when it does not exist yet.
pub fn config_text(den_bin: &Path, shell: &str) -> String {
    let mut text = String::new();
    for line in [
        "set -g prefix None",
        "set -g mouse on",
        "set -g base-index 0",
        "set -g renumber-windows off",
        "set -g status-position top",
        "set -g escape-time 0",
        "set -g default-terminal \"tmux-256color\"",
        "set -g history-limit 50000",
        "set -g allow-rename off",
        "set -g automatic-rename off",
        "set -g status-left \"\"",
        "set -g status-right \"\"",
        "set -g window-status-format \"#I #W\"",
        "set -g window-status-current-format \"#I #W\"",
        "set -g window-status-current-style bold",
        "set -g window-status-separator \" | \"",
    ] {
        text.push_str(line);
        text.push('\n');
    }
    let new_claude = conf_quote(&launch_command(&What::NewClaude, shell, SESSION));
    for (key, command) in KEYS {
        let command = command.replace(NEW_CLAUDE, &new_claude);
        text.push_str(&format!("bind -n {key} {command}\n"));
    }
    let home_loop = format!(
        "while :; do {} home; sleep 0.2; done",
        sh_quote(&den_bin.to_string_lossy())
    );
    let home_command = format!("sh -c {}", sh_quote(&home_loop));
    text.push_str(&format!(
        "if -F \"#{{S:#{{?#{{==:#{{session_name}},{SESSION}}},x,}}}}\" \"\" {{\n  new-session -d -s {SESSION} -n home {}\n}}\n",
        conf_quote(&home_command)
    ));
    text
}

/// The shell command a new tab runs.
pub fn launch_command(what: &What, shell: &str, socket: &str) -> String {
    let shell = sh_quote(shell);
    match what {
        What::Resume(id) => {
            let script = format!(
                "claude --resume {}; tmux -L {} set -p -u @den_session; exec {shell}",
                sh_quote(id),
                sh_quote(socket)
            );
            format!("{shell} -ic {}", sh_quote(&script))
        }
        What::NewClaude => {
            let script = format!("claude; exec {shell}");
            format!("{shell} -ic {}", sh_quote(&script))
        }
        What::Shell => shell,
    }
}

/// Quotes `s` as one sh word. Words made only of characters sh never
/// treats specially are left as they are.
pub fn sh_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Quotes `s` as one double-quoted string for the tmux config parser.
fn conf_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('$', r"\$");
    format!("\"{escaped}\"")
}

/// Escapes `#` for tmux arguments that are format-expanded (`-c`, `-n`).
fn escape_format(s: &str) -> String {
    s.replace('#', "##")
}

/// tmux reads an argument ending in `;` as a command separator; `\;` keeps
/// it literal.
fn chain_safe(s: &str) -> String {
    match s.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_quote_handles_quotes_and_spaces() {
        assert_eq!(sh_quote("a b'c"), r#"'a b'\''c'"#);
    }

    #[test]
    fn resume_command_clears_its_tag_and_drops_to_a_shell() {
        assert_eq!(
            launch_command(&What::Resume("abc".into()), "/bin/zsh", "den"),
            "/bin/zsh -ic 'claude --resume abc; tmux -L den set -p -u @den_session; exec /bin/zsh'"
        );
    }

    #[test]
    fn new_claude_and_shell_commands() {
        assert_eq!(
            launch_command(&What::NewClaude, "/bin/zsh", "den"),
            "/bin/zsh -ic 'claude; exec /bin/zsh'"
        );
        assert_eq!(launch_command(&What::Shell, "/bin/zsh", "den"), "/bin/zsh");
    }

    #[test]
    fn config_has_no_prefix_and_every_spec_key() {
        let text = config_text(Path::new("/usr/bin/den"), "/bin/zsh");
        assert!(text.contains("prefix None"), "{text}");
        let keys = [
            "M-0", "M-1", "M-2", "M-3", "M-4", "M-5", "M-6", "M-7", "M-8", "M-9", "M-[", "M-]",
            "M-\\\\", "M--", "M-w", "M-z", "M-c", "M-Left",
        ];
        for key in keys {
            let start = format!("bind -n {key} ");
            assert!(
                text.lines().any(|l| l.starts_with(&start)),
                "no binding for {key} in:\n{text}"
            );
        }
        assert!(
            text.contains("sh -c 'while :; do /usr/bin/den home; sleep 0.2; done'"),
            "{text}"
        );
    }

    #[test]
    fn config_has_no_middle_dot() {
        let text = config_text(Path::new("/usr/bin/den"), "/bin/zsh");
        assert!(!text.contains('\u{b7}'));
    }

    #[test]
    fn attach_args_outside_and_inside_den() {
        let tmux = Tmux {
            socket: "den".into(),
            conf: PathBuf::from("/x/tmux.conf"),
        };
        assert_eq!(
            tmux.attach_args(false),
            ["-L", "den", "attach-session", "-t", "den"]
        );
        assert_eq!(
            tmux.attach_args(true),
            ["-L", "den", "select-window", "-t", "den:0"]
        );
    }

    #[test]
    fn tmux_env_names_the_socket_by_file_name() {
        assert!(tmux_env_is_socket("/tmp/tmux-1000/den,123,0", "den"));
        assert!(!tmux_env_is_socket("/tmp/tmux-1000/default,123,0", "den"));
        assert!(!tmux_env_is_socket("", "den"));
    }

    #[test]
    fn tmux_versions() {
        assert_eq!(parse_tmux_version("tmux 3.8"), Some((3, 8)));
        assert_eq!(parse_tmux_version("tmux 3.2a\n"), Some((3, 2)));
        assert_eq!(parse_tmux_version("tmux next-3.5"), Some((3, 5)));
        assert_eq!(parse_tmux_version("tmux 2.9"), Some((2, 9)));
        assert_eq!(parse_tmux_version("tmux master"), None);
        assert_eq!(parse_tmux_version(""), None);
    }

    #[test]
    fn socket_name_comes_from_den_socket() {
        assert_eq!(socket_name(None), "den");
        assert_eq!(socket_name(Some(String::new())), "den");
        assert_eq!(socket_name(Some("den-test-1".into())), "den-test-1");
    }

    #[test]
    fn tabs_parse_dirs_and_whether_home_is_watched() {
        let out = "0\t1\t1\t\n1\t0\t1\t/w/a b\n2\t0\t1\t\n";
        assert_eq!(parse_tabs(out), (vec![PathBuf::from("/w/a b")], true));
        assert!(
            !parse_tabs(
                "0	0	1	
1	1	1	/w
"
            )
            .1,
            "another tab"
        );
        assert!(!parse_tabs("0\t1\t0\t\n").1, "detached");
    }

    #[test]
    fn chain_safe_escapes_a_trailing_separator() {
        assert_eq!(chain_safe("a;"), r"a\;");
        assert_eq!(chain_safe("a;b"), "a;b");
    }
}
