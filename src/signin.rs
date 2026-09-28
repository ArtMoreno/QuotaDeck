//! Which providers are signed in, judged from this machine, and what to do
//! about the ones that are not.
//!
//! "Signed in before" is read from the same credential files the collectors
//! use (plus Claude Code's macOS Keychain entry), never from the network, so
//! the dashboard can ask on every frame. Only a refresh can tell a stored
//! login from an expired one; that answer arrives as the cached `login`
//! problem code, which [`classify`] ranks above everything local.
//!
//! `quotadeck setup` is the guided form: one forced refresh, a checklist, and
//! an offer to run each missing sign-in command in place.

use crate::cache::CacheStore;
use crate::dashboard_prefs::{DashboardPreferences, DashboardProvider};
use anyhow::{Context, Result};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Quota is showing.
    Ready,
    /// A stored login is on disk but no quota has arrived yet.
    SignedIn,
    /// The provider rejected the stored login.
    Expired,
    /// The agent is on this machine but has never been signed in.
    SignedOut,
    /// Nothing of the agent is on this machine.
    NotInstalled,
}

/// What the local probes found for one provider.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// Where a stored login was found, as the user would recognise it.
    pub credential: Option<String>,
    /// The agent's CLI or its home directory exists.
    pub installed: bool,
}

pub fn classify(evidence: &Evidence, has_data: bool, problem: Option<&str>) -> Status {
    if problem == Some("login") {
        return Status::Expired;
    }
    if has_data {
        return Status::Ready;
    }
    if evidence.credential.is_some() {
        return Status::SignedIn;
    }
    if evidence.installed {
        Status::SignedOut
    } else {
        Status::NotInstalled
    }
}

/// The command that signs this provider in, when there is one to run.
pub fn login_command(provider: DashboardProvider) -> Option<&'static [&'static str]> {
    Some(match provider {
        DashboardProvider::Claude => &["claude", "auth", "login"],
        DashboardProvider::Codex => &["codex", "login"],
        DashboardProvider::Grok => &["grok", "login"],
        DashboardProvider::Hermes => &["hermes", "portal", "login"],
        DashboardProvider::OpenCodeGo => &["opencode", "auth", "login"],
        DashboardProvider::Agy
        | DashboardProvider::OpenCode
        | DashboardProvider::OpenRouter
        | DashboardProvider::Omp => return None,
    })
}

fn login_text(provider: DashboardProvider) -> Option<String> {
    login_command(provider).map(|command| command.join(" "))
}

/// One short phrase for a dashboard row that has no quota to show, or `None`
/// when the row's own message is already the right one (a network failure,
/// a missing CLI, a stale value).
pub fn hint(provider: DashboardProvider, status: Status, problem: Option<&str>) -> Option<String> {
    use DashboardProvider as P;
    // The last refresh already knows the cause, and it is not the login.
    if matches!(problem, Some("failed" | "cli")) {
        return None;
    }
    match status {
        Status::Ready => None,
        Status::Expired => Some(match provider {
            P::OpenRouter => "key rejected · run quotadeck setup".to_string(),
            _ => match login_text(provider) {
                Some(login) => format!("sign-in expired · run {login}"),
                None => "sign-in expired · run quotadeck setup".to_string(),
            },
        }),
        Status::SignedIn => Some(
            match provider {
                // The usage API needs a live token in the credentials file;
                // a reply always works, through the statusLine.
                P::Claude => "signed in · send one message in claude",
                P::Agy => "signed in · send one message in agy",
                P::Omp => "signed in · use omp in a Herdr pane",
                P::OpenCode => "no turns in the last 30 days",
                _ => "signed in · press r to fetch",
            }
            .to_string(),
        ),
        Status::SignedOut => Some(match provider {
            P::Agy => "no data yet · send one message in agy".to_string(),
            P::Omp => "no data yet · use omp in a Herdr pane".to_string(),
            P::OpenCode => "not used yet · run opencode".to_string(),
            P::OpenRouter => "no API key · run quotadeck setup".to_string(),
            P::OpenCodeGo => "no Go key · run opencode auth login".to_string(),
            _ => format!(
                "not signed in · run {}",
                login_text(provider).unwrap_or_else(|| "quotadeck setup".to_string())
            ),
        }),
        Status::NotInstalled => Some("not installed · hide it in settings (s)".to_string()),
    }
}

/// The dashboard's hint for a row with nothing to draw.
pub fn row_hint(provider: DashboardProvider, problem: Option<&str>) -> Option<String> {
    let evidence = probe(provider)?;
    hint(provider, classify(&evidence, false, problem), problem)
}

#[cfg(not(test))]
fn probe(provider: DashboardProvider) -> Option<Evidence> {
    Some(evidence(provider))
}

// Tests render the dashboard on the machine running them; the real probes
// would read that machine's logins. Off unless a test sets it, per thread.
#[cfg(test)]
thread_local! {
    static TEST_EVIDENCE: std::cell::RefCell<Option<Evidence>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn probe(_provider: DashboardProvider) -> Option<Evidence> {
    TEST_EVIDENCE.with(|evidence| evidence.borrow().clone())
}

#[cfg(test)]
pub(crate) fn set_test_evidence(evidence: Option<Evidence>) {
    TEST_EVIDENCE.with(|slot| *slot.borrow_mut() = evidence);
}

/// The full instruction `quotadeck setup` prints under a provider that is not
/// ready.
fn steps(provider: DashboardProvider, status: Status) -> Vec<String> {
    use DashboardProvider as P;
    if status == Status::SignedIn {
        return vec![match provider {
            P::Claude => {
                "Signed in. Send one message in Claude Code; its quota appears with the reply."
                    .into()
            }
            P::Agy => "Signed in. Send one message in agy and its quota appears.".into(),
            P::Omp => "Signed in. Use omp in a Herdr pane; QuotaDeck reads `omp usage`.".into(),
            P::OpenCode => {
                "Use OpenCode once; this row shows your last 30 days of local usage.".into()
            }
            _ => "Signed in, but the last check did not return quota. Press r in the dashboard to retry.".into(),
        }];
    }
    match provider {
        P::Agy => {
            vec!["Open `agy`, sign in, and send one message; its statusLine reports quota.".into()]
        }
        P::OpenCode => {
            vec!["Use OpenCode once; this row shows your last 30 days of local usage.".into()]
        }
        P::Omp => {
            vec!["Use omp in a Herdr pane; QuotaDeck reads `omp usage` for that account.".into()]
        }
        P::OpenRouter => {
            let file = openrouter_key_file()
                .map(|path| display_path(&path))
                .unwrap_or_else(|| "~/.config/openrouter/key".to_string());
            vec![
                "Create a key at https://openrouter.ai/settings/keys, then either".into(),
                format!("paste it when `quotadeck setup` asks, save it in {file},"),
                "or export OPENROUTER_API_KEY where Herdr starts.".into(),
            ]
        }
        P::OpenCodeGo => vec![
            "Run `opencode auth login` and choose OpenCode Go, or export OPENCODE_API_KEY.".into(),
        ],
        _ => {
            let login = login_text(provider).unwrap_or_default();
            vec![if status == Status::Expired {
                format!("The stored login was rejected. Run `{login}` to sign in again.")
            } else {
                format!("Run `{login}`.")
            }]
        }
    }
}

/* ----------------------------------------------------------- probes */

/// Look for this provider's login and installation on disk. Cheap enough for
/// every dashboard frame; the one process it can start (the Keychain lookup)
/// runs at most once per process.
pub fn evidence(provider: DashboardProvider) -> Evidence {
    use DashboardProvider as P;
    let home = crate::platform::home_dir_opt();
    let in_home = |relative: &str| {
        home.as_ref()
            .is_some_and(|home| home.join(relative).is_dir())
    };
    match provider {
        P::Claude => Evidence {
            credential: claude_login(),
            installed: on_path("claude") || in_home(".claude"),
        },
        P::Codex => {
            let path = crate::providers::codex::auth_path().ok();
            Evidence {
                credential: path
                    .as_deref()
                    .filter(|path| path.is_file())
                    .map(display_path),
                installed: on_path("codex") || parent_is_dir(path.as_deref()),
            }
        }
        P::Grok => {
            let path = crate::providers::grok::auth_path().ok();
            Evidence {
                credential: path
                    .as_deref()
                    .filter(|path| crate::providers::grok::read_credentials(path).is_ok())
                    .map(display_path),
                installed: on_path("grok") || parent_is_dir(path.as_deref()),
            }
        }
        P::Hermes => {
            let path = crate::providers::hermes::auth_path().ok();
            Evidence {
                credential: path
                    .as_deref()
                    .filter(|path| crate::providers::hermes::read_credentials(path).is_ok())
                    .map(display_path),
                installed: on_path("hermes")
                    || crate::providers::hermes::hermes_home().is_ok_and(|home| home.is_dir()),
            }
        }
        P::OpenRouter => Evidence {
            credential: crate::providers::openrouter::api_key()
                .ok()
                .map(|_| "API key".to_string()),
            // An account, not an agent: there is nothing to install.
            installed: true,
        },
        P::Agy => Evidence {
            credential: None,
            installed: on_path("agy") || in_home(".gemini/antigravity-cli"),
        },
        P::OpenCode => {
            let paths = crate::opencode::OpenCodePaths::from_env();
            Evidence {
                credential: paths
                    .as_ref()
                    .filter(|paths| paths.db.is_file())
                    .map(|_| "local history".to_string()),
                installed: on_path("opencode")
                    || parent_is_dir(paths.as_ref().map(|paths| paths.db.as_path())),
            }
        }
        P::OpenCodeGo => {
            let paths = crate::opencode::OpenCodePaths::from_env();
            let credential = if crate::opencode::env_go_key_present() {
                Some("OPENCODE_API_KEY".to_string())
            } else {
                paths
                    .as_ref()
                    .and_then(crate::opencode::go_key)
                    .map(|_| "OpenCode auth".to_string())
            };
            Evidence {
                credential,
                installed: on_path("opencode")
                    || parent_is_dir(paths.as_ref().map(|paths| paths.db.as_path())),
            }
        }
        P::Omp => Evidence {
            credential: None,
            installed: std::env::var_os("HERDR_AGENT_QUOTA_OMP_BIN").is_some()
                || on_path("omp")
                || in_home(".omp"),
        },
    }
}

fn parent_is_dir(path: Option<&Path>) -> bool {
    path.and_then(Path::parent).is_some_and(Path::is_dir)
}

/// Where Claude Code's login lives. On macOS the file can be a leftover copy
/// whose token expired long ago while the live login sits in the Keychain,
/// so an expired file only counts when there is nothing better.
fn claude_login() -> Option<String> {
    let path = crate::providers::claude::credentials_path().ok()?;
    let file = crate::providers::claude::access_token(&path).is_ok();
    if file && !claude_file_expired(&path, CacheStore::now_unix()) {
        return Some(display_path(&path));
    }
    if claude_keychain() {
        return Some("macOS Keychain".to_string());
    }
    file.then(|| display_path(&path))
}

fn claude_file_expired(path: &Path, now_unix: u64) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.pointer("/claudeAiOauth/expiresAt")?.as_u64())
        .is_some_and(|expires_ms| expires_ms / 1_000 <= now_unix)
}

/// Claude Code on macOS keeps its login in the Keychain rather than a file.
/// Without `-w` the lookup reads only the item's attributes, so it never
/// prompts and never sees the secret.
fn claude_keychain() -> bool {
    #[cfg(target_os = "macos")]
    {
        static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FOUND.get_or_init(|| {
            std::process::Command::new("/usr/bin/security")
                .args(["find-generic-password", "-s", "Claude Code-credentials"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Is `name` an executable on `PATH` or in a directory agent installers
/// commonly use? Herdr's server can start with a shorter `PATH` than the
/// user's shell, so the common install locations are checked as well.
fn on_path(name: &str) -> bool {
    let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(home) = crate::platform::home_dir_opt() {
        for relative in [
            ".local/bin",
            ".opencode/bin",
            ".bun/bin",
            ".npm-global/bin",
            ".cargo/bin",
        ] {
            directories.push(home.join(relative));
        }
    }
    if !cfg!(windows) {
        directories.push("/opt/homebrew/bin".into());
        directories.push("/usr/local/bin".into());
    }
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd"]
            .iter()
            .map(|extension| format!("{name}.{extension}"))
            .collect()
    } else {
        vec![name.to_string()]
    };
    directories
        .iter()
        .filter(|directory| !directory.as_os_str().is_empty())
        .any(|directory| names.iter().any(|name| directory.join(name).is_file()))
}

fn display_path(path: &Path) -> String {
    if let Some(home) = crate::platform::home_dir_opt() {
        if let Ok(relative) = path.strip_prefix(&home) {
            return format!("~/{}", relative.display());
        }
    }
    path.display().to_string()
}

fn openrouter_key_file() -> Option<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(|directory| PathBuf::from(directory).join(crate::prefs::OPENROUTER_KEY))
}

/* ------------------------------------------------------------ report */

/// One provider's line in the checklist.
#[derive(Debug, Clone)]
pub struct Check {
    pub provider: DashboardProvider,
    pub status: Status,
    pub evidence: Evidence,
    /// The row's quota text when it has one, without the provider label.
    pub quota: Option<String>,
    pub shown: bool,
    /// The last refresh's public problem code, when it had one.
    pub problem: Option<&'static str>,
}

fn checks(
    cache: &CacheStore,
    preferences: &DashboardPreferences,
    opencode_usage: Option<crate::opencode::LocalUsage>,
) -> Result<Vec<Check>> {
    let mut everything = preferences.clone();
    for preference in &mut everything.providers {
        preference.show = true;
    }
    let quota = crate::dashboard::provider_quota_text(
        cache,
        &everything,
        opencode_usage,
        CacheStore::now_unix(),
    )?;
    Ok(everything
        .providers
        .iter()
        .map(|preference| {
            let provider = preference.provider;
            let evidence = evidence(provider);
            let text = quota
                .iter()
                .find(|(candidate, _)| *candidate == provider)
                .and_then(|(_, text)| text.clone());
            let problem = provider
                .quota_provider()
                .and_then(|quota| cache.refresh_problem_code(quota));
            Check {
                provider,
                status: classify(&evidence, text.is_some(), problem),
                evidence,
                quota: text,
                shown: preferences.get(provider).show,
                problem,
            }
        })
        .collect())
}

/// Render the checklist. Pure, so the wording is testable.
pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    let ready: Vec<&Check> = checks
        .iter()
        .filter(|check| matches!(check.status, Status::Ready))
        .collect();
    let needs_step = |check: &&Check| {
        matches!(
            check.status,
            Status::SignedIn | Status::Expired | Status::SignedOut
        )
    };
    // A provider the user already hid and never set up is a choice, not a
    // problem: it gets one quiet line instead of instructions.
    let attention: Vec<&Check> = checks
        .iter()
        .filter(needs_step)
        .filter(|check| check.shown)
        .collect();
    let hidden_unused: Vec<&Check> = checks
        .iter()
        .filter(|check| check.status == Status::SignedOut && !check.shown)
        .collect();
    // Hidden rows are never refreshed, so a login there has no quota yet;
    // that is not "not set up", it only needs the row turned on.
    let hidden_signed_in: Vec<&Check> = checks
        .iter()
        .filter(|check| matches!(check.status, Status::SignedIn | Status::Expired))
        .filter(|check| !check.shown)
        .collect();
    let missing: Vec<&Check> = checks
        .iter()
        .filter(|check| check.status == Status::NotInstalled)
        .collect();
    let width = checks
        .iter()
        .map(|check| check.provider.label().chars().count())
        .max()
        .unwrap_or(8)
        + 2;
    let hidden = |check: &Check| {
        if check.shown {
            ""
        } else {
            "  (hidden in dashboard)"
        }
    };

    if !ready.is_empty() {
        out.push_str("Showing quota\n");
        for check in &ready {
            let source = check
                .evidence
                .credential
                .as_deref()
                .map(|source| format!("  ({source})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  ✓ {:<width$}{}{source}{}\n",
                check.provider.label(),
                check.quota.as_deref().unwrap_or(""),
                hidden(check),
            ));
        }
        out.push('\n');
    }
    if !attention.is_empty() {
        out.push_str("Needs a step\n");
        for check in &attention {
            let failed = matches!(check.problem, Some("failed" | "cli"));
            let (mark, state) = match check.status {
                _ if check.problem == Some("cli") => {
                    ("!", "signed in, but its CLI was not found".to_string())
                }
                _ if failed => ("!", "last check failed (network or service)".to_string()),
                Status::SignedIn if check.provider == DashboardProvider::OpenCode => {
                    ("…", "no turns in the last 30 days".to_string())
                }
                Status::SignedIn => (
                    "…",
                    format!(
                        "signed in ({}), no quota yet",
                        check.evidence.credential.as_deref().unwrap_or("found")
                    ),
                ),
                Status::Expired => ("!", "sign-in expired".to_string()),
                _ => ("✗", "not signed in".to_string()),
            };
            out.push_str(&format!(
                "  {mark} {:<width$}{state}\n",
                check.provider.label()
            ));
            let steps = if failed {
                vec![
                    "Press r in the dashboard to retry, or run `quotadeck setup` again in a minute."
                        .to_string(),
                ]
            } else {
                steps(check.provider, check.status)
            };
            for step in steps {
                out.push_str(&format!("    {:<width$}→ {step}\n", ""));
            }
        }
        out.push('\n');
    }
    if !hidden_unused.is_empty() {
        let names: Vec<&str> = hidden_unused
            .iter()
            .map(|check| check.provider.label())
            .collect();
        out.push_str(&format!("Hidden and not set up: {}\n\n", names.join(", ")));
    }
    if !hidden_signed_in.is_empty() {
        let names: Vec<&str> = hidden_signed_in
            .iter()
            .map(|check| check.provider.label())
            .collect();
        out.push_str(&format!("Signed in but hidden: {}\n", names.join(", ")));
        out.push_str("  Show their rows: press s in the dashboard → Dashboard providers.\n\n");
    }
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|check| check.provider.label()).collect();
        out.push_str(&format!("Not installed: {}\n", names.join(", ")));
        if missing.iter().any(|check| check.shown) {
            out.push_str("  Hide their rows: press s in the dashboard → Dashboard providers.\n");
        }
        out.push('\n');
    }
    let shown_ready = ready.iter().filter(|check| check.shown).count();
    out.push_str(&format!(
        "{shown_ready} of {} providers showing quota.",
        shown_ready + attention.len()
    ));
    if !attention.is_empty() {
        out.push_str(" After a step, press r in the dashboard or run `quotadeck setup` again.");
    }
    out.push('\n');
    out
}

/// The checklist from what is already on disk, for `configure --apply`.
pub fn print_local_report() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let preferences = DashboardPreferences::load_or_default(&cache);
    let usage = local_opencode_usage();
    println!();
    println!("Providers found on this machine:");
    print!("{}", render(&checks(&cache, &preferences, usage)?));
    println!("Run `quotadeck setup` for a guided sign-in.");
    Ok(())
}

fn local_opencode_usage() -> Option<crate::opencode::LocalUsage> {
    crate::opencode::OpenCodePaths::from_env()
        .and_then(|paths| crate::opencode::local_usage_30d(&paths, CacheStore::now_unix()))
}

fn refreshed_checks(cache: &CacheStore) -> Result<Vec<Check>> {
    let preferences = DashboardPreferences::load_or_default(cache);
    let mut everything = preferences.clone();
    for preference in &mut everything.providers {
        preference.show = true;
    }
    // An open dashboard may be mid-refresh and holding the lock; its fetch
    // finishes in seconds, and skipping ours would report stale answers.
    let mut refreshed = None;
    for _ in 0..40 {
        refreshed = crate::refresh::refresh_dashboard(cache, &everything, true)?;
        if refreshed.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let usage = refreshed.flatten().or_else(local_opencode_usage);
    checks(cache, &preferences, usage)
}

/// `quotadeck setup`: check every provider, then walk through the ones that
/// are not signed in.
pub fn run(no_prompt: bool) -> Result<()> {
    let cache = CacheStore::from_env()?;
    println!("QuotaDeck setup — checking each provider…\n");
    let checks = refreshed_checks(&cache)?;
    print!("{}", render(&checks));

    let interactive =
        !no_prompt && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        return Ok(());
    }
    let mut acted = false;
    for check in checks.iter().filter(|check| check.shown) {
        match check.status {
            Status::SignedOut | Status::Expired => {}
            _ => continue,
        }
        if check.provider == DashboardProvider::OpenRouter {
            acted |= offer_openrouter_key()?;
            continue;
        }
        let Some(command) = login_command(check.provider) else {
            continue;
        };
        if !on_path(command[0]) {
            continue;
        }
        let question = format!(
            "\nSign in to {} now? This runs `{}`.",
            check.provider.label(),
            command.join(" ")
        );
        if !confirm(&question)? {
            continue;
        }
        let status = std::process::Command::new(command[0])
            .args(&command[1..])
            .status();
        match status {
            Ok(status) if status.success() => acted = true,
            Ok(_) => println!("`{}` did not finish; skipped.", command.join(" ")),
            Err(error) => println!("Could not start `{}`: {error}", command[0]),
        }
    }
    if acted {
        println!("\nChecking again…\n");
        print!("{}", render(&refreshed_checks(&cache)?));
    }
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [Y/n] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    ))
}

/// Ask for an OpenRouter key with the typed characters hidden, and store it
/// where the collector already looks. Returns whether a key was saved.
fn offer_openrouter_key() -> Result<bool> {
    let Some(path) = openrouter_key_file() else {
        return Ok(false);
    };
    print!("\nPaste an OpenRouter API key to save it (Enter to skip): ");
    std::io::stdout().flush()?;
    let key = read_hidden()?;
    println!();
    let key = key.trim();
    if key.is_empty() {
        return Ok(false);
    }
    save_secret(&path, key)?;
    println!("Saved to {}.", display_path(&path));
    Ok(true)
}

fn read_hidden() -> Result<String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    crossterm::terminal::enable_raw_mode()?;
    let mut value = String::new();
    let result = loop {
        match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Esc => {
                    value.clear();
                    break Ok(());
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    value.clear();
                    break Ok(());
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(character) => value.push(character),
                _ => {}
            },
            Ok(Event::Paste(text)) => value.push_str(&text),
            Ok(_) => {}
            Err(error) => break Err(error),
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    result?;
    Ok(value)
}

fn save_secret(path: &Path, value: &str) -> Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("create {}", directory.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(value.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(credential: Option<&str>, installed: bool) -> Evidence {
        Evidence {
            credential: credential.map(str::to_string),
            installed,
        }
    }

    #[test]
    fn a_rejected_login_outranks_everything_found_locally() {
        let evidence = found(Some("~/.grok/auth.json"), true);
        assert_eq!(classify(&evidence, true, Some("login")), Status::Expired);
        assert_eq!(classify(&evidence, true, None), Status::Ready);
        assert_eq!(classify(&evidence, false, None), Status::SignedIn);
        assert_eq!(classify(&found(None, true), false, None), Status::SignedOut);
        assert_eq!(
            classify(&found(None, false), false, Some("credentials")),
            Status::NotInstalled
        );
    }

    #[test]
    fn hints_name_the_command_that_fixes_the_row() {
        use DashboardProvider as P;
        assert_eq!(
            hint(P::Grok, Status::SignedOut, Some("credentials")).as_deref(),
            Some("not signed in · run grok login")
        );
        assert_eq!(
            hint(P::Hermes, Status::Expired, Some("login")).as_deref(),
            Some("sign-in expired · run hermes portal login")
        );
        assert_eq!(
            hint(P::OpenRouter, Status::SignedOut, Some("credentials")).as_deref(),
            Some("no API key · run quotadeck setup")
        );
        assert_eq!(
            hint(P::Claude, Status::SignedIn, None).as_deref(),
            Some("signed in · send one message in claude")
        );
        assert!(hint(P::Agy, Status::NotInstalled, None)
            .unwrap()
            .contains("settings"));
    }

    #[test]
    fn a_network_failure_or_missing_cli_keeps_its_own_message() {
        use DashboardProvider as P;
        assert_eq!(hint(P::Codex, Status::SignedIn, Some("failed")), None);
        assert_eq!(hint(P::Codex, Status::SignedIn, Some("cli")), None);
        assert_eq!(hint(P::Codex, Status::SignedOut, Some("failed")), None);
        assert_eq!(hint(P::Codex, Status::Ready, None), None);
    }

    #[test]
    fn every_hint_fits_beside_a_label_in_a_narrow_pane() {
        for provider in DashboardProvider::ALL {
            for status in [
                Status::SignedIn,
                Status::Expired,
                Status::SignedOut,
                Status::NotInstalled,
            ] {
                if let Some(hint) = hint(provider, status, None) {
                    assert!(
                        hint.chars().count() <= 42,
                        "{provider:?} {status:?}: {hint}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_checklist_groups_providers_and_says_where_each_login_was_found() {
        use DashboardProvider as P;
        let check =
            |provider, status, credential: Option<&str>, quota: Option<&str>, shown| Check {
                provider,
                status,
                evidence: found(credential, status != Status::NotInstalled),
                quota: quota.map(str::to_string),
                shown,
                problem: None,
            };
        let text = render(&[
            check(
                P::Codex,
                Status::Ready,
                Some("~/.codex/auth.json"),
                Some("7d 80%"),
                true,
            ),
            check(P::Grok, Status::SignedOut, None, None, true),
            check(
                P::Hermes,
                Status::Expired,
                Some("~/.hermes/auth.json"),
                None,
                true,
            ),
            check(P::Agy, Status::NotInstalled, None, None, true),
            check(P::Omp, Status::NotInstalled, None, None, false),
            check(P::OpenCodeGo, Status::SignedOut, None, None, false),
        ]);
        assert!(text.contains("✓ Codex"), "{text}");
        assert!(text.contains("7d 80%  (~/.codex/auth.json)"), "{text}");
        assert!(text.contains("✗ Grok"), "{text}");
        assert!(text.contains("→ Run `grok login`."), "{text}");
        assert!(text.contains("! Hermes"), "{text}");
        assert!(text.contains("`hermes portal login`"), "{text}");
        assert!(text.contains("Not installed: Agy, OMP"), "{text}");
        assert!(text.contains("Hide their rows"), "{text}");
        assert!(
            text.contains("Hidden and not set up: OpenCode Go"),
            "{text}"
        );
        assert!(!text.contains("opencode auth login"), "{text}");
        assert!(!text.contains("Signed in but hidden"), "{text}");
        assert!(text.contains("1 of 3 providers showing quota."), "{text}");

        let text = render(&[check(
            P::OpenCodeGo,
            Status::SignedIn,
            Some("OpenCode auth"),
            None,
            false,
        )]);
        assert!(text.contains("Signed in but hidden: OpenCode Go"), "{text}");
        assert!(text.contains("Show their rows: press s"), "{text}");
        assert!(!text.contains("not set up"), "{text}");

        let mut failed = check(
            P::Claude,
            Status::SignedIn,
            Some("macOS Keychain"),
            None,
            true,
        );
        failed.problem = Some("failed");
        let text = render(&[failed]);
        assert!(text.contains("last check failed"), "{text}");
        assert!(!text.contains("Send one message"), "{text}");
    }

    #[test]
    fn an_expired_claude_token_file_is_recognised() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"t","expiresAt":1000000}}"#,
        )
        .unwrap();
        assert!(claude_file_expired(&path, 1_001));
        assert!(!claude_file_expired(&path, 999));
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"t"}}"#).unwrap();
        assert!(
            !claude_file_expired(&path, 1_001),
            "no expiry is not expired"
        );
    }

    #[test]
    fn a_saved_key_is_readable_only_by_its_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/openrouter-key");
        save_secret(&path, "sk-or-test").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-or-test");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
