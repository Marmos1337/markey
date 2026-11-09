//! Settings that follow the account: three named sets on the Identity server.
//!
//! `rules` are the switches that decide what gets corrected and
//! `app-exceptions` the apps left alone; both sync by themselves.
//! `word-exceptions` is opt-in, off by default: the engine learns those words
//! from undone corrections, so the list can hold whatever was typed. While it
//! is off nothing about it is read or sent, and switching it off deletes
//! nothing. Abbreviations can hold personal text and stay on this computer,
//! as do hotkeys, the menu bar icon, the assist, the master switch and the
//! API key.
//!
//! Each set is merged three ways against what was last synced, so a change on
//! either side survives. All of this happens on one worker thread, never on
//! the keyboard hook or the main thread, and never with the engine locked
//! during a network call.
//!
//! The server, `/identity/apps/markey/config/{key}`:
//! * `GET` answers `{key, title, count, revision, size, updatedAt, value}`, or
//!   404 when the set does not exist.
//! * `PUT` takes exactly `{title, count, value}` and answers the same without
//!   `value`. `If-Match` carries the revision that was read, or `0` for "must
//!   not exist yet". A stale one gets 409.
//! * 413 means this set will not fit, and trying again will not help.
//!
//! Data from the server that is not the expected shape is refused whole. A
//! list read as empty because it could not be understood would otherwise look
//! like the user removing everything on it.

use crate::account::{spawn_logged, TokenError};
use crate::identity::{Answer, Http, NetError, Write, APP_ID};
use crate::private_files::atomic_write;
use rekey_core::config::{Config, Sensitivity};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The server's limit for one set.
const MAX_SET_BYTES: usize = 256 * 1024;
/// A full set plus its description.
const MAX_ANSWER_BYTES: u64 = 512 * 1024;
/// After the first PUT is turned down, how many more times to merge and retry.
const MAX_CONFLICT_RETRIES: usize = 3;
/// Quiet time after the last change before anything is sent.
const DEBOUNCE: Duration = Duration::from_secs(2);
/// When a sync failed, and nothing else happens first.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);
const CREATE_REVISION: u64 = 0;

const MAX_SYNCED_WORDS: usize = 5000;
/// Leaves room for the default exclusions under the settings' own limit.
const MAX_SYNCED_APPS: usize = 492;
const MAX_WORD_CHARS: usize = 128;
const MAX_APP_ID_LEN: usize = 256;
const MAX_LAYOUTS: usize = 16;
const MAX_LENGTH_SETTING: u64 = 64;

/// The fields of the `rules` set.
const RULE_FIELDS: [&str; 15] = [
    "sensitivity",
    "layouts",
    "min_word_len",
    "require_dict_below_len",
    "skip_all_caps",
    "switch_layout",
    "convert_selection",
    "convert_word_before_caret",
    "sound",
    "fix_case",
    "fix_two_caps",
    "correct_on_enter_tab",
    "fix_typos",
    "expand_abbreviations",
    "extended_dictionary",
];

/// Where the access token comes from.
pub trait TokenSource: Send + Sync {
    fn account_id(&self) -> Option<String>;
    fn access_token(&self) -> Result<String, TokenError>;
    fn token_after_reject(&self, rejected: &str) -> Result<String, TokenError>;
}

/// What the cloud needs from the app.
pub trait ConfigHost: Send + Sync {
    /// The live configuration, or `None` before the app is ready. Never a
    /// stand-in: defaults taken for the user's settings would get uploaded.
    fn config(&self) -> Option<Config>;
    /// Change the live configuration the way the settings window does.
    /// `change` gets the current one and returns false to back out, which
    /// makes this return `Ok(false)`.
    fn apply(&self, change: &mut dyn FnMut(&mut Config) -> bool) -> Result<bool, String>;
    /// `get_account` would now say something else about the sync.
    fn sync_changed(&self);
    /// Whether the word list takes part in syncing.
    fn sync_words(&self) -> bool;
    fn set_sync_words(&self, on: bool);
}

// -- the three sets ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetKind {
    Rules,
    WordExceptions,
    AppExceptions,
}

fn config_fields(config: &Config) -> Map<String, Value> {
    match serde_json::to_value(config) {
        Ok(Value::Object(fields)) => fields,
        _ => Map::new(),
    }
}

fn is_default_app(id: &str) -> bool {
    Config::default()
        .excluded_apps
        .iter()
        .any(|d| d.eq_ignore_ascii_case(id))
}

fn word_key(word: &str) -> String {
    // The way `Config::is_exception` compares them: ё and е are one letter.
    rekey_core::model::normalize(word)
}

fn app_key(id: &str) -> String {
    id.to_ascii_lowercase()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Is this a value the app could use for that field of `rules`?
fn rule_is_usable(field: &str, value: &Value) -> bool {
    match field {
        "sensitivity" => serde_json::from_value::<Sensitivity>(value.clone()).is_ok(),
        "layouts" => value.as_array().is_some_and(|ids| {
            !ids.is_empty()
                && ids.len() <= MAX_LAYOUTS
                && ids.iter().all(|id| {
                    id.as_str()
                        .is_some_and(|id| rekey_core::layout::layout(id).is_some())
                })
        }),
        "min_word_len" | "require_dict_below_len" => {
            value.as_u64().is_some_and(|n| n <= MAX_LENGTH_SETTING)
        }
        _ => value.is_boolean(),
    }
}

/// `(local ∪ remote) − removedLocally − removedRemotely`. With no base nothing
/// counts as removed, which makes a first sync a union. Local order first.
fn merge_lists(
    base: Option<&[String]>,
    local: &[String],
    remote: &[String],
    key: fn(&str) -> String,
) -> Vec<String> {
    let keys = |items: &[String]| items.iter().map(|i| key(i)).collect::<HashSet<String>>();
    let base_keys = keys(base.unwrap_or(&[]));
    let local_keys = keys(local);
    let remote_keys = keys(remote);
    let mut seen = HashSet::new();
    local
        .iter()
        .chain(remote)
        .filter(|item| {
            let k = key(item);
            let removed =
                base_keys.contains(&k) && (!local_keys.contains(&k) || !remote_keys.contains(&k));
            !removed && seen.insert(k)
        })
        .cloned()
        .collect()
}

/// Per field: the side that moved away from the base wins, and when both did,
/// local wins. Before the first sync the base is the defaults, so a new
/// computer that was never set up takes the account's settings instead of
/// overwriting them with its own defaults.
fn merge_rules(base: Option<&Value>, local: &Value, remote: &Value) -> Value {
    let defaults = rules_of(&Config::default());
    let mut merged = Map::new();
    for field in RULE_FIELDS {
        let Some(mine) = local.get(field) else {
            continue;
        };
        let reference = base
            .and_then(|b| b.get(field))
            .or_else(|| defaults.get(field));
        let chosen = match remote.get(field) {
            Some(theirs) if Some(mine) == reference && Some(theirs) != reference => theirs,
            _ => mine,
        };
        merged.insert(field.to_string(), chosen.clone());
    }
    Value::Object(merged)
}

fn rules_of(config: &Config) -> Value {
    let all = config_fields(config);
    let mut out = Map::new();
    for field in RULE_FIELDS {
        if let Some(value) = all.get(field) {
            out.insert(field.to_string(), value.clone());
        }
    }
    Value::Object(out)
}

impl SetKind {
    const ALL: [SetKind; 3] = [
        SetKind::Rules,
        SetKind::WordExceptions,
        SetKind::AppExceptions,
    ];

    fn key(self) -> &'static str {
        match self {
            SetKind::Rules => "rules",
            SetKind::WordExceptions => "word-exceptions",
            SetKind::AppExceptions => "app-exceptions",
        }
    }

    fn title(self) -> &'static str {
        match self {
            SetKind::Rules => "Правила исправления",
            SetKind::WordExceptions => "Слова-исключения",
            SetKind::AppExceptions => "Приложения-исключения",
        }
    }

    /// This set as it stands in `config`.
    fn extract(self, config: &Config) -> Value {
        match self {
            SetKind::Rules => rules_of(config),
            SetKind::WordExceptions => json!(config.exceptions),
            // A default the user removed never travels, and neither does one
            // they kept: every install starts with them anyway.
            SetKind::AppExceptions => json!(config
                .excluded_apps
                .iter()
                .filter(|id| !is_default_app(id))
                .collect::<Vec<_>>()),
        }
    }

    fn count(self, value: &Value) -> usize {
        match self {
            SetKind::Rules => value.as_object().map_or(0, Map::len),
            _ => value.as_array().map_or(0, Vec::len),
        }
    }

    fn is_empty(self, value: &Value) -> bool {
        self.count(value) == 0
    }

    /// How many entries this set may hold on the server. None for `rules`,
    /// which has a fixed size.
    fn max_items(self) -> Option<usize> {
        match self {
            SetKind::Rules => None,
            SetKind::WordExceptions => Some(MAX_SYNCED_WORDS),
            SetKind::AppExceptions => Some(MAX_SYNCED_APPS),
        }
    }

    fn usable_item(self, item: &str) -> bool {
        let fits = match self {
            SetKind::AppExceptions => item.len() <= MAX_APP_ID_LEN,
            _ => item.chars().count() <= MAX_WORD_CHARS,
        };
        fits && !item.is_empty() && !item.chars().any(char::is_control)
    }

    /// An answer from the server in the form the app can use, or
    /// `ApiError::Malformed`. In a list, every entry must be good: quietly
    /// dropping one would read as a removal and take it off this computer.
    fn clean_remote(self, value: &Value) -> Result<Value, ApiError> {
        if self == SetKind::Rules {
            if !value.is_object() {
                return Err(ApiError::Malformed);
            }
            let mut clean = Map::new();
            for field in RULE_FIELDS {
                if let Some(v) = value.get(field).filter(|v| rule_is_usable(field, v)) {
                    clean.insert(field.to_string(), v.clone());
                }
            }
            return Ok(Value::Object(clean));
        }
        let items = value.as_array().ok_or(ApiError::Malformed)?;
        if self.max_items().is_some_and(|max| items.len() > max) {
            return Err(ApiError::Malformed);
        }
        let mut clean = Vec::with_capacity(items.len());
        for item in items {
            let item = item.as_str().ok_or(ApiError::Malformed)?.trim();
            if !self.usable_item(item) {
                return Err(ApiError::Malformed);
            }
            // A default on the list is not damage: an older version may have
            // had it as the user's own, before it became a default.
            if self != SetKind::AppExceptions || !is_default_app(item) {
                clean.push(item);
            }
        }
        Ok(json!(clean))
    }

    fn merge(self, base: Option<&Value>, local: &Value, remote: &Value) -> Value {
        match self {
            SetKind::Rules => merge_rules(base, local, remote),
            SetKind::WordExceptions | SetKind::AppExceptions => {
                let key = self.list_key();
                let base = base.map(strings);
                json!(merge_lists(
                    base.as_deref(),
                    &strings(local),
                    &strings(remote),
                    key
                ))
            }
        }
    }

    fn list_key(self) -> fn(&str) -> String {
        match self {
            SetKind::AppExceptions => app_key,
            _ => word_key,
        }
    }

    /// Do these say the same thing? Lists are sets as far as the app is
    /// concerned, and comparing their order would have two computers rewrite
    /// each other's copy forever.
    fn same(self, a: &Value, b: &Value) -> bool {
        match self {
            SetKind::Rules => RULE_FIELDS.iter().all(|f| a.get(f) == b.get(f)),
            _ => {
                let key = self.list_key();
                let keys = |v: &Value| strings(v).iter().map(|s| key(s)).collect::<HashSet<_>>();
                keys(a) == keys(b)
            }
        }
    }

    /// Put `value` into `config`, leaving everything outside this set alone.
    fn apply_to(self, config: &mut Config, value: &Value) {
        match self {
            SetKind::Rules => {
                let mut all = config_fields(config);
                for field in RULE_FIELDS {
                    if let Some(v) = value.get(field) {
                        all.insert(field.to_string(), v.clone());
                    }
                }
                match serde_json::from_value(Value::Object(all)) {
                    Ok(updated) => *config = updated,
                    Err(_) => log::warn!("cloud settings: rules could not be applied"),
                }
            }
            SetKind::WordExceptions => config.exceptions = strings(value),
            SetKind::AppExceptions => {
                let wanted = strings(value);
                config.excluded_apps.retain(|id| {
                    is_default_app(id) || wanted.iter().any(|w| w.eq_ignore_ascii_case(id))
                });
                for id in wanted {
                    if !config.is_excluded_app(&id) {
                        config.excluded_apps.push(id);
                    }
                }
            }
        }
    }
}

// -- the server ---------------------------------------------------------------

/// A set as the server holds it.
struct Remote {
    value: Value,
    revision: u64,
    updated_at: Option<String>,
}

enum PutOutcome {
    Stored {
        revision: u64,
        updated_at: Option<String>,
    },
    /// Someone else got there first.
    Conflict,
    /// Over a server limit: this set will not go up, however often it tries.
    /// Says which limit, for the log.
    Refused(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiError {
    Unauthorized,
    Network,
    Status(u16),
    Malformed,
}

/// The revision in an answer: the JSON number, or the ETag the server sends
/// beside it (`"<revision>"`).
fn revision_of(body: Option<&Value>, etag: Option<&str>) -> Option<u64> {
    body.and_then(|b| b.get("revision"))
        .and_then(Value::as_u64)
        .or_else(|| {
            etag?
                .trim()
                .trim_start_matches("W/")
                .trim_matches('"')
                .parse()
                .ok()
        })
}

fn updated_at_of(body: Option<&Value>) -> Option<String> {
    match body?.get("updatedAt")? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Which limit a 413 was about. Only known names reach the log.
fn refusal_class(answer: &Answer) -> &'static str {
    let body: Option<Value> = serde_json::from_slice(&answer.body).ok();
    match body
        .as_ref()
        .and_then(|b| b.pointer("/error/code")?.as_str())
    {
        Some("APP_CONFIG_TOO_LARGE") => "too large",
        Some("APP_CONFIG_LIMIT_REACHED") => "too many sets",
        Some("APP_CONFIG_QUOTA_EXCEEDED") => "over quota",
        _ => "unknown limit",
    }
}

fn config_path(key: &str) -> String {
    format!("/identity/apps/{APP_ID}/config/{key}")
}

fn parse_get(answer: &Answer) -> Result<Remote, ApiError> {
    let body: Value = serde_json::from_slice(&answer.body).map_err(|_| ApiError::Malformed)?;
    let revision = revision_of(Some(&body), answer.etag.as_deref()).ok_or(ApiError::Malformed)?;
    Ok(Remote {
        updated_at: updated_at_of(Some(&body)),
        value: body.get("value").cloned().ok_or(ApiError::Malformed)?,
        revision,
    })
}

fn parse_put(answer: &Answer) -> Result<PutOutcome, ApiError> {
    let body: Option<Value> = serde_json::from_slice(&answer.body).ok();
    let revision = revision_of(body.as_ref(), answer.etag.as_deref()).ok_or(ApiError::Malformed)?;
    Ok(PutOutcome::Stored {
        revision,
        updated_at: updated_at_of(body.as_ref()),
    })
}

fn net_error(_: NetError) -> ApiError {
    ApiError::Network
}

fn get_set(http: &Http, token: &str, key: &str) -> Result<Option<Remote>, ApiError> {
    let answer = http
        .get(&config_path(key), Some(token), MAX_ANSWER_BYTES)
        .map_err(net_error)?;
    match answer.status {
        404 => Ok(None),
        401 => Err(ApiError::Unauthorized),
        200..=299 => parse_get(&answer).map(Some),
        status => Err(ApiError::Status(status)),
    }
}

fn put_set(
    http: &Http,
    token: &str,
    kind: SetKind,
    value: &Value,
    revision: u64,
) -> Result<PutOutcome, ApiError> {
    let body = json!({
        "title": kind.title(),
        "count": kind.count(value),
        "value": value,
    });
    let answer = http
        .send(
            Write::Put,
            &config_path(kind.key()),
            Some(token),
            Some(&revision.to_string()),
            &body,
            MAX_ANSWER_BYTES,
        )
        .map_err(net_error)?;
    match answer.status {
        409 => Ok(PutOutcome::Conflict),
        413 => Ok(PutOutcome::Refused(refusal_class(&answer))),
        401 => Err(ApiError::Unauthorized),
        200..=299 => parse_put(&answer),
        status => Err(ApiError::Status(status)),
    }
}

// -- what was last synced ------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
struct SetBase {
    value: Value,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<String>,
    /// Someone removed the server's copy, «Удалить из облака» in MarHub. It
    /// stays removed: `value` is what this computer held at that moment, and
    /// only a real change from that, or a new sign-in, sends the set up again.
    #[serde(default, rename = "deletedRemotely")]
    deleted_remotely: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SyncFile {
    account: Option<String>,
    sets: BTreeMap<String, SetBase>,
}

fn read_sync_file(path: &std::path::Path) -> SyncFile {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

// -- the worker ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Msg {
    /// Fetch everything now: after sign-in, and at launch when signed in.
    Pull,
    /// The configuration changed; send it once things are quiet.
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Syncing,
    Error,
    /// A set is over a limit. Not an error to retry on a timer: only a new
    /// local change can make it fit.
    Refused,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Syncing => "syncing",
            State::Error => "error",
            State::Refused => "refused",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    SignedOut,
    Failed,
    Refused,
}

/// How one set ended, when nothing went wrong on the way.
enum SetDone {
    Settled,
    Refused,
}

enum SyncError {
    SignedOut,
    Failed(String),
}

/// One line of `sync.sets` in `get_account`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetView {
    key: &'static str,
    title: &'static str,
    count: usize,
    updated_at: Option<String>,
}

/// `sync` in `get_account`.
#[derive(Debug, Clone, Serialize)]
pub struct SyncView {
    state: &'static str,
    sets: Vec<SetView>,
}

impl SyncView {
    #[cfg(test)]
    pub fn off() -> SyncView {
        SyncView {
            state: "off",
            sets: Vec::new(),
        }
    }
}

pub struct Cloud {
    http: Http,
    path: PathBuf,
    tokens: Arc<dyn TokenSource>,
    host: Arc<dyn ConfigHost>,
    file: Mutex<SyncFile>,
    state: Mutex<State>,
    tx: Sender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
    debounce: Duration,
}

impl Cloud {
    /// `path` is where the merge base lives between launches.
    pub fn new(
        base: &str,
        path: PathBuf,
        tokens: Arc<dyn TokenSource>,
        host: Arc<dyn ConfigHost>,
    ) -> Arc<Cloud> {
        Self::with_debounce(base, path, tokens, host, DEBOUNCE)
    }

    fn with_debounce(
        base: &str,
        path: PathBuf,
        tokens: Arc<dyn TokenSource>,
        host: Arc<dyn ConfigHost>,
        debounce: Duration,
    ) -> Arc<Cloud> {
        let (tx, rx) = mpsc::channel();
        let file = read_sync_file(&path);
        Arc::new(Cloud {
            http: Http::new(base),
            path,
            tokens,
            host,
            file: Mutex::new(file),
            state: Mutex::new(State::Idle),
            tx,
            rx: Mutex::new(Some(rx)),
            debounce,
        })
    }

    /// Start the worker. Once is enough.
    pub fn spawn(self: &Arc<Self>) {
        let Some(rx) = self.rx.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let me = self.clone();
        spawn_logged("markey-cloud", move || me.work(rx));
    }

    /// A session has just begun. Only one the user asked for counts as coming
    /// back for sets deleted from the cloud: a silent one happens by itself,
    /// and must not undo a deletion.
    pub fn signed_in(&self, interactive: bool) {
        self.set_state(State::Idle);
        if interactive {
            self.forget_deletions();
        }
        self.pull();
    }

    /// Fetch and merge all sets as soon as possible.
    pub fn pull(&self) {
        let _ = self.tx.send(Msg::Pull);
    }

    /// The configuration changed. Safe from any thread: this only posts.
    pub fn changed(&self) {
        let _ = self.tx.send(Msg::Changed);
    }

    fn work(&self, rx: Receiver<Msg>) {
        let mut due: Option<Instant> = None;
        let mut pull = false;
        loop {
            let message = match due {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match message {
                Ok(Msg::Pull) => {
                    pull = true;
                    due = Some(Instant::now());
                }
                Ok(Msg::Changed) => {
                    // A pull that is about to run sends local changes anyway;
                    // one waiting out a failure should not hold them back.
                    let soon = Instant::now() + self.debounce;
                    due = Some(match due {
                        Some(at) if pull && at < soon => at,
                        _ => soon,
                    });
                }
                Err(RecvTimeoutError::Timeout) => {
                    due = None;
                    match self.sync_now(pull) {
                        // Nothing is lost by waiting: what differs from the
                        // base goes up the next time this runs.
                        Outcome::Failed => due = Some(Instant::now() + RETRY_AFTER_FAILURE),
                        Outcome::Done | Outcome::SignedOut | Outcome::Refused => pull = false,
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn set_state(&self, state: State) {
        let changed = self
            .state
            .lock()
            .map(|mut slot| {
                let changed = *slot != state;
                *slot = state;
                changed
            })
            .unwrap_or(false);
        if changed {
            self.host.sync_changed();
        }
    }

    fn base(&self, kind: SetKind) -> Option<SetBase> {
        self.file.lock().ok()?.sets.get(kind.key()).cloned()
    }

    fn write_file(&self) {
        let Ok(file) = self.file.lock() else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&*file) else {
            return;
        };
        drop(file);
        let saved = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| atomic_write(&self.path, &bytes));
        if let Err(e) = saved {
            log::warn!("cloud settings: cannot save the sync state: {e}");
        }
    }

    /// The sets that take part right now.
    fn active_sets(&self) -> Vec<SetKind> {
        let words = self.host.sync_words();
        SetKind::ALL
            .into_iter()
            .filter(|kind| words || *kind != SetKind::WordExceptions)
            .collect()
    }

    /// Another account's base says nothing about this one's data, and the
    /// choice to send words was made for the account before it.
    fn adopt_account(&self, account: &str) {
        let mut reset = false;
        let mut different = false;
        if let Ok(mut file) = self.file.lock() {
            if file.account.as_deref() != Some(account) {
                different = file.account.is_some();
                *file = SyncFile {
                    account: Some(account.to_string()),
                    sets: BTreeMap::new(),
                };
                reset = true;
            }
        }
        if reset {
            self.write_file();
            self.set_state(State::Idle);
        }
        if different {
            self.host.set_sync_words(false);
        }
    }

    fn remember(&self, kind: SetKind, value: Value, revision: u64, updated_at: Option<String>) {
        self.store_base(
            kind,
            SetBase {
                value,
                revision: Some(revision),
                updated_at,
                deleted_remotely: false,
            },
        );
    }

    fn remember_deleted(&self, kind: SetKind, value: Value) {
        self.store_base(
            kind,
            SetBase {
                value,
                revision: None,
                updated_at: None,
                deleted_remotely: true,
            },
        );
    }

    fn store_base(&self, kind: SetKind, base: SetBase) {
        if let Ok(mut file) = self.file.lock() {
            file.sets.insert(kind.key().to_string(), base);
        }
        self.write_file();
    }

    /// Back to "never uploaded" for whatever was deleted from the cloud.
    fn forget_deletions(&self) {
        let mut any = false;
        if let Ok(mut file) = self.file.lock() {
            let before = file.sets.len();
            file.sets.retain(|_, base| !base.deleted_remotely);
            any = file.sets.len() != before;
        }
        if any {
            self.write_file();
        }
    }

    /// Is there anything of this set the server has not seen?
    fn needs_push(&self, kind: SetKind, config: &Config) -> bool {
        let local = kind.extract(config);
        match self.base(kind) {
            Some(base) => !kind.same(&base.value, &local),
            None => !kind.is_empty(&local),
        }
    }

    /// One round with the server. `pull` looks at every set; otherwise only
    /// the ones that changed here.
    fn sync_now(&self, pull: bool) -> Outcome {
        let Some(account) = self.tokens.account_id() else {
            self.set_state(State::Idle);
            return Outcome::SignedOut;
        };
        let Some(config) = self.host.config() else {
            log::warn!("cloud settings: the configuration is not available");
            self.set_state(State::Error);
            return Outcome::Failed;
        };
        self.adopt_account(&account);
        // After adopting, which can switch the word list off.
        let work: Vec<SetKind> = self
            .active_sets()
            .into_iter()
            .filter(|kind| pull || self.needs_push(*kind, &config))
            .collect();
        if work.is_empty() {
            // Whatever failed earlier no longer needs sending.
            self.set_state(State::Idle);
            return Outcome::Done;
        }
        self.set_state(State::Syncing);
        let mut outcome = Outcome::Done;
        let mut refused = false;
        for kind in work {
            match self.sync_set(kind, &account) {
                Ok(SetDone::Settled) => {}
                Ok(SetDone::Refused) => refused = true,
                Err(SyncError::SignedOut) => {
                    outcome = Outcome::SignedOut;
                    break;
                }
                Err(SyncError::Failed(why)) => {
                    log::info!("cloud settings: {} not synced ({why})", kind.key());
                    outcome = Outcome::Failed;
                }
            }
        }
        if outcome == Outcome::Done && refused {
            outcome = Outcome::Refused;
        }
        self.set_state(match outcome {
            Outcome::Failed => State::Error,
            Outcome::Refused => State::Refused,
            Outcome::Done | Outcome::SignedOut => State::Idle,
        });
        outcome
    }

    /// Run a request with a token, renewing it once if the server turns it
    /// down early.
    ///
    /// A round belongs to the account it started with. Someone who signed in
    /// meanwhile must not receive the previous account's settings.
    fn authorized<T>(
        &self,
        account: &str,
        call: impl Fn(&str) -> Result<T, ApiError>,
    ) -> Result<T, SyncError> {
        let token = self.tokens.access_token().map_err(token_failure)?;
        // After taking the token, so that a token of the new account is seen.
        if self.tokens.account_id().as_deref() != Some(account) {
            return Err(SyncError::SignedOut);
        }
        let result = match call(&token) {
            Err(ApiError::Unauthorized) => {
                let token = self
                    .tokens
                    .token_after_reject(&token)
                    .map_err(token_failure)?;
                call(&token)
            }
            other => other,
        };
        result.map_err(|e| SyncError::Failed(format!("{e:?}")))
    }

    /// False when the configuration moved while the server was being asked.
    fn apply_merged(
        &self,
        kind: SetKind,
        account: &str,
        local: &Value,
        merged: &Value,
    ) -> Result<bool, SyncError> {
        if self.tokens.account_id().as_deref() != Some(account) {
            return Err(SyncError::SignedOut);
        }
        self.host
            .apply(&mut |config| {
                if !kind.same(&kind.extract(config), local) {
                    return false;
                }
                kind.apply_to(config, merged);
                true
            })
            // The app's message can quote settings; the log gets none of that.
            .map_err(|_| SyncError::Failed("the merged settings were not accepted".to_string()))
    }

    fn sync_set(&self, kind: SetKind, account: &str) -> Result<SetDone, SyncError> {
        for _ in 0..=MAX_CONFLICT_RETRIES {
            let config = self
                .host
                .config()
                .ok_or_else(|| SyncError::Failed("not ready".to_string()))?;
            let local = kind.extract(&config);
            let base = self.base(kind);
            let remote =
                self.authorized(account, |token| get_set(&self.http, token, kind.key()))?;
            // Before anything is applied, sent or remembered.
            let clean = remote
                .as_ref()
                .map(|remote| kind.clean_remote(&remote.value))
                .transpose()
                .map_err(|e| SyncError::Failed(format!("{e:?}")))?;
            if remote.is_none() {
                if let Some(base) = &base {
                    if base.deleted_remotely {
                        if kind.same(&base.value, &local) {
                            return Ok(SetDone::Settled);
                        }
                    } else if base.revision.is_some() {
                        // It was there at the last sync, so someone removed
                        // it. This computer keeps its data and does not put
                        // the copy back.
                        log::info!("cloud settings: {} was deleted from the cloud", kind.key());
                        self.remember_deleted(kind, local);
                        return Ok(SetDone::Settled);
                    }
                }
            }
            // After a deletion the server's copy is a new one, which removed
            // nothing: what it lacks is not a removal.
            let merge_base = base
                .as_ref()
                .filter(|base| !base.deleted_remotely)
                .map(|base| &base.value);
            let merged = match &clean {
                Some(clean) => kind.merge(merge_base, &local, clean),
                None => local.clone(),
            };
            // Before applying: what cannot be stored must not grow the local
            // lists either.
            let size = serde_json::to_vec(&merged).map_or(usize::MAX, |bytes| bytes.len());
            if size > MAX_SET_BYTES {
                log::warn!(
                    "cloud settings: {} refused (over the size limit)",
                    kind.key()
                );
                return Ok(SetDone::Refused);
            }
            // Past this the next pull could not read it back.
            if kind
                .max_items()
                .is_some_and(|max| kind.count(&merged) > max)
            {
                log::warn!("cloud settings: {} refused (too many items)", kind.key());
                return Ok(SetDone::Refused);
            }
            if !kind.same(&merged, &local) && !self.apply_merged(kind, account, &local, &merged)? {
                continue;
            }
            let revision = match (&remote, &clean) {
                (Some(remote), Some(clean)) if kind.same(&merged, clean) => {
                    self.remember(kind, merged, remote.revision, remote.updated_at.clone());
                    return Ok(SetDone::Settled);
                }
                (Some(remote), _) => remote.revision,
                (None, _) => {
                    if kind.is_empty(&merged) {
                        if base.is_some_and(|base| base.deleted_remotely) {
                            self.remember_deleted(kind, merged);
                        }
                        return Ok(SetDone::Settled);
                    }
                    CREATE_REVISION
                }
            };
            // Switched off while this round was running.
            if !self.active_sets().contains(&kind) {
                return Ok(SetDone::Settled);
            }
            let outcome = self.authorized(account, |token| {
                put_set(&self.http, token, kind, &merged, revision)
            })?;
            match outcome {
                PutOutcome::Stored {
                    revision,
                    updated_at,
                } => {
                    self.remember(kind, merged, revision, updated_at);
                    return Ok(SetDone::Settled);
                }
                PutOutcome::Conflict => {}
                PutOutcome::Refused(class) => {
                    log::warn!(
                        "cloud settings: {} refused by the server ({class})",
                        kind.key()
                    );
                    return Ok(SetDone::Refused);
                }
            }
        }
        Err(SyncError::Failed("still conflicting".to_string()))
    }

    /// What `get_account` shows.
    pub fn view(&self) -> SyncView {
        if self.tokens.account_id().is_none() {
            return SyncView {
                state: "off",
                sets: Vec::new(),
            };
        }
        let state = self
            .state
            .lock()
            .map(|s| *s)
            .unwrap_or(State::Idle)
            .as_str();
        let config = self.host.config();
        let sets = self
            .active_sets()
            .into_iter()
            .map(|kind| SetView {
                key: kind.key(),
                title: kind.title(),
                count: config
                    .as_ref()
                    .map_or(0, |config| kind.count(&kind.extract(config))),
                updated_at: self.base(kind).and_then(|b| b.updated_at),
            })
            .collect();
        SyncView { state, sets }
    }
}

fn token_failure(error: TokenError) -> SyncError {
    match error {
        TokenError::SignedOut => SyncError::SignedOut,
        TokenError::Offline => SyncError::Failed("offline".to_string()),
        TokenError::Server => SyncError::Failed("server".to_string()),
    }
}

/// Where the merge base is kept.
pub fn sync_file_path() -> PathBuf {
    crate::settings::config_dir().join("cloud-sync.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{refused_base, temp_dir, Cleanup, Reply, Request, Server};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // -- merging lists -------------------------------------------------------

    #[test]
    fn a_first_sync_is_a_union() {
        assert_eq!(
            merge_lists(None, &words(&["a", "b"]), &words(&["b", "c"]), word_key),
            words(&["a", "b", "c"])
        );
    }

    #[test]
    fn additions_on_either_side_survive() {
        let base = words(&["a"]);
        assert_eq!(
            merge_lists(
                Some(&base),
                &words(&["a", "local"]),
                &words(&["a", "remote"]),
                word_key
            ),
            words(&["a", "local", "remote"])
        );
    }

    #[test]
    fn a_removal_on_either_side_wins_over_an_unchanged_copy() {
        let base = words(&["a", "b", "c"]);
        assert_eq!(
            merge_lists(
                Some(&base),
                &words(&["a", "c"]),
                &words(&["a", "b", "c"]),
                word_key
            ),
            words(&["a", "c"]),
            "removed here"
        );
        assert_eq!(
            merge_lists(
                Some(&base),
                &words(&["a", "b", "c"]),
                &words(&["a", "c"]),
                word_key
            ),
            words(&["a", "c"]),
            "removed there"
        );
        assert_eq!(
            merge_lists(Some(&base), &words(&["a"]), &words(&["c"]), word_key),
            Vec::<String>::new(),
            "each removed what the other kept"
        );
    }

    #[test]
    fn an_item_new_to_both_sides_appears_once() {
        assert_eq!(
            merge_lists(Some(&words(&[])), &words(&["x"]), &words(&["x"]), word_key),
            words(&["x"])
        );
    }

    #[test]
    fn words_compare_the_way_the_engine_does() {
        assert_eq!(
            merge_lists(None, &words(&["Ёлка"]), &words(&["елка", "ЕЛКА"]), word_key),
            words(&["Ёлка"])
        );
        let base = words(&["ёлка"]);
        assert_eq!(
            merge_lists(Some(&base), &words(&[]), &words(&["ЕЛКА"]), word_key),
            Vec::<String>::new()
        );
    }

    #[test]
    fn app_ids_ignore_ascii_case_only() {
        assert_eq!(
            merge_lists(
                None,
                &words(&["com.Foo.App"]),
                &words(&["com.foo.app", "Bar.exe"]),
                app_key
            ),
            words(&["com.Foo.App", "Bar.exe"])
        );
    }

    #[test]
    fn local_order_is_kept_so_an_equal_merge_changes_nothing() {
        let local = words(&["b", "a"]);
        assert_eq!(
            merge_lists(Some(&local), &local, &words(&["a", "b"]), word_key),
            local
        );
    }

    // -- merging rules -------------------------------------------------------

    fn rules_with(change: impl FnOnce(&mut Config)) -> Value {
        let mut config = Config::default();
        change(&mut config);
        rules_of(&config)
    }

    #[test]
    fn the_rules_set_has_exactly_its_fields() {
        let rules = rules_of(&Config::default());
        assert_eq!(rules.as_object().unwrap().len(), 15);
        assert_eq!(SetKind::Rules.count(&rules), 15);
        for field in [
            "enabled",
            "abbreviations",
            "ai_assist",
            "shortcut",
            "force_layout_1",
            "menu_bar_icon",
            "exceptions",
            "excluded_apps",
        ] {
            assert!(rules.get(field).is_none(), "{field} stays local");
        }
    }

    #[test]
    fn the_side_that_changed_wins_each_field() {
        let base = rules_of(&Config::default());
        let local = rules_with(|c| c.sound = true);
        let remote = rules_with(|c| c.fix_typos = true);
        let merged = merge_rules(Some(&base), &local, &remote);
        assert_eq!(merged["sound"], true);
        assert_eq!(merged["fix_typos"], true);
    }

    #[test]
    fn local_wins_a_real_conflict() {
        let base = rules_of(&Config::default());
        let local = rules_with(|c| c.min_word_len = 3);
        let remote = rules_with(|c| c.min_word_len = 5);
        assert_eq!(merge_rules(Some(&base), &local, &remote)["min_word_len"], 3);
    }

    #[test]
    fn a_new_computer_takes_the_accounts_rules() {
        let local = rules_of(&Config::default());
        let remote = rules_with(|c| {
            c.sound = true;
            c.layouts = vec!["us".into(), "uk".into()];
        });
        let merged = merge_rules(None, &local, &remote);
        assert_eq!(merged["sound"], true);
        assert_eq!(merged["layouts"], json!(["us", "uk"]));
    }

    #[test]
    fn with_no_base_someone_elses_choice_does_not_beat_a_local_one() {
        let local = rules_with(|c| c.sound = true);
        let remote = rules_with(|c| c.sound = false);
        assert_eq!(merge_rules(None, &local, &remote)["sound"], true);
    }

    #[test]
    fn a_field_the_server_lacks_stays_local() {
        let base = rules_of(&Config::default());
        let local = rules_with(|c| c.sound = true);
        let merged = merge_rules(Some(&base), &local, &json!({}));
        assert_eq!(merged["sound"], true);
        assert_eq!(merged.as_object().unwrap().len(), 15);
    }

    #[test]
    fn unusable_remote_rules_are_dropped_field_by_field() {
        let cleaned = SetKind::Rules
            .clean_remote(&json!({
                "sound": "yes",
                "fix_typos": true,
                "layouts": ["us", "klingon"],
                "min_word_len": 1000,
                "require_dict_below_len": 3,
                "sensitivity": "ultra",
                "enabled": false,
                "abbreviations": [],
            }))
            .unwrap();
        assert_eq!(
            cleaned,
            json!({ "fix_typos": true, "require_dict_below_len": 3 })
        );
        let empty = SetKind::Rules.clean_remote(&json!({ "layouts": [] }));
        assert_eq!(empty, Ok(json!({})));
    }

    #[test]
    fn good_list_entries_are_trimmed() {
        assert_eq!(
            SetKind::WordExceptions.clean_remote(&json!([" привет ", "мир"])),
            Ok(json!(["привет", "мир"]))
        );
        // A default on the list is simply not ours to apply, not damage.
        assert_eq!(
            SetKind::AppExceptions.clean_remote(&json!(["com.foo", "com.apple.Terminal"])),
            Ok(json!(["com.foo"]))
        );
        // A valid empty list is real: removal travels between computers.
        assert_eq!(
            SetKind::WordExceptions.clean_remote(&json!([])),
            Ok(json!([]))
        );
    }

    #[test]
    fn anything_else_in_a_set_is_malformed() {
        let many = |n: usize| json!((0..n).map(|i| format!("item{i}")).collect::<Vec<_>>());
        for (kind, value) in [
            (SetKind::Rules, json!("nope")),
            (SetKind::Rules, json!(["array"])),
            (SetKind::Rules, json!(null)),
            (SetKind::WordExceptions, json!({ "not": "a list" })),
            (SetKind::WordExceptions, json!("text")),
            (SetKind::WordExceptions, json!(null)),
            (SetKind::WordExceptions, json!(["ok", 5])),
            (SetKind::WordExceptions, json!(["ok", ""])),
            (SetKind::WordExceptions, json!(["ok", "   "])),
            (SetKind::WordExceptions, json!(["ok", "a\nb"])),
            (SetKind::WordExceptions, json!(["x".repeat(129)])),
            (SetKind::WordExceptions, many(5001)),
            (SetKind::AppExceptions, json!({})),
            (SetKind::AppExceptions, json!(["ok", ""])),
            (SetKind::AppExceptions, json!(["a\u{7}b"])),
            (SetKind::AppExceptions, json!(["x".repeat(257)])),
            (SetKind::AppExceptions, many(MAX_SYNCED_APPS + 1)),
        ] {
            assert_eq!(
                kind.clean_remote(&value),
                Err(ApiError::Malformed),
                "{kind:?}"
            );
        }
        assert!(SetKind::WordExceptions.clean_remote(&many(5000)).is_ok());
        assert!(SetKind::AppExceptions
            .clean_remote(&many(MAX_SYNCED_APPS))
            .is_ok());
    }

    #[test]
    fn defaults_never_travel() {
        let mut config = Config::default();
        config.excluded_apps.push("com.mine.App".into());
        config.excluded_apps.retain(|id| id != "com.apple.Terminal");
        assert_eq!(
            SetKind::AppExceptions.extract(&config),
            json!(["com.mine.App"]),
            "neither the defaults kept nor the one removed"
        );
    }

    #[test]
    fn applying_apps_leaves_the_defaults_as_this_computer_has_them() {
        let mut config = Config::default();
        config.excluded_apps.retain(|id| id != "com.apple.Terminal");
        config.excluded_apps.push("com.old.App".into());
        SetKind::AppExceptions.apply_to(&mut config, &json!(["com.new.App"]));
        assert!(config.is_excluded_app("com.new.App"));
        assert!(!config.is_excluded_app("com.old.App"));
        assert!(config.is_excluded_app("com.googlecode.iterm2"));
        assert!(
            !config.is_excluded_app("com.apple.Terminal"),
            "the default removed here stays removed"
        );
    }

    #[test]
    fn applying_rules_touches_only_rules() {
        let mut config = Config {
            enabled: false,
            ai_assist: true,
            exceptions: words(&["слово"]),
            ..Config::default()
        };
        let merged = rules_with(|c| c.sound = true);
        SetKind::Rules.apply_to(&mut config, &merged);
        assert!(config.sound);
        assert!(!config.enabled);
        assert!(config.ai_assist);
        assert_eq!(config.exceptions, words(&["слово"]));
    }

    #[test]
    fn revisions_are_numbers_or_the_etag() {
        assert_eq!(revision_of(Some(&json!({ "revision": 7 })), None), Some(7));
        assert_eq!(
            revision_of(Some(&json!({ "revision": 7 })), Some("\"9\"")),
            Some(7)
        );
        assert_eq!(revision_of(Some(&json!({})), Some("\"9\"")), Some(9));
        assert_eq!(revision_of(None, Some("W/\"9\"")), Some(9));
        assert_eq!(revision_of(None, Some("12")), Some(12));
        assert_eq!(revision_of(Some(&json!({ "revision": "x" })), None), None);
        assert_eq!(revision_of(None, Some("\"abc\"")), None);
    }

    // -- the whole thing, against a server ------------------------------------

    struct Tokens;

    impl TokenSource for Tokens {
        fn account_id(&self) -> Option<String> {
            Some("u1".into())
        }
        fn access_token(&self) -> Result<String, TokenError> {
            Ok("token-1".into())
        }
        fn token_after_reject(&self, _: &str) -> Result<String, TokenError> {
            Ok("token-2".into())
        }
    }

    struct SwitchableTokens(Mutex<Option<String>>);

    impl TokenSource for SwitchableTokens {
        fn account_id(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn access_token(&self) -> Result<String, TokenError> {
            self.account_id()
                .map(|_| "token-1".to_string())
                .ok_or(TokenError::SignedOut)
        }
        fn token_after_reject(&self, _: &str) -> Result<String, TokenError> {
            self.access_token()
        }
    }

    type Interference = Box<dyn FnOnce(&mut Config) + Send>;

    #[derive(Default)]
    struct MemHost {
        config: Mutex<Config>,
        applied: AtomicUsize,
        notified: AtomicUsize,
        /// Run once, right before the next apply looks at the configuration.
        interfere: Mutex<Option<Interference>>,
        sync_words: AtomicBool,
        unavailable: AtomicBool,
    }

    /// Most of these tests are about the word list, so it is on here.
    fn words_host(config: Config) -> Arc<MemHost> {
        Arc::new(MemHost {
            config: Mutex::new(config),
            sync_words: AtomicBool::new(true),
            ..MemHost::default()
        })
    }

    impl ConfigHost for MemHost {
        fn config(&self) -> Option<Config> {
            if self.unavailable.load(Ordering::SeqCst) {
                return None;
            }
            Some(self.config.lock().unwrap().clone())
        }
        fn sync_words(&self) -> bool {
            self.sync_words.load(Ordering::SeqCst)
        }
        fn set_sync_words(&self, on: bool) {
            self.sync_words.store(on, Ordering::SeqCst);
        }
        fn apply(&self, change: &mut dyn FnMut(&mut Config) -> bool) -> Result<bool, String> {
            let mut config = self.config.lock().unwrap();
            if let Some(interfere) = self.interfere.lock().unwrap().take() {
                interfere(&mut config);
            }
            let mut next = config.clone();
            if !change(&mut next) {
                return Ok(false);
            }
            *config = next;
            self.applied.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
        fn sync_changed(&self) {
            self.notified.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The server's config endpoints, in memory.
    #[derive(Default)]
    struct Store {
        sets: BTreeMap<String, (u64, Value)>,
    }

    fn key_of(r: &Request) -> String {
        r.route().rsplit('/').next().unwrap_or("").to_string()
    }

    fn describe(key: &str, revision: u64, value: Option<&Value>) -> Value {
        let mut out = json!({
            "key": key, "title": "t", "count": 0, "revision": revision,
            "size": 2, "updatedAt": format!("2026-11-08T10:00:{revision:02}Z"),
        });
        if let Some(value) = value {
            out["value"] = value.clone();
        }
        out
    }

    fn serve(store: Arc<Mutex<Store>>, r: &Request) -> Reply {
        let key = key_of(r);
        let mut store = store.lock().unwrap();
        match r.method.as_str() {
            "GET" => match store.sets.get(&key) {
                Some((revision, value)) => Reply::Json(200, describe(&key, *revision, Some(value))),
                None => Reply::Json(
                    404,
                    json!({ "error": { "code": "APP_CONFIG_NOT_FOUND", "message": "absent" } }),
                ),
            },
            "PUT" => {
                let body = r.json();
                let fields = body.as_object().unwrap();
                // Exactly as strict as the real one.
                if fields.len() != 3
                    || !fields.contains_key("title")
                    || !fields.contains_key("count")
                    || !fields.contains_key("value")
                {
                    return Reply::Json(400, json!({ "error": { "code": "BAD", "message": "x" } }));
                }
                let expected = r
                    .header("if-match")
                    .and_then(|v| v.trim_matches('"').parse::<u64>().ok());
                let current = store.sets.get(&key).map_or(0, |(rev, _)| *rev);
                if expected != Some(current) {
                    return Reply::Json(
                        409,
                        json!({ "error": {
                            "code": "APP_CONFIG_CONFLICT", "message": "x", "revision": current
                        } }),
                    );
                }
                let next = current + 1;
                store
                    .sets
                    .insert(key.clone(), (next, body["value"].clone()));
                Reply::Json(200, describe(&key, next, None))
            }
            _ => Reply::Json(405, json!({})),
        }
    }

    struct Rig {
        server: Server,
        store: Arc<Mutex<Store>>,
        host: Arc<MemHost>,
        cloud: Arc<Cloud>,
        path: PathBuf,
        _clean: Cleanup,
    }

    fn rig(config: Config) -> Rig {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r| serve(shared.clone(), r));
        rig_on(server, store, config)
    }

    fn rig_on(server: Server, store: Arc<Mutex<Store>>, config: Config) -> Rig {
        let host = words_host(config);
        let dir = temp_dir("cloud");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cloud-sync.json");
        let cloud = Cloud::new(&server.base, path.clone(), Arc::new(Tokens), host.clone());
        Rig {
            server,
            store,
            host,
            cloud,
            path,
            _clean: Cleanup(dir),
        }
    }

    impl Rig {
        fn remote(&self, key: &str) -> Option<(u64, Value)> {
            self.store.lock().unwrap().sets.get(key).cloned()
        }

        fn seed(&self, key: &str, revision: u64, value: Value) {
            self.store
                .lock()
                .unwrap()
                .sets
                .insert(key.to_string(), (revision, value));
        }

        fn puts(&self) -> Vec<Request> {
            self.server
                .requests()
                .into_iter()
                .filter(|r| r.method == "PUT")
                .collect()
        }

        fn config(&self) -> Config {
            self.host.config().unwrap()
        }

        fn change(&self, change: impl FnOnce(&mut Config)) {
            change(&mut self.host.config.lock().unwrap());
        }
    }

    fn with_words(items: &[&str]) -> Config {
        Config {
            exceptions: words(items),
            ..Config::default()
        }
    }

    #[test]
    fn a_missing_set_is_created_with_if_match_zero() {
        let r = rig(with_words(&["слово"]));
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        let (revision, value) = r.remote("word-exceptions").unwrap();
        assert_eq!(revision, 1);
        assert_eq!(value, json!(["слово"]));
        let put = r
            .puts()
            .into_iter()
            .find(|p| p.route().ends_with("word-exceptions"))
            .unwrap();
        assert_eq!(put.header("if-match"), Some("0"));
        assert_eq!(put.header("authorization"), Some("Bearer token-1"));
        assert_eq!(put.path, "/identity/apps/markey/config/word-exceptions");
        let body = put.json();
        assert_eq!(
            body,
            json!({ "title": "Слова-исключения", "count": 1, "value": ["слово"] })
        );
        // Rules always exist; an empty list that never did is not created.
        assert!(r.remote("rules").is_some());
        assert!(r.remote("app-exceptions").is_none());
        assert_eq!(r.host.applied.load(Ordering::SeqCst), 0, "nothing to apply");
    }

    #[test]
    fn nothing_is_applied_or_sent_when_everyone_agrees() {
        let r = rig(with_words(&["a"]));
        r.seed("word-exceptions", 5, json!(["a"]));
        r.seed("rules", 2, rules_of(&Config::default()));
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        assert_eq!(r.host.applied.load(Ordering::SeqCst), 0);
        assert!(r.puts().is_empty());
    }

    #[test]
    fn a_change_from_the_cloud_is_applied_and_merged_results_go_back() {
        let r = rig(with_words(&["local"]));
        r.seed("word-exceptions", 5, json!(["remote"]));
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        assert_eq!(r.config().exceptions, words(&["local", "remote"]));
        let (revision, value) = r.remote("word-exceptions").unwrap();
        assert_eq!(revision, 6);
        assert_eq!(value, json!(["local", "remote"]));
        let put = r
            .puts()
            .into_iter()
            .find(|p| p.route().ends_with("word-exceptions"))
            .unwrap();
        assert_eq!(
            put.header("if-match"),
            Some("5"),
            "the revision that was read"
        );
    }

    #[test]
    fn syncing_again_sends_nothing() {
        let r = rig(with_words(&["a"]));
        r.seed("word-exceptions", 5, json!(["b"]));
        r.cloud.sync_now(true);
        let applied = r.host.applied.load(Ordering::SeqCst);
        let puts = r.puts().len();
        assert_eq!(r.cloud.sync_now(false), Outcome::Done);
        assert_eq!(
            r.puts().len(),
            puts,
            "a push after a pull finds nothing new"
        );
        r.cloud.sync_now(true);
        assert_eq!(r.puts().len(), puts);
        assert_eq!(r.host.applied.load(Ordering::SeqCst), applied);
    }

    #[test]
    fn a_push_with_no_change_does_not_touch_the_network() {
        let r = rig(with_words(&["a"]));
        r.cloud.sync_now(true);
        let before = r.server.requests().len();
        assert_eq!(r.cloud.sync_now(false), Outcome::Done);
        assert_eq!(r.server.requests().len(), before);
    }

    #[test]
    fn a_push_sends_only_what_changed() {
        let r = rig(with_words(&["a"]));
        r.cloud.sync_now(true);
        r.change(|c| c.exceptions.push("b".into()));
        let before = r.puts().len();
        assert_eq!(r.cloud.sync_now(false), Outcome::Done);
        let puts = r.puts();
        assert_eq!(puts.len(), before + 1);
        assert!(puts.last().unwrap().route().ends_with("word-exceptions"));
        assert_eq!(r.remote("word-exceptions").unwrap().1, json!(["a", "b"]));
    }

    #[test]
    fn a_removal_here_reaches_the_server_and_a_removal_there_reaches_here() {
        let r = rig(with_words(&["a", "b"]));
        r.cloud.sync_now(true);
        r.change(|c| c.exceptions.retain(|w| w != "a"));
        r.cloud.sync_now(false);
        assert_eq!(r.remote("word-exceptions").unwrap().1, json!(["b"]));

        r.change(|c| c.exceptions.push("c".into()));
        r.cloud.sync_now(false);
        let (revision, _) = r.remote("word-exceptions").unwrap();
        r.seed("word-exceptions", revision + 1, json!(["c"]));
        r.cloud.sync_now(true);
        assert_eq!(r.config().exceptions, words(&["c"]));
    }

    #[test]
    fn an_empty_list_replaces_a_remote_one_when_that_is_the_truth() {
        let r = rig(with_words(&["a"]));
        r.cloud.sync_now(true);
        r.change(|c| c.exceptions.clear());
        r.cloud.sync_now(false);
        assert_eq!(r.remote("word-exceptions").unwrap().1, json!([]));
    }

    #[test]
    fn a_set_deleted_from_the_cloud_stays_deleted_and_stays_here() {
        let r = rig(with_words(&["a"]));
        r.cloud.sync_now(true);
        let puts = r.puts().len();
        r.store.lock().unwrap().sets.remove("word-exceptions");

        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        assert_eq!(
            r.config().exceptions,
            words(&["a"]),
            "kept on this computer"
        );
        assert!(r.remote("word-exceptions").is_none());
        assert_eq!(r.puts().len(), puts, "not put back");
        assert!(
            r.cloud
                .base(SetKind::WordExceptions)
                .unwrap()
                .deleted_remotely
        );

        // Still so on later syncs, and after a restart.
        r.cloud.sync_now(false);
        let again = Cloud::new(
            &r.server.base,
            r.path.clone(),
            Arc::new(Tokens),
            r.host.clone(),
        );
        assert_eq!(again.sync_now(true), Outcome::Done);
        assert!(r.remote("word-exceptions").is_none());
        assert_eq!(r.puts().len(), puts);
    }

    #[test]
    fn a_real_local_change_brings_a_deleted_set_back() {
        let r = rig(with_words(&["a"]));
        r.cloud.sync_now(true);
        r.store.lock().unwrap().sets.remove("word-exceptions");
        r.cloud.sync_now(true);

        r.change(|c| c.exceptions.push("b".into()));
        assert_eq!(r.cloud.sync_now(false), Outcome::Done);
        let (revision, value) = r.remote("word-exceptions").unwrap();
        assert_eq!((revision, value), (1, json!(["a", "b"])));
        let last = r.puts().pop().unwrap();
        assert_eq!(last.header("if-match"), Some("0"));
        assert!(
            !r.cloud
                .base(SetKind::WordExceptions)
                .unwrap()
                .deleted_remotely
        );
    }

    #[test]
    fn signing_in_on_purpose_brings_a_deleted_set_back_and_silently_does_not() {
        let r = rig(with_words(&["a"]));
        r.cloud.sync_now(true);
        r.store.lock().unwrap().sets.remove("word-exceptions");
        r.cloud.sync_now(true);

        r.cloud.signed_in(false);
        r.cloud.sync_now(true);
        assert!(r.remote("word-exceptions").is_none());

        r.cloud.signed_in(true);
        r.cloud.sync_now(true);
        assert_eq!(r.remote("word-exceptions").unwrap().1, json!(["a"]));
    }

    #[test]
    fn a_new_copy_after_a_deletion_is_not_read_as_removals() {
        let r = rig(with_words(&["a", "b"]));
        r.cloud.sync_now(true);
        r.store.lock().unwrap().sets.remove("word-exceptions");
        r.cloud.sync_now(true);
        // Another computer uploads its own list afterwards.
        r.seed("word-exceptions", 1, json!(["c"]));

        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        assert_eq!(r.config().exceptions, words(&["a", "b", "c"]));
        assert!(
            !r.cloud
                .base(SetKind::WordExceptions)
                .unwrap()
                .deleted_remotely
        );
    }

    #[test]
    fn a_missing_set_with_no_base_is_a_first_upload() {
        let r = rig(with_words(&["a"]));
        assert!(r.cloud.base(SetKind::WordExceptions).is_none());
        r.cloud.sync_now(true);
        assert_eq!(r.remote("word-exceptions").unwrap().1, json!(["a"]));
    }

    #[test]
    fn a_conflict_is_fetched_merged_and_retried() {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let puts = AtomicUsize::new(0);
        let server = Server::start(move |r: &Request| {
            // Another computer writes just before the first PUT lands.
            if r.method == "PUT"
                && key_of(r) == "word-exceptions"
                && puts.fetch_add(1, Ordering::SeqCst) == 0
            {
                shared
                    .lock()
                    .unwrap()
                    .sets
                    .insert("word-exceptions".into(), (3, json!(["other"])));
            }
            serve(shared.clone(), r)
        });
        let r = rig_on(server, store, with_words(&["mine"]));
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        assert_eq!(
            r.remote("word-exceptions").unwrap().1,
            json!(["mine", "other"])
        );
        assert_eq!(r.config().exceptions, words(&["mine", "other"]));
        let word_puts: Vec<_> = r
            .puts()
            .into_iter()
            .filter(|p| p.route().ends_with("word-exceptions"))
            .collect();
        assert_eq!(word_puts.len(), 2);
        assert_eq!(word_puts[0].header("if-match"), Some("0"));
        assert_eq!(word_puts[1].header("if-match"), Some("3"));
    }

    #[test]
    fn conflicts_are_retried_three_times_and_then_left() {
        let server = Server::start(|r: &Request| {
            if r.method == "PUT" {
                Reply::Json(
                    409,
                    json!({ "error": { "code": "APP_CONFIG_CONFLICT", "revision": 9 } }),
                )
            } else {
                Reply::Json(200, describe(&key_of(r), 9, Some(&json!(["x"]))))
            }
        });
        let r = rig_on(server, Arc::default(), with_words(&["mine"]));
        r.host.config.lock().unwrap().excluded_apps = Config::default().excluded_apps;
        assert_eq!(r.cloud.sync_now(false), Outcome::Failed);
        let word_puts = r
            .puts()
            .iter()
            .filter(|p| p.route().ends_with("word-exceptions"))
            .count();
        assert_eq!(word_puts, 1 + MAX_CONFLICT_RETRIES);
        assert_eq!(r.cloud.view().state, "error");
    }

    #[test]
    fn a_set_the_server_cannot_hold_is_left_alone_without_looping() {
        for code in [
            "APP_CONFIG_TOO_LARGE",
            "APP_CONFIG_LIMIT_REACHED",
            "APP_CONFIG_QUOTA_EXCEEDED",
        ] {
            let server = Server::start(move |r: &Request| {
                if r.method == "PUT" {
                    Reply::Json(413, json!({ "error": { "code": code, "message": "x" } }))
                } else {
                    Reply::Json(404, json!({ "error": { "code": "APP_CONFIG_NOT_FOUND" } }))
                }
            });
            let r = rig_on(server, Arc::default(), with_words(&["a"]));
            assert_eq!(r.cloud.sync_now(false), Outcome::Refused, "{code}");
            assert_eq!(r.cloud.view().state, "refused", "{code}");
            let word_puts = r
                .puts()
                .iter()
                .filter(|p| p.route().ends_with("word-exceptions"))
                .count();
            assert_eq!(word_puts, 1, "{code}");
        }
    }

    #[test]
    fn oversized_data_is_skipped_before_it_is_sent() {
        let big = (0..40_000).map(|i| format!("слово{i}")).collect();
        let r = rig(Config {
            exceptions: big,
            ..Config::default()
        });
        assert_eq!(r.cloud.sync_now(true), Outcome::Refused);
        assert_eq!(r.cloud.view().state, "refused");
        assert!(r.remote("word-exceptions").is_none());
        assert_eq!(r.config().exceptions.len(), 40_000, "nothing lost");
    }

    #[test]
    fn more_items_than_a_later_pull_could_read_are_not_sent() {
        let r = rig(Config {
            exceptions: (0..5001).map(|i| format!("w{i}")).collect(),
            ..Config::default()
        });
        assert_eq!(r.cloud.sync_now(true), Outcome::Refused);
        assert!(r.remote("word-exceptions").is_none());
    }

    #[test]
    fn a_refused_set_may_try_again_after_a_new_local_change() {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r: &Request| {
            let refuse = r.method == "PUT" && r.json()["count"].as_u64() == Some(1);
            if refuse && key_of(r) == "word-exceptions" {
                Reply::Json(
                    413,
                    json!({ "error": { "code": "APP_CONFIG_QUOTA_EXCEEDED", "message": "x" } }),
                )
            } else {
                serve(shared.clone(), r)
            }
        });
        let r = rig_on(server, store, with_words(&["a"]));
        assert_eq!(r.cloud.sync_now(true), Outcome::Refused);
        assert_eq!(r.cloud.view().state, "refused");
        r.change(|c| c.exceptions.push("b".into()));
        assert_eq!(r.cloud.sync_now(false), Outcome::Done);
        assert_eq!(r.cloud.view().state, "idle");
        assert_eq!(r.remote("word-exceptions").unwrap().1, json!(["a", "b"]));
    }

    #[test]
    fn malformed_data_is_never_read_as_a_removal() {
        let list = |n: usize| json!((0..n).map(|i| format!("w{i}")).collect::<Vec<_>>());
        for (kind, value) in [
            (SetKind::WordExceptions, json!({ "not": "a list" })),
            (SetKind::WordExceptions, json!(["b", 5])),
            (SetKind::WordExceptions, list(5001)),
            (SetKind::AppExceptions, json!("text")),
            (SetKind::AppExceptions, json!(["com.mine.App", ""])),
            (SetKind::AppExceptions, list(513)),
            (SetKind::Rules, json!([1, 2])),
        ] {
            let mut config = with_words(&["a", "b"]);
            config.excluded_apps.push("com.mine.App".into());
            let r = rig(config.clone());
            r.cloud.sync_now(true);
            let before = r.cloud.base(kind).unwrap();
            let puts = r.puts().len();
            r.seed(kind.key(), before.revision.unwrap() + 1, value);

            assert_eq!(r.cloud.sync_now(true), Outcome::Failed, "{kind:?}");
            assert_eq!(r.cloud.view().state, "error", "{kind:?}");
            assert_eq!(
                r.host.applied.load(Ordering::SeqCst),
                0,
                "applied: {kind:?}"
            );
            assert_eq!(r.puts().len(), puts, "sent: {kind:?}");
            assert_eq!(r.config().exceptions, config.exceptions, "{kind:?}");
            assert_eq!(r.config().excluded_apps, config.excluded_apps, "{kind:?}");
            let after = r.cloud.base(kind).unwrap();
            assert_eq!(after.revision, before.revision, "remembered: {kind:?}");
            assert_eq!(after.value, before.value, "remembered: {kind:?}");
        }
    }

    #[test]
    fn a_valid_empty_list_from_the_server_still_removes() {
        let r = rig(with_words(&["a"]));
        r.cloud.sync_now(true);
        let (revision, _) = r.remote("word-exceptions").unwrap();
        r.seed("word-exceptions", revision + 1, json!([]));
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        assert_eq!(r.config().exceptions, Vec::<String>::new());
    }

    fn set_keys(cloud: &Cloud) -> Vec<&'static str> {
        cloud.view().sets.iter().map(|set| set.key).collect()
    }

    #[test]
    fn the_word_list_stays_home_until_it_is_switched_on() {
        let r = rig(with_words(&["личное"]));
        r.host.sync_words.store(false, Ordering::SeqCst);
        r.seed("word-exceptions", 5, json!(["с сервера"]));
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        r.change(|c| c.exceptions.push("ещё".into()));
        assert_eq!(r.cloud.sync_now(false), Outcome::Done);
        assert_eq!(
            r.server.count("/word-exceptions"),
            0,
            "neither read nor sent"
        );
        assert_eq!(r.config().exceptions, words(&["личное", "ещё"]));
        assert_eq!(set_keys(&r.cloud), ["rules", "app-exceptions"]);
        // The others carry on.
        assert!(r.remote("rules").is_some());
    }

    #[test]
    fn switching_the_word_list_on_syncs_it_and_off_deletes_nothing() {
        let r = rig(with_words(&["a"]));
        r.host.sync_words.store(false, Ordering::SeqCst);
        r.cloud.sync_now(true);
        r.seed("word-exceptions", 5, json!(["b"]));

        r.host.sync_words.store(true, Ordering::SeqCst);
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        assert_eq!(r.config().exceptions, words(&["a", "b"]));
        assert_eq!(
            set_keys(&r.cloud),
            ["rules", "word-exceptions", "app-exceptions"]
        );
        let (revision, value) = r.remote("word-exceptions").unwrap();

        r.host.sync_words.store(false, Ordering::SeqCst);
        r.change(|c| c.exceptions.clear());
        r.cloud.sync_now(false);
        assert_eq!(r.remote("word-exceptions"), Some((revision, value)));
        assert!(r.server.requests().iter().all(|q| q.method != "DELETE"));
    }

    #[test]
    fn a_different_account_switches_the_word_list_off() {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r| serve(shared.clone(), r));
        let host = words_host(with_words(&["a"]));
        let dir = temp_dir("cloud-words-account");
        let _clean = Cleanup(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let tokens = Arc::new(SwitchableTokens(Mutex::new(Some("first".into()))));
        let cloud = Cloud::new(
            &server.base,
            dir.join("cloud-sync.json"),
            tokens.clone(),
            host.clone(),
        );
        cloud.sync_now(true);
        assert!(host.sync_words(), "the first account keeps the choice");
        cloud.sync_now(true);
        assert!(host.sync_words(), "the same account keeps it too");
        let seen = server.count("/word-exceptions");

        *tokens.0.lock().unwrap() = Some("second".into());
        assert_eq!(cloud.sync_now(true), Outcome::Done);
        assert!(!host.sync_words());
        assert_eq!(server.count("/word-exceptions"), seen);
    }

    #[test]
    fn a_round_never_reaches_the_server_under_another_account() {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r| serve(shared.clone(), r));
        let dir = temp_dir("cloud-round-account");
        let _clean = Cleanup(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let tokens = Arc::new(SwitchableTokens(Mutex::new(Some("second".into()))));
        let cloud = Cloud::new(
            &server.base,
            dir.join("cloud-sync.json"),
            tokens,
            words_host(with_words(&["a"])),
        );
        let result = cloud.authorized("first", |_token| -> Result<(), ApiError> {
            panic!("the request must not be sent");
        });
        assert!(matches!(result, Err(SyncError::SignedOut)));
        assert!(server.requests().is_empty());
    }

    #[test]
    fn the_state_starts_over_for_a_new_account() {
        let r = rig(Config::default());
        *r.cloud.state.lock().unwrap() = State::Error;
        r.cloud.adopt_account("someone-else");
        assert_eq!(*r.cloud.state.lock().unwrap(), State::Idle);
    }

    #[test]
    fn an_unavailable_configuration_is_an_error_and_sends_nothing() {
        let r = rig(with_words(&["a"]));
        r.host.unavailable.store(true, Ordering::SeqCst);
        assert_eq!(r.cloud.sync_now(true), Outcome::Failed);
        assert_eq!(*r.cloud.state.lock().unwrap(), State::Error);
        assert!(r.server.requests().is_empty());
    }

    #[test]
    fn a_body_from_the_server_needs_a_value_and_a_revision() {
        let server = Server::start(|_| Reply::Json(200, json!({ "key": "rules", "revision": 1 })));
        let r = rig_on(server, Arc::default(), Config::default());
        assert_eq!(r.cloud.sync_now(true), Outcome::Failed);
        assert_eq!(r.host.applied.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_answer_with_only_an_etag_is_understood() {
        let server = Server::start(|r: &Request| {
            if r.method == "GET" {
                Reply::Raw(
                    200,
                    vec![("ETag".into(), "\"11\"".into())],
                    json!({ "value": ["a"] }).to_string().into_bytes(),
                )
            } else {
                Reply::Json(500, json!({}))
            }
        });
        let r = rig_on(server, Arc::default(), with_words(&["a"]));
        // `a` matches, so only the rules PUT is attempted, and fails.
        r.cloud.sync_now(true);
        let base = r.cloud.base(SetKind::WordExceptions).unwrap();
        assert_eq!(base.revision, Some(11));
    }

    #[test]
    fn a_token_turned_down_is_renewed_once() {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r: &Request| {
            if r.header("authorization") == Some("Bearer token-1") {
                Reply::Json(401, json!({}))
            } else {
                serve(shared.clone(), r)
            }
        });
        let r = rig_on(server, store, with_words(&["a"]));
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        assert_eq!(r.remote("word-exceptions").unwrap().1, json!(["a"]));
    }

    #[test]
    fn being_signed_out_does_nothing_and_loses_nothing() {
        let server = Server::start(|_| Reply::Json(500, json!({})));
        let host = words_host(with_words(&["a"]));
        let dir = temp_dir("cloud-signed-out");
        let _clean = Cleanup(dir.clone());
        let tokens = Arc::new(SwitchableTokens(Mutex::new(None)));
        let cloud = Cloud::new(
            &server.base,
            dir.join("cloud-sync.json"),
            tokens,
            host.clone(),
        );
        assert_eq!(cloud.sync_now(true), Outcome::SignedOut);
        assert!(server.requests().is_empty());
        assert_eq!(cloud.view().state, "off");
        assert_eq!(host.config().unwrap().exceptions, words(&["a"]));
    }

    #[test]
    fn being_offline_loses_nothing_and_the_change_goes_up_later() {
        let host = words_host(with_words(&["a"]));
        let dir = temp_dir("cloud-offline");
        let _clean = Cleanup(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cloud-sync.json");
        let offline = Cloud::new(
            &refused_base(),
            path.clone(),
            Arc::new(Tokens),
            host.clone(),
        );
        assert_eq!(offline.sync_now(false), Outcome::Failed);
        assert_eq!(offline.view().state, "error");
        assert_eq!(host.config().unwrap().exceptions, words(&["a"]));

        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r| serve(shared.clone(), r));
        let online = Cloud::new(&server.base, path, Arc::new(Tokens), host);
        assert_eq!(online.sync_now(false), Outcome::Done);
        assert!(store.lock().unwrap().sets.contains_key("word-exceptions"));
    }

    #[test]
    fn a_change_made_during_the_request_is_not_overwritten() {
        let r = rig(with_words(&["local"]));
        r.seed("word-exceptions", 5, json!(["remote"]));
        *r.host.interfere.lock().unwrap() = Some(Box::new(|c: &mut Config| {
            c.exceptions.push("typed-now".into())
        }));
        assert_eq!(r.cloud.sync_now(true), Outcome::Done);
        let exceptions = r.config().exceptions;
        for word in ["local", "remote", "typed-now"] {
            assert!(
                exceptions.iter().any(|w| w == word),
                "{word}: {exceptions:?}"
            );
        }
    }

    #[test]
    fn another_account_starts_from_no_base() {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r| serve(shared.clone(), r));
        let host = words_host(with_words(&["a", "b"]));
        let dir = temp_dir("cloud-switch");
        let _clean = Cleanup(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let tokens = Arc::new(SwitchableTokens(Mutex::new(Some("first".into()))));
        let cloud = Cloud::new(
            &server.base,
            dir.join("cloud-sync.json"),
            tokens.clone(),
            host.clone(),
        );
        cloud.sync_now(true);
        // With the first account's base, `a` and `b` missing remotely would
        // read as removals. For a new account they are simply local.
        store
            .lock()
            .unwrap()
            .sets
            .insert("word-exceptions".into(), (1, json!(["c"])));
        *tokens.0.lock().unwrap() = Some("second".into());
        // The new account starts with the word list off; the point here is
        // what happens to the base if it is then switched on.
        cloud.adopt_account("second");
        assert!(!host.sync_words());
        host.set_sync_words(true);
        cloud.sync_now(true);
        let exceptions = host.config().unwrap().exceptions;
        assert_eq!(exceptions, words(&["a", "b", "c"]));
    }

    #[test]
    fn the_merge_base_survives_a_restart_and_is_private() {
        let r = rig(with_words(&["a", "b"]));
        r.cloud.sync_now(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&r.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // A removal on the server after a restart is read as a removal,
        // which only works if the base came back.
        let (revision, _) = r.remote("word-exceptions").unwrap();
        r.seed("word-exceptions", revision + 1, json!(["b"]));
        let again = Cloud::new(
            &r.server.base,
            r.path.clone(),
            Arc::new(Tokens),
            r.host.clone(),
        );
        assert_eq!(again.sync_now(true), Outcome::Done);
        assert_eq!(r.config().exceptions, words(&["b"]));
    }

    #[test]
    fn a_damaged_sync_file_just_means_a_first_sync() {
        let r = rig(with_words(&["a"]));
        std::fs::write(&r.path, "{ not json").unwrap();
        let again = Cloud::new(
            &r.server.base,
            r.path.clone(),
            Arc::new(Tokens),
            r.host.clone(),
        );
        assert_eq!(again.sync_now(true), Outcome::Done);
        assert_eq!(r.remote("word-exceptions").unwrap().1, json!(["a"]));
    }

    #[test]
    fn the_view_lists_the_three_sets_with_what_is_known() {
        let r = rig(with_words(&["a", "b"]));
        r.cloud.sync_now(true);
        let view = serde_json::to_value(r.cloud.view()).unwrap();
        assert_eq!(view["state"], "idle");
        let sets = view["sets"].as_array().unwrap();
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[0]["key"], "rules");
        assert_eq!(sets[0]["title"], "Правила исправления");
        assert_eq!(sets[0]["count"], 15);
        assert!(sets[0]["updatedAt"].is_string());
        assert_eq!(sets[1]["key"], "word-exceptions");
        assert_eq!(sets[1]["count"], 2);
        assert_eq!(sets[2]["key"], "app-exceptions");
        assert_eq!(sets[2]["updatedAt"], Value::Null);
    }

    #[test]
    fn the_worker_waits_for_quiet_then_sends_once() {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r| serve(shared.clone(), r));
        let host = words_host(with_words(&["a"]));
        let dir = temp_dir("cloud-worker");
        let _clean = Cleanup(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let cloud = Cloud::with_debounce(
            &server.base,
            dir.join("cloud-sync.json"),
            Arc::new(Tokens),
            host.clone(),
            Duration::from_millis(500),
        );
        cloud.spawn();
        for _ in 0..5 {
            cloud.changed();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            server.requests().is_empty(),
            "nothing goes out while changes keep coming"
        );
        let word_puts = || {
            server
                .requests()
                .iter()
                .filter(|r| r.method == "PUT" && r.route().ends_with("word-exceptions"))
                .count()
        };
        for _ in 0..400 {
            if word_puts() > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(word_puts(), 1);
    }

    #[test]
    fn an_error_does_not_outlive_the_work_it_was_about() {
        let host = words_host(with_words(&["a"]));
        let dir = temp_dir("cloud-stale-error");
        let _clean = Cleanup(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let cloud = Cloud::new(
            &refused_base(),
            dir.join("cloud-sync.json"),
            Arc::new(Tokens),
            host.clone(),
        );
        // Only the word list needs sending, and it cannot be sent.
        cloud.file.lock().unwrap().sets.insert(
            "rules".into(),
            SetBase {
                value: rules_of(&Config::default()),
                revision: Some(1),
                updated_at: None,
                deleted_remotely: false,
            },
        );
        cloud.file.lock().unwrap().account = Some("u1".into());
        assert_eq!(cloud.sync_now(false), Outcome::Failed);
        assert_eq!(cloud.view().state, "error");
        // The user takes the change back: there is nothing left to fail.
        host.config.lock().unwrap().exceptions.clear();
        assert_eq!(cloud.sync_now(false), Outcome::Done);
        assert_eq!(cloud.view().state, "idle");
    }

    #[test]
    fn a_pull_does_not_wait() {
        let store: Arc<Mutex<Store>> = Arc::default();
        let shared = store.clone();
        let server = Server::start(move |r| serve(shared.clone(), r));
        let host = words_host(with_words(&["a"]));
        let dir = temp_dir("cloud-pull");
        let _clean = Cleanup(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let cloud = Cloud::with_debounce(
            &server.base,
            dir.join("cloud-sync.json"),
            Arc::new(Tokens),
            host,
            Duration::from_secs(600),
        );
        cloud.spawn();
        cloud.pull();
        assert!(
            server.wait_for("/rules"),
            "long before the debounce would end"
        );
    }

    #[test]
    fn state_changes_are_announced_only_when_something_runs() {
        let r = rig(with_words(&["a"]));
        r.cloud.sync_now(true);
        let announced = r.host.notified.load(Ordering::SeqCst);
        assert!(announced >= 2, "syncing, then idle");
        r.cloud.sync_now(false);
        assert_eq!(r.host.notified.load(Ordering::SeqCst), announced);
    }
}
