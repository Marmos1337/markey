//! MarMic ID on the wire: `markey://` links, PKCE and the Identity server's API.
//!
//! The sign-in flow itself is in [`crate::account`]; this is the part that can
//! be checked without any state. Nothing here logs a token, a code, a verifier,
//! a state or a whole address, and the types that hold one do not print it.
//!
//! `MARKEY_IDENTITY_URL` points MarKey at another server, a local mock in
//! practice (`tools/mock_identity.py`). Only https, or http to this machine,
//! is ever used, the same rule the update feed has.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::time::Duration;
use url::Url;

/// The account's name in the interface. It may change, so it lives only here.
pub const ACCOUNT_NAME: &str = "MarMic ID";
pub const APP_ID: &str = "markey";
pub const RETURN_URL: &str = "markey://auth";
pub const DEFAULT_BASE: &str = "https://hub.mic.marhub.ru";
const HUB_AUTHORIZE: &str = "marhub://sso/authorize";

const USER_AGENT: &str = concat!("MarKey/", env!("CARGO_PKG_VERSION"));

/// Name lookup and connecting get their own budgets, smaller than the total.
/// A timeout reported as one of these proves nothing was sent; if the total
/// could run out first, a slow connect would look like a lost answer and cost
/// the user their session.
const TIMEOUT_RESOLVE: Duration = Duration::from_secs(6);
const TIMEOUT_CONNECT: Duration = Duration::from_secs(10);
const TIMEOUT_TOTAL: Duration = Duration::from_secs(30);

const MAX_LINK: usize = 4096;
/// An account answer can carry the avatar inline: about 700 KB of base64.
const MAX_TOKEN_BODY: u64 = 1024 * 1024;
/// Enough to read the error code from a refusal.
const MAX_ERROR_BODY: u64 = 16 * 1024;
const MAX_TOKEN: usize = 8192;
const MAX_NAME_CHARS: usize = 80;
const MAX_AVATAR: usize = 512 * 1024;
/// No access token is believed to last longer than a year.
const MAX_LIFETIME_SECS: f64 = 365.0 * 24.0 * 3600.0;

/// The server to talk to: `MARKEY_IDENTITY_URL` when it is allowed, else the
/// real one.
pub fn base_url() -> String {
    match std::env::var("MARKEY_IDENTITY_URL") {
        Ok(text) => clean_base(&text).unwrap_or_else(|| {
            log::warn!("MARKEY_IDENTITY_URL is not https or this machine; ignoring it");
            DEFAULT_BASE.to_string()
        }),
        Err(_) => DEFAULT_BASE.to_string(),
    }
}

/// https, or plain http to this machine, with no user name or password that
/// could make the address read as one host and connect to another.
pub fn allowed_url(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    match url.scheme() {
        "https" => !host.is_empty(),
        "http" => matches!(host, "127.0.0.1" | "localhost" | "[::1]"),
        _ => false,
    }
}

/// `text` as a base address without a trailing slash, when it is allowed.
fn clean_base(text: &str) -> Option<String> {
    let url = Url::parse(text.trim()).ok()?;
    if url.query().is_some() || url.fragment().is_some() || !allowed_url(&url) {
        return None;
    }
    Some(url.as_str().trim_end_matches('/').to_string())
}

// -- PKCE -------------------------------------------------------------------

/// What one sign-in attempt keeps to itself, and what it shows.
pub struct FlowSecrets {
    pub state: String,
    pub verifier: String,
    pub challenge: String,
}

/// The S256 challenge for `verifier`: 43 unpadded base64url characters.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A fresh random state and verifier, or `None` if the OS has no randomness
/// to give.
pub fn new_flow_secrets() -> Option<FlowSecrets> {
    let mut verifier = [0u8; 32];
    let mut state = [0u8; 24];
    getrandom::fill(&mut verifier).ok()?;
    getrandom::fill(&mut state).ok()?;
    let verifier = URL_SAFE_NO_PAD.encode(verifier);
    Some(FlowSecrets {
        state: URL_SAFE_NO_PAD.encode(state),
        challenge: challenge_for(&verifier),
        verifier,
    })
}

/// Compare without stopping at the first difference.
pub fn same_text(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |seen, (x, y)| seen | (x ^ y))
            == 0
}

/// MarHub signs in against the real server only. Against any other server its
/// code means nothing and would be handed to a server it was not issued for, so
/// then MarHub is not asked at all: no silent sign-in, and the button goes to
/// the browser.
pub fn hub_serves(base: &str) -> bool {
    base == DEFAULT_BASE
}

/// `marhub://sso/authorize?…`.
pub fn hub_authorize_url(state: &str, challenge: &str, silent: bool) -> String {
    let mut url = Url::parse(HUB_AUTHORIZE).expect("the constant is a URL");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("app", APP_ID)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("return", RETURN_URL);
        if silent {
            query.append_pair("silent", "1");
        }
    }
    url.into()
}

/// `<base>/identity/authorize?…`, the sign-in page in the system browser.
pub fn browser_authorize_url(base: &str, state: &str, challenge: &str) -> Option<String> {
    let mut url = Url::parse(&format!("{base}/identity/authorize")).ok()?;
    url.query_pairs_mut()
        .append_pair("app", APP_ID)
        .append_pair("code_challenge", challenge)
        .append_pair("state", state)
        .append_pair("return", RETURN_URL);
    Some(url.into())
}

// -- links ------------------------------------------------------------------

/// What a `markey://` link asks for. Anything on the computer can open one,
/// so nothing is taken from it unless every field checks out.
#[derive(PartialEq, Eq)]
pub enum Link {
    /// `markey://`: show MarKey.
    Open,
    Code {
        code: String,
        state: String,
    },
    Error {
        error: String,
        state: String,
    },
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Link::Open => f.write_str("Link::Open"),
            Link::Code { .. } => f.write_str("Link::Code"),
            Link::Error { .. } => f.write_str("Link::Error"),
        }
    }
}

impl Link {
    pub fn state(&self) -> Option<&str> {
        match self {
            Link::Open => None,
            Link::Code { state, .. } | Link::Error { state, .. } => Some(state),
        }
    }
}

/// True when command-line arguments carry a `markey:` link. The link itself
/// reaches the app through the deep-link plugin; this only says that a second
/// launch was a link rather than the user opening MarKey again.
pub fn carries_link(args: &[String]) -> bool {
    args.iter().skip(1).any(|arg| {
        arg.get(..7)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("markey:"))
    })
}

fn valid_state(text: &str) -> bool {
    (16..=256).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
}

/// The server's one-time codes and refresh tokens: exactly 43 base64url
/// characters, and it answers 400 to anything else.
fn valid_opaque(text: &str) -> bool {
    text.len() == 43
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn valid_error(text: &str) -> bool {
    (1..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// Read a `markey://` link. `None` for anything not exactly as expected.
pub fn parse_link(raw: &str) -> Option<Link> {
    if raw.len() > MAX_LINK {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "markey"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return None;
    }
    match url.host_str() {
        None | Some("") => url.query().is_none().then_some(Link::Open),
        Some("auth") => parse_auth(&url),
        Some(_) => None,
    }
}

fn parse_auth(url: &Url) -> Option<Link> {
    let (mut code, mut state, mut error) = (None, None, None);
    for (name, value) in url.query_pairs() {
        let slot = match name.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "error" => &mut error,
            _ => continue,
        };
        // Which of two values a server meant is not something to guess.
        if slot.is_some() {
            return None;
        }
        *slot = Some(value.into_owned());
    }
    let state = state.filter(|s| valid_state(s))?;
    match (code, error) {
        (Some(code), None) if valid_opaque(&code) => Some(Link::Code { code, state }),
        (None, Some(error)) if valid_error(&error) => Some(Link::Error { error, state }),
        _ => None,
    }
}

// -- HTTP -------------------------------------------------------------------

/// Why a request produced no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// Provably never reached the server: name lookup failed, the connection
    /// was refused, the network is down.
    NotSent,
    /// The server may have received it, and its answer is gone. A refresh
    /// token sent this way may already be spent.
    AnswerLost,
}

/// Roughly what went wrong, for the log. `ureq`'s own messages can contain
/// the address.
fn error_kind(error: &ureq::Error) -> &'static str {
    match error {
        ureq::Error::HostNotFound => "dns",
        ureq::Error::ConnectionFailed => "connect",
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::Io(_) => "io",
        ureq::Error::Protocol(_) => "protocol",
        ureq::Error::BodyExceedsLimit(_) => "too large",
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => "tls",
        _ => "other",
    }
}

/// Whether `error` proves the request never left this machine.
///
/// The default is no: a refresh token that may have been delivered must never
/// be sent again, so only failures that happen before the first byte count.
/// A connection is never reused (see [`Http::new`]), so "refused" cannot mean
/// a stale socket that had already carried the request.
fn nothing_sent(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        ureq::Error::Timeout(reason) => {
            matches!(reason, ureq::Timeout::Resolve | ureq::Timeout::Connect)
        }
        ureq::Error::Io(io) => io_proves_nothing_sent(io),
        _ => false,
    }
}

fn io_proves_nothing_sent(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::ConnectionRefused | ErrorKind::AddrNotAvailable => true,
        // A certificate is only ever checked during the handshake, before
        // any of the request goes out. This is what a captive portal gets.
        ErrorKind::InvalidData => error.to_string().contains("invalid peer certificate"),
        _ => is_unreachable(error.raw_os_error()),
    }
}

/// No route to the server: what an offline machine gets when the name was
/// still cached. Matched by number because the `ErrorKind`s are newer than
/// the crate's minimum Rust.
fn is_unreachable(code: Option<i32>) -> bool {
    #[cfg(target_os = "macos")]
    const CODES: [i32; 2] = [51, 65];
    #[cfg(target_os = "windows")]
    const CODES: [i32; 2] = [10051, 10065];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    const CODES: [i32; 2] = [101, 113];
    code.is_some_and(|c| CODES.contains(&c))
}

/// An answer that arrived. The body of a non-2xx answer is read only as far as
/// it is short and whole: a long error page must not look like a lost answer.
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
}

impl Answer {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The verbs that carry a JSON body.
#[derive(Clone, Copy)]
pub enum Write {
    Post,
    Put,
}

/// The HTTP client for one Identity server: no redirects, no connection reuse,
/// bounded answers.
pub struct Http {
    base: String,
    agent: ureq::Agent,
}

impl Http {
    /// Connections are not kept between requests. These calls are rare, and a
    /// reused socket that turns out to be dead would fail like a lost answer.
    pub fn new(base: &str) -> Http {
        let agent = ureq::Agent::config_builder()
            .https_only(base.starts_with("https://"))
            .http_status_as_error(false)
            .max_redirects(0)
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .timeout_global(Some(TIMEOUT_TOTAL))
            .timeout_resolve(Some(TIMEOUT_RESOLVE))
            .timeout_connect(Some(TIMEOUT_CONNECT))
            .user_agent(USER_AGENT)
            .build()
            .into();
        Http {
            base: base.to_string(),
            agent,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn finish(
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        limit: u64,
    ) -> Result<Answer, NetError> {
        let mut response = result.map_err(|e| {
            let sent = if nothing_sent(&e) {
                "not sent"
            } else {
                "maybe sent"
            };
            log::info!("identity request failed ({}; {sent})", error_kind(&e));
            if nothing_sent(&e) {
                NetError::NotSent
            } else {
                NetError::AnswerLost
            }
        })?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let etag = header("etag");
        let content_type = header("content-type");
        let body = if (200..300).contains(&status) {
            response
                .body_mut()
                .with_config()
                .limit(limit)
                .read_to_vec()
                .map_err(|e| {
                    log::info!("identity answer unreadable ({})", error_kind(&e));
                    NetError::AnswerLost
                })?
        } else {
            response
                .body_mut()
                .with_config()
                .limit(MAX_ERROR_BODY)
                .read_to_vec()
                .unwrap_or_default()
        };
        Ok(Answer {
            status,
            body,
            etag,
            content_type,
        })
    }

    pub fn get(&self, path: &str, bearer: Option<&str>, limit: u64) -> Result<Answer, NetError> {
        self.get_url(&format!("{}{path}", self.base), bearer, limit)
    }

    pub fn get_url(&self, url: &str, bearer: Option<&str>, limit: u64) -> Result<Answer, NetError> {
        let mut request = self.agent.get(url);
        if let Some(token) = bearer {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        Self::finish(request.call(), limit)
    }

    /// POST or PUT `body`. `if_match` guards against overwriting a newer copy.
    pub fn send(
        &self,
        verb: Write,
        path: &str,
        bearer: Option<&str>,
        if_match: Option<&str>,
        body: &Value,
        limit: u64,
    ) -> Result<Answer, NetError> {
        let url = format!("{}{path}", self.base);
        let mut request = match verb {
            Write::Post => self.agent.post(&url),
            Write::Put => self.agent.put(&url),
        };
        if let Some(token) = bearer {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(revision) = if_match {
            request = request.header("If-Match", revision);
        }
        Self::finish(request.send_json(body), limit)
    }
}

// -- the Identity API --------------------------------------------------------

/// `{id, name, avatarUrl}`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

/// A new pair of tokens.
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub expires_in: u64,
    pub profile: Option<Profile>,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tokens {{ expires_in: {} }}", self.expires_in)
    }
}

#[derive(Deserialize)]
struct RawAccount {
    id: Value,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "avatarUrl")]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct RawTokens {
    #[serde(rename = "accessToken")]
    access: String,
    #[serde(rename = "refreshToken")]
    refresh: String,
    #[serde(rename = "expiresIn")]
    expires_in: f64,
    #[serde(default)]
    account: Option<RawAccount>,
}

#[derive(Deserialize)]
struct RawMe {
    account: RawAccount,
}

/// Printable and bounded: an access token ends up in a header.
fn valid_access(token: &str) -> bool {
    (16..=MAX_TOKEN).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic())
}

/// Whether a stored pair of tokens could still be used.
pub fn valid_tokens(access: &str, refresh: &str) -> bool {
    valid_access(access) && valid_opaque(refresh)
}

fn profile_of(raw: RawAccount) -> Option<Profile> {
    let id = match raw.id {
        Value::String(id) if !id.is_empty() && id.len() <= 128 => id,
        Value::Number(id) => id.to_string(),
        _ => return None,
    };
    Some(Profile {
        id,
        name: raw
            .name
            .unwrap_or_default()
            .trim()
            .chars()
            .take(MAX_NAME_CHARS)
            .collect(),
        avatar_url: raw.avatar_url.filter(|url| !url.is_empty()),
    })
}

fn parse_tokens(body: &[u8]) -> Option<Tokens> {
    let raw: RawTokens = serde_json::from_slice(body).ok()?;
    if !valid_tokens(&raw.access, &raw.refresh) || raw.expires_in.is_nan() || raw.expires_in < 1.0 {
        return None;
    }
    let profile = match raw.account {
        Some(account) => Some(profile_of(account)?),
        None => None,
    };
    Some(Tokens {
        access: raw.access,
        refresh: raw.refresh,
        expires_in: raw.expires_in.min(MAX_LIFETIME_SECS) as u64,
        profile,
    })
}

/// Why a code could not be exchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeError {
    Network,
    Rejected,
    Server,
}

/// How a refresh ended, and so what happens to the session.
#[derive(Debug)]
pub enum RefreshOutcome {
    Renewed(Tokens),
    /// The server refused the token.
    Refused,
    /// The token may be spent and the new one is out of reach: sending the
    /// old one again would look like theft and revoke the whole chain.
    AnswerLost,
    /// Nothing reached the server; the token is as good as it was.
    Offline,
    /// The server is unwell and did not take the token.
    ServerError(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeError {
    Unauthorized,
    Failed,
}

/// Calls for MarKey's own app id.
pub struct Client {
    http: Http,
}

fn app_path(call: &str) -> String {
    format!("/identity/apps/{APP_ID}/{call}")
}

impl Client {
    pub fn new(base: &str) -> Client {
        Client {
            http: Http::new(base),
        }
    }

    pub fn base(&self) -> &str {
        self.http.base()
    }

    pub fn exchange(&self, code: &str, verifier: &str) -> Result<Tokens, ExchangeError> {
        let answer = self
            .http
            .send(
                Write::Post,
                &app_path("exchange"),
                None,
                None,
                &json!({ "code": code, "code_verifier": verifier }),
                MAX_TOKEN_BODY,
            )
            .map_err(|_| ExchangeError::Network)?;
        match answer.status {
            200..=299 => match parse_tokens(&answer.body) {
                Some(tokens) if tokens.profile.is_some() => Ok(tokens),
                _ => Err(ExchangeError::Server),
            },
            400 | 401 | 403 => Err(ExchangeError::Rejected),
            _ => Err(ExchangeError::Server),
        }
    }

    pub fn refresh(&self, refresh_token: &str) -> RefreshOutcome {
        let answer = self.http.send(
            Write::Post,
            &app_path("refresh"),
            None,
            None,
            &json!({ "refreshToken": refresh_token }),
            MAX_TOKEN_BODY,
        );
        let answer = match answer {
            Ok(answer) => answer,
            Err(NetError::NotSent) => return RefreshOutcome::Offline,
            Err(NetError::AnswerLost) => return RefreshOutcome::AnswerLost,
        };
        match answer.status {
            // A garbled 200 means the rotation happened and the result did
            // not make it, the same as no answer at all.
            200..=299 => parse_tokens(&answer.body)
                .map_or(RefreshOutcome::AnswerLost, RefreshOutcome::Renewed),
            400 | 401 | 403 => RefreshOutcome::Refused,
            status => RefreshOutcome::ServerError(status),
        }
    }

    /// Best effort: the session is already gone from this computer.
    pub fn logout(&self, refresh_token: &str) {
        let result = self.http.send(
            Write::Post,
            &app_path("logout"),
            None,
            None,
            &json!({ "refreshToken": refresh_token }),
            MAX_TOKEN_BODY,
        );
        if !result.is_ok_and(|answer| answer.is_success()) {
            log::info!("the server did not confirm the sign-out");
        }
    }

    pub fn me(&self, access_token: &str) -> Result<Profile, MeError> {
        let answer = self
            .http
            .get(&app_path("me"), Some(access_token), MAX_TOKEN_BODY)
            .map_err(|_| MeError::Failed)?;
        match answer.status {
            200..=299 => serde_json::from_slice::<RawMe>(&answer.body)
                .ok()
                .and_then(|me| profile_of(me.account))
                .ok_or(MeError::Failed),
            401 => Err(MeError::Unauthorized),
            _ => Err(MeError::Failed),
        }
    }

    /// `source` as a `data:` URL: as it came when it already is one, fetched
    /// when it is an address on the Identity server. Anything else is `None`,
    /// and the interface shows initials.
    pub fn avatar(&self, source: &str) -> Option<String> {
        if source.starts_with("data:") {
            return clean_data_avatar(source);
        }
        // Resolved against the server, so a relative address works and a
        // scheme-relative one to somewhere else is caught below.
        let url = Url::parse(self.http.base()).ok()?.join(source).ok()?;
        if !same_origin(self.http.base(), &url) {
            return None;
        }
        let answer = self
            .http
            .get_url(url.as_str(), None, MAX_AVATAR as u64)
            .ok()?;
        if answer.status != 200 {
            return None;
        }
        let declared = answer
            .content_type
            .as_deref()?
            .split(';')
            .next()?
            .trim()
            .to_ascii_lowercase();
        avatar_data_url(&declared, &answer.body)
    }
}

fn same_origin(base: &str, url: &Url) -> bool {
    let Ok(base) = Url::parse(base) else {
        return false;
    };
    url.username().is_empty()
        && url.password().is_none()
        && url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default()
}

/// Does `bytes` start the way `kind` says an image of that type starts?
fn matches_image(kind: &str, bytes: &[u8]) -> bool {
    match kind {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

fn avatar_data_url(kind: &str, bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_AVATAR || !matches_image(kind, bytes) {
        return None;
    }
    Some(format!("data:{kind};base64,{}", STANDARD.encode(bytes)))
}

fn clean_data_avatar(source: &str) -> Option<String> {
    if source.len() > MAX_AVATAR * 4 / 3 + 64 {
        return None;
    }
    let rest = source.strip_prefix("data:")?;
    let (kind, payload) = rest.split_once(";base64,")?;
    let bytes = STANDARD.decode(payload).ok()?;
    avatar_data_url(kind, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{refused_base, Reply, Server};

    #[test]
    fn marhub_is_asked_only_for_the_server_it_signs_in_to() {
        assert!(hub_serves(DEFAULT_BASE));
        assert!(hub_serves(
            &clean_base("https://hub.mic.marhub.ru/").unwrap()
        ));
        for other in ["http://127.0.0.1:8788", "https://another.example"] {
            assert!(!hub_serves(&clean_base(other).unwrap()), "{other}");
        }
    }

    #[test]
    fn pkce_matches_rfc_7636_appendix_b() {
        // The appendix's own random bytes, verifier and challenge.
        let random: [u8; 32] = [
            116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212,
            37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        ];
        let verifier = URL_SAFE_NO_PAD.encode(random);
        assert_eq!(verifier, "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            challenge_for(&verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn fresh_secrets_fit_what_marhub_accepts() {
        let a = new_flow_secrets().unwrap();
        let b = new_flow_secrets().unwrap();
        assert_eq!(a.verifier.len(), 43);
        assert_eq!(a.state.len(), 32);
        assert_eq!(a.challenge.len(), 43);
        assert!(valid_state(&a.state));
        assert_eq!(a.challenge, challenge_for(&a.verifier));
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
    }

    #[test]
    fn the_hub_link_is_what_marhub_parses() {
        let silent = Url::parse(&hub_authorize_url("s".repeat(32).as_str(), "c", true)).unwrap();
        assert_eq!(silent.scheme(), "marhub");
        assert_eq!(silent.host_str(), Some("sso"));
        assert_eq!(silent.path(), "/authorize");
        let pairs: Vec<(String, String)> = silent.query_pairs().into_owned().collect();
        let get = |n: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert_eq!(get("app"), "markey");
        assert_eq!(get("return"), "markey://auth");
        assert_eq!(get("code_challenge"), "c");
        assert_eq!(get("silent"), "1");
        assert!(!hub_authorize_url("x", "c", false).contains("silent"));
    }

    #[test]
    fn the_browser_link_lands_on_the_identity_page() {
        let url = browser_authorize_url("https://hub.mic.marhub.ru", "st", "ch").unwrap();
        assert!(url.starts_with("https://hub.mic.marhub.ru/identity/authorize?"));
        assert!(url.contains("app=markey"));
        assert!(url.contains("code_challenge=ch"));
        assert!(url.contains("state=st"));
    }

    const STATE: &str = "abcdefghijklmnopqrstuvwxyz012345";
    /// The shape of the server's codes and refresh tokens.
    const OPAQUE: &str = "abcdefghijklmnopqrstuvwxyz0123456789-_ABCDE";

    #[test]
    fn well_formed_links_are_read() {
        assert_eq!(parse_link("markey://"), Some(Link::Open));
        assert_eq!(parse_link("markey:///"), Some(Link::Open));
        assert_eq!(
            parse_link(&format!("markey://auth?code={OPAQUE}&state={STATE}")),
            Some(Link::Code {
                code: OPAQUE.into(),
                state: STATE.into()
            })
        );
        assert_eq!(
            parse_link(&format!("markey://auth/?error=access_denied&state={STATE}")),
            Some(Link::Error {
                error: "access_denied".into(),
                state: STATE.into()
            })
        );
        // The scheme is case-insensitive by definition, and an extra
        // parameter a future server adds is not a reason to refuse.
        assert!(parse_link(&format!("MARKEY://auth?code={OPAQUE}&state={STATE}")).is_some());
        assert!(parse_link(&format!("markey://auth?code={OPAQUE}&state={STATE}&iss=x")).is_some());
    }

    #[test]
    fn anything_else_is_refused() {
        let state = STATE;
        let code = OPAQUE;
        let long_link = format!("markey://auth?state={state}&code={}", "a".repeat(5000));
        for bad in [
            format!("https://auth?code={code}&state={state}"),
            format!("marhub://auth?code={code}&state={state}"),
            format!("markey://other?code={code}&state={state}"),
            format!("markey://AUTH?code={code}&state={state}"),
            format!("markey://auth:8080?code={code}&state={state}"),
            format!("markey://user@auth?code={code}&state={state}"),
            format!("markey://user:pw@auth?code={code}&state={state}"),
            format!("markey://auth/extra?code={code}&state={state}"),
            format!("markey://auth?code={code}&state={state}#frag"),
            // Duplicates of anything that matters.
            format!("markey://auth?code={code}&code={code}&state={state}"),
            format!("markey://auth?code={code}&state={state}&state={state}"),
            format!("markey://auth?error=x&error=y&state={state}"),
            // A reply must say which attempt it answers and what happened.
            format!("markey://auth?code={code}"),
            format!("markey://auth?state={state}"),
            // Both at once is not a reply anything sends.
            format!("markey://auth?code={code}&error=x&state={state}"),
            // Bounds and alphabets.
            format!("markey://auth?code={code}&state=short"),
            format!("markey://auth?code={code}&state={}", "a".repeat(257)),
            format!("markey://auth?code={code}&state=a%20b{state}"),
            format!("markey://auth?code=short&state={state}"),
            format!("markey://auth?code={}&state={state}", "a".repeat(44)),
            format!("markey://auth?code={}.&state={state}", "a".repeat(42)),
            format!("markey://auth?code={}%20&state={state}", "a".repeat(42)),
            format!("markey://auth?error=Upper&state={state}"),
            format!("markey://auth?error=has-dash&state={state}"),
            format!("markey://auth?error={}&state={state}", "a".repeat(65)),
            "markey://?code=x".to_string(),
            long_link,
            "not a url".to_string(),
        ] {
            assert_eq!(parse_link(&bad), None, "{bad}");
        }
    }

    #[test]
    fn a_debug_print_of_a_link_shows_no_values() {
        let link = Link::Code {
            code: "secret-code".into(),
            state: "secret-state".into(),
        };
        assert!(!format!("{link:?}").contains("secret"));
    }

    #[test]
    fn second_launches_with_a_link_are_recognised() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(carries_link(&args(&["markey.exe", "markey://auth?x=1"])));
        assert!(carries_link(&args(&["markey.exe", "MARKEY://"])));
        assert!(!carries_link(&args(&["markey.exe"])));
        assert!(!carries_link(&args(&["markey.exe", "--flag"])));
        // The program's own path is not a link, whatever it is called.
        assert!(!carries_link(&args(&["markey:weird", "--flag"])));
        // Not a boundary inside a multi-byte character.
        assert!(!carries_link(&args(&["markey.exe", "привет мир"])));
    }

    #[test]
    fn only_https_or_this_machine_can_be_the_server() {
        for good in [
            "https://hub.mic.marhub.ru",
            "https://hub.mic.marhub.ru/",
            "http://127.0.0.1:8788",
            "http://localhost:8788/",
            "http://[::1]:8788",
        ] {
            let base = clean_base(good).unwrap_or_else(|| panic!("{good}"));
            assert!(!base.ends_with('/'), "{base}");
        }
        for bad in [
            "http://example.com",
            "http://127.0.0.1.evil.com",
            "http://127.0.0.1:80@evil.com",
            "https://user:pw@hub.mic.marhub.ru",
            "ftp://127.0.0.1",
            "file:///etc/passwd",
            "https://hub.mic.marhub.ru/?x=1",
            "",
        ] {
            assert_eq!(clean_base(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_failures_before_the_first_byte_prove_nothing_was_sent() {
        use std::io::{Error, ErrorKind};
        let io = |kind: ErrorKind| ureq::Error::Io(Error::new(kind, "x"));
        for sure in [
            ureq::Error::HostNotFound,
            ureq::Error::ConnectionFailed,
            ureq::Error::Timeout(ureq::Timeout::Resolve),
            ureq::Error::Timeout(ureq::Timeout::Connect),
            io(ErrorKind::ConnectionRefused),
            io(ErrorKind::AddrNotAvailable),
            ureq::Error::Io(Error::new(
                ErrorKind::InvalidData,
                "invalid peer certificate: UnknownIssuer",
            )),
        ] {
            assert!(nothing_sent(&sure), "{sure:?}");
        }
        for unsure in [
            ureq::Error::Timeout(ureq::Timeout::Global),
            ureq::Error::Timeout(ureq::Timeout::SendRequest),
            ureq::Error::Timeout(ureq::Timeout::RecvResponse),
            io(ErrorKind::ConnectionReset),
            io(ErrorKind::BrokenPipe),
            io(ErrorKind::UnexpectedEof),
            io(ErrorKind::InvalidData),
            ureq::Error::BodyExceedsLimit(1),
        ] {
            assert!(!nothing_sent(&unsure), "{unsure:?}");
        }
    }

    fn tokens_json() -> Value {
        json!({
            "accessToken": "header.payload.signature",
            "refreshToken": OPAQUE,
            "expiresIn": 900,
            "account": { "id": "u1", "name": "Степан", "avatarUrl": null }
        })
    }

    #[test]
    fn an_exchange_sends_the_code_and_the_verifier() {
        let server = Server::start(|_| Reply::Json(200, tokens_json()));
        let tokens = Client::new(&server.base)
            .exchange("code-1234", "verifier")
            .unwrap();
        assert_eq!(tokens.access, "header.payload.signature");
        assert_eq!(tokens.expires_in, 900);
        assert_eq!(tokens.profile.unwrap().name, "Степан");
        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/identity/apps/markey/exchange");
        assert_eq!(
            request.json(),
            json!({ "code": "code-1234", "code_verifier": "verifier" })
        );
    }

    #[test]
    fn exchange_failures_are_told_apart() {
        for (status, want) in [
            (400, ExchangeError::Rejected),
            (401, ExchangeError::Rejected),
            (403, ExchangeError::Rejected),
            (404, ExchangeError::Server),
            (500, ExchangeError::Server),
        ] {
            let server = Server::start(move |_| Reply::Json(status, json!({})));
            assert_eq!(
                Client::new(&server.base).exchange("c", "v").unwrap_err(),
                want
            );
        }
        // A success that says nothing useful.
        let server = Server::start(|_| Reply::Json(200, json!({ "accessToken": "short" })));
        assert_eq!(
            Client::new(&server.base).exchange("c", "v").unwrap_err(),
            ExchangeError::Server
        );
        // Without the account there is no id to keep cloud data apart by.
        let mut no_account = tokens_json();
        no_account.as_object_mut().unwrap().remove("account");
        let server = Server::start(move |_| Reply::Json(200, no_account.clone()));
        assert_eq!(
            Client::new(&server.base).exchange("c", "v").unwrap_err(),
            ExchangeError::Server
        );
        assert_eq!(
            Client::new(&refused_base()).exchange("c", "v").unwrap_err(),
            ExchangeError::Network
        );
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let server = Server::start(|r| {
            if r.route().ends_with("/exchange") {
                Reply::Raw(
                    302,
                    vec![("Location".into(), "/elsewhere".into())],
                    Vec::new(),
                )
            } else {
                Reply::Json(200, tokens_json())
            }
        });
        assert_eq!(
            Client::new(&server.base).exchange("c", "v").unwrap_err(),
            ExchangeError::Server
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn an_oversized_success_is_a_lost_answer_not_a_crash() {
        let server = Server::start(|_| Reply::Raw(200, Vec::new(), vec![b'x'; 70 * 1024]));
        assert!(matches!(
            Client::new(&server.base).refresh("old"),
            RefreshOutcome::AnswerLost
        ));
    }

    #[test]
    fn a_long_error_page_is_still_a_server_error() {
        let server = Server::start(|_| Reply::Raw(503, Vec::new(), vec![b'x'; 200 * 1024]));
        assert!(matches!(
            Client::new(&server.base).refresh("old"),
            RefreshOutcome::ServerError(503)
        ));
    }

    #[test]
    fn an_answer_carrying_a_full_size_avatar_fits() {
        let mut body = tokens_json();
        let picture = data_url("image/png", &[PNG, &vec![0; 500 * 1024]].concat());
        assert!(picture.len() > 650 * 1024);
        body["account"]["avatarUrl"] = json!(picture);
        let server = Server::start(move |_| Reply::Json(200, body.clone()));
        let tokens = Client::new(&server.base).exchange("c", "v").unwrap();
        assert_eq!(tokens.profile.unwrap().avatar_url, Some(picture));
    }

    #[test]
    fn numeric_account_ids_are_accepted() {
        let mut body = tokens_json();
        body["account"]["id"] = json!(42);
        let tokens = parse_tokens(body.to_string().as_bytes()).unwrap();
        assert_eq!(tokens.profile.unwrap().id, "42");
        body["account"]["id"] = json!(null);
        assert!(parse_tokens(body.to_string().as_bytes()).is_none());
    }

    #[test]
    fn a_token_that_could_not_be_a_header_is_refused() {
        let mut body = tokens_json();
        body["accessToken"] = json!("has a space in it, sadly");
        assert!(parse_tokens(body.to_string().as_bytes()).is_none());
        let mut body = tokens_json();
        body["refreshToken"] = json!("line\r\nbreak0000000000");
        assert!(parse_tokens(body.to_string().as_bytes()).is_none());
        // The server only ever accepts its own format back.
        let mut body = tokens_json();
        body["refreshToken"] = json!("too-short-to-be-one-of-theirs");
        assert!(parse_tokens(body.to_string().as_bytes()).is_none());
        let mut body = tokens_json();
        body["expiresIn"] = json!(0);
        assert!(parse_tokens(body.to_string().as_bytes()).is_none());
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-rest-of-the-image";

    fn data_url(kind: &str, bytes: &[u8]) -> String {
        format!("data:{kind};base64,{}", STANDARD.encode(bytes))
    }

    #[test]
    fn data_avatars_must_be_small_real_images() {
        let client = Client::new("https://hub.mic.marhub.ru");
        let good = data_url("image/png", PNG);
        assert_eq!(client.avatar(&good), Some(good.clone()));
        let jpeg = data_url("image/jpeg", &[0xFF, 0xD8, 0xFF, 0xE0, 1]);
        assert_eq!(client.avatar(&jpeg), Some(jpeg.clone()));
        let webp = data_url("image/webp", b"RIFF\0\0\0\0WEBPVP8 ");
        assert_eq!(client.avatar(&webp), Some(webp.clone()));
        for bad in [
            data_url("image/svg+xml", b"<svg onload=alert(1)>"),
            data_url("text/html", b"<script>"),
            // Says PNG, is not.
            data_url("image/png", b"GIF89a"),
            data_url("image/png", &[PNG, &vec![0; MAX_AVATAR]].concat()),
            "data:image/png,rawbytes".to_string(),
            "data:image/png;base64,!!!notbase64".to_string(),
        ] {
            assert_eq!(client.avatar(&bad), None, "{bad:.40}");
        }
    }

    #[test]
    fn avatars_come_only_from_the_identity_server() {
        let server = Server::start(|r| {
            if r.route() == "/a.png" {
                Reply::Raw(
                    200,
                    vec![("Content-Type".into(), "image/png".into())],
                    PNG.to_vec(),
                )
            } else if r.route() == "/page.png" {
                Reply::Raw(
                    200,
                    vec![("Content-Type".into(), "text/html".into())],
                    b"<html>".to_vec(),
                )
            } else {
                Reply::Raw(404, Vec::new(), Vec::new())
            }
        });
        let client = Client::new(&server.base);
        assert_eq!(
            client.avatar(&format!("{}/a.png", server.base)),
            Some(data_url("image/png", PNG))
        );
        assert_eq!(client.avatar(&format!("{}/page.png", server.base)), None);
        assert_eq!(client.avatar(&format!("{}/none.png", server.base)), None);
        // Relative to the Identity server, as the server may send it.
        assert_eq!(client.avatar("/a.png"), Some(data_url("image/png", PNG)));
        assert_eq!(client.avatar("a.png"), Some(data_url("image/png", PNG)));
        assert_eq!(client.avatar("https://evil.example/a.png"), None);
        assert_eq!(client.avatar("//evil.example/a.png"), None);
        assert_eq!(client.avatar("javascript:alert(1)"), None);
        assert_eq!(client.avatar("not a url"), None);
        let other_port = format!("{}/a.png", refused_base());
        assert_eq!(client.avatar(&other_port), None);
    }
}
