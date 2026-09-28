//! The settings popup pane.
//!
//! Every option here is one `configure` already accepts. The pane does not
//! write configuration itself: it re-invokes this same binary, exactly as
//! Herdr's "Install / repair" action does. That keeps one writer for the
//! sidebar rows and the statusLine entries, and it keeps `configure`'s report
//! off a screen that is in raw mode.
//!
//! Applying finishes the job: it reloads Herdr's configuration so new rows
//! take effect, then forces one refresh so the tokens in those rows are
//! republished. Without that last step a changed percentage style would sit
//! invisible until the next agent event.

use crate::brand::GlyphSet;
use crate::cache::CacheStore;
use crate::cli::{
    AgentOrder, AgentSelection, BrandColors, FieldSet, LowQuotaAlert, PercentStyle, SidebarField,
    SidebarLayout, SidebarRowGap,
};
use crate::dashboard_prefs::{
    is_valid_color, DashboardField, DashboardPreferences, DashboardProvider,
};
use crate::model::Harness;
use crate::prefs;
use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use std::io::{self, IsTerminal, Write};
use std::process::Command;

/// Poll intervals the pane offers, in seconds. `configure` accepts anything
/// between 30s and 1h; these are the values worth one keypress.
const INTERVALS: [u64; 7] = [30, 60, 120, 300, 600, 1_800, 3_600];

const TARGET_WIDTH: u16 = 78;
const TARGET_HEIGHT: u16 = 30;
const FOOTER: &str = "↑/↓ move · Space change · a apply · q/Esc back";
const PROVIDER_FOOTER: &str =
    "↑↓ move · u/d order · Space show · Enter fields · c color · a apply · q back";

const OUTER_BG: Color = rgb(14, 16, 20);
const PANEL_BG: Color = rgb(27, 30, 37);
const SELECTED_BG: Color = rgb(50, 56, 68);
const TEXT: Color = rgb(232, 237, 247);
const MUTED: Color = rgb(144, 155, 175);
const CYAN: Color = rgb(89, 192, 228);

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb { r, g, b }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Fields,
    Agents,
    DashboardProviders,
    DashboardFields(DashboardProvider),
}

/// Selectable rows on the current compact page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Choice(Choice),
    /// A display choice stored with the dashboard settings: it needs no
    /// `configure` run, only a save.
    Display(DisplayChoice),
    Fields,
    Agents,
    DashboardProviders,
    DashboardProvider(DashboardProvider),
    DashboardField(DashboardField),
    Field(SidebarField),
    Agent(Harness),
}

/// A row whose value cycles through a small list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Percent,
    Layout,
    RowGap,
    Interval,
    Brand,
    Glyphs,
    Order,
    Alert,
}

/// A two-way display choice kept in the dashboard settings file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayChoice {
    Bars,
    ResetClock,
    SidebarMeter,
}

impl DisplayChoice {
    fn label(self) -> &'static str {
        match self {
            Self::Bars => "Dashboard bars",
            Self::ResetClock => "Reset times",
            Self::SidebarMeter => "Sidebar meter",
        }
    }

    fn value(self, dashboard: &DashboardPreferences) -> &'static str {
        match self {
            Self::Bars => on_off(dashboard.display.bars),
            Self::ResetClock => {
                if dashboard.display.reset_clock {
                    "clock"
                } else {
                    "countdown"
                }
            }
            Self::SidebarMeter => on_off(dashboard.display.sidebar_meter),
        }
    }

    fn toggle(self, dashboard: &mut DashboardPreferences) {
        let display = &mut dashboard.display;
        match self {
            Self::Bars => display.bars = !display.bars,
            Self::ResetClock => display.reset_clock = !display.reset_clock,
            Self::SidebarMeter => display.sidebar_meter = !display.sidebar_meter,
        }
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

impl Choice {
    fn label(self) -> &'static str {
        match self {
            Self::Percent => "Percentages",
            Self::Layout => "Sidebar layout",
            Self::RowGap => "Row gap",
            Self::Interval => "Watch interval",
            Self::Brand => "Brand colors",
            Self::Glyphs => "Brand glyphs",
            Self::Order => "Row order",
            Self::Alert => "Low quota alert",
        }
    }
}

fn rows(page: Page, dashboard: &DashboardPreferences) -> Vec<Row> {
    match page {
        Page::Main => vec![
            Row::Choice(Choice::Percent),
            Row::Display(DisplayChoice::Bars),
            Row::Display(DisplayChoice::ResetClock),
            Row::Choice(Choice::Layout),
            Row::Choice(Choice::RowGap),
            Row::Display(DisplayChoice::SidebarMeter),
            Row::Choice(Choice::Interval),
            Row::Choice(Choice::Brand),
            Row::Choice(Choice::Glyphs),
            Row::Choice(Choice::Order),
            Row::Choice(Choice::Alert),
            Row::Fields,
            Row::DashboardProviders,
            Row::Agents,
        ],
        Page::Fields => SidebarField::ALL.into_iter().map(Row::Field).collect(),
        Page::Agents => AgentSelection::SUPPORTED
            .into_iter()
            .map(Row::Agent)
            .collect(),
        Page::DashboardProviders => dashboard
            .providers
            .iter()
            .map(|preference| Row::DashboardProvider(preference.provider))
            .collect(),
        Page::DashboardFields(provider) => provider
            .allowed_fields()
            .iter()
            .copied()
            .map(Row::DashboardField)
            .collect(),
    }
}

fn initial_selection() -> usize {
    rows(Page::Main, &DashboardPreferences::default())
        .iter()
        .position(|row| matches!(row, Row::Choice(Choice::Glyphs)))
        .unwrap_or(0)
}

/// The choices as the user has them on screen, before applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    percent: PercentStyle,
    layout: SidebarLayout,
    gap: SidebarRowGap,
    interval_seconds: u64,
    brand: BrandColors,
    glyphs: GlyphSet,
    order: AgentOrder,
    alert: LowQuotaAlert,
    fields: FieldSet,
    /// Indexed by [`AgentSelection::SUPPORTED`], so the whole struct stays
    /// `Copy` and comparing a draft to what is applied is one `==`.
    agents: [bool; AgentSelection::SUPPORTED.len()],
}

impl Settings {
    /// What a fresh `configure` run would resolve today, so the pane opens on
    /// the installation's real state rather than on defaults.
    fn current(cache: Option<&CacheStore>) -> Self {
        let installed = AgentSelection::from_args_or_env(&[]);
        let mut agents = [false; AgentSelection::SUPPORTED.len()];
        for (slot, harness) in agents.iter_mut().zip(AgentSelection::SUPPORTED) {
            *slot = installed.contains(&harness);
        }
        Self {
            percent: crate::configure::resolved_percent_style(None, cache),
            layout: crate::configure::resolved_sidebar_layout(None, cache),
            gap: crate::configure::resolved_row_gap(None, cache),
            interval_seconds: cache
                .map(CacheStore::watch_interval_seconds)
                .unwrap_or(crate::cache::DEFAULT_WATCH_INTERVAL_SECONDS),
            brand: crate::configure::resolved_brand_colors(None, cache),
            glyphs: crate::configure::resolved_brand_glyphs(None, cache),
            order: crate::configure::resolved_agent_order(None, cache),
            alert: crate::configure::resolved_low_quota_alert(None, cache),
            fields: crate::configure::resolved_fields(None, cache),
            agents,
        }
    }

    fn choice_value(self, choice: Choice) -> String {
        match choice {
            Choice::Percent => self.percent.as_str().to_string(),
            Choice::Layout => self.layout.as_str().to_string(),
            Choice::RowGap => self.gap.to_string(),
            Choice::Interval => format_interval(self.interval_seconds),
            Choice::Brand => self.brand.as_str().to_string(),
            Choice::Glyphs => self.glyphs.as_str().to_string(),
            Choice::Order => match self.order {
                AgentOrder::Default => "manual".to_string(),
                AgentOrder::Quota => "least left".to_string(),
            },
            Choice::Alert => self.alert.to_string(),
        }
    }

    fn agents(self) -> Vec<Harness> {
        AgentSelection::SUPPORTED
            .into_iter()
            .zip(self.agents)
            .filter_map(|(harness, on)| on.then_some(harness))
            .collect()
    }

    fn has_agent(self, harness: Harness) -> bool {
        self.agents().contains(&harness)
    }

    /// Agents this draft would remove from an installation that has `applied`.
    fn removed_agents(self, applied: Settings) -> Vec<Harness> {
        AgentSelection::SUPPORTED
            .into_iter()
            .filter(|harness| applied.has_agent(*harness) && !self.has_agent(*harness))
            .collect()
    }

    /// Move one row's value by `step` (+1 or -1). Two-way options flip on
    /// either arrow, so no one has to remember which direction they live in.
    fn cycle(&mut self, row: Row, step: i8) {
        match row {
            Row::Fields
            | Row::Display(_)
            | Row::Agents
            | Row::DashboardProviders
            | Row::DashboardProvider(_)
            | Row::DashboardField(_) => {}
            Row::Choice(Choice::Percent) => {
                self.percent = match self.percent {
                    PercentStyle::Remaining => PercentStyle::Used,
                    PercentStyle::Used => PercentStyle::Remaining,
                }
            }
            Row::Choice(Choice::Layout) => {
                self.layout = match self.layout {
                    SidebarLayout::Packed => SidebarLayout::Stacked,
                    SidebarLayout::Stacked => SidebarLayout::Packed,
                }
            }
            Row::Choice(Choice::RowGap) => {
                self.gap = match self.gap.as_u8() {
                    0 => SidebarRowGap::SEPARATED,
                    _ => SidebarRowGap::FLUSH,
                }
            }
            Row::Choice(Choice::Brand) => {
                self.brand = match self.brand {
                    BrandColors::On => BrandColors::Off,
                    BrandColors::Off => BrandColors::On,
                }
            }
            Row::Choice(Choice::Glyphs) => {
                self.glyphs = if step < 0 {
                    self.glyphs.next().next()
                } else {
                    self.glyphs.next()
                };
            }
            Row::Choice(Choice::Order) => {
                self.order = match self.order {
                    AgentOrder::Default => AgentOrder::Quota,
                    AgentOrder::Quota => AgentOrder::Default,
                }
            }
            Row::Choice(Choice::Alert) => {
                let current = LowQuotaAlert::CHOICES
                    .iter()
                    .position(|value| *value == self.alert)
                    .unwrap_or(0);
                let count = LowQuotaAlert::CHOICES.len() as i8;
                let next = (current as i8 + step).rem_euclid(count);
                self.alert = LowQuotaAlert::CHOICES[next as usize];
            }
            Row::Choice(Choice::Interval) => {
                let current = INTERVALS
                    .iter()
                    .position(|value| *value == self.interval_seconds)
                    .unwrap_or(1);
                let count = INTERVALS.len() as i8;
                let next = (current as i8 + step).rem_euclid(count);
                self.interval_seconds = INTERVALS[next as usize];
            }
            Row::Field(field) => self.fields = self.fields.toggled(field),
            Row::Agent(harness) => {
                if let Some(index) = AgentSelection::SUPPORTED
                    .iter()
                    .position(|supported| *supported == harness)
                {
                    self.agents[index] = !self.agents[index];
                }
            }
        }
    }

    /// The `configure --apply` invocation that makes this installation match
    /// the pane.
    ///
    /// Every value is passed explicitly, so applying cannot inherit a stale
    /// preference from an earlier installer run.
    fn apply_arguments(self) -> Vec<String> {
        let mut arguments = vec![
            "configure".to_string(),
            "--apply".to_string(),
            "--agent".to_string(),
            agent_list(&self.agents()),
            "--quota-percent".to_string(),
            self.percent.as_str().to_string(),
            "--sidebar-layout".to_string(),
            self.layout.as_str().to_string(),
            "--row-gap".to_string(),
            self.gap.to_string(),
            "--brand-colors".to_string(),
            self.brand.as_str().to_string(),
            "--brand-glyphs".to_string(),
            self.glyphs.as_str().to_string(),
            "--fields".to_string(),
            self.fields.as_list(),
            "--agent-order".to_string(),
            self.order.as_str().to_string(),
            "--low-quota-alert".to_string(),
            self.alert.to_string(),
        ];
        arguments.push("--watch-interval-seconds".to_string());
        arguments.push(self.interval_seconds.to_string());
        arguments
    }

    /// Removing an agent is an uninstall of that agent's collector, not a
    /// narrower install: its statusLine entry and hook file have to be given
    /// back before the remaining agents are re-applied.
    fn uninstall_arguments(removed: &[Harness]) -> Vec<String> {
        vec![
            "configure".to_string(),
            "--uninstall".to_string(),
            "--agent".to_string(),
            agent_list(removed),
        ]
    }
}

/// The name written to the agents pref and read back by
/// [`crate::cli::AgentSelection::parse`]. Visible to the crate so that
/// round trip can be asserted rather than assumed.
pub(crate) fn agent_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
        Harness::Grok => "grok",
        Harness::Agy => "agy",
        Harness::OpenCode => "opencode",
        Harness::Pi => "pi",
        Harness::Omp => "omp",
        Harness::Hermes => "hermes",
    }
}

fn agent_list(agents: &[Harness]) -> String {
    agents
        .iter()
        .copied()
        .map(agent_name)
        .collect::<Vec<_>>()
        .join(",")
}

fn format_interval(seconds: u64) -> String {
    if seconds > 60 && seconds.is_multiple_of(60) {
        return format!("{}m", seconds / 60);
    }
    format!("{seconds}s")
}

pub fn run() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let settings = Settings::current(Some(&cache));
    let dashboard = DashboardPreferences::load(&cache)?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print!(
            "{}",
            render(
                &settings,
                settings,
                (&dashboard, &dashboard),
                (Page::Main, initial_selection()),
                TARGET_WIDTH,
                None,
                false,
            )
        );
        return Ok(());
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let result = crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)
        .context("enter settings screen")
        .and_then(|_| interactive(settings, dashboard, &cache));
    let terminal_cleanup = crossterm::execute!(
        stdout,
        ResetColor,
        Show,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .context("leave settings screen");
    let raw_cleanup = disable_raw_mode().context("disable settings raw mode");
    result.and(terminal_cleanup).and(raw_cleanup)
}

fn interactive(
    applied: Settings,
    dashboard_applied: DashboardPreferences,
    cache: &CacheStore,
) -> Result<()> {
    let mut applied = applied;
    let mut draft = applied;
    let mut dashboard_applied = dashboard_applied;
    let mut dashboard_draft = dashboard_applied.clone();
    let mut page = Page::Main;
    let mut selected = initial_selection();
    let mut status: Option<String> = None;
    let mut color_input: Option<(DashboardProvider, String)> = None;
    let mut confirming = false;
    let mut painted: Option<String> = None;
    loop {
        let (width, height) = crossterm::terminal::size().unwrap_or((TARGET_WIDTH, TARGET_HEIGHT));
        let editing_status = color_input
            .as_ref()
            .map(|(_, value)| format!("Color {value} · Enter save · Esc cancel"));
        let frame = fit_height(
            render(
                &draft,
                applied,
                (&dashboard_draft, &dashboard_applied),
                (page, selected),
                width,
                status.as_deref().or(editing_status.as_deref()),
                true,
            ),
            height,
            selected_line(page, selected),
        );
        if painted.as_deref() != Some(frame.as_str()) {
            print!("{}{frame}", repaint_prefix());
            io::stdout().flush()?;
            painted = Some(frame);
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Raw mode delivers Ctrl+C as a key event, so the pane has to honour
        // it itself or there is no way out but killing the pane.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if let Some((provider, input)) = color_input.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    color_input = None;
                    status = None;
                }
                KeyCode::Enter if is_valid_color(input) => {
                    input.make_ascii_uppercase();
                    dashboard_draft.get_mut(*provider).color.clone_from(input);
                    color_input = None;
                    status = None;
                }
                KeyCode::Enter => status = Some("Use exactly #RRGGBB.".to_string()),
                KeyCode::Backspace => {
                    input.pop();
                    status = None;
                }
                KeyCode::Char(character)
                    if input.len() < 7 && (character == '#' || character.is_ascii_hexdigit()) =>
                {
                    input.push(character);
                    status = None;
                }
                _ => {}
            }
            continue;
        }
        let active_rows = rows(page, &dashboard_draft);
        selected = selected.min(active_rows.len().saturating_sub(1));
        let row = active_rows[selected];
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if back(&mut page, &mut selected, &dashboard_draft) {
                    return Ok(());
                }
                status = None;
                confirming = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                selected = step_selection(&active_rows, selected, -1)
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                selected = step_selection(&active_rows, selected, 1)
            }
            KeyCode::Char('u') if page == Page::DashboardProviders => {
                selected = dashboard_draft.move_by(selected, -1);
                status = None;
            }
            KeyCode::Char('d') if page == Page::DashboardProviders => {
                selected = dashboard_draft.move_by(selected, 1);
                status = None;
            }
            KeyCode::Char('c') if page == Page::DashboardProviders => {
                if let Row::DashboardProvider(provider) = row {
                    color_input = Some((provider, dashboard_draft.get(provider).color.clone()));
                    status = None;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                change(
                    &mut page,
                    &mut selected,
                    &mut draft,
                    &mut dashboard_draft,
                    row,
                    -1,
                );
                status = None;
                confirming = false;
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                change(
                    &mut page,
                    &mut selected,
                    &mut draft,
                    &mut dashboard_draft,
                    row,
                    1,
                );
                status = None;
                confirming = false;
            }
            KeyCode::Enter
                if matches!(
                    row,
                    Row::Fields | Row::Agents | Row::DashboardProviders | Row::DashboardProvider(_)
                ) =>
            {
                change(
                    &mut page,
                    &mut selected,
                    &mut draft,
                    &mut dashboard_draft,
                    row,
                    2,
                );
                status = None;
                confirming = false;
            }
            KeyCode::Char('a') | KeyCode::Enter => {
                let (next_status, confirmed) = attempt_apply(
                    draft,
                    &mut applied,
                    &dashboard_draft,
                    &mut dashboard_applied,
                    cache,
                    confirming,
                );
                status = Some(next_status);
                confirming = confirmed;
            }
            _ => {}
        }
    }
}

fn back(page: &mut Page, selected: &mut usize, dashboard: &DashboardPreferences) -> bool {
    match *page {
        Page::Main => true,
        Page::Fields => {
            *page = Page::Main;
            *selected = 8;
            false
        }
        Page::DashboardProviders => {
            *page = Page::Main;
            *selected = 9;
            false
        }
        Page::Agents => {
            *page = Page::Main;
            *selected = 10;
            false
        }
        Page::DashboardFields(provider) => {
            *page = Page::DashboardProviders;
            *selected = dashboard
                .providers
                .iter()
                .position(|preference| preference.provider == provider)
                .unwrap_or(0);
            false
        }
    }
}

fn change(
    page: &mut Page,
    selected: &mut usize,
    draft: &mut Settings,
    dashboard: &mut DashboardPreferences,
    row: Row,
    step: i8,
) {
    match row {
        Row::Fields => {
            *page = Page::Fields;
            *selected = 0;
        }
        Row::Agents => {
            *page = Page::Agents;
            *selected = 0;
        }
        Row::DashboardProviders => {
            *page = Page::DashboardProviders;
            *selected = 0;
        }
        Row::DashboardProvider(provider) if step == 2 => {
            *page = Page::DashboardFields(provider);
            *selected = 0;
        }
        Row::DashboardProvider(provider) => {
            let preference = dashboard.get_mut(provider);
            preference.show = !preference.show;
        }
        Row::DashboardField(field) => {
            let Page::DashboardFields(provider) = *page else {
                return;
            };
            let fields = &mut dashboard.get_mut(provider).fields;
            if !fields.remove(&field) {
                fields.insert(field);
            }
        }
        Row::Display(choice) => choice.toggle(dashboard),
        _ => draft.cycle(row, step),
    }
}

/// One `a` press. Returns the line to show and whether the next press is a
/// confirmation of an agent removal.
fn attempt_apply(
    draft: Settings,
    applied: &mut Settings,
    dashboard_draft: &DashboardPreferences,
    dashboard_applied: &mut DashboardPreferences,
    cache: &CacheStore,
    confirming: bool,
) -> (String, bool) {
    if !apply_needed(&draft, applied, dashboard_draft, dashboard_applied, cache) {
        return ("Nothing to apply.".to_string(), false);
    }
    if draft.agents().is_empty() {
        return ("Keep at least one agent.".to_string(), false);
    }
    let removed = draft.removed_agents(*applied);
    if !removed.is_empty() && !confirming {
        return (
            format!(
                "Removing {} restores its own config. Press a again.",
                agent_list(&removed)
            ),
            true,
        );
    }
    let (dashboard_saved, result) = apply(draft, dashboard_draft, cache, &removed);
    match result {
        Ok(warning) => {
            *applied = draft;
            dashboard_applied.clone_from(dashboard_draft);
            let message = warning.map_or_else(
                || "Applied. Restart running agent panes to reload hooks.".to_string(),
                |warning| format!("Applied; {warning}"),
            );
            (message, false)
        }
        Err(error) if dashboard_saved => (
            format!("Saved; configuration incomplete: {error}. Press a to retry."),
            false,
        ),
        Err(error) => (format!("Failed: {error}"), false),
    }
}

fn apply_needed(
    draft: &Settings,
    applied: &Settings,
    dashboard_draft: &DashboardPreferences,
    dashboard_applied: &DashboardPreferences,
    cache: &CacheStore,
) -> bool {
    draft != applied || dashboard_draft != dashboard_applied || cache.settings_apply_pending()
}

/// Write the configuration, make Herdr re-read it, then republish the tokens.
///
/// `configure` runs as a child process rather than in-process: it prints a
/// report, and this screen is in raw mode. The child inherits Herdr's plugin
/// environment, which is what lets it write at all.
fn apply(
    settings: Settings,
    dashboard: &DashboardPreferences,
    cache: &CacheStore,
    removed: &[Harness],
) -> (bool, Result<Option<String>>) {
    if let Err(error) = cache.set_settings_apply_pending() {
        return (false, Err(error));
    }
    if let Err(error) = dashboard.save(cache) {
        let _ = cache.clear_settings_apply_pending();
        return (false, Err(error));
    }
    match apply_configure(settings, removed) {
        Ok(warning) => (true, cache.clear_settings_apply_pending().map(|()| warning)),
        Err(error) => (true, Err(error)),
    }
}

fn apply_configure(settings: Settings, removed: &[Harness]) -> Result<Option<String>> {
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    if !removed.is_empty() {
        run_self(&executable, &Settings::uninstall_arguments(removed))?;
    }
    // A Herdr plugin action runs a fixed command line, so the agent selection
    // has to be stored where a later "Install / repair" will find it.
    prefs::write(prefs::AGENTS, &agent_list(&settings.agents()))?;
    run_self(&executable, &settings.apply_arguments())?;

    let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let reload = Command::new(herdr)
        .args(["server", "reload-config"])
        .output();
    let reload_warning = match reload {
        Ok(output) if output.status.success() => None,
        Ok(output) => Some(
            first_line(&String::from_utf8_lossy(&output.stderr))
                .unwrap_or_else(|| "Herdr reload failed".to_string()),
        ),
        Err(error) => Some(format!("Herdr reload failed: {error}")),
    };
    // Reloading redraws the rows; the tokens inside them are only republished
    // by a refresh, so a changed percentage style would otherwise wait for the
    // next agent event.
    let refresh_warning = run_self(
        &executable,
        &[
            "refresh".to_string(),
            "--provider".to_string(),
            "all".to_string(),
            "--force".to_string(),
        ],
    )
    .err()
    .map(|error| format!("refresh failed: {error}"));
    Ok([reload_warning, refresh_warning]
        .into_iter()
        .flatten()
        .next())
}

fn run_self(executable: &std::path::Path, arguments: &[String]) -> Result<()> {
    let output = Command::new(executable)
        .args(arguments)
        .output()
        .with_context(|| {
            format!(
                "run {}",
                arguments.first().map_or("configure", String::as_str)
            )
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "{}",
            first_line(&String::from_utf8_lossy(&output.stderr))
                .unwrap_or_else(|| "configure failed".to_string())
        );
    }
    Ok(())
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(56).collect())
}

/// Move the selection by one row and wrap at either end.
fn step_selection(rows: &[Row], selected: usize, step: isize) -> usize {
    if rows.is_empty() {
        return 0;
    }
    (selected as isize + step).rem_euclid(rows.len() as isize) as usize
}

/// The whole compact frame. ANSI styling is emitted only for the interactive
/// PTY; redirected output stays deterministic and plain.
fn render(
    draft: &Settings,
    applied: Settings,
    dashboard: (&DashboardPreferences, &DashboardPreferences),
    location: (Page, usize),
    width: u16,
    status: Option<&str>,
    ansi: bool,
) -> String {
    if ansi {
        crossterm::style::force_color_output(true);
    }
    let width = usize::from(width.max(1));
    let (dashboard_draft, dashboard_applied) = dashboard;
    let (page, selected) = location;
    let mut output = String::new();
    push_outer_blank(&mut output, width, ansi);
    draw_blank(&mut output, width, ansi);

    match page {
        Page::Main => render_main(
            &mut output,
            draft,
            applied,
            (dashboard_draft, dashboard_applied),
            selected,
            width,
            ansi,
        ),
        Page::Fields | Page::Agents => {
            render_detail(&mut output, draft, applied, page, selected, width, ansi)
        }
        Page::DashboardProviders => render_dashboard_providers(
            &mut output,
            dashboard_draft,
            dashboard_applied,
            selected,
            width,
            ansi,
        ),
        Page::DashboardFields(provider) => render_dashboard_fields(
            &mut output,
            dashboard_draft,
            dashboard_applied,
            provider,
            selected,
            width,
            ansi,
        ),
    }
    render_status_and_footer(&mut output, status, footer(page), width, ansi);

    draw_blank(&mut output, width, ansi);
    push_outer_blank(&mut output, width, ansi);
    output
}

fn fit_height(frame: String, height: u16, selected_line: usize) -> String {
    let lines = frame
        .trim_end_matches("\r\n")
        .split("\r\n")
        .collect::<Vec<_>>();
    let height = usize::from(height.max(1));
    if lines.len() <= height {
        return lines.join("\r\n");
    }
    let footer_index = lines.len().saturating_sub(3);
    let footer = lines.get(footer_index).copied().unwrap_or_default();
    let body_height = height.saturating_sub(1);
    let max_start = footer_index.saturating_sub(body_height);
    let start = selected_line.saturating_sub(body_height / 2).min(max_start);
    let mut visible = lines
        .iter()
        .skip(start)
        .take(body_height.min(footer_index.saturating_sub(start)))
        .copied()
        .collect::<Vec<_>>();
    visible.push(footer);
    visible.join("\r\n")
}

fn selected_line(page: Page, selected: usize) -> usize {
    match page {
        // The agents row sits below a blank line and its group heading.
        Page::Main if selected + 1 == rows(Page::Main, &DashboardPreferences::default()).len() => {
            4 + selected + 2
        }
        Page::Main => 4 + selected,
        Page::DashboardProviders => 6 + selected,
        Page::Fields | Page::Agents | Page::DashboardFields(_) => 4 + selected,
    }
}

fn footer(page: Page) -> &'static str {
    match page {
        Page::DashboardProviders => PROVIDER_FOOTER,
        _ => FOOTER,
    }
}

fn render_main(
    output: &mut String,
    draft: &Settings,
    applied: Settings,
    dashboard: (&DashboardPreferences, &DashboardPreferences),
    selected: usize,
    width: usize,
    ansi: bool,
) {
    let (dashboard_draft, dashboard_applied) = dashboard;
    let title = if draft != &applied || dashboard_draft != dashboard_applied {
        "QuotaDeck settings *"
    } else {
        "QuotaDeck settings"
    };
    draw_row(output, title, "", CYAN, false, width, ansi);
    draw_blank(output, width, ansi);

    let main_rows = rows(Page::Main, dashboard_draft);
    for (index, row) in main_rows.iter().enumerate() {
        let row_selected = index == selected;
        let (label, value, color) = match row {
            Row::Choice(choice) => (
                marked_label(
                    choice.label(),
                    draft.choice_value(*choice) != applied.choice_value(*choice),
                ),
                draft.choice_value(*choice),
                TEXT,
            ),
            Row::Display(choice) => (
                marked_label(
                    choice.label(),
                    choice.value(dashboard_draft) != choice.value(dashboard_applied),
                ),
                choice.value(dashboard_draft).to_string(),
                TEXT,
            ),
            Row::Fields => (
                marked_label("Fields", draft.fields != applied.fields),
                field_summary(*draft),
                TEXT,
            ),
            Row::DashboardProviders => {
                let visible = dashboard_draft
                    .providers
                    .iter()
                    .filter(|preference| preference.show)
                    .count();
                (
                    marked_label(
                        "Dashboard providers",
                        dashboard_draft.providers != dashboard_applied.providers,
                    ),
                    format!("{visible}/{}", dashboard_draft.providers.len()),
                    TEXT,
                )
            }
            // Agents are drawn as their own group below.
            Row::Agents => continue,
            Row::DashboardProvider(_) | Row::DashboardField(_) | Row::Field(_) | Row::Agent(_) => {
                unreachable!()
            }
        };
        draw_row(
            output,
            &label,
            &selection_value(&value, row_selected),
            color,
            row_selected,
            width,
            ansi,
        );
    }
    draw_blank(output, width, ansi);

    let agents_selected = selected + 1 == main_rows.len();
    draw_row(output, "Agents", "", MUTED, false, width, ansi);
    let primary_dirty = draft.agents[..7] != applied.agents[..7];
    let primary_label = marked_label("claude codex grok agy opencode pi omp", primary_dirty);
    let primary_value = selection_value(agent_group_state(&draft.agents[..7]), agents_selected);
    draw_row(
        output,
        &primary_label,
        &primary_value,
        MUTED,
        agents_selected,
        width,
        ansi,
    );
    let last = AgentSelection::SUPPORTED.len() - 1;
    let hermes_label = marked_label("hermes", draft.agents[last] != applied.agents[last]);
    draw_row(
        output,
        &hermes_label,
        if draft.agents[last] { "on" } else { "off" },
        MUTED,
        false,
        width,
        ansi,
    );
}

fn render_detail(
    output: &mut String,
    draft: &Settings,
    applied: Settings,
    page: Page,
    selected: usize,
    width: usize,
    ansi: bool,
) {
    let title = match page {
        Page::Fields => "QuotaDeck settings / Fields",
        Page::Agents => "QuotaDeck settings / Agents",
        Page::Main | Page::DashboardProviders | Page::DashboardFields(_) => unreachable!(),
    };
    draw_row(output, title, "", CYAN, false, width, ansi);
    draw_blank(output, width, ansi);
    for (index, row) in rows(page, &DashboardPreferences::default())
        .into_iter()
        .enumerate()
    {
        let (label, on, dirty) = match row {
            Row::Field(field) => (
                field.name(),
                draft.fields.contains(field),
                draft.fields.contains(field) != applied.fields.contains(field),
            ),
            Row::Agent(harness) => (
                agent_name(harness),
                draft.has_agent(harness),
                draft.has_agent(harness) != applied.has_agent(harness),
            ),
            _ => unreachable!(),
        };
        let selected = selected == index;
        let label = marked_label(label, dirty);
        let value = selection_value(if on { "on" } else { "off" }, selected);
        draw_row(output, &label, &value, TEXT, selected, width, ansi);
    }
}

fn render_dashboard_providers(
    output: &mut String,
    draft: &DashboardPreferences,
    applied: &DashboardPreferences,
    selected: usize,
    width: usize,
    ansi: bool,
) {
    draw_row(
        output,
        "QuotaDeck settings / Dashboard providers",
        "",
        CYAN,
        false,
        width,
        ansi,
    );
    draw_blank(output, width, ansi);
    draw_row(
        output,
        "SHOW  PROVIDER",
        "COLOR   FIELDS",
        MUTED,
        false,
        width,
        ansi,
    );
    draw_row(
        output,
        "Sidebar colors: Pi = Codex; OpenCode = OpenCode Go",
        "plugin rows only",
        MUTED,
        false,
        width,
        ansi,
    );
    for (index, preference) in draft.providers.iter().enumerate() {
        let selected = selected == index;
        let previous = applied.get(preference.provider);
        let dirty = preference != previous;
        let label = marked_label(
            &format!(
                "[{}]   {}",
                if preference.show { "x" } else { " " },
                preference.provider.label()
            ),
            dirty,
        );
        let fields = format!(
            "{}   {}/{}",
            preference.color,
            preference.fields.len(),
            preference.provider.allowed_fields().len()
        );
        let (r, g, b) = crate::dashboard_prefs::color_rgb(&preference.color);
        draw_row(
            output,
            &label,
            &selection_value(&fields, selected),
            rgb(r, g, b),
            selected,
            width,
            ansi,
        );
    }
}

fn render_dashboard_fields(
    output: &mut String,
    draft: &DashboardPreferences,
    applied: &DashboardPreferences,
    provider: DashboardProvider,
    selected: usize,
    width: usize,
    ansi: bool,
) {
    draw_row(
        output,
        &format!("QuotaDeck settings / {} fields", provider.label()),
        "",
        CYAN,
        false,
        width,
        ansi,
    );
    draw_blank(output, width, ansi);
    let current = draft.get(provider);
    let previous = applied.get(provider);
    for (index, field) in provider.allowed_fields().iter().copied().enumerate() {
        let selected = selected == index;
        let enabled = current.has(field);
        let label = marked_label(field.label(), enabled != previous.has(field));
        draw_row(
            output,
            &label,
            &selection_value(if enabled { "on" } else { "off" }, selected),
            TEXT,
            selected,
            width,
            ansi,
        );
    }
}

fn render_status_and_footer(
    output: &mut String,
    status: Option<&str>,
    footer: &str,
    width: usize,
    ansi: bool,
) {
    if let Some(status) = status {
        draw_row(output, status, "", MUTED, false, width, ansi);
    } else {
        draw_blank(output, width, ansi);
    }
    draw_row(output, footer, "", MUTED, false, width, ansi);
}

fn selection_value(value: &str, selected: bool) -> String {
    if selected {
        format!("‹ {value} ›")
    } else {
        value.to_string()
    }
}

fn field_summary(settings: Settings) -> String {
    let count = SidebarField::ALL
        .into_iter()
        .filter(|field| settings.fields.contains(*field))
        .count();
    match count {
        0 => "none".to_string(),
        count if count == SidebarField::ALL.len() => "all".to_string(),
        count => format!("{count}/{}", SidebarField::ALL.len()),
    }
}

fn agent_group_state(agents: &[bool]) -> &'static str {
    match agents.iter().filter(|enabled| **enabled).count() {
        0 => "off",
        count if count == agents.len() => "on",
        _ => "mixed",
    }
}

fn marked_label(label: &str, dirty: bool) -> String {
    format!("{label}{}", if dirty { " *" } else { "" })
}

fn push_outer_blank(output: &mut String, width: usize, ansi: bool) {
    paint(output, &" ".repeat(width), TEXT, OUTER_BG, ansi);
    finish_line(output, ansi);
}

fn draw_blank(output: &mut String, width: usize, ansi: bool) {
    draw_row(output, "", "", TEXT, false, width, ansi);
}

fn draw_row(
    output: &mut String,
    label: &str,
    value: &str,
    label_color: Color,
    selected: bool,
    width: usize,
    ansi: bool,
) {
    let margin = usize::from(width >= 24) * 2;
    let panel_width = width.saturating_sub(margin * 2);
    let padding = usize::from(panel_width >= 8) * 2;
    let content_width = panel_width.saturating_sub(padding * 2);
    let background = if selected { SELECTED_BG } else { PANEL_BG };

    let right: String = value.chars().take(content_width).collect();
    let right_width = right.chars().count();
    let left_limit = content_width.saturating_sub(right_width + usize::from(right_width > 0));
    let left: String = label.chars().take(left_limit).collect();
    let left_width = left.chars().count();
    let gap = content_width.saturating_sub(left_width + right_width);

    paint(output, &" ".repeat(margin), TEXT, OUTER_BG, ansi);
    paint(output, &" ".repeat(padding), TEXT, background, ansi);
    paint(output, &left, label_color, background, ansi);
    paint(output, &" ".repeat(gap), TEXT, background, ansi);
    paint(output, &right, CYAN, background, ansi);
    paint(output, &" ".repeat(padding), TEXT, background, ansi);
    paint(output, &" ".repeat(margin), TEXT, OUTER_BG, ansi);
    finish_line(output, ansi);
}

fn paint(output: &mut String, text: &str, foreground: Color, background: Color, ansi: bool) {
    if ansi {
        push_ansi(output, SetBackgroundColor(background));
        push_ansi(output, SetForegroundColor(foreground));
        output.push_str(text);
    } else {
        output.push_str(text);
    }
}

fn finish_line(output: &mut String, ansi: bool) {
    if ansi {
        push_ansi(output, ResetColor);
    }
    output.push_str("\r\n");
}

fn repaint_prefix() -> String {
    let mut output = String::new();
    push_ansi(&mut output, SetBackgroundColor(OUTER_BG));
    push_ansi(
        &mut output,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
    );
    push_ansi(&mut output, crossterm::cursor::MoveTo(0, 0));
    output
}

fn push_ansi(output: &mut String, command: impl crossterm::Command) {
    command
        .write_ansi(output)
        .expect("writing ANSI to a String cannot fail");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dashboard() -> DashboardPreferences {
        DashboardPreferences::default()
    }

    fn settings() -> Settings {
        Settings {
            percent: PercentStyle::Remaining,
            layout: SidebarLayout::Packed,
            gap: SidebarRowGap::SEPARATED,
            interval_seconds: 60,
            brand: BrandColors::On,
            glyphs: GlyphSet::IconFont,
            order: AgentOrder::Default,
            alert: LowQuotaAlert::OFF,
            fields: FieldSet::default(),
            agents: [true; AgentSelection::SUPPORTED.len()],
        }
    }

    #[test]
    fn every_offered_interval_is_one_configure_accepts() {
        for seconds in INTERVALS {
            assert!(
                CacheStore::validate_watch_interval_seconds(seconds).is_ok(),
                "{seconds}s is outside the range configure accepts"
            );
        }
    }

    #[test]
    fn a_pending_configuration_remains_retryable_after_reopening_settings() {
        let directory = tempfile::tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let current = settings();
        let dashboard = dashboard();
        assert!(!apply_needed(
            &current, &current, &dashboard, &dashboard, &cache
        ));
        cache.set_settings_apply_pending().unwrap();
        assert!(apply_needed(
            &current, &current, &dashboard, &dashboard, &cache
        ));
        cache.clear_settings_apply_pending().unwrap();
        assert!(!apply_needed(
            &current, &current, &dashboard, &dashboard, &cache
        ));
    }

    #[test]
    fn two_way_options_flip_whichever_arrow_is_pressed() {
        let mut draft = settings();
        draft.cycle(Row::Choice(Choice::Percent), -1);
        assert_eq!(draft.percent, PercentStyle::Used);
        draft.cycle(Row::Choice(Choice::Percent), 1);
        assert_eq!(draft.percent, PercentStyle::Remaining);

        draft.cycle(Row::Choice(Choice::Layout), 1);
        assert_eq!(draft.layout, SidebarLayout::Stacked);
        draft.cycle(Row::Choice(Choice::RowGap), 1);
        assert_eq!(draft.gap, SidebarRowGap::FLUSH);
        draft.cycle(Row::Choice(Choice::Brand), 1);
        assert_eq!(draft.brand, BrandColors::Off);

        draft.cycle(Row::Choice(Choice::Glyphs), 1);
        assert_eq!(draft.glyphs, GlyphSet::Unicode);
        draft.cycle(Row::Choice(Choice::Glyphs), -1);
        assert_eq!(draft.glyphs, GlyphSet::IconFont);
    }

    #[test]
    fn the_interval_wraps_at_both_ends_of_the_offered_list() {
        let mut draft = settings();
        draft.cycle(Row::Choice(Choice::Interval), -1);
        assert_eq!(draft.interval_seconds, INTERVALS[0]);
        draft.cycle(Row::Choice(Choice::Interval), -1);
        assert_eq!(draft.interval_seconds, INTERVALS[INTERVALS.len() - 1]);
        draft.cycle(Row::Choice(Choice::Interval), 1);
        assert_eq!(draft.interval_seconds, INTERVALS[0]);
    }

    /// An unknown stored interval (`configure` accepts any value in range)
    /// must not trap the list: the first press lands on a known entry.
    #[test]
    fn an_interval_outside_the_offered_list_still_moves() {
        let mut draft = settings();
        draft.interval_seconds = 45;
        draft.cycle(Row::Choice(Choice::Interval), 1);
        assert_eq!(draft.interval_seconds, INTERVALS[2]);
    }

    #[test]
    fn toggling_a_field_and_an_agent_changes_only_that_one() {
        let mut draft = settings();
        draft.cycle(Row::Field(SidebarField::Cache), 1);
        assert!(!draft.fields.contains(SidebarField::Cache));
        assert!(draft.fields.contains(SidebarField::Ttl));

        draft.cycle(Row::Agent(Harness::Grok), 1);
        assert!(!draft.has_agent(Harness::Grok));
        assert!(draft.has_agent(Harness::Claude));
    }

    /// Applying names every value, so it cannot inherit a stale preference,
    /// and it names the agents so a narrowed selection is what gets installed.
    #[test]
    fn applying_names_every_value_including_the_agent_selection() {
        let mut draft = settings();
        draft.cycle(Row::Choice(Choice::Percent), 1);
        draft.cycle(Row::Field(SidebarField::Topic), 1);
        draft.cycle(Row::Agent(Harness::Pi), 1);
        assert_eq!(
            draft.apply_arguments(),
            vec![
                "configure",
                "--apply",
                "--agent",
                "claude,codex,grok,agy,opencode,omp,hermes",
                "--quota-percent",
                "used",
                "--sidebar-layout",
                "packed",
                "--row-gap",
                "1",
                "--brand-colors",
                "on",
                "--brand-glyphs",
                "icon",
                "--fields",
                "topic,model,cache,ttl,context,5h,7d",
                "--agent-order",
                "default",
                "--low-quota-alert",
                "off",
                "--watch-interval-seconds",
                "60",
            ]
        );
    }

    /// Turning every field off is a real choice, and `configure` accepts the
    /// word for it — an empty `--fields` value would read as "not set".
    #[test]
    fn an_empty_field_selection_is_passed_as_none() {
        let mut draft = settings();
        draft.fields = FieldSet::all();
        for field in SidebarField::ALL {
            draft.cycle(Row::Field(field), 1);
        }
        assert!(draft.apply_arguments().contains(&"none".to_string()));
    }

    /// Removing an agent gives its statusLine back before the rest are
    /// re-applied, and it takes a second keypress to get there.
    #[test]
    fn removing_an_agent_needs_confirmation_and_uninstalls_that_agent() {
        let applied = settings();
        let mut draft = applied;
        draft.cycle(Row::Agent(Harness::Claude), 1);
        assert_eq!(draft.removed_agents(applied), vec![Harness::Claude]);
        assert_eq!(
            Settings::uninstall_arguments(&draft.removed_agents(applied)),
            vec!["configure", "--uninstall", "--agent", "claude"]
        );

        let mut current = applied;
        let directory = tempfile::tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let dashboard = dashboard();
        let mut applied_dashboard = dashboard.clone();
        let (message, confirming) = attempt_apply(
            draft,
            &mut current,
            &dashboard,
            &mut applied_dashboard,
            &cache,
            false,
        );
        assert!(message.contains("Press a again"), "{message}");
        assert!(confirming);
        // Nothing was applied while the question was open.
        assert_eq!(current, applied);
    }

    /// An empty agent list would uninstall everything through a path meant for
    /// narrowing, so the pane refuses it before `configure` ever runs.
    #[test]
    fn applying_with_no_agent_selected_is_refused() {
        let applied = settings();
        let mut draft = applied;
        for harness in AgentSelection::SUPPORTED {
            draft.cycle(Row::Agent(harness), 1);
        }
        let mut current = applied;
        let directory = tempfile::tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let dashboard = dashboard();
        let mut applied_dashboard = dashboard.clone();
        let (message, confirming) = attempt_apply(
            draft,
            &mut current,
            &dashboard,
            &mut applied_dashboard,
            &cache,
            true,
        );
        assert_eq!(message, "Keep at least one agent.");
        assert!(!confirming);
        assert_eq!(current, applied);
    }

    #[test]
    fn compact_pages_wrap_without_exposing_individual_fields_or_agents() {
        let dashboard = dashboard();
        let main = rows(Page::Main, &dashboard);
        assert_eq!(main.len(), 14);
        assert!(matches!(main[0], Row::Choice(Choice::Percent)));
        assert!(matches!(main[1], Row::Display(DisplayChoice::Bars)));
        assert!(matches!(main[11], Row::Fields));
        assert!(matches!(main[12], Row::DashboardProviders));
        assert!(matches!(main[13], Row::Agents));
        assert_eq!(step_selection(&main, 0, -1), 13);
        assert_eq!(selected_line(Page::Main, 13), 19);
        assert_eq!(selected_line(Page::Main, 12), 16);
        assert_eq!(
            rows(Page::Fields, &dashboard).len(),
            SidebarField::ALL.len()
        );
        assert_eq!(
            rows(Page::Agents, &dashboard).len(),
            AgentSelection::SUPPORTED.len()
        );
    }

    #[test]
    fn main_frame_matches_the_designed_compact_settings_card() {
        let applied = settings();
        let layout_row = rows(Page::Main, &dashboard())
            .iter()
            .position(|row| matches!(row, Row::Choice(Choice::Layout)))
            .unwrap();
        let clean = render(
            &applied,
            applied,
            (&dashboard(), &dashboard()),
            (Page::Main, layout_row),
            TARGET_WIDTH,
            None,
            false,
        );
        assert!(clean.contains("QuotaDeck settings"), "{clean}");
        assert!(!clean.contains("QuotaDeck settings *"), "{clean}");
        let mut draft = applied;
        draft.cycle(Row::Choice(Choice::Layout), 1);
        let frame = render(
            &draft,
            applied,
            (&dashboard(), &dashboard()),
            (Page::Main, layout_row),
            TARGET_WIDTH,
            Some("Nothing to apply."),
            false,
        );
        assert!(frame.contains("QuotaDeck settings *"), "{frame}");
        assert!(frame.contains("Sidebar layout *"), "{frame}");
        assert!(frame.contains("‹ stacked ›"), "{frame}");
        assert!(frame.contains("Dashboard bars"), "{frame}");
        assert!(frame.contains("Sidebar meter"), "{frame}");
        assert!(frame.contains("countdown"), "{frame}");
        assert!(frame.contains("Brand glyphs"), "{frame}");
        assert!(
            frame.contains("claude codex grok agy opencode pi omp"),
            "{frame}"
        );
        assert!(frame.contains("hermes"), "{frame}");
        assert!(!frame.contains('•'), "{frame}");
        assert!(frame.contains(FOOTER), "{frame}");
        assert!(
            !frame.contains('\u{1b}'),
            "plain output leaked ANSI: {frame:?}"
        );
        assert!(
            !frame.contains('>'),
            "the pane draws no text cursor: {frame}"
        );

        let lines: Vec<&str> = frame.trim_end_matches("\r\n").split("\r\n").collect();
        assert!(
            lines.len() <= TARGET_HEIGHT as usize,
            "{} lines:\n{frame}",
            lines.len()
        );
        for line in lines {
            assert!(!line.contains('\n'), "{frame}");
            assert_eq!(line.chars().count(), TARGET_WIDTH as usize, "{line:?}");
        }
    }

    #[test]
    fn display_rows_change_only_the_dashboard_draft_and_mark_it_dirty() {
        let applied = settings();
        let mut draft = applied;
        let dashboard_applied = dashboard();
        let mut dashboard_draft = dashboard_applied.clone();
        let mut page = Page::Main;
        let mut selected = 0;
        for (choice, step) in [
            (DisplayChoice::Bars, 1),
            (DisplayChoice::ResetClock, -1),
            (DisplayChoice::SidebarMeter, 1),
        ] {
            change(
                &mut page,
                &mut selected,
                &mut draft,
                &mut dashboard_draft,
                Row::Display(choice),
                step,
            );
        }
        assert_eq!(draft, applied, "display rows never touch configure state");
        assert!(!dashboard_draft.display.bars);
        assert!(dashboard_draft.display.reset_clock);
        assert!(dashboard_draft.display.sidebar_meter);
        assert_eq!(page, Page::Main);
        let frame = render(
            &draft,
            applied,
            (&dashboard_draft, &dashboard_applied),
            (Page::Main, 2),
            TARGET_WIDTH,
            None,
            false,
        );
        assert!(frame.contains("QuotaDeck settings *"), "{frame}");
        assert!(frame.contains("Dashboard bars *"), "{frame}");
        assert!(frame.contains("Reset times *"), "{frame}");
        assert!(frame.contains("‹ clock ›"), "{frame}");
        assert!(frame.contains("Sidebar meter *"), "{frame}");
        assert!(!frame.contains("Dashboard providers *"), "{frame}");
    }

    #[test]
    fn fields_and_agents_stay_independently_editable_in_compact_drill_ins() {
        let applied = settings();
        let mut draft = applied;
        draft.cycle(Row::Field(SidebarField::Cache), 1);
        draft.cycle(Row::Agent(Harness::Grok), 1);

        let fields = render(
            &draft,
            applied,
            (&dashboard(), &dashboard()),
            (Page::Fields, 2),
            TARGET_WIDTH,
            None,
            false,
        );
        assert!(fields.contains("QuotaDeck settings / Fields"), "{fields}");
        assert!(fields.contains("cache *"), "{fields}");
        assert!(fields.contains("‹ off ›"), "{fields}");

        let agents = render(
            &draft,
            applied,
            (&dashboard(), &dashboard()),
            (Page::Agents, 2),
            TARGET_WIDTH,
            None,
            false,
        );
        assert!(agents.contains("QuotaDeck settings / Agents"), "{agents}");
        assert!(agents.contains("grok *"), "{agents}");
        assert!(agents.contains("‹ off ›"), "{agents}");
    }

    #[test]
    fn dashboard_provider_page_exposes_only_valid_controls_and_keeps_selection_visible() {
        let applied = dashboard();
        assert!(!applied.get(DashboardProvider::OpenCodeGo).show);
        let mut draft = applied.clone();
        let mut page = Page::DashboardProviders;
        let mut selected = 0;
        let mut global = settings();
        change(
            &mut page,
            &mut selected,
            &mut global,
            &mut draft,
            Row::DashboardProvider(DashboardProvider::Claude),
            1,
        );
        assert!(!draft.get(DashboardProvider::Claude).show);
        change(
            &mut page,
            &mut selected,
            &mut global,
            &mut draft,
            Row::DashboardProvider(DashboardProvider::Claude),
            2,
        );
        assert_eq!(page, Page::DashboardFields(DashboardProvider::Claude));

        let provider_page = render(
            &global,
            global,
            (&draft, &applied),
            (Page::DashboardProviders, 0),
            100,
            None,
            false,
        );
        assert!(
            provider_page.contains("QuotaDeck settings / Dashboard providers"),
            "{provider_page}"
        );
        assert!(provider_page.contains("SHOW  PROVIDER"), "{provider_page}");
        assert!(provider_page.contains("COLOR   FIELDS"), "{provider_page}");
        assert!(
            provider_page.contains("Sidebar colors: Pi = Codex; OpenCode = OpenCode Go"),
            "{provider_page}"
        );
        assert!(
            provider_page.contains("plugin rows only"),
            "{provider_page}"
        );
        assert!(
            provider_page.contains("[ ]   OpenCode Go"),
            "{provider_page}"
        );
        assert!(provider_page.contains("u/d order"), "{provider_page}");
        assert!(provider_page.contains("c color"), "{provider_page}");

        let hermes = render(
            &global,
            global,
            (&draft, &applied),
            (Page::DashboardFields(DashboardProvider::Hermes), 0),
            TARGET_WIDTH,
            None,
            false,
        );
        assert!(hermes.contains("plan amount"), "{hermes}");
        assert!(hermes.contains("top-up percentage"), "{hermes}");
        assert!(!hermes.contains("top-up reset"), "{hermes}");
        let clipped = fit_height(provider_page, 6, selected_line(Page::DashboardProviders, 8));
        assert!(clipped.contains("OpenCode Go"), "{clipped}");
        assert!(clipped.contains(PROVIDER_FOOTER), "{clipped}");
    }

    #[test]
    fn interactive_frame_uses_opaque_panel_and_full_row_selection_colors() {
        let frame = render(
            &settings(),
            settings(),
            (&dashboard(), &dashboard()),
            (Page::Main, initial_selection()),
            TARGET_WIDTH,
            None,
            true,
        );
        assert!(frame.contains("\u{1b}[48;2;14;16;20m"), "{frame:?}");
        assert!(frame.contains("\u{1b}[48;2;27;30;37m"), "{frame:?}");
        assert!(frame.contains("\u{1b}[48;2;50;56;68m"), "{frame:?}");
        assert_eq!(
            repaint_prefix(),
            "\u{1b}[48;2;14;16;20m\u{1b}[2J\u{1b}[1;1H"
        );
        assert!(frame.contains("‹ icon ›"), "{frame:?}");
    }

    #[test]
    fn a_clamped_pane_keeps_the_footer_without_scrolling() {
        let frame = fit_height(
            render(
                &settings(),
                settings(),
                (&dashboard(), &dashboard()),
                (Page::Main, initial_selection()),
                TARGET_WIDTH,
                None,
                true,
            ),
            5,
            selected_line(Page::Main, initial_selection()),
        );
        assert_eq!(frame.matches("\r\n").count(), 4, "{frame:?}");
        assert!(frame.contains(FOOTER), "{frame:?}");
    }

    #[test]
    fn the_default_interval_matches_the_mockup_and_longer_values_stay_compact() {
        assert_eq!(format_interval(30), "30s");
        assert_eq!(format_interval(60), "60s");
        assert_eq!(format_interval(3_600), "60m");
    }
}
