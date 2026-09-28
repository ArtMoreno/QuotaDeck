use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Exact session-id lookup. Never a full-table scan.
const SESSION_BY_ID: &str = "SELECT id FROM session WHERE id = ?1 LIMIT 1";
/// Bounded same-session providerID lookup. Not a spend scan.
const MESSAGE_DATA_FOR_SESSION: &str =
    "SELECT data FROM message WHERE session_id = ?1 ORDER BY time_created DESC LIMIT 8";
const MESSAGE_DATA_SINCE: &str = "SELECT data FROM message WHERE time_created >= ?1";
/// OpenCode 2 exact session-id lookup. `model` is the session's JSON
/// `{"id","providerID"}`, used only until the session has an assistant reply.
const SESSION_BY_ID_V2: &str = "SELECT model FROM session_v2 WHERE id = ?1 LIMIT 1";
/// OpenCode 2 bounded same-session lookup. Only assistant rows carry a model;
/// `seq` is the per-session order and is indexed with `session_id` and `type`.
const ASSISTANT_DATA_FOR_SESSION_V2: &str = "SELECT data FROM session_message \
     WHERE session_id = ?1 AND type = 'assistant' ORDER BY seq DESC LIMIT 8";
/// OpenCode 2 moved messages to `session_message` and records the role in a
/// `type` column instead of inside `data`; the token and cost fields kept
/// their shape.
const ASSISTANT_DATA_SINCE_V2: &str =
    "SELECT data FROM session_message WHERE type = 'assistant' AND time_created >= ?1";
const MAX_MODELS_BYTES: u64 = 8 * 1024 * 1024;
const THIRTY_DAYS_SECONDS: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Api { has_secret: bool },
    Oauth,
    WellKnown { has_secret: bool },
}

impl CredentialKind {
    fn has_secret(self) -> bool {
        match self {
            Self::Api { has_secret } | Self::WellKnown { has_secret } => has_secret,
            Self::Oauth => true,
        }
    }

    fn is_api_like(self) -> bool {
        matches!(self, Self::Api { .. } | Self::WellKnown { .. })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthMap {
    entries: BTreeMap<String, CredentialKind>,
}

impl AuthMap {
    pub fn get(&self, provider_id: &str) -> Option<CredentialKind> {
        self.entries.get(&provider_id.to_ascii_lowercase()).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthReadError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvidence {
    pub session_id: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub context_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLookup {
    Found(SessionEvidence),
    Missing,
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalUsage {
    pub tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone)]
pub struct OpenCodePaths {
    pub auth: PathBuf,
    pub db: PathBuf,
    pub models: PathBuf,
}

impl OpenCodePaths {
    pub fn from_env() -> Option<Self> {
        let dir = opencode_data_dir()?;
        let cache = opencode_cache_dir()?;
        Some(Self {
            auth: dir.join("auth.json"),
            db: dir.join("opencode.db"),
            models: cache.join("models.json"),
        })
    }

    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            auth: dir.join("auth.json"),
            db: dir.join("opencode.db"),
            models: dir.join("models.json"),
        }
    }
}

fn opencode_cache_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg).join("opencode"));
    }
    // OpenCode honours XDG on every platform, but a Windows install that was
    // never given those variables lands in %LOCALAPPDATA%. Prefer whichever
    // directory actually exists so both layouts are read.
    #[cfg(windows)]
    if let Some(local) = crate::platform::local_data_dir() {
        let candidate = local.join("opencode");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    Some(
        crate::platform::home_dir_opt()?
            .join(".cache")
            .join("opencode"),
    )
}

fn opencode_data_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("opencode"));
    }
    // OpenCode uses %LOCALAPPDATA% on Windows when XDG_DATA_HOME is unset.
    #[cfg(windows)]
    if let Some(local) = crate::platform::local_data_dir() {
        let candidate = local.join("opencode");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    Some(
        crate::platform::home_dir_opt()?
            .join(".local")
            .join("share")
            .join("opencode"),
    )
}

pub fn env_go_key_present() -> bool {
    std::env::var_os("OPENCODE_API_KEY").is_some_and(|value| !value.is_empty())
}

/// The Go subscription key itself, for the one caller that must send it.
///
/// [`AuthMap`] deliberately records only whether a secret exists, so the value
/// never travels with the parsed credential map. This reads it on demand and
/// hands back an owned string the caller drops as soon as the request is made.
/// `OPENCODE_API_KEY` wins, matching how OpenCode itself resolves the key.
pub fn go_key(paths: &OpenCodePaths) -> Option<String> {
    if let Some(key) = std::env::var("OPENCODE_API_KEY")
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    {
        return Some(key);
    }
    let bytes = fs::read(&paths.auth).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let key = value
        .get("opencode-go")?
        .get("key")?
        .as_str()?
        .trim()
        .to_string();
    (!key.is_empty()).then_some(key)
}

pub fn read_auth(paths: &OpenCodePaths) -> Result<AuthMap, AuthReadError> {
    read_auth_file(&paths.auth)
}

pub fn lookup_session(paths: &OpenCodePaths, session_id: &str) -> SessionLookup {
    lookup_session_db(&paths.db, session_id)
}

fn read_auth_file(path: &Path) -> Result<AuthMap, AuthReadError> {
    let bytes = fs::read(path).map_err(|_| AuthReadError)?;
    parse_auth_json(&bytes)
}

pub fn parse_auth_json(bytes: &[u8]) -> Result<AuthMap, AuthReadError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| AuthReadError)?;
    let object = value.as_object().ok_or(AuthReadError)?;
    let mut entries = BTreeMap::new();
    for (provider_id, entry) in object {
        let Some(kind) = credential_kind(entry) else {
            continue;
        };
        entries.insert(provider_id.to_ascii_lowercase(), kind);
    }
    Ok(AuthMap { entries })
}

fn credential_kind(entry: &Value) -> Option<CredentialKind> {
    let object = entry.as_object()?;
    let kind = object.get("type").and_then(Value::as_str)?;
    match kind {
        "api" => Some(CredentialKind::Api {
            has_secret: non_empty_secret(object.get("key")),
        }),
        "wellknown" => Some(CredentialKind::WellKnown {
            has_secret: non_empty_secret(object.get("token")),
        }),
        "oauth" => Some(CredentialKind::Oauth),
        _ => None,
    }
}

fn non_empty_secret(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|secret| !secret.is_empty())
}

fn lookup_session_db(path: &Path, session_id: &str) -> SessionLookup {
    if session_id.is_empty() {
        return SessionLookup::Missing;
    }
    let Ok(connection) = open_readonly(path) else {
        return SessionLookup::Unreadable;
    };
    let (Ok(has_v1), Ok(has_v2)) = (
        table_exists(&connection, "session"),
        table_exists(&connection, "session_v2"),
    ) else {
        return SessionLookup::Unreadable;
    };
    if !has_v1 && !has_v2 {
        return SessionLookup::Unreadable;
    }
    // A store carrying both schemas is checked in the old one first; a
    // session id lives in exactly one of them.
    let evidence = if has_v1 {
        match session_exists(&connection, session_id) {
            Ok(true) => Some(session_evidence(&connection, session_id)),
            Ok(false) => None,
            Err(_) => return SessionLookup::Unreadable,
        }
    } else {
        None
    };
    let evidence = match evidence {
        Some(evidence) => evidence,
        None if has_v2 => match session_model_v2(&connection, session_id) {
            Ok(Some(session_model)) => session_evidence_v2(&connection, session_id, session_model),
            Ok(None) => return SessionLookup::Missing,
            Err(_) => return SessionLookup::Unreadable,
        },
        None => return SessionLookup::Missing,
    };
    match evidence {
        Ok((provider_id, model_id, context_tokens)) => SessionLookup::Found(SessionEvidence {
            session_id: session_id.to_string(),
            provider_id,
            model_id,
            context_tokens,
        }),
        Err(_) => SessionLookup::Unreadable,
    }
}

fn table_exists(connection: &Connection, name: &str) -> rusqlite::Result<bool> {
    let mut statement =
        connection.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    let mut rows = statement.query([name])?;
    Ok(rows.next()?.is_some())
}

fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

/// OpenCode's own local 30-day usage, matching the token and cost fields used
/// by `opencode stats`. No prompt or response content leaves this process.
pub fn local_usage_30d(paths: &OpenCodePaths, now_unix: u64) -> Option<LocalUsage> {
    let cutoff_millis = now_unix
        .saturating_sub(THIRTY_DAYS_SECONDS)
        .checked_mul(1_000)
        .and_then(|value| i64::try_from(value).ok())?;
    let connection = open_readonly(&paths.db).ok()?;
    let v2 = connection.prepare(MESSAGE_DATA_SINCE).is_err();
    let mut statement = connection
        .prepare(if v2 {
            ASSISTANT_DATA_SINCE_V2
        } else {
            MESSAGE_DATA_SINCE
        })
        .ok()?;
    let rows = statement
        .query_map([cutoff_millis], |row| row.get::<_, String>(0))
        .ok()?;
    let mut usage = LocalUsage {
        tokens: 0,
        cost_usd: 0.0,
    };
    let mut found = false;
    for data in rows {
        let value: Value = serde_json::from_str(&data.ok()?).ok()?;
        if !v2 && value.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        found = true;
        let tokens = value.get("tokens").unwrap_or(&Value::Null);
        let cache = tokens.get("cache").unwrap_or(&Value::Null);
        usage.tokens = usage
            .tokens
            .saturating_add(token(tokens, "input"))
            .saturating_add(token(tokens, "output"))
            .saturating_add(token(tokens, "reasoning"))
            .saturating_add(token(cache, "read"))
            .saturating_add(token(cache, "write"));
        usage.cost_usd += value.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
        if !usage.cost_usd.is_finite() || usage.cost_usd < 0.0 {
            return None;
        }
    }
    found.then_some(usage)
}

fn session_exists(connection: &Connection, session_id: &str) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(SESSION_BY_ID)?;
    let mut rows = statement.query([session_id])?;
    Ok(rows.next()?.is_some())
}

/// A `(provider id, model id)` pair read from a message or session.
type Route = (String, Option<String>);
type Evidence = (Option<String>, Option<String>, Option<u64>);

fn session_evidence(connection: &Connection, session_id: &str) -> rusqlite::Result<Evidence> {
    let mut statement = connection.prepare(MESSAGE_DATA_FOR_SESSION)?;
    let mut rows = statement.query([session_id])?;
    let mut identity = None;
    while let Some(row) = rows.next()? {
        let data: String = row.get(0)?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let message_identity = provider_from_message(&value);
        if identity.is_none() {
            identity.clone_from(&message_identity);
        }
        let is_assistant = value.get("role").and_then(Value::as_str) == Some("assistant");
        let context_tokens = is_assistant.then(|| context_tokens(&value)).flatten();
        if let (Some((provider_id, model_id)), Some(context_tokens)) =
            (message_identity, context_tokens)
        {
            if identity.as_ref() == Some(&(provider_id, model_id)) {
                let (provider_id, model_id) = identity.unwrap();
                return Ok((Some(provider_id), model_id, Some(context_tokens)));
            }
        }
    }
    let (provider_id, model_id) = identity.unzip();
    Ok((provider_id, model_id.flatten(), None))
}

/// `session_v2.model` for an exact id: `Ok(None)` when the session is absent,
/// `Ok(Some(None))` when it exists without a readable model.
fn session_model_v2(
    connection: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<Option<Route>>> {
    let mut statement = connection.prepare(SESSION_BY_ID_V2)?;
    let mut rows = statement.query([session_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let model = row
        .get::<_, Option<String>>(0)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|model| provider_from_model(&model));
    Ok(Some(model))
}

/// OpenCode 2 keeps the role in the row's `type` column and the route under
/// `data.model` as `{"id","providerID","variant"}`. The latest assistant reply
/// names the route actually used; the session's own model is the fallback for
/// a session that has not replied yet.
fn session_evidence_v2(
    connection: &Connection,
    session_id: &str,
    session_model: Option<Route>,
) -> rusqlite::Result<Evidence> {
    let mut statement = connection.prepare(ASSISTANT_DATA_FOR_SESSION_V2)?;
    let mut rows = statement.query([session_id])?;
    let mut identity = None;
    while let Some(row) = rows.next()? {
        let data: String = row.get(0)?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let message_identity = value.get("model").and_then(provider_from_model);
        if identity.is_none() {
            identity.clone_from(&message_identity);
        }
        if message_identity.is_some() && message_identity == identity {
            if let Some(context_tokens) = context_tokens(&value) {
                let (provider_id, model_id) = identity.unzip();
                return Ok((provider_id, model_id.flatten(), Some(context_tokens)));
            }
        }
    }
    let (provider_id, model_id) = identity.or(session_model).unzip();
    Ok((provider_id, model_id.flatten(), None))
}

fn provider_from_message(value: &Value) -> Option<Route> {
    let provider_id = string_field(value, "providerID")
        .or_else(|| {
            value
                .get("model")
                .and_then(|model| string_field(model, "providerID"))
        })?
        .trim()
        .to_string();
    if provider_id.is_empty() {
        return None;
    }
    let model_id = string_field(value, "modelID").or_else(|| {
        value
            .get("model")
            .and_then(|model| string_field(model, "modelID"))
    });
    Some((provider_id, model_id))
}

/// An OpenCode 2 model object: `{"id": <model>, "providerID": <provider>}`.
fn provider_from_model(model: &Value) -> Option<Route> {
    Some((
        string_field(model, "providerID")?,
        string_field(model, "id"),
    ))
}

fn context_tokens(value: &Value) -> Option<u64> {
    let tokens = value.get("tokens")?;
    let output = token(tokens, "output");
    if output == 0 {
        return None;
    }
    let cache = tokens.get("cache").unwrap_or(&Value::Null);
    Some(
        token(tokens, "input")
            .saturating_add(output)
            .saturating_add(token(tokens, "reasoning"))
            .saturating_add(token(cache, "read"))
            .saturating_add(token(cache, "write")),
    )
}

fn token(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

pub fn model_context_window(
    paths: &OpenCodePaths,
    provider_id: &str,
    model_id: &str,
) -> Option<u64> {
    let mut bytes = Vec::new();
    fs::File::open(&paths.models)
        .ok()?
        .take(MAX_MODELS_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_MODELS_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get(provider_id)?
        .get("models")?
        .get(model_id)?
        .get("limit")?
        .get("context")?
        .as_u64()
        .filter(|window| *window > 0)
}

fn string_field(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The only OpenCode subscription route. Upstream writes exactly this id for
/// the Go plan; OpenCode Zen (`opencode`) is pay-per-token and is not one.
fn is_approved_go_provider(provider_id: &str) -> bool {
    provider_id.trim().eq_ignore_ascii_case("opencode-go")
}

fn go_credential_approved(kind: CredentialKind, env_go_key_present: bool) -> bool {
    match kind {
        CredentialKind::Api { has_secret } | CredentialKind::WellKnown { has_secret } => {
            has_secret || env_go_key_present
        }
        CredentialKind::Oauth => false,
    }
}

pub fn classify_opencode(
    lookup: SessionLookup,
    auth: Result<&AuthMap, AuthReadError>,
    env_go_key_present: bool,
) -> crate::model::Resolution {
    use crate::model::{BillingTarget, Resolution};

    let Ok(auth) = auth else {
        return Resolution::Indeterminate;
    };
    let SessionLookup::Found(session) = lookup else {
        return Resolution::Indeterminate;
    };
    let Some(provider_id) = session.provider_id.as_deref() else {
        return Resolution::Indeterminate;
    };
    let credential = auth.get(provider_id);

    if is_approved_go_provider(provider_id) {
        let approved = match credential {
            Some(kind) => go_credential_approved(kind, env_go_key_present),
            None => env_go_key_present,
        };
        return if approved {
            Resolution::Subscription(BillingTarget::opencode_go())
        } else {
            Resolution::Indeterminate
        };
    }

    // For any other backend, an API-style key filed in OpenCode's own auth.json
    // under that exact provider id is proof the session pays per token, or
    // through a plan this plugin cannot read. Either way it owns no quota here,
    // so stale quota is cleared once. OAuth logins, missing credentials, and
    // unrecognised entry shapes stay Indeterminate and keep prior metadata.
    match credential {
        Some(kind) if kind.is_api_like() && kind.has_secret() => Resolution::NoSubscription,
        _ => Resolution::Indeterminate,
    }
}

#[cfg(test)]
pub(crate) fn write_fixture_db(path: &Path, rows: &[(&str, &str)]) -> rusqlite::Result<()> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL DEFAULT 'proj',
            slug TEXT NOT NULL DEFAULT 's',
            directory TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL DEFAULT 't',
            version TEXT NOT NULL DEFAULT '1',
            time_created INTEGER NOT NULL DEFAULT 1,
            time_updated INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );",
    )?;
    for (index, (session_id, data)) in rows.iter().enumerate() {
        connection.execute(
            "INSERT INTO session (id) VALUES (?1)
             ON CONFLICT(id) DO NOTHING",
            [*session_id],
        )?;
        connection.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            rusqlite::params![format!("msg_{index}"), *session_id, index as i64 + 1, *data],
        )?;
    }
    Ok(())
}

/// An OpenCode 2 store: `session_v2` plus `session_message`, with the role in
/// the `type` column. Rows are `(session_id, type, data)` in `seq` order; each
/// session is created with a NULL `model`.
#[cfg(test)]
pub(crate) fn write_fixture_db_v2(
    path: &Path,
    rows: &[(&str, &str, &str)],
) -> rusqlite::Result<()> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE session_v2 (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL DEFAULT 'proj',
            slug TEXT NOT NULL DEFAULT 's',
            directory TEXT NOT NULL DEFAULT '',
            version TEXT NOT NULL DEFAULT '2',
            cost REAL DEFAULT 0 NOT NULL,
            tokens_input INTEGER DEFAULT 0 NOT NULL,
            tokens_output INTEGER DEFAULT 0 NOT NULL,
            model TEXT,
            time_created INTEGER NOT NULL DEFAULT 1,
            time_updated INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE session_message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            type TEXT NOT NULL,
            seq INTEGER NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );
        CREATE UNIQUE INDEX session_message_session_seq_idx
            ON session_message (session_id, seq);
        CREATE INDEX session_message_session_type_seq_idx
            ON session_message (session_id, type, seq);",
    )?;
    for (index, (session_id, kind, data)) in rows.iter().enumerate() {
        connection.execute(
            "INSERT INTO session_v2 (id) VALUES (?1)
             ON CONFLICT(id) DO NOTHING",
            [*session_id],
        )?;
        connection.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?4, ?4, ?5)",
            rusqlite::params![format!("msg_{index}"), *session_id, *kind, index as i64 + 1, *data],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn exact_go_session_reads_provider_from_bounded_message_lookup() {
        let directory = tempdir().unwrap();
        let db = directory.path().join("opencode.db");
        write_fixture_db(
            &db,
            &[(
                "ses_go",
                r#"{"role":"assistant","providerID":"opencode-go","modelID":"kimi-k2.5"}"#,
            )],
        )
        .unwrap();
        let paths = OpenCodePaths {
            auth: directory.path().join("auth.json"),
            db,
            models: directory.path().join("models.json"),
        };
        match lookup_session(&paths, "ses_go") {
            SessionLookup::Found(session) => {
                assert_eq!(session.provider_id.as_deref(), Some("opencode-go"));
                assert_eq!(session.model_id.as_deref(), Some("kimi-k2.5"));
                assert_eq!(session.context_tokens, None);
            }
            other => panic!("expected found session, got {other:?}"),
        }
    }

    #[test]
    fn missing_session_is_missing_even_when_auth_has_one_key() {
        let directory = tempdir().unwrap();
        let db = directory.path().join("opencode.db");
        write_fixture_db(
            &db,
            &[(
                "ses_go",
                r#"{"role":"assistant","providerID":"opencode-go","modelID":"kimi-k2.5"}"#,
            )],
        )
        .unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        assert_eq!(lookup_session(&paths, "ses_absent"), SessionLookup::Missing);
    }

    #[test]
    fn malformed_database_is_unreadable() {
        let directory = tempdir().unwrap();
        let db = directory.path().join("opencode.db");
        fs::write(&db, b"this is not a sqlite database").unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        assert_eq!(lookup_session(&paths, "ses_go"), SessionLookup::Unreadable);
    }

    #[test]
    fn the_go_key_is_read_on_demand_and_not_kept_in_the_credential_map() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        fs::write(
            &paths.auth,
            br#"{"opencode-go":{"type":"api","key":"go_secret"},"anthropic":{"type":"api","key":"other"}}"#,
        )
        .unwrap();
        assert_eq!(go_key(&paths).as_deref(), Some("go_secret"));
        let auth = read_auth(&paths).unwrap();
        assert!(!format!("{auth:?}").contains("go_secret"));

        fs::write(
            &paths.auth,
            br#"{"anthropic":{"type":"api","key":"other"}}"#,
        )
        .unwrap();
        assert_eq!(go_key(&paths), None);
        fs::write(&paths.auth, br#"{"opencode-go":{"type":"api","key":"  "}}"#).unwrap();
        assert_eq!(go_key(&paths), None);
    }

    #[test]
    fn malformed_auth_is_an_error() {
        assert!(parse_auth_json(b"{not json").is_err());
        assert!(parse_auth_json(b"[1]").is_err());
    }

    #[test]
    fn auth_parser_records_kind_without_keeping_secrets() {
        let auth = parse_auth_json(
            br#"{
                "opencode-go": {"type":"api","key":"placeholder"},
                "anthropic": {"type":"api","key":"placeholder"}
            }"#,
        )
        .unwrap();
        assert_eq!(
            auth.get("opencode-go"),
            Some(CredentialKind::Api { has_secret: true })
        );
        assert_eq!(
            format!("{:?}", auth.get("opencode-go")),
            "Some(Api { has_secret: true })"
        );
        assert!(!format!("{auth:?}").contains("placeholder"));
    }

    #[test]
    fn user_message_model_object_is_accepted() {
        let directory = tempdir().unwrap();
        let db = directory.path().join("opencode.db");
        write_fixture_db(
            &db,
            &[(
                "ses_go",
                r#"{"role":"user","model":{"providerID":"opencode-go","modelID":"glm-5.2"}}"#,
            )],
        )
        .unwrap();
        match lookup_session_db(&db, "ses_go") {
            SessionLookup::Found(session) => {
                assert_eq!(session.provider_id.as_deref(), Some("opencode-go"));
            }
            other => panic!("expected found, got {other:?}"),
        }
    }

    #[test]
    fn latest_completed_assistant_matches_opencode_context_math() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        write_fixture_db(
            &paths.db,
            &[
                (
                    "ses_context",
                    r#"{"role":"assistant","providerID":"opencode","modelID":"big-pickle","tokens":{"input":100,"output":10,"reasoning":5,"cache":{"read":20,"write":30}}}"#,
                ),
                (
                    "ses_context",
                    r#"{"role":"assistant","providerID":"opencode","modelID":"big-pickle","tokens":{"input":999,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
                ),
            ],
        )
        .unwrap();
        match lookup_session(&paths, "ses_context") {
            SessionLookup::Found(session) => {
                assert_eq!(session.provider_id.as_deref(), Some("opencode"));
                assert_eq!(session.model_id.as_deref(), Some("big-pickle"));
                assert_eq!(session.context_tokens, Some(165));
            }
            other => panic!("expected found session, got {other:?}"),
        }
    }

    #[test]
    fn local_usage_matches_opencode_stats_fields_for_the_last_30_days() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        write_fixture_db(
            &paths.db,
            &[
                (
                    "ses_recent",
                    r#"{"role":"assistant","tokens":{"input":100,"output":10,"reasoning":5,"cache":{"read":20,"write":30}},"cost":1.25}"#,
                ),
                ("ses_recent", r#"{"role":"user","content":"ignored"}"#),
                (
                    "ses_old",
                    r#"{"role":"assistant","tokens":{"input":999,"output":1},"cost":9.0}"#,
                ),
            ],
        )
        .unwrap();
        let connection = Connection::open(&paths.db).unwrap();
        connection
            .execute(
                "UPDATE message SET time_created = ?1 WHERE id IN ('msg_0', 'msg_1')",
                [2_000_000_000_000_i64],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE message SET time_created = ?1 WHERE id = 'msg_2'",
                [1_900_000_000_000_i64],
            )
            .unwrap();
        drop(connection);

        assert_eq!(
            local_usage_30d(&paths, 2_000_000_000),
            Some(LocalUsage {
                tokens: 165,
                cost_usd: 1.25,
            })
        );
    }

    #[test]
    fn local_usage_reads_opencode_2_session_messages() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        let connection = Connection::open(&paths.db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_message (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
                    seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL, data TEXT NOT NULL
                );",
            )
            .unwrap();
        for (id, kind, time, data) in [
            (
                "a",
                "assistant",
                2_000_000_000_000_i64,
                r#"{"tokens":{"input":100,"output":10,"reasoning":5,"cache":{"read":20,"write":30}},"cost":1.25}"#,
            ),
            (
                "b",
                "user",
                2_000_000_000_000,
                r#"{"tokens":{"input":999}}"#,
            ),
            (
                "c",
                "assistant",
                1_900_000_000_000,
                r#"{"tokens":{"input":999},"cost":9.0}"#,
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO session_message VALUES (?1, 's', ?2, 0, ?3, ?3, ?4)",
                    rusqlite::params![id, kind, time, data],
                )
                .unwrap();
        }
        drop(connection);

        assert_eq!(
            local_usage_30d(&paths, 2_000_000_000),
            Some(LocalUsage {
                tokens: 165,
                cost_usd: 1.25,
            })
        );
    }

    #[test]
    fn opencode_2_session_reads_route_and_context_from_latest_assistant() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        write_fixture_db_v2(
            &paths.db,
            &[
                (
                    "ses_go",
                    "assistant",
                    r#"{"model":{"id":"glm-5.2","providerID":"opencode","variant":"default"},"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
                ),
                ("ses_go", "user", r#"{"text":"hi","files":[]}"#),
                (
                    "ses_go",
                    "assistant",
                    r#"{"model":{"id":"kimi-k2.5","providerID":"opencode-go","variant":"default"},"finish":"stop","tokens":{"input":100,"output":10,"reasoning":5,"cache":{"read":20,"write":30}},"cost":0}"#,
                ),
                ("ses_go", "idle", r#"{"outcome":"succeeded"}"#),
                (
                    "ses_go",
                    "assistant",
                    r#"{"model":{"id":"kimi-k2.5","providerID":"opencode-go","variant":"default"},"finish":"error"}"#,
                ),
                (
                    "ses_go",
                    "assistant",
                    r#"{"model":{"id":"kimi-k2.5","providerID":"opencode-go","variant":"default"},"tokens":{"input":999,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
                ),
            ],
        )
        .unwrap();
        let before = fs::read(&paths.db).unwrap();
        match lookup_session(&paths, "ses_go") {
            SessionLookup::Found(session) => {
                assert_eq!(session.provider_id.as_deref(), Some("opencode-go"));
                assert_eq!(session.model_id.as_deref(), Some("kimi-k2.5"));
                assert_eq!(session.context_tokens, Some(165));
            }
            other => panic!("expected found session, got {other:?}"),
        }
        assert_eq!(lookup_session(&paths, "ses_absent"), SessionLookup::Missing);
        assert_eq!(
            fs::read(&paths.db).unwrap(),
            before,
            "the store is read-only"
        );
    }

    #[test]
    fn opencode_2_session_without_a_reply_falls_back_to_the_session_model() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        write_fixture_db_v2(
            &paths.db,
            &[
                ("ses_new", "user", r#"{"text":"hi","files":[]}"#),
                ("ses_bare", "user", r#"{"text":"hi","files":[]}"#),
            ],
        )
        .unwrap();
        let connection = Connection::open(&paths.db).unwrap();
        connection
            .execute(
                "UPDATE session_v2 SET model = ?1 WHERE id = 'ses_new'",
                [r#"{"id":"kimi-k2.5","providerID":"opencode-go"}"#],
            )
            .unwrap();
        drop(connection);

        match lookup_session(&paths, "ses_new") {
            SessionLookup::Found(session) => {
                assert_eq!(session.provider_id.as_deref(), Some("opencode-go"));
                assert_eq!(session.model_id.as_deref(), Some("kimi-k2.5"));
                assert_eq!(session.context_tokens, None);
            }
            other => panic!("expected found session, got {other:?}"),
        }
        match lookup_session(&paths, "ses_bare") {
            SessionLookup::Found(session) => {
                assert_eq!(session.provider_id, None);
                assert_eq!(session.model_id, None);
            }
            other => panic!("expected found session, got {other:?}"),
        }
    }

    #[test]
    fn a_store_with_both_schemas_finds_sessions_in_either() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        write_fixture_db(
            &paths.db,
            &[(
                "ses_old",
                r#"{"role":"assistant","providerID":"anthropic","modelID":"sonnet"}"#,
            )],
        )
        .unwrap();
        write_fixture_db_v2(
            &paths.db,
            &[(
                "ses_new",
                "assistant",
                r#"{"model":{"id":"kimi-k2.5","providerID":"opencode-go"}}"#,
            )],
        )
        .unwrap();
        let provider = |id| match lookup_session(&paths, id) {
            SessionLookup::Found(session) => session.provider_id,
            other => panic!("expected found session, got {other:?}"),
        };
        assert_eq!(provider("ses_old").as_deref(), Some("anthropic"));
        assert_eq!(provider("ses_new").as_deref(), Some("opencode-go"));
        assert_eq!(lookup_session(&paths, "ses_absent"), SessionLookup::Missing);
    }

    #[test]
    fn a_store_with_neither_schema_is_unreadable() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        Connection::open(&paths.db)
            .unwrap()
            .execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        assert_eq!(lookup_session(&paths, "ses_go"), SessionLookup::Unreadable);
    }

    #[test]
    fn model_context_lookup_is_exact_and_bounded() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::from_dir(directory.path());
        fs::write(
            &paths.models,
            br#"{"opencode":{"models":{"big-pickle":{"limit":{"context":200000}}}},"other":{"models":{"big-pickle":{"limit":{"context":1}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            model_context_window(&paths, "opencode", "big-pickle"),
            Some(200_000)
        );
        assert_eq!(model_context_window(&paths, "other", "missing"), None);

        fs::write(&paths.models, vec![b' '; MAX_MODELS_BYTES as usize + 1]).unwrap();
        assert_eq!(model_context_window(&paths, "opencode", "big-pickle"), None);
    }

    #[test]
    fn database_opens_under_a_path_containing_uri_punctuation() {
        let directory = tempdir().unwrap();
        // `?` and `#` are the two characters SQLite's URI filename mode gives
        // meaning to. Windows forbids `?` in a path outright, so there the
        // fixture exercises the half of the pair the filesystem allows.
        let store = if cfg!(windows) {
            directory.path().join("weird#dir")
        } else {
            directory.path().join("we?ird#dir")
        };
        fs::create_dir_all(&store).unwrap();
        write_fixture_db(
            &store.join("opencode.db"),
            &[(
                "ses_go",
                r#"{"role":"assistant","providerID":"opencode-go","modelID":"kimi-k3"}"#,
            )],
        )
        .unwrap();
        let paths = OpenCodePaths::from_dir(&store);
        assert!(matches!(
            lookup_session(&paths, "ses_go"),
            SessionLookup::Found(_)
        ));
    }

    #[test]
    fn queries_are_exact_session_lookups() {
        assert!(SESSION_BY_ID.contains("WHERE id = ?1"));
        assert!(!SESSION_BY_ID.to_ascii_lowercase().contains("scan"));
        assert!(MESSAGE_DATA_FOR_SESSION.contains("WHERE session_id = ?1"));
        assert!(MESSAGE_DATA_FOR_SESSION.contains("LIMIT 8"));
        assert!(!MESSAGE_DATA_FOR_SESSION.contains("SUM("));
        assert!(!MESSAGE_DATA_FOR_SESSION.contains("cost"));
        assert!(SESSION_BY_ID_V2.contains("WHERE id = ?1"));
        assert!(SESSION_BY_ID_V2.contains("LIMIT 1"));
        assert!(ASSISTANT_DATA_FOR_SESSION_V2.contains("WHERE session_id = ?1"));
        assert!(ASSISTANT_DATA_FOR_SESSION_V2.contains("LIMIT 8"));
        assert!(!ASSISTANT_DATA_FOR_SESSION_V2.contains("SUM("));
        assert!(!ASSISTANT_DATA_FOR_SESSION_V2.contains("cost"));
    }
}
