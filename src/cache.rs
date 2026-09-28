use crate::brand::GlyphSet;
use crate::cli::{
    AgentOrder, BrandColors, FieldSet, LowQuotaAlert, PercentStyle, SidebarLayout, SidebarRowGap,
};
use crate::model::{
    merge_omitted_window_list, BillingTarget, ContextUsage, Provider, ProviderSnapshot, WindowKind,
};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One recorded observation of a quota window: `(fetched_at_unix, used_percent)`.
pub type HistorySample = (u64, f64);

pub const DEFAULT_WATCH_INTERVAL_SECONDS: u64 = 60;
pub const MIN_WATCH_INTERVAL_SECONDS: u64 = 30;
pub const MAX_WATCH_INTERVAL_SECONDS: u64 = 60 * 60;
const WATCH_INTERVAL_ENV: &str = "HERDR_AGENT_QUOTA_WATCH_INTERVAL_SECONDS";
const WATCH_INTERVAL_FILE: &str = "watch-interval-seconds";
const SIDEBAR_LAYOUT_FILE: &str = "sidebar-layout";
const ROW_GAP_FILE: &str = "row-gap";
const QUOTA_PERCENT_FILE: &str = "quota-percent";
const BRAND_GLYPHS_FILE: &str = "brand-glyphs";
const FIELDS_FILE: &str = "fields";
const BRAND_COLORS_FILE: &str = "brand-colors";
const AGENT_ORDER_FILE: &str = "agent-order";
const LOW_QUOTA_ALERT_FILE: &str = "low-quota-alert";
const SETTINGS_APPLY_PENDING_FILE: &str = "settings-apply.pending";
/// One line per provider that is currently below the alert threshold, so a
/// crossing notifies once instead of on every refresh.
const LOW_QUOTA_ALERTED_FILE: &str = "low-quota-alerted";
const MAX_STATUSLINE_SESSIONS: usize = 128;
/// Enough history for a 48-hour sparkline at the minimum watch interval,
/// without ever growing a file beyond a few tens of kilobytes.
const HISTORY_MAX_SAMPLES: usize = 720;
/// StatusLine hooks can write several times a minute; the trend does not
/// need that resolution and the file must not churn for it.
const HISTORY_MIN_GAP_SECONDS: u64 = 60;

#[derive(Debug, Clone)]
pub struct CacheStore {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatuslineObservation {
    pub snapshot: ProviderSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
}

#[derive(Deserialize)]
struct StoredStatuslineObservation {
    snapshot: ProviderSnapshot,
    #[serde(default)]
    session_key: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

impl CacheStore {
    pub fn from_env() -> Result<Self> {
        let root = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                ProjectDirs::from("dev", "herdr", "herdr-agent-quota")
                    .map(|dirs| dirs.data_local_dir().to_path_buf())
            })
            .context("cannot determine plugin state directory")?;
        Ok(Self { root })
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Only fixed public status codes are persisted, never provider error text.
    pub fn set_refresh_problem(&self, provider: Provider, code: Option<&str>) -> Result<()> {
        self.ensure()?;
        let path = self.root.join(format!("{}.problem", provider.source()));
        match code {
            Some(code @ ("login" | "credentials" | "failed" | "cli")) => {
                let temp = path.with_extension(format!("problem.{}.tmp", std::process::id()));
                Self::atomic_replace(&path, &temp, code.as_bytes().to_vec())
            }
            Some(_) => anyhow::bail!("invalid public refresh status"),
            None => match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
        }
    }

    pub fn refresh_warning(
        &self,
        provider: Provider,
        snapshot: Option<&ProviderSnapshot>,
        now: u64,
    ) -> Option<String> {
        self.refresh_problem(provider)
            .map(str::to_string)
            .or_else(|| {
                let snapshot = snapshot?;
                let age = now.saturating_sub(snapshot.fetched_at_unix);
                let stale_after = self.watch_interval_seconds().saturating_mul(2).max(120);
                (age > stale_after).then(|| format!("stale; last update {}m ago", age / 60))
            })
    }

    pub fn refresh_problem(&self, provider: Provider) -> Option<&'static str> {
        Some(match self.refresh_problem_code(provider)? {
            "login" => "sign in again",
            "credentials" => "credentials unavailable",
            "failed" => "refresh failed; check connection",
            "cli" => "CLI not found; check installation",
            _ => return None,
        })
    }

    /// The fixed public code behind [`Self::refresh_problem`], for machine
    /// readers. Anything that is not one of the known codes reads as none.
    pub fn refresh_problem_code(&self, provider: Provider) -> Option<&'static str> {
        let value =
            fs::read_to_string(self.root.join(format!("{}.problem", provider.source()))).ok()?;
        ["login", "credentials", "failed", "cli"]
            .into_iter()
            .find(|code| *code == value.as_str())
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create cache directory {}", self.root.display()))
    }

    pub fn load(&self, provider: Provider) -> Result<Option<ProviderSnapshot>> {
        let path = self.snapshot_path(provider);
        if !path.exists() {
            return Ok(None);
        }
        for _ in 0..3 {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let mut snapshot: ProviderSnapshot = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse cached {} snapshot", provider.source()))?;
            let legacy = snapshot.clone();
            snapshot.protect_private_fields();
            if snapshot == legacy {
                return Ok(Some(snapshot));
            }
            let temporary = self.root.join(format!(
                ".{}.migration.{}.tmp",
                provider.source(),
                std::process::id()
            ));
            let migrated = serde_json::to_vec_pretty(&snapshot)
                .context("serialize migrated quota snapshot")?;
            if Self::atomic_replace_if_unchanged(&path, &temporary, migrated, &bytes)? {
                return Ok(Some(snapshot));
            }
        }
        anyhow::bail!(
            "{} changed repeatedly during privacy migration",
            path.display()
        )
    }

    /// Load the snapshot of a target that is not a canonical collector.
    ///
    /// Scoped targets share a [`Provider`] with the canonical store they are
    /// distinct from, so they are addressed by [`BillingTarget::cache_identity`]
    /// rather than by provider alone.
    pub fn load_target(&self, target: &BillingTarget) -> Result<Option<ProviderSnapshot>> {
        let path = self.target_snapshot_path(target);
        if !path.exists() {
            return Ok(None);
        }
        for _ in 0..3 {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let mut snapshot: ProviderSnapshot = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse cached {} snapshot", target.cache_identity()))?;
            let legacy = snapshot.clone();
            snapshot.protect_private_fields();
            if snapshot == legacy {
                return Ok(Some(snapshot));
            }
            let temporary = self.root.join(format!(
                ".{}.migration.{}.tmp",
                target.cache_identity(),
                std::process::id()
            ));
            let migrated = serde_json::to_vec_pretty(&snapshot)
                .context("serialize migrated quota snapshot")?;
            if Self::atomic_replace_if_unchanged(&path, &temporary, migrated, &bytes)? {
                return Ok(Some(snapshot));
            }
        }
        anyhow::bail!(
            "{} changed repeatedly during privacy migration",
            path.display()
        )
    }

    pub fn save_target(&self, target: &BillingTarget, snapshot: &ProviderSnapshot) -> Result<()> {
        self.ensure()?;
        let identity = target.cache_identity();
        let destination = self.target_snapshot_path(target);
        let temporary = self
            .root
            .join(format!(".{identity}.{}.tmp", std::process::id()));
        let mut snapshot = snapshot.clone();
        snapshot.protect_private_fields();
        let bytes = serde_json::to_vec_pretty(&snapshot).context("serialize quota snapshot")?;
        Self::atomic_replace(&destination, &temporary, bytes)?;
        // Trend data is a convenience; a failure to record it must not turn
        // a successful fetch into a failed refresh.
        let _ = self.record_history(&identity, &snapshot);
        Ok(())
    }

    pub fn should_debounce_target(
        &self,
        target: &BillingTarget,
        now_unix: u64,
        interval_seconds: u64,
    ) -> Result<bool> {
        let Ok(contents) = fs::read_to_string(self.target_refresh_marker_path(target)) else {
            return Ok(false);
        };
        let Ok(last) = contents.trim().parse::<u64>() else {
            return Ok(false);
        };
        Ok(now_unix.saturating_sub(last) < interval_seconds)
    }

    pub fn mark_refresh_target(&self, target: &BillingTarget, now_unix: u64) -> Result<()> {
        self.ensure()?;
        fs::write(
            self.target_refresh_marker_path(target),
            now_unix.to_string(),
        )
        .context("write refresh marker")
    }

    pub fn save(&self, snapshot: &ProviderSnapshot) -> Result<()> {
        self.ensure()?;
        let destination = self.snapshot_path(snapshot.provider);
        let temporary = self.root.join(format!(
            ".{}.{}.tmp",
            snapshot.provider.source(),
            std::process::id()
        ));
        let mut snapshot = snapshot.clone();
        snapshot.protect_private_fields();
        let bytes = serde_json::to_vec_pretty(&snapshot).context("serialize quota snapshot")?;
        Self::atomic_replace(&destination, &temporary, bytes)?;
        let _ = self.record_history(snapshot.provider.source(), &snapshot);
        Ok(())
    }

    /// The history identity of a canonical collector's snapshot file.
    pub fn history_identity(provider: Provider) -> &'static str {
        provider.source()
    }

    /// Append each account window's `(fetched_at, used_percent)` to the
    /// trend file beside the snapshot. Only percentages and timestamps are
    /// stored: no model names, session ids, or provider payloads.
    ///
    /// A sample within [`HISTORY_MIN_GAP_SECONDS`] of the previous one
    /// replaces it, and each window keeps at most
    /// [`HISTORY_MAX_SAMPLES`], so the file is bounded however often the
    /// collectors run.
    pub fn record_history(&self, identity: &str, snapshot: &ProviderSnapshot) -> Result<()> {
        if snapshot.windows.is_empty() {
            return Ok(());
        }
        let path = self.history_path(identity);
        let mut history: BTreeMap<String, Vec<HistorySample>> = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut changed = false;
        for window in &snapshot.windows {
            let samples = history.entry(window.kind.label().to_string()).or_default();
            let sample = (snapshot.fetched_at_unix, window.used_percent);
            match samples.last() {
                Some(last) if last.0 >= sample.0 => continue,
                Some(last) if sample.0 - last.0 < HISTORY_MIN_GAP_SECONDS => {
                    *samples.last_mut().expect("checked above") = sample;
                }
                _ => samples.push(sample),
            }
            if samples.len() > HISTORY_MAX_SAMPLES {
                let excess = samples.len() - HISTORY_MAX_SAMPLES;
                samples.drain(..excess);
            }
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        let temporary = self
            .root
            .join(format!(".{identity}.history.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec(&history).context("serialize quota history")?;
        Self::atomic_replace(&path, &temporary, bytes)
    }

    /// The recorded `(fetched_at, used_percent)` samples of one window,
    /// oldest first. Missing or unreadable history is simply empty.
    pub fn load_history(&self, identity: &str, kind: WindowKind) -> Vec<HistorySample> {
        fs::read(self.history_path(identity))
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<BTreeMap<String, Vec<HistorySample>>>(&bytes).ok()
            })
            .and_then(|mut history| history.remove(kind.label()))
            .unwrap_or_default()
    }

    fn history_path(&self, identity: &str) -> PathBuf {
        self.root.join(format!("{identity}.history.json"))
    }

    /// Keep provider-local diagnostics when a successful quota refresh cannot
    /// read one of the session files for a moment. Quota windows from the
    /// latest fetch replace the cache; an omitted 5h/weekly window is restored
    /// from the previous snapshot only when that window is still current and
    /// the signed-in account has not changed. A newer credential file drops
    /// the previous login's windows even when neither snapshot has an
    /// `account_id`.
    pub fn save_preserving_diagnostics(&self, mut snapshot: ProviderSnapshot) -> Result<()> {
        self.save_preserving_diagnostics_for_sessions(&mut snapshot, &[], None)
    }

    /// Variant used by the refresh path, which knows the session ids currently
    /// visible in Herdr. It preserves only those ids, keeping a bounded local
    /// snapshot instead of growing it forever as old sessions age out.
    pub fn save_preserving_diagnostics_for_sessions(
        &self,
        snapshot: &mut ProviderSnapshot,
        session_ids: &[String],
        credentials_mtime_unix: Option<u64>,
    ) -> Result<()> {
        snapshot.protect_private_fields();
        let session_ids = session_ids
            .iter()
            .map(|session_id| crate::model::private_identifier("session", session_id))
            .collect::<Vec<_>>();
        if let Some(previous) = self.load(snapshot.provider).ok().flatten() {
            let same_account =
                previous.usable_for_account(snapshot.account_id.as_deref(), credentials_mtime_unix);
            if same_account {
                snapshot.merge_omitted_windows(&previous);
                // A refresh scoped to one pane's session still must not delete
                // what it never looked at. An agent event names a single pane,
                // so the fetch only enriches that session; every other pane's
                // diagnostics are carried forward rather than dropped and then
                // re-published as a cleared token on the next focus.
                // `prune_session_diagnostics` below keeps the map bounded.
                for (session_id, context) in previous.session_contexts {
                    snapshot
                        .session_contexts
                        .entry(session_id)
                        .or_insert(context);
                }
                for (session_id, model) in previous.session_models {
                    snapshot.session_models.entry(session_id).or_insert(model);
                }
                // Provider-level values speak for a pane whose session Herdr
                // could not identify, so they are only inherited by a refresh
                // that spoke for every session too.
                if session_ids.is_empty() {
                    if snapshot.context.is_none() {
                        snapshot.context = previous.context.clone();
                    }
                    if snapshot.model.is_none() {
                        snapshot.model = previous.model.clone();
                    }
                }
            }
        }
        prune_session_diagnostics(snapshot, &session_ids);
        self.save(snapshot)
    }

    /// Store the latest statusLine observation without coordinating with a
    /// refresh. The statusLine hook is a latency-sensitive producer; its only
    /// shared-state operation is an atomic last-observation replacement.
    pub fn save_statusline_observation(
        &self,
        provider: Provider,
        mut snapshot: ProviderSnapshot,
        observation: &Value,
    ) -> Result<()> {
        self.ensure()?;
        let destination = self.statusline_observation_path(provider);
        let temporary = self.root.join(format!(
            ".{}.observation.{}.tmp",
            provider.source(),
            std::process::id()
        ));
        Self::with_write_lock(&destination, || {
            let previous = match fs::read(&destination) {
                Ok(bytes) => Self::decode_statusline_observation(provider, &bytes)
                    .ok()
                    .map(|(observation, _)| observation),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error).with_context(|| format!("read {}", destination.display()))
                }
            };
            let session_key = statusline_session_id(observation)
                .map(|session_id| crate::model::private_identifier("session", session_id));
            if let Some(session_key) = session_key.as_deref() {
                if let Some(cache) = snapshot
                    .context
                    .as_mut()
                    .and_then(|context| context.cache.as_mut())
                {
                    cache.session_id = Some(session_key.to_string());
                }
            }
            let previous_snapshot = previous.as_ref().map(|value| &value.snapshot);
            let previous_session_id = previous
                .as_ref()
                .and_then(|value| value.session_key.as_deref());
            let previous_cache = previous_snapshot
                .and_then(|value| value.context.as_ref())
                .and_then(|context| context.cache.as_ref());
            crate::providers::statusline::enrich_cache_session(
                &mut snapshot,
                observation,
                previous_cache,
            );
            merge_preserved_context(
                &mut snapshot,
                previous_snapshot.and_then(|value| value.context.clone()),
                previous_session_id,
                session_key.as_deref(),
            );
            merge_session_models(
                &mut snapshot,
                previous_snapshot,
                previous_session_id,
                session_key.as_deref(),
            );
            if let Some(previous_snapshot) = previous_snapshot {
                for (session_id, context) in &previous_snapshot.session_contexts {
                    snapshot
                        .session_contexts
                        .entry(session_id.clone())
                        .or_insert_with(|| context.clone());
                }
            }
            if let Some(session_id) = session_key.as_deref() {
                if let Some(context) = snapshot.context.clone() {
                    snapshot
                        .session_contexts
                        .insert(session_id.to_string(), context);
                }
            }
            merge_session_windows(&mut snapshot, previous_snapshot, session_key.as_deref());
            let current_session_ids = session_key
                .as_ref()
                .map(|session_id| vec![session_id.clone()])
                .unwrap_or_default();
            prune_session_diagnostics(&mut snapshot, &current_session_ids);
            snapshot.protect_private_fields();
            let saved = StatuslineObservation {
                snapshot,
                session_key,
            };
            let bytes = serde_json::to_vec(&saved).context("serialize statusLine observation")?;
            Self::replace_unlocked(&destination, &temporary, bytes, None).map(|_| ())
        })
    }

    pub fn load_statusline_observation(
        &self,
        provider: Provider,
    ) -> Result<Option<StatuslineObservation>> {
        let path = self.statusline_observation_path(provider);
        if !path.exists() {
            return Ok(None);
        }
        for _ in 0..3 {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let (observation, migrated) = Self::decode_statusline_observation(provider, &bytes)?;
            if !migrated {
                return Ok(Some(observation));
            }
            let temporary = self.root.join(format!(
                ".{}.observation.migration.{}.tmp",
                provider.source(),
                std::process::id()
            ));
            let migrated = serde_json::to_vec(&observation)
                .context("serialize migrated statusLine observation")?;
            if Self::atomic_replace_if_unchanged(&path, &temporary, migrated, &bytes)? {
                return Ok(Some(observation));
            }
        }
        anyhow::bail!(
            "{} changed repeatedly during privacy migration",
            path.display()
        )
    }

    fn decode_statusline_observation(
        provider: Provider,
        bytes: &[u8],
    ) -> Result<(StatuslineObservation, bool)> {
        let stored: StoredStatuslineObservation = serde_json::from_slice(bytes)
            .with_context(|| format!("parse {} observation", provider.source()))?;
        let legacy_payload = stored.payload.is_some();
        let legacy_snapshot = stored.snapshot.clone();
        let stored_session_key = stored.session_key.clone();
        let mut snapshot = stored.snapshot;
        snapshot.protect_private_fields();
        let session_key = stored
            .session_key
            .as_deref()
            .map(|session_id| crate::model::persisted_private_identifier("session", session_id))
            .or_else(|| {
                stored
                    .payload
                    .as_ref()
                    .and_then(statusline_session_id)
                    .map(|session_id| crate::model::private_identifier("session", session_id))
            });
        let observation = StatuslineObservation {
            snapshot,
            session_key,
        };
        let migrated = legacy_payload
            || observation.snapshot != legacy_snapshot
            || observation.session_key != stored_session_key;
        Ok((observation, migrated))
    }

    /// StatusLine payloads may temporarily omit context (before the first
    /// response and immediately after compaction). Keep the last known value.
    /// Quota windows still come from the newest snapshot, except an omitted
    /// 5h/weekly window is restored when it is still current.
    pub fn save_preserving_context(&self, snapshot: ProviderSnapshot) -> Result<()> {
        self.save_preserving_context_for_session(snapshot, None)
    }

    /// Save a statusLine snapshot while matching preserved diagnostics to the
    /// session id from the same stdin payload. This keeps a compacted Claude
    /// session's aggregate offset without carrying it into a new session.
    pub fn save_preserving_context_for_session(
        &self,
        mut snapshot: ProviderSnapshot,
        session_id: Option<&str>,
    ) -> Result<()> {
        snapshot.protect_private_fields();
        let session_id =
            session_id.map(|session_id| crate::model::private_identifier("session", session_id));
        if let Some(session_id) = session_id.as_deref() {
            if let Some(cache) = snapshot
                .context
                .as_mut()
                .and_then(|context| context.cache.as_mut())
            {
                cache.session_id = Some(session_id.to_string());
            }
        }
        // A malformed/temporarily unreadable old snapshot must not prevent a
        // fresh statusLine value from replacing it.
        let previous = self.load(snapshot.provider).ok().flatten();
        let previous_session_id = previous
            .as_ref()
            .and_then(|snapshot| snapshot.context.as_ref())
            .and_then(|context| context.cache.as_ref())
            .and_then(|cache| cache.session_id.as_deref());
        merge_preserved_context(
            &mut snapshot,
            previous
                .as_ref()
                .and_then(|snapshot| snapshot.context.clone()),
            previous_session_id,
            session_id.as_deref(),
        );
        merge_session_models(
            &mut snapshot,
            previous.as_ref(),
            previous_session_id,
            session_id.as_deref(),
        );
        if let Some(previous) = previous.as_ref() {
            for (session_id, context) in &previous.session_contexts {
                snapshot
                    .session_contexts
                    .entry(session_id.clone())
                    .or_insert_with(|| context.clone());
            }
        }
        if let Some(session_id) = session_id.as_deref() {
            if let Some(context) = snapshot.context.clone() {
                snapshot
                    .session_contexts
                    .insert(session_id.to_string(), context);
            }
        }
        merge_session_windows(&mut snapshot, previous.as_ref(), session_id.as_deref());
        let current_session_ids = session_id
            .map(|session_id| vec![session_id])
            .unwrap_or_default();
        prune_session_diagnostics(&mut snapshot, &current_session_ids);
        self.save(&snapshot)
    }

    /// Try to claim a named long-running coordination lock.
    ///
    /// Active-turn refreshers are started by two Herdr events at the same
    /// boundary (and there may be several working providers). A non-blocking
    /// OS lock lets the first global watcher own the poll loop while later
    /// starts exit immediately instead of creating duplicate pollers.
    pub fn try_lock_named(&self, name: &str) -> Result<Option<File>> {
        self.ensure()?;
        let path = self.root.join(name);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("lock {}", path.display())),
        }
    }

    /// Claim a provider refresh lease without making a statusLine or event
    /// caller wait behind another provider's slow I/O.
    pub fn try_lock_provider_refresh(&self, provider: Provider) -> Result<Option<File>> {
        self.try_lock_target_refresh(&BillingTarget::original_four(provider))
    }

    /// Refresh lease for a billing target. Original-four names stay the 0.2
    /// `*.refresh.lock` files; OpenCode Go is scoped to the OpenCode store.
    pub fn try_lock_target_refresh(&self, target: &BillingTarget) -> Result<Option<File>> {
        self.try_lock_named(&format!("{}.refresh.lock", target.cache_identity()))
    }

    pub fn stop_turn_watchers(&self) -> Result<()> {
        self.ensure()?;
        fs::write(
            self.root.join("turn-watch.stop"),
            Self::now_millis().to_string(),
        )
        .context("stop active-turn quota watchers")
    }

    pub fn clear_turn_watcher_stop(&self) -> Result<()> {
        let path = self.root.join("turn-watch.stop");
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("clear active-turn watcher stop marker"),
        }
    }

    pub fn turn_watchers_stopped_after(&self, started_millis: u64) -> Result<bool> {
        let path = self.root.join("turn-watch.stop");
        let Ok(value) = fs::read_to_string(path) else {
            return Ok(false);
        };
        Ok(value
            .trim()
            .parse::<u64>()
            .is_ok_and(|stopped| stopped >= started_millis))
    }

    pub fn settings_apply_pending(&self) -> bool {
        self.root.join(SETTINGS_APPLY_PENDING_FILE).exists()
    }

    pub fn set_settings_apply_pending(&self) -> Result<()> {
        self.ensure()?;
        fs::write(self.root.join(SETTINGS_APPLY_PENDING_FILE), [])
            .context("mark settings configuration pending")
    }

    pub fn clear_settings_apply_pending(&self) -> Result<()> {
        match fs::remove_file(self.root.join(SETTINGS_APPLY_PENDING_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("clear settings configuration pending"),
        }
    }

    /// Return the configured active-turn polling interval.
    ///
    /// An environment override is useful for one-off runs and installation
    /// scripts; the state file is the persistent user setting. Invalid or
    /// out-of-range values deliberately fall back to the safe default.
    pub fn watch_interval_seconds(&self) -> u64 {
        std::env::var(WATCH_INTERVAL_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .and_then(Self::valid_watch_interval)
            .or_else(|| {
                fs::read_to_string(self.watch_interval_path())
                    .ok()
                    .and_then(|value| value.trim().parse().ok())
                    .and_then(Self::valid_watch_interval)
            })
            .unwrap_or(DEFAULT_WATCH_INTERVAL_SECONDS)
    }

    pub fn set_watch_interval_seconds(&self, seconds: u64) -> Result<()> {
        Self::valid_watch_interval(seconds).with_context(|| {
            format!(
                "watch interval must be between {MIN_WATCH_INTERVAL_SECONDS} and {MAX_WATCH_INTERVAL_SECONDS} seconds"
            )
        })?;
        self.ensure()?;
        fs::write(self.watch_interval_path(), seconds.to_string())
            .context("write active-turn watch interval")
    }

    pub fn clear_watch_interval(&self) -> Result<()> {
        match fs::remove_file(self.watch_interval_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove active-turn watch interval"),
        }
    }

    /// Last sidebar layout written by `configure --apply`.
    ///
    /// Invalid files are ignored so a repair never refuses to write rows.
    /// The installer environment and config-dir prefs are resolved by
    /// `configure`, not here.
    pub fn sidebar_layout(&self) -> Option<SidebarLayout> {
        fs::read_to_string(self.sidebar_layout_path())
            .ok()
            .as_deref()
            .and_then(SidebarLayout::parse)
    }

    pub fn set_sidebar_layout(&self, layout: SidebarLayout) -> Result<()> {
        self.ensure()?;
        fs::write(self.sidebar_layout_path(), layout.as_str()).context("write sidebar layout")
    }

    pub fn clear_sidebar_layout(&self) -> Result<()> {
        match fs::remove_file(self.sidebar_layout_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove sidebar layout"),
        }
    }

    pub fn row_gap(&self) -> Option<SidebarRowGap> {
        fs::read_to_string(self.row_gap_path())
            .ok()
            .as_deref()
            .and_then(SidebarRowGap::parse)
    }

    pub fn set_row_gap(&self, gap: SidebarRowGap) -> Result<()> {
        self.ensure()?;
        fs::write(self.row_gap_path(), gap.to_string()).context("write sidebar row gap")
    }

    pub fn clear_row_gap(&self) -> Result<()> {
        match fs::remove_file(self.row_gap_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove sidebar row gap"),
        }
    }

    /// The percentage style every renderer reads.
    ///
    /// This one lives in the state directory rather than only in the plugin
    /// config directory because the Claude/Agy statusLine hooks are launched
    /// by their harness with just `HERDR_PLUGIN_STATE_DIR` set — the config
    /// directory is not injected there, so a preference kept only in it would
    /// be invisible to half the renderers.
    pub fn percent_style(&self) -> Option<PercentStyle> {
        fs::read_to_string(self.quota_percent_path())
            .ok()
            .as_deref()
            .and_then(PercentStyle::parse)
    }

    pub fn set_percent_style(&self, style: PercentStyle) -> Result<()> {
        self.ensure()?;
        fs::write(self.quota_percent_path(), style.as_str()).context("write quota percent style")
    }

    /// The brand-mark set every renderer reads. Same reasoning as
    /// [`Self::percent_style`]: it must be visible to the statusLine hooks.
    pub fn brand_glyphs(&self) -> Option<GlyphSet> {
        fs::read_to_string(self.brand_glyphs_path())
            .ok()
            .as_deref()
            .and_then(GlyphSet::parse)
    }

    pub fn set_brand_glyphs(&self, glyphs: GlyphSet) -> Result<()> {
        self.ensure()?;
        fs::write(self.brand_glyphs_path(), glyphs.as_str()).context("write brand glyph set")
    }

    pub fn clear_brand_glyphs(&self) -> Result<()> {
        match fs::remove_file(self.brand_glyphs_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove brand glyph set"),
        }
    }

    fn brand_glyphs_path(&self) -> PathBuf {
        self.root.join(BRAND_GLYPHS_FILE)
    }

    pub fn clear_percent_style(&self) -> Result<()> {
        match fs::remove_file(self.quota_percent_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove quota percent style"),
        }
    }

    /// The sidebar settings that shaped the rows currently on disk.
    ///
    /// Uninstall needs them to recognise its own work, and the settings pane
    /// needs them to open on what is actually installed.
    pub fn fields(&self) -> Option<FieldSet> {
        fs::read_to_string(self.fields_path())
            .ok()
            .as_deref()
            .and_then(FieldSet::parse)
    }

    pub fn set_fields(&self, fields: FieldSet) -> Result<()> {
        self.ensure()?;
        fs::write(self.fields_path(), fields.as_list()).context("write sidebar fields")
    }

    pub fn clear_fields(&self) -> Result<()> {
        match fs::remove_file(self.fields_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove sidebar fields"),
        }
    }

    pub fn brand_colors(&self) -> Option<BrandColors> {
        fs::read_to_string(self.brand_colors_path())
            .ok()
            .as_deref()
            .and_then(BrandColors::parse)
    }

    pub fn set_brand_colors(&self, colors: BrandColors) -> Result<()> {
        self.ensure()?;
        fs::write(self.brand_colors_path(), colors.as_str()).context("write brand colors")
    }

    pub fn clear_brand_colors(&self) -> Result<()> {
        match fs::remove_file(self.brand_colors_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove brand colors"),
        }
    }

    pub fn agent_order(&self) -> Option<AgentOrder> {
        fs::read_to_string(self.agent_order_path())
            .ok()
            .as_deref()
            .and_then(AgentOrder::parse)
    }

    pub fn set_agent_order(&self, order: AgentOrder) -> Result<()> {
        self.ensure()?;
        fs::write(self.agent_order_path(), order.as_str()).context("write agent order")
    }

    pub fn clear_agent_order(&self) -> Result<()> {
        match fs::remove_file(self.agent_order_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove agent order"),
        }
    }

    pub fn low_quota_alert(&self) -> Option<LowQuotaAlert> {
        fs::read_to_string(self.low_quota_alert_path())
            .ok()
            .as_deref()
            .and_then(LowQuotaAlert::parse)
    }

    pub fn set_low_quota_alert(&self, alert: LowQuotaAlert) -> Result<()> {
        self.ensure()?;
        fs::write(self.low_quota_alert_path(), alert.to_string()).context("write low quota alert")
    }

    pub fn clear_low_quota_alert(&self) -> Result<()> {
        for path in [self.low_quota_alert_path(), self.low_quota_alerted_path()] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("remove low quota alert"),
            }
        }
        Ok(())
    }

    /// Providers that have already been notified and have not recovered above
    /// the threshold since.
    ///
    /// Kept as a set rather than a timestamp so a quota that stays low stays
    /// quiet for as long as it stays low, however many refreshes pass, and
    /// notifies again the moment it drops back after recovering.
    pub fn low_quota_alerted(&self) -> Vec<String> {
        fs::read_to_string(self.low_quota_alerted_path())
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    pub fn set_low_quota_alerted(&self, sources: &[String]) -> Result<()> {
        if sources.is_empty() {
            return match fs::remove_file(self.low_quota_alerted_path()) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context("clear low quota alert state"),
            };
        }
        self.ensure()?;
        fs::write(self.low_quota_alerted_path(), sources.join("\n"))
            .context("write low quota alert state")
    }

    pub fn validate_watch_interval_seconds(seconds: u64) -> Result<u64> {
        Self::valid_watch_interval(seconds).with_context(|| {
            format!(
                "watch interval must be between {MIN_WATCH_INTERVAL_SECONDS} and {MAX_WATCH_INTERVAL_SECONDS} seconds"
            )
        })
    }

    pub fn should_debounce(
        &self,
        provider: Provider,
        now_unix: u64,
        interval_seconds: u64,
    ) -> Result<bool> {
        let Ok(contents) = fs::read_to_string(self.refresh_marker_path(provider)) else {
            return Ok(false);
        };
        let Ok(last) = contents.trim().parse::<u64>() else {
            return Ok(false);
        };
        Ok(now_unix.saturating_sub(last) < interval_seconds)
    }

    pub fn mark_refresh(&self, provider: Provider, now_unix: u64) -> Result<()> {
        self.ensure()?;
        fs::write(self.refresh_marker_path(provider), now_unix.to_string())
            .context("write refresh marker")
    }

    pub fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default()
    }

    pub fn file_mtime_unix(path: &Path) -> Option<u64> {
        fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs())
    }

    pub fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    }

    fn snapshot_path(&self, provider: Provider) -> PathBuf {
        self.root.join(format!("{}.json", provider.source()))
    }

    fn target_snapshot_path(&self, target: &BillingTarget) -> PathBuf {
        self.root.join(format!("{}.json", target.cache_identity()))
    }

    fn target_refresh_marker_path(&self, target: &BillingTarget) -> PathBuf {
        self.root
            .join(format!("{}.refresh", target.cache_identity()))
    }

    fn statusline_observation_path(&self, provider: Provider) -> PathBuf {
        self.root
            .join(format!("{}.observation.json", provider.source()))
    }

    pub(crate) fn atomic_replace(
        destination: &Path,
        temporary: &Path,
        bytes: Vec<u8>,
    ) -> Result<()> {
        Self::atomic_replace_locked(destination, temporary, bytes, None).map(|_| ())
    }

    fn atomic_replace_if_unchanged(
        destination: &Path,
        temporary: &Path,
        bytes: Vec<u8>,
        expected: &[u8],
    ) -> Result<bool> {
        Self::atomic_replace_locked(destination, temporary, bytes, Some(expected))
    }

    fn atomic_replace_locked(
        destination: &Path,
        temporary: &Path,
        bytes: Vec<u8>,
        expected: Option<&[u8]>,
    ) -> Result<bool> {
        Self::with_write_lock(destination, || {
            Self::replace_unlocked(destination, temporary, bytes, expected)
        })
    }

    fn with_write_lock<T>(destination: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let mut lock_path = destination.as_os_str().to_os_string();
        lock_path.push(".write.lock");
        let lock_path = PathBuf::from(lock_path);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("lock {}", lock_path.display()))?;
        action()
    }

    fn replace_unlocked(
        destination: &Path,
        temporary: &Path,
        bytes: Vec<u8>,
        expected: Option<&[u8]>,
    ) -> Result<bool> {
        if let Some(expected) = expected {
            match fs::read(destination) {
                Ok(current) if current == expected => {}
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(error).with_context(|| format!("re-read {}", destination.display()))
                }
            }
        }
        fs::write(temporary, bytes).with_context(|| format!("write {}", temporary.display()))?;
        if let Err(error) = fs::rename(temporary, destination) {
            // Otherwise a failed rename leaves the scratch file behind, and
            // every later refresh adds another one.
            let _ = fs::remove_file(temporary);
            return Err(error).with_context(|| {
                format!(
                    "atomically replace {} with {}",
                    destination.display(),
                    temporary.display()
                )
            });
        }
        Ok(true)
    }

    fn refresh_marker_path(&self, provider: Provider) -> PathBuf {
        self.root.join(format!("{}.refresh", provider.source()))
    }

    fn watch_interval_path(&self) -> PathBuf {
        self.root.join(WATCH_INTERVAL_FILE)
    }

    fn sidebar_layout_path(&self) -> PathBuf {
        self.root.join(SIDEBAR_LAYOUT_FILE)
    }

    fn row_gap_path(&self) -> PathBuf {
        self.root.join(ROW_GAP_FILE)
    }

    fn quota_percent_path(&self) -> PathBuf {
        self.root.join(QUOTA_PERCENT_FILE)
    }

    fn fields_path(&self) -> PathBuf {
        self.root.join(FIELDS_FILE)
    }

    fn brand_colors_path(&self) -> PathBuf {
        self.root.join(BRAND_COLORS_FILE)
    }

    fn agent_order_path(&self) -> PathBuf {
        self.root.join(AGENT_ORDER_FILE)
    }

    fn low_quota_alert_path(&self) -> PathBuf {
        self.root.join(LOW_QUOTA_ALERT_FILE)
    }

    fn low_quota_alerted_path(&self) -> PathBuf {
        self.root.join(LOW_QUOTA_ALERTED_FILE)
    }

    fn valid_watch_interval(seconds: u64) -> Option<u64> {
        (MIN_WATCH_INTERVAL_SECONDS..=MAX_WATCH_INTERVAL_SECONDS)
            .contains(&seconds)
            .then_some(seconds)
    }
}

fn merge_session_models(
    snapshot: &mut ProviderSnapshot,
    previous: Option<&ProviderSnapshot>,
    previous_session_id: Option<&str>,
    session_id: Option<&str>,
) {
    if let Some(previous) = previous {
        for (session_id, model) in &previous.session_models {
            snapshot
                .session_models
                .entry(session_id.clone())
                .or_insert_with(|| model.clone());
        }
    }
    let Some(session_id) = session_id else {
        return;
    };
    if let Some(model) = snapshot.model.as_ref() {
        snapshot
            .session_models
            .insert(session_id.to_string(), model.clone());
    } else if previous_session_id == Some(session_id) {
        if let Some(model) = previous.and_then(|previous| previous.model.as_ref()) {
            snapshot
                .session_models
                .entry(session_id.to_string())
                .or_insert_with(|| model.clone());
        }
    }
}

fn merge_session_windows(
    snapshot: &mut ProviderSnapshot,
    previous: Option<&ProviderSnapshot>,
    session_id: Option<&str>,
) {
    if let Some(previous) = previous {
        for (session_id, windows) in &previous.session_windows {
            snapshot
                .session_windows
                .entry(session_id.clone())
                .or_insert_with(|| windows.clone());
        }
        match session_id {
            Some(session_id) => {
                let previous_windows = previous
                    .session_windows
                    .get(session_id)
                    .map(Vec::as_slice)
                    .or_else(|| {
                        previous
                            .session_windows
                            .is_empty()
                            .then_some(previous.windows.as_slice())
                    });
                if let Some(previous_windows) = previous_windows {
                    if snapshot.windows.is_empty() {
                        for kind in [WindowKind::FiveHour, WindowKind::Weekly] {
                            if let Some(window) = previous_windows
                                .iter()
                                .find(|window| window.kind == kind)
                                .filter(|window| {
                                    window.resets_at.is_some_and(|reset| {
                                        reset.unix_seconds() > snapshot.fetched_at_unix
                                    })
                                })
                            {
                                snapshot.windows.push(window.clone());
                            }
                        }
                    } else {
                        merge_omitted_window_list(
                            &mut snapshot.windows,
                            previous_windows,
                            snapshot.fetched_at_unix,
                        );
                    }
                }
            }
            None => snapshot.merge_omitted_windows(previous),
        }
    }
    if let Some(session_id) = session_id {
        snapshot
            .session_windows
            .insert(session_id.to_string(), snapshot.windows.clone());
    }
}

fn prune_session_diagnostics(snapshot: &mut ProviderSnapshot, current_session_ids: &[String]) {
    prune_session_map(&mut snapshot.session_models, current_session_ids);
    prune_session_map(&mut snapshot.session_contexts, current_session_ids);
    prune_session_map(&mut snapshot.session_windows, current_session_ids);
}

fn prune_session_map<T>(map: &mut BTreeMap<String, T>, current_session_ids: &[String]) {
    while map.len() > MAX_STATUSLINE_SESSIONS {
        let Some(session_id) = map
            .keys()
            .find(|session_id| {
                !current_session_ids
                    .iter()
                    .any(|current| current == *session_id)
            })
            .cloned()
            .or_else(|| map.keys().next().cloned())
        else {
            break;
        };
        map.remove(&session_id);
    }
}

fn statusline_session_id(observation: &Value) -> Option<&str> {
    observation
        .get("session_id")
        .or_else(|| observation.get("sessionId"))
        .or_else(|| observation.get("conversation_id"))
        .or_else(|| observation.get("conversationId"))
        .and_then(Value::as_str)
}

fn merge_preserved_context(
    snapshot: &mut ProviderSnapshot,
    previous: Option<ContextUsage>,
    previous_session_id: Option<&str>,
    session_id: Option<&str>,
) {
    let Some(previous_context) = previous else {
        return;
    };
    let same_session = sessions_match(previous_session_id, session_id);
    match (&mut snapshot.context, previous_context) {
        (None, previous_context) if same_session => {
            snapshot.context = Some(previous_context);
        }
        (None, _) => {}
        (Some(current), previous_context) if current.cache.is_none() && same_session => {
            current.cache = previous_context.cache;
        }
        (Some(current), previous_context) => {
            let Some(current_cache) = current.cache.as_mut() else {
                return;
            };
            let Some(previous_cache) = previous_context.cache.as_ref() else {
                return;
            };
            if same_session {
                if current_cache.session_totals.is_none() {
                    current_cache.session_totals = previous_cache.session_totals.clone();
                }
                if current_cache.transcript_offset == 0 {
                    current_cache.transcript_offset = previous_cache.transcript_offset;
                }
                if current_cache.ttl_seconds.is_none() {
                    current_cache.ttl_seconds = previous_cache.ttl_seconds;
                }
                if current_cache.last_activity_unix.is_none() {
                    current_cache.last_activity_unix = previous_cache.last_activity_unix;
                }
                if current_cache.expires_at_unix.is_none() {
                    current_cache.expires_at_unix = previous_cache.expires_at_unix;
                }
            }
        }
    }
}

fn sessions_match(previous_session_id: Option<&str>, session_id: Option<&str>) -> bool {
    match (previous_session_id, session_id) {
        (Some(previous), Some(current)) => previous == current,
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        BillingTarget, CacheUsage, ContextUsage, Provider, ResetAt, UsageWindow, WindowKind,
    };
    use serde_json::json;
    use tempfile::tempdir;

    fn snapshot() -> ProviderSnapshot {
        ProviderSnapshot::new(
            Provider::Grok,
            vec![UsageWindow::new(WindowKind::Weekly, 42.5, None).unwrap()],
            123,
        )
    }

    #[test]
    fn successful_snapshot_round_trips_through_atomic_cache() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache.save(&snapshot()).unwrap();
        assert_eq!(cache.load(Provider::Grok).unwrap(), Some(snapshot()));
    }

    #[test]
    fn opencode_go_lease_does_not_touch_original_four_cache_files() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let target = BillingTarget::opencode_go();
        let lease = cache.try_lock_target_refresh(&target).unwrap();
        assert!(lease.is_some());
        assert!(directory
            .path()
            .join("opencode-go.opencode-store.refresh.lock")
            .exists());
        for filename in [
            "codex-app-server.json",
            "grok-cli-billing.json",
            "claude-statusline.json",
            "agy-statusline.json",
            "codex-app-server.refresh.lock",
            "grok-cli-billing.refresh.lock",
            "claude-statusline.refresh.lock",
            "agy-statusline.refresh.lock",
            "codex-app-server.refresh",
            "grok-cli-billing.refresh",
            "claude-statusline.refresh",
            "agy-statusline.refresh",
        ] {
            assert!(
                !directory.path().join(filename).exists(),
                "OpenCode lease created {filename}"
            );
        }
    }

    #[test]
    fn original_four_snapshots_use_canonical_0_2_cache_filenames() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        for (provider, filename) in [
            (Provider::Codex, "codex-app-server.json"),
            (Provider::Grok, "grok-cli-billing.json"),
            (Provider::Claude, "claude-statusline.json"),
            (Provider::Agy, "agy-statusline.json"),
        ] {
            cache
                .save(&ProviderSnapshot::new(provider, vec![], 1))
                .unwrap();
            let path = directory.path().join(filename);
            assert!(path.exists(), "missing {filename}");
            let loaded: ProviderSnapshot =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(loaded.provider, provider);
            assert_eq!(loaded.source, provider.source());
            assert_eq!(cache.load(provider).unwrap().unwrap().provider, provider);
        }
    }

    #[test]
    fn legacy_cache_rewrites_private_values_without_retaining_payloads() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache.ensure().unwrap();
        let raw_account = "account-private-123";
        let raw_session = "session-private-456";
        let prompt_preview = "private prompt preview";
        let extra_payload = "unowned extra payload";
        let mut legacy = ProviderSnapshot::new(Provider::Claude, vec![], 1);
        legacy.account_id = Some(raw_account.to_string());
        legacy
            .session_models
            .insert(raw_session.to_string(), "Sonnet".to_string());
        let mut context = ContextUsage::new(12.0)
            .unwrap()
            .with_cache(Some(CacheUsage::from_token_counts(1, 2, 3).unwrap()));
        context.cache.as_mut().unwrap().session_id = Some(raw_session.to_string());
        legacy
            .session_contexts
            .insert(raw_session.to_string(), context);

        let mut legacy_json = serde_json::to_value(&legacy).unwrap();
        legacy_json.as_object_mut().unwrap().insert(
            "session_summaries".to_string(),
            json!({raw_session: prompt_preview}),
        );
        let snapshot_path = cache.snapshot_path(Provider::Claude);
        fs::write(&snapshot_path, serde_json::to_vec(&legacy_json).unwrap()).unwrap();
        let loaded = cache.load(Provider::Claude).unwrap().unwrap();
        assert_eq!(loaded.model_for_session(Some(raw_session)), Some("Sonnet"));
        let rewritten = fs::read_to_string(&snapshot_path).unwrap();
        for private in [raw_account, raw_session, prompt_preview] {
            assert!(!rewritten.contains(private), "cache retained {private}");
        }
        assert!(!rewritten.contains("session_summaries"));

        let observation_path = cache.statusline_observation_path(Provider::Claude);
        fs::write(
            &observation_path,
            serde_json::to_vec(&json!({
                "snapshot": legacy_json,
                "payload": {
                    "session_id": raw_session,
                    "prompt": prompt_preview,
                    "extra": extra_payload
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let loaded = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.snapshot.model_for_session(Some(raw_session)),
            Some("Sonnet")
        );
        let rewritten = fs::read_to_string(&observation_path).unwrap();
        for private in [raw_account, raw_session, prompt_preview, extra_payload] {
            assert!(
                !rewritten.contains(private),
                "observation retained {private}"
            );
        }
        assert!(!rewritten.contains("\"payload\""));
        assert!(!rewritten.contains("session_summaries"));
    }

    #[test]
    fn privacy_migration_never_replaces_newer_bytes() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("snapshot.json");
        let temporary = directory.path().join("snapshot.tmp");
        fs::write(&destination, b"newer").unwrap();
        assert!(!CacheStore::atomic_replace_if_unchanged(
            &destination,
            &temporary,
            b"migrated".to_vec(),
            b"older",
        )
        .unwrap());
        assert_eq!(fs::read(destination).unwrap(), b"newer");
        assert!(!temporary.exists());
    }

    #[test]
    fn privacy_migration_reports_a_rewrite_failure() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache.ensure().unwrap();
        let mut legacy = snapshot();
        legacy.account_id = Some("raw-account".to_string());
        let destination = cache.snapshot_path(Provider::Grok);
        fs::write(&destination, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let mut lock_path = destination.as_os_str().to_os_string();
        lock_path.push(".write.lock");
        fs::create_dir(PathBuf::from(lock_path)).unwrap();
        assert!(cache.load(Provider::Grok).is_err());
        assert!(fs::read_to_string(destination)
            .unwrap()
            .contains("raw-account"));
    }

    #[test]
    fn statusline_observation_preserves_context_when_the_next_payload_omits_it() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let previous = ProviderSnapshot::new(
            Provider::Claude,
            vec![UsageWindow::new(WindowKind::Weekly, 27.0, None).unwrap()],
            1,
        )
        .with_context(Some(ContextUsage::new(23.5).unwrap()));
        cache
            .save_statusline_observation(
                Provider::Claude,
                previous,
                &json!({"session_id": "session-1"}),
            )
            .unwrap();

        let latest = ProviderSnapshot::new(Provider::Claude, vec![], 2);
        cache
            .save_statusline_observation(
                Provider::Claude,
                latest,
                &json!({"session_id": "session-1"}),
            )
            .unwrap();

        let saved = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap();
        assert_eq!(saved.snapshot.windows.len(), 0);
        assert_eq!(saved.snapshot.context.as_ref().unwrap().used_percent, 23.5);
    }

    #[test]
    fn statusline_observations_keep_models_for_multiple_sessions() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(Provider::Claude, vec![], 1)
                    .with_model(Some("Sonnet".to_string())),
                &json!({"session_id": "session-1"}),
            )
            .unwrap();
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(Provider::Claude, vec![], 2)
                    .with_model(Some("Opus".to_string())),
                &json!({"conversation_id": "session-2"}),
            )
            .unwrap();
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(Provider::Claude, vec![], 3),
                &json!({"session_id": "session-2"}),
            )
            .unwrap();

        let saved = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap()
            .snapshot;
        assert_eq!(saved.model_for_session(Some("session-1")), Some("Sonnet"));
        assert_eq!(saved.model_for_session(Some("session-2")), Some("Opus"));
    }

    #[test]
    fn statusline_observations_keep_quota_windows_for_multiple_sessions() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(
                    Provider::Claude,
                    vec![UsageWindow::new(WindowKind::Weekly, 10.0, None).unwrap()],
                    1,
                ),
                &json!({"session_id": "work"}),
            )
            .unwrap();
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(
                    Provider::Claude,
                    vec![UsageWindow::new(WindowKind::Weekly, 90.0, None).unwrap()],
                    2,
                ),
                &json!({"session_id": "personal"}),
            )
            .unwrap();

        let saved = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap()
            .snapshot;
        assert_eq!(
            saved.windows_for_session(Some("work"))[0].used_percent,
            10.0
        );
        assert_eq!(
            saved.windows_for_session(Some("personal"))[0].used_percent,
            90.0
        );
        assert!(saved.windows_for_session(Some("unknown")).is_empty());
    }

    #[test]
    fn empty_statusline_tick_reuses_only_its_own_unexpired_windows() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(
                    Provider::Claude,
                    vec![UsageWindow::new(
                        WindowKind::Weekly,
                        49.0,
                        Some(ResetAt::from_unix_seconds(10_000)),
                    )
                    .unwrap()],
                    1_000,
                ),
                &json!({"session_id": "same-session"}),
            )
            .unwrap();
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(Provider::Claude, vec![], 1_100),
                &json!({"session_id": "same-session"}),
            )
            .unwrap();
        let same = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap();
        assert_eq!(same.snapshot.windows[0].used_percent, 49.0);

        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(Provider::Claude, vec![], 1_200),
                &json!({"session_id": "different-session"}),
            )
            .unwrap();
        let different = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap();
        assert!(different.snapshot.windows.is_empty());
    }

    #[test]
    fn concurrent_statusline_sessions_share_one_read_merge_write_lock() {
        use std::sync::{Arc, Barrier};

        let directory = tempdir().unwrap();
        let cache = Arc::new(CacheStore::new(directory.path()));
        let barrier = Arc::new(Barrier::new(12));
        let threads = (0..12)
            .map(|index| {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    cache
                        .save_statusline_observation(
                            Provider::Claude,
                            ProviderSnapshot::new(Provider::Claude, vec![], index + 1)
                                .with_model(Some(format!("model-{index}"))),
                            &json!({"session_id": format!("session-{index}")}),
                        )
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }

        let saved = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap()
            .snapshot;
        for index in 0..12 {
            assert_eq!(
                saved.model_for_session(Some(&format!("session-{index}"))),
                Some(format!("model-{index}").as_str())
            );
        }
    }

    #[test]
    fn statusline_omitted_five_hour_window_stays_on_the_same_session() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(
                    Provider::Claude,
                    vec![
                        UsageWindow::new(
                            WindowKind::FiveHour,
                            22.0,
                            Some(ResetAt::from_unix_seconds(2_000)),
                        )
                        .unwrap(),
                        UsageWindow::new(
                            WindowKind::Weekly,
                            65.0,
                            Some(ResetAt::from_unix_seconds(10_000)),
                        )
                        .unwrap(),
                    ],
                    1_000,
                ),
                &json!({"session_id": "work"}),
            )
            .unwrap();
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(
                    Provider::Claude,
                    vec![UsageWindow::new(
                        WindowKind::Weekly,
                        90.0,
                        Some(ResetAt::from_unix_seconds(10_000)),
                    )
                    .unwrap()],
                    1_100,
                ),
                &json!({"session_id": "personal"}),
            )
            .unwrap();
        cache
            .save_statusline_observation(
                Provider::Claude,
                ProviderSnapshot::new(
                    Provider::Claude,
                    vec![UsageWindow::new(
                        WindowKind::Weekly,
                        66.0,
                        Some(ResetAt::from_unix_seconds(10_000)),
                    )
                    .unwrap()],
                    1_200,
                ),
                &json!({"session_id": "work"}),
            )
            .unwrap();

        let saved = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap()
            .snapshot;
        assert_eq!(
            saved
                .windows_for_session(Some("work"))
                .iter()
                .find(|window| window.kind == WindowKind::FiveHour)
                .unwrap()
                .used_percent,
            22.0
        );
        assert!(saved
            .windows_for_session(Some("personal"))
            .iter()
            .all(|window| window.kind != WindowKind::FiveHour));
    }

    #[test]
    fn statusline_refresh_preserves_previous_cache_diagnostics_when_current_usage_is_missing() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let context = ContextUsage::new(23.5).unwrap().with_cache(Some(
            CacheUsage::from_token_counts(10, 90, 0)
                .unwrap()
                .with_ttl_estimate(300, 1_000),
        ));
        cache
            .save(&snapshot().with_context(Some(context.clone())))
            .unwrap();

        let latest = snapshot().with_context(Some(ContextUsage::new(24.0).unwrap()));
        cache.save_preserving_context(latest).unwrap();
        let saved_context = cache
            .load(Provider::Grok)
            .unwrap()
            .unwrap()
            .context
            .unwrap();
        assert_eq!(saved_context.used_percent, 24.0);
        assert_eq!(saved_context.cache, context.cache);
    }

    #[test]
    fn statusline_refresh_records_context_for_the_current_session() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let current = snapshot().with_context(Some(ContextUsage::new(12.0).unwrap()));
        cache
            .save_preserving_context_for_session(current, Some("session-1"))
            .unwrap();
        let saved = cache.load(Provider::Grok).unwrap().unwrap();
        assert_eq!(
            saved
                .context_for_session(Some("session-1"))
                .map(|context| context.used_percent),
            Some(12.0)
        );
        assert!(saved.context_for_session(Some("session-1")).is_some());
    }

    #[test]
    fn statusline_refresh_preserves_model_for_the_same_session() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let previous = snapshot()
            .with_model(Some("Sonnet".to_string()))
            .with_context(Some(
                ContextUsage::new(10.0).unwrap().with_cache(Some(
                    CacheUsage::from_token_counts(1, 1, 0)
                        .unwrap()
                        .with_session_totals(None, "session-1", 0),
                )),
            ));
        cache.save(&previous).unwrap();

        let current = snapshot().with_context(Some(ContextUsage::new(12.0).unwrap()));
        cache
            .save_preserving_context_for_session(current, Some("session-1"))
            .unwrap();
        let saved = cache.load(Provider::Grok).unwrap().unwrap();
        assert_eq!(saved.model_for_session(Some("session-1")), Some("Sonnet"));
    }

    #[test]
    fn direct_provider_refresh_preserves_missing_local_session_diagnostics() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut previous = snapshot().with_account_id(Some("account-1".to_string()));
        previous.model = Some("grok-4.6".to_string());
        previous
            .session_models
            .insert("session-1".to_string(), "grok-4.6".to_string());
        previous
            .session_contexts
            .insert("session-1".to_string(), ContextUsage::new(24.0).unwrap());
        cache.save(&previous).unwrap();

        let latest = snapshot().with_account_id(Some("account-1".to_string()));
        cache.save_preserving_diagnostics(latest).unwrap();
        let saved = cache.load(Provider::Grok).unwrap().unwrap();
        assert_eq!(saved.model.as_deref(), Some("grok-4.6"));
        assert_eq!(saved.model_for_session(Some("session-1")), Some("grok-4.6"));
        assert!(saved.context_for_session(Some("session-1")).is_some());
    }

    #[test]
    fn direct_provider_refresh_does_not_leak_global_context_to_a_new_session() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let previous = snapshot().with_context(Some(
            ContextUsage::new(24.0).unwrap().with_cache(Some(
                CacheUsage::from_token_counts(10, 90, 0)
                    .unwrap()
                    .with_session_totals(None, "old-session", 0),
            )),
        ));
        cache.save(&previous).unwrap();

        let mut latest = snapshot();
        cache
            .save_preserving_diagnostics_for_sessions(
                &mut latest,
                &["new-session".to_string()],
                None,
            )
            .unwrap();
        let saved = cache.load(Provider::Grok).unwrap().unwrap();
        assert!(saved.context_for_session(Some("new-session")).is_none());
    }

    /// A Herdr agent event names one pane, so Grok's fetch enriches only that
    /// session. The panes it did not look at must keep their context instead
    /// of losing it and being republished as a cleared token — every such
    /// write risks a visible repaint.
    #[test]
    fn a_single_pane_refresh_keeps_the_other_panes_diagnostics() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut previous = snapshot();
        for session_id in ["pane-a", "pane-b"] {
            previous
                .session_contexts
                .insert(session_id.to_string(), ContextUsage::new(42.0).unwrap());
            previous
                .session_models
                .insert(session_id.to_string(), "grok-4.6".to_string());
        }
        cache.save(&previous).unwrap();

        let mut latest = snapshot();
        latest
            .session_contexts
            .insert("pane-a".to_string(), ContextUsage::new(51.0).unwrap());
        cache
            .save_preserving_diagnostics_for_sessions(&mut latest, &["pane-a".to_string()], None)
            .unwrap();

        let saved = cache.load(Provider::Grok).unwrap().unwrap();
        assert_eq!(
            saved
                .context_for_session(Some("pane-a"))
                .map(|context| context.used_percent),
            Some(51.0),
            "the refreshed session must take the new value"
        );
        assert_eq!(
            saved
                .context_for_session(Some("pane-b"))
                .map(|context| context.used_percent),
            Some(42.0),
            "an untouched session must keep its last known context"
        );
        assert_eq!(saved.model_for_session(Some("pane-b")), Some("grok-4.6"));
    }

    #[test]
    fn direct_provider_diagnostics_remain_bounded() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut previous = snapshot();
        for index in 0..(MAX_STATUSLINE_SESSIONS + 8) {
            let session_id = format!("session-{index}");
            previous
                .session_models
                .insert(session_id.clone(), format!("model-{index}"));
            previous
                .session_contexts
                .insert(session_id, ContextUsage::new((index % 100) as f64).unwrap());
        }
        cache.save(&previous).unwrap();

        cache.save_preserving_diagnostics(snapshot()).unwrap();
        let saved = cache.load(Provider::Grok).unwrap().unwrap();
        assert_eq!(saved.session_models.len(), MAX_STATUSLINE_SESSIONS);
        assert_eq!(saved.session_contexts.len(), MAX_STATUSLINE_SESSIONS);
    }

    fn codex_windows(five_hour: Option<f64>, weekly: f64, fetched_at: u64) -> ProviderSnapshot {
        let mut windows = Vec::new();
        if let Some(used) = five_hour {
            windows.push(
                UsageWindow::new(
                    WindowKind::FiveHour,
                    used,
                    Some(ResetAt::from_unix_seconds(2_000)),
                )
                .unwrap(),
            );
        }
        windows.push(
            UsageWindow::new(
                WindowKind::Weekly,
                weekly,
                Some(ResetAt::from_unix_seconds(10_000)),
            )
            .unwrap(),
        );
        ProviderSnapshot::new(Provider::Codex, windows, fetched_at)
    }

    #[test]
    fn direct_provider_refresh_does_not_restore_five_hour_window_after_account_switch() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .save(
                &codex_windows(Some(80.0), 31.0, 1_000)
                    .with_account_id(Some("account-a".to_string())),
            )
            .unwrap();

        let mut latest =
            codex_windows(None, 12.0, 1_100).with_account_id(Some("account-b".to_string()));
        cache
            .save_preserving_diagnostics_for_sessions(&mut latest, &[], None)
            .unwrap();
        let saved = cache.load(Provider::Codex).unwrap().unwrap();
        assert!(saved.window(WindowKind::FiveHour).is_none());
        assert_eq!(saved.window(WindowKind::Weekly).unwrap().used_percent, 12.0);
    }

    #[test]
    fn unstamped_refresh_does_not_restore_five_hour_window_after_newer_credentials() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache.save(&codex_windows(Some(80.0), 31.0, 1_000)).unwrap();

        let mut latest = codex_windows(None, 12.0, 1_100);
        cache
            .save_preserving_diagnostics_for_sessions(&mut latest, &[], Some(1_050))
            .unwrap();
        let saved = cache.load(Provider::Codex).unwrap().unwrap();
        assert!(saved.window(WindowKind::FiveHour).is_none());
        assert_eq!(saved.window(WindowKind::Weekly).unwrap().used_percent, 12.0);
    }

    #[test]
    fn same_account_still_restores_an_omitted_five_hour_window() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache
            .save(
                &codex_windows(Some(22.0), 65.0, 1_000)
                    .with_account_id(Some("account-a".to_string())),
            )
            .unwrap();

        let mut latest =
            codex_windows(None, 66.0, 1_100).with_account_id(Some("account-a".to_string()));
        cache
            .save_preserving_diagnostics_for_sessions(&mut latest, &[], Some(900))
            .unwrap();
        let saved = cache.load(Provider::Codex).unwrap().unwrap();
        assert_eq!(
            saved.window(WindowKind::FiveHour).unwrap().used_percent,
            22.0
        );
        assert_eq!(saved.window(WindowKind::Weekly).unwrap().used_percent, 66.0);
    }

    #[test]
    fn statusline_refresh_preserves_session_totals_only_for_the_same_session() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let previous_cache = CacheUsage::from_token_counts(10, 90, 0)
            .unwrap()
            .with_ttl_estimate(300, 1_000)
            .with_session_totals(
                crate::model::CacheTotals::from_token_counts(10, 90, 0),
                "session-1",
                512,
            );
        cache
            .save(
                &snapshot().with_context(Some(
                    ContextUsage::new(23.5)
                        .unwrap()
                        .with_cache(Some(previous_cache.clone())),
                )),
            )
            .unwrap();

        let same_session = snapshot().with_context(Some(
            ContextUsage::new(24.0).unwrap().with_cache(Some(
                CacheUsage::from_token_counts(1, 2, 3)
                    .unwrap()
                    .with_session_totals(None, "session-1", 0),
            )),
        ));
        cache
            .save_preserving_context_for_session(same_session, Some("session-1"))
            .unwrap();
        let saved = cache.load(Provider::Grok).unwrap().unwrap();
        let saved_cache = saved.context.unwrap().cache.unwrap();
        assert_eq!(saved_cache.session_totals, previous_cache.session_totals);
        assert_eq!(saved_cache.transcript_offset, 512);
        assert_eq!(saved_cache.ttl_seconds, Some(300));

        let new_session = snapshot().with_context(Some(
            ContextUsage::new(25.0).unwrap().with_cache(Some(
                CacheUsage::from_token_counts(1, 2, 3)
                    .unwrap()
                    .with_session_totals(None, "session-2", 0),
            )),
        ));
        cache
            .save_preserving_context_for_session(new_session, Some("session-2"))
            .unwrap();
        let saved_cache = cache
            .load(Provider::Grok)
            .unwrap()
            .unwrap()
            .context
            .unwrap()
            .cache
            .unwrap();
        assert!(saved_cache.session_totals.is_none());
        assert!(saved_cache.ttl_seconds.is_none());
    }

    #[test]
    fn statusline_new_session_does_not_inherit_previous_cache_diagnostics() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let previous = ProviderSnapshot::new(Provider::Claude, vec![], 1).with_context(Some(
            ContextUsage::new(23.5).unwrap().with_cache(Some(
                CacheUsage::from_token_counts(10, 90, 0)
                    .unwrap()
                    .with_session_totals(
                        crate::model::CacheTotals::from_token_counts(10, 90, 0),
                        "session-1",
                        512,
                    ),
            )),
        ));
        cache
            .save_statusline_observation(
                Provider::Claude,
                previous,
                &json!({"session_id": "session-1"}),
            )
            .unwrap();

        let latest = ProviderSnapshot::new(Provider::Claude, vec![], 2)
            .with_context(Some(ContextUsage::new(0.0).unwrap()));
        cache
            .save_statusline_observation(
                Provider::Claude,
                latest,
                &json!({"session_id": "session-2"}),
            )
            .unwrap();

        let saved = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap()
            .snapshot;
        assert!(saved
            .context_for_session(Some("session-2"))
            .unwrap()
            .cache
            .is_none());
        assert!(saved.context_for_session(Some("session-2")).is_some());
    }

    #[test]
    fn statusline_session_diagnostics_remain_bounded() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        for index in 0..(MAX_STATUSLINE_SESSIONS + 8) {
            let session_id = format!("session-{index}");
            cache
                .save_statusline_observation(
                    Provider::Claude,
                    ProviderSnapshot::new(Provider::Claude, vec![], index as u64)
                        .with_model(Some(format!("model-{index}")))
                        .with_context(Some(ContextUsage::new(0.0).unwrap())),
                    &json!({"session_id": session_id}),
                )
                .unwrap();
        }

        let saved = cache
            .load_statusline_observation(Provider::Claude)
            .unwrap()
            .unwrap()
            .snapshot;
        assert_eq!(saved.session_models.len(), MAX_STATUSLINE_SESSIONS);
        assert_eq!(saved.session_contexts.len(), MAX_STATUSLINE_SESSIONS);
        assert_eq!(saved.session_windows.len(), MAX_STATUSLINE_SESSIONS);
        assert!(saved
            .model_for_session(Some(&format!("session-{}", MAX_STATUSLINE_SESSIONS + 7)))
            .is_some());
    }

    #[test]
    fn saves_record_bounded_history_of_used_percent_per_window() {
        let directory = tempfile::tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let window = |kind, used, at| {
            let mut snapshot = ProviderSnapshot::new(Provider::Codex, vec![], at);
            snapshot.windows = vec![UsageWindow::new(kind, used, None).unwrap()];
            snapshot
        };
        cache
            .save(&window(WindowKind::FiveHour, 10.0, 1_000))
            .unwrap();
        // Within a minute of the last sample: replaces it rather than adding.
        cache
            .save(&window(WindowKind::FiveHour, 11.0, 1_030))
            .unwrap();
        cache
            .save(&window(WindowKind::FiveHour, 12.0, 1_100))
            .unwrap();
        // Older than the last sample: ignored, history stays monotonic.
        cache
            .save(&window(WindowKind::FiveHour, 99.0, 900))
            .unwrap();
        let identity = CacheStore::history_identity(Provider::Codex);
        assert_eq!(
            cache.load_history(identity, WindowKind::FiveHour),
            vec![(1_030, 11.0), (1_100, 12.0)]
        );
        assert!(cache.load_history(identity, WindowKind::Weekly).is_empty());
        assert!(cache
            .load_history("never-seen", WindowKind::FiveHour)
            .is_empty());

        for index in 0..(HISTORY_MAX_SAMPLES as u64 + 50) {
            cache
                .save(&window(WindowKind::Weekly, 1.0, 10_000 + index * 60))
                .unwrap();
        }
        let weekly = cache.load_history(identity, WindowKind::Weekly);
        assert_eq!(weekly.len(), HISTORY_MAX_SAMPLES);
        assert_eq!(weekly.first().unwrap().0, 10_000 + 50 * 60);
        let text = fs::read_to_string(cache.root().join("codex-app-server.history.json")).unwrap();
        assert!(!text.contains("session"), "{text}");
        assert!(!text.contains("model"), "{text}");
    }

    #[test]
    fn missing_cache_is_not_an_error() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        assert_eq!(cache.load(Provider::Claude).unwrap(), None);
    }

    #[test]
    fn statusline_refresh_preserves_an_omitted_five_hour_window() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let previous = ProviderSnapshot::new(
            Provider::Claude,
            vec![
                UsageWindow::new(
                    WindowKind::FiveHour,
                    22.0,
                    Some(ResetAt::from_unix_seconds(2_000)),
                )
                .unwrap(),
                UsageWindow::new(
                    WindowKind::Weekly,
                    65.0,
                    Some(ResetAt::from_unix_seconds(10_000)),
                )
                .unwrap(),
            ],
            1_000,
        );
        cache.save(&previous).unwrap();

        let current = ProviderSnapshot::new(
            Provider::Claude,
            vec![UsageWindow::new(
                WindowKind::Weekly,
                66.0,
                Some(ResetAt::from_unix_seconds(10_000)),
            )
            .unwrap()],
            1_100,
        );
        cache
            .save_preserving_context_for_session(current, Some("session-1"))
            .unwrap();
        let saved = cache.load(Provider::Claude).unwrap().unwrap();
        assert_eq!(
            saved.window(WindowKind::FiveHour).unwrap().used_percent,
            22.0
        );
        assert_eq!(saved.window(WindowKind::Weekly).unwrap().used_percent, 66.0);
        assert_eq!(
            saved
                .windows_for_session(Some("session-1"))
                .iter()
                .find(|window| window.kind == WindowKind::FiveHour)
                .unwrap()
                .used_percent,
            22.0
        );
    }

    #[test]
    fn refresh_marker_debounces_only_within_interval() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache.mark_refresh(Provider::Codex, 100).unwrap();
        assert!(cache.should_debounce(Provider::Codex, 120, 60).unwrap());
        assert!(!cache.should_debounce(Provider::Codex, 161, 60).unwrap());
    }

    #[test]
    fn named_turn_lock_is_non_blocking_and_exclusive() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let first = cache.try_lock_named("codex.turn.lock").unwrap();
        assert!(first.is_some());
        let second = cache.try_lock_named("codex.turn.lock").unwrap();
        assert!(second.is_none());
        drop(first);
        assert!(cache.try_lock_named("codex.turn.lock").unwrap().is_some());
    }

    #[test]
    fn provider_refresh_lease_is_non_blocking_and_scoped_to_one_provider() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let first = cache.try_lock_provider_refresh(Provider::Claude).unwrap();
        assert!(first.is_some());
        assert!(cache
            .try_lock_provider_refresh(Provider::Claude)
            .unwrap()
            .is_none());
        assert!(cache
            .try_lock_provider_refresh(Provider::Agy)
            .unwrap()
            .is_some());
    }

    #[test]
    fn watcher_stop_marker_is_reversible_for_reinstall() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let started_millis = CacheStore::now_millis();
        cache.stop_turn_watchers().unwrap();
        assert!(cache.turn_watchers_stopped_after(started_millis).unwrap());
        cache.clear_turn_watcher_stop().unwrap();
        assert!(!cache.turn_watchers_stopped_after(started_millis).unwrap());
    }

    #[test]
    fn watch_interval_defaults_and_persists_a_safe_custom_value() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        assert_eq!(
            cache.watch_interval_seconds(),
            DEFAULT_WATCH_INTERVAL_SECONDS
        );
        cache.set_watch_interval_seconds(300).unwrap();
        assert_eq!(cache.watch_interval_seconds(), 300);
        cache.clear_watch_interval().unwrap();
        assert_eq!(
            cache.watch_interval_seconds(),
            DEFAULT_WATCH_INTERVAL_SECONDS
        );
    }

    #[test]
    fn watch_interval_rejects_values_that_are_too_short_or_long() {
        assert!(
            CacheStore::validate_watch_interval_seconds(MIN_WATCH_INTERVAL_SECONDS - 1).is_err()
        );
        assert!(
            CacheStore::validate_watch_interval_seconds(MAX_WATCH_INTERVAL_SECONDS + 1).is_err()
        );
        assert!(CacheStore::validate_watch_interval_seconds(MIN_WATCH_INTERVAL_SECONDS).is_ok());
        assert!(CacheStore::validate_watch_interval_seconds(MAX_WATCH_INTERVAL_SECONDS).is_ok());
    }

    #[test]
    fn sidebar_layout_defaults_to_packed_and_persists_stacked() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        assert_eq!(cache.sidebar_layout(), None);
        cache
            .set_sidebar_layout(crate::cli::SidebarLayout::Stacked)
            .unwrap();
        assert_eq!(
            cache.sidebar_layout(),
            Some(crate::cli::SidebarLayout::Stacked)
        );
        cache.clear_sidebar_layout().unwrap();
        assert_eq!(cache.sidebar_layout(), None);
    }

    #[test]
    fn row_gap_persists_flush_and_separated() {
        let directory = tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        assert_eq!(cache.row_gap(), None);
        cache.set_row_gap(SidebarRowGap::FLUSH).unwrap();
        assert_eq!(cache.row_gap(), Some(SidebarRowGap::FLUSH));
        cache.clear_row_gap().unwrap();
        assert_eq!(cache.row_gap(), None);
    }
}
