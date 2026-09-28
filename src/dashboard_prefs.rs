//! Persisted dashboard display choices. Credentials and provider payloads
//! never belong in this file.

use crate::cache::CacheStore;
use crate::model::Provider;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

const FILE: &str = "dashboard-providers.json";
const VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DashboardProvider {
    Claude,
    Codex,
    Grok,
    Agy,
    OpenCode,
    Hermes,
    OpenRouter,
    Omp,
    OpenCodeGo,
}

impl DashboardProvider {
    pub const ALL: [Self; 9] = [
        Self::Claude,
        Self::Codex,
        Self::Grok,
        Self::Agy,
        Self::OpenCode,
        Self::Hermes,
        Self::OpenRouter,
        Self::Omp,
        Self::OpenCodeGo,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Agy => "agy",
            Self::OpenCode => "opencode",
            Self::Hermes => "hermes",
            Self::OpenRouter => "openrouter",
            Self::Omp => "omp",
            Self::OpenCodeGo => "opencode-go",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Grok => "Grok",
            Self::Agy => "Agy",
            Self::OpenCode => "OpenCode",
            Self::Hermes => "Hermes",
            Self::OpenRouter => "OpenRouter",
            Self::Omp => "OMP",
            Self::OpenCodeGo => "OpenCode Go",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.id() == value)
    }

    pub const fn quota_provider(self) -> Option<Provider> {
        Some(match self {
            Self::Claude => Provider::Claude,
            Self::Codex => Provider::Codex,
            Self::Grok => Provider::Grok,
            Self::Agy => Provider::Agy,
            Self::Hermes => Provider::Hermes,
            Self::OpenRouter => Provider::OpenRouter,
            Self::Omp => Provider::Omp,
            Self::OpenCodeGo => Provider::OpenCodeGo,
            Self::OpenCode => return None,
        })
    }

    pub const fn from_quota_provider(provider: Provider) -> Self {
        match provider {
            Provider::Claude => Self::Claude,
            Provider::Codex => Self::Codex,
            Provider::Grok => Self::Grok,
            Provider::Agy => Self::Agy,
            Provider::Hermes => Self::Hermes,
            Provider::OpenRouter => Self::OpenRouter,
            Provider::Omp => Self::Omp,
            Provider::OpenCodeGo => Self::OpenCodeGo,
        }
    }

    pub const fn default_color(self) -> &'static str {
        match self {
            Self::Claude => "#E88461",
            Self::Codex => "#C4D7F5",
            Self::Grok => "#D5D5D8",
            Self::Agy => "#8AB4F8",
            Self::OpenCode | Self::OpenCodeGo => "#E8EDF7",
            Self::Hermes => "#CFD6E4",
            Self::OpenRouter => "#CCFF00",
            Self::Omp => "#AA8EFF",
        }
    }

    pub fn allowed_fields(self) -> &'static [DashboardField] {
        use DashboardField::*;
        match self {
            Self::Claude | Self::Codex | Self::Agy | Self::Omp => {
                &[ShortPercent, ShortReset, LongPercent, LongReset]
            }
            Self::Grok => &[LongPercent, LongReset],
            Self::OpenCodeGo => &[ShortPercent, ShortReset, LongPercent, LongReset],
            Self::OpenCode => &[Tokens, Spend],
            Self::Hermes => &[
                PlanAmount,
                PlanPercent,
                PlanReset,
                TopUpAmount,
                TopUpPercent,
            ],
            Self::OpenRouter => &[CreditsAmount, CreditsPercent],
        }
    }

    fn default_fields(self) -> BTreeSet<DashboardField> {
        self.allowed_fields()
            .iter()
            .copied()
            .filter(|field| !(self == Self::Hermes && *field == DashboardField::PlanAmount))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DashboardField {
    ShortPercent,
    ShortReset,
    LongPercent,
    LongReset,
    Tokens,
    Spend,
    PlanAmount,
    PlanPercent,
    PlanReset,
    TopUpAmount,
    TopUpPercent,
    CreditsAmount,
    CreditsPercent,
}

impl DashboardField {
    pub const fn id(self) -> &'static str {
        match self {
            Self::ShortPercent => "short-percent",
            Self::ShortReset => "short-reset",
            Self::LongPercent => "long-percent",
            Self::LongReset => "long-reset",
            Self::Tokens => "tokens",
            Self::Spend => "spend",
            Self::PlanAmount => "plan-amount",
            Self::PlanPercent => "plan-percent",
            Self::PlanReset => "plan-reset",
            Self::TopUpAmount => "top-up-amount",
            Self::TopUpPercent => "top-up-percent",
            Self::CreditsAmount => "credits-amount",
            Self::CreditsPercent => "credits-percent",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::ShortPercent => "short-window percentage",
            Self::ShortReset => "short-window reset",
            Self::LongPercent => "long-window percentage",
            Self::LongReset => "long-window reset",
            Self::Tokens => "30d tokens",
            Self::Spend => "30d spend",
            Self::PlanAmount => "plan amount",
            Self::PlanPercent => "plan percentage",
            Self::PlanReset => "plan reset",
            Self::TopUpAmount => "top-up amount",
            Self::TopUpPercent => "top-up percentage",
            Self::CreditsAmount => "credits amount",
            Self::CreditsPercent => "credits percentage",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        DashboardProvider::ALL
            .into_iter()
            .flat_map(|provider| provider.allowed_fields().iter().copied())
            .find(|field| field.id() == value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPreference {
    pub provider: DashboardProvider,
    pub show: bool,
    pub color: String,
    pub fields: BTreeSet<DashboardField>,
}

impl ProviderPreference {
    pub fn defaults(provider: DashboardProvider) -> Self {
        Self {
            provider,
            // OpenCode Go is a separate paid plan and is opt-in so a normal
            // OpenCode installation does not grow a permanent N/A row.
            show: provider != DashboardProvider::OpenCodeGo,
            color: provider.default_color().to_string(),
            fields: provider.default_fields(),
        }
    }

    pub fn has(&self, field: DashboardField) -> bool {
        self.fields.contains(&field)
    }
}

/// Display choices that are not about one provider.
///
/// Stored beside the provider rows because they are read by the same
/// renderers and, like them, need no `configure` round trip to take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayPreferences {
    /// Draw a ten-cell meter beside each dashboard window.
    pub bars: bool,
    /// Publish the same meter inside the sidebar's 5h/7d tokens.
    pub sidebar_meter: bool,
    /// Show reset times as a local clock time rather than a countdown.
    pub reset_clock: bool,
}

impl Default for DisplayPreferences {
    fn default() -> Self {
        Self {
            bars: true,
            sidebar_meter: false,
            reset_clock: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardPreferences {
    pub providers: Vec<ProviderPreference>,
    pub display: DisplayPreferences,
}

impl Default for DashboardPreferences {
    fn default() -> Self {
        Self {
            providers: DashboardProvider::ALL
                .into_iter()
                .map(ProviderPreference::defaults)
                .collect(),
            display: DisplayPreferences::default(),
        }
    }
}

impl DashboardPreferences {
    pub fn load(cache: &CacheStore) -> Result<Self> {
        Self::load_internal(cache, true)
    }

    pub fn load_read_only(cache: &CacheStore) -> Result<Self> {
        Self::load_internal(cache, false)
    }

    fn load_internal(cache: &CacheStore, persist_migration: bool) -> Result<Self> {
        let path = cache.root().join(FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        let (preferences, migrated) = match version {
            Some(1) => (
                Self::from_v1(
                    serde_json::from_value(value).context("parse dashboard settings v1")?,
                ),
                true,
            ),
            Some(version) if version == u64::from(VERSION) => (
                Self::from_v2(
                    serde_json::from_value(value).context("parse dashboard settings v2")?,
                ),
                false,
            ),
            Some(version) => anyhow::bail!("unsupported dashboard settings version {version}"),
            None => anyhow::bail!("dashboard settings version is missing"),
        };
        let preferences = preferences.normalized();
        if migrated && persist_migration {
            preferences.save(cache)?;
        }
        Ok(preferences)
    }

    pub fn load_or_default(cache: &CacheStore) -> Self {
        Self::load(cache).unwrap_or_default()
    }

    pub fn save(&self, cache: &CacheStore) -> Result<()> {
        cache.ensure()?;
        let disk = DiskV2 {
            _version: VERSION,
            display: DiskDisplay {
                bars: self.display.bars,
                sidebar_meter: self.display.sidebar_meter,
                reset_clock: self.display.reset_clock,
            },
            providers: self
                .clone()
                .normalized()
                .providers
                .into_iter()
                .map(|preference| DiskProvider {
                    provider: preference.provider.id().to_string(),
                    show: preference.show,
                    color: preference.color,
                    fields: preference
                        .fields
                        .into_iter()
                        .map(|field| field.id().to_string())
                        .collect(),
                })
                .collect(),
        };
        let destination = cache.root().join(FILE);
        let temporary = cache
            .root()
            .join(format!(".{FILE}.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec_pretty(&disk).context("serialize dashboard settings")?;
        CacheStore::atomic_replace(&destination, &temporary, bytes)
    }

    pub fn remove(cache: &CacheStore) -> Result<()> {
        let path = cache.root().join(FILE);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }

    pub fn get(&self, provider: DashboardProvider) -> &ProviderPreference {
        self.providers
            .iter()
            .find(|preference| preference.provider == provider)
            .expect("normalized dashboard settings contain every provider")
    }

    pub fn get_mut(&mut self, provider: DashboardProvider) -> &mut ProviderPreference {
        self.providers
            .iter_mut()
            .find(|preference| preference.provider == provider)
            .expect("normalized dashboard settings contain every provider")
    }

    pub fn move_by(&mut self, index: usize, step: isize) -> usize {
        let next = index
            .saturating_add_signed(step)
            .min(self.providers.len().saturating_sub(1));
        self.providers.swap(index, next);
        next
    }

    fn normalized(mut self) -> Self {
        let mut seen = BTreeSet::new();
        self.providers.retain_mut(|preference| {
            if !seen.insert(preference.provider) {
                return false;
            }
            if !is_valid_color(&preference.color) {
                preference.color = preference.provider.default_color().to_string();
            } else {
                preference.color.make_ascii_uppercase();
            }
            let allowed: BTreeSet<_> = preference
                .provider
                .allowed_fields()
                .iter()
                .copied()
                .collect();
            preference.fields.retain(|field| allowed.contains(field));
            true
        });
        for provider in DashboardProvider::ALL {
            if !seen.contains(&provider) {
                self.providers.push(ProviderPreference::defaults(provider));
            }
        }
        self
    }

    fn from_v2(disk: DiskV2) -> Self {
        Self {
            display: DisplayPreferences {
                bars: disk.display.bars,
                sidebar_meter: disk.display.sidebar_meter,
                reset_clock: disk.display.reset_clock,
            },
            providers: disk
                .providers
                .into_iter()
                .filter_map(|entry| {
                    Some(ProviderPreference {
                        provider: DashboardProvider::parse(&entry.provider)?,
                        show: entry.show,
                        color: entry.color,
                        fields: entry
                            .fields
                            .iter()
                            .filter_map(|field| DashboardField::parse(field))
                            .collect(),
                    })
                })
                .collect(),
        }
    }

    fn from_v1(disk: DiskV1) -> Self {
        let DiskV1 {
            order,
            hidden,
            colors,
            fields,
            ..
        } = disk;
        let mut defaults = Self::default();
        let mut providers = Vec::new();
        for id in order {
            if let Some(provider) = DashboardProvider::parse(&id) {
                let mut preference = defaults.get(provider).clone();
                apply_v1_overrides(&mut preference, &hidden, &colors, &fields);
                providers.push(preference);
            }
        }
        for mut preference in defaults.providers.drain(..) {
            if !providers
                .iter()
                .any(|item| item.provider == preference.provider)
            {
                apply_v1_overrides(&mut preference, &hidden, &colors, &fields);
                providers.push(preference);
            }
        }
        Self {
            providers,
            display: DisplayPreferences::default(),
        }
    }
}

fn apply_v1_overrides(
    preference: &mut ProviderPreference,
    hidden: &BTreeSet<String>,
    colors: &BTreeMap<String, String>,
    fields: &BTreeMap<String, Vec<String>>,
) {
    let id = preference.provider.id();
    preference.show = !hidden.contains(id);
    if let Some(color) = colors.get(id) {
        preference.color.clone_from(color);
    }
    if let Some(saved) = fields.get(id) {
        preference.fields = saved
            .iter()
            .filter_map(|field| DashboardField::parse(field))
            .collect();
    }
}

pub fn is_valid_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
}

pub fn color_rgb(value: &str) -> (u8, u8, u8) {
    if !is_valid_color(value) {
        return (232, 237, 247);
    }
    (
        u8::from_str_radix(&value[1..3], 16).unwrap_or(232),
        u8::from_str_radix(&value[3..5], 16).unwrap_or(237),
        u8::from_str_radix(&value[5..7], 16).unwrap_or(247),
    )
}

#[derive(Debug, Serialize, Deserialize)]
struct DiskV2 {
    #[serde(rename = "version", default = "current_version")]
    _version: u8,
    #[serde(default)]
    display: DiskDisplay,
    #[serde(default)]
    providers: Vec<DiskProvider>,
}

/// Version 2 files written before these existed have no `display` key, and
/// every field defaults so that key can also be partial.
#[derive(Debug, Serialize, Deserialize)]
struct DiskDisplay {
    #[serde(default = "shown")]
    bars: bool,
    #[serde(default)]
    sidebar_meter: bool,
    #[serde(default)]
    reset_clock: bool,
}

impl Default for DiskDisplay {
    fn default() -> Self {
        let defaults = DisplayPreferences::default();
        Self {
            bars: defaults.bars,
            sidebar_meter: defaults.sidebar_meter,
            reset_clock: defaults.reset_clock,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct DiskProvider {
    provider: String,
    #[serde(default = "shown")]
    show: bool,
    color: String,
    #[serde(default)]
    fields: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DiskV1 {
    #[serde(rename = "version")]
    _version: u8,
    #[serde(default)]
    order: Vec<String>,
    #[serde(default)]
    hidden: BTreeSet<String>,
    #[serde(default)]
    colors: BTreeMap<String, String>,
    #[serde(default)]
    fields: BTreeMap<String, Vec<String>>,
}

const fn current_version() -> u8 {
    VERSION
}

const fn shown() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_validate_round_trip_and_migrate_v1() {
        let directory = tempfile::tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut preferences = DashboardPreferences::default();
        assert!(!preferences.get(DashboardProvider::OpenCodeGo).show);
        preferences.get_mut(DashboardProvider::Claude).color = "#a1b2c3".to_string();
        preferences
            .get_mut(DashboardProvider::Hermes)
            .fields
            .insert(DashboardField::CreditsAmount);
        preferences.save(&cache).unwrap();
        let loaded = DashboardPreferences::load(&cache).unwrap();
        assert_eq!(loaded.get(DashboardProvider::Claude).color, "#A1B2C3");
        assert!(!loaded
            .get(DashboardProvider::Hermes)
            .has(DashboardField::CreditsAmount));
        assert!(!loaded
            .get(DashboardProvider::Hermes)
            .has(DashboardField::PlanAmount));
        assert_eq!(
            DashboardProvider::Hermes.allowed_fields(),
            &[
                DashboardField::PlanAmount,
                DashboardField::PlanPercent,
                DashboardField::PlanReset,
                DashboardField::TopUpAmount,
                DashboardField::TopUpPercent,
            ]
        );

        fs::write(
            cache.root().join(FILE),
            br##"{"version":1,"order":["grok","claude"],"hidden":["grok","openrouter"],"colors":{"claude":"#123456","openrouter":"#010203"},"fields":{"grok":["long-percent","credits-amount"]}}"##,
        )
        .unwrap();
        let migrated = DashboardPreferences::load(&cache).unwrap();
        assert_eq!(migrated.providers[0].provider, DashboardProvider::Grok);
        assert!(!migrated.providers[0].show);
        assert_eq!(migrated.get(DashboardProvider::Claude).color, "#123456");
        assert_eq!(
            migrated.get(DashboardProvider::Grok).fields,
            [DashboardField::LongPercent].into_iter().collect()
        );
        assert!(!migrated.get(DashboardProvider::OpenRouter).show);
        assert_eq!(migrated.get(DashboardProvider::OpenRouter).color, "#010203");
        let migrated_disk: serde_json::Value =
            serde_json::from_slice(&fs::read(cache.root().join(FILE)).unwrap()).unwrap();
        assert_eq!(migrated_disk["version"], VERSION);

        fs::write(cache.root().join(FILE), br#"{"version":99,"providers":[]}"#).unwrap();
        assert!(DashboardPreferences::load(&cache).is_err());
        assert_eq!(
            DashboardProvider::OpenCodeGo.allowed_fields(),
            &[
                DashboardField::ShortPercent,
                DashboardField::ShortReset,
                DashboardField::LongPercent,
                DashboardField::LongReset,
            ]
        );
    }

    #[test]
    fn display_choices_round_trip_and_default_when_the_key_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        let mut preferences = DashboardPreferences::default();
        assert!(preferences.display.bars);
        assert!(!preferences.display.sidebar_meter);
        assert!(!preferences.display.reset_clock);
        preferences.display.bars = false;
        preferences.display.sidebar_meter = true;
        preferences.display.reset_clock = true;
        preferences.save(&cache).unwrap();
        let loaded = DashboardPreferences::load(&cache).unwrap();
        assert_eq!(loaded.display, preferences.display);

        fs::write(cache.root().join(FILE), br#"{"version":2,"providers":[]}"#).unwrap();
        let legacy = DashboardPreferences::load(&cache).unwrap();
        assert_eq!(legacy.display, DisplayPreferences::default());
    }

    #[test]
    fn color_editing_accepts_only_exact_rgb_hex() {
        for valid in ["#000000", "#aBc123", "#FFFFFF"] {
            assert!(is_valid_color(valid), "{valid}");
        }
        for invalid in ["", "112233", "#123", "#1234567", "#GG0000"] {
            assert!(!is_valid_color(invalid), "{invalid}");
        }
        assert_eq!(color_rgb("#A1B2C3"), (0xA1, 0xB2, 0xC3));
    }

    #[test]
    fn read_only_load_parses_v1_without_migrating_its_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let cache = CacheStore::new(directory.path());
        cache.ensure().unwrap();
        let path = cache.root().join(FILE);
        let original = br##"{"version":1,"order":["codex"],"colors":{"codex":"#123456"}}"##;
        fs::write(&path, original).unwrap();
        let loaded = DashboardPreferences::load_read_only(&cache).unwrap();
        assert_eq!(loaded.get(DashboardProvider::Codex).color, "#123456");
        assert_eq!(fs::read(path).unwrap(), original);
    }
}
