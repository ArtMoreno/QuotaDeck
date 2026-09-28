use crate::cache::CacheStore;
use crate::cli::{AgentOrder, PercentStyle};
use crate::dashboard_prefs::{
    color_rgb, DashboardField, DashboardPreferences, DashboardProvider, ProviderPreference,
};
use crate::herdr::AgentPane;
use crate::model::{format_percent, Provider, ProviderSnapshot, Severity, UsageWindow};
use crate::opencode::LocalUsage;
use crate::presentation::{
    dashboard_cells, format_reset, format_reset_eta, meter, CellSlot, RowStyle, WindowCell,
    METER_CELLS,
};
use crate::trend;
use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{
    self, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use time::UtcOffset;

const OUTER_BACKGROUND: Color = Color::Rgb {
    r: 14,
    g: 16,
    b: 20,
};
const PANEL_BACKGROUND: Color = Color::Rgb {
    r: 27,
    g: 30,
    b: 37,
};
const TEXT: Color = Color::Rgb {
    r: 232,
    g: 237,
    b: 247,
};
const MUTED: Color = Color::Rgb {
    r: 143,
    g: 155,
    b: 176,
};
/// The empty cells of a meter and the gaps of a sparkline: present, but
/// quieter than a muted label so the filled part carries the row.
const DIM: Color = Color::Rgb {
    r: 58,
    g: 66,
    b: 82,
};
const CYAN: Color = Color::Rgb {
    r: 83,
    g: 191,
    b: 230,
};
const GREEN: Color = Color::Rgb {
    r: 130,
    g: 217,
    b: 120,
};
const AMBER: Color = Color::Rgb {
    r: 228,
    g: 185,
    b: 87,
};
const RED: Color = Color::Rgb {
    r: 241,
    g: 111,
    b: 126,
};
const SETTINGS_FOOTER_PREFIX: &str = "  ";
const SETTINGS_LINK_LABEL: &str = "settings";
/// Space kept clear at the right edge of every panel line.
const RIGHT_PADDING: usize = 2;
/// The provider column: wide enough for `  ❑ OpenCode Go` with its margin.
const LABEL_WIDTH: usize = 15;
/// Between the short-window and long-window columns.
const COLUMN_GAP: usize = 3;
/// The sparkline looks back this far.
const HISTORY_SPAN_SECONDS: u64 = 48 * 60 * 60;
const METER_FILLED: char = '\u{2588}';
const METER_EMPTY: char = '\u{2591}';

type StyledLine = Vec<(Color, String)>;

/// What one background refresh pass produced.
struct RefreshUpdate {
    completed: bool,
    opencode_usage: Option<Option<LocalUsage>>,
    panes: Vec<AgentPane>,
}

/// Everything the interactive pane knows that the cache does not.
#[derive(Clone, Default)]
struct View {
    scroll_offset: usize,
    /// `o` flips this for the life of the pane. The saved setting, which
    /// also orders the Herdr sidebar, is left alone.
    order: Option<AgentOrder>,
    /// The local UTC offset, resolved once before any thread exists.
    /// `None` when the platform would not say; clock times then show UTC.
    clock_offset: Option<UtcOffset>,
    help: bool,
    refreshing: bool,
    next_refresh_in: Option<u64>,
    panes: Vec<AgentPane>,
}

struct Frame {
    text: String,
    max_scroll: usize,
    footer_row: u16,
}

/// One dashboard row's source: the provider, its usable snapshot, and the
/// identity its trend history is filed under.
struct DashboardRow {
    provider: DashboardProvider,
    snapshot: Option<ProviderSnapshot>,
    history_identity: Option<String>,
    /// What to do about a row with no quota to show, from the sign-in check.
    hint: Option<String>,
}

pub fn run() -> Result<()> {
    let cache = CacheStore::from_env()?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let preferences = DashboardPreferences::load_or_default(&cache);
        let opencode_usage = crate::refresh::refresh_dashboard(&cache, &preferences, false)
            .ok()
            .flatten()
            .flatten();
        print_snapshot(&cache, &preferences, opencode_usage)?;
        return Ok(());
    }
    // `time` refuses to read the local offset once a second thread exists,
    // so it is read here, before the refresh worker starts.
    let clock_offset = UtcOffset::current_local_offset().ok();
    let (refresh_requests, refresh_updates) = start_dashboard_refresh_worker(cache.clone());
    let _ = refresh_requests.send(false);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let result = execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    )
    .context("enter dashboard screen")
    .and_then(|_| interactive(&cache, &refresh_requests, &refresh_updates, clock_offset));
    let terminal_cleanup = execute!(
        stdout,
        ResetColor,
        cursor::Show,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .context("leave dashboard screen");
    let raw_cleanup = disable_raw_mode().context("disable dashboard raw mode");
    result.and(terminal_cleanup).and(raw_cleanup)
}

/// Idle wait between frames. `poll` returns as soon as a key arrives, so this
/// bounds only how long an unattended popup sleeps, never how fast it reacts.
const IDLE_POLL: Duration = Duration::from_secs(1);

/// Repaint only when the rendered frame actually changed, and without
/// clearing the screen: every line is padded to the full width and the frame
/// is exactly the terminal's height, so drawing over the previous frame
/// leaves nothing behind. A clear is spent only on the first paint and on a
/// resize, where the old frame's shape no longer matches.
fn interactive(
    cache: &CacheStore,
    refresh_requests: &Sender<bool>,
    refresh_updates: &Receiver<RefreshUpdate>,
    clock_offset: Option<UtcOffset>,
) -> Result<()> {
    let mut painted: Option<String> = None;
    let mut painted_size: Option<(u16, u16)> = None;
    let mut opencode_usage = None;
    let refresh_interval = Duration::from_secs(cache.watch_interval_seconds());
    let mut refreshed_at = Instant::now();
    let mut view = View {
        clock_offset,
        refreshing: true,
        ..View::default()
    };
    loop {
        while let Ok(update) = refresh_updates.try_recv() {
            view.refreshing = false;
            view.panes = update.panes;
            if update.completed {
                refreshed_at = Instant::now();
                if let Some(usage) = update.opencode_usage {
                    opencode_usage = usage;
                }
            }
        }
        if !view.refreshing && refreshed_at.elapsed() >= refresh_interval {
            view.refreshing = refresh_requests.send(false).is_ok();
        }
        view.next_refresh_in = (!view.refreshing).then(|| {
            refresh_interval
                .saturating_sub(refreshed_at.elapsed())
                .as_secs()
        });
        let (width, height) = terminal::size().unwrap_or((78, 24));
        let frame = render_terminal_scrolled(cache, width, height, opencode_usage, &view)?;
        view.scroll_offset = view.scroll_offset.min(frame.max_scroll);
        if painted.as_deref() != Some(frame.text.as_str()) {
            let resized = painted_size != Some((width, height));
            print!("{}{}", repaint_prefix(resized), frame.text);
            io::stdout().flush()?;
            painted = Some(frame.text);
            painted_size = Some((width, height));
        }
        if event::poll(IDLE_POLL)? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(());
                    }
                    if view.help {
                        match key.code {
                            KeyCode::Char('q') => return Ok(()),
                            _ => view.help = false,
                        }
                        continue;
                    }
                    let page = usize::from(height.max(2) / 2);
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('s') => open_settings_from_dashboard(),
                        KeyCode::Char('?') => view.help = true,
                        KeyCode::Char('o') => {
                            let current = view
                                .order
                                .unwrap_or_else(|| cache.agent_order().unwrap_or_default());
                            view.order = Some(match current {
                                AgentOrder::Default => AgentOrder::Quota,
                                AgentOrder::Quota => AgentOrder::Default,
                            });
                        }
                        KeyCode::Char('t') => toggle_reset_clock(cache),
                        KeyCode::Up | KeyCode::Char('k') => {
                            view.scroll_offset = view.scroll_offset.saturating_sub(1)
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            view.scroll_offset =
                                view.scroll_offset.saturating_add(1).min(frame.max_scroll)
                        }
                        KeyCode::PageUp => {
                            view.scroll_offset = view.scroll_offset.saturating_sub(page)
                        }
                        KeyCode::PageDown => {
                            view.scroll_offset = view
                                .scroll_offset
                                .saturating_add(page)
                                .min(frame.max_scroll)
                        }
                        KeyCode::Home => view.scroll_offset = 0,
                        KeyCode::End => view.scroll_offset = frame.max_scroll,
                        KeyCode::Char('r') if refresh_requests.send(true).is_ok() => {
                            view.refreshing = true
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if settings_link_hit(mouse.column, mouse.row, frame.footer_row, width) =>
                    {
                        open_settings_from_dashboard();
                    }
                    MouseEventKind::ScrollUp => {
                        view.scroll_offset = view.scroll_offset.saturating_sub(1)
                    }
                    MouseEventKind::ScrollDown => {
                        view.scroll_offset =
                            view.scroll_offset.saturating_add(1).min(frame.max_scroll)
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn open_settings_from_dashboard() {
    if let Err(error) = crate::herdr::invoke_settings_action() {
        let _ = crate::herdr::notify("QuotaDeck", &format!("Could not open settings: {error}"));
    }
}

/// `t` flips the saved clock preference so the choice survives reopening
/// the pane and the settings pane shows the same value.
fn toggle_reset_clock(cache: &CacheStore) {
    let mut preferences = DashboardPreferences::load_or_default(cache);
    preferences.display.reset_clock = !preferences.display.reset_clock;
    if let Err(error) = preferences.save(cache) {
        let _ = crate::herdr::notify(
            "QuotaDeck",
            &format!("Could not save reset time display: {error}"),
        );
    }
}

fn start_dashboard_refresh_worker(cache: CacheStore) -> (Sender<bool>, Receiver<RefreshUpdate>) {
    let (request_tx, request_rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(mut force) = request_rx.recv() {
            while let Ok(queued_force) = request_rx.try_recv() {
                force |= queued_force;
            }
            let preferences = DashboardPreferences::load_or_default(&cache);
            let refreshed = loop {
                match crate::refresh::refresh_dashboard(&cache, &preferences, force) {
                    Ok(Some(usage)) => break Some(usage),
                    Ok(None) if force => thread::sleep(Duration::from_millis(250)),
                    Ok(None) | Err(_) => break None,
                }
            };
            // The sessions section reads the tokens the sidebar already has,
            // so it can never disagree with the sidebar; a Herdr that will
            // not answer simply leaves the section out.
            let panes = crate::herdr::list_agent_panes().unwrap_or_default();
            let update = match refreshed {
                Some(usage) => RefreshUpdate {
                    completed: true,
                    opencode_usage: preferences
                        .get(DashboardProvider::OpenCode)
                        .show
                        .then_some(usage),
                    panes,
                },
                None => RefreshUpdate {
                    completed: false,
                    opencode_usage: None,
                    panes,
                },
            };
            if update_tx.send(update).is_err() {
                break;
            }
        }
    });
    (request_tx, update_rx)
}

fn print_snapshot(
    cache: &CacheStore,
    preferences: &DashboardPreferences,
    opencode_usage: Option<LocalUsage>,
) -> Result<()> {
    print!(
        "{}",
        render_snapshot_with_preferences(
            cache,
            CacheStore::now_unix(),
            opencode_usage,
            preferences,
        )?
    );
    Ok(())
}

#[cfg(test)]
fn render_snapshot_with_opencode(
    cache: &CacheStore,
    now: u64,
    opencode_usage: Option<LocalUsage>,
) -> Result<String> {
    render_snapshot_with_preferences(
        cache,
        now,
        opencode_usage,
        &DashboardPreferences::load_or_default(cache),
    )
}

/// The plain, ANSI-free dashboard for a redirected stdout. Meters and the
/// column grid belong to the interactive pane; piped output stays the stable
/// one-line-per-provider text that scripts and tests read.
fn render_snapshot_with_preferences(
    cache: &CacheStore,
    now: u64,
    opencode_usage: Option<LocalUsage>,
    preferences: &DashboardPreferences,
) -> Result<String> {
    let style = RowStyle::new(
        cache.percent_style().unwrap_or_default(),
        cache.brand_glyphs().unwrap_or_default(),
    );
    let order = cache.agent_order().unwrap_or_default();
    let mut output = String::from("QuotaDeck\r\n\r\n");
    for row in dashboard_rows(cache, preferences, now, order)? {
        let preference = preferences.get(row.provider);
        output.push_str(&match row.provider {
            DashboardProvider::OpenCode => {
                render_opencode_usage(opencode_usage, style, preference, row.hint.as_deref())
            }
            _ => render_quota_provider(
                row.provider.quota_provider().expect("quota provider"),
                row.snapshot.as_ref(),
                now,
                style,
                preference,
                row.hint.as_deref(),
            ),
        });
        output.push_str("\r\n");
    }
    Ok(output)
}

pub fn render_provider(
    provider: Provider,
    snapshot: Option<&ProviderSnapshot>,
    now_unix: u64,
    style: impl Into<RowStyle>,
) -> String {
    let preference = ProviderPreference::defaults(DashboardProvider::from_quota_provider(provider));
    render_quota_provider(provider, snapshot, now_unix, style, &preference, None)
}

fn render_quota_provider(
    provider: Provider,
    snapshot: Option<&ProviderSnapshot>,
    now_unix: u64,
    style: impl Into<RowStyle>,
    preference: &ProviderPreference,
    hint: Option<&str>,
) -> String {
    let style = style.into();
    let values = provider_content(
        provider,
        snapshot,
        now_unix,
        style.percent,
        preference,
        hint,
    )
    .text(now_unix, None)
    .into_iter()
    .map(|(value, _)| value)
    .collect::<Vec<_>>()
    .join(" · ");
    format!(
        "{}  {values}",
        style.glyphs.label(provider, provider.display_name())
    )
}

fn dashboard_rows(
    cache: &CacheStore,
    preferences: &DashboardPreferences,
    now: u64,
    order: AgentOrder,
) -> Result<Vec<DashboardRow>> {
    let mut rows = Vec::new();
    for preference in &preferences.providers {
        if !preference.show {
            continue;
        }
        let (mut snapshot, history_identity) = match preference.provider {
            DashboardProvider::OpenCode => (None, None),
            DashboardProvider::Omp => match load_latest_omp(cache)? {
                Some((snapshot, identity)) => (Some(snapshot), Some(identity)),
                None => (None, None),
            },
            provider => {
                let provider = provider.quota_provider().expect("quota provider");
                (
                    crate::refresh::load_usable_snapshot(cache, provider)?,
                    Some(CacheStore::history_identity(provider).to_string()),
                )
            }
        };
        let problem = preference
            .provider
            .quota_provider()
            .and_then(|provider| cache.refresh_problem_code(provider));
        if let Some(provider) = preference.provider.quota_provider() {
            let failure = cache.refresh_problem(provider);
            if failure.is_some() && snapshot.is_none() {
                snapshot = Some(ProviderSnapshot::new(provider, Vec::new(), now));
            }
            if let Some(snapshot) = snapshot.as_mut() {
                snapshot.refresh_warning = cache.refresh_warning(provider, Some(snapshot), now);
            }
        }
        // Only a row with nothing to draw asks what is wrong: a stale value
        // keeps its own wording, and the probes stay off rows that work.
        let has_windows = snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.windows.is_empty());
        let hint = (problem.is_some() || !has_windows)
            .then(|| crate::signin::row_hint(preference.provider, problem))
            .flatten();
        rows.push(DashboardRow {
            provider: preference.provider,
            snapshot,
            history_identity,
            hint,
        });
    }
    if order.is_quota() {
        rows.sort_by(|left, right| {
            let headroom = |row: &DashboardRow| {
                tightest_visible_window(
                    row.provider,
                    row.snapshot.as_ref(),
                    preferences.get(row.provider),
                )
                .map(|window| window.remaining_percent)
            };
            match (headroom(left), headroom(right)) {
                (Some(left), Some(right)) => left.total_cmp(&right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        });
    }
    Ok(rows)
}

/// OMP targets are keyed by an irreversible provider-id hash. The dashboard
/// needs one useful OMP row, so select the newest sanitized cached snapshot
/// without opening OMP's credential database or attempting to reverse the id.
/// The file stem comes back with it: that is the identity its history is
/// filed under.
fn load_latest_omp(cache: &CacheStore) -> Result<Option<(ProviderSnapshot, String)>> {
    let entries = match fs::read_dir(cache.root()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read cache directory {}", cache.root().display()))
        }
    };
    let mut latest: Option<(ProviderSnapshot, String)> = None;
    for entry in entries {
        let entry = entry.context("read cached OMP entry")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("omp-usage-") || !name.ends_with(".omp-store.json") {
            continue;
        }
        let bytes =
            fs::read(entry.path()).with_context(|| format!("read cached OMP snapshot {name}"))?;
        let snapshot: ProviderSnapshot = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse cached OMP snapshot {name}"))?;
        if snapshot.provider != Provider::Omp {
            continue;
        }
        if latest
            .as_ref()
            .is_none_or(|(current, _)| snapshot.fetched_at_unix > current.fetched_at_unix)
        {
            let identity = name.trim_end_matches(".json").to_string();
            latest = Some((snapshot, identity));
        }
    }
    Ok(latest)
}

fn render_opencode_usage(
    usage: Option<LocalUsage>,
    style: RowStyle,
    preference: &ProviderPreference,
    hint: Option<&str>,
) -> String {
    let values = opencode_segments(usage, preference, hint)
        .into_iter()
        .map(|(value, _)| value)
        .collect::<Vec<_>>()
        .join(" · ");
    format!(
        "{}  {values}",
        style.glyphs.label(Provider::OpenCodeGo, "OpenCode")
    )
}

#[cfg(test)]
fn render_terminal(
    cache: &CacheStore,
    width: u16,
    height: u16,
    opencode_usage: Option<LocalUsage>,
) -> Result<String> {
    render_terminal_scrolled(cache, width, height, opencode_usage, &View::default())
        .map(|frame| frame.text)
}

/// The column grid the interactive pane draws when it is wide enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GridLayout {
    label_width: usize,
    /// Meter cells per window; zero when bars are off.
    bar: usize,
    /// Width of the reset column: a countdown or a clock time.
    eta: usize,
}

impl GridLayout {
    /// Countdowns are at most `25d23h`; clock times at most `Thu 09:12`.
    const ETA_COUNTDOWN: usize = 6;
    const ETA_CLOCK: usize = 9;

    fn cell_width(self) -> usize {
        let bar = if self.bar > 0 { self.bar + 1 } else { 0 };
        3 + 1 + bar + 4 + 1 + self.eta
    }

    fn total_width(self) -> usize {
        self.label_width + 2 * self.cell_width() + COLUMN_GAP + RIGHT_PADDING
    }

    /// The widest grid that fits, shrinking the meter before giving up.
    fn fit(panel_width: usize, label_width: usize, bars: bool, clock: bool) -> Option<Self> {
        let eta = if clock {
            Self::ETA_CLOCK
        } else {
            Self::ETA_COUNTDOWN
        };
        let candidates: &[usize] = if bars { &[METER_CELLS, 8, 6] } else { &[0] };
        candidates
            .iter()
            .map(|bar| Self {
                label_width,
                bar: *bar,
                eta,
            })
            .find(|layout| layout.total_width() <= panel_width)
    }
}

fn render_terminal_scrolled(
    cache: &CacheStore,
    width: u16,
    height: u16,
    opencode_usage: Option<LocalUsage>,
    view: &View,
) -> Result<Frame> {
    crossterm::style::force_color_output(true);
    let now = CacheStore::now_unix();
    let style = RowStyle::new(
        cache.percent_style().unwrap_or_default(),
        cache.brand_glyphs().unwrap_or_default(),
    );
    let preferences = DashboardPreferences::load_or_default(cache);
    let order = view
        .order
        .unwrap_or_else(|| cache.agent_order().unwrap_or_default());
    let rows = dashboard_rows(cache, &preferences, now, order)?;
    let width = usize::from(width.max(1));
    let panel_width = width.saturating_sub(4).max(1);
    let clock = preferences
        .display
        .reset_clock
        .then(|| view.clock_offset.unwrap_or(UtcOffset::UTC));
    let clock_is_utc_fallback = preferences.display.reset_clock && view.clock_offset.is_none();
    let bars = preferences.display.bars;

    let labels: Vec<String> = rows
        .iter()
        .map(|row| format!("  {}", row_label(row.provider, style)))
        .collect();
    let label_width = labels
        .iter()
        .map(|label| label.chars().count() + 1)
        .max()
        .unwrap_or(LABEL_WIDTH)
        .max(LABEL_WIDTH);
    let grid = GridLayout::fit(panel_width, label_width, bars, clock.is_some());

    let mut lines: Vec<(bool, StyledLine)> = vec![
        (false, Vec::new()),
        (true, Vec::new()),
        (true, title_line(&rows, view, now, panel_width)),
        (true, Vec::new()),
    ];
    if view.help {
        lines.extend(help_lines().into_iter().map(|line| (true, line)));
    } else {
        if let Some(grid) = grid {
            lines.push((
                true,
                header_line(grid, style.percent, clock_is_utc_fallback),
            ));
        }
        for (row, label) in rows.iter().zip(labels) {
            let preference = preferences.get(row.provider);
            let label_color = provider_color(preference);
            let row_lines = match row.provider {
                DashboardProvider::OpenCode => summary_lines(
                    label,
                    label_color,
                    opencode_segments(opencode_usage, preference, row.hint.as_deref())
                        .into_iter()
                        .map(|(text, severity)| Segment::plain(text, severity_color(severity)))
                        .collect(),
                    panel_width,
                ),
                _ => provider_lines(
                    row.provider.quota_provider().expect("quota provider"),
                    row.snapshot.as_ref(),
                    now,
                    style,
                    preference,
                    row.hint.as_deref(),
                    (label, label_color),
                    panel_width,
                    grid,
                    clock,
                ),
            };
            lines.extend(row_lines.into_iter().map(|line| (true, line)));
        }
        let tightest = tightest(&rows, &preferences);
        if let Some(tightest) = &tightest {
            lines.push((true, Vec::new()));
            lines.push((
                true,
                headline_line(tightest, cache, now, style.percent, clock, panel_width),
            ));
        }
        lines.push((true, Vec::new()));

        // Optional sections fill a tall split without ever pushing the
        // provider rows into a scroll: each one is added only when it fits
        // beneath what is already there.
        let height_budget = usize::from(height.max(1)).saturating_sub(1);
        let mut used = lines.len();
        let sessions = sessions_lines(&view.panes, panel_width);
        if !sessions.is_empty() && used + sessions.len() < height_budget {
            used += sessions.len() + 1;
            lines.extend(sessions.into_iter().map(|line| (true, line)));
            lines.push((true, Vec::new()));
        }
        if let Some(tightest) = &tightest {
            let history = history_lines(tightest, cache, now, panel_width);
            if !history.is_empty() && used + history.len() < height_budget {
                lines.extend(history.into_iter().map(|line| (true, line)));
                lines.push((true, Vec::new()));
            }
        }
    }

    let height = usize::from(height.max(1));
    let body_height = height.saturating_sub(1);
    let max_scroll = lines.len().saturating_sub(body_height);
    let scroll_offset = view.scroll_offset.min(max_scroll);
    let footer_tail = if max_scroll > 0 {
        format!(
            " [s/click] · ↑/↓ {}/{} · Pg · ? help",
            scroll_offset + 1,
            max_scroll + 1
        )
    } else {
        " [s/click] · r refresh · o sort · t clock · ? help".to_string()
    };
    let footer_row = if max_scroll > 0 {
        height.saturating_sub(1)
    } else {
        lines.len()
    } as u16;
    let footer = (
        true,
        right_aligned(
            vec![
                (MUTED, SETTINGS_FOOTER_PREFIX.to_string()),
                (CYAN, SETTINGS_LINK_LABEL.to_string()),
                (MUTED, footer_tail),
            ],
            vec![(MUTED, "q close".to_string())],
            panel_width,
        ),
    );
    if max_scroll > 0 {
        lines = lines
            .into_iter()
            .skip(scroll_offset)
            .take(body_height)
            .collect();
        lines.push(footer);
    } else {
        lines.push(footer);
        lines.resize_with(height, || (false, Vec::new()));
    }

    let mut output = String::new();
    push_ansi(&mut output, SetBackgroundColor(OUTER_BACKGROUND));
    let line_count = lines.len();
    for (index, (inside_panel, line)) in lines.into_iter().enumerate() {
        push_styled_line(&mut output, &line, width, inside_panel);
        if index + 1 < line_count {
            output.push_str("\r\n");
        }
    }
    push_ansi(&mut output, ResetColor);
    Ok(Frame {
        text: output,
        max_scroll,
        footer_row,
    })
}

fn row_label(provider: DashboardProvider, style: RowStyle) -> String {
    match provider {
        DashboardProvider::OpenCode => style.glyphs.label(Provider::OpenCodeGo, "OpenCode"),
        provider => {
            let provider = provider.quota_provider().expect("quota provider");
            style.glyphs.label(provider, provider.display_name())
        }
    }
}

/// `QuotaDeck` with, at the right, how old the numbers are and when the
/// pane will fetch again.
fn title_line(rows: &[DashboardRow], view: &View, now: u64, panel_width: usize) -> StyledLine {
    let newest = rows
        .iter()
        .filter_map(|row| row.snapshot.as_ref())
        .filter(|snapshot| !snapshot.windows.is_empty())
        .map(|snapshot| snapshot.fetched_at_unix)
        .max();
    let status = if view.refreshing {
        "refreshing\u{2026}".to_string()
    } else {
        let updated = match newest {
            Some(at) => format!("updated {} ago", format_age(now.saturating_sub(at))),
            None => "no data yet".to_string(),
        };
        match view.next_refresh_in {
            Some(seconds) => format!("{updated} · next in {}", format_age(seconds)),
            None => updated,
        }
    };
    right_aligned(
        vec![(CYAN, "  QuotaDeck".to_string())],
        vec![(MUTED, status)],
        panel_width,
    )
}

fn format_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else if seconds < 48 * 60 * 60 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// Column captions over the grid, so the numbers need no repeated words.
fn header_line(grid: GridLayout, percent: PercentStyle, utc: bool) -> StyledLine {
    let resets = if utc { "resets (UTC)" } else { "resets" };
    let cell = |_: ()| {
        let mut text = String::new();
        text.push_str(&" ".repeat(3 + 1));
        if grid.bar > 0 {
            text.push_str(&format!("{:<width$} ", percent.suffix(), width = grid.bar));
            text.push_str(&" ".repeat(4 + 1));
        } else {
            text.push_str(&format!("{:>4} ", percent.suffix()));
        }
        text.push_str(&format!("{resets:>width$}", width = grid.eta));
        text
    };
    let mut line = " ".repeat(grid.label_width);
    line.push_str(&cell(()));
    line.push_str(&" ".repeat(COLUMN_GAP));
    line.push_str(&cell(()));
    vec![(MUTED, line)]
}

/// What a provider row shows: structured windows, or text when the plugin
/// has only a status to report.
enum RowContent {
    Cells(Vec<WindowCell>),
    Text(Vec<(String, Severity)>),
}

impl RowContent {
    fn text(&self, now_unix: u64, clock: Option<UtcOffset>) -> Vec<(String, Severity)> {
        match self {
            Self::Cells(cells) => cells
                .iter()
                .map(|cell| (cell.text(now_unix, clock), cell.severity))
                .collect(),
            Self::Text(segments) => segments.clone(),
        }
    }
}

fn provider_content(
    provider: Provider,
    snapshot: Option<&ProviderSnapshot>,
    now_unix: u64,
    style: PercentStyle,
    preference: &ProviderPreference,
    hint: Option<&str>,
) -> RowContent {
    if let Some(hint) = hint {
        return RowContent::Text(vec![(hint.to_string(), Severity::Warning)]);
    }
    if let Some(snapshot) = snapshot {
        if let Some(warning) = &snapshot.refresh_warning {
            return RowContent::Text(vec![(warning.clone(), Severity::Warning)]);
        }
        let cells = dashboard_cells(snapshot, now_unix, style, preference);
        if !cells.is_empty() {
            return RowContent::Cells(cells);
        }
    }
    RowContent::Text(match provider {
        Provider::Claude | Provider::Agy => [
            (preference.has(DashboardField::ShortPercent)
                || preference.has(DashboardField::ShortReset))
            .then(|| ("5h N/A".to_string(), Severity::Unknown)),
            (preference.has(DashboardField::LongPercent)
                || preference.has(DashboardField::LongReset))
            .then(|| ("7d N/A".to_string(), Severity::Unknown)),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Provider::OpenRouter if !preference.fields.is_empty() => {
            vec![("credentials unavailable".to_string(), Severity::Unknown)]
        }
        _ if preference.fields.is_empty() => Vec::new(),
        _ => vec![("N/A".to_string(), Severity::Unknown)],
    })
}

#[cfg(test)]
fn provider_segments(
    provider: Provider,
    snapshot: Option<&ProviderSnapshot>,
    now_unix: u64,
    style: PercentStyle,
    preference: &ProviderPreference,
) -> Vec<(String, Severity)> {
    provider_content(provider, snapshot, now_unix, style, preference, None).text(now_unix, None)
}

/// A value on a flowing row: its styled parts and its printed width.
struct Segment {
    parts: StyledLine,
    width: usize,
}

impl Segment {
    fn plain(text: String, color: Color) -> Self {
        Self {
            width: text.chars().count(),
            parts: vec![(color, text)],
        }
    }

    fn from_cell(cell: &WindowCell, now_unix: u64, clock: Option<UtcOffset>, bars: bool) -> Self {
        let color = severity_color(cell.severity);
        let mut parts: StyledLine = vec![(color, cell.label.clone())];
        if let Some(amount) = &cell.amount {
            parts.push((color, format!(" {amount}")));
        }
        if let Some(percent) = cell.percent {
            if bars {
                parts.push((TEXT, " ".to_string()));
                parts.extend(meter_parts(percent, METER_CELLS, color));
            }
            parts.push((color, format!(" {}%", format_percent(percent))));
        }
        if let Some(reset) = cell.reset {
            parts.push((
                color,
                format!(" reset {}", format_reset(reset, now_unix, clock)),
            ));
        }
        let width = parts.iter().map(|(_, text)| text.chars().count()).sum();
        Self { parts, width }
    }
}

/// A meter split into its filled and empty runs, so the empty cells can sit
/// back while the filled ones take the severity colour.
fn meter_parts(percent: f64, cells: usize, color: Color) -> StyledLine {
    let bar = meter(percent, cells, METER_FILLED, METER_EMPTY);
    let filled: String = bar.chars().filter(|glyph| *glyph == METER_FILLED).collect();
    let empty: String = bar.chars().filter(|glyph| *glyph == METER_EMPTY).collect();
    let mut parts = Vec::new();
    if !filled.is_empty() {
        parts.push((color, filled));
    }
    if !empty.is_empty() {
        parts.push((DIM, empty));
    }
    parts
}

#[allow(clippy::too_many_arguments)]
fn provider_lines(
    provider: Provider,
    snapshot: Option<&ProviderSnapshot>,
    now_unix: u64,
    style: RowStyle,
    preference: &ProviderPreference,
    hint: Option<&str>,
    label: (String, Color),
    width: usize,
    grid: Option<GridLayout>,
    clock: Option<UtcOffset>,
) -> Vec<StyledLine> {
    let (label, label_color) = label;
    let content = provider_content(
        provider,
        snapshot,
        now_unix,
        style.percent,
        preference,
        hint,
    );
    // Flowing rows carry meters only in a pane wide enough for the grid, so
    // one frame never mixes rows with meters and rows without.
    let bars = grid.is_some_and(|grid| grid.bar > 0);
    if let (Some(grid), RowContent::Cells(cells)) = (grid, &content) {
        if let Some(line) = grid_line(cells, grid, (&label, label_color), now_unix, clock) {
            return vec![line];
        }
    }
    let segments = |bars: bool| -> Vec<Segment> {
        match &content {
            RowContent::Cells(cells) => cells
                .iter()
                .map(|cell| Segment::from_cell(cell, now_unix, clock, bars))
                .collect(),
            RowContent::Text(segments) => segments
                .iter()
                .map(|(text, severity)| Segment::plain(text.clone(), severity_color(*severity)))
                .collect(),
        }
    };
    // One line per provider is worth more than a meter: a flowing row that
    // only fits without its meters drops them before it wraps.
    if let Some(line) = single_line(&label, label_color, segments(bars), width) {
        return vec![line];
    }
    if bars {
        if let Some(line) = single_line(&label, label_color, segments(false), width) {
            return vec![line];
        }
    }
    summary_lines(label, label_color, segments(bars), width)
}

/// One grid row: the label, then the short and long window columns. `None`
/// when the row does not fit the grid's two plain-percentage columns (a
/// dollar balance, or a third window), which flows instead.
fn grid_line(
    cells: &[WindowCell],
    grid: GridLayout,
    label: (&str, Color),
    now_unix: u64,
    clock: Option<UtcOffset>,
) -> Option<StyledLine> {
    if cells.is_empty()
        || cells
            .iter()
            .any(|cell| cell.amount.is_some() || cell.slot == CellSlot::Extra)
    {
        return None;
    }
    let short = cells.iter().find(|cell| cell.slot == CellSlot::Short);
    let long = cells.iter().find(|cell| cell.slot == CellSlot::Long);
    if cells.len() > usize::from(short.is_some()) + usize::from(long.is_some()) {
        return None;
    }
    let (label, label_color) = label;
    let mut line: StyledLine = vec![(label_color, label.to_string())];
    line.push((
        TEXT,
        " ".repeat(grid.label_width.saturating_sub(label.chars().count())),
    ));
    line.extend(grid_cell(short, grid, now_unix, clock));
    line.push((TEXT, " ".repeat(COLUMN_GAP)));
    line.extend(grid_cell(long, grid, now_unix, clock));
    Some(line)
}

fn grid_cell(
    cell: Option<&WindowCell>,
    grid: GridLayout,
    now_unix: u64,
    clock: Option<UtcOffset>,
) -> StyledLine {
    let Some(cell) = cell else {
        return vec![(TEXT, " ".repeat(grid.cell_width()))];
    };
    let color = severity_color(cell.severity);
    let mut parts: StyledLine = vec![(MUTED, format!("{:<3} ", cell.label))];
    match cell.percent {
        Some(percent) => {
            if grid.bar > 0 {
                parts.extend(meter_parts(percent, grid.bar, color));
                parts.push((TEXT, " ".to_string()));
            }
            parts.push((
                color,
                format!("{:>4}", format!("{}%", format_percent(percent))),
            ));
        }
        None => {
            let blank = if grid.bar > 0 { grid.bar + 1 } else { 0 } + 4;
            parts.push((TEXT, " ".repeat(blank)));
        }
    }
    let eta = cell
        .reset
        .map(|reset| format_reset(reset, now_unix, clock))
        .unwrap_or_default();
    parts.push((MUTED, format!(" {eta:>width$}", width = grid.eta)));
    parts
}

/// The tightest visible window across every row, as the headline reports it.
struct Tightest {
    provider: DashboardProvider,
    history_identity: Option<String>,
    window: UsageWindow,
}

/// `▲ Codex 7d · 14% left · resets 4d23h`, with the pace over the last hours
/// on the right when the history can say.
fn headline_line(
    tightest: &Tightest,
    cache: &CacheStore,
    now: u64,
    percent: PercentStyle,
    clock: Option<UtcOffset>,
    panel_width: usize,
) -> StyledLine {
    let window = &tightest.window;
    let color = severity_color(Severity::for_window(window, now));
    let mut text = format!(
        "{} {} · {}% {}",
        tightest.provider.label(),
        window.display_label(),
        format_percent(percent.percent_of(window)),
        percent.suffix()
    );
    if let Some(reset) = window.resets_at {
        text.push_str(&format!(" · resets {}", format_reset(reset, now, clock)));
        if clock.is_some() {
            text.push_str(&format!(" ({})", format_reset_eta(reset, now)));
        }
    }
    let left = vec![(color, "  \u{25b2} ".to_string()), (TEXT, text)];
    let right = pace_parts(tightest, cache, now);
    right_aligned(left, right, panel_width)
}

fn pace_parts(tightest: &Tightest, cache: &CacheStore, now: u64) -> StyledLine {
    let Some(identity) = &tightest.history_identity else {
        return Vec::new();
    };
    let window = &tightest.window;
    let samples = cache.load_history(identity, window.kind);
    let Some(pace) = trend::pace(&samples, now, window.remaining_percent) else {
        return Vec::new();
    };
    let reset_in = window
        .resets_at
        .map(|reset| reset.unix_seconds().saturating_sub(now));
    match (pace.empties_in_seconds, reset_in) {
        (None, _) => vec![(MUTED, "no use in 6h".to_string())],
        (Some(empties), Some(reset)) if empties >= reset => {
            vec![(GREEN, "lasts to reset".to_string())]
        }
        (Some(empties), _) => {
            let color = if empties < 60 * 60 { RED } else { AMBER };
            vec![(
                color,
                format!(
                    "~{} at this pace",
                    crate::presentation::format_duration(empties)
                ),
            )]
        }
    }
}

/// Every Herdr agent pane and the diagnostics its sidebar row already shows.
fn sessions_lines(panes: &[AgentPane], panel_width: usize) -> Vec<StyledLine> {
    if panes.is_empty() {
        return Vec::new();
    }
    const MODEL: usize = 16;
    const CTX: usize = 5;
    const CACHE: usize = 7;
    const TTL: usize = 6;
    let right_width = MODEL + 2 + CTX + 2 + CACHE + 2 + TTL;
    let name_width = panel_width.saturating_sub(2 + right_width + 1 + RIGHT_PADDING);
    let mut lines = vec![right_aligned(
        vec![(CYAN, "  Sessions".to_string())],
        vec![(
            MUTED,
            format!(
                "{:<MODEL$}  {:>CTX$}  {:>CACHE$}  {:>TTL$}",
                "model", "ctx", "cache", "ttl"
            ),
        )],
        panel_width,
    )];
    for pane in panes {
        let token = |name: &str, prefix: &[&str]| {
            pane.tokens
                .get(name)
                .map(|value| {
                    let mut value = value.as_str();
                    for prefix in prefix {
                        value = value.strip_prefix(prefix).unwrap_or(value);
                    }
                    value.trim().to_string()
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "\u{2014}".to_string())
        };
        let mut name = format!(
            "{}  {}",
            crate::settings::agent_name(pane.harness),
            pane.pane_id
        );
        if name.chars().count() > name_width {
            name = name
                .chars()
                .take(name_width.saturating_sub(1))
                .collect::<String>()
                + "\u{2026}";
        }
        let mut model = token("quota_model", &[]);
        if model.chars().count() > MODEL {
            model = model.chars().take(MODEL - 1).collect::<String>() + "\u{2026}";
        }
        let right = format!(
            "{:<MODEL$}  {:>CTX$}  {:>CACHE$}  {:>TTL$}",
            model,
            token("quota_context", &["context"]),
            token("quota_cache", &["cache"]),
            token("quota_cache_ttl", &["ttl\u{2248}", "ttl"]),
        );
        lines.push(right_aligned(
            vec![(TEXT, format!("  {name}"))],
            vec![(MUTED, right)],
            panel_width,
        ));
    }
    lines
}

/// A 48-hour sparkline of the tightest window: caption, bars, axis.
fn history_lines(
    tightest: &Tightest,
    cache: &CacheStore,
    now: u64,
    panel_width: usize,
) -> Vec<StyledLine> {
    let Some(identity) = &tightest.history_identity else {
        return Vec::new();
    };
    let window = &tightest.window;
    let samples = cache.load_history(identity, window.kind);
    let columns = panel_width.saturating_sub(2 + 2 + 5 + RIGHT_PADDING).max(8);
    let cells = trend::sparkline(&samples, now, HISTORY_SPAN_SECONDS, columns);
    if cells.iter().flatten().count() < 2 {
        return Vec::new();
    }
    let mut caption_right = Vec::new();
    if let Some(pace) = trend::pace(&samples, now, window.remaining_percent) {
        caption_right.push((MUTED, format!("burn {:.1}%/h", pace.percent_per_hour)));
    }
    if let Some(reset) = window.resets_at {
        if !caption_right.is_empty() {
            caption_right.push((MUTED, " · ".to_string()));
        }
        caption_right.push((MUTED, format!("resets {}", format_reset_eta(reset, now))));
    }
    let caption = right_aligned(
        vec![(
            CYAN,
            format!(
                "  {} {} · last 48h",
                tightest.provider.label(),
                window.display_label()
            ),
        )],
        caption_right,
        panel_width,
    );
    let mut spark: StyledLine = vec![(TEXT, "  ".to_string())];
    for cell in &cells {
        match cell {
            Some((remaining, glyph)) => {
                let color = if *remaining >= 50.0 {
                    GREEN
                } else if *remaining >= 20.0 {
                    AMBER
                } else {
                    RED
                };
                spark.push((color, glyph.to_string()));
            }
            None => spark.push((DIM, "\u{00b7}".to_string())),
        }
    }
    spark.push((
        MUTED,
        format!(
            "  {:>4}",
            format!("{}%", format_percent(window.remaining_percent))
        ),
    ));
    let axis = vec![(
        MUTED,
        format!(
            "  {:<width$}{}",
            "48h ago",
            "now",
            width = columns.saturating_sub(3)
        ),
    )];
    vec![caption, spark, axis]
}

fn help_lines() -> Vec<StyledLine> {
    [
        ("r", "refresh every provider now"),
        (
            "o",
            "sort by least left, or back to the saved order (this pane only)",
        ),
        ("t", "reset times as a clock time, or as a countdown"),
        ("s", "open settings · hide providers you do not use"),
        (
            "setup",
            "run `quotadeck setup` in a shell for guided sign-in",
        ),
        ("↑ ↓ j k", "scroll · PgUp PgDn Home End"),
        ("?", "close this help"),
        ("q Esc", "close QuotaDeck"),
    ]
    .into_iter()
    .map(|(key, what)| vec![(CYAN, format!("  {key:<8}")), (TEXT, what.to_string())])
    .collect()
}

/// `left` at the margin and `right` against the panel's right padding,
/// clipped from the right side first when both cannot fit.
fn right_aligned(left: StyledLine, right: StyledLine, panel_width: usize) -> StyledLine {
    let left_width: usize = left.iter().map(|(_, text)| text.chars().count()).sum();
    let right_width: usize = right.iter().map(|(_, text)| text.chars().count()).sum();
    let available = panel_width.saturating_sub(RIGHT_PADDING);
    if right.is_empty() || left_width + 1 + right_width > available {
        return left;
    }
    let mut line = left;
    line.push((TEXT, " ".repeat(available - left_width - right_width)));
    line.extend(right);
    line
}

fn settings_link_hit(column: u16, row: u16, footer_row: u16, width: u16) -> bool {
    let margin = u16::from(width >= 5) * 2;
    let start = margin + SETTINGS_FOOTER_PREFIX.chars().count() as u16;
    row == footer_row
        && (start..start + SETTINGS_LINK_LABEL.chars().count() as u16).contains(&column)
}

/// The label with every value right-aligned on the same line, when they fit.
fn single_line(
    label: &str,
    label_color: Color,
    segments: Vec<Segment>,
    width: usize,
) -> Option<StyledLine> {
    let values_width = segments.iter().map(|segment| segment.width).sum::<usize>()
        + 3 * segments.len().saturating_sub(1);
    let label_width = label.chars().count();
    let gap = width.saturating_sub(label_width + values_width + RIGHT_PADDING);
    if gap == 0 {
        return None;
    }
    let mut line = vec![(label_color, label.to_string())];
    line.push((TEXT, " ".repeat(gap)));
    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 {
            line.push((MUTED, " · ".to_string()));
        }
        line.extend(segment.parts);
    }
    Some(line)
}

fn summary_lines(
    label: String,
    label_color: Color,
    segments: Vec<Segment>,
    width: usize,
) -> Vec<StyledLine> {
    if let Some(line) = single_line(
        &label,
        label_color,
        segments
            .iter()
            .map(|segment| Segment {
                parts: segment.parts.clone(),
                width: segment.width,
            })
            .collect(),
        width,
    ) {
        return vec![line];
    }

    let mut lines = vec![vec![(label_color, label)]];
    for segment in segments {
        let padding = width.saturating_sub(segment.width + RIGHT_PADDING);
        let mut line = vec![(TEXT, " ".repeat(padding))];
        line.extend(segment.parts);
        lines.push(line);
    }
    lines
}

fn opencode_segments(
    usage: Option<LocalUsage>,
    preference: &ProviderPreference,
    hint: Option<&str>,
) -> Vec<(String, Severity)> {
    match usage {
        Some(usage) => [
            preference.has(DashboardField::Tokens).then(|| {
                (
                    format!("30d {} tok", format_token_count(usage.tokens)),
                    Severity::Normal,
                )
            }),
            preference
                .has(DashboardField::Spend)
                .then(|| (format!("spent ${:.2}", usage.cost_usd), Severity::Normal)),
        ]
        .into_iter()
        .flatten()
        .collect(),
        None => match hint {
            Some(hint) => vec![(hint.to_string(), Severity::Warning)],
            None if preference.fields.is_empty() => Vec::new(),
            None => vec![("30d N/A".to_string(), Severity::Unknown)],
        },
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000_000 {
        format!("{:.1}B", tokens as f64 / 1_000_000_000.0)
    } else if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn push_styled_line(output: &mut String, line: &StyledLine, width: usize, inside_panel: bool) {
    let margin = if inside_panel && width >= 5 { 2 } else { 0 };
    let content_width = width.saturating_sub(margin * 2);
    let mut remaining = content_width;
    push_ansi(output, SetBackgroundColor(OUTER_BACKGROUND));
    output.push_str(&" ".repeat(margin));
    if inside_panel {
        push_ansi(output, SetBackgroundColor(PANEL_BACKGROUND));
    }
    for (color, text) in line {
        if remaining == 0 {
            break;
        }
        let text = text.chars().take(remaining).collect::<String>();
        remaining = remaining.saturating_sub(text.chars().count());
        push_ansi(output, SetForegroundColor(*color));
        output.push_str(&text);
    }
    output.push_str(&" ".repeat(remaining));
    if inside_panel {
        push_ansi(output, SetBackgroundColor(OUTER_BACKGROUND));
    }
    output.push_str(&" ".repeat(margin));
}

fn repaint_prefix(clear: bool) -> String {
    let mut output = String::new();
    push_ansi(&mut output, SetBackgroundColor(OUTER_BACKGROUND));
    if clear {
        push_ansi(&mut output, terminal::Clear(terminal::ClearType::All));
    }
    push_ansi(&mut output, cursor::MoveTo(0, 0));
    output
}

fn push_ansi(output: &mut String, command: impl crossterm::Command) {
    command
        .write_ansi(output)
        .expect("writing ANSI to a String cannot fail");
}

fn tightest(rows: &[DashboardRow], preferences: &DashboardPreferences) -> Option<Tightest> {
    rows.iter()
        .filter_map(|row| {
            tightest_visible_window(
                row.provider,
                row.snapshot.as_ref(),
                preferences.get(row.provider),
            )
            .map(|window| (row, window))
        })
        .min_by(|(_, left), (_, right)| left.remaining_percent.total_cmp(&right.remaining_percent))
        .map(|(row, window)| Tightest {
            provider: row.provider,
            history_identity: row.history_identity.clone(),
            window: window.clone(),
        })
}

fn tightest_visible_window<'a>(
    provider: DashboardProvider,
    snapshot: Option<&'a ProviderSnapshot>,
    preference: &ProviderPreference,
) -> Option<&'a UsageWindow> {
    snapshot
        .filter(|snapshot| snapshot.refresh_warning.is_none())?
        .windows
        .iter()
        .filter(|window| percentage_visible(provider, window.kind, preference))
        .min_by(|left, right| left.remaining_percent.total_cmp(&right.remaining_percent))
}

fn percentage_visible(
    provider: DashboardProvider,
    kind: crate::model::WindowKind,
    preference: &ProviderPreference,
) -> bool {
    use crate::model::WindowKind;
    use DashboardField::*;
    preference.has(match (provider, kind) {
        (DashboardProvider::Hermes, WindowKind::Monthly) => PlanPercent,
        (DashboardProvider::Hermes, _) => TopUpPercent,
        (DashboardProvider::OpenRouter, _) => CreditsPercent,
        (_, WindowKind::FiveHour) => ShortPercent,
        _ => LongPercent,
    })
}

fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Normal => GREEN,
        Severity::Warning => AMBER,
        Severity::Danger => RED,
        Severity::Unknown => MUTED,
    }
}

fn provider_color(preference: &ProviderPreference) -> Color {
    let (r, g, b) = color_rgb(&preference.color);
    Color::Rgb { r, g, b }
}

/// The dashboard as data, for companion apps that draw QuotaDeck themselves
/// (Shep's QuotaDeck pane and scrolling strip, scripts). Everything the
/// interactive pane computes is here; nothing here is a terminal string a
/// consumer would have to parse.
///
/// Stable contract: `schema` bumps only when a field changes meaning. New
/// fields may appear at any time; consumers ignore what they do not know.
#[derive(Debug, serde::Serialize)]
pub struct DashboardJson {
    pub schema: u8,
    pub generated_at_unix: u64,
    /// The newest fetch among the rows, or null when nothing has been fetched.
    pub updated_at_unix: Option<u64>,
    pub percent_style: &'static str,
    pub display: DisplayJson,
    pub tightest: Option<TightestJson>,
    pub providers: Vec<ProviderJson>,
    pub sessions: Vec<SessionJson>,
}

#[derive(Debug, serde::Serialize)]
pub struct DisplayJson {
    pub bars: bool,
    pub reset_clock: bool,
    pub meter_cells: usize,
}

#[derive(Debug, serde::Serialize)]
pub struct ProviderJson {
    pub id: &'static str,
    pub label: &'static str,
    /// The user's chosen row colour, `#RRGGBB`. Themed consumers may ignore it.
    pub color: String,
    /// `ok`, `stale` (values present but older than two watch intervals) or
    /// `unavailable` (no values; see `reason_code`).
    pub state: &'static str,
    /// `login`, `credentials`, `failed`, `cli`, `stale`, or `none` when the
    /// plugin simply has nothing yet. Null when `state` is `ok`.
    pub reason_code: Option<&'static str>,
    /// The sentence the dashboard shows for that state, when it shows one.
    pub reason: Option<String>,
    pub fetched_at_unix: Option<u64>,
    /// One line for a ticker or strip: `Codex 5h 64% · 7d 14%`.
    pub strip: String,
    pub windows: Vec<WindowJson>,
    /// OpenCode's local 30-day usage; spend, not a limit.
    pub local_usage: Option<LocalUsageJson>,
}

#[derive(Debug, serde::Serialize)]
pub struct WindowJson {
    /// `5h`, `7d` or `30d`.
    pub kind: &'static str,
    /// The label the dashboard prints: usually `kind`, or `plan`, `top-up`,
    /// `credits` for dollar balances.
    pub label: String,
    pub amount: Option<String>,
    pub used_percent: f64,
    pub remaining_percent: f64,
    /// The percentage in the user's chosen style (`percent_style`).
    pub shown_percent: f64,
    pub meter: MeterJson,
    /// `normal`, `warning`, `danger` or `unknown`; always about headroom.
    pub severity: &'static str,
    pub resets_at_unix: Option<u64>,
    pub resets_in_seconds: Option<u64>,
    /// The countdown the dashboard prints (`3d23h`, `due`).
    pub resets_in: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct MeterJson {
    pub filled: usize,
    pub cells: usize,
}

#[derive(Debug, serde::Serialize)]
pub struct LocalUsageJson {
    pub tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct TightestJson {
    pub provider: &'static str,
    pub label: &'static str,
    pub window: String,
    pub remaining_percent: f64,
    pub used_percent: f64,
    pub severity: &'static str,
    pub resets_at_unix: Option<u64>,
    pub resets_in_seconds: Option<u64>,
    pub pace: Option<PaceJson>,
}

#[derive(Debug, serde::Serialize)]
pub struct PaceJson {
    pub percent_per_hour: f64,
    /// Seconds until empty at that rate; null when nothing is being used.
    pub empties_in_seconds: Option<u64>,
    /// True when the window resets before it would empty.
    pub outlasts_reset: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionJson {
    pub pane_id: String,
    pub harness: &'static str,
    /// The provider token as the sidebar shows it (`✳ Claude`), when known.
    pub provider: Option<String>,
    pub model: Option<String>,
    pub context: Option<String>,
    pub cache: Option<String>,
    pub ttl: Option<String>,
}

fn severity_id(severity: Severity) -> &'static str {
    match severity {
        Severity::Normal => "normal",
        Severity::Warning => "warning",
        Severity::Danger => "danger",
        Severity::Unknown => "unknown",
    }
}

/// Refresh like the plain dashboard does, then print the view model.
pub fn run_json() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let preferences = DashboardPreferences::load_or_default(&cache);
    let opencode_usage = crate::refresh::refresh_dashboard(&cache, &preferences, false)
        .ok()
        .flatten()
        .flatten();
    let panes = crate::herdr::list_agent_panes().unwrap_or_default();
    let model = dashboard_json(
        &cache,
        &preferences,
        opencode_usage,
        &panes,
        CacheStore::now_unix(),
    )?;
    println!("{}", serde_json::to_string_pretty(&model)?);
    Ok(())
}

/// Each row's quota as one line without its label, or `None` when the row
/// has nothing to show. For the `setup` checklist.
pub(crate) fn provider_quota_text(
    cache: &CacheStore,
    preferences: &DashboardPreferences,
    opencode_usage: Option<LocalUsage>,
    now: u64,
) -> Result<Vec<(DashboardProvider, Option<String>)>> {
    let model = dashboard_json(cache, preferences, opencode_usage, &[], now)?;
    Ok(model
        .providers
        .iter()
        .filter_map(|entry| {
            let provider = DashboardProvider::parse(entry.id)?;
            let text = matches!(entry.state, "ok" | "stale").then(|| {
                entry
                    .strip
                    .strip_prefix(entry.label)
                    .unwrap_or(&entry.strip)
                    .trim()
                    .to_string()
            });
            Some((provider, text))
        })
        .collect())
}

fn dashboard_json(
    cache: &CacheStore,
    preferences: &DashboardPreferences,
    opencode_usage: Option<LocalUsage>,
    panes: &[AgentPane],
    now: u64,
) -> Result<DashboardJson> {
    let percent = cache.percent_style().unwrap_or_default();
    let order = cache.agent_order().unwrap_or_default();
    let rows = dashboard_rows(cache, preferences, now, order)?;
    let mut providers = Vec::new();
    for row in &rows {
        let preference = preferences.get(row.provider);
        let mut entry = ProviderJson {
            id: row.provider.id(),
            label: row.provider.label(),
            color: preference.color.clone(),
            state: "unavailable",
            reason_code: Some("none"),
            reason: None,
            fetched_at_unix: None,
            strip: String::new(),
            windows: Vec::new(),
            local_usage: None,
        };
        match row.provider {
            DashboardProvider::OpenCode => {
                if let Some(usage) = opencode_usage {
                    entry.state = "ok";
                    entry.reason_code = None;
                    entry.local_usage = Some(LocalUsageJson {
                        tokens: usage.tokens,
                        cost_usd: usage.cost_usd,
                    });
                } else {
                    entry.reason = row.hint.clone();
                }
                entry.strip = strip_text(
                    row.provider.label(),
                    &opencode_segments(opencode_usage, preference, row.hint.as_deref())
                        .into_iter()
                        .map(|(text, _)| text)
                        .collect::<Vec<_>>(),
                );
            }
            _ => {
                let provider = row.provider.quota_provider().expect("quota provider");
                if let Some(snapshot) = &row.snapshot {
                    entry.fetched_at_unix = Some(snapshot.fetched_at_unix);
                    let code = cache.refresh_problem_code(provider);
                    let stale = code.is_none() && snapshot.refresh_warning.is_some();
                    if let Some(code) = code {
                        entry.reason_code = Some(code);
                        entry.reason = row.hint.clone().or(snapshot.refresh_warning.clone());
                    } else {
                        let cells = dashboard_cells(snapshot, now, percent, preference);
                        entry.windows = cells
                            .iter()
                            .map(|cell| window_json(cell, snapshot, now, percent))
                            .collect();
                        if stale {
                            entry.state = "stale";
                            entry.reason_code = Some("stale");
                            entry.reason = snapshot.refresh_warning.clone();
                        } else if !entry.windows.is_empty() {
                            entry.state = "ok";
                            entry.reason_code = None;
                        }
                    }
                }
                if entry.state == "unavailable" && entry.reason.is_none() {
                    entry.reason = row.hint.clone();
                }
                let segments = match &entry.reason {
                    Some(reason) => vec![reason.clone()],
                    None => provider_content(
                        provider,
                        row.snapshot.as_ref(),
                        now,
                        percent,
                        preference,
                        None,
                    )
                    .text(now, None)
                    .into_iter()
                    .map(|(text, _)| text)
                    .collect(),
                };
                entry.strip = strip_text(row.provider.label(), &segments);
            }
        }
        providers.push(entry);
    }
    let tightest = tightest(&rows, preferences).map(|tightest| {
        let window = &tightest.window;
        let reset_in = window
            .resets_at
            .map(|reset| reset.unix_seconds().saturating_sub(now));
        let pace = tightest.history_identity.as_ref().and_then(|identity| {
            let samples = cache.load_history(identity, window.kind);
            trend::pace(&samples, now, window.remaining_percent).map(|pace| PaceJson {
                percent_per_hour: pace.percent_per_hour,
                empties_in_seconds: pace.empties_in_seconds,
                outlasts_reset: match (pace.empties_in_seconds, reset_in) {
                    (Some(empties), Some(reset)) => Some(empties >= reset),
                    _ => None,
                },
            })
        });
        TightestJson {
            provider: tightest.provider.id(),
            label: tightest.provider.label(),
            window: window.display_label().to_string(),
            remaining_percent: window.remaining_percent,
            used_percent: window.used_percent,
            severity: severity_id(Severity::for_window(window, now)),
            resets_at_unix: window.resets_at.map(|reset| reset.unix_seconds()),
            resets_in_seconds: reset_in,
            pace,
        }
    });
    let sessions = panes
        .iter()
        .map(|pane| {
            let token = |name: &str, prefixes: &[&str]| {
                pane.tokens.get(name).and_then(|value| {
                    let mut value = value.as_str();
                    for prefix in prefixes {
                        value = value.strip_prefix(prefix).unwrap_or(value);
                    }
                    let value = value.trim();
                    (!value.is_empty()).then(|| value.to_string())
                })
            };
            SessionJson {
                pane_id: pane.pane_id.clone(),
                harness: crate::settings::agent_name(pane.harness),
                provider: token("quota_provider", &[]),
                model: token("quota_model", &[]),
                context: token("quota_context", &["context"]),
                cache: token("quota_cache", &["cache"]),
                ttl: token("quota_cache_ttl", &["ttl\u{2248}", "ttl"]),
            }
        })
        .collect();
    Ok(DashboardJson {
        schema: 1,
        generated_at_unix: now,
        updated_at_unix: rows
            .iter()
            .filter_map(|row| row.snapshot.as_ref())
            .filter(|snapshot| !snapshot.windows.is_empty())
            .map(|snapshot| snapshot.fetched_at_unix)
            .max(),
        percent_style: percent.as_str(),
        display: DisplayJson {
            bars: preferences.display.bars,
            reset_clock: preferences.display.reset_clock,
            meter_cells: METER_CELLS,
        },
        tightest,
        providers,
        sessions,
    })
}

fn window_json(
    cell: &WindowCell,
    snapshot: &ProviderSnapshot,
    now: u64,
    percent: PercentStyle,
) -> WindowJson {
    // The cell already applied the user's field choices; the window behind
    // it supplies the raw percentages a consumer may want regardless.
    let window = snapshot
        .windows
        .iter()
        .find(|window| dashboard_label_base(window.display_label()) == cell.label)
        .expect("a cell comes from one of the snapshot's windows");
    let shown = percent.percent_of(window);
    let filled = meter(shown, METER_CELLS, '#', '.')
        .chars()
        .filter(|glyph| *glyph == '#')
        .count();
    WindowJson {
        kind: window.kind.label(),
        label: cell.label.clone(),
        amount: cell.amount.clone(),
        used_percent: window.used_percent,
        remaining_percent: window.remaining_percent,
        shown_percent: shown,
        meter: MeterJson {
            filled,
            cells: METER_CELLS,
        },
        severity: severity_id(cell.severity),
        resets_at_unix: window.resets_at.map(|reset| reset.unix_seconds()),
        resets_in_seconds: window
            .resets_at
            .map(|reset| reset.unix_seconds().saturating_sub(now)),
        resets_in: window.resets_at.map(|reset| format_reset_eta(reset, now)),
    }
}

/// `plan $14.40` → `plan`; `7d` → `7d`. The same split the cells use.
fn dashboard_label_base(label: &str) -> &str {
    label
        .rsplit_once('$')
        .map(|(base, _)| base.trim_end())
        .unwrap_or(label)
}

/// `Codex 5h 64% reset 3h24m · 7d 14% reset 4d23h`, for a one-line strip.
fn strip_text(label: &str, segments: &[String]) -> String {
    if segments.is_empty() {
        return label.to_string();
    }
    format!("{label} {}", segments.join(" \u{00b7} "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BillingTarget, ResetAt, UsageWindow, WindowKind};
    use tempfile::tempdir;

    fn window(kind: WindowKind, used: f64, reset: Option<u64>) -> UsageWindow {
        UsageWindow::new(kind, used, reset.map(ResetAt::from_unix_seconds)).unwrap()
    }

    fn plain(frame: &str) -> String {
        let mut text = String::new();
        let mut chars = frame.chars().peekable();
        while let Some(character) = chars.next() {
            if character == '\u{1b}' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            text.push(character);
        }
        text
    }

    #[test]
    fn rows_without_quota_name_the_step_that_fixes_them() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .set_refresh_problem(Provider::Grok, Some("login"))
            .unwrap();
        cache
            .set_refresh_problem(Provider::Hermes, Some("credentials"))
            .unwrap();
        cache
            .set_refresh_problem(Provider::Codex, Some("failed"))
            .unwrap();
        let mut codex = ProviderSnapshot::new(
            Provider::Codex,
            vec![window(WindowKind::FiveHour, 50.0, None)],
            1000,
        );
        codex.refresh_warning = None;
        cache.save(&codex).unwrap();

        crate::signin::set_test_evidence(Some(crate::signin::Evidence {
            credential: None,
            installed: true,
        }));
        let frame = render_snapshot_with_opencode(&cache, 1001, None).unwrap();
        let model = dashboard_json(
            &cache,
            &DashboardPreferences::load_or_default(&cache),
            None,
            &[],
            1001,
        )
        .unwrap();
        crate::signin::set_test_evidence(None);

        assert!(
            frame.contains("Grok  sign-in expired · run grok login"),
            "{frame}"
        );
        assert!(
            frame.contains("Hermes  not signed in · run hermes portal login"),
            "{frame}"
        );
        assert!(
            frame.contains("Agy  no data yet · send one message in agy"),
            "{frame}"
        );
        assert!(
            frame.contains("OpenRouter  no API key · run quotadeck setup"),
            "{frame}"
        );
        assert!(
            frame.contains("Codex  refresh failed; check connection"),
            "a network failure keeps its own message: {frame}"
        );
        let grok = model
            .providers
            .iter()
            .find(|provider| provider.id == "grok")
            .unwrap();
        assert_eq!(grok.reason_code, Some("login"));
        assert_eq!(
            grok.reason.as_deref(),
            Some("sign-in expired · run grok login")
        );
        let agy = model
            .providers
            .iter()
            .find(|provider| provider.id == "agy")
            .unwrap();
        assert_eq!(agy.reason_code, Some("none"));
        assert_eq!(
            agy.reason.as_deref(),
            Some("no data yet · send one message in agy")
        );
    }

    #[test]
    fn failed_expired_and_recovered_refreshes_never_show_old_quota_as_current() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let provider = Provider::OpenRouter;
        let mut snapshot = ProviderSnapshot::new(
            provider,
            vec![window(WindowKind::Monthly, 61.0, None)],
            1000,
        );
        cache.save(&snapshot).unwrap();
        cache.set_refresh_problem(provider, Some("login")).unwrap();
        let frame = render_snapshot_with_opencode(&cache, 1001, None).unwrap();
        assert!(frame.contains("sign in again"));
        assert!(!frame.contains("39%"));
        cache.set_refresh_problem(provider, Some("failed")).unwrap();
        assert!(render_snapshot_with_opencode(&cache, 1002, None)
            .unwrap()
            .contains("refresh failed"));
        cache.set_refresh_problem(provider, None).unwrap();
        assert!(render_snapshot_with_opencode(&cache, 1201, None)
            .unwrap()
            .contains("stale; last update"));
        snapshot.fetched_at_unix = 1202;
        cache.save(&snapshot).unwrap();
        let frame = render_snapshot_with_opencode(&cache, 1203, None).unwrap();
        assert!(frame.contains("39%"));
        assert!(!frame.contains("sign in again"));
        assert!(!frame.contains("refresh failed"));
        assert!(cache
            .set_refresh_problem(provider, Some("secret-token"))
            .is_err());
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("refresh_warning"));
    }

    #[test]
    fn renders_compact_values_without_status_prose() {
        let snapshot = ProviderSnapshot::new(
            Provider::Claude,
            vec![
                window(WindowKind::FiveHour, 58.0, Some(14_820)),
                window(WindowKind::Weekly, 27.0, Some(183_600)),
            ],
            1,
        );
        let rendered = render_provider(
            Provider::Claude,
            Some(&snapshot),
            0,
            PercentStyle::default(),
        );
        assert_eq!(
            rendered,
            "\u{e1a0} Claude  5h 42% reset 4h07m · 7d 73% reset 2d3h"
        );
        assert!(!rendered.contains("WARN"));
        assert!(!rendered.contains("LOW"));
        assert!(!rendered.contains("left"));
    }

    #[test]
    fn cached_scoped_collectors_appear_once_and_latest_omp_wins() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut preferences = DashboardPreferences::default();
        preferences.get_mut(DashboardProvider::OpenCodeGo).show = true;
        preferences.save(&cache).unwrap();
        cache
            .save(&ProviderSnapshot::new(
                Provider::OpenCodeGo,
                vec![window(WindowKind::Monthly, 30.0, None)],
                1,
            ))
            .unwrap();
        cache
            .save_target(
                &BillingTarget::omp("older"),
                &ProviderSnapshot::new(
                    Provider::Omp,
                    vec![window(WindowKind::Weekly, 80.0, None)],
                    1,
                ),
            )
            .unwrap();
        cache
            .save_target(
                &BillingTarget::omp("newer"),
                &ProviderSnapshot::new(
                    Provider::Omp,
                    vec![window(WindowKind::Weekly, 31.0, None)],
                    2,
                ),
            )
            .unwrap();

        let rendered = render_snapshot_with_opencode(&cache, 0, None).unwrap();
        assert_eq!(rendered.matches("OpenCode Go").count(), 1, "{rendered}");
        assert_eq!(rendered.matches("OMP").count(), 1, "{rendered}");
        assert!(rendered.contains("OMP  7d 69%"), "{rendered}");
        assert!(!rendered.contains("OMP  7d 20%"), "{rendered}");
        let (_, identity) = load_latest_omp(&cache).unwrap().unwrap();
        assert!(identity.starts_with("omp-usage-"), "{identity}");
        assert!(identity.ends_with(".omp-store"), "{identity}");
    }

    #[test]
    fn dashboard_never_borrows_an_unscoped_session_window() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut snapshot = ProviderSnapshot::new(Provider::Claude, vec![], CacheStore::now_unix());
        snapshot.session_windows.insert(
            "live".to_string(),
            vec![window(WindowKind::Weekly, 51.0, Some(183_600))],
        );
        cache.save(&snapshot).unwrap();

        let rows = dashboard_rows(
            &cache,
            &DashboardPreferences::default(),
            0,
            AgentOrder::Default,
        )
        .unwrap();
        let claude = rows
            .into_iter()
            .find(|row| row.provider == DashboardProvider::Claude)
            .and_then(|row| row.snapshot)
            .unwrap();
        assert!(claude.windows.is_empty());
        let rendered = render_snapshot_with_opencode(&cache, 0, None).unwrap();
        assert!(!rendered.contains("49%"), "{rendered}");
    }

    #[test]
    fn quota_order_sorts_dashboard_rows_by_lowest_visible_headroom() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .set_agent_order(crate::cli::AgentOrder::Quota)
            .unwrap();
        let now = CacheStore::now_unix();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Codex,
                vec![window(WindowKind::FiveHour, 80.0, None)],
                now,
            ))
            .unwrap();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Grok,
                vec![window(WindowKind::Weekly, 10.0, None)],
                now,
            ))
            .unwrap();

        let rows = dashboard_rows(
            &cache,
            &DashboardPreferences::default(),
            0,
            cache.agent_order().unwrap(),
        )
        .unwrap();
        assert_eq!(rows[0].provider, DashboardProvider::Codex);
        assert_eq!(rows[1].provider, DashboardProvider::Grok);

        // The pane's own `o` override wins over the saved order.
        let rows = dashboard_rows(
            &cache,
            &DashboardPreferences::default(),
            0,
            AgentOrder::Default,
        )
        .unwrap();
        assert_eq!(rows[0].provider, DashboardProvider::Claude);
    }

    #[test]
    fn interactive_rows_use_independent_window_colors_and_an_opaque_panel() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .save(&ProviderSnapshot::new(
                Provider::Claude,
                vec![
                    window(WindowKind::FiveHour, 38.0, None),
                    window(WindowKind::Weekly, 76.0, None),
                ],
                CacheStore::now_unix(),
            ))
            .unwrap();
        let frame = render_terminal(&cache, 78, 18, None).unwrap();
        assert!(frame.starts_with("\u{1b}[48;2;14;16;20m"), "{frame:?}");
        assert!(frame.contains("\u{1b}[48;2;27;30;37m"), "{frame:?}");
        assert!(frame.contains("settings"), "{frame:?}");
        assert!(frame.contains("[s/click] · r refresh"), "{frame:?}");
        // The 5h cell: a six-cell green meter, then the green percentage.
        assert!(
            frame.contains("\u{1b}[38;2;130;217;120m██████\u{1b}[38;2;58;66;82m░░░░"),
            "{frame:?}"
        );
        assert!(frame.contains("\u{1b}[38;2;130;217;120m 62%"), "{frame:?}");
        assert!(frame.contains("\u{1b}[38;2;228;185;87m 24%"), "{frame:?}");
        assert_eq!(
            repaint_prefix(true),
            "\u{1b}[48;2;14;16;20m\u{1b}[2J\u{1b}[1;1H"
        );
        assert_eq!(repaint_prefix(false), "\u{1b}[48;2;14;16;20m\u{1b}[1;1H");
    }

    #[test]
    fn the_grid_aligns_short_and_long_windows_in_fixed_columns() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let now = CacheStore::now_unix();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Claude,
                vec![
                    window(WindowKind::FiveHour, 18.0, Some(now + 6_840)),
                    window(WindowKind::Weekly, 62.0, Some(now + 342_000)),
                ],
                now,
            ))
            .unwrap();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Grok,
                vec![window(WindowKind::Weekly, 73.0, Some(now + 255_600))],
                now,
            ))
            .unwrap();
        let frame = plain(&render_terminal(&cache, 78, 24, None).unwrap());
        let lines: Vec<&str> = frame.split("\r\n").collect();
        let header = lines.iter().find(|line| line.contains("resets")).unwrap();
        let claude = lines.iter().find(|line| line.contains("Claude")).unwrap();
        let grok = lines.iter().find(|line| line.contains("Grok")).unwrap();
        // Every grid line is exactly the terminal width, and the long-window
        // column starts at the same cell on every row.
        for line in [header, claude, grok] {
            assert_eq!(line.chars().count(), 78, "{line:?}");
        }
        let column = |line: &str, needle: &str| {
            let chars: Vec<char> = line.chars().collect();
            let needle: Vec<char> = needle.chars().collect();
            chars
                .windows(needle.len())
                .position(|window| window == needle)
                .unwrap()
        };
        assert_eq!(
            column(claude, "7d"),
            column(grok, "7d"),
            "{claude:?}\n{grok:?}"
        );
        assert!(claude.contains("5h  ████████░░  82%  1h54m"), "{claude:?}");
        assert!(claude.contains("7d  ████░░░░░░  38%  3d23h"), "{claude:?}");
        assert!(grok.contains("7d  ███░░░░░░░  27%  2d23h"), "{grok:?}");
        assert!(!grok.contains("5h"), "{grok:?}");
        assert!(!frame.contains("reset "), "{frame}");
        assert!(frame.contains("left"), "{frame}");
        // The tightest visible window leads the headline.
        assert!(
            frame.contains("▲ Grok 7d · 27% left · resets 2d23h"),
            "{frame}"
        );
        assert!(frame.contains("updated 0s ago"), "{frame}");
    }

    #[test]
    fn a_narrow_pane_flows_the_same_values_without_the_grid() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let now = CacheStore::now_unix();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Claude,
                vec![
                    window(WindowKind::FiveHour, 18.0, Some(now + 6_840)),
                    window(WindowKind::Weekly, 62.0, Some(now + 342_000)),
                ],
                now,
            ))
            .unwrap();
        // Too narrow for the grid: rows flow on one line without meters.
        let frame = plain(&render_terminal(&cache, 60, 24, None).unwrap());
        assert!(!frame.contains("resets  "), "{frame}");
        assert!(
            frame.contains("5h 82% reset 1h54m · 7d 38% reset 3d23h"),
            "{frame}"
        );
        assert!(!frame.contains('█'), "{frame}");
        // A pane with a little more room gets the grid back with a shorter
        // meter rather than a flowing line.
        let frame = plain(&render_terminal(&cache, 70, 24, None).unwrap());
        assert!(
            frame.contains("5h  █████░  82%  1h54m   7d  ██░░░░  38%  3d23h"),
            "{frame}"
        );

        let mut preferences = DashboardPreferences::default();
        preferences.display.bars = false;
        preferences.save(&cache).unwrap();
        // Without meters the grid is narrow enough for this pane again.
        let frame = plain(&render_terminal(&cache, 60, 24, None).unwrap());
        assert!(frame.contains("5h   82%  1h54m"), "{frame}");
        assert!(!frame.contains('█'), "{frame}");
    }

    #[test]
    fn dollar_rows_flow_with_a_meter_beside_each_balance() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let now = CacheStore::now_unix();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Hermes,
                vec![
                    window(WindowKind::Monthly, 28.0, Some(now + 2_242_800))
                        .with_source_window("plan $14.40", None),
                    UsageWindow::new(WindowKind::Weekly, 82.0, None)
                        .unwrap()
                        .with_source_window("top-up $3.60", None),
                ],
                now,
            ))
            .unwrap();
        // With both meters the row would wrap, so it keeps one line and
        // drops them; a wider pane gets them back.
        let frame = plain(&render_terminal(&cache, 78, 24, None).unwrap());
        assert!(
            frame.contains("plan 72% reset 25d23h · top-up $3.60 18%"),
            "{frame}"
        );
        let wide = plain(&render_terminal(&cache, 100, 24, None).unwrap());
        assert!(
            wide.contains("plan ███████░░░ 72% reset 25d23h · top-up $3.60 ██░░░░░░░░ 18%"),
            "{wide}"
        );
    }

    #[test]
    fn the_clock_choice_shows_wall_times_and_survives_reopening() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let now = CacheStore::now_unix();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Codex,
                vec![
                    window(WindowKind::FiveHour, 40.0, Some(now + 600)),
                    window(WindowKind::Weekly, 60.0, Some(now + 3 * 86_400)),
                ],
                now,
            ))
            .unwrap();
        assert!(
            !DashboardPreferences::load_or_default(&cache)
                .display
                .reset_clock
        );
        toggle_reset_clock(&cache);
        assert!(
            DashboardPreferences::load_or_default(&cache)
                .display
                .reset_clock
        );
        let view = View {
            clock_offset: Some(UtcOffset::UTC),
            ..View::default()
        };
        let frame = plain(
            &render_terminal_scrolled(&cache, 78, 24, None, &view)
                .unwrap()
                .text,
        );
        let expected = crate::presentation::format_reset(
            ResetAt::from_unix_seconds(now + 600),
            now,
            Some(UtcOffset::UTC),
        );
        assert_eq!(expected.len(), 5, "{expected}");
        let codex = frame
            .split("\r\n")
            .find(|line| line.contains("Codex"))
            .unwrap();
        // The grid keeps its columns with a narrower meter for clock times.
        assert!(
            codex.contains(&format!("5h  █████░░░  60%     {expected}")),
            "{codex:?}"
        );
        assert!(frame.contains("resets"), "{frame}");
        assert!(!frame.contains("(UTC)"), "{frame}");
        let frame = plain(&render_terminal(&cache, 78, 24, None).unwrap());
        assert!(frame.contains("resets (UTC)"), "{frame}");
    }

    #[test]
    fn the_headline_projects_from_recorded_history() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let now = CacheStore::now_unix();
        for (minutes_ago, used) in [(120, 10.0), (60, 30.0), (0, 50.0)] {
            cache
                .save(&ProviderSnapshot::new(
                    Provider::Codex,
                    vec![window(WindowKind::FiveHour, used, Some(now + 4 * 3_600))],
                    now - minutes_ago * 60,
                ))
                .unwrap();
        }
        // 20 points per hour with 50 left: empty in 2h30m, before the reset.
        let frame = plain(&render_terminal(&cache, 78, 24, None).unwrap());
        assert!(frame.contains("~2h30m at this pace"), "{frame}");
        assert!(frame.contains("Codex 5h · last 48h"), "{frame}");
        assert!(frame.contains("burn 20.0%/h"), "{frame}");
        assert!(frame.contains("48h ago"), "{frame}");

        // A pane too short for the sections keeps the rows and the headline.
        let short = plain(&render_terminal(&cache, 78, 18, None).unwrap());
        assert!(short.contains("~2h30m at this pace"), "{short}");
        assert!(!short.contains("last 48h"), "{short}");
    }

    #[test]
    fn the_sessions_section_reads_the_tokens_the_sidebar_shows() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let pane = AgentPane {
            pane_id: "%7".to_string(),
            harness: crate::model::Harness::Claude,
            session: None,
            session_summary: String::new(),
            topic: "secret prompt".to_string(),
            tokens: [
                ("quota_model", "Opus 5"),
                ("quota_context", "context 5%"),
                ("quota_cache", "cache 99.2%"),
                ("quota_cache_ttl", "ttl≈42m"),
                ("quota_topic", "secret prompt"),
            ]
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        };
        let view = View {
            panes: vec![pane],
            ..View::default()
        };
        let frame = plain(
            &render_terminal_scrolled(&cache, 78, 30, None, &view)
                .unwrap()
                .text,
        );
        assert!(frame.contains("Sessions"), "{frame}");
        assert!(frame.contains("claude  %7"), "{frame}");
        assert!(frame.contains("Opus 5"), "{frame}");
        assert!(frame.contains("99.2%"), "{frame}");
        assert!(frame.contains("42m"), "{frame}");
        assert!(!frame.contains("secret prompt"), "{frame}");
    }

    #[test]
    fn help_replaces_the_body_and_the_title_reports_refreshing() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let view = View {
            help: true,
            refreshing: true,
            ..View::default()
        };
        let frame = plain(
            &render_terminal_scrolled(&cache, 78, 20, None, &view)
                .unwrap()
                .text,
        );
        assert!(frame.contains("refreshing\u{2026}"), "{frame}");
        assert!(frame.contains("close this help"), "{frame}");
        assert!(!frame.contains("Claude"), "{frame}");
    }

    #[test]
    fn saved_display_choices_control_order_color_fields_and_footer() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut preferences = DashboardPreferences::default();
        preferences.move_by(0, 2);
        preferences.get_mut(DashboardProvider::Codex).show = false;
        preferences.get_mut(DashboardProvider::Claude).color = "#010203".to_string();
        for preference in &mut preferences.providers {
            preference.fields.retain(|field| {
                !matches!(
                    field,
                    DashboardField::ShortPercent
                        | DashboardField::LongPercent
                        | DashboardField::PlanPercent
                        | DashboardField::TopUpPercent
                        | DashboardField::CreditsPercent
                )
            });
        }
        preferences.save(&cache).unwrap();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Claude,
                vec![window(WindowKind::FiveHour, 95.0, Some(3_600))],
                CacheStore::now_unix(),
            ))
            .unwrap();

        let plain_output = render_snapshot_with_opencode(&cache, 0, None).unwrap();
        assert!(!plain_output.contains("Codex"), "{plain_output}");
        assert!(plain_output.find("Grok").unwrap() < plain_output.find("Claude").unwrap());
        let frame = render_terminal(&cache, 78, 18, None).unwrap();
        assert!(
            frame.contains("\u{1b}[38;2;1;2;3m  \u{e1a0} Claude"),
            "{frame:?}"
        );
        // Percentages are off, so the cell keeps only its reset time and
        // no headline names a window nobody can see.
        let text = plain(&frame);
        let claude = text
            .split("\r\n")
            .find(|line| line.contains("Claude"))
            .unwrap();
        assert!(
            claude.contains("5h") && claude.contains("due"),
            "{claude:?}"
        );
        assert!(!text.contains('%'), "{text}");
        assert!(!text.contains("tightest:"), "{text}");
        assert!(!text.contains('▲'), "{text}");
    }

    #[test]
    fn opencode_go_keeps_short_and_long_windows_and_money_keeps_severity() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut preferences = DashboardPreferences::default();
        preferences.get_mut(DashboardProvider::OpenCodeGo).show = true;
        preferences.save(&cache).unwrap();
        let now = CacheStore::now_unix();
        cache
            .save(&ProviderSnapshot::new(
                Provider::OpenCodeGo,
                vec![
                    window(WindowKind::FiveHour, 10.0, None),
                    window(WindowKind::Weekly, 20.0, None),
                    window(WindowKind::Monthly, 30.0, None),
                ],
                now,
            ))
            .unwrap();
        let plain_output = render_snapshot_with_opencode(&cache, now, None).unwrap();
        for expected in ["5h 90%", "7d 80%", "30d 70%"] {
            assert!(plain_output.contains(expected), "{plain_output}");
        }
        // Three windows do not fit two columns, so the row flows instead,
        // with meters as soon as the pane has room for them on one line.
        let frame = plain(&render_terminal(&cache, 78, 24, None).unwrap());
        assert!(frame.contains("5h 90% · 7d 80% · 30d 70%"), "{frame}");
        let wide = plain(&render_terminal(&cache, 84, 24, None).unwrap());
        assert!(
            wide.contains("5h █████████░ 90% · 7d ████████░░ 80% · 30d ███████░░░ 70%"),
            "{wide}"
        );

        let top_up =
            window(WindowKind::Weekly, 95.0, None).with_source_window("top-up $1.00", None);
        let snapshot = ProviderSnapshot::new(Provider::Hermes, vec![top_up], 1);
        let preference = ProviderPreference::defaults(DashboardProvider::Hermes);
        let segments = provider_segments(
            Provider::Hermes,
            Some(&snapshot),
            0,
            PercentStyle::Remaining,
            &preference,
        );
        assert!(segments[0].0.contains('$'));
        assert_eq!(segments[0].1, Severity::Danger);
    }

    #[test]
    fn dashboard_honors_each_stored_brand_glyph_set() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        for (glyphs, claude, opencode, openrouter) in [
            (
                crate::brand::GlyphSet::IconFont,
                "\u{e1a0} Claude",
                "\u{e1a2} OpenCode",
                "\u{e500} OpenRouter",
            ),
            (
                crate::brand::GlyphSet::Unicode,
                "✳ Claude",
                "❑ OpenCode",
                "⇄ OpenRouter",
            ),
            (
                crate::brand::GlyphSet::Off,
                "Claude",
                "OpenCode",
                "OpenRouter",
            ),
        ] {
            cache.set_brand_glyphs(glyphs).unwrap();

            let snapshot = render_snapshot_with_opencode(&cache, 0, None).unwrap();
            assert!(
                snapshot.contains(&format!("\r\n{claude}  ")),
                "{snapshot:?}"
            );
            assert!(
                snapshot.contains(&format!("\r\n{opencode}  30d N/A")),
                "{snapshot:?}"
            );
            assert!(
                snapshot.contains(&format!("\r\n{openrouter}  ")),
                "{snapshot:?}"
            );

            let frame = render_terminal(&cache, 78, 18, None).unwrap();
            assert!(frame.contains(&format!("  {claude}")), "{frame:?}");
            assert!(frame.contains(&format!("  {opencode}")), "{frame:?}");
            assert!(frame.contains(&format!("  {openrouter}")), "{frame:?}");
        }
    }

    #[test]
    fn local_opencode_usage_is_labeled_as_spend_not_remaining_quota() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let usage = Some(LocalUsage {
            tokens: 12_345_678,
            cost_usd: 4.567,
        });
        let snapshot = render_snapshot_with_opencode(&cache, 0, usage).unwrap();
        assert!(
            snapshot.contains("\u{e1a2} OpenCode  30d 12.3M tok · spent $4.57"),
            "{snapshot:?}"
        );
        assert!(!snapshot.contains("OpenCode  30d 12.3M tok · credits"));
    }

    #[test]
    fn plain_snapshot_is_ansi_free_and_returns_to_column_zero() {
        let directory = tempdir().unwrap();
        let rendered =
            render_snapshot_with_opencode(&CacheStore::new(directory.path()), 0, None).unwrap();
        assert!(rendered.contains("QuotaDeck\r\n\r\n\u{e1a0} Claude"));
        assert!(rendered.contains("\u{e1a2} OpenCode  30d N/A"));
        assert!(rendered.contains("OpenRouter  credentials unavailable"));
        assert!(!rendered.contains("Hermes •"), "{rendered:?}");
        assert!(!rendered.contains("OpenRouter •"), "{rendered:?}");
        assert!(rendered.contains("OMP  N/A"));
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains("QuotaDeck\n"));
        assert!(!rendered.contains('█'), "{rendered:?}");
    }

    #[test]
    fn a_short_terminal_renders_exactly_its_height_without_scrolling() {
        let directory = tempdir().unwrap();
        let frame = render_terminal(&CacheStore::new(directory.path()), 78, 5, None).unwrap();
        assert_eq!(frame.matches("\r\n").count(), 4, "{frame:?}");
    }

    #[test]
    fn a_short_terminal_scrolls_the_current_dashboard_without_growing_history() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let top = render_terminal_scrolled(&cache, 78, 5, None, &View::default()).unwrap();
        let bottom = render_terminal_scrolled(
            &cache,
            78,
            5,
            None,
            &View {
                scroll_offset: usize::MAX,
                ..View::default()
            },
        )
        .unwrap();

        assert!(top.max_scroll > 0);
        assert_eq!(bottom.max_scroll, top.max_scroll);
        assert_ne!(top.text, bottom.text);
        assert!(top.text.contains("QuotaDeck"), "{:?}", top.text);
        assert!(bottom.text.contains("OMP"), "{:?}", bottom.text);
        assert!(bottom.text.contains("↑/↓"), "{:?}", bottom.text);
        assert_eq!(bottom.text.matches("\r\n").count(), 4, "{:?}", bottom.text);
    }

    #[test]
    fn styled_lines_clip_before_the_terminal_wraps() {
        let mut output = String::new();
        push_styled_line(
            &mut output,
            &vec![(TEXT, "0123456789".to_string())],
            5,
            false,
        );
        assert!(output.contains("01234"), "{output:?}");
        assert!(!output.contains("012345"), "{output:?}");
    }

    #[test]
    fn only_the_settings_label_on_the_footer_is_clickable() {
        assert!(settings_link_hit(4, 20, 20, 78));
        assert!(settings_link_hit(11, 20, 20, 78));
        assert!(!settings_link_hit(3, 20, 20, 78));
        assert!(!settings_link_hit(12, 20, 20, 78));
        assert!(!settings_link_hit(4, 19, 20, 78));
    }

    #[test]
    fn the_json_view_model_carries_states_windows_meters_and_pace() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let now = CacheStore::now_unix();
        for (minutes_ago, used) in [(120, 10.0), (60, 30.0), (0, 50.0)] {
            cache
                .save(&ProviderSnapshot::new(
                    Provider::Codex,
                    vec![window(WindowKind::FiveHour, used, Some(now + 4 * 3_600))],
                    now - minutes_ago * 60,
                ))
                .unwrap();
        }
        cache
            .save(&ProviderSnapshot::new(
                Provider::Hermes,
                vec![window(WindowKind::Monthly, 28.0, Some(now + 2_242_800))
                    .with_source_window("plan $14.40", None)],
                now,
            ))
            .unwrap();
        cache
            .set_refresh_problem(Provider::Grok, Some("login"))
            .unwrap();
        let pane = AgentPane {
            pane_id: "%3".to_string(),
            harness: crate::model::Harness::Codex,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens: [("quota_model", "gpt-5.3"), ("quota_context", "context 41%")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let model = dashboard_json(
            &cache,
            &DashboardPreferences::default(),
            Some(LocalUsage {
                tokens: 1_500,
                cost_usd: 0.25,
            }),
            &[pane],
            now,
        )
        .unwrap();
        let text = serde_json::to_string(&model).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["updated_at_unix"], now);
        assert_eq!(value["display"]["meter_cells"], 10);
        let providers = value["providers"].as_array().unwrap();
        let find = |id: &str| {
            providers
                .iter()
                .find(|provider| provider["id"] == id)
                .unwrap()
        };
        let codex = find("codex");
        assert_eq!(codex["state"], "ok");
        assert!(codex["reason_code"].is_null());
        assert_eq!(codex["windows"][0]["kind"], "5h");
        assert_eq!(codex["windows"][0]["remaining_percent"], 50.0);
        assert_eq!(codex["windows"][0]["meter"]["filled"], 5);
        assert_eq!(codex["windows"][0]["severity"], "normal");
        assert_eq!(codex["windows"][0]["resets_in"], "4h00m");
        assert_eq!(codex["strip"], "Codex 5h 50% reset 4h00m");
        let hermes = find("hermes");
        assert_eq!(hermes["windows"][0]["label"], "plan");
        assert!(
            hermes["windows"][0]["amount"].is_null(),
            "amount is off by default"
        );
        assert_eq!(hermes["windows"][0]["used_percent"], 28.0);
        let grok = find("grok");
        assert_eq!(grok["state"], "unavailable");
        assert_eq!(grok["reason_code"], "login");
        assert_eq!(grok["reason"], "sign in again");
        assert_eq!(grok["strip"], "Grok sign in again");
        let claude = find("claude");
        assert_eq!(claude["reason_code"], "none");
        assert_eq!(claude["strip"], "Claude 5h N/A · 7d N/A");
        let opencode = find("opencode");
        assert_eq!(opencode["state"], "ok");
        assert_eq!(opencode["local_usage"]["tokens"], 1_500);
        assert_eq!(value["tightest"]["provider"], "codex");
        assert_eq!(value["tightest"]["pace"]["percent_per_hour"], 20.0);
        assert_eq!(value["tightest"]["pace"]["outlasts_reset"], false);
        assert_eq!(value["sessions"][0]["harness"], "codex");
        assert_eq!(value["sessions"][0]["context"], "41%");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn stale_rows_keep_their_values_in_json_but_say_so() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let now = CacheStore::now_unix();
        cache
            .save(&ProviderSnapshot::new(
                Provider::Codex,
                vec![window(WindowKind::Weekly, 40.0, None)],
                now - 1_000,
            ))
            .unwrap();
        let model =
            dashboard_json(&cache, &DashboardPreferences::default(), None, &[], now).unwrap();
        let codex = model
            .providers
            .iter()
            .find(|provider| provider.id == "codex")
            .unwrap();
        assert_eq!(codex.state, "stale");
        assert_eq!(codex.reason_code, Some("stale"));
        assert_eq!(codex.windows.len(), 1);
        assert!(
            model.tightest.is_none(),
            "a stale window is not the tightest"
        );
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        assert_eq!(format_age(12), "12s");
        assert_eq!(format_age(61), "1m");
        assert_eq!(format_age(7_200), "2h");
        assert_eq!(format_age(3 * 86_400), "3d");
    }
}
