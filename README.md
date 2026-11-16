# den

A project and session picker for Claude Code. It lists your projects with their
Claude sessions, and opens each one as a tab in its own tmux server.

No more `cd` into a folder, `claude --resume`, find out the session lives in the
parent folder, `cd ..`, try again. Run `den` from anywhere and pick.

## What it shows

- Every folder you have used with Claude, plus the other folders under your
  scan roots, so a first session can start from the same place.
- Sessions grouped under their project. A session started in one folder that
  also worked in another project is listed there too, marked `(also)`.
- Git worktrees nested under their repository.
- For each project: last Claude activity, branch, and git state.
- Sessions whose folder no longer exists, under `gone`. They resume from the
  nearest folder that still exists, and so do new sessions and shells opened
  there.

Sessions started by scripts and agents are hidden until you press `a`.

## Install

Needs Linux or macOS, tmux 3.2 or newer, and `claude` on your `PATH`.

```sh
cargo install --path .
den
```

## Home tab

Tab 0 is always the picker.

| Key | Does |
| --- | --- |
| `j` `k`, arrows | move |
| `l` `h`, right, left | open or close a project's children |
| `tab` | switch between projects and sessions |
| `enter` | resume the session, or open the project |
| `n` | new Claude session in the project |
| `t` | shell in the project |
| `/` | filter, `enter` keeps it, `esc` clears it |
| `s` | sort by last Claude activity or by last change |
| `a` | show or hide sessions started by scripts and agents |
| `q` | detach, tabs keep running |

Opening a session that already has a tab goes to that tab.

## Tab keys

No prefix key. These work in every tab.

| Key | Does |
| --- | --- |
| `Alt-0` | home tab |
| `Alt-1` to `Alt-9` | that tab |
| `Alt-[` `Alt-]` | previous, next tab |
| `Alt-\` | split side by side, in the project folder (not in the home tab) |
| `Alt--` | split top and bottom, in the project folder (not in the home tab) |
| `Alt-arrows` | move between panes |
| `Alt-z` | zoom the pane |
| `Alt-c` | new Claude session in a split (not in the home tab) |
| `Alt-w` | close the pane (never the home tab) |

On macOS the Option key must act as Alt, or none of these work:

- iTerm2: Settings, Profiles, Keys, set "Left Option key" to "Esc+".
- Terminal.app: Settings, Profiles, Keyboard, tick "Use Option as Meta key".
- Ghostty: `macos-option-as-alt = true`.

Tabs live in a tmux server of their own (`tmux -L den`). Your normal tmux and
`~/.tmux.conf` are not touched. Close the terminal and run `den` again: the
tabs are still there. They do not survive a reboot.

After upgrading den, `tmux -L den kill-server` restarts the picker itself
(this closes every tab).

## Config

`~/.config/den/config.toml`, written on first run and read again every 30
seconds while the home tab is on screen (git is only asked then, too):

```toml
roots = ["~/www"]   # folders to scan for projects
depth = 2           # how deep to look under each root
hide = []           # folders to leave out
```

den writes only `~/.config/den/` and `~/.cache/den/`. It reads
`~/.claude/projects` and never writes there.
