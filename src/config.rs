//! `~/.config/den/config.toml`: scan roots, scan depth, hidden paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn two() -> u8 {
    2
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    #[serde(default = "two")]
    pub depth: u8,
    #[serde(default)]
    pub hide: Vec<PathBuf>,
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// `$XDG_CONFIG_HOME/den`, or `~/.config/den`. The same on Linux and macOS.
pub fn config_dir() -> PathBuf {
    den_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir(),
        ".config",
    )
}

/// `$XDG_CACHE_HOME/den`, or `~/.cache/den`. The same on Linux and macOS.
pub fn cache_dir() -> PathBuf {
    den_dir(
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        dirs::home_dir(),
        ".cache",
    )
}

/// An absolute XDG value wins; otherwise the dot folder under home.
fn den_dir(xdg: Option<PathBuf>, home: Option<PathBuf>, dot: &str) -> PathBuf {
    match xdg {
        Some(x) if x.is_absolute() => x.join("den"),
        _ => home.unwrap_or_default().join(dot).join("den"),
    }
}

/// The folder whose direct children hold the most typed sessions: each
/// session home counts once for its parent folder. Ties go to the shorter
/// path, then the smaller one.
pub fn pick_root(typed_homes: &[PathBuf]) -> Option<PathBuf> {
    let mut counts: HashMap<&Path, usize> = HashMap::new();
    for home in typed_homes {
        if let Some(parent) = home.parent() {
            *counts.entry(parent).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .min_by(|(pa, ca), (pb, cb)| {
            cb.cmp(ca)
                .then_with(|| pa.components().count().cmp(&pb.components().count()))
                .then_with(|| pa.cmp(pb))
        })
        .map(|(p, _)| p.to_path_buf())
}

fn expand_tilde(p: &Path, home: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// `p` with a leading `home` written as `~`; unchanged when `home` is empty.
pub fn contract_tilde(p: &Path, home: &Path) -> PathBuf {
    match p.strip_prefix(home) {
        Ok(_) if home.as_os_str().is_empty() => p.to_path_buf(),
        Ok(rest) if rest.as_os_str().is_empty() => PathBuf::from("~"),
        Ok(rest) => Path::new("~").join(rest),
        _ => p.to_path_buf(),
    }
}

fn map_paths(cfg: &Config, f: impl Fn(&Path) -> PathBuf) -> Config {
    Config {
        roots: cfg.roots.iter().map(|p| f(p)).collect(),
        depth: cfg.depth,
        hide: cfg.hide.iter().map(|p| f(p)).collect(),
    }
}

/// Loads the config at `path`, or writes a first one whose root is
/// `pick_root(typed_homes)`. The bool is true when the file was just written.
pub fn load_or_init(path: &Path, typed_homes: &[PathBuf]) -> anyhow::Result<(Config, bool)> {
    let home = dirs::home_dir().unwrap_or_default();
    if path.exists() {
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        return Ok((map_paths(&cfg, |p| expand_tilde(p, &home)), false));
    }
    let cfg = Config {
        roots: pick_root(typed_homes).into_iter().collect(),
        depth: two(),
        hide: Vec::new(),
    };
    let on_disk = map_paths(&cfg, |p| contract_tilde(p, &home));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, toml::to_string(&on_disk)?)?;
    Ok((cfg, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_root_takes_the_parent_with_most_typed_sessions() {
        let homes: Vec<PathBuf> = [
            "/h/www/inventory",
            "/h/www/inventory",
            "/h/www/bongo",
            "/h/www/lab/zombies",
            "/h/www/lab/decomp",
            "/h",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(pick_root(&homes), Some(PathBuf::from("/h/www")));
        assert_eq!(pick_root(&[]), None);
    }

    #[test]
    fn load_or_init_writes_once_and_expands_tilde() {
        let home = dirs::home_dir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("den/config.toml");
        let homes = vec![home.join("www/a"), home.join("www/b"), home.join("x")];

        let (cfg, written) = load_or_init(&path, &homes).unwrap();
        assert!(written);
        assert_eq!(cfg.roots, vec![home.join("www")]);
        assert_eq!(cfg.depth, 2);
        assert!(cfg.hide.is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"~/www\""), "{text}");

        let (again, written) = load_or_init(&path, &[]).unwrap();
        assert!(!written);
        assert_eq!(again, cfg);

        std::fs::write(&path, "roots = [\"~/code\", \"/srv\"]\nhide = [\"~\"]\n").unwrap();
        let (cfg, written) = load_or_init(&path, &homes).unwrap();
        assert!(!written);
        assert_eq!(cfg.roots, vec![home.join("code"), PathBuf::from("/srv")]);
        assert_eq!(cfg.depth, 2);
        assert_eq!(cfg.hide, vec![home.clone()]);
    }

    #[test]
    fn no_root_found_still_writes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("den/config.toml");
        let (cfg, written) = load_or_init(&path, &[]).unwrap();
        assert!(written && cfg.roots.is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("roots = []"), "{text}");
    }

    #[test]
    fn den_dirs_follow_xdg_or_the_dot_folder_on_every_system() {
        let home = Some(PathBuf::from("/Users/a"));
        assert_eq!(
            den_dir(None, home.clone(), ".config"),
            PathBuf::from("/Users/a/.config/den")
        );
        assert_eq!(
            den_dir(Some(PathBuf::from("/x/cfg")), home.clone(), ".config"),
            PathBuf::from("/x/cfg/den")
        );
        assert_eq!(
            den_dir(Some(PathBuf::from("relative")), home.clone(), ".cache"),
            PathBuf::from("/Users/a/.cache/den")
        );
        assert_eq!(
            den_dir(Some(PathBuf::new()), home, ".cache"),
            PathBuf::from("/Users/a/.cache/den")
        );
    }
}
