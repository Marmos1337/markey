//! The MarMic ID session: signing in, keeping tokens fresh, signing out.
//!
//! MarKey works in full without an account. The account adds cloud settings,
//! so nothing here may get in the way: no window, no focus and no error for
//! anything the user did not ask for. See `identity.rs` for the wire format.
//!
//! Sign-in has two routes. At launch, with no session and no «Выйти» on
//! record, MarKey quietly asks MarHub for a code; if that goes nowhere the
//! button simply stays «Войти». When the user presses it, MarKey goes through
//! MarHub if it can be opened and otherwise through the system browser.
//!
//! Network calls never run on the main thread or under the state lock.

use crate::cloud::{SyncView, TokenSource};
use crate::identity::{
    self, same_text, Client, ExchangeError, Link, MeError, Profile, RefreshOutcome, Tokens,
};
use crate::sealed::SealedStore;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How long an attempt the user started stays open.
const INTERACTIVE_TTL: u64 = 10 * 60;
/// Nothing waits on a silent attempt, so a slow MarHub cold start may still
/// sign in; the button just says «Войти» meanwhile.
const SILENT_TTL: u64 = 60;
/// Renew this long before the access token runs out.
const EARLY_REFRESH_SECS: u64 = 30;
const STORE_VERSION: u32 = 1;

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Seconds since the epoch.
pub trait Clock: Send + Sync {
    fn now(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        unix_now()
    }
}

/// How links leave MarKey.
pub trait LinkOpener: Send + Sync {
    /// In the real MarHub only. False when it cannot be reached.
    fn open_hub(&self, url: &str, silent: bool) -> bool;
    fn open_browser(&self, url: &str) -> bool;
}

/// The one account setting kept with the others.
pub trait Prefs: Send + Sync {
    fn signed_out_by_user(&self) -> bool;
    fn set_signed_out_by_user(&self, value: bool);
}

/// What the rest of the app hears.
pub trait AccountEvents: Send + Sync {
    /// Something `get_account` reports changed.
    fn changed(&self);
    /// A session has just begun. `interactive`: the user asked for it.
    fn signed_in(&self, interactive: bool);
}

/// Why no access token could be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    SignedOut,
    Offline,
    Server,
}

/// What the interface may say went wrong with an attempt the user made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountError {
    Denied,
    Network,
    Rejected,
    Server,
    Expired,
    Open,
    /// A session ended without the user asking.
    Session,
    /// The session could not be saved and lasts only until MarKey quits.
    Storage,
}

impl AccountError {
    fn as_str(self) -> &'static str {
        match self {
            AccountError::Denied => "denied",
            AccountError::Network => "network",
            AccountError::Rejected => "rejected",
            AccountError::Server => "server",
            AccountError::Expired => "expired",
            AccountError::Open => "open",
            AccountError::Session => "session",
            AccountError::Storage => "storage",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    Marhub,
    Browser,
}

impl Via {
    fn as_str(self) -> &'static str {
        match self {
            Via::Marhub => "marhub",
            Via::Browser => "browser",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Silent,
    Interactive,
    /// A retry after MarHub said it has nobody signed in.
    BrowserOnly,
}

/// The sign-in attempt waiting for its answer. There is only ever one.
struct Flow {
    state: String,
    verifier: String,
    silent: bool,
    expires: u64,
    route: Via,
}

/// A reply that matched the waiting attempt, and has consumed it.
struct Claim {
    flow: Flow,
    link: Link,
    epoch: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    access_token: String,
    refresh_token: String,
    /// Seconds since the epoch.
    expires_at: u64,
    account_id: String,
    name: String,
    /// As a `data:` URL, so the window never fetches anything.
    #[serde(default)]
    avatar: Option<String>,
    /// The address the avatar came from, to notice when it changes.
    #[serde(default)]
    avatar_source: Option<String>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Session {{ expires_at: {} }}", self.expires_at)
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    /// The server this came from. Its refresh token means nothing elsewhere,
    /// and another server has no business seeing it.
    base: String,
    session: Session,
}

#[derive(Default)]
struct Inner {
    session: Option<Session>,
    flow: Option<Flow>,
    /// A code from an attempt the user made is being exchanged.
    exchanging: bool,
    /// How the waiting attempt was opened, for the window.
    via: Option<Via>,
    error: Option<AccountError>,
    /// Bumped by anything that invalidates an attempt in progress, so a late
    /// result can tell it is no longer wanted.
    flow_epoch: u64,
    /// The same for the session, against a refresh still on the wire.
    session_epoch: u64,
    /// A session that was lost tries a silent sign-in, once.
    silent_retry_spent: bool,
    want_silent: bool,
}

impl Inner {
    /// A new or cancelled attempt replaces what the last attempt said. What
    /// happened to the session stands until a sign-in or a save succeeds.
    fn clear_attempt_error(&mut self) {
        if !matches!(
            self.error,
            Some(AccountError::Session | AccountError::Storage)
        ) {
            self.error = None;
        }
    }
}

/// `get_account`. No tokens, codes or addresses: this goes to the window.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    account_name: &'static str,
    status: &'static str,
    via: Option<&'static str>,
    error: Option<&'static str>,
    name: Option<String>,
    initials: Option<String>,
    avatar: Option<String>,
    sync_words: bool,
    sync: SyncView,
}

/// Two letters for the round badge when there is no picture.
fn initials_of(name: &str) -> Option<String> {
    let mut words = name.split_whitespace();
    let first = words.next()?;
    let picked: String = match words.next() {
        Some(second) => first
            .chars()
            .take(1)
            .chain(second.chars().take(1))
            .collect(),
        None => first.chars().take(2).collect(),
    };
    Some(picked.to_uppercase())
}

/// Start a named thread. MarKey goes on without it, but the log has to say so.
pub fn spawn_logged(name: &str, work: impl FnOnce() + Send + 'static) -> bool {
    match std::thread::Builder::new()
        .name(name.to_string())
        .spawn(work)
    {
        Ok(_) => true,
        Err(e) => {
            log::error!("cannot start the {name} thread: {e}");
            false
        }
    }
}

/// The session in the sealed file, if it is whole and belongs to this server.
fn restore_session(deps: &Deps) -> Option<Session> {
    let bytes = deps.store.load()?;
    let stored = serde_json::from_slice::<Stored>(&bytes)
        .ok()
        .filter(|s| s.version == STORE_VERSION)
        .filter(|s| {
            identity::valid_tokens(&s.session.access_token, &s.session.refresh_token)
                && !s.session.account_id.is_empty()
        });
    let Some(stored) = stored else {
        log::warn!("the stored account session cannot be read (format)");
        return None;
    };
    if stored.base != deps.client.base() {
        log::info!("the stored account session is for another server; ignoring it");
        return None;
    }
    Some(stored.session)
}

/// Everything the account needs from outside, as seams for the tests.
pub struct Deps {
    pub client: Arc<Client>,
    pub store: Arc<dyn SealedStore>,
    pub clock: Arc<dyn Clock>,
    pub opener: Arc<dyn LinkOpener>,
    pub prefs: Arc<dyn Prefs>,
    pub events: Arc<dyn AccountEvents>,
}

pub struct Account {
    deps: Deps,
    inner: Mutex<Inner>,
    /// Held for the length of a refresh. Rotation is strictly one at a time:
    /// a second request with the same token would look like theft to the
    /// server, which then revokes the whole chain.
    refresh_gate: Mutex<()>,
    /// Held around every save and clear of the sealed file, each of which
    /// looks at the session again once it has this. So a write that was on
    /// its way cannot undo a sign-out or put back a token already rotated.
    store_gate: Mutex<()>,
    /// Wakes the expiry watcher when an attempt starts.
    wake: Condvar,
}

impl Account {
    /// Restores a stored session. Whatever is wrong with the file, the answer
    /// is simply "signed out".
    pub fn new(deps: Deps) -> Account {
        let session = restore_session(&deps);
        Account {
            deps,
            inner: Mutex::new(Inner {
                session,
                ..Inner::default()
            }),
            refresh_gate: Mutex::new(()),
            store_gate: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_signed_in(&self) -> bool {
        self.lock().session.is_some()
    }

    /// Begin background work: watching for attempts to expire, and either
    /// freshening the profile or trying a silent sign-in.
    pub fn start(self: &Arc<Self>) {
        let watcher = self.clone();
        spawn_logged("markey-account-expiry", move || watcher.watch_expiry());
        let startup = self.clone();
        spawn_logged("markey-account", move || startup.on_startup());
    }

    fn on_startup(&self) {
        if self.is_signed_in() {
            self.refresh_profile();
        } else if !self.deps.prefs.signed_out_by_user() {
            self.start_flow(Mode::Silent);
        }
    }

    // -- attempts ------------------------------------------------------------

    /// «Войти». Does nothing when already signed in. Opening MarHub can take a
    /// moment, so this is called off the main thread.
    pub fn sign_in(&self) {
        if !self.is_signed_in() {
            self.start_flow(Mode::Interactive);
        }
    }

    pub fn cancel(&self) {
        {
            let mut inner = self.lock();
            inner.flow = None;
            inner.flow_epoch += 1;
            inner.exchanging = false;
            inner.via = None;
            inner.clear_attempt_error();
        }
        self.deps.events.changed();
    }

    fn start_flow(&self, mode: Mode) {
        let silent = mode == Mode::Silent;
        let Some(secrets) = identity::new_flow_secrets() else {
            log::warn!("no randomness available for a sign-in attempt");
            if !silent {
                self.lock().error = Some(AccountError::Open);
                self.deps.events.changed();
            }
            return;
        };
        let epoch = {
            let mut inner = self.lock();
            if inner.session.is_some() {
                return;
            }
            // Something the user asked for is never replaced by something
            // they did not.
            if silent && (inner.exchanging || inner.flow.as_ref().is_some_and(|f| !f.silent)) {
                return;
            }
            inner.flow_epoch += 1;
            inner.exchanging = false;
            inner.via = None;
            if !silent {
                inner.clear_attempt_error();
            }
            let ttl = if silent { SILENT_TTL } else { INTERACTIVE_TTL };
            inner.flow = Some(Flow {
                state: secrets.state.clone(),
                verifier: secrets.verifier.clone(),
                silent,
                expires: self.deps.clock.now() + ttl,
                route: if mode == Mode::BrowserOnly {
                    Via::Browser
                } else {
                    Via::Marhub
                },
            });
            self.wake.notify_all();
            inner.flow_epoch
        };
        if silent {
            self.open_silently(&secrets, epoch);
        } else {
            self.deps.events.changed();
            self.open_interactively(mode, &secrets, epoch);
        }
    }

    fn open_silently(&self, secrets: &identity::FlowSecrets, epoch: u64) {
        let url = identity::hub_authorize_url(&secrets.state, &secrets.challenge, true);
        if !self.deps.opener.open_hub(&url, true) {
            log::info!("silent sign-in: MarHub is not available");
            let mut inner = self.lock();
            if inner.flow_epoch == epoch {
                inner.flow = None;
            }
        }
    }

    fn open_interactively(&self, mode: Mode, secrets: &identity::FlowSecrets, epoch: u64) {
        let through_hub = mode == Mode::Interactive && {
            let url = identity::hub_authorize_url(&secrets.state, &secrets.challenge, false);
            self.deps.opener.open_hub(&url, false)
        };
        let via = if through_hub {
            Some(Via::Marhub)
        } else {
            identity::browser_authorize_url(
                self.deps.client.base(),
                &secrets.state,
                &secrets.challenge,
            )
            .filter(|url| self.deps.opener.open_browser(url))
            .map(|_| Via::Browser)
        };
        {
            let mut inner = self.lock();
            // A newer attempt, or a reply that was faster than `open`.
            if inner.flow_epoch != epoch {
                return;
            }
            match via {
                Some(via) => {
                    inner.via = Some(via);
                    if let Some(flow) = inner.flow.as_mut() {
                        flow.route = via;
                    }
                }
                None => {
                    log::info!("sign-in: neither MarHub nor the browser could be opened");
                    inner.flow = None;
                    inner.exchanging = false;
                    inner.via = None;
                    inner.error = Some(AccountError::Open);
                }
            }
        }
        self.deps.events.changed();
    }

    fn expire_due(&self) {
        let now = self.deps.clock.now();
        let expired_interactive = {
            let mut inner = self.lock();
            if !inner.flow.as_ref().is_some_and(|f| now >= f.expires) {
                return;
            }
            let silent = inner.flow.take().is_some_and(|f| f.silent);
            inner.flow_epoch += 1;
            if !silent {
                inner.via = None;
                inner.error = Some(AccountError::Expired);
            }
            !silent
        };
        log::info!("a sign-in attempt expired");
        if expired_interactive {
            self.deps.events.changed();
        }
    }

    /// Sleeps until the waiting attempt is due, or until there is one.
    fn watch_expiry(&self) {
        loop {
            let inner = self.lock();
            let now = self.deps.clock.now();
            match inner.flow.as_ref().map(|f| f.expires) {
                None => {
                    drop(self.wake.wait(inner).unwrap_or_else(|e| e.into_inner()));
                }
                Some(at) if at > now => {
                    drop(
                        self.wake
                            .wait_timeout(inner, Duration::from_secs(at - now))
                            .unwrap_or_else(|e| e.into_inner()),
                    );
                }
                Some(_) => {
                    drop(inner);
                    self.expire_due();
                }
            }
        }
    }

    // -- replies -------------------------------------------------------------

    /// A `markey://auth` link arrived. Never opens a window by itself, and
    /// checks the state where it is, so that a flood of forged links does not
    /// become a flood of threads.
    pub fn accept(self: &Arc<Self>, link: Link) {
        let Some(claim) = self.claim(link) else {
            return;
        };
        let silent = claim.flow.silent;
        let epoch = claim.epoch;
        let me = self.clone();
        if !spawn_logged("markey-signin", move || me.finish_reply(claim)) {
            self.conclude(epoch, silent, AccountError::Server);
        }
    }

    /// Take the waiting attempt if `link` answers it: once, and only with the
    /// right state.
    fn claim(&self, link: Link) -> Option<Claim> {
        let state = link.state()?.to_string();
        let mut inner = self.lock();
        if !inner
            .flow
            .as_ref()
            .is_some_and(|flow| same_text(&state, &flow.state))
        {
            log::info!("a sign-in reply matched no waiting attempt; ignored");
            return None;
        }
        let flow = inner.flow.take()?;
        inner.exchanging = !flow.silent;
        Some(Claim {
            flow,
            link,
            epoch: inner.flow_epoch,
        })
    }

    fn finish_reply(&self, claim: Claim) {
        let Claim { flow, link, epoch } = claim;
        if self.deps.clock.now() >= flow.expires {
            log::info!("a sign-in reply came after its attempt expired");
            self.conclude(epoch, flow.silent, AccountError::Expired);
            return;
        }
        match link {
            Link::Error { error, .. } => self.on_error_reply(&error, &flow, epoch),
            Link::Code { code, .. } => match self.deps.client.exchange(&code, &flow.verifier) {
                Ok(tokens) => self.finish_sign_in(tokens, flow.silent, epoch),
                Err(failure) => {
                    log::info!("sign-in: the code could not be exchanged ({failure:?})");
                    let error = match failure {
                        ExchangeError::Network => AccountError::Network,
                        ExchangeError::Rejected => AccountError::Rejected,
                        ExchangeError::Server => AccountError::Server,
                    };
                    self.conclude(epoch, flow.silent, error);
                }
            },
            Link::Open => {}
        }
    }

    fn on_error_reply(&self, error: &str, flow: &Flow, epoch: u64) {
        // `error` passed the link parser, so it is only [a-z_].
        log::info!("sign-in was answered with {error}");
        if flow.silent {
            return;
        }
        match error {
            "login_required" if flow.route == Via::Marhub => {
                // MarHub is there but nobody is signed in to it. The page in
                // the browser can sign in; once, with a new attempt.
                if self.lock().flow_epoch == epoch {
                    self.start_flow(Mode::BrowserOnly);
                }
            }
            "access_denied" => self.conclude(epoch, false, AccountError::Denied),
            "login_required" => self.conclude(epoch, false, AccountError::Rejected),
            _ => self.conclude(epoch, false, AccountError::Server),
        }
    }

    /// An attempt ended without a session. Only attempts the user made may
    /// have anything to say about it.
    fn conclude(&self, epoch: u64, silent: bool, error: AccountError) {
        if silent {
            return;
        }
        {
            let mut inner = self.lock();
            if inner.flow_epoch != epoch {
                return;
            }
            inner.flow_epoch += 1;
            inner.exchanging = false;
            inner.via = None;
            inner.error = Some(error);
        }
        self.deps.events.changed();
    }

    fn finish_sign_in(&self, tokens: Tokens, silent: bool, epoch: u64) {
        let profile = tokens.profile.unwrap_or_default();
        let session = Session {
            access_token: tokens.access,
            refresh_token: tokens.refresh,
            expires_at: self.deps.clock.now() + tokens.expires_in,
            account_id: profile.id.clone(),
            name: profile.name.clone(),
            avatar: None,
            avatar_source: None,
        };
        {
            let mut inner = self.lock();
            if inner.flow_epoch != epoch || inner.session.is_some() {
                drop(inner);
                log::info!("a sign-in finished after it stopped being wanted; discarded");
                self.logout_in_background(session.refresh_token);
                return;
            }
            inner.session = Some(session);
            inner.session_epoch += 1;
            inner.flow_epoch += 1;
            inner.exchanging = false;
            inner.via = None;
            inner.error = None;
            inner.silent_retry_spent = false;
            inner.want_silent = false;
        }
        self.persist();
        if !silent {
            self.deps.prefs.set_signed_out_by_user(false);
        }
        log::info!("signed in (silent: {silent})");
        self.deps.events.changed();
        self.deps.events.signed_in(!silent);
        self.apply_profile(profile);
    }

    // -- the session ---------------------------------------------------------

    /// Save the session as it is right now. Taking the current one under the
    /// gate, rather than a copy the caller made earlier, is what keeps writes
    /// from landing out of order.
    fn persist(&self) {
        let gate = self.store_gate.lock().unwrap_or_else(|e| e.into_inner());
        let Some(session) = self.lock().session.clone() else {
            return;
        };
        let stored = Stored {
            version: STORE_VERSION,
            base: self.deps.client.base().to_string(),
            session,
        };
        let saved = serde_json::to_vec(&stored)
            .map_err(|e| e.to_string())
            .and_then(|bytes| self.deps.store.save(&bytes));
        if let Err(e) = &saved {
            log::warn!("cannot save the account session; it lasts until MarKey quits: {e}");
            // What is on disk may be a refresh token rotation has just
            // killed, and replaying that next launch would revoke the chain.
            if let Err(e) = self.deps.store.clear() {
                log::warn!("cannot remove the old account session: {e}");
            }
        }
        drop(gate);
        let changed = {
            let mut inner = self.lock();
            let before = inner.error;
            if saved.is_err() {
                inner.error = Some(AccountError::Storage);
            } else if before == Some(AccountError::Storage) {
                inner.error = None;
            }
            inner.error != before
        };
        if changed {
            self.deps.events.changed();
        }
    }

    /// Empty the sealed file, unless a new session has arrived in the
    /// meantime: then that is what belongs there.
    fn clear_store(&self) {
        let _gate = self.store_gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.lock().session.is_some() {
            return;
        }
        if let Err(e) = self.deps.store.clear() {
            log::warn!("cannot remove the account session: {e}");
        }
    }

    fn logout_in_background(&self, refresh_token: String) {
        let client = self.deps.client.clone();
        spawn_logged("markey-signout", move || client.logout(&refresh_token));
    }

    /// «Выйти»: local at once, the server when it can be reached. MarKey then
    /// stays out until the user presses «Войти».
    pub fn sign_out(&self) {
        let refresh = {
            let mut inner = self.lock();
            inner.flow = None;
            inner.flow_epoch += 1;
            inner.session_epoch += 1;
            inner.exchanging = false;
            inner.via = None;
            inner.error = None;
            inner.want_silent = false;
            inner.session.take().map(|s| s.refresh_token)
        };
        self.clear_store();
        self.deps.prefs.set_signed_out_by_user(true);
        self.deps.events.changed();
        if let Some(token) = refresh {
            self.logout_in_background(token);
        }
    }

    /// The session is gone and not because the user asked.
    fn forget(&self, epoch: u64) {
        {
            let mut inner = self.lock();
            if inner.session_epoch != epoch {
                return;
            }
            inner.session = None;
            inner.session_epoch += 1;
            inner.want_silent = !inner.silent_retry_spent;
            inner.silent_retry_spent = true;
            inner.error = Some(AccountError::Session);
        }
        self.clear_store();
        self.deps.events.changed();
    }

    /// After a refresh, outside the gate: opening MarHub can take a moment.
    fn retry_silently_if_wanted(&self) {
        let wanted = std::mem::take(&mut self.lock().want_silent);
        if wanted && !self.deps.prefs.signed_out_by_user() {
            self.start_flow(Mode::Silent);
        }
    }

    fn fresh_token(&self, rejected: Option<&str>) -> Result<String, TokenError> {
        let _gate = self.refresh_gate.lock().unwrap_or_else(|e| e.into_inner());
        let (access, refresh, expires_at, epoch) = {
            let inner = self.lock();
            let session = inner.session.as_ref().ok_or(TokenError::SignedOut)?;
            (
                session.access_token.clone(),
                session.refresh_token.clone(),
                session.expires_at,
                inner.session_epoch,
            )
        };
        let fresh = expires_at > self.deps.clock.now() + EARLY_REFRESH_SECS;
        // Whoever held the gate before may already have replaced the token
        // this caller found out was bad.
        let turned_down = rejected.is_some_and(|bad| same_text(bad, &access));
        if fresh && !turned_down {
            return Ok(access);
        }
        match self.deps.client.refresh(&refresh) {
            RefreshOutcome::Renewed(tokens) => self.store_renewed(tokens, epoch),
            RefreshOutcome::Offline => Err(TokenError::Offline),
            RefreshOutcome::ServerError(status) => {
                log::info!("the server could not renew the session ({status})");
                Err(TokenError::Server)
            }
            RefreshOutcome::Refused => {
                log::info!("the server no longer accepts this session");
                self.forget(epoch);
                Err(TokenError::SignedOut)
            }
            RefreshOutcome::AnswerLost => {
                // The server may have rotated the token and its answer is
                // gone. Sending the old one again would revoke everything.
                log::info!("the answer to a refresh was lost; signing in again");
                self.forget(epoch);
                Err(TokenError::SignedOut)
            }
        }
    }

    fn store_renewed(&self, tokens: Tokens, epoch: u64) -> Result<String, TokenError> {
        let now = self.deps.clock.now();
        let renamed = {
            let mut inner = self.lock();
            let current = (inner.session_epoch == epoch)
                .then_some(())
                .and(inner.session.as_mut());
            let Some(session) = current else {
                drop(inner);
                // Signed out while this was on the wire, and this is now the
                // only live token in the chain.
                self.logout_in_background(tokens.refresh);
                return Err(TokenError::SignedOut);
            };
            session.access_token.clone_from(&tokens.access);
            session.refresh_token.clone_from(&tokens.refresh);
            session.expires_at = now + tokens.expires_in;
            let mut renamed = false;
            if let Some(profile) = &tokens.profile {
                renamed = session.name != profile.name;
                session.name.clone_from(&profile.name);
            }
            renamed
        };
        // Before anything uses it: the old refresh token is dead from now on.
        self.persist();
        if renamed {
            self.deps.events.changed();
        }
        Ok(tokens.access)
    }

    /// An access token good for at least the next moments.
    pub fn access_token(&self) -> Result<String, TokenError> {
        let result = self.fresh_token(None);
        self.retry_silently_if_wanted();
        result
    }

    /// The server turned `rejected` down before its time. A new one, unless
    /// someone has already got one.
    pub fn token_after_reject(&self, rejected: &str) -> Result<String, TokenError> {
        let result = self.fresh_token(Some(rejected));
        self.retry_silently_if_wanted();
        result
    }

    fn refresh_profile(&self) {
        let Ok(token) = self.access_token() else {
            return;
        };
        let profile = match self.deps.client.me(&token) {
            Err(MeError::Unauthorized) => match self.token_after_reject(&token) {
                Ok(token) => self.deps.client.me(&token),
                Err(_) => return,
            },
            other => other,
        };
        match profile {
            Ok(profile) => self.apply_profile(profile),
            Err(e) => log::info!("the account profile was not refreshed ({e:?})"),
        }
    }

    /// Bring the name and picture up to date. The picture is fetched outside
    /// the lock, and only when its address changed.
    fn apply_profile(&self, profile: Profile) {
        let (epoch, fetch) = {
            let inner = self.lock();
            let Some(session) = inner.session.as_ref() else {
                return;
            };
            let unchanged = session.avatar_source == profile.avatar_url
                && (session.avatar.is_some() || profile.avatar_url.is_none());
            (inner.session_epoch, !unchanged)
        };
        let avatar = if fetch {
            profile
                .avatar_url
                .as_deref()
                .and_then(|source| self.deps.client.avatar(source))
        } else {
            None
        };
        {
            let mut inner = self.lock();
            if inner.session_epoch != epoch {
                return;
            }
            let Some(session) = inner.session.as_mut() else {
                return;
            };
            let renamed = session.name != profile.name;
            if !renamed && !fetch {
                return;
            }
            session.name.clone_from(&profile.name);
            if fetch {
                session.avatar = avatar;
                session.avatar_source.clone_from(&profile.avatar_url);
            }
        }
        self.persist();
        self.deps.events.changed();
    }

    /// What the window shows, with the cloud's part supplied by the caller.
    pub fn view(&self, sync: SyncView, sync_words: bool) -> AccountView {
        // Here too, so "pending" cannot outlive its attempt even if the
        // watcher thread never started.
        self.expire_due();
        let inner = self.lock();
        let session = inner.session.as_ref();
        let pending = session.is_none()
            && (inner.exchanging || inner.flow.as_ref().is_some_and(|f| !f.silent));
        let status = if session.is_some() {
            "signedIn"
        } else if pending {
            "pending"
        } else {
            "signedOut"
        };
        AccountView {
            account_name: identity::ACCOUNT_NAME,
            status,
            via: inner.via.filter(|_| pending).map(Via::as_str),
            error: inner.error.map(AccountError::as_str),
            name: session.map(|s| s.name.clone()).filter(|n| !n.is_empty()),
            initials: session.and_then(|s| initials_of(&s.name)),
            avatar: session.and_then(|s| s.avatar.clone()),
            sync_words,
            sync,
        }
    }
}

impl TokenSource for Account {
    fn account_id(&self) -> Option<String> {
        self.lock().session.as_ref().map(|s| s.account_id.clone())
    }

    fn access_token(&self) -> Result<String, TokenError> {
        Account::access_token(self)
    }

    fn token_after_reject(&self, rejected: &str) -> Result<String, TokenError> {
        Account::token_after_reject(self, rejected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::parse_link;
    use crate::sealed::MemoryStore;
    use crate::testing::{refused_base, Reply, Request, Server};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    #[derive(Default)]
    struct ManualClock(AtomicU64);

    impl Clock for ManualClock {
        fn now(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl ManualClock {
        fn advance(&self, secs: u64) {
            self.0.fetch_add(secs, Ordering::SeqCst);
        }
    }

    struct RecordingOpener {
        hub_works: AtomicBool,
        browser_works: AtomicBool,
        hub: Mutex<Vec<(String, bool)>>,
        browser: Mutex<Vec<String>>,
    }

    impl Default for RecordingOpener {
        fn default() -> Self {
            RecordingOpener {
                hub_works: AtomicBool::new(true),
                browser_works: AtomicBool::new(true),
                hub: Mutex::default(),
                browser: Mutex::default(),
            }
        }
    }

    impl LinkOpener for RecordingOpener {
        fn open_hub(&self, url: &str, silent: bool) -> bool {
            self.hub.lock().unwrap().push((url.to_string(), silent));
            self.hub_works.load(Ordering::SeqCst)
        }
        fn open_browser(&self, url: &str) -> bool {
            self.browser.lock().unwrap().push(url.to_string());
            self.browser_works.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct MemoryPrefs(AtomicBool);

    impl Prefs for MemoryPrefs {
        fn signed_out_by_user(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
        fn set_signed_out_by_user(&self, value: bool) {
            self.0.store(value, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct Recorder {
        changed: AtomicUsize,
        silent_sign_ins: AtomicUsize,
        interactive_sign_ins: AtomicUsize,
    }

    impl AccountEvents for Recorder {
        fn changed(&self) {
            self.changed.fetch_add(1, Ordering::SeqCst);
        }
        fn signed_in(&self, interactive: bool) {
            let counter = if interactive {
                &self.interactive_sign_ins
            } else {
                &self.silent_sign_ins
            };
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Fixture {
        account: Arc<Account>,
        store: Arc<MemoryStore>,
        opener: Arc<RecordingOpener>,
        prefs: Arc<MemoryPrefs>,
        events: Arc<Recorder>,
        clock: Arc<ManualClock>,
    }

    fn fixture_with(base: &str, stored: Option<&Session>) -> Fixture {
        fixture_stored_for(base, base, stored)
    }

    /// `stored` as saved against `stored_base`, opened by a client of `base`.
    fn fixture_stored_for(base: &str, stored_base: &str, stored: Option<&Session>) -> Fixture {
        let store = Arc::new(MemoryStore::default());
        if let Some(session) = stored {
            let bytes = serde_json::to_vec(&Stored {
                version: STORE_VERSION,
                base: stored_base.to_string(),
                session: session.clone(),
            })
            .unwrap();
            store.save(&bytes).unwrap();
        }
        let opener = Arc::new(RecordingOpener::default());
        let prefs = Arc::new(MemoryPrefs::default());
        let events = Arc::new(Recorder::default());
        let clock = Arc::new(ManualClock(AtomicU64::new(1_000_000)));
        let account = Arc::new(Account::new(Deps {
            client: Arc::new(Client::new(base)),
            store: store.clone(),
            clock: clock.clone(),
            opener: opener.clone(),
            prefs: prefs.clone(),
            events: events.clone(),
        }));
        Fixture {
            account,
            store,
            opener,
            prefs,
            events,
            clock,
        }
    }

    fn fixture(base: &str) -> Fixture {
        fixture_with(base, None)
    }

    /// In the server's own format.
    fn opaque(seed: u32) -> String {
        format!("{seed:0>43}")
    }

    fn access(seed: u32) -> String {
        format!("header.payload.{seed:0>16}")
    }

    fn session(refresh_seed: u32, expires_at: u64) -> Session {
        Session {
            access_token: access(refresh_seed),
            refresh_token: opaque(refresh_seed),
            expires_at,
            account_id: "u1".into(),
            name: "Степан Иванов".into(),
            avatar: None,
            avatar_source: None,
        }
    }

    fn tokens_for(seed: u32) -> Value {
        json!({
            "accessToken": access(seed),
            "refreshToken": opaque(seed),
            "expiresIn": 900,
            "account": { "id": "u1", "name": "Степан Иванов", "avatarUrl": null },
        })
    }

    /// Exchange and refresh both succeed; each refresh hands out the next pair.
    fn happy_server() -> Server {
        let next = AtomicU64::new(100);
        Server::start(move |r: &Request| {
            let seed = next.fetch_add(1, Ordering::SeqCst) as u32;
            match r.route().rsplit('/').next().unwrap_or("") {
                "exchange" | "refresh" => Reply::Json(200, tokens_for(seed)),
                "logout" => Reply::Json(200, json!({})),
                "me" => Reply::Json(200, json!({ "account": tokens_for(1)["account"] })),
                _ => Reply::Json(404, json!({})),
            }
        })
    }

    fn state_of(url: &str) -> String {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .into_owned()
    }

    impl Fixture {
        fn last_hub_state(&self) -> String {
            state_of(&self.opener.hub.lock().unwrap().last().unwrap().0)
        }

        fn last_browser_state(&self) -> String {
            state_of(self.opener.browser.lock().unwrap().last().unwrap())
        }

        fn deliver(&self, query: &str) {
            let link = parse_link(&format!("markey://auth?{query}")).expect("a valid link");
            if let Some(claim) = self.account.claim(link) {
                self.account.finish_reply(claim);
            }
        }

        fn view(&self) -> Value {
            let sync = SyncView::off();
            serde_json::to_value(self.account.view(sync, false)).unwrap()
        }

        fn status(&self) -> String {
            self.view()["status"].as_str().unwrap().to_string()
        }

        fn error(&self) -> Value {
            self.view()["error"].clone()
        }

        fn stored(&self) -> Option<Stored> {
            self.store
                .load()
                .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        }

        fn sign_in_with_code(&self) {
            self.account.sign_in();
            let state = self.last_hub_state();
            self.deliver(&format!("code={}&state={state}", opaque(7)));
        }
    }

    #[test]
    fn the_button_goes_through_marhub_when_it_opens() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.sign_in();
        assert_eq!(f.status(), "pending");
        assert_eq!(f.view()["via"], "marhub");
        let (url, silent) = f.opener.hub.lock().unwrap()[0].clone();
        assert!(!silent);
        assert!(url.starts_with("marhub://sso/authorize?"));
        assert!(!url.contains("silent"));
        assert!(f.opener.browser.lock().unwrap().is_empty());

        f.deliver(&format!("code={}&state={}", opaque(7), f.last_hub_state()));
        assert_eq!(f.status(), "signedIn");
        assert_eq!(f.view()["name"], "Степан Иванов");
        assert_eq!(f.view()["initials"], "СИ");
        assert_eq!(f.view()["via"], Value::Null);
        assert_eq!(f.events.interactive_sign_ins.load(Ordering::SeqCst), 1);
        assert_eq!(f.events.silent_sign_ins.load(Ordering::SeqCst), 0);
        // The exchange carries exactly what the server accepts.
        let exchange = &server.requests()[0];
        assert_eq!(exchange.path, "/identity/apps/markey/exchange");
        let body = exchange.json();
        assert_eq!(body.as_object().unwrap().len(), 2);
        assert_eq!(body["code"], opaque(7));
        assert_eq!(body["code_verifier"].as_str().unwrap().len(), 43);
    }

    #[test]
    fn without_marhub_the_button_opens_the_browser() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.opener.hub_works.store(false, Ordering::SeqCst);
        f.account.sign_in();
        assert_eq!(f.status(), "pending");
        assert_eq!(f.view()["via"], "browser");
        let url = f.opener.browser.lock().unwrap()[0].clone();
        assert!(url.starts_with(&format!("{}/identity/authorize?", server.base)));
        f.deliver(&format!(
            "code={}&state={}",
            opaque(7),
            f.last_browser_state()
        ));
        assert_eq!(f.status(), "signedIn");
    }

    #[test]
    fn when_nothing_can_be_opened_the_window_says_so() {
        let f = fixture(&refused_base());
        f.opener.hub_works.store(false, Ordering::SeqCst);
        f.opener.browser_works.store(false, Ordering::SeqCst);
        f.account.sign_in();
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), "open");
    }

    #[test]
    fn a_reply_with_the_wrong_state_changes_nothing() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.sign_in();
        f.deliver(&format!(
            "code={}&state=wrong-state-of-the-right-size",
            opaque(7)
        ));
        assert_eq!(f.status(), "pending");
        assert_eq!(server.count("/exchange"), 0);
        // And the real answer still works afterwards.
        f.deliver(&format!("code={}&state={}", opaque(7), f.last_hub_state()));
        assert_eq!(f.status(), "signedIn");
    }

    #[test]
    fn a_reply_with_nothing_waiting_is_ignored() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.deliver(&format!(
            "code={}&state=nothing-is-waiting-for-this",
            opaque(7)
        ));
        assert_eq!(f.status(), "signedOut");
        assert_eq!(server.count("/exchange"), 0);
    }

    #[test]
    fn a_reply_is_used_once() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.sign_in();
        let state = f.last_hub_state();
        f.deliver(&format!("code={}&state={state}", opaque(7)));
        f.deliver(&format!("code={}&state={state}", opaque(7)));
        assert_eq!(server.count("/exchange"), 1);
    }

    #[test]
    fn a_new_attempt_replaces_the_old_one() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.sign_in();
        let old = f.last_hub_state();
        f.account.sign_in();
        let new = f.last_hub_state();
        assert_ne!(old, new);
        f.deliver(&format!("code={}&state={old}", opaque(7)));
        assert_eq!(f.status(), "pending");
        assert_eq!(server.count("/exchange"), 0);
        f.deliver(&format!("code={}&state={new}", opaque(7)));
        assert_eq!(f.status(), "signedIn");
    }

    #[test]
    fn an_interactive_attempt_expires_after_ten_minutes() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.sign_in();
        let state = f.last_hub_state();
        f.clock.advance(INTERACTIVE_TTL - 1);
        f.account.expire_due();
        assert_eq!(f.status(), "pending");
        f.clock.advance(1);
        f.deliver(&format!("code={}&state={state}", opaque(7)));
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), "expired");
        assert_eq!(server.count("/exchange"), 0);
    }

    #[test]
    fn the_watcher_expires_an_attempt_nobody_answers() {
        let f = fixture(&refused_base());
        f.account.sign_in();
        f.clock.advance(INTERACTIVE_TTL);
        f.account.expire_due();
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), "expired");
    }

    #[test]
    fn a_silent_attempt_expires_after_a_minute_and_says_nothing() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.on_startup();
        let state = f.last_hub_state();
        f.clock.advance(SILENT_TTL);
        f.deliver(&format!("code={}&state={state}", opaque(7)));
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), Value::Null);
        assert_eq!(server.count("/exchange"), 0);
    }

    #[test]
    fn a_late_marhub_can_still_sign_in_within_the_minute() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.on_startup();
        f.clock.advance(SILENT_TTL - 1);
        f.deliver(&format!("code={}&state={}", opaque(7), f.last_hub_state()));
        assert_eq!(f.status(), "signedIn");
    }

    #[test]
    fn startup_asks_marhub_quietly() {
        let server = happy_server();
        let f = fixture(&server.base);
        let before = f.events.changed.load(Ordering::SeqCst);
        f.account.on_startup();
        let (url, silent) = f.opener.hub.lock().unwrap()[0].clone();
        assert!(silent);
        assert!(url.contains("silent=1"));
        assert_eq!(f.status(), "signedOut", "the button stays «Войти»");
        assert_eq!(f.events.changed.load(Ordering::SeqCst), before);
        f.deliver(&format!("code={}&state={}", opaque(7), f.last_hub_state()));
        assert_eq!(f.status(), "signedIn");
        assert_eq!(f.events.silent_sign_ins.load(Ordering::SeqCst), 1);
        assert_eq!(f.events.interactive_sign_ins.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn silent_failures_never_show() {
        let server = Server::start(|_| Reply::Json(500, json!({})));
        for query in [
            "error=login_required",
            "error=temporarily_unavailable",
            "error=access_denied",
            "code=CODE",
        ] {
            let f = fixture(&server.base);
            f.account.on_startup();
            let state = f.last_hub_state();
            let query = query.replace("CODE", &opaque(7));
            f.deliver(&format!("{query}&state={state}"));
            assert_eq!(f.status(), "signedOut", "{query}");
            assert_eq!(f.error(), Value::Null, "{query}");
            assert!(f.opener.browser.lock().unwrap().is_empty(), "{query}");
        }
    }

    #[test]
    fn without_marhub_startup_stays_signed_out_with_nothing_waiting() {
        let f = fixture(&refused_base());
        f.opener.hub_works.store(false, Ordering::SeqCst);
        f.account.on_startup();
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), Value::Null);
        assert!(f.account.lock().flow.is_none());
        assert!(f.opener.browser.lock().unwrap().is_empty());
    }

    #[test]
    fn someone_who_signed_out_is_not_signed_back_in() {
        let f = fixture(&refused_base());
        f.prefs.set_signed_out_by_user(true);
        f.account.on_startup();
        assert!(f.opener.hub.lock().unwrap().is_empty());
    }

    #[test]
    fn a_stored_session_means_no_sign_in_at_startup() {
        let server = happy_server();
        let f = fixture_with(&server.base, Some(&session(1, 2_000_000)));
        assert_eq!(f.status(), "signedIn");
        f.account.on_startup();
        assert!(f.opener.hub.lock().unwrap().is_empty());
        assert_eq!(server.count("/me"), 1);
    }

    #[test]
    fn a_denied_attempt_says_denied() {
        let f = fixture(&refused_base());
        f.account.sign_in();
        f.deliver(&format!("error=access_denied&state={}", f.last_hub_state()));
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), "denied");
    }

    #[test]
    fn other_errors_are_the_servers_fault() {
        let f = fixture(&refused_base());
        f.account.sign_in();
        f.deliver(&format!(
            "error=temporarily_unavailable&state={}",
            f.last_hub_state()
        ));
        assert_eq!(f.error(), "server");
    }

    #[test]
    fn login_required_moves_to_the_browser_once_with_a_new_attempt() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.sign_in();
        let first = f.last_hub_state();
        f.deliver(&format!("error=login_required&state={first}"));
        assert_eq!(f.status(), "pending");
        assert_eq!(f.view()["via"], "browser");
        let second = f.last_browser_state();
        assert_ne!(first, second);
        assert_eq!(
            f.opener.hub.lock().unwrap().len(),
            1,
            "MarHub is not asked twice"
        );

        // The browser's page answering the same way ends it.
        f.deliver(&format!("error=login_required&state={second}"));
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), "rejected");
        assert_eq!(f.opener.browser.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_rejected_code_says_rejected_and_a_dead_network_says_network() {
        let server = Server::start(|_| Reply::Json(400, json!({ "error": { "code": "BAD" } })));
        let f = fixture(&server.base);
        f.sign_in_with_code();
        assert_eq!(f.error(), "rejected");
        let f = fixture(&refused_base());
        f.sign_in_with_code();
        assert_eq!(f.error(), "network");
        let server = Server::start(|_| Reply::Json(503, json!({})));
        let f = fixture(&server.base);
        f.sign_in_with_code();
        assert_eq!(f.error(), "server");
    }

    #[test]
    fn cancelling_drops_the_attempt() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.sign_in();
        let state = f.last_hub_state();
        f.account.cancel();
        assert_eq!(f.status(), "signedOut");
        f.deliver(&format!("code={}&state={state}", opaque(7)));
        assert_eq!(f.status(), "signedOut");
        assert_eq!(server.count("/exchange"), 0);
    }

    #[test]
    fn a_sign_in_finishing_after_a_cancel_is_discarded() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.account.sign_in();
        let link = parse_link(&format!(
            "markey://auth?code={}&state={}",
            opaque(7),
            f.last_hub_state()
        ))
        .unwrap();
        let claim = f.account.claim(link).unwrap();
        f.account.cancel();
        f.account.finish_reply(claim);
        assert_eq!(f.status(), "signedOut");
        assert!(f.store.load().is_none());
        assert!(server.wait_for("/logout"), "the tokens must not linger");
    }

    #[test]
    fn a_session_is_saved_and_restored() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.sign_in_with_code();
        let stored = f.stored().unwrap();
        assert_eq!(stored.session.refresh_token, opaque(100));
        // As after a restart.
        let again = fixture_with(&server.base, Some(&stored.session));
        assert_eq!(again.status(), "signedIn");
        assert_eq!(again.view()["name"], "Степан Иванов");
    }

    #[test]
    fn damaged_or_outdated_storage_means_signed_out() {
        let base = refused_base();
        let f = fixture(&base);
        for bad in [
            b"not json".to_vec(),
            br#"{"version":99,"session":{}}"#.to_vec(),
            serde_json::to_vec(&json!({
                "version": STORE_VERSION,
                "base": base,
                "session": {
                    "accessToken": "short", "refreshToken": "short", "expiresAt": 1,
                    "accountId": "u1", "name": "x"
                }
            }))
            .unwrap(),
        ] {
            f.store.save(&bad).unwrap();
            let account = Account::new(Deps {
                client: Arc::new(Client::new(&base)),
                store: f.store.clone(),
                clock: f.clock.clone(),
                opener: f.opener.clone(),
                prefs: f.prefs.clone(),
                events: f.events.clone(),
            });
            assert!(!account.is_signed_in());
        }
    }

    #[test]
    fn a_fresh_token_is_used_without_asking_the_server() {
        let server = happy_server();
        let f = fixture_with(&server.base, Some(&session(1, 1_000_000 + 900)));
        assert_eq!(f.account.access_token().unwrap(), access(1));
        assert_eq!(server.count("/refresh"), 0);
        // Within the last 30 seconds it is renewed early.
        f.clock.advance(900 - EARLY_REFRESH_SECS);
        assert_eq!(f.account.access_token().unwrap(), access(100));
        assert_eq!(server.count("/refresh"), 1);
    }

    #[test]
    fn a_rotated_token_is_on_disk_before_it_is_used() {
        let server = happy_server();
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        assert_eq!(f.account.access_token().unwrap(), access(100));
        let stored = f.stored().unwrap();
        assert_eq!(stored.session.refresh_token, opaque(100));
        assert_eq!(stored.session.expires_at, 1_000_000 + 900);
        // The strict body the server takes, with the token that was current.
        let refresh = &server.requests()[0];
        assert_eq!(refresh.path, "/identity/apps/markey/refresh");
        assert_eq!(refresh.json(), json!({ "refreshToken": opaque(1) }));
        // The next renewal uses the new token.
        f.clock.advance(1000);
        f.account.access_token().unwrap();
        assert_eq!(
            server.requests()[1].json(),
            json!({ "refreshToken": opaque(100) })
        );
    }

    #[test]
    fn when_saving_fails_the_stale_token_is_removed_and_the_new_one_still_works() {
        let server = happy_server();
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        f.store.fail.store(true, Ordering::SeqCst);
        assert_eq!(f.account.access_token().unwrap(), access(100));
        assert!(
            f.store.load().is_none(),
            "a spent token must not wait on disk for the next launch"
        );
        assert_eq!(f.error(), "storage");
        assert_eq!(f.status(), "signedIn");
        // The next save that works takes the message away.
        f.store.fail.store(false, Ordering::SeqCst);
        f.clock.advance(1000);
        f.account.access_token().unwrap();
        assert_eq!(f.error(), Value::Null);
        assert!(f.stored().is_some());
    }

    #[test]
    fn a_session_lost_by_itself_stays_on_record_until_a_sign_in_works() {
        let responses = Arc::new(AtomicU64::new(0));
        let counter = responses.clone();
        let server = Server::start(move |r: &Request| {
            if r.route().ends_with("/refresh") {
                return Reply::Json(401, json!({}));
            }
            let seed = counter.fetch_add(1, Ordering::SeqCst) as u32;
            Reply::Json(200, tokens_for(100 + seed))
        });
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        f.account.access_token().unwrap_err();
        assert_eq!(f.error(), "session");

        // The silent retry going nowhere does not hide it…
        f.deliver(&format!(
            "error=login_required&state={}",
            f.last_hub_state()
        ));
        assert_eq!(f.error(), "session");
        // …and neither does trying and backing out.
        f.account.sign_in();
        assert_eq!(f.error(), "session");
        f.account.cancel();
        assert_eq!(f.error(), "session");

        f.sign_in_with_code();
        assert_eq!(f.status(), "signedIn");
        assert_eq!(f.error(), Value::Null);
    }

    #[test]
    fn a_silent_sign_in_that_works_clears_a_lost_session_too() {
        let server = Server::start(|r: &Request| {
            if r.route().ends_with("/refresh") {
                Reply::Json(401, json!({}))
            } else {
                Reply::Json(200, tokens_for(100))
            }
        });
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        f.account.access_token().unwrap_err();
        assert_eq!(f.error(), "session");
        f.deliver(&format!("code={}&state={}", opaque(7), f.last_hub_state()));
        assert_eq!(f.status(), "signedIn");
        assert_eq!(f.error(), Value::Null);
    }

    #[test]
    fn a_session_from_another_server_is_ignored_and_never_sent() {
        let server = happy_server();
        let f = fixture_stored_for(
            &server.base,
            "https://another.example",
            Some(&session(1, 1)),
        );
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.account.access_token(), Err(TokenError::SignedOut));
        assert_eq!(server.count("/refresh"), 0);
    }

    /// Holds the first save open until told to let go.
    struct PausingStore {
        memory: MemoryStore,
        entered: Mutex<std::sync::mpsc::Sender<()>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl SealedStore for PausingStore {
        fn load(&self) -> Option<Vec<u8>> {
            self.memory.load()
        }
        fn save(&self, plain: &[u8]) -> Result<(), String> {
            self.entered.lock().unwrap().send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            self.memory.save(plain)
        }
        fn clear(&self) -> Result<(), String> {
            self.memory.clear()
        }
    }

    #[test]
    fn a_write_in_progress_cannot_undo_a_sign_out() {
        use std::sync::mpsc::channel;
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let store = Arc::new(PausingStore {
            memory: MemoryStore::default(),
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
        });
        let base = refused_base();
        let f = fixture(&base);
        let account = Arc::new(Account::new(Deps {
            client: Arc::new(Client::new(&base)),
            store: store.clone(),
            clock: f.clock.clone(),
            opener: f.opener.clone(),
            prefs: f.prefs.clone(),
            events: f.events.clone(),
        }));
        account.lock().session = Some(session(1, 2_000_000));

        let writer = account.clone();
        let writing = std::thread::spawn(move || writer.persist());
        entered_rx.recv().unwrap();
        // The write is on its way into the file when the user signs out.
        let leaving = account.clone();
        let signing_out = std::thread::spawn(move || leaving.sign_out());
        std::thread::sleep(Duration::from_millis(50));
        release_tx.send(()).unwrap();
        writing.join().unwrap();
        signing_out.join().unwrap();
        assert!(store.load().is_none(), "the session must not come back");
    }

    #[test]
    fn a_save_with_nothing_left_to_save_writes_nothing() {
        let f = fixture_with(&refused_base(), Some(&session(1, 2_000_000)));
        f.account.sign_out();
        assert!(f.store.load().is_none());
        f.account.persist();
        assert!(f.store.load().is_none());
    }

    #[test]
    fn a_late_profile_update_cannot_put_back_a_spent_token() {
        let png = b"\x89PNG\r\n\x1a\n-image".to_vec();
        let server = Server::start(move |r: &Request| {
            if r.route().ends_with("/refresh") {
                Reply::Json(200, tokens_for(100))
            } else {
                Reply::Slow(
                    Duration::from_millis(200),
                    Box::new(Reply::Raw(
                        200,
                        vec![("Content-Type".into(), "image/png".into())],
                        png.clone(),
                    )),
                )
            }
        });
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        let account = f.account.clone();
        let avatar_url = Some(format!("{}/slow.png", server.base));
        let updating = std::thread::spawn(move || {
            account.apply_profile(Profile {
                id: "u1".into(),
                name: "Степан Иванов".into(),
                avatar_url,
            })
        });
        assert!(server.wait_for("/slow.png"));
        f.account.access_token().unwrap();
        updating.join().unwrap();
        let stored = f.stored().unwrap();
        assert_eq!(stored.session.refresh_token, opaque(100));
        assert!(stored.session.avatar.is_some());
    }

    #[test]
    fn pending_cannot_outlive_its_attempt_without_the_watcher() {
        let f = fixture(&refused_base());
        f.account.sign_in();
        assert_eq!(f.status(), "pending");
        f.clock.advance(INTERACTIVE_TTL);
        // Nothing but reading the state.
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), "expired");
    }

    #[test]
    fn a_lost_answer_forgets_the_session_and_never_resends() {
        let server = Server::start(|_| Reply::Hangup);
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        assert_eq!(f.account.access_token(), Err(TokenError::SignedOut));
        assert_eq!(f.status(), "signedOut");
        assert_eq!(f.error(), "session");
        assert!(f.stored().is_none());
        assert_eq!(f.account.access_token(), Err(TokenError::SignedOut));
        assert_eq!(server.count("/refresh"), 1, "the old token went out once");
    }

    #[test]
    fn a_garbled_success_counts_as_a_lost_answer() {
        let server = Server::start(|_| Reply::Json(200, json!({ "nonsense": true })));
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        assert_eq!(f.account.access_token(), Err(TokenError::SignedOut));
        assert!(f.stored().is_none());
        assert_eq!(server.count("/refresh"), 1);
    }

    #[test]
    fn server_errors_keep_the_session() {
        let server = Server::start(|_| Reply::Json(503, json!({})));
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        assert_eq!(f.account.access_token(), Err(TokenError::Server));
        assert_eq!(f.status(), "signedIn");
        assert_eq!(f.error(), Value::Null);
        assert_eq!(f.stored().unwrap().session.refresh_token, opaque(1));
        assert!(f.opener.hub.lock().unwrap().is_empty());
    }

    #[test]
    fn a_refused_connection_keeps_the_session() {
        let f = fixture_with(&refused_base(), Some(&session(1, 1)));
        assert_eq!(f.account.access_token(), Err(TokenError::Offline));
        assert_eq!(f.status(), "signedIn");
        assert_eq!(f.stored().unwrap().session.refresh_token, opaque(1));
        assert!(f.opener.hub.lock().unwrap().is_empty());
    }

    #[test]
    fn a_refused_token_forgets_the_session() {
        for status in [400, 401, 403] {
            let server = Server::start(move |_| Reply::Json(status, json!({})));
            let f = fixture_with(&server.base, Some(&session(1, 1)));
            assert_eq!(f.account.access_token(), Err(TokenError::SignedOut));
            assert_eq!(f.status(), "signedOut");
            assert_eq!(f.error(), "session");
            assert!(f.stored().is_none());
        }
    }

    #[test]
    fn a_lost_session_tries_a_silent_sign_in_once() {
        let server = Server::start(|_| Reply::Json(401, json!({})));
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        f.account.access_token().unwrap_err();
        {
            let hub = f.opener.hub.lock().unwrap();
            assert_eq!(hub.len(), 1);
            assert!(hub[0].1, "silently");
        }
        // Signed back in, then lost again: that session gets its own try…
        f.deliver(&format!(
            "error=login_required&state={}",
            f.last_hub_state()
        ));
        f.account.lock().session = Some(session(2, 1));
        f.account.lock().session_epoch += 1;
        f.account.access_token().unwrap_err();
        // …but only the one, until a sign-in succeeds.
        assert_eq!(f.opener.hub.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_lost_session_does_not_come_back_after_a_deliberate_sign_out() {
        let server = Server::start(|_| Reply::Json(401, json!({})));
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        f.prefs.set_signed_out_by_user(true);
        f.account.access_token().unwrap_err();
        assert!(f.opener.hub.lock().unwrap().is_empty());
    }

    #[test]
    fn only_one_refresh_runs_at_a_time() {
        let next = Arc::new(AtomicU64::new(100));
        let counter = next.clone();
        let server = Server::start(move |_| {
            let seed = counter.fetch_add(1, Ordering::SeqCst) as u32;
            Reply::Slow(
                Duration::from_millis(150),
                Box::new(Reply::Json(200, tokens_for(seed))),
            )
        });
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        let workers: Vec<_> = (0..6)
            .map(|_| {
                let account = f.account.clone();
                std::thread::spawn(move || account.access_token().unwrap())
            })
            .collect();
        let tokens: Vec<String> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(server.count("/refresh"), 1);
        assert!(tokens.iter().all(|t| *t == access(100)));
    }

    #[test]
    fn a_token_turned_down_early_is_renewed_once() {
        let server = happy_server();
        let f = fixture_with(&server.base, Some(&session(1, 2_000_000)));
        let bad = f.account.access_token().unwrap();
        assert_eq!(f.account.token_after_reject(&bad).unwrap(), access(100));
        assert_eq!(server.count("/refresh"), 1);
        // Another worker holding the same bad token does not renew again.
        assert_eq!(f.account.token_after_reject(&bad).unwrap(), access(100));
        assert_eq!(server.count("/refresh"), 1);
    }

    #[test]
    fn signing_out_is_immediate_and_remembered() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.sign_in_with_code();
        assert!(!f.prefs.signed_out_by_user());
        f.account.sign_out();
        assert_eq!(f.status(), "signedOut");
        assert!(f.stored().is_none());
        assert!(f.prefs.signed_out_by_user());
        assert!(server.wait_for("/logout"));
        let logout = server
            .requests()
            .into_iter()
            .find(|r| r.route().ends_with("/logout"))
            .unwrap();
        assert_eq!(logout.json(), json!({ "refreshToken": opaque(100) }));
    }

    #[test]
    fn signing_out_works_with_no_server() {
        let f = fixture_with(&refused_base(), Some(&session(1, 2_000_000)));
        f.account.sign_out();
        assert_eq!(f.status(), "signedOut");
        assert!(f.prefs.signed_out_by_user());
    }

    #[test]
    fn a_chosen_sign_in_clears_the_mark_and_a_silent_one_would_not() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.prefs.set_signed_out_by_user(true);
        f.sign_in_with_code();
        assert_eq!(f.status(), "signedIn");
        assert!(!f.prefs.signed_out_by_user());
    }

    #[test]
    fn a_refresh_landing_after_a_sign_out_is_not_kept() {
        let server = Server::start(|r| {
            if r.route().ends_with("/refresh") {
                Reply::Slow(
                    Duration::from_millis(200),
                    Box::new(Reply::Json(200, tokens_for(100))),
                )
            } else {
                Reply::Json(200, json!({}))
            }
        });
        let f = fixture_with(&server.base, Some(&session(1, 1)));
        let account = f.account.clone();
        let worker = std::thread::spawn(move || account.access_token());
        assert!(server.wait_for("/refresh"));
        f.account.sign_out();
        assert_eq!(worker.join().unwrap(), Err(TokenError::SignedOut));
        assert!(f.stored().is_none(), "nothing returns to disk");
    }

    #[test]
    fn the_window_never_gets_a_secret() {
        let server = happy_server();
        let f = fixture(&server.base);
        f.sign_in_with_code();
        let view = f.view().to_string();
        let stored = f.stored().unwrap();
        for secret in [&stored.session.access_token, &stored.session.refresh_token] {
            assert!(!view.contains(secret.as_str()));
        }
        assert!(!format!("{:?}", stored.session).contains(&stored.session.refresh_token));
    }

    #[test]
    fn the_avatar_is_stored_and_only_refetched_when_it_changes() {
        use base64::Engine;
        let png = b"\x89PNG\r\n\x1a\n-image".to_vec();
        let picture = png.clone();
        let base = Arc::new(Mutex::new(String::new()));
        let shared = base.clone();
        let server = Server::start(move |r| match r.route().rsplit('/').next().unwrap_or("") {
            "exchange" => {
                let mut tokens = tokens_for(100);
                tokens["account"]["avatarUrl"] =
                    json!(format!("{}/avatars/1.png", shared.lock().unwrap()));
                Reply::Json(200, tokens)
            }
            "1.png" => Reply::Raw(
                200,
                vec![("Content-Type".into(), "image/png".into())],
                picture.clone(),
            ),
            _ => Reply::Json(404, json!({})),
        });
        *base.lock().unwrap() = server.base.clone();
        let f = fixture(&server.base);
        f.sign_in_with_code();
        let expected = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        assert_eq!(f.view()["avatar"], expected);
        assert_eq!(
            f.stored().unwrap().session.avatar.as_deref(),
            Some(&*expected)
        );
        assert_eq!(server.count("/1.png"), 1);
        let same = Profile {
            id: "u1".into(),
            name: "Степан Иванов".into(),
            avatar_url: Some(format!("{}/avatars/1.png", server.base)),
        };
        f.account.apply_profile(same);
        assert_eq!(server.count("/1.png"), 1);
    }

    #[test]
    fn a_picture_from_elsewhere_means_initials() {
        let server = Server::start(|r| {
            if r.route().ends_with("/exchange") {
                let mut tokens = tokens_for(100);
                tokens["account"]["avatarUrl"] = json!("https://evil.example/a.png");
                Reply::Json(200, tokens)
            } else {
                Reply::Json(404, json!({}))
            }
        });
        let f = fixture(&server.base);
        f.sign_in_with_code();
        assert_eq!(f.view()["avatar"], Value::Null);
        assert_eq!(f.view()["initials"], "СИ");
    }

    #[test]
    fn initials_come_from_the_first_two_words() {
        assert_eq!(initials_of("Степан Иванов Петрович").as_deref(), Some("СИ"));
        assert_eq!(initials_of("  степан ").as_deref(), Some("СТ"));
        assert_eq!(initials_of("A").as_deref(), Some("A"));
        assert_eq!(initials_of("   "), None);
        assert_eq!(initials_of(""), None);
    }

    #[test]
    fn the_view_has_the_shape_the_window_reads() {
        let f = fixture(&refused_base());
        let view = f.view();
        assert_eq!(view["accountName"], "MarMic ID");
        assert_eq!(view["status"], "signedOut");
        assert_eq!(view["syncWords"], false);
        for field in ["via", "error", "name", "initials", "avatar"] {
            assert_eq!(view[field], Value::Null, "{field}");
        }
        assert_eq!(view["sync"]["state"], "off");
    }
}
