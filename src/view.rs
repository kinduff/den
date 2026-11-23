//! Draws the picker: overview, projects table, the selected project and its
//! sessions, key hints or every key, notice.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::activity::{bars, bars_scaled};
use crate::app::{App, Focus, Sort, ago, session_place};
use crate::config::contract_tilde;
use crate::git::GitStatus;
use crate::project::Kind;

/// From this size on the sessions sit beside the projects, not below.
const WIDE_MIN_W: u16 = 160;
const WIDE_MIN_H: u16 = 24;
/// A stacked screen shorter than this has no overview row.
const OVERVIEW_MIN_H: u16 = 16;
const GUTTER_W: usize = 4;
/// Days of activity on a project row, and on the long lines.
const ROW_DAYS: usize = 14;
const LONG_DAYS: usize = 84;
const SPARK_W: usize = ROW_DAYS + 2;
const SESS_W: usize = 6;
const LAST_W: usize = 10;
const BRANCH_W: usize = 14;
const STATE_MIN_W: usize = 12;
const TAB_MARK_W: usize = 10;
/// Rows of the keys under the list: a title and four rows of keys.
const DOCKED_ROWS: u16 = 5;
const HINTS: &[(&str, &str)] = &[
    ("enter", "open"),
    ("n", "new session"),
    ("t", "shell"),
    ("/", "filter"),
    ("s", "sort"),
    ("a", "all"),
    ("?", "keys"),
    ("q", "detach"),
];

/// Every key of the home tab: what `?` shows.
pub const HOME_KEYS: &[(&str, &str)] = &[
    ("j k ↑ ↓", "move"),
    ("h l ← →", "close, open children"),
    ("tab", "projects or sessions"),
    ("enter", "resume session, open project"),
    ("n", "new Claude session"),
    ("t", "shell in the project"),
    ("/", "filter, esc clears it"),
    ("s", "sort by activity or change"),
    ("a", "show script and agent sessions"),
    ("?", "show or hide these keys"),
    ("q ctrl-c", "detach, tabs keep running"),
];

/// Every key den binds in tmux: they work in every tab.
pub const TAB_KEYS: &[(&str, &str)] = &[
    ("Alt-0", "home tab"),
    ("Alt-1..9", "that tab"),
    ("Alt-[ Alt-]", "previous, next tab"),
    ("Alt-\\", "split side by side"),
    ("Alt--", "split top and bottom"),
    ("Alt-arrows", "move between panes"),
    ("Alt-z", "zoom the pane"),
    ("Alt-c", "Claude in a split"),
    ("Alt-w", "close the pane"),
];
const TAB_KEYS_NOTE: [&str; 2] = ["splits and close do nothing", "in the home tab"];

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn bars_style() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::DIM)
}

/// Draws the whole home screen. Never panics, whatever the frame size.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let s = Screen::new(area, app.keys_shown);

    let overview = overview_lines(app, s.overview.width as usize, s.wide);
    f.render_widget(Paragraph::new(overview), s.overview);
    for rule in s.rules {
        let line = Line::styled("─".repeat(rule.width as usize), dim());
        f.render_widget(Paragraph::new(line), rule);
    }

    let cols = Columns::new(s.head.width as usize);
    f.render_widget(Paragraph::new(projects_header(app, &cols)), s.head);
    draw_projects(f, app, &cols, s.list);
    let bar = vec![Line::styled("│", dim()); s.divider.height as usize];
    f.render_widget(Paragraph::new(bar), s.divider);
    f.render_widget(
        Paragraph::new(card_lines(app, s.card.width as usize)),
        s.card,
    );
    let shead = sessions_header(app, s.shead.width as usize, s.wide);
    f.render_widget(Paragraph::new(shead), s.shead);
    draw_sessions(f, app, s.slist);

    if let Some(keys) = docked_keys(area.width, split_at(area.width)) {
        f.render_widget(Paragraph::new(keys), s.docked);
    }
    let hints = hint_line(app, s.hints.width as usize);
    f.render_widget(Paragraph::new(hints), s.hints);
    if let Some(n) = &app.notice {
        let text = fit(&format!(" {n}"), s.notice.width as usize);
        let line = Line::from(text).style(Style::new().fg(Color::Yellow));
        f.render_widget(Paragraph::new(line), s.notice);
    }
    if s.boxed {
        draw_keys_box(f, area);
    }
}

/// Where each part of the home screen goes. A part that is not drawn has
/// no height.
struct Screen {
    /// Sessions beside the projects, with the selected project above them.
    wide: bool,
    overview: Rect,
    rules: [Rect; 2],
    head: Rect,
    list: Rect,
    divider: Rect,
    card: Rect,
    shead: Rect,
    slist: Rect,
    /// The keys under the list, on a screen wide enough for them.
    docked: Rect,
    /// The keys go in a box over the list instead.
    boxed: bool,
    hints: Rect,
    notice: Rect,
}

impl Screen {
    fn new(area: Rect, keys_shown: bool) -> Screen {
        use Constraint::{Fill, Length};
        if area.width < WIDE_MIN_W || area.height < WIDE_MIN_H {
            let overview = u16::from(area.height >= OVERVIEW_MIN_H);
            let [overview, head, list, _, shead, slist, _, hints, notice] = Layout::vertical([
                Length(overview),
                Length(1),
                Fill(3),
                Length(1),
                Length(1),
                Fill(2),
                Length(1),
                Length(1),
                Length(1),
            ])
            .areas(area);
            return Screen {
                wide: false,
                overview,
                rules: [Rect::ZERO; 2],
                head,
                list,
                divider: Rect::ZERO,
                card: Rect::ZERO,
                shead,
                slist,
                docked: Rect::ZERO,
                boxed: keys_shown,
                hints,
                notice,
            };
        }
        let left = split_at(area.width);
        let docked = keys_shown && docked_keys(area.width, left).is_some();
        // The docked keys take the place of the gap and the hints.
        let (keys, hints) = if docked { (DOCKED_ROWS, 0) } else { (0, 1) };
        let [overview, rule, body, keys_rule, keys, _, hints, notice] = Layout::vertical([
            Length(2),
            Length(1),
            Fill(1),
            Length(u16::from(docked)),
            Length(keys),
            Length(hints),
            Length(hints),
            Length(1),
        ])
        .areas(area);
        let [projects, divider, detail] =
            Layout::horizontal([Length(left), Length(1), Fill(1)]).areas(body);
        let [head, list] = Layout::vertical([Length(1), Fill(1)]).areas(projects);
        let [card, _, shead, slist] =
            Layout::vertical([Length(3), Length(1), Length(1), Fill(1)]).areas(detail);
        Screen {
            wide: true,
            overview,
            rules: [rule, keys_rule],
            head,
            list,
            divider,
            card,
            shead,
            slist,
            docked: keys,
            boxed: keys_shown && !docked,
            hints,
            notice,
        }
    }
}

/// The width of the projects pane on a wide screen.
fn split_at(width: u16) -> u16 {
    (u32::from(width.saturating_sub(1)) * 52 / 100) as u16
}

/// The first line of the projects list drawn, given the selected line.
fn list_offset(selected_line: usize, height: usize) -> usize {
    (selected_line + 1).saturating_sub(height)
}

/// The indexes into `app.rows()` that are on a screen of size `screen`.
pub fn project_rows_on_screen(app: &App, screen: Rect) -> Range<usize> {
    let height = Screen::new(screen, app.keys_shown).list.height as usize;
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

/// "1,340".
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (k, c) in digits.chars().enumerate() {
        if k > 0 && (digits.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The overview band: counts, then sessions per day for every project. One
/// row on a stacked screen, two on a wide one.
fn overview_lines(app: &App, w: usize, wide: bool) -> Vec<Line<'static>> {
    let o = app.overview();
    let s = |n: usize| if n == 1 { "" } else { "s" };
    let mut counts = vec![
        (o.tabs, format!("tab{} open", s(o.tabs))),
        (o.projects, format!("project{}", s(o.projects))),
        (o.sessions, format!("session{}", s(o.sessions))),
        (o.today, "today".to_string()),
        (o.week, "this week".to_string()),
    ];
    if wide && o.auto > 0 {
        counts.push((o.auto, "by scripts and agents".to_string()));
    }
    let mut spans = vec![Span::raw(" ")];
    if wide {
        spans.push(Span::styled("OVERVIEW", bold()));
    }
    for (k, (n, label)) in counts.iter().enumerate() {
        if wide || k > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(grouped(*n), bold()));
        spans.push(Span::styled(format!(" {label}"), dim()));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    let room = w.saturating_sub(used + 3);

    if !wide {
        let label = format!("{ROW_DAYS} days ");
        if width(&label) + ROW_DAYS <= room {
            let gap = w - used - 1 - width(&label) - ROW_DAYS;
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(label, dim()));
            let days = bars_scaled(&app.total_activity(ROW_DAYS));
            spans.push(Span::styled(days, bars_style()));
        }
        return vec![Line::from(spans)];
    }

    let busiest = format!("busiest today: {}", o.busiest.join(", "));
    if !o.busiest.is_empty() && width(&busiest) <= room {
        let gap = w - used - 1 - width(&busiest);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(busiest, dim()));
    }
    let (label, days) = long_days(w.saturating_sub(34));
    let all = vec![
        Span::styled(label, dim()),
        Span::styled(bars_scaled(&app.total_activity(days)), bars_style()),
        Span::styled("  sessions per day, all projects", dim()),
    ];
    vec![Line::from(spans), Line::from(all)]
}

/// The label and the number of days of a long activity line with `w` cells
/// to draw in: whole weeks, twelve at most.
fn long_days(w: usize) -> (String, usize) {
    let days = (w.saturating_sub(12) / 7 * 7).min(LONG_DAYS);
    (format!(" {} weeks  ", days / 7), days)
}

/// Widths of the projects table. Narrower panes drop the activity columns,
/// then branch and state.
struct Columns {
    name: usize,
    activity: bool,
    branch: usize,
    state: Option<usize>,
}

impl Columns {
    fn new(width: usize) -> Columns {
        let fixed = GUTTER_W + LAST_W + BRANCH_W + STATE_MIN_W;
        let activity = width >= fixed + SPARK_W + SESS_W + 20;
        if width < fixed + 12 {
            return Columns {
                name: width.saturating_sub(GUTTER_W + LAST_W),
                activity,
                branch: 0,
                state: None,
            };
        }
        let branch = if width >= 96 { BRANCH_W + 2 } else { BRANCH_W };
        let extra = if activity { SPARK_W + SESS_W } else { 0 };
        let rest = width - GUTTER_W - LAST_W - branch - extra;
        let name = (rest - STATE_MIN_W).min(40);
        Columns {
            name,
            activity,
            branch,
            state: Some(rest - name),
        }
    }
}

fn projects_header(app: &App, cols: &Columns) -> Line<'static> {
    let mut title = "PROJECTS".to_string();
    if app.filtering {
        title.push_str(&format!("  /{}_", app.filter));
    } else if !app.filter.is_empty() {
        title.push_str(&format!("  /{}", app.filter));
    }
    let last = match app.sort {
        Sort::Activity => "last ▾",
        Sort::Modified => "changed ▾",
    };
    let mut spans = vec![
        Span::raw(" ".repeat(GUTTER_W)),
        Span::styled(cell(&title, cols.name), bold()),
    ];
    if cols.activity {
        let days = format!("{ROW_DAYS} days");
        spans.push(Span::styled(cell(&days, SPARK_W), dim()));
        spans.push(Span::styled(format!("{:>4}  ", "sess"), dim()));
    }
    spans.push(Span::styled(pad(&fit(last, LAST_W), LAST_W), dim()));
    if let Some(state) = cols.state {
        spans.push(Span::styled(cell("branch", cols.branch), dim()));
        spans.push(Span::styled(cell("state", state), dim()));
    }
    Line::from(spans)
}

/// " today ─────": the separator above the first row of a section.
fn section_line(label: &str, w: usize) -> Line<'static> {
    let head = format!(" {label} ");
    let rule = "─".repeat(w.saturating_sub(width(&head) + 1));
    Line::styled(format!("{head}{rule}"), dim())
}

fn draw_projects(f: &mut Frame, app: &App, cols: &Columns, area: Rect) {
    let rows = app.rows();
    if rows.is_empty() {
        let msg = if app.filter.is_empty() {
            "  no projects yet".to_string()
        } else {
            format!("  nothing matches /{}", app.filter)
        };
        f.render_widget(Paragraph::new(Line::styled(msg, dim())), area);
        return;
    }
    let mut lines = Vec::new();
    let mut selected_line = 0;
    for (k, row) in rows.iter().enumerate() {
        if let Some(h) = row.header {
            lines.push(section_line(h, area.width as usize));
        }
        let open = rows
            .get(k + 1)
            .is_some_and(|n| app.tree.projects[n.project].parent == Some(row.project));
        let mut style = Style::new();
        if !app.tree.projects[row.project].exists {
            style = style.add_modifier(Modifier::DIM);
        }
        let selected = k == app.project_cursor;
        let focused = app.focus == Focus::Projects;
        if selected {
            selected_line = lines.len();
            style = style.add_modifier(selection(focused));
        }
        let cells = project_cells(app, cols, row.project, row.depth, open);
        lines.push(line_of(cells, selected && focused).style(style));
    }
    let height = area.height as usize;
    let offset = list_offset(selected_line, height);
    let shown: Vec<Line> = lines.into_iter().skip(offset).take(height).collect();
    f.render_widget(Paragraph::new(shown), area);
}

/// A piece of a line and how it is drawn.
type Piece = (String, Style);

/// The pieces as one line. A `plain` line drops their colors: the selected
/// row is one bar.
fn line_of(pieces: Vec<Piece>, plain: bool) -> Line<'static> {
    let span = |(text, style)| match plain {
        true => Span::raw(text),
        false => Span::styled(text, style),
    };
    Line::from(pieces.into_iter().map(span).collect::<Vec<Span>>())
}

/// `pieces` followed by the blanks that make them `w` cells wide; when they
/// do not fit with a two-cell gap, their text as one cut cell.
fn in_cell(mut pieces: Vec<Piece>, w: usize) -> Vec<Piece> {
    let used: usize = pieces.iter().map(|(text, _)| width(text)).sum();
    if used + 2 > w {
        let text: String = pieces.into_iter().map(|(text, _)| text).collect();
        return vec![(cell(&text, w), Style::new())];
    }
    pieces.push((" ".repeat(w - used), Style::new()));
    pieces
}

/// "✚3 ↑1", "clean" or "(no git)", each part in its color.
fn state_pieces(app: &App, kind: Kind, status: Option<&GitStatus>) -> Vec<Piece> {
    let status = match (kind, status) {
        (Kind::Folder, _) if !app.provisional => return vec![("(no git)".into(), dim())],
        (_, Some(s)) => s,
        (_, None) => return Vec::new(),
    };
    let parts = [
        ("✚", status.changed, Color::Yellow),
        ("↑", status.ahead, Color::Cyan),
        ("↓", status.behind, Color::Magenta),
    ];
    let mut out: Vec<Piece> = Vec::new();
    for (glyph, n, color) in parts {
        if n > 0 {
            if !out.is_empty() {
                out.push((" ".into(), Style::new()));
            }
            out.push((format!("{glyph}{n}"), Style::new().fg(color)));
        }
    }
    if out.is_empty() {
        out.push(("clean".into(), dim()));
    }
    out
}

/// "now" stands out, weeks and "never" step back.
fn age_style(age: &str) -> Style {
    if age == "now" {
        Style::new().fg(Color::Green)
    } else if age == "never" || age.ends_with("w ago") {
        dim()
    } else {
        Style::new()
    }
}

/// "3" for one tab, "3+" when the project has more: the lowest number.
fn tab_mark(numbers: &[u32]) -> String {
    match numbers {
        [] => String::new(),
        [one] => one.to_string(),
        [first, ..] => format!("{first}+"),
    }
}

fn project_cells(app: &App, cols: &Columns, i: usize, depth: u8, open: bool) -> Vec<Piece> {
    let p = &app.tree.projects[i];
    let tabs = tab_mark(&app.tab_numbers(&p.path));
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
    let age = ago(app.now, then);
    let green = Style::new().fg(Color::Green);
    let gutter = format!("{tabs:>w$} ", w = GUTTER_W - 1);
    let mut cells = vec![
        (gutter, green.add_modifier(Modifier::BOLD)),
        (name, Style::new()),
    ];
    if cols.activity {
        let days = bars(&app.activity(i, ROW_DAYS));
        cells.push((pad(&days, SPARK_W), bars_style()));
        let count = match app.session_counts(i).0 {
            0 => String::new(),
            n => n.to_string(),
        };
        cells.push((format!("{count:>4}  "), Style::new()));
    }
    cells.push((cell(&age, LAST_W), age_style(&age)));
    if let Some(state_w) = cols.state {
        let status = app.status.get(&p.path);
        let branch = status.and_then(|s| s.branch.as_deref()).unwrap_or("");
        cells.push((cell(branch, cols.branch), Style::new()));
        cells.extend(in_cell(state_pieces(app, p.kind, status), state_w));
    }
    cells
}

/// On a collapsed row: `⎇N` for worktrees, a plain count for other children.
fn child_badge(app: &App, i: usize) -> String {
    let ps = &app.tree.projects;
    let wt = worktrees(app, i);
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

fn worktrees(app: &App, i: usize) -> usize {
    let ps = &app.tree.projects;
    let is_worktree = |c: &&usize| ps[**c].kind == Kind::Worktree;
    ps[i].children.iter().filter(is_worktree).count()
}

/// The selected project above its sessions on a wide screen: name and
/// path, git and tabs, twelve weeks of activity.
fn card_lines(app: &App, w: usize) -> Vec<Line<'static>> {
    let Some(i) = app.selected() else {
        return Vec::new();
    };
    let p = &app.tree.projects[i];
    let name = fit(&format!(" {}", app.display_name(i)), w.saturating_sub(2));
    let path = contract_tilde(&p.path, &app.home).display().to_string();
    let path = fit(&path, w.saturating_sub(width(&name) + 3));
    let gap = w.saturating_sub(width(&name) + width(&path) + 1);
    let title = Line::from(vec![
        Span::styled(name, bold()),
        Span::raw(" ".repeat(gap)),
        Span::styled(path, dim()),
    ]);

    let status = app.status.get(&p.path);
    let mut facts: Vec<Vec<Piece>> = Vec::new();
    if let Some(branch) = status.and_then(|s| s.branch.as_deref()) {
        facts.push(vec![(branch.to_string(), Style::new())]);
    }
    let state = state_pieces(app, p.kind, status);
    if !state.is_empty() {
        facts.push(state);
    }
    match worktrees(app, i) {
        0 => {}
        1 => facts.push(vec![("1 worktree".into(), dim())]),
        n => facts.push(vec![(format!("{n} worktrees"), dim())]),
    }
    let tabs: Vec<String> = app
        .tab_numbers(&p.path)
        .iter()
        .map(u32::to_string)
        .collect();
    if !tabs.is_empty() {
        let label = if tabs.len() == 1 { "tab" } else { "tabs" };
        let text = format!("{label} {}", tabs.join(", "));
        facts.push(vec![(text, Style::new().fg(Color::Green))]);
    }
    if let Some(t) = app.modified.get(&p.path) {
        let what = match p.kind {
            Kind::Folder => "changed",
            Kind::Repo | Kind::Worktree => "last commit",
        };
        let text = format!("{what} {}", ago(app.now, Some(*t)));
        facts.push(vec![(text, dim())]);
    }
    let mut pieces: Vec<Piece> = Vec::new();
    for fact in facts {
        let gap = if pieces.is_empty() { " " } else { "   " };
        pieces.push((gap.into(), Style::new()));
        pieces.extend(fact);
    }

    let (label, days) = long_days(w);
    let activity = Line::from(vec![
        Span::styled(label, dim()),
        Span::styled(bars(&app.activity(i, days)), bars_style()),
    ]);
    vec![title, line_of(pieces, false), activity]
}

fn sessions_header(app: &App, w: usize, wide: bool) -> Line<'static> {
    let Some(p) = app.selected() else {
        return Line::styled(" SESSIONS", bold());
    };
    let project = &app.tree.projects[p];
    let title = match wide {
        true => " SESSIONS".to_string(),
        false => format!(" SESSIONS in {}", app.display_name(p)),
    };
    let title = fit(&title, w);
    let included = match (project.children.is_empty(), wide) {
        (true, _) => "",
        (false, true) => "  worktrees and subfolders included",
        (false, false) => " (worktrees and subfolders included)",
    };
    let typed = |i: usize| app.sessions.get(i).is_some_and(|s| s.is_typed());
    let auto = project.sessions.iter().filter(|r| !typed(r.idx)).count();
    let by = if wide { " by scripts and agents" } else { "" };
    let note = match (auto, app.show_all) {
        (0, _) => String::new(),
        (n, false) => format!("{n}{by} hidden, a shows"),
        (n, true) => format!("{n}{by} shown, a hides"),
    };
    // The note is flush right; the remark gives way to it.
    let room = w.saturating_sub(width(&title) + 1);
    let note = if width(&note) + 2 <= room {
        note
    } else {
        String::new()
    };
    let room = room.saturating_sub(width(&note));
    let included = if width(included) + 2 <= room || note.is_empty() {
        included
    } else {
        ""
    };
    let gap = room.saturating_sub(width(included));
    Line::from(vec![
        Span::styled(title, bold()),
        Span::styled(included, dim()),
        Span::raw(" ".repeat(if note.is_empty() { 0 } else { gap })),
        Span::styled(note, dim()),
    ])
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = app.selected() else {
        return;
    };
    let refs = app.session_rows();
    if refs.is_empty() {
        let line = Line::styled("   no sessions here, n starts one", dim());
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let line_w = area.width as usize;
    let place_w = (line_w / 5).clamp(8, 24);
    let mark_w = if line_w >= 60 { TAB_MARK_W } else { 0 };
    let focused = app.focus == Focus::Sessions;
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
            let title_w = line_w.saturating_sub(width(&head) + mark_w);
            let title = pad(&fit(&s.label(), title_w), title_w);
            let mark = match app.tab_of_session(&s.id) {
                Some(n) => format!("● tab {n}"),
                None => String::new(),
            };
            let mark = pad(&format!("{mark:>w$}", w = mark_w.saturating_sub(1)), mark_w);
            let mut style = Style::new();
            if !s.is_typed() {
                style = style.add_modifier(Modifier::DIM);
            }
            let selected = k == app.session_cursor;
            if selected {
                style = style.add_modifier(selection(focused));
            }
            let pieces = vec![
                (format!("{head}{title}"), Style::new()),
                (fit(&mark, mark_w), Style::new().fg(Color::Green)),
            ];
            line_of(pieces, selected && focused).style(style)
        })
        .collect();
    let height = area.height as usize;
    let offset = (app.session_cursor + 1).saturating_sub(height);
    let shown: Vec<Line> = lines.into_iter().skip(offset).take(height).collect();
    f.render_widget(Paragraph::new(shown), area);
}

/// The short list of keys. The gaps shrink before anything is cut.
fn hint_line(app: &App, w: usize) -> Line<'static> {
    let mut pairs: Vec<(&str, &str)> = if app.filtering {
        vec![("type", "to filter"), ("enter", "keep"), ("esc", "clear")]
    } else {
        HINTS.to_vec()
    };
    if !app.filtering && !app.filter.is_empty() {
        pairs.push(("esc", "clear filter"));
    }
    let text: usize = pairs.iter().map(|(k, d)| width(k) + 1 + width(d)).sum();
    let gaps = pairs.len().saturating_sub(1);
    let gap = (2..=3)
        .rev()
        .find(|g| 1 + text + g * gaps <= w)
        .unwrap_or(1);
    let mut spans = vec![Span::raw(" ")];
    for (k, (key, what)) in pairs.iter().enumerate() {
        if k > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
        }
        spans.push(Span::styled(*key, bold()));
        spans.push(Span::styled(format!(" {what}"), dim()));
    }
    Line::from(spans)
}

/// `keys` in columns of `rows` entries each: the pieces of every row, and
/// the width of the rows.
fn key_columns(keys: &[(&str, &str)], rows: usize) -> (Vec<Vec<Piece>>, usize) {
    let mut lines = vec![Vec::new(); rows];
    let mut total = 0;
    for column in keys.chunks(rows.max(1)) {
        let key_w = column.iter().map(|(k, _)| width(k)).max().unwrap_or(0) + 2;
        let what_w = column.iter().map(|(_, d)| width(d)).max().unwrap_or(0) + 2;
        for (r, line) in lines.iter_mut().enumerate() {
            let (key, what) = column.get(r).copied().unwrap_or(("", ""));
            line.push((pad(key, key_w), bold()));
            line.push((pad(what, what_w), dim()));
        }
        total += key_w + what_w;
    }
    (lines, total)
}

/// Every key in five rows under the list, the home tab's on the left and
/// the tab keys from where the sessions pane starts. None when the screen
/// is not `width` enough for them.
fn docked_keys(width: u16, split: u16) -> Option<Vec<Line<'static>>> {
    let rows = DOCKED_ROWS as usize - 1;
    let (home, home_w) = key_columns(HOME_KEYS, rows);
    let (tabs, tabs_w) = key_columns(TAB_KEYS, rows - 1);
    let note = TAB_KEYS_NOTE.join(" ");
    let second = (home_w + 1).max(split as usize + 2);
    let w = width as usize;
    if second + tabs_w.max(self::width(&note) + 2) > w + 2 {
        return None;
    }
    let hides = "? hides ";
    let title = vec![
        (" KEYS".to_string(), bold()),
        (pad("  home tab", second - 5), dim()),
        (pad("every tab", w - second - hides.len()), dim()),
        (hides.to_string(), dim()),
    ];
    let mut lines = vec![line_of(title, false)];
    for (r, home) in home.into_iter().enumerate() {
        let mut pieces = vec![(" ".to_string(), Style::new())];
        pieces.extend(home);
        pieces.push((" ".repeat(second - 1 - home_w), Style::new()));
        match tabs.get(r) {
            Some(tabs) => pieces.extend(tabs.iter().cloned()),
            None => pieces.push((note.clone(), dim())),
        }
        lines.push(line_of(pieces, false));
    }
    Some(lines)
}

/// Every key in a box over the middle of the screen, with what is behind
/// it dimmed: two columns, or one when the screen is narrow.
fn draw_keys_box(f: &mut Frame, area: Rect) {
    let (home, home_w) = key_columns(HOME_KEYS, HOME_KEYS.len());
    let (tabs, tabs_w) = key_columns(TAB_KEYS, TAB_KEYS.len());
    let label = |text: &str, w: usize| vec![(pad(text, w), dim())];
    let mut right = vec![label("every tab", tabs_w)];
    right.extend(tabs);
    right.extend(TAB_KEYS_NOTE.iter().map(|line| label(line, tabs_w)));
    let mut left = vec![label("home tab", home_w)];
    left.extend(home);

    let two_columns = area.width as usize >= home_w + tabs_w + 4;
    let (rows, inner_w): (Vec<Vec<Piece>>, usize) = if two_columns {
        let blank = label("", home_w);
        let rows = (0..left.len().max(right.len())).map(|r| {
            let mut row = left.get(r).unwrap_or(&blank).clone();
            row.extend(right.get(r).cloned().unwrap_or_default());
            row
        });
        (rows.collect(), home_w + tabs_w - 2)
    } else {
        left.push(Vec::new());
        left.extend(right);
        (left, home_w.max(tabs_w) - 2)
    };

    // A border, then two blank cells left and right and a blank row above
    // and below.
    let w = (inner_w as u16 + 6).min(area.width);
    let h = (rows.len() as u16 + 4).min(area.height);
    let place = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    f.buffer_mut().set_style(area, dim());
    f.render_widget(Clear, place);
    let closes = Line::styled(" ? or esc closes ", dim()).right_aligned();
    let frame = Block::bordered()
        .border_style(dim())
        .title(Span::styled(" KEYS ", bold()))
        .title_bottom(closes);
    f.render_widget(frame, place);
    let inside = Rect::new(
        place.x + 3.min(w),
        place.y + 2.min(h),
        w.saturating_sub(6),
        h.saturating_sub(4),
    );
    let lines: Vec<Line> = rows.into_iter().map(|r| line_of(r, false)).collect();
    f.render_widget(Paragraph::new(lines), inside);
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

/// `s` followed by the blanks that make it `w` cells wide.
fn pad(s: &str, w: usize) -> String {
    let mut out = s.to_string();
    out.extend(std::iter::repeat_n(' ', w.saturating_sub(width(s))));
    out
}

/// A column of exactly `w` cells: `s` cut to leave a two-cell gap, padded.
fn cell(s: &str, w: usize) -> String {
    pad(&fit(s, w.saturating_sub(2)), w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fixture::{self, INV, NOW};
    use crate::git::GitStatus;
    use crate::tmux::Tab;
    use crossterm::event::{KeyCode, KeyEvent};
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

    fn text(app: &App, w: u16, h: u16) -> String {
        lines(&render(app, w, h)).join("\n")
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::from(KeyCode::Char(c)), &fixture::probe());
    }

    /// The first line holding `needle`.
    fn line_with<'a>(ls: &'a [String], needle: &str) -> &'a str {
        match ls.iter().find(|l| l.contains(needle)) {
            Some(l) => l,
            None => panic!("no line with {needle:?} in\n{}", ls.join("\n")),
        }
    }

    /// The first cell showing `symbol` on the first line holding `needle`.
    fn cell_of<'a>(b: &'a Buffer, needle: &str, symbol: &str) -> &'a ratatui::buffer::Cell {
        let ls = lines(b);
        let y = ls.iter().position(|l| l.contains(needle)).unwrap() as u16;
        let x = (0..b.area.width).find(|&x| b[(x, y)].symbol() == symbol);
        &b[(
            x.unwrap_or_else(|| panic!("no {symbol:?} on {:?}", ls[y as usize])),
            y,
        )]
    }

    fn tab(index: u32, dir: &str, sessions: &[&str]) -> Tab {
        Tab {
            index,
            dir: dir.into(),
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn status(branch: &str, changed: u32, ahead: u32, behind: u32) -> GitStatus {
        GitStatus {
            branch: Some(branch.into()),
            changed,
            ahead,
            behind,
        }
    }

    /// The fixture with inventory selected and open, its git state known,
    /// and tab 3 resuming its session inv1.
    fn spec_app() -> App {
        let (mut app, probe) = fixture::app();
        fixture::select(&mut app, INV);
        app.on_key(KeyEvent::from(KeyCode::Right), &probe);
        app.status.insert(INV.into(), status("main", 3, 1, 0));
        app.status
            .insert(fixture::SCHED.into(), status("count-schedule", 0, 0, 0));
        app.tabs.push(tab(3, INV, &["inv1"]));
        app
    }

    #[test]
    fn draws_the_spec_screen_at_100x30() {
        let text = text(&spec_app(), 100, 30);
        println!("{text}");
        for want in [
            "PROJECTS",
            "  3 ▾ inventory",
            "⎇ count-schedule",
            "SESSIONS in inventory",
            "enter open",
            "? keys",
            "✚3 ↑1",
            "clean",
            "(no git)",
            "elsewhere",
            "gone",
            "Fix the schedule count",
            "(also)",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert!(!text.contains('│'), "sessions sit below, not beside");
    }

    #[test]
    fn a_wide_screen_puts_the_project_and_its_sessions_beside_the_list() {
        let mut app = spec_app();
        app.modified.insert(INV.into(), NOW - 7200);
        let ls = lines(&render(&app, 211, 47));
        println!("{}", ls.join("\n"));
        // Two overview rows and a rule, then both panes start on row 3.
        for want in ["PROJECTS", "│", "inventory", "/w/inventory"] {
            assert!(ls[3].contains(want), "{want:?} in {:?}", ls[3]);
        }
        for want in ["main", "✚3 ↑1", "worktrees", "tab 3", "last commit 2h ago"] {
            assert!(ls[4].contains(want), "{want:?} in {:?}", ls[4]);
        }
        assert!(ls[5].contains("12 weeks"), "{:?}", ls[5]);
        // The sessions header shares a row with the selected project.
        assert!(ls[7].contains("▾ inventory"), "{:?}", ls[7]);
        assert!(ls[7].contains("SESSIONS"), "{:?}", ls[7]);
        assert!(!ls[7].contains("SESSIONS in"), "{:?}", ls[7]);
        assert!(ls[8].contains("Fix the schedule count"), "{:?}", ls[8]);
    }

    #[test]
    fn the_top_list_is_cut_into_sections_with_a_rule() {
        let ls = lines(&render(&spec_app(), 100, 30));
        for label in ["today", "this week", "no sessions yet", "elsewhere"] {
            let line = line_with(&ls, &format!(" {label} ─"));
            assert!(line.starts_with(&format!(" {label} ─")), "{line:?}");
            assert!(line.ends_with('─'), "{line:?}");
        }
    }

    #[test]
    fn a_project_row_shows_its_activity_and_session_count() {
        let ls = lines(&render(&spec_app(), 100, 30));
        let header = line_with(&ls, "PROJECTS");
        for want in ["14 days", "sess", "last ▾", "branch", "state"] {
            assert!(header.contains(want), "{want:?} in {header:?}");
        }
        // Sessions ten and three days ago and yesterday; four are listed.
        let row = line_with(&ls, "▾ inventory");
        assert!(row.contains("  ▁      ▁ ▁  "), "{row:?}");
        assert!(row.contains("   4  2h ago"), "{row:?}");
        let never = line_with(&ls, "kpis");
        assert!(never.contains("never") && !never.contains('▁'), "{never:?}");
    }

    #[test]
    fn the_sorted_column_is_marked() {
        let mut app = spec_app();
        press(&mut app, 's');
        let ls = lines(&render(&app, 100, 30));
        assert!(line_with(&ls, "PROJECTS").contains("changed ▾"));
    }

    #[test]
    fn a_project_with_tabs_shows_the_lowest_tab_number() {
        let mut app = spec_app();
        app.tabs = vec![tab(5, INV, &[]), tab(3, INV, &[]), tab(12, "/w/kpis", &[])];
        let b = render(&app, 100, 30);
        let ls = lines(&b);
        assert!(line_with(&ls, "▾ inventory").starts_with(" 3+ ▾ inventory"));
        assert!(line_with(&ls, "kpis").starts_with(" 12   kpis"));
        assert!(line_with(&ls, "bongo-cli").starts_with("      bongo-cli"));
        let digit = cell_of(&b, "kpis", "1");
        assert_eq!(digit.fg, Color::Green);
        assert!(digit.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_session_row_names_the_tab_resuming_it() {
        let ls = lines(&render(&spec_app(), 100, 30));
        assert!(line_with(&ls, "Why is last scan empty").ends_with("● tab 3"));
        assert!(!line_with(&ls, "Fix the schedule count").contains('●'));
    }

    #[test]
    fn git_state_is_colored_except_on_the_selected_row() {
        let mut app = spec_app();
        app.status
            .insert("/w/bongo-cli".into(), status("main", 2, 0, 1));
        let b = render(&app, 100, 30);
        assert_eq!(cell_of(&b, "bongo-cli", "✚").fg, Color::Yellow);
        assert_eq!(cell_of(&b, "bongo-cli", "↓").fg, Color::Magenta);
        let selected = cell_of(&b, "▾ inventory", "✚");
        assert_eq!(selected.fg, Color::Reset, "one plain bar");
        assert!(selected.modifier.contains(Modifier::REVERSED));
        let ahead = cell_of(&b, "▾ inventory", "↑");
        assert_eq!(ahead.fg, Color::Reset);
    }

    #[test]
    fn the_overview_counts_and_draws_two_weeks_on_one_row() {
        let ls = lines(&render(&spec_app(), 100, 30));
        for want in [
            "1 tab open",
            "9 projects",
            "9 sessions",
            "1 today",
            "6 this week",
            // One session ten, five and three days ago, three yesterday,
            // one today.
            "14 days    ▃    ▃ ▃ █▃",
        ] {
            assert!(ls[0].contains(want), "{want:?} in {:?}", ls[0]);
        }
        assert!(ls[1].contains("PROJECTS"), "{:?}", ls[1]);
    }

    #[test]
    fn a_wide_overview_has_two_rows_and_a_rule() {
        let ls = lines(&render(&spec_app(), 211, 47));
        for want in [
            " OVERVIEW",
            "1 tab open",
            "9 sessions",
            "1 by scripts and agents",
            "busiest today: lab",
        ] {
            assert!(ls[0].contains(want), "{want:?} in {:?}", ls[0]);
        }
        assert!(ls[1].starts_with(" 12 weeks"), "{:?}", ls[1]);
        assert!(ls[1].contains("█▃  sessions per day, all projects"));
        assert_eq!(ls[2], "─".repeat(211));
    }

    #[test]
    fn large_counts_are_grouped_in_thousands() {
        let (mut app, _) = fixture::app();
        let auto = app.sessions.iter().position(|s| !s.is_typed()).unwrap();
        for n in 0..1339 {
            let mut more = app.sessions[auto].clone();
            more.id = format!("auto-{n}");
            app.sessions.push(more);
        }
        let ls = lines(&render(&app, 211, 47));
        assert!(
            ls[0].contains("   1,340 by scripts and agents"),
            "{:?}",
            ls[0]
        );
        assert!(ls[0].contains("   9 sessions"), "{:?}", ls[0]);
    }

    #[test]
    fn a_short_screen_has_no_overview() {
        let ls = lines(&render(&spec_app(), 100, 12));
        assert!(ls[0].contains("PROJECTS"), "{:?}", ls[0]);
    }

    #[test]
    fn the_sessions_header_says_how_many_are_left_out() {
        let mut app = spec_app();
        let ls = lines(&render(&app, 100, 30));
        assert!(line_with(&ls, "SESSIONS in").ends_with("1 hidden, a shows"));
        let wide = lines(&render(&app, 211, 47));
        let want = "1 by scripts and agents hidden, a shows";
        assert!(line_with(&wide, "SESSIONS").ends_with(want));
        press(&mut app, 'a');
        let ls = lines(&render(&app, 100, 30));
        assert!(line_with(&ls, "SESSIONS in").ends_with("1 shown, a hides"));
        fixture::select(&mut app, "/w/kpis");
        let ls = lines(&render(&app, 100, 30));
        assert!(line_with(&ls, "SESSIONS in").ends_with("SESSIONS in kpis"));
    }

    /// Whether every key and what it does is on screen.
    fn all_keys_drawn(text: &str) -> Result<(), String> {
        for (key, what) in HOME_KEYS.iter().chain(TAB_KEYS) {
            for want in [key, what] {
                if !text.contains(want) {
                    return Err(format!("missing {want:?} in\n{text}"));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn the_keys_stay_hidden_until_question_mark() {
        let mut app = spec_app();
        for (w, h) in [(100, 30), (211, 47)] {
            let text = text(&app, w, h);
            assert!(!text.contains("Alt-") && !text.contains("KEYS"), "{text}");
            assert!(text.contains("? keys"), "{text}");
        }
        press(&mut app, '?');
        for (w, h) in [(100, 30), (211, 47), (60, 40)] {
            all_keys_drawn(&text(&app, w, h)).unwrap();
        }
        press(&mut app, '?');
        assert!(!text(&app, 100, 30).contains("Alt-"));
    }

    #[test]
    fn a_wide_screen_docks_the_keys_under_the_list() {
        let mut app = spec_app();
        press(&mut app, '?');
        let ls = lines(&render(&app, 211, 47));
        println!("{}", ls.join("\n"));
        assert_eq!(ls[40], "─".repeat(211));
        assert!(ls[41].starts_with(" KEYS  home tab"), "{:?}", ls[41]);
        assert!(ls[41].contains("every tab"), "{:?}", ls[41]);
        assert!(ls[41].ends_with("? hides"), "{:?}", ls[41]);
        assert!(ls[42].starts_with(" j k ↑ ↓  move"), "{:?}", ls[42]);
        assert!(ls[42].contains("Alt-0"), "{:?}", ls[42]);
        assert!(ls[45].contains("splits and close do nothing in the home tab"));
        let text = ls.join("\n");
        assert!(!text.contains('┌'), "no box on a wide screen");
        assert!(!text.contains("enter open"), "the keys replace the hints");
        assert!(ls[3].contains("PROJECTS") && ls[8].contains("Fix the schedule count"));
    }

    #[test]
    fn a_smaller_screen_shows_the_keys_in_a_box() {
        let mut app = spec_app();
        press(&mut app, '?');
        let b = render(&app, 100, 30);
        let ls = lines(&b);
        println!("{}", ls.join("\n"));
        let top = line_with(&ls, "┌");
        assert!(top.contains("KEYS"), "{top:?}");
        assert!(line_with(&ls, "└").contains("? or esc closes"));
        assert!(line_with(&ls, "j k ↑ ↓").contains("Alt-0"), "two columns");
        assert!(ls.join("\n").contains("enter open"), "hints stay");
        // What is behind the box is dimmed.
        let behind = cell_of(&b, "9 projects", "9");
        assert!(behind.modifier.contains(Modifier::DIM));
        let inside = cell_of(&b, "j k ↑ ↓", "j");
        assert!(!inside.modifier.contains(Modifier::DIM));
    }

    #[test]
    fn every_tmux_binding_is_listed_in_the_keys() {
        let listed: Vec<&str> = TAB_KEYS
            .iter()
            .flat_map(|(keys, _)| keys.split(' '))
            .collect();
        for (key, _) in crate::tmux::KEYS {
            let name = match key.strip_prefix("M-").unwrap() {
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => "Alt-1..9".to_string(),
                "Left" | "Right" | "Up" | "Down" => "Alt-arrows".to_string(),
                r"\\" => r"Alt-\".to_string(),
                other => format!("Alt-{other}"),
            };
            assert!(listed.contains(&name.as_str()), "{key} is not listed");
        }
    }

    #[test]
    fn provisional_tree_does_not_claim_no_git() {
        let mut app = spec_app();
        app.provisional = true;
        let text = text(&app, 100, 30);
        assert!(!text.contains("(no git)"), "{text}");
        assert!(text.contains("PROJECTS") && text.contains("inventory"));
    }

    #[test]
    fn outside_session_path_is_drawn_from_home() {
        let (mut app, _) = fixture::app();
        app.home = "/w".into();
        fixture::select(&mut app, INV);
        let text = text(&app, 100, 30);
        assert!(text.contains("~  (also)"), "{text}");
    }

    #[test]
    fn a_kept_filter_shows_how_to_clear_it() {
        let (mut app, probe) = fixture::app();
        assert!(!text(&app, 100, 30).contains("esc clear filter"));
        for k in ['/', 'z'] {
            app.on_key(KeyEvent::from(KeyCode::Char(k)), &probe);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter), &probe);
        let hints = text(&app, 100, 30);
        assert!(hints.contains("esc clear filter"), "{hints}");
        assert!(hints.contains("q detach"), "{hints}");
    }

    #[test]
    fn rows_on_screen_follow_the_scroll() {
        let (mut app, _) = fixture::app();
        let rows = app.rows().len();
        let screen = Rect::new(0, 0, 100, 12);
        let top = project_rows_on_screen(&app, screen);
        assert_eq!(top.start, 0);
        assert!(!top.is_empty() && top.end < rows, "must scroll: {top:?}");
        app.project_cursor = rows - 1;
        let bottom = project_rows_on_screen(&app, screen);
        assert_eq!(bottom.end, rows, "{bottom:?}");
        assert!(
            bottom.start > 0 && bottom.len() <= top.len() + 1,
            "{bottom:?}"
        );
    }

    #[test]
    fn rows_on_screen_are_the_rows_drawn() {
        let (mut app, _) = fixture::app();
        app.project_cursor = app.rows().len() - 1;
        for (w, h) in [(100, 12), (100, 30), (211, 47), (211, 24)] {
            let on = project_rows_on_screen(&app, Rect::new(0, 0, w, h));
            let text = text(&app, w, h);
            for (k, row) in app.rows().iter().enumerate() {
                let name = &app.tree.projects[row.project].name;
                let drawn = [format!("  {name}  "), format!("▸ {name}  ")]
                    .iter()
                    .any(|row| text.contains(row));
                assert_eq!(on.contains(&k), drawn, "{name} at {w}x{h}\n{text}");
            }
        }
    }

    #[test]
    fn draws_at_40x8_without_panic() {
        let b = render(&spec_app(), 40, 8);
        assert!(lines(&b).join("\n").contains("PROJECTS"));
    }

    #[test]
    fn draws_at_extreme_sizes_without_panic() {
        let (empty, mut full) = (App::new(NOW), spec_app());
        let sizes = [
            (0, 0),
            (1, 1),
            (2, 3),
            (200, 60),
            (200, 1),
            (1, 60),
            (160, 24),
        ];
        for keys in [false, true] {
            full.keys_shown = keys;
            for (w, h) in sizes {
                render(&empty, w, h);
                render(&full, w, h);
            }
        }
    }

    #[test]
    fn draws_empty_app_with_first_run_notice() {
        let mut app = App::new(NOW);
        app.notice = Some("root picked: ~/www".into());
        let text = text(&app, 80, 20);
        assert!(text.contains("root picked: ~/www"), "{text}");
        assert!(text.contains("PROJECTS"), "{text}");
    }

    #[test]
    fn no_middle_dot_in_any_drawn_cell() {
        let mut app = spec_app();
        app.show_all = true;
        app.keys_shown = true;
        let empty = App::new(NOW);
        for (w, h) in [(100, 30), (211, 47)] {
            for b in [render(&app, w, h), render(&empty, w, h)] {
                for cell in b.content() {
                    assert!(!cell.symbol().contains('\u{b7}'));
                }
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
        for (w, h) in [(60, 20), (211, 47)] {
            let ls = lines(&render(&app, w, h));
            let text = ls.join("\n");
            let hits = |needle: &str| ls.iter().filter(|l| l.contains(needle)).count();
            assert_eq!(hits("▸ xxxx"), 1, "{text}");
            assert_eq!(hits("日本語"), 1, "{text}");
            // The rows around the long one are each on a line of their own,
            // in order.
            let at = |name: &str| ls.iter().position(|l| l.contains(name)).unwrap();
            let order = [at("▸ lab  "), at("▸ xxxx"), at("  bongo-cli  ")];
            assert!(
                order.is_sorted() && order[0] < order[2],
                "{order:?}\n{text}"
            );
        }
    }
}
