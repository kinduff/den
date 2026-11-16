//! The `den` binary's arguments. Never runs `den` without arguments: that
//! would start and attach to a tmux server.

use std::process::{Command, Output};

fn den(args: &[&str]) -> Output {
    let tmp = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_den"))
        .args(args)
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_CACHE_HOME", tmp.path().join("cache"))
        .env("DEN_SOCKET", format!("den-test-{}-cli", std::process::id()))
        .output()
        .unwrap()
}

#[test]
fn help_prints_usage_and_exits_zero() {
    for flag in ["--help", "-h"] {
        let out = den(&[flag]);
        assert_eq!(out.status.code(), Some(0), "{flag}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("den home"), "{flag}: {text}");
        assert!(text.contains("den/config.toml"), "{flag}: {text}");
    }
}

#[test]
fn unknown_command_prints_usage_to_stderr_and_exits_two() {
    for arg in ["bogus", "tree"] {
        let out = den(&[arg]);
        assert_eq!(out.status.code(), Some(2), "{arg}");
        assert!(out.stdout.is_empty(), "{arg}");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(text.contains("den home"), "{arg}: {text}");
    }
}
