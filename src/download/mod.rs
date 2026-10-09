//! HTTP download helpers.
//!
//! Every fetch follows redirects itself — one request per hop, at most five, each `Location` an
//! absolute https URL — so a credential can be decided per hop. The credential itself, a GitHub
//! token from the environment and the second attempt through the GitHub API for a private asset,
//! is the `github` feature's (`Credentials`, `set_credentials`); without it no fetch ever
//! carries an `Authorization` header.

#[cfg(feature = "github")]
mod github;

#[cfg(feature = "github")]
#[cfg_attr(docsrs, doc(cfg(feature = "github")))]
pub use github::{set_credentials, Credentials};

use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;
use ureq::config::{Config, ConfigBuilder};
use ureq::http::header::{HeaderValue, AUTHORIZATION, LOCATION};
use ureq::http::Uri;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::typestate::{AgentScope, WithoutBody};
use ureq::{Body, RequestBuilder};

/// HTTP timeouts for downloads; `None` disables a bound.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Cap on establishing the connection.
    pub connect: Option<Duration>,
    /// Cap on a single request, connect through transfer — applied per request of a redirect
    /// chain, not across the fetch or the run (ureq's per-call `timeout_global`).
    pub global: Option<Duration>,
}

impl Default for Timeouts {
    /// 30 s to connect, 120 s per request — enough for a large tarball on a slow link, while a
    /// stalled peer can't hang the build.
    fn default() -> Self {
        Self {
            connect: Some(Duration::from_secs(30)),
            global: Some(Duration::from_secs(120)),
        }
    }
}

impl Timeouts {
    /// Build from the CLI flags: `--no-timeout` removes every bound; `--timeout <secs>` sets the
    /// per-request timeout (connect stays at the default); neither keeps the default.
    pub fn from_cli(timeout_secs: Option<u64>, no_timeout: bool) -> Timeouts {
        if no_timeout {
            Timeouts {
                connect: None,
                global: None,
            }
        } else if let Some(secs) = timeout_secs {
            Timeouts {
                global: Some(Duration::from_secs(secs)),
                ..Timeouts::default()
            }
        } else {
            Timeouts::default()
        }
    }
}

static TIMEOUTS: OnceLock<Timeouts> = OnceLock::new();

/// Override the process-wide download timeouts. Intended to be called once at startup (the CLI
/// derives them from `--timeout` / `--no-timeout`); the library default applies if never set, and a
/// later call is ignored. The shared agents capture the timeouts when the first download builds
/// them, so call this before any fetch — set after that, the values are inert.
pub fn set_timeouts(timeouts: Timeouts) {
    let _ = TIMEOUTS.set(timeouts);
}

fn timeouts() -> Timeouts {
    TIMEOUTS.get().copied().unwrap_or_default()
}

/// How every request identifies itself, to the registry and to every other host: the crate,
/// its version and where to find it.
pub const USER_AGENT: &str = concat!(
    "npm-utils/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/gronke/npm-utils)"
);
/// Redirects a fetch follows before giving up.
const MAX_REDIRECTS: usize = 5;
/// Tries per fetch: the one retry covers a connection GitHub drops mid-transfer.
const ATTEMPTS: u32 = 2;
/// Cap on a response body.
const BODY_LIMIT: u64 = 100 * 1024 * 1024;
/// The pause before the one retry when the server names none.
const DEFAULT_RETRY_PAUSE: Duration = Duration::from_millis(500);
/// The longest `Retry-After` honoured; a server asking for more gets the retry after this.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// The process-wide HTTP agents, built once on first use — ureq's `Agent` is an `Arc`-backed cheap
/// clone sharing one connection pool, so every fetch in the process reuses warm TCP+TLS
/// connections instead of re-handshaking per request. Two agents share one TLS/timeout policy
/// that honours `--timeout` / `--no-timeout`: [`AGENT`] lets ureq follow redirects and turn a
/// 4xx/5xx into an error, which is what [`post_json`] wants; [`CHAIN_AGENT`] does neither, so the
/// fetch chain ([`fetch_chain`]) sees every hop and decides it. The two knobs sit on the agent
/// rather than on each request because ureq caches its built TLS configuration (platform roots
/// included) only for agent-level requests.
static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
static CHAIN_AGENT: OnceLock<ureq::Agent> = OnceLock::new();

fn agent() -> ureq::Agent {
    AGENT
        .get_or_init(|| ureq::Agent::new_with_config(agent_config(timeouts())))
        .clone()
}

fn chain_agent() -> ureq::Agent {
    CHAIN_AGENT
        .get_or_init(|| ureq::Agent::new_with_config(chain_config(timeouts())))
        .clone()
}

/// The shared agent configuration: platform-verified TLS, the process-wide timeouts, an idle
/// pool sized for the resolver's 8-wide packument prefetch, and https on **every** request —
/// redirects included.
fn config_builder(t: Timeouts) -> ConfigBuilder<AgentScope> {
    ureq::Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .timeout_connect(t.connect)
        .timeout_global(t.global)
        // ureq's default would be `ureq/<version>`.
        .user_agent(USER_AGENT)
        // The resolver prefetches packuments 8-wide against a single registry host
        // (`registry`'s PACKUMENT_CONCURRENCY); ureq's idle-pool defaults (3 per
        // host, 10 total) would drop and re-handshake most of those connections
        // between rounds.
        .max_idle_connections_per_host(8)
        .max_idle_connections(16)
        // The fetch chain accepts only an absolute https `Location`; `https_only` is the
        // second line behind that check, and the only one for `post_json`, which lets ureq
        // follow redirects.
        .https_only(true)
}

/// [`AGENT`]'s configuration: ureq's own redirect following and status-as-error.
fn agent_config(t: Timeouts) -> Config {
    config_builder(t).build()
}

/// [`CHAIN_AGENT`]'s configuration: every response, a 3xx or a 4xx/5xx included, comes back to
/// the chain as a response.
fn chain_config(t: Timeouts) -> Config {
    config_builder(t)
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
}

/// One request of a redirect chain as handed to the wire: the URL, the caller's `Accept`, and the
/// `Authorization` value when the chain's credential policy grants one for this hop.
struct Hop<'a> {
    url: &'a str,
    accept: Option<&'a str>,
    authorization: Option<&'a HeaderValue>,
}

/// What one hop answered: the status, the `Location` when present and ASCII, the `Retry-After`
/// of a failure when the server named one, and the body — read only for a 2xx, empty otherwise.
#[derive(Debug)]
struct Reply {
    status: u16,
    location: Option<String>,
    retry_after: Option<Duration>,
    body: Vec<u8>,
}

/// The transport seam of the chain: the real one wraps [`CHAIN_AGENT`], the tests script replies.
type Transport<'a> = dyn FnMut(&Hop<'_>) -> Result<Reply, crate::Error> + 'a;

/// Why a chain stopped. `Status` keeps the facts a text and a retry need — the status, the
/// answering host and its `Retry-After` — never a hop's URL, so a signed redirect target (a
/// short-lived credential of its own) is never echoed.
#[derive(Debug)]
enum ChainError {
    Status {
        status: u16,
        #[cfg_attr(not(feature = "github"), allow(dead_code))]
        host: String,
        retry_after: Option<Duration>,
    },
    Other(crate::Error),
}

/// A failed attempt: the error for the caller, and the pause before the retry.
#[derive(Debug)]
struct Failed {
    error: crate::Error,
    pause: Duration,
}

impl From<crate::Error> for ChainError {
    fn from(error: crate::Error) -> Self {
        ChainError::Other(error)
    }
}

impl ChainError {
    /// The text a status has always had: ureq's own `http status: N`.
    fn into_error(self) -> crate::Error {
        match self {
            ChainError::Status { status, .. } => ureq::Error::StatusCode(status).into(),
            ChainError::Other(error) => error,
        }
    }

    /// The pause before the retry: the `Retry-After` of a 429 or 503, capped at
    /// [`MAX_RETRY_AFTER`], else [`DEFAULT_RETRY_PAUSE`].
    fn retry_pause(&self) -> Duration {
        match self {
            ChainError::Status {
                status: 429 | 503,
                retry_after: Some(wait),
                ..
            } => (*wait).min(MAX_RETRY_AFTER),
            _ => DEFAULT_RETRY_PAUSE,
        }
    }
}

/// `500 ms` or `3 s`, for the warning.
fn pause_text(pause: Duration) -> String {
    if pause < Duration::from_secs(1) {
        format!("{} ms", pause.as_millis())
    } else {
        format!("{} s", pause.as_secs())
    }
}

/// `Retry-After` as delta seconds; an HTTP-date reads as absent.
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// One hop's request over the chain agent: `agent.get(url)` plus the caller's `Accept` and the
/// `Authorization` when given — without the latter, the request as it was always built.
fn hop_request(agent: &ureq::Agent, hop: &Hop<'_>) -> RequestBuilder<WithoutBody> {
    let mut request = agent.get(hop.url);
    if let Some(accept) = hop.accept {
        request = request.header("Accept", accept);
    }
    if let Some(authorization) = hop.authorization {
        request = request.header(AUTHORIZATION, authorization.clone());
    }
    request
}

/// Reduce a response to its [`Reply`]: the body is read (under [`BODY_LIMIT`]) only for a 2xx; a
/// redirect or error response is dropped unread, its `Retry-After` kept.
fn classify(mut response: ureq::http::Response<Body>) -> Result<Reply, crate::Error> {
    let status = response.status();
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let retry_after = if status.is_success() {
        None
    } else {
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after)
    };
    let body = if status.is_success() {
        response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()?
    } else {
        Vec::new()
    };
    Ok(Reply {
        status: status.as_u16(),
        location,
        retry_after,
        body,
    })
}

/// The real transport: one hop on the wire, then [`classify`].
fn send_hop(agent: &ureq::Agent, hop: &Hop<'_>) -> Result<Reply, crate::Error> {
    classify(hop_request(agent, hop).call()?)
}

/// Fetch `url` through `send` one hop at a time: `authorize` names the `Authorization` value a
/// hop's URL carries, if any; a 301/302/303/307/308 is followed to the `Location`
/// [`redirect_target`] accepts (at most [`MAX_REDIRECTS`] times); a 2xx body is returned; and any
/// other status — 300 and 304 included — stops the chain. ureq's own header stripping on a
/// cross-host redirect is not relied on: each hop is a fresh request with its own decision.
fn fetch_chain<'a>(
    send: &mut Transport<'_>,
    url: &str,
    accept: Option<&str>,
    authorize: &dyn Fn(&str) -> Option<&'a HeaderValue>,
) -> Result<Vec<u8>, ChainError> {
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let reply = send(&Hop {
            url: &current,
            accept,
            authorization: authorize(&current),
        })?;
        match reply.status {
            200..=299 => return Ok(reply.body),
            301 | 302 | 303 | 307 | 308 => {
                current = redirect_target(url, reply.status, reply.location.as_deref())?;
            }
            status => {
                return Err(ChainError::Status {
                    status,
                    host: host_of(&current),
                    retry_after: reply.retry_after,
                })
            }
        }
    }
    Err(ChainError::Other(
        format!("too many redirects (more than {MAX_REDIRECTS}) fetching `{url}`").into(),
    ))
}

/// The host of a hop URL, lowercased; empty when there is none.
fn host_of(url: &str) -> String {
    url.parse::<Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

/// The next hop of a redirect: `location` must parse as an absolute `https` URL with a host and
/// no userinfo, and is handed on as that parsed value. Neither a missing nor a refused `Location`
/// is echoed; `url` is the fetch's initial URL.
fn redirect_target(url: &str, status: u16, location: Option<&str>) -> Result<String, ChainError> {
    let Some(location) = location else {
        return Err(ChainError::Other(
            format!("the {status} redirect while fetching `{url}` carries no Location").into(),
        ));
    };
    let target = location.parse::<Uri>().ok().filter(|uri| {
        uri.scheme_str() == Some("https")
            && uri.authority().is_some_and(|authority| {
                !authority.host().is_empty() && !authority.as_str().contains('@')
            })
    });
    match target {
        Some(uri) => Ok(uri.to_string()),
        None => Err(ChainError::Other(
            format!(
                "refusing the {status} redirect while fetching `{url}`: \
                 the Location is not an absolute https URL"
            )
            .into(),
        )),
    }
}

/// One attempt of a fetch without the `github` feature: the chain, no credential on any hop, and
/// ureq's own text for a status.
#[cfg(not(feature = "github"))]
fn attempt(send: &mut Transport<'_>, url: &str, accept: Option<&str>) -> Result<Vec<u8>, Failed> {
    fetch_chain(send, url, accept, &|_| None).map_err(|error| Failed {
        pause: error.retry_pause(),
        error: error.into_error(),
    })
}

/// One attempt of a fetch with the `github` feature: the chain with the token on `api.github.com`
/// hops, and the second attempt through the GitHub API for a private asset.
#[cfg(feature = "github")]
fn attempt(send: &mut Transport<'_>, url: &str, accept: Option<&str>) -> Result<Vec<u8>, Failed> {
    github::attempt(send, url, accept)
}

/// Run `try_once` up to [`ATTEMPTS`] times, the failure's pause apart; a failure before the last
/// try goes to `warn` as a line naming `url` and the error — never a header.
fn with_retry(
    url: &str,
    warn: &mut dyn FnMut(&str),
    mut try_once: impl FnMut() -> Result<Vec<u8>, Failed>,
) -> crate::Result<Vec<u8>> {
    let attempts = ATTEMPTS;
    for n in 1..=attempts {
        match try_once() {
            Ok(body) => return Ok(body),
            Err(Failed { error, pause }) if n < attempts => {
                warn(&format!(
                    "download attempt {n}/{attempts} failed for {url}: {error}; retrying in {}",
                    pause_text(pause)
                ));
                std::thread::sleep(pause);
            }
            Err(Failed { error, .. }) => return Err(error),
        }
    }
    unreachable!("ATTEMPTS is at least one")
}

/// Download an `https://` URL into memory (100 MB cap), retrying once on transient failure.
///
/// Only `https` is fetched: a non-https URL is refused up front, and redirects are followed by
/// this crate itself — one request per hop, at most five, each `Location` an absolute https URL —
/// with the agent's `https_only` as a second line. The tarball URL is advertised by the registry,
/// so this keeps a hostile or redirecting registry from steering us at a plain-http or internal
/// endpoint (the downloaded bytes are sha512-verified regardless — this is defense-in-depth).
/// Per-request connect and transfer timeouts are set so a stalled peer can't hang the build; the
/// 100 MB cap bounds size, the timeouts bound time.
///
/// No request carries a credential unless the `github` feature is on. With it, a GitHub token
/// from `GH_TOKEN`, else `GITHUB_TOKEN` (`Credentials::from_env`, overridable through
/// `set_credentials`) travels as `Authorization: Bearer` to `api.github.com` only — the
/// registry, `github.com` and the hosts a GitHub redirect lands on never see it — and a
/// `github.com` browser URL of a private release asset or repository archive that answers 404 is
/// resolved through the GitHub API, something npm itself cannot do. ureq's `log` output at trace
/// level prints each hop's URL, signed redirect targets included.
///
/// Some hosts (GitHub in particular) occasionally drop a connection
/// mid-transfer — observed as `io: Peer disconnected` on CI — and the same URL
/// has not been seen to fail twice in a row, so one retry after a short pause is
/// enough. A 429 or 503 names its own pause in `Retry-After`, honoured up to 30 seconds.
/// Every request carries [`USER_AGENT`].
pub fn fetch(url: &str) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    fetch_with_accept(url, None)
}

/// Like [`fetch`], but sends an `Accept` header on every hop — the npm registry's abbreviated
/// packument (`application/vnd.npm.install-v1+json`), which is far smaller than the full document,
/// or `application/octet-stream` for a GitHub release asset addressed by its API id.
pub fn fetch_with_accept(
    url: &str,
    accept: Option<&str>,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    if !url.starts_with("https://") {
        return Err(format!(
            "refusing to fetch non-https URL {url:?}: npm-utils downloads over https only"
        )
        .into());
    }
    let agent = chain_agent();
    with_retry(url, &mut |message: &str| crate::warn::warn(message), || {
        attempt(&mut |hop: &Hop<'_>| send_hop(&agent, hop), url, accept)
    })
}

/// POST `body` to an `https://` URL and return the parsed JSON response, or `None` on **any**
/// failure — a non-https URL, a network error, a non-2xx status, or an unparseable body.
///
/// The single-attempt, error-swallowing contract is deliberate: the audit advisory sources read
/// `None` as "no advisories", so an unreachable endpoint, a 410 (npm's retired legacy paths), or a
/// flaky link degrades to an empty result instead of failing the run — matching `npm audit` /
/// `pnpm audit`, which exit 0 when the advisory endpoint can't be reached.
///
/// `content_encoding` sets the `Content-Encoding` header (`Some("gzip")` when `body` is
/// gzip-compressed, as npm's bulk-advisory endpoint requires); `accept` overrides the `Accept`
/// header (default `application/json`). `Content-Type` is always `application/json`.
pub fn post_json(
    url: &str,
    body: &[u8],
    content_encoding: Option<&str>,
    accept: Option<&str>,
) -> Option<Value> {
    if !url.starts_with("https://") {
        return None;
    }
    let request = agent()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", accept.unwrap_or("application/json"));
    let request = match content_encoding {
        Some(enc) => request.header("Content-Encoding", enc),
        None => request,
    };
    let mut response = request.send(body).ok()?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(100 * 1024 * 1024)
        .read_to_vec()
        .ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

/// URL for a GitHub repository archive (zip) at a ref (branch, tag, or commit).
pub fn github_archive_url(owner: &str, repo: &str, git_ref: &str) -> String {
    format!("https://github.com/{owner}/{repo}/archive/{git_ref}.zip")
}

/// A scripted wire for the chain's tests, shared with the `github` module's.
#[cfg(test)]
mod script {
    use super::{Hop, Reply};
    use std::collections::VecDeque;
    use std::time::Duration;

    pub fn ok(body: &[u8]) -> Reply {
        Reply {
            status: 200,
            location: None,
            retry_after: None,
            body: body.to_vec(),
        }
    }

    pub fn redirect(status: u16, location: &str) -> Reply {
        Reply {
            status,
            location: Some(location.to_owned()),
            retry_after: None,
            body: Vec::new(),
        }
    }

    pub fn status(status: u16) -> Reply {
        Reply {
            status,
            location: None,
            retry_after: None,
            body: Vec::new(),
        }
    }

    /// A failure naming its own pause in `Retry-After`.
    pub fn throttled(status: u16, retry_after_secs: u64) -> Reply {
        Reply {
            status,
            location: None,
            retry_after: Some(Duration::from_secs(retry_after_secs)),
            body: Vec::new(),
        }
    }

    /// One request as the scripted wire saw it.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Seen {
        pub url: String,
        pub accept: Option<String>,
        pub authorization: Option<String>,
    }

    pub fn seen(url: &str, accept: Option<&str>, authorization: Option<&str>) -> Seen {
        Seen {
            url: url.to_owned(),
            accept: accept.map(str::to_owned),
            authorization: authorization.map(str::to_owned),
        }
    }

    /// Answers hops from a queue, in order, and records what each hop carried.
    pub struct Script {
        pub replies: VecDeque<Reply>,
        pub seen: Vec<Seen>,
    }

    impl Script {
        pub fn new(replies: impl IntoIterator<Item = Reply>) -> Script {
            Script {
                replies: replies.into_iter().collect(),
                seen: Vec::new(),
            }
        }

        pub fn send(&mut self, hop: &Hop<'_>) -> Result<Reply, crate::Error> {
            self.seen.push(Seen {
                url: hop.url.to_owned(),
                accept: hop.accept.map(str::to_owned),
                authorization: hop
                    .authorization
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            });
            self.replies
                .pop_front()
                .ok_or_else(|| "script exhausted: unexpected hop".into())
        }

        /// The `Authorization` each hop carried, in order.
        pub fn authorizations(&self) -> Vec<Option<&str>> {
            self.seen
                .iter()
                .map(|s| s.authorization.as_deref())
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::script::*;
    use super::*;

    /// The chain without any credential policy.
    fn chain(script: &mut Script, url: &str) -> Result<Vec<u8>, ChainError> {
        fetch_chain(&mut |hop: &Hop<'_>| script.send(hop), url, None, &|_| None)
    }

    /// An agent of the chain's configuration that does not pin the process-wide agents or
    /// timeouts; building it is offline (TLS initialises at the first connect).
    fn offline_agent() -> ureq::Agent {
        ureq::Agent::new_with_config(chain_config(Timeouts::default()))
    }

    /// An offline response for `classify`.
    fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> ureq::http::Response<Body> {
        let mut builder = ureq::http::Response::builder().status(status);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::builder().data(body.to_vec())).unwrap()
    }

    #[test]
    fn fetch_refuses_non_https() {
        // The scheme guard rejects before any network request, so this is offline.
        for url in [
            "http://registry.npmjs.org/x",
            "http://github.com/x",
            "http://api.github.com/x",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "registry.npmjs.org/x",
        ] {
            assert!(fetch(url).is_err(), "{url:?} must be refused");
        }
    }

    #[test]
    fn post_json_refuses_non_https() {
        // The scheme guard rejects before any network request, so this is offline.
        for url in [
            "http://api.example.com/x",
            "ftp://example.com/x",
            "api.example.com/x",
        ] {
            assert!(
                post_json(url, b"{}", None, None).is_none(),
                "{url:?} must be refused"
            );
        }
    }

    #[test]
    fn the_agent_refuses_redirects_off_https() {
        // `https_only` applies the scheme guard to every request ureq makes, behind the chain's
        // own `Location` check and alone for `post_json`.
        assert!(agent_config(Timeouts::default()).https_only());
        assert!(chain_config(Timeouts::default()).https_only());
        assert_eq!(chain_config(Timeouts::default()).max_redirects(), 0);
        assert!(!chain_config(Timeouts::default()).http_status_as_error());
    }

    #[test]
    fn requests_identify_the_crate() {
        assert!(USER_AGENT.starts_with(concat!("npm-utils/", env!("CARGO_PKG_VERSION"))));
        for config in [
            agent_config(Timeouts::default()),
            chain_config(Timeouts::default()),
        ] {
            assert!(matches!(
                config.user_agent(),
                ureq::config::AutoHeaderValue::Provided(agent) if agent.as_str() == USER_AGENT
            ));
        }
    }

    #[test]
    fn timeouts_from_cli_flags() {
        let d = Timeouts::default();
        // Neither flag → the library default.
        let unset = Timeouts::from_cli(None, false);
        assert_eq!((unset.connect, unset.global), (d.connect, d.global));
        // --timeout sets the per-request timeout, keeping the default connect.
        let t = Timeouts::from_cli(Some(5), false);
        assert_eq!(t.global, Some(Duration::from_secs(5)));
        assert_eq!(t.connect, d.connect);
        // --no-timeout removes every bound and wins over --timeout.
        let off = Timeouts::from_cli(Some(5), true);
        assert_eq!((off.connect, off.global), (None, None));
    }

    #[test]
    fn chain_follows_a_redirect_and_returns_the_body() {
        let mut script = Script::new([
            redirect(302, "https://codeload.github.com/o/r/zip/main"),
            ok(b"zip"),
        ]);
        let url = "https://github.com/o/r/archive/main.zip";
        assert_eq!(chain(&mut script, url).unwrap(), b"zip");
        assert_eq!(
            script.seen,
            vec![
                seen(url, None, None),
                seen("https://codeload.github.com/o/r/zip/main", None, None),
            ]
        );
    }

    #[test]
    fn chain_refuses_a_bad_location() {
        // Every refused Location carries a marker the error text must not echo.
        for location in [
            Some("http://SECRET-LOCATION/x?token=signed"),
            Some("/SECRET-LOCATION/path"),
            Some("//SECRET-LOCATION/path"),
            Some("https:"),
            Some("https://user@SECRET-LOCATION/path"),
            None,
        ] {
            let reply = Reply {
                status: 302,
                location: location.map(str::to_owned),
                retry_after: None,
                body: Vec::new(),
            };
            let mut script = Script::new([reply]);
            let error = chain(&mut script, "https://github.com/o/r/archive/main.zip").unwrap_err();
            let text = match error {
                ChainError::Other(e) => e.to_string(),
                other => panic!("{location:?} gives a text error, not {other:?}"),
            };
            assert_eq!(script.seen.len(), 1, "{location:?}: one request");
            assert!(text.contains("302"), "{text}");
            assert!(!text.contains("SECRET"), "{text} echoes the Location");
        }
    }

    #[test]
    fn chain_stops_after_five_redirects() {
        let loop_to = "https://github.com/o/r/loop";
        let mut script = Script::new((0..10).map(|_| redirect(302, loop_to)));
        let error = chain(&mut script, loop_to).unwrap_err();
        assert_eq!(script.seen.len(), MAX_REDIRECTS + 1);
        assert!(
            matches!(error, ChainError::Other(ref e) if e.to_string().contains("too many redirects"))
        );
    }

    #[test]
    fn chain_treats_other_statuses_as_status() {
        for code in [300, 304, 305, 404, 500] {
            let mut script = Script::new([status(code)]);
            let error = chain(&mut script, "https://registry.npmjs.org/lit").unwrap_err();
            assert!(
                matches!(error, ChainError::Status { status, ref host, retry_after: None } if status == code && host == "registry.npmjs.org"),
                "{code}: {error:?}"
            );
            assert_eq!(
                error.into_error().to_string(),
                format!("http status: {code}")
            );
        }
        // The answering host is the hop's, after a redirect.
        let mut script = Script::new([redirect(302, "https://codeload.github.com/x"), status(403)]);
        let error = chain(&mut script, "https://github.com/o/r/archive/main.zip").unwrap_err();
        assert!(
            matches!(error, ChainError::Status { status: 403, ref host, .. } if host == "codeload.github.com")
        );
        // A throttling answer carries its Retry-After.
        let mut script = Script::new([throttled(503, 7)]);
        let error = chain(&mut script, "https://registry.npmjs.org/lit").unwrap_err();
        assert!(
            matches!(error, ChainError::Status { status: 503, retry_after: Some(wait), .. } if wait == Duration::from_secs(7))
        );
    }

    #[test]
    fn a_throttling_status_waits_for_retry_after_within_the_cap() {
        let status = |status, retry_after| ChainError::Status {
            status,
            host: "registry.npmjs.org".to_owned(),
            retry_after,
        };
        assert_eq!(
            status(429, Some(Duration::from_secs(3))).retry_pause(),
            Duration::from_secs(3)
        );
        assert_eq!(
            status(503, Some(Duration::from_secs(120))).retry_pause(),
            MAX_RETRY_AFTER
        );
        assert_eq!(status(429, None).retry_pause(), DEFAULT_RETRY_PAUSE);
        assert_eq!(
            status(500, Some(Duration::from_secs(3))).retry_pause(),
            DEFAULT_RETRY_PAUSE
        );
        assert_eq!(
            ChainError::Other("peer disconnected".into()).retry_pause(),
            DEFAULT_RETRY_PAUSE
        );
        assert_eq!(pause_text(DEFAULT_RETRY_PAUSE), "500 ms");
        assert_eq!(pause_text(Duration::from_secs(3)), "3 s");
    }

    #[test]
    fn retry_after_reads_seconds_and_not_dates() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after(" 10 "), Some(Duration::from_secs(10)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("-1"), None);
    }

    #[test]
    fn classify_reads_only_success_bodies() {
        let reply = classify(response(200, &[], b"data")).unwrap();
        assert_eq!(
            (reply.status, reply.location, reply.body),
            (200, None, b"data".to_vec())
        );
        let reply = classify(response(302, &[("location", "https://x/y")], b"ignored")).unwrap();
        assert_eq!(
            (reply.status, reply.location.as_deref(), reply.body.len()),
            (302, Some("https://x/y"), 0)
        );
        let reply = classify(response(302, &[], b"")).unwrap();
        assert_eq!((reply.status, reply.location), (302, None));
        for code in [304, 404, 500] {
            let reply = classify(response(code, &[], b"error page")).unwrap();
            assert_eq!(
                (reply.status, reply.body.len(), reply.retry_after),
                (code, 0, None)
            );
        }
        // Retry-After is kept for a failure and ignored on a success.
        let reply = classify(response(429, &[("retry-after", "3")], b"")).unwrap();
        assert_eq!(reply.retry_after, Some(Duration::from_secs(3)));
        let reply = classify(response(200, &[("retry-after", "3")], b"fine")).unwrap();
        assert_eq!(reply.retry_after, None);
    }

    #[test]
    fn hop_request_matches_a_plain_get() {
        let agent = offline_agent();
        for (url, accept) in [
            (
                "https://registry.npmjs.org/lit",
                Some("application/vnd.npm.install-v1+json"),
            ),
            ("https://github.com/o/r/archive/main.zip", None),
            (
                "https://api.github.com/repos/o/r/releases/assets/1",
                Some("application/octet-stream"),
            ),
        ] {
            let hop = Hop {
                url,
                accept,
                authorization: None,
            };
            let ours = hop_request(&agent, &hop);
            let plain = match accept {
                Some(accept) => agent.get(url).header("Accept", accept),
                None => agent.get(url),
            };
            assert_eq!(ours.headers_ref(), plain.headers_ref(), "{url}");
            assert_eq!(ours.uri_ref(), plain.uri_ref(), "{url}");
        }
        // With an authorization, that header is the only addition.
        let value = HeaderValue::from_static("Bearer x");
        let hop = Hop {
            url: "https://api.github.com/repos/o/r",
            accept: None,
            authorization: Some(&value),
        };
        let headers = hop_request(&agent, &hop).headers_ref().unwrap().clone();
        let plain = agent.get(hop.url).headers_ref().unwrap().clone();
        assert_eq!(headers.len(), plain.len() + 1);
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer x");
    }

    #[cfg(not(feature = "github"))]
    #[test]
    fn without_the_feature_no_hop_carries_a_credential_and_a_status_keeps_its_text() {
        let mut script = Script::new([status(404)]);
        let url = "https://github.com/o/r/releases/download/v1/lib.tgz";
        let failed = attempt(&mut |hop: &Hop<'_>| script.send(hop), url, None).unwrap_err();
        assert_eq!(failed.error.to_string(), "http status: 404");
        assert_eq!(failed.pause, DEFAULT_RETRY_PAUSE);
        assert_eq!(script.seen, vec![seen(url, None, None)]);

        let mut script =
            Script::new([redirect(302, "https://x.githubusercontent.com/b"), ok(b"b")]);
        attempt(
            &mut |hop: &Hop<'_>| script.send(hop),
            "https://api.github.com/repos/o/r/releases/assets/1",
            Some("application/octet-stream"),
        )
        .unwrap();
        assert_eq!(script.authorizations(), vec![None, None]);

        // A throttling answer sets the pause of the retry.
        let mut script = Script::new([throttled(503, 3)]);
        let failed = attempt(&mut |hop: &Hop<'_>| script.send(hop), url, None).unwrap_err();
        assert_eq!(failed.pause, Duration::from_secs(3));
    }

    #[test]
    fn the_retry_warns_once_and_then_fails() {
        let mut warnings = Vec::new();
        let mut calls = 0;
        let result = with_retry(
            "https://registry.npmjs.org/lit",
            &mut |message: &str| warnings.push(message.to_owned()),
            || {
                calls += 1;
                Err(Failed {
                    error: format!("boom {calls}").into(),
                    pause: Duration::ZERO,
                })
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "boom 2");
        assert_eq!(calls, 2);
        assert_eq!(
            warnings,
            vec!["download attempt 1/2 failed for https://registry.npmjs.org/lit: boom 1; retrying in 0 ms"]
        );

        // A success on the second try is a success.
        let mut calls = 0;
        let result = with_retry("https://x/y", &mut |_: &str| {}, || {
            calls += 1;
            if calls == 1 {
                Err(Failed {
                    error: "once".into(),
                    pause: Duration::ZERO,
                })
            } else {
                Ok(b"ok".to_vec())
            }
        });
        assert_eq!(result.unwrap(), b"ok");
    }
}
