use crate::cache::CacheStore;
use crate::cli::{LowQuotaAlert, SidebarField};
use crate::dashboard_prefs::{DashboardPreferences, DashboardProvider};
use crate::herdr::{
    current_focused_pane, list_agent_panes, list_agent_state, plugin_quota_present,
    publish_pane_tokens, refresh_pane_topic, AgentPane, PaneQuotaUpdate, PaneTokens,
};
use crate::model::{
    BillingTarget, CredentialScope, Harness, Provider, ProviderSnapshot, Resolution,
};
use crate::omp::OmpEvidence;
use crate::opencode::{local_usage_30d, LocalUsage, OpenCodePaths};
use crate::presentation::MetadataTokens;
use crate::presentation::RowStyle;
use crate::providers::{claude, codex, grok, hermes, omp as omp_provider, opencode_go, openrouter};
use crate::route;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const MAX_ACTIVE_TURN_WATCH: Duration = Duration::from_secs(60 * 60);
const TURN_WATCH_LOCK: &str = "turn.lock";
const DASHBOARD_REFRESH_LOCK: &str = "dashboard.refresh.lock";
const STATUSLINE_STALE_AFTER_INTERVALS: u64 = 2;

#[derive(Debug, Serialize)]
pub struct ProviderOutcome {
    pub provider: Provider,
    pub available: bool,
    pub from_cache: bool,
    pub error: Option<String>,
}

pub fn run(providers: &[Provider], force: bool, json: bool) -> Result<()> {
    run_internal(providers, force, json, None)
}

/// Refresh only globally attributable providers shown by the dashboard.
///
/// Agy observations and OpenCode's local database are re-read by the renderer.
/// OMP remains pane/provider scoped and can never enter this list.
pub(crate) fn refresh_dashboard(
    cache: &CacheStore,
    preferences: &DashboardPreferences,
    force: bool,
) -> Result<Option<Option<LocalUsage>>> {
    let Some(_lock) = cache.try_lock_named(DASHBOARD_REFRESH_LOCK)? else {
        return Ok(None);
    };
    for provider in dashboard_refresh_providers(preferences) {
        let _ = refresh_provider(cache, provider, force, &[]);
    }
    if preferences.get(DashboardProvider::OpenCodeGo).show {
        refresh_opencode_go_dashboard(cache, force);
    }
    let local_usage = if preferences.get(DashboardProvider::OpenCode).show {
        OpenCodePaths::from_env().and_then(|paths| local_usage_30d(&paths, CacheStore::now_unix()))
    } else {
        None
    };
    Ok(Some(local_usage))
}

fn dashboard_refresh_providers(preferences: &DashboardPreferences) -> Vec<Provider> {
    preferences
        .providers
        .iter()
        .filter(|preference| preference.show)
        .filter_map(|preference| match preference.provider {
            DashboardProvider::Claude => Some(Provider::Claude),
            DashboardProvider::Codex => Some(Provider::Codex),
            DashboardProvider::Grok => Some(Provider::Grok),
            DashboardProvider::Hermes => Some(Provider::Hermes),
            DashboardProvider::OpenRouter => Some(Provider::OpenRouter),
            DashboardProvider::Agy
            | DashboardProvider::OpenCode
            | DashboardProvider::Omp
            | DashboardProvider::OpenCodeGo => None,
        })
        .collect()
}

/// Restore the Herdr state this plugin owns, then refresh once.
///
/// Only the agent order is restored, and only when it is this plugin's to
/// restore: a `default` order owns no Herdr view, so startup has nothing to
/// put back and must not spend a socket call saying so.
pub fn startup(providers: &[Provider]) -> Result<()> {
    if let Ok(cache) = CacheStore::from_env() {
        let order = crate::configure::resolved_agent_order(None, Some(&cache));
        if order.is_quota() {
            crate::configure::apply_agent_order(order);
        }
    }
    run(providers, false, false)
}

/// Refresh selected providers until their agents leave the working state.
///
/// This command is normally launched detached by `event` and is deliberately
/// quota-only: it never reads a pane. Claude/Agy statusLine hooks publish
/// observations to the local mailbox, while Codex/Grok use their normal
/// providers' fetchers. One global watcher reads Herdr's agent inventory once
/// per poll and refreshes every selected provider that is working. Each
/// provider has its own non-blocking refresh lease, so slow I/O never stalls a
/// statusLine hook or another provider. The existing provider-level debounce
/// remains the lower bound for network requests.
pub fn watch(providers: &[Provider], interval_seconds: Option<u64>) -> Result<()> {
    let cache = CacheStore::from_env()?;
    let interval_seconds = interval_seconds
        .map(CacheStore::validate_watch_interval_seconds)
        .transpose()?
        .unwrap_or_else(|| cache.watch_interval_seconds());
    let Some(_lock) = cache.try_lock_named(TURN_WATCH_LOCK)? else {
        return Ok(());
    };

    let started = Instant::now();
    let started_at = SystemTime::now();
    let started_millis = CacheStore::now_millis();
    let interval = Duration::from_secs(interval_seconds);
    let mut previous_active = Vec::new();
    loop {
        if cache.turn_watchers_stopped_after(started_millis)? {
            break;
        }
        if watch_binary_is_newer(started_at, current_exe_modified()) {
            drop(_lock);
            return reexec_watch();
        }
        // A transient Herdr failure should not terminate a live watcher; the
        // one-hour cap below still prevents an orphaned process. The next
        // poll retries the single inventory call.
        let Ok(state) = list_agent_state() else {
            if started.elapsed() >= MAX_ACTIVE_TURN_WATCH {
                break;
            }
            thread::sleep(interval);
            continue;
        };
        let active = providers
            .iter()
            .copied()
            .filter(|provider| state.working_providers.contains(provider))
            .collect::<Vec<_>>();
        let finishing = previous_active
            .iter()
            .copied()
            .filter(|provider| !active.contains(provider))
            .collect::<Vec<_>>();

        // A provider can settle while another provider keeps working. Run one
        // final debounced pass for providers that just transitioned to idle,
        // then continue polling the remaining active set in the same process.
        if !finishing.is_empty() {
            let _ = refresh_and_publish(&cache, &finishing, false, &state.panes);
        }
        if active.is_empty() {
            break;
        }
        let _ = refresh_and_publish(&cache, &active, false, &state.panes);
        previous_active = active;
        if started.elapsed() >= MAX_ACTIVE_TURN_WATCH {
            break;
        }
        thread::sleep(interval);
    }
    Ok(())
}

fn refresh_and_publish(
    cache: &CacheStore,
    providers: &[Provider],
    force: bool,
    panes: &[AgentPane],
) -> Result<()> {
    refresh_selected(cache, providers, force, panes)?;
    let mut selected = panes_for_providers(panes, providers);
    publish_resolved(cache, &mut selected, None)
}

fn run_internal(
    providers: &[Provider],
    force: bool,
    json: bool,
    topic_pane: Option<&str>,
) -> Result<()> {
    let cache = CacheStore::from_env()?;
    // Agent inventory is metadata-only. Reusing it for both the fetch and the
    // publish pass lets local Codex/Grok diagnostics target the exact pane
    // sessions without adding another Herdr call or reading any pane output.
    let panes = list_agent_panes().ok();
    let session_panes = panes.as_deref().unwrap_or_default();
    let outcomes = refresh_selected(&cache, providers, force, session_panes)?;
    // The all-provider pass (startup and the manual refresh action) is the only
    // one that speaks for every pane, including harnesses with no legacy 1:1
    // collector. A narrower `--provider` selection publishes only its own panes.
    let mut publish_panes = if covers_every_collector(providers) {
        session_panes.to_vec()
    } else {
        panes_for_providers(session_panes, providers)
    };
    publish_resolved(&cache, &mut publish_panes, topic_pane)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    }
    Ok(())
}

pub fn event() -> Result<()> {
    let event = event_json();
    let Some(event) = event.as_ref() else {
        return Ok(());
    };
    let Some(agent) = find_agent(event) else {
        return Ok(());
    };
    let Some(harness) = Harness::from_agent_name(agent) else {
        return Ok(());
    };
    let Some(pane_id) = find_pane_id(event) else {
        return Ok(());
    };

    let cache = CacheStore::from_env()?;
    let Some(pane) = named_pane(pane_id, harness)? else {
        return Ok(());
    };

    let status = find_status(event);
    // Pi's and omp's exact session files carry the routing evidence. Reading
    // their panes would add a visible repaint without improving attribution.
    let topic_pane = (!matches!(harness, Harness::Pi | Harness::Omp)).then_some(pane_id);
    let result = handle_named_pane(&cache, pane, topic_pane);
    // OpenCode (and other non-collector harnesses) must not start the
    // original-four all-provider watch.
    if status.is_some_and(is_working_status) && harness.billing().is_some() {
        if let Err(error) = spawn_watch() {
            if result.is_ok() {
                return Err(error);
            }
        }
    }
    result
}

pub fn focus() -> Result<()> {
    let Some((pane_id, harness)) = current_focused_pane()? else {
        return Ok(());
    };
    let cache = CacheStore::from_env()?;
    let Some(pane) = named_pane(&pane_id, harness)? else {
        return Ok(());
    };
    handle_named_pane(&cache, pane, None)
}

/// The single pane an entry point is allowed to act on.
///
/// It must still be in the one inventory read and still be running the harness
/// that named it; a stale or mismatched pane id yields nothing, so the caller
/// fetches nothing and writes no metadata to any sibling pane.
fn named_pane(pane_id: &str, harness: Harness) -> Result<Option<AgentPane>> {
    Ok(list_agent_state()?
        .panes
        .into_iter()
        .find(|pane| pane.pane_id == pane_id && pane.harness == harness))
}

fn handle_named_pane(cache: &CacheStore, pane: AgentPane, topic_pane: Option<&str>) -> Result<()> {
    let mut panes = [pane];
    let include_topic = cache
        .fields()
        .unwrap_or_default()
        .contains(SidebarField::Topic);
    if include_topic && topic_pane == Some(panes[0].pane_id.as_str()) {
        refresh_pane_topic(&mut panes[0]);
    }
    let resolved = route::resolve_with_identity(&panes[0]);
    // A cross-harness route may reuse an original collector only after its
    // credential scope is proved. Pi's account-id match is the first such
    // route; its path-shaped session is deliberately not passed to Codex as a
    // thread id.
    if let Resolution::Subscription(target) = &resolved.resolution {
        if let Some(provider) = target.original_provider() {
            refresh_selected(cache, &[provider], false, &panes)?;
        }
    }
    let style = sidebar_row_style(cache);
    let tokens = resolved_pane_tokens(
        cache,
        &mut panes[0],
        resolved,
        CacheStore::now_unix(),
        style,
    )?
    .into_iter()
    .collect::<Vec<_>>();
    // Event and focus see one pane, not the whole inventory, which is exactly
    // what the alert needs: the entry is keyed by provider, and a provider
    // with no pane in the pass keeps whatever state it had. Warning here is
    // what makes the alert land at the end of the turn that spent the quota
    // rather than at the next poll.
    notify_low_quota(cache, &tokens);
    publish_pane_tokens(&panes, &tokens, CacheStore::now_millis(), include_topic)
}

/// How every published sidebar token is shaped: the percentage style, the
/// brand marks, and whether the window tokens carry a meter.
fn sidebar_row_style(cache: &CacheStore) -> RowStyle {
    RowStyle::new(
        cache.percent_style().unwrap_or_default(),
        cache.brand_glyphs().unwrap_or_default(),
    )
    .with_meter(
        DashboardPreferences::load_read_only(cache)
            .map(|preferences| preferences.display.sidebar_meter)
            .unwrap_or_default(),
    )
}

fn resolved_pane_tokens(
    cache: &CacheStore,
    pane: &mut AgentPane,
    resolved: route::ResolvedPane,
    now: u64,
    style: RowStyle,
) -> Result<Option<PaneTokens>> {
    let route::ResolvedPane {
        resolution,
        identity,
        context,
        omp,
    } = resolved;
    let mut quota = match resolution {
        Resolution::Subscription(target)
            if target.credential_scope == CredentialScope::OMP_STORE =>
        {
            omp_quota(cache, &target, omp.as_ref(), now, style)
        }
        Resolution::Subscription(target) => {
            if let Some(provider) = target.original_provider() {
                let snapshot = cache.load(provider)?;
                let (account_id, mtime) = current_account_gate(provider);
                let usable = snapshot.as_ref().filter(|snapshot| {
                    snapshot.usable_for_account(account_id.as_deref(), mtime)
                        && snapshot_is_fresh(cache, provider, snapshot, CacheStore::now_unix())
                });
                let values = if let Some(warning) = cache.refresh_warning(provider, usable, now) {
                    Some(MetadataTokens::unavailable_with(
                        provider,
                        warning,
                        style.glyphs,
                    ))
                } else {
                    tokens_for_loaded_snapshot(
                        provider,
                        snapshot.as_ref(),
                        usable,
                        now,
                        pane.session.as_ref().and_then(|session| session.id()),
                        style,
                    )
                };
                values.map(|values| PaneQuotaUpdate::Replace(Box::new(values)))
            } else {
                // Not one of the original four, so it is never fetched by the
                // provider list: this pane resolved to it, so this pane pays
                // for at most one debounced request.
                refresh_scoped_target(cache, &target, false);
                let snapshot = cache.load(target.billing)?;
                tokens_for_provider(
                    snapshot.as_ref(),
                    now,
                    pane.session.as_ref().and_then(|session| session.id()),
                    style,
                )
                .map(|values| PaneQuotaUpdate::Replace(Box::new(values)))
            }
        }
        Resolution::NoSubscription if plugin_quota_present(&pane.tokens) || identity.is_some() => {
            Some(PaneQuotaUpdate::Clear)
        }
        Resolution::NoSubscription => None,
        Resolution::Indeterminate if identity.is_some() => Some(PaneQuotaUpdate::Preserve),
        Resolution::Indeterminate => None,
    };
    if quota.is_none() && (identity.is_some() || context.is_some()) {
        quota = Some(PaneQuotaUpdate::Preserve);
    }
    Ok(quota.map(|quota| PaneTokens {
        pane_id: pane.pane_id.clone(),
        quota,
        identity,
        context,
    }))
}

/// Quota for an omp pane, from omp's own usage layer.
///
/// One `omp usage --json` per debounce window, for the one provider the pane
/// is actually talking to — never a fan-out over omp's whole credential pool.
/// Without an account to attribute the numbers to, the pane keeps what it
/// already published rather than showing a peer account's quota.
fn omp_quota(
    cache: &CacheStore,
    target: &BillingTarget,
    evidence: Option<&OmpEvidence>,
    now: u64,
    style: RowStyle,
) -> Option<PaneQuotaUpdate> {
    let evidence = evidence?;
    omp_quota_with_refresh(cache, target, evidence, now, style, refresh_omp_target)
}

fn omp_quota_with_refresh(
    cache: &CacheStore,
    target: &BillingTarget,
    evidence: &OmpEvidence,
    now: u64,
    style: RowStyle,
    refresh: impl FnOnce(&CacheStore, &BillingTarget, &OmpEvidence, u64) -> OmpUsage,
) -> Option<PaneQuotaUpdate> {
    let pin = evidence.account_pin.as_deref();
    let cached = cache
        .load_target(target)
        .ok()
        .flatten()
        .filter(|snapshot| snapshot.usable_for_account(pin, None));
    let debounced = cache
        .should_debounce_target(target, now, 60)
        .unwrap_or(false);
    if debounced {
        return cached.as_ref().and_then(|snapshot| {
            tokens_for_provider(Some(snapshot), now, None, style)
                .map(|values| PaneQuotaUpdate::Replace(Box::new(values)))
        });
    }
    match refresh(cache, target, evidence, now) {
        OmpUsage::Account(snapshot) => tokens_for_provider(Some(&snapshot), now, None, style)
            .map(|values| PaneQuotaUpdate::Replace(Box::new(values))),
        // omp holds an API key for this provider and no subscription account
        // at all, so any subscription numbers still on the pane belong to a
        // login that is not paying for it.
        OmpUsage::PayAsYouGo => Some(PaneQuotaUpdate::Clear),
        OmpUsage::Unavailable if cached.is_none() => Some(PaneQuotaUpdate::Replace(Box::new(
            MetadataTokens::unavailable_with(
                target.billing,
                "omp reported no quota data",
                style.glyphs,
            ),
        ))),
        OmpUsage::Unavailable | OmpUsage::Unknown => cached.as_ref().and_then(|snapshot| {
            tokens_for_provider(Some(snapshot), now, None, style)
                .map(|values| PaneQuotaUpdate::Replace(Box::new(values)))
        }),
    }
}

/// What one `omp usage --json` call established about a pane's provider.
enum OmpUsage {
    Account(Box<ProviderSnapshot>),
    PayAsYouGo,
    Unavailable,
    Unknown,
}

/// Ask omp for one provider's usage, and cache the account this pane pins.
///
/// Process and parse failures remain silent and preserve the last good value.
/// A successful CLI response that explicitly lists this OAuth account under
/// `accountsWithoutUsage` is different: without an older snapshot it renders
/// N/A so a failed upstream quota fetch is not mistaken for missing support.
fn refresh_omp_target(
    cache: &CacheStore,
    target: &BillingTarget,
    evidence: &OmpEvidence,
    now: u64,
) -> OmpUsage {
    let Ok(Some(_lease)) = cache.try_lock_target_refresh(target) else {
        return OmpUsage::Unknown;
    };
    // Marked before the call so a failing binary cannot be retried on every
    // event; the window applies to attempts, not to successes.
    if cache.mark_refresh_target(target, now).is_err() {
        return OmpUsage::Unknown;
    }
    let Ok(usage) = omp_provider::fetch(&evidence.paths, &evidence.provider_id, now) else {
        return OmpUsage::Unknown;
    };
    let Some(account) = omp_provider::select_account(&usage, evidence.account_pin.as_deref())
    else {
        if omp_provider::oauth_without_usage_matches(&usage, evidence.account_pin.as_deref()) {
            return OmpUsage::Unavailable;
        }
        // Several accounts and no pin is not a coin flip either: only a
        // provider that has an API key and nothing else is proved to be
        // pay-as-you-go.
        return if usage.accounts.is_empty() && usage.has_api_key {
            OmpUsage::PayAsYouGo
        } else {
            OmpUsage::Unknown
        };
    };
    let snapshot = omp_provider::snapshot(target, account);
    if cache.save_target(target, &snapshot).is_err() {
        return OmpUsage::Unknown;
    }
    OmpUsage::Account(Box::new(snapshot))
}

/// Refresh a billing target that has no 1:1 harness collector.
///
/// Failure is deliberately silent: the pane keeps the last good snapshot for
/// this same target rather than being cleared, and a missing key is a normal
/// state (the user may not have a Go subscription) rather than an error worth
/// surfacing on every event.
fn refresh_scoped_target(cache: &CacheStore, target: &BillingTarget, force: bool) {
    let now = CacheStore::now_unix();
    if should_skip_fetch(cache, target.billing, force, now).unwrap_or(true) {
        return;
    }
    let Ok(Some(_lease)) = cache.try_lock_target_refresh(target) else {
        return;
    };
    let Some(paths) = OpenCodePaths::from_env() else {
        return;
    };
    let Some(key) = crate::opencode::go_key(&paths) else {
        return;
    };
    // Marked before the request so a failing endpoint cannot be retried on
    // every event; the debounce window applies to attempts, not successes.
    if cache.mark_refresh(target.billing, now).is_err() {
        return;
    }
    match opencode_go::fetch(&key) {
        Ok(snapshot) => {
            if cache.save(&snapshot).is_ok() {
                let _ = cache.set_refresh_problem(target.billing, None);
            }
        }
        Err(error) => {
            let _ = cache.set_refresh_problem(target.billing, Some(public_refresh_problem(&error)));
        }
    }
}

/// Refresh the global OpenCode Go dashboard row when its dedicated key exists.
///
/// A pane still needs exact session evidence before it receives sidebar quota,
/// but the all-provider dashboard is allowed to show the account-level plan.
pub(crate) fn refresh_opencode_go_dashboard(cache: &CacheStore, force: bool) {
    refresh_scoped_target(cache, &BillingTarget::opencode_go(), force);
}

fn covers_every_collector(providers: &[Provider]) -> bool {
    Provider::ALL
        .iter()
        .all(|provider| providers.contains(provider))
}

fn panes_for_providers(panes: &[AgentPane], providers: &[Provider]) -> Vec<AgentPane> {
    panes
        .iter()
        .filter(|pane| {
            pane.harness
                .billing()
                .is_some_and(|billing| providers.contains(&billing))
        })
        .cloned()
        .collect()
}

#[derive(Debug)]
struct FetchedSnapshot {
    snapshot: ProviderSnapshot,
    preserve_context: bool,
    session_id: Option<String>,
}

impl FetchedSnapshot {
    fn direct(snapshot: ProviderSnapshot) -> Self {
        Self {
            snapshot,
            preserve_context: false,
            session_id: None,
        }
    }
}

fn refresh_selected(
    cache: &CacheStore,
    providers: &[Provider],
    force: bool,
    panes: &[AgentPane],
) -> Result<Vec<ProviderOutcome>> {
    providers
        .iter()
        .copied()
        .map(|provider| refresh_provider(cache, provider, force, panes))
        .collect()
}

fn refresh_provider(
    cache: &CacheStore,
    provider: Provider,
    force: bool,
    panes: &[AgentPane],
) -> Result<ProviderOutcome> {
    let now = CacheStore::now_unix();
    if should_skip_fetch(cache, provider, force, now)? {
        return Ok(ProviderOutcome {
            provider,
            available: load_usable_snapshot(cache, provider)?.is_some(),
            from_cache: true,
            error: None,
        });
    }
    let Some(_lease) = cache.try_lock_provider_refresh(provider)? else {
        return Ok(ProviderOutcome {
            provider,
            available: load_usable_snapshot(cache, provider)?.is_some(),
            from_cache: true,
            error: Some("refresh already in progress".to_string()),
        });
    };

    let session_ids = panes
        .iter()
        .filter(|pane| pane.harness.billing() == Some(provider))
        .filter_map(|pane| {
            pane.session
                .as_ref()
                .and_then(|session| session.id())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    let fetched = match provider {
        Provider::Codex => codex::fetch_for_sessions(&session_ids).map(FetchedSnapshot::direct),
        Provider::Grok => grok::fetch_for_sessions(&session_ids).map(FetchedSnapshot::direct),
        Provider::Claude => claude::fetch()
            .map(FetchedSnapshot::direct)
            .or_else(|_| load_statusline_snapshot(cache, provider)),
        Provider::Agy => load_statusline_snapshot(cache, provider),
        Provider::Hermes => hermes::fetch().map(FetchedSnapshot::direct),
        Provider::OpenRouter => openrouter::fetch().map(FetchedSnapshot::direct),
        // OpenCode Go is fetched for a resolved pane, never through the
        // provider list; see `fetch_opencode_go`.
        Provider::OpenCodeGo | Provider::Omp => Err(anyhow::anyhow!(
            "scoped providers are refreshed per resolved pane, not through --provider"
        )),
    };
    cache.mark_refresh(provider, now)?;
    match fetched {
        Ok(fetched) => {
            let FetchedSnapshot {
                mut snapshot,
                preserve_context,
                session_id,
            } = fetched;
            if preserve_context {
                cache.save_preserving_context_for_session(snapshot, session_id.as_deref())?;
            } else if matches!(
                provider,
                Provider::Claude | Provider::Codex | Provider::Grok
            ) {
                let (_, mtime) = current_account_gate(provider);
                cache.save_preserving_diagnostics_for_sessions(
                    &mut snapshot,
                    &session_ids,
                    mtime,
                )?;
            } else {
                cache.save(&snapshot)?;
            }
            cache.set_refresh_problem(provider, None)?;
            Ok(ProviderOutcome {
                provider,
                available: true,
                from_cache: false,
                error: None,
            })
        }
        Err(error) => {
            cache.set_refresh_problem(provider, Some(public_refresh_problem(&error)))?;
            Ok(ProviderOutcome {
                provider,
                available: load_usable_snapshot(cache, provider)?.is_some(),
                from_cache: true,
                error: Some(error.to_string()),
            })
        }
    }
}

#[test]
fn public_refresh_failures_do_not_persist_private_error_details() {
    use crate::providers::ProviderError;
    assert_eq!(
        public_refresh_problem(&ProviderError::Request("HTTP 401".into()).into()),
        "login"
    );
    assert_eq!(
        public_refresh_problem(&ProviderError::MissingCredentials.into()),
        "credentials"
    );
    assert_eq!(
        public_refresh_problem(&anyhow::anyhow!("private token and endpoint")),
        "failed"
    );
}

fn public_refresh_problem(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<crate::providers::ProviderError>() {
        Some(crate::providers::ProviderError::MissingCredentials) => "credentials",
        Some(crate::providers::ProviderError::Request(status))
            if matches!(status.as_str(), "HTTP 401" | "HTTP 403") =>
        {
            "login"
        }
        _ if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            "cli"
        }
        _ => "failed",
    }
}

fn should_skip_fetch(
    cache: &CacheStore,
    provider: Provider,
    force: bool,
    now_unix: u64,
) -> Result<bool> {
    if force || !cache.should_debounce(provider, now_unix, 60)? {
        return Ok(false);
    }
    if load_usable_snapshot(cache, provider)?.is_some() {
        return Ok(true);
    }
    // No snapshot at all: keep debounce so missing credentials do not hammer
    // the provider. A snapshot for another account must not debounce — fetch
    // the signed-in identity now.
    Ok(cache.load(provider)?.is_none())
}

pub(crate) fn load_usable_snapshot(
    cache: &CacheStore,
    provider: Provider,
) -> Result<Option<ProviderSnapshot>> {
    let Some(snapshot) = cache.load(provider)? else {
        return Ok(None);
    };
    let (account_id, mtime) = current_account_gate(provider);
    Ok((snapshot.usable_for_account(account_id.as_deref(), mtime)
        && snapshot_is_fresh(cache, provider, &snapshot, CacheStore::now_unix()))
    .then_some(snapshot))
}

fn snapshot_is_fresh(
    cache: &CacheStore,
    provider: Provider,
    snapshot: &ProviderSnapshot,
    now_unix: u64,
) -> bool {
    !matches!(provider, Provider::Claude | Provider::Agy)
        || now_unix.saturating_sub(snapshot.fetched_at_unix)
            <= cache
                .watch_interval_seconds()
                .saturating_mul(STATUSLINE_STALE_AFTER_INTERVALS)
}

fn current_account_gate(provider: Provider) -> (Option<String>, Option<u64>) {
    match provider {
        Provider::Grok => {
            let path = grok::auth_path().ok();
            let account_id = path
                .as_ref()
                .and_then(|path| grok::read_credentials(path).ok())
                .and_then(|credentials| credentials.user_id);
            let mtime = path.as_ref().and_then(|path| grok::auth_mtime_unix(path));
            (account_id, mtime)
        }
        Provider::Codex => (codex::current_account_id(), codex::auth_mtime_unix()),
        // The Hermes token identifies its Portal org, so a re-login to a
        // different account drops the previous account's cached dollars.
        Provider::Hermes => (
            hermes::auth_path()
                .ok()
                .and_then(|path| hermes::read_credentials(&path).ok())
                .and_then(|credentials| credentials.account_id),
            None,
        ),
        Provider::OpenCodeGo => (
            OpenCodePaths::from_env()
                .and_then(|paths| crate::opencode::go_key(&paths))
                .map(|key| crate::model::private_identifier("credential", key.trim())),
            None,
        ),
        Provider::OpenRouter => (
            openrouter::api_key()
                .ok()
                .map(|key| crate::model::private_identifier("credential", key.trim())),
            None,
        ),
        Provider::Claude => {
            let path = claude::credentials_path().ok();
            (
                path.as_ref()
                    .and_then(|path| claude::access_token(path).ok())
                    .map(|token| crate::model::private_identifier("credential", token.trim())),
                path.as_ref()
                    .and_then(|path| CacheStore::file_mtime_unix(path)),
            )
        }
        Provider::Agy | Provider::Omp => (None, None),
    }
}

fn load_statusline_snapshot(cache: &CacheStore, provider: Provider) -> Result<FetchedSnapshot> {
    let observation = cache
        .load_statusline_observation(provider)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} usage is collected by the statusLine hook",
                provider.source()
            )
        })?;
    Ok(FetchedSnapshot {
        snapshot: observation.snapshot,
        preserve_context: true,
        session_id: observation.session_key,
    })
}

fn publish_resolved(
    cache: &CacheStore,
    panes: &mut [AgentPane],
    topic_pane: Option<&str>,
) -> Result<()> {
    let include_topic = cache
        .fields()
        .unwrap_or_default()
        .contains(SidebarField::Topic);
    if include_topic {
        if let Some(pane) =
            topic_pane.and_then(|pane_id| panes.iter_mut().find(|pane| pane.pane_id == pane_id))
        {
            refresh_pane_topic(pane);
        }
    }
    let mut tokens = Vec::new();
    let now = CacheStore::now_unix();
    let style = sidebar_row_style(cache);
    for pane in panes.iter_mut() {
        if let Some(pane_tokens) =
            resolved_pane_tokens(cache, pane, route::resolve_with_identity(pane), now, style)?
        {
            tokens.push(pane_tokens);
        }
    }
    notify_low_quota(cache, &tokens);
    publish_pane_tokens(panes, &tokens, CacheStore::now_millis(), include_topic)
}

/// The lowest headroom each provider is showing in this pass.
///
/// Keyed by the provider's display name because that is both what a pane
/// reports and what a notification has to say. Several panes on one provider
/// collapse to one entry, so three Claude panes are one warning.
fn lowest_headroom_by_provider(tokens: &[PaneTokens]) -> BTreeMap<String, u8> {
    let mut lowest = BTreeMap::new();
    for pane in tokens {
        let PaneQuotaUpdate::Replace(values) = &pane.quota else {
            continue;
        };
        let Some(headroom) = values.quota_headroom else {
            continue;
        };
        lowest
            .entry(values.quota_provider.clone())
            .and_modify(|current: &mut u8| *current = (*current).min(headroom))
            .or_insert(headroom);
    }
    lowest
}

/// Warn once per provider that has fallen to the alert threshold.
///
/// A provider stays quiet for as long as it stays low, and is re-armed only by
/// recovering above the threshold — a quota that resets and is spent again
/// warns again. Providers with no pane in this pass keep whatever state they
/// had, so closing and reopening a pane is not a way to be warned twice.
fn notify_low_quota(cache: &CacheStore, tokens: &[PaneTokens]) {
    let alert = cache.low_quota_alert().unwrap_or_default();
    if alert.is_off() {
        return;
    }
    let lowest = lowest_headroom_by_provider(tokens);
    let previous = cache.low_quota_alerted();
    let (warn, alerted) = low_quota_transitions(alert, &lowest, &previous);
    for provider in &warn {
        let headroom = lowest.get(provider).copied().unwrap_or_default();
        let _ = crate::herdr::notify(
            &format!("{provider} quota is low"),
            &format!("{headroom}% left in the window closest to its limit."),
        );
    }
    // Publishing happens on every event path. Rewriting an unchanged set every
    // time would be disk churn for nothing.
    if alerted != previous {
        let _ = cache.set_low_quota_alerted(&alerted);
    }
}

/// Which providers to warn about now, and the state to remember afterwards.
///
/// Split out from the notification itself so the rule can be tested without a
/// cache or a Herdr: a provider is warned about on the way down and not again
/// until it has been seen above the threshold.
fn low_quota_transitions(
    alert: LowQuotaAlert,
    lowest: &BTreeMap<String, u8>,
    previous: &[String],
) -> (Vec<String>, Vec<String>) {
    // A provider with no pane in this pass keeps the state it had. Otherwise
    // closing a pane would re-arm the warning and reopening it would repeat.
    let mut alerted: Vec<String> = previous
        .iter()
        .filter(|provider| !lowest.contains_key(*provider))
        .cloned()
        .collect();
    let mut warn = Vec::new();
    for (provider, headroom) in lowest {
        if !alert.triggers(*headroom) {
            continue;
        }
        alerted.push(provider.clone());
        if !previous.contains(provider) {
            warn.push(provider.clone());
        }
    }
    alerted.sort();
    alerted.dedup();
    (warn, alerted)
}

fn event_json() -> Option<Value> {
    let input = std::env::var("HERDR_PLUGIN_EVENT_JSON").ok()?;
    serde_json::from_str(&input).ok()
}

fn find_status(value: &Value) -> Option<&str> {
    find_field(value, &["agent_status", "agentStatus", "status", "state"])
}

fn is_working_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("working")
}

fn current_exe_modified() -> Option<SystemTime> {
    std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
}

fn watch_binary_is_newer(started: SystemTime, modified: Option<SystemTime>) -> bool {
    modified.is_some_and(|mtime| mtime > started)
}

fn reexec_watch() -> Result<()> {
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    let mut command = Command::new(executable);
    command.args(["watch", "--provider", "all"]);
    #[cfg(unix)]
    {
        let error = command.exec();
        anyhow::bail!("re-exec active-turn quota watcher: {error}");
    }
    #[cfg(not(unix))]
    {
        // Windows has no exec(); the closest equivalent is a detached
        // replacement plus an immediate exit by the caller.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::platform::detach_fully(&mut command);
        command
            .spawn()
            .context("restart active-turn quota watcher")?;
        Ok(())
    }
}

fn spawn_watch() -> Result<()> {
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    let mut command = Command::new(executable);
    command
        .args(["watch", "--provider", "all"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A Herdr event process is short-lived. Detach the watcher so it survives
    // the hook supervisor cleanly: a new session on Unix, a detached process
    // group on Windows.
    crate::platform::detach_fully(&mut command);
    command.spawn().context("start active-turn quota watcher")?;
    Ok(())
}

// Event payloads are nested and their shape differs per event, so look the
// field up anywhere in the tree rather than at a fixed path.
fn find_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    match value {
        Value::Object(map) => names
            .iter()
            .find_map(|name| map.get(*name).and_then(Value::as_str))
            .or_else(|| map.values().find_map(|child| find_field(child, names))),
        Value::Array(values) => values.iter().find_map(|child| find_field(child, names)),
        _ => None,
    }
}

fn find_agent(value: &Value) -> Option<&str> {
    find_field(value, &["agent"])
}

fn find_pane_id(value: &Value) -> Option<&str> {
    find_field(value, &["pane_id", "paneId"])
}

fn tokens_for_provider(
    snapshot: Option<&crate::model::ProviderSnapshot>,
    now_unix: u64,
    session_id: Option<&str>,
    style: RowStyle,
) -> Option<MetadataTokens> {
    snapshot.map(|snapshot| {
        MetadataTokens::from_snapshot_for_pane(snapshot, now_unix, session_id, style)
    })
}

fn tokens_for_loaded_snapshot(
    provider: Provider,
    raw: Option<&ProviderSnapshot>,
    usable: Option<&ProviderSnapshot>,
    now_unix: u64,
    session_id: Option<&str>,
    style: RowStyle,
) -> Option<MetadataTokens> {
    match (usable, raw) {
        (Some(snapshot), _) => tokens_for_provider(Some(snapshot), now_unix, session_id, style),
        (None, Some(_)) => Some(MetadataTokens::unavailable_with(
            provider,
            "signed-in account changed",
            style.glyphs,
        )),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderSnapshot, UsageWindow, WindowKind};
    use tempfile::tempdir;

    fn low(pairs: &[(&str, u8)]) -> BTreeMap<String, u8> {
        pairs
            .iter()
            .map(|(provider, headroom)| ((*provider).to_string(), *headroom))
            .collect()
    }

    #[test]
    fn dashboard_refreshes_only_visible_global_collectors() {
        let mut preferences = DashboardPreferences::default();
        assert_eq!(
            dashboard_refresh_providers(&preferences),
            vec![
                Provider::Claude,
                Provider::Codex,
                Provider::Grok,
                Provider::Hermes,
                Provider::OpenRouter
            ]
        );
        preferences.get_mut(DashboardProvider::Grok).show = false;
        preferences.get_mut(DashboardProvider::Omp).show = true;
        assert_eq!(
            dashboard_refresh_providers(&preferences),
            vec![
                Provider::Claude,
                Provider::Codex,
                Provider::Hermes,
                Provider::OpenRouter
            ]
        );
    }

    #[test]
    fn dashboard_refresh_does_not_overlap_an_active_pass() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let lock = cache
            .try_lock_named(DASHBOARD_REFRESH_LOCK)
            .unwrap()
            .unwrap();
        assert!(
            refresh_dashboard(&cache, &DashboardPreferences::default(), true)
                .unwrap()
                .is_none()
        );
        drop(lock);
    }

    #[test]
    fn a_provider_below_the_threshold_is_warned_about_once_until_it_recovers() {
        let alert = LowQuotaAlert::parse("10").unwrap();
        let (warn, state) = low_quota_transitions(alert, &low(&[("Claude", 8)]), &[]);
        assert_eq!(warn, vec!["Claude".to_string()]);
        assert_eq!(state, vec!["Claude".to_string()]);

        // Still low: remembered, and silent.
        let (warn, state) = low_quota_transitions(alert, &low(&[("Claude", 3)]), &state);
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(state, vec!["Claude".to_string()]);

        // Recovered above the threshold: re-armed.
        let (warn, state) = low_quota_transitions(alert, &low(&[("Claude", 40)]), &state);
        assert!(warn.is_empty(), "{warn:?}");
        assert!(state.is_empty(), "{state:?}");

        let (warn, _) = low_quota_transitions(alert, &low(&[("Claude", 9)]), &state);
        assert_eq!(warn, vec!["Claude".to_string()]);
    }

    /// Closing the last pane of a provider must not re-arm its warning: the
    /// quota did not recover, the window into it just went away.
    #[test]
    fn a_provider_with_no_pane_in_this_pass_keeps_its_state() {
        let alert = LowQuotaAlert::parse("20").unwrap();
        let previous = vec!["Codex".to_string()];
        let (warn, state) = low_quota_transitions(alert, &low(&[("Claude", 90)]), &previous);
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(state, previous);
    }

    #[test]
    fn the_threshold_is_inclusive_and_off_never_warns() {
        let alert = LowQuotaAlert::parse("10").unwrap();
        let (warn, _) = low_quota_transitions(alert, &low(&[("Grok", 10)]), &[]);
        assert_eq!(warn, vec!["Grok".to_string()]);
        let (warn, _) = low_quota_transitions(alert, &low(&[("Grok", 11)]), &[]);
        assert!(warn.is_empty(), "{warn:?}");
        let (warn, _) = low_quota_transitions(LowQuotaAlert::OFF, &low(&[("Grok", 0)]), &[]);
        assert!(warn.is_empty(), "{warn:?}");
    }

    /// Several panes on one provider are one quota, so they are one warning,
    /// reported at the lowest headroom any of them saw.
    #[test]
    fn panes_sharing_a_provider_collapse_to_one_entry() {
        let tokens = |provider: &str, headroom: Option<u8>| {
            let mut values = MetadataTokens::unavailable(Provider::Claude, "test");
            values.quota_provider = provider.to_string();
            values.quota_headroom = headroom;
            PaneTokens {
                pane_id: format!("w1:{provider}{headroom:?}"),
                quota: PaneQuotaUpdate::Replace(Box::new(values)),
                identity: None,
                context: None,
            }
        };
        let lowest = lowest_headroom_by_provider(&[
            tokens("Claude", Some(40)),
            tokens("Claude", Some(12)),
            tokens("Codex", None),
        ]);
        assert_eq!(lowest, low(&[("Claude", 12)]));
    }

    #[test]
    fn replaced_watch_binary_is_detected() {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert!(watch_binary_is_newer(
            started,
            Some(started + Duration::from_secs(1))
        ));
        assert!(!watch_binary_is_newer(
            started,
            Some(started - Duration::from_secs(1))
        ));
        assert!(!watch_binary_is_newer(started, None));
    }

    #[test]
    fn successful_snapshot_is_kept_when_provider_refresh_fails() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let snapshot = ProviderSnapshot::new(
            Provider::Grok,
            vec![UsageWindow::new(WindowKind::Weekly, 42.5, None).unwrap()],
            1,
        );
        cache.save(&snapshot).unwrap();
        assert_eq!(cache.load(Provider::Grok).unwrap(), Some(snapshot));
    }

    #[test]
    fn statusline_snapshots_stop_claiming_to_be_live_after_two_intervals() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache.set_watch_interval_seconds(60).unwrap();
        let claude = ProviderSnapshot::new(Provider::Claude, vec![], 100);
        let codex = ProviderSnapshot::new(Provider::Codex, vec![], 100);

        assert!(snapshot_is_fresh(&cache, Provider::Claude, &claude, 220));
        assert!(!snapshot_is_fresh(&cache, Provider::Claude, &claude, 221));
        assert!(snapshot_is_fresh(&cache, Provider::Codex, &codex, 10_000));
    }

    #[test]
    fn missing_snapshot_does_not_overwrite_sidebar_with_unavailable() {
        let values = tokens_for_provider(None, 1, None, RowStyle::default());
        assert!(values.is_none());
    }

    #[test]
    fn an_omp_oauth_account_without_usage_is_explicit_on_the_first_fetch() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let target = BillingTarget::omp("anthropic");
        let evidence = crate::omp::OmpEvidence {
            paths: crate::omp::OmpPaths {
                agent_dir: directory.path().join(".omp/agent"),
                sessions: directory.path().join(".omp/agent/sessions"),
            },
            provider_id: "anthropic".to_string(),
            account_pin: Some("account-pin".to_string()),
        };
        let update = omp_quota_with_refresh(
            &cache,
            &target,
            &evidence,
            100,
            RowStyle::default(),
            |_, _, _, _| OmpUsage::Unavailable,
        )
        .expect("explicit unavailable update");
        let PaneQuotaUpdate::Replace(values) = update else {
            panic!("expected replacement");
        };
        assert_eq!(values.quota_week, "7d N/A");
        assert_eq!(
            values.quota_error.as_deref(),
            Some("omp reported no quota data")
        );
    }

    #[test]
    fn an_omp_failed_first_fetch_is_debounced_without_a_snapshot() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let target = BillingTarget::omp("anthropic");
        cache.mark_refresh_target(&target, 100).unwrap();
        let evidence = crate::omp::OmpEvidence {
            paths: crate::omp::OmpPaths {
                agent_dir: directory.path().join(".omp/agent"),
                sessions: directory.path().join(".omp/agent/sessions"),
            },
            provider_id: "anthropic".to_string(),
            account_pin: Some("account-pin".to_string()),
        };
        let update = omp_quota_with_refresh(
            &cache,
            &target,
            &evidence,
            120,
            RowStyle::default(),
            |_, _, _, _| panic!("debounced refresh must not run"),
        );
        assert!(update.is_none());
    }

    #[test]
    fn an_omp_usage_failure_keeps_the_same_accounts_last_good_snapshot() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let target = BillingTarget::omp("anthropic");
        let snapshot = ProviderSnapshot::new(
            Provider::Claude,
            vec![UsageWindow::new(WindowKind::Weekly, 42.0, None).unwrap()],
            90,
        )
        .with_account_id(Some("account-pin".to_string()));
        cache.save_target(&target, &snapshot).unwrap();
        let evidence = crate::omp::OmpEvidence {
            paths: crate::omp::OmpPaths {
                agent_dir: directory.path().join(".omp/agent"),
                sessions: directory.path().join(".omp/agent/sessions"),
            },
            provider_id: "anthropic".to_string(),
            account_pin: Some("account-pin".to_string()),
        };
        let update = omp_quota_with_refresh(
            &cache,
            &target,
            &evidence,
            200,
            RowStyle::default(),
            |_, _, _, _| OmpUsage::Unavailable,
        )
        .expect("last good update");
        let PaneQuotaUpdate::Replace(values) = update else {
            panic!("expected replacement");
        };
        assert_eq!(values.quota_week, "7d 58%");
        assert_eq!(values.quota_error, None);
    }

    #[test]
    fn other_account_snapshot_is_not_shown_as_the_current_quota() {
        let snapshot = ProviderSnapshot::new(
            Provider::Grok,
            vec![UsageWindow::new(WindowKind::Weekly, 100.0, None).unwrap()],
            1,
        )
        .with_account_id(Some("old-account".to_string()));
        let values = tokens_for_loaded_snapshot(
            Provider::Grok,
            Some(&snapshot),
            None,
            1,
            None,
            RowStyle::default(),
        )
        .unwrap();
        assert_eq!(values.quota_week, "7d N/A");
        assert_eq!(
            values.quota_week_severity,
            Some(crate::model::Severity::Unknown)
        );
        assert_eq!(
            values.quota_error.as_deref(),
            Some("signed-in account changed")
        );
        // A failure must not masquerade as a lapsed prompt cache.
        assert_eq!(values.quota_cache_state, "");
    }

    #[test]
    fn debounce_does_not_keep_another_accounts_grok_snapshot() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let snapshot = ProviderSnapshot::new(
            Provider::Grok,
            vec![UsageWindow::new(WindowKind::Weekly, 100.0, None).unwrap()],
            1,
        )
        .with_account_id(Some("old-account".to_string()));
        cache.save(&snapshot).unwrap();
        cache.mark_refresh(Provider::Grok, 100).unwrap();
        assert!(
            !should_skip_fetch(&cache, Provider::Grok, false, 120).unwrap(),
            "a snapshot for another Grok login must be fetched even inside the debounce window"
        );
    }

    // Reading a pane repaints it, which visibly scrolls the agent's terminal.
    // An event must name exactly one pane to read, so the other panes of the
    // same provider are left alone.
    #[test]
    fn unknown_and_opencode_events_select_no_collectors() {
        // `event` reads the agent name straight off the payload, so this is
        // the exact chain that decides whether a watch may start.
        fn collector(payload: &str) -> Option<Provider> {
            let value: Value = serde_json::from_str(payload).unwrap();
            let agent = find_agent(&value)?;
            Harness::from_agent_name(agent)?.billing()
        }

        assert_eq!(
            collector(
                r#"{"event":"pane_agent_status_changed",
                    "data":{"pane_id":"w1:p9","agent":"opencode","status":"working"}}"#
            ),
            None
        );
        assert_eq!(
            collector(r#"{"data":{"agent":"OpenCode","status":"working"}}"#),
            None
        );
        assert_eq!(
            collector(r#"{"data":{"agent":"cursor","status":"working"}}"#),
            None
        );
        assert_eq!(
            collector(r#"{"data":{"agent":"claude-code","pane_id":"w1:p1"}}"#),
            Some(Provider::Claude)
        );
    }

    #[test]
    fn event_payload_names_the_single_pane_whose_topic_may_be_read() {
        let value: Value = serde_json::from_str(
            r#"{"event":"pane_agent_status_changed",
                "data":{"pane_id":"w1:p2","agent":"grok","status":"working"}}"#,
        )
        .unwrap();
        assert_eq!(find_pane_id(&value), Some("w1:p2"));
        assert_eq!(find_agent(&value), Some("grok"));
    }

    #[test]
    fn an_event_without_a_pane_reads_no_pane_at_all() {
        let value: Value =
            serde_json::from_str(r#"{"event":"x","data":{"agent":"claude"}}"#).unwrap();
        assert_eq!(find_pane_id(&value), None);
    }

    #[test]
    fn status_events_start_pulses_only_for_working_turns() {
        let working: Value =
            serde_json::from_str(r#"{"data":{"agent":"codex","agent_status":"working"}}"#).unwrap();
        let idle: Value =
            serde_json::from_str(r#"{"data":{"agent":"codex","status":"idle"}}"#).unwrap();
        assert!(find_status(&working).is_some_and(is_working_status));
        assert_eq!(find_status(&idle), Some("idle"));
    }
}
