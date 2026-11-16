use std::path::Path;
use std::process::Command;

use den::tmux::{Tmux, parse_tmux_version};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => println!("den {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h") => print!("{}", usage()),
        Some("home") => std::process::exit(den::home::main()),
        None => {
            if let Err(message) = launch() {
                eprintln!("den: {message}");
                std::process::exit(1);
            }
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            eprint!("{}", usage());
            std::process::exit(2);
        }
    }
}

fn usage() -> String {
    let home = dirs::home_dir().unwrap_or_default();
    let config = den::config::contract_tilde(&den::config::config_path(), &home);
    format!(
        "usage: den [home | --help | --version]

  den         open the picker: starts den's own tmux server (tmux -L den)
              if needed and attaches to it
  den home    the picker itself, run by that server in tab 0

config: {}
",
        config.display()
    )
}

/// `den`: checks tmux and claude, starts the private server if needed and
/// attaches to it. Every error is one line.
fn launch() -> Result<(), String> {
    let version = Command::new("tmux")
        .arg("-V")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
    check(version.as_deref(), on_path("claude"))?;
    let one_line = |e: anyhow::Error| format!("{e:#}").replace('\n', " ");
    let exe = std::env::current_exe().map_err(|e| format!("finding the den binary: {e}"))?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let tmux = Tmux::real();
    tmux.ensure_server(&exe, &shell).map_err(one_line)?;
    tmux.attach().map_err(one_line)
}

/// The launcher's checks: `version_out` is the output of `tmux -V`, None
/// when tmux could not be run. An unparseable version counts as new enough.
fn check(version_out: Option<&str>, claude_on_path: bool) -> Result<(), String> {
    let Some(version) = version_out else {
        return Err("tmux is not installed (den needs tmux 3.2 or newer)".into());
    };
    if let Some(found) = parse_tmux_version(version)
        && found < (3, 2)
    {
        return Err(format!(
            "found {}, den needs tmux 3.2 or newer",
            version.trim()
        ));
    }
    if !claude_on_path {
        return Err("claude is not on PATH".into());
    }
    Ok(())
}

/// Whether `PATH` holds an executable file called `name`.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(Path::new(&dir).join(name))
            .is_ok_and(|md| md.is_file() && md.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn launcher_checks_and_their_texts() {
        assert_eq!(
            check(None, true),
            Err("tmux is not installed (den needs tmux 3.2 or newer)".into())
        );
        assert_eq!(
            check(Some("tmux 3.1\n"), true),
            Err("found tmux 3.1, den needs tmux 3.2 or newer".into())
        );
        assert_eq!(check(Some("tmux 3.2"), true), Ok(()));
        assert_eq!(check(Some("tmux 3.2a"), true), Ok(()));
        assert_eq!(check(Some("tmux 3.8"), true), Ok(()));
        assert_eq!(check(Some("tmux master"), true), Ok(()));
        assert_eq!(
            check(Some("tmux 3.8"), false),
            Err("claude is not on PATH".into())
        );
    }
}
