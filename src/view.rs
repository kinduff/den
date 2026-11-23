//! Draws the picker: projects table, sessions list, key hints, notice.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus, Sort, ago, session_place};
use crate::project::Kind;

const LAST_W: usize = 10;
const BRANCH_W: usize = 14;
const STATE_MIN_W: usize = 12;
const HINTS: &[(&str, &str)] = &[
    ("enter", "open"),
    ("n", "new session"),
    ("t", "shell"),
    ("/", "filter"),
    ("s", "sort"),
    ("a", "all"),
    ("q", "detach"),
];

/// Draws the whole home screen. Never panics, whatever the frame size.
pub fn draw(f: &mut Frame, app: &App) {
    let [head, list, _, shead, slist, _, hints, notice] = areas(f.area());

    let cols = Columns::new(head.width as usize);
    f.render_widget(Paragraph::new(projects_header(app, &cols)), head);
    draw_projects(f, app, &cols, list);
    f.render_widget(Paragraph::new(sessions_header(app, shead.width)), shead);
    draw_sessions(f, app, slist);
    f.render_widget(Paragraph::new(hint_line(app)), hints);
    if let Some(n) = &app.notice {
        let text = fit(&format!(" {n}"), notice.width as usize);
        let line = Line::from(text).style(Style::new().fg(Color::Yellow));
        f.render_widget(Paragraph::new(line), notice);
    }
}

/// The screen's parts: projects header and list, gap, sessions header and
/// list, gap, hints, notice.
fn areas(screen: Rect) -> [Rect; 8] {
    use Constraint::{Fill, Length};
    Layout::vertical([
        Length(1),
        Fill(3),
        Length(1),
        Length(1),
        Fill(2),
        Length(1),
        Length(1),
        Length(1),
    ])
    .areas(screen)
}

/// The first line of the projects list drawn, given the selected line.
fn list_offset(selected_line: usize, height: usize) -> usize {
    (selected_line + 1).saturating_sub(height)
}

/// The indexes into `app.rows()` that are on a screen of size `screen`.
pub fn project_rows_on_screen(app: &App, screen: Rect) -> Range<usize> {
    let height = areas(screen)[1].height as usize;
    let rows = app.rows();
    let mut line = 0;
    let mut starts = Vec::with_capacity(rows.len());
    for r in &rows {
        if r.header.is_some() {
            line += 1;
        }
        starts.push(line);
        line += 1;
    }
    let selected = starts.get(app.project_cursor).copied().unwrap_or(0);
    let offset = list_offset(selected, height);
    let first = starts.iter().position(|&l| l >= offset);
    let end = starts.iter().position(|&l| l >= offset + height);
    let first = first.unwrap_or(rows.len());
    first..end.unwrap_or(rows.len()).max(first)
}

/// Widths of the projects table. Narrow screens drop branch and state.
struct Columns {
    name: usize,
    state: Option<usize>,
}

impl Columns {
    fn new(width: usize) -> Columns {
        let rest = width.saturating_sub(1 + LAST_W + BRANCH_W + STATE_MIN_W);
        if rest < 12 {
            return Columns {
                name: width.saturating_sub(1 + LAST_W),
                state: None,
            };
        }
        let name = rest.min(40);
        Columns {
            name,
            state: Some(width - 1 - LAST_W - BRANCH_W - name),
        }
    }
}

fn projects_header(app: &App, cols: &Columns) -> Line<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut title = "PROJECTS".to_string();
    if app.filtering {
        title.push_str(&format!("  /{}_", app.filter));
    } else if !app.filter.is_empty() {
        title.push_str(&format!("  /{}", app.filter));
    }
    let last = match app.sort {
        Sort::Activity => "last",
        Sort::Modified => "changed",
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            cell(&title, cols.name),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(cell(last, LAST_W), dim),
    ];
    if let Some(state) = cols.state {
        spans.push(Span::styled(cell("branch", BRANCH_W), dim));
        spans.push(Span::styled(cell("state", state), dim));
    }
    Line::from(spans)
}

fn draw_projects(f: &mut Frame, app: &App, cols: &Columns, area: Rect) {
    let rows = app.rows();
    let dim = Style::new().add_modifier(Modifier::DIM);
    if rows.is_empty() {
        let msg = if app.filter.is_empty() {
            "  no projects yet".to_string()
        } else {
            format!("  nothing matches /{}", app.filter)
        };
        f.render_widget(Paragraph::new(Line::styled(msg, dim)), area);
        return;
    }
    let line_w = area.width as usize;
    let mut lines = Vec::new();
    let mut selected_line = 0;
    for (k, row) in rows.iter().enumerate() {
        if let Some(h) = row.header {
            lines.push(Line::styled(cell(&format!(" {h}"), line_w), dim));
        }
        let open = rows
            .get(k + 1)
            .is_some_and(|n| app.tree.projects[n.project].parent == Some(row.project));
        let mut style = Style::new();
        if !app.tree.projects[row.project].exists {
            style = style.add_modifier(Modifier::DIM);
        }
        if k == app.project_cursor {
            selected_line = lines.len();
            style = style.add_modifier(selection(app.focus == Focus::Projects));
        }
        lines.push(project_line(app, cols, row.project, row.depth, open).style(style));
    }
    let height = area.height as usize;
    let offset = list_offset(selected_line, height);
    let shown: Vec<Line> = lines.into_iter().skip(offset).take(height).collect();
    f.render_widget(Paragraph::new(shown), area);
}

fn project_line(app: &App, cols: &Columns, i: usize, depth: u8, open: bool) -> Line<'static> {
    let p = &app.tree.projects[i];
    let mark = if app.tabs.iter().any(|t| t.dir == p.path) {
        "●"
    } else {
        " "
    };
    let fold = match (p.children.is_empty(), open) {
        (true, _) => "  ",
        (false, true) => "▾ ",
        (false, false) => "▸ ",
    };
    let wt = if p.kind == Kind::Worktree { "⎇ " } else { "" };
    let left = format!("{}{fold}{wt}{}", "  ".repeat(depth as usize), p.name);
    let badge = if open {
        String::new()
    } else {
        child_badge(app, i)
    };
    let name = if badge.is_empty() {
        cell(&left, cols.name)
    } else {
        let badge = format!("   {badge}");
        let room = cols.name.saturating_sub(width(&badge) + 2);
        cell(&format!("{}{badge}", fit(&left, room)), cols.name)
    };
    let then = match app.sort {
        Sort::Activity => p.last_activity,
        Sort::Modified => app.modified.get(&p.path).copied(),
    };
    let mut spans = vec![
        Span::raw(mark),
        Span::raw(name),
        Span::raw(cell(&ago(app.now, then), LAST_W)),
    ];
    if let Some(state_w) = cols.state {
        let status = app.status.get(&p.path);
        let branch = status.and_then(|s| s.branch.as_deref()).unwrap_or("");
        let state = match (p.kind, status) {
            (Kind::Folder, _) if !app.provisional => "(no git)".to_string(),
            (_, Some(s)) => {
                let parts: Vec<String> = [("✚", s.changed), ("↑", s.ahead), ("↓", s.behind)]
                    .iter()
                    .filter(|(_, n)| *n > 0)
                    .map(|(g, n)| format!("{g}{n}"))
                    .collect();
                if parts.is_empty() {
                    "clean".to_string()
                } else {
                    parts.join(" ")
                }
            }
            (_, None) => String::new(),
        };
        spans.push(Span::raw(cell(branch, BRANCH_W)));
        spans.push(Span::raw(cell(&state, state_w)));
    }
    Line::from(spans)
}

/// On a collapsed row: `⎇N` for worktrees, a plain count for other children.
fn child_badge(app: &App, i: usize) -> String {
    let ps = &app.tree.projects;
    let wt = ps[i]
        .children
        .iter()
        .filter(|&&c| ps[c].kind == Kind::Worktree)
        .count();
    let other = ps[i].children.len() - wt;
    let mut parts = Vec::new();
    if wt > 0 {
        parts.push(format!("⎇{wt}"));
    }
    if other > 0 {
        parts.push(other.to_string());
    }
    parts.join(" ")
}

fn sessions_header(app: &App, w: u16) -> Line<'static> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let Some(p) = app.selected() else {
        return Line::styled(" SESSIONS", bold);
    };
    let title = fit(&format!(" SESSIONS in {}", app.display_name(p)), w as usize);
    let mut spans = vec![Span::styled(title, bold)];
    if !app.tree.projects[p].children.is_empty() {
        spans.push(Span::styled(
            " (worktrees and subfolders included)",
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = app.selected() else {
        return;
    };
    let refs = app.session_rows();
    if refs.is_empty() {
        let line = Line::styled(
            "   no sessions here, n starts one",
            Style::new().add_modifier(Modifier::DIM),
        );
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let line_w = area.width as usize;
    let place_w = (line_w / 5).clamp(8, 24);
    let lines: Vec<Line> = refs
        .iter()
        .enumerate()
        .map(|(k, r)| {
            let s = &app.sessions[r.idx];
            let age = ago(app.now, s.last_ts);
            let age = age.strip_suffix(" ago").unwrap_or(&age);
            let tag = match (r.also, s.is_typed()) {
                (true, true) => "(also)",
                (true, false) => "(also) (auto)",
                (false, false) => "(auto)",
                (false, true) => "",
            };
            let place = session_place(&app.tree, p, s, &app.home);
            let place = if tag.is_empty() {
                place
            } else {
                let room = place_w.saturating_sub(width(tag) + 4);
                format!("{}  {tag}", fit(&place, room))
            };
            let head = format!("   {}{}", cell(age, 6), cell(&place, place_w));
            let title = fit(&s.label(), line_w.saturating_sub(width(&head)));
            let mut style = Style::new();
            if !s.is_typed() {
                style = style.add_modifier(Modifier::DIM);
            }
            if k == app.session_cursor {
                style = style.add_modifier(selection(app.focus == Focus::Sessions));
            }
            Line::from(cell(&format!("{head}{title}"), line_w)).style(style)
        })
        .collect();
    let height = area.height as usize;
    let offset = (app.session_cursor + 1).saturating_sub(height);
    let shown: Vec<Line> = lines.into_iter().skip(offset).take(height).collect();
    f.render_widget(Paragraph::new(shown), area);
}

fn hint_line(app: &App) -> Line<'static> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut pairs: Vec<(&str, &str)> = if app.filtering {
        vec![("type", "to filter"), ("enter", "keep"), ("esc", "clear")]
    } else {
        HINTS.to_vec()
    };
    if !app.filtering && !app.filter.is_empty() {
        pairs.push(("esc", "clear filter"));
    }
    let mut spans = vec![Span::raw(" ")];
    for (k, (key, what)) in pairs.iter().enumerate() {
        if k > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(*key, bold));
        spans.push(Span::styled(format!(" {what}"), dim));
    }
    Line::from(spans)
}

fn selection(focused: bool) -> Modifier {
    if focused {
        Modifier::REVERSED
    } else {
        Modifier::BOLD
    }
}

fn char_width(c: char) -> usize {
    let mut buf = [0; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

fn width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// `s` cut on a char boundary to at most `w` cells, ending in '…' when cut.
fn fit(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    if w > 0 {
        out.push('…');
    }
    out
}

/// A column of exactly `w` cells: `s` cut to leave a two-cell gap, padded.
fn cell(s: &str, w: usize) -> String {
    let mut out = fit(s, w.saturating_sub(2));
    let pad = w.saturating_sub(width(&out));
    out.extend(std::iter::repeat_n(' ', pad));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fixture::{self, INV, NOW};
    use crate::git::GitStatus;
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn render(app: &App, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        t.backend().buffer().clone()
    }

    /// One string per screen row, wide glyphs' trailing cells skipped.
    fn lines(b: &Buffer) -> Vec<String> {
        let a = b.area;
        (0..a.height)
            .map(|y| {
                let mut s = String::new();
                let mut skip = 0;
                for x in 0..a.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let sym = b[(x, y)].symbol();
                    s.push_str(sym);
                    skip = ratatui::text::Span::raw(sym).width().saturating_sub(1);
                }
                s.trim_end().to_string()
            })
            .collect()
    }

    fn spec_app() -> App {
        let (mut app, probe) = fixture::app();
        fixture::select(&mut app, INV);
        app.on_key(
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Right),
            &probe,
        );
        app.status.insert(
            INV.into(),
            GitStatus {
                branch: Some("main".into()),
                changed: 3,
                ahead: 1,
                behind: 0,
            },
        );
        app.status.insert(
            fixture::SCHED.into(),
            GitStatus {
                branch: Some("count-schedule".into()),
                ..GitStatus::default()
            },
        );
        app.tabs.push(crate::tmux::Tab {
            index: 3,
            dir: INV.into(),
            sessions: Vec::new(),
        });
        app
    }

    #[test]
    fn draws_the_spec_screen_at_100x30() {
        let b = render(&spec_app(), 100, 30);
        let text = lines(&b).join("\n");
        println!("{text}");
        for want in [
            "PROJECTS",
            "inventory",
            "⎇ count-schedule",
            "SESSIONS in inventory",
            "enter open",
            "✚3 ↑1",
            "clean",
            "(no git)",
            "elsewhere",
            "gone",
            "Fix the schedule count",
            "(also)",
            "●",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
    }

    #[test]
    fn provisional_tree_does_not_claim_no_git() {
        let mut app = spec_app();
        app.provisional = true;
        let text = lines(&render(&app, 100, 30)).join("\n");
        assert!(!text.contains("(no git)"), "{text}");
        assert!(text.contains("PROJECTS") && text.contains("inventory"));
    }

    #[test]
    fn outside_session_path_is_drawn_from_home() {
        let (mut app, _) = fixture::app();
        app.home = "/w".into();
        fixture::select(&mut app, INV);
        let text = lines(&render(&app, 100, 30)).join("\n");
        assert!(text.contains("~  (also)"), "{text}");
    }

    #[test]
    fn a_kept_filter_shows_how_to_clear_it() {
        let (mut app, probe) = fixture::app();
        let hint = |app: &App| lines(&render(app, 100, 30)).join("\n");
        assert!(!hint(&app).contains("esc clear filter"));
        for k in ['/', 'z'] {
            app.on_key(crossterm::event::KeyEvent::from(KeyCode::Char(k)), &probe);
        }
        app.on_key(crossterm::event::KeyEvent::from(KeyCode::Enter), &probe);
        assert!(hint(&app).contains("esc clear filter"), "{}", hint(&app));
    }

    #[test]
    fn rows_on_screen_follow_the_scroll() {
        let (mut app, _) = fixture::app();
        let rows = app.rows().len();
        let screen = Rect::new(0, 0, 100, 12);
        let height = areas(screen)[1].height as usize;
        assert!(height < rows, "the list must scroll at this size");
        let top = project_rows_on_screen(&app, screen);
        assert_eq!(top.start, 0);
        assert!(top.len() <= height && !top.is_empty(), "{top:?}");
        app.project_cursor = rows - 1;
        let bottom = project_rows_on_screen(&app, screen);
        assert_eq!(bottom.end, rows, "{bottom:?}");
        assert!(bottom.len() <= height && bottom.start > 0, "{bottom:?}");
    }

    #[test]
    fn draws_at_40x8_without_panic() {
        let b = render(&spec_app(), 40, 8);
        assert!(lines(&b).join("\n").contains("PROJECTS"));
    }

    #[test]
    fn draws_at_extreme_sizes_without_panic() {
        let (empty, full) = (App::new(NOW), spec_app());
        for (w, h) in [(0, 0), (1, 1), (2, 3), (200, 60), (200, 1), (1, 60)] {
            render(&empty, w, h);
            render(&full, w, h);
        }
    }

    #[test]
    fn draws_empty_app_with_first_run_notice() {
        let mut app = App::new(NOW);
        app.notice = Some("root picked: ~/www".into());
        let text = lines(&render(&app, 80, 20)).join("\n");
        assert!(text.contains("root picked: ~/www"), "{text}");
        assert!(text.contains("PROJECTS"), "{text}");
    }

    #[test]
    fn no_middle_dot_in_any_drawn_cell() {
        let mut app = spec_app();
        app.show_all = true;
        for b in [render(&app, 100, 30), render(&App::new(NOW), 100, 30)] {
            for cell in b.content() {
                assert!(!cell.symbol().contains('\u{b7}'));
            }
        }
    }

    #[test]
    fn long_and_wide_text_stays_on_one_row() {
        let (mut app, _) = fixture::app();
        let long = "x".repeat(300);
        let mut sessions = app.sessions.clone();
        let mut tree = app.tree.clone();
        let inv = tree
            .projects
            .iter()
            .position(|p| p.path == std::path::Path::new(INV))
            .unwrap();
        tree.projects[inv].name = long.clone();
        let sched = sessions.iter().position(|s| s.id == "sched1").unwrap();
        sessions[sched].title = Some("日本語のタイトル 🚀🚀 ".repeat(20));
        app.set_data(tree, sessions);
        fixture::select(&mut app, INV);
        let b = render(&app, 60, 20);
        let ls = lines(&b);
        let text = ls.join("\n");
        let rows = app.rows();
        let projects = &ls[..ls.iter().position(|l| l.contains("SESSIONS")).unwrap()];
        let first = projects.iter().position(|l| l.contains("xxxx")).unwrap();
        let hits = projects.iter().filter(|l| l.contains("xxxx")).count();
        assert_eq!(hits, 1, "{text}");
        let at = app.project_cursor;
        // Every row from the top to the "elsewhere" separator sits on the
        // screen line right below the one before it.
        let end = rows.iter().position(|r| r.header.is_some()).unwrap();
        for (k, r) in rows.iter().enumerate().take(end) {
            let line = &ls[first + k - at];
            let name = &app.tree.projects[r.project].name;
            let shown: String = name.chars().take(10).collect();
            assert!(line.contains(&shown), "row {k} {shown}\n{text}");
        }
        assert_eq!(
            ls.iter().filter(|l| l.contains("日本語")).count(),
            1,
            "{text}"
        );
    }
}
