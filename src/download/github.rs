//! The `github` feature: a GitHub token from the environment, sent to `api.github.com` only, and
//! the second attempt through the GitHub API for a `github.com` browser URL of a private release
//! asset or repository archive. Without this feature no fetch carries an `Authorization` header,
//! so a library build never picks up a token from its environment by accident; the CLI enables
//! it, since it reads the token the way `gh` does.

use super::{fetch_chain, ChainError, Failed, Transport};
use percent_encoding::percent_decode_str;
use serde_json::Value;
use std::borrow::Cow;
use std::fmt;
use std::sync::OnceLock;
use ureq::http::header::HeaderValue;
use ureq::http::Uri;

/// The variables a GitHub token is read from, in precedence order — the order `gh` reads them.
const TOKEN_VARS: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];
/// The one host that receives the token. `github.com` ignores an `Authorization` header (a private
/// asset answers 404 with or without it), so only the API is worth authenticating.
const TOKEN_HOST: &str = "api.github.com";
/// The hosts whose 401, 403 or 404 speak of a private asset and the token.
const GITHUB_HOSTS: [&str; 2] = ["github.com", "api.github.com"];

/// A GitHub token, held as the ready-made `Authorization` value (`Bearer <token>`, flagged
/// sensitive for the http crate). Nothing reads it back, and `Debug` prints a placeholder, so the
/// token reaches no warning, error or debug output.
#[derive(Clone)]
pub struct Credentials {
    authorization: HeaderValue,
}

impl fmt::Debug for Credentials {
    /// Always `Credentials([redacted])`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials([redacted])")
    }
}

impl Credentials {
    /// Wrap a token. Refused, without echoing it, unless it is non-empty visible ASCII: the check
    /// is done here because `HeaderValue` admits spaces, tabs and non-ASCII bytes, and a token
    /// pasted with its trailing newline would otherwise fail every GitHub request on the wire.
    pub fn new(token: &str) -> crate::Result<Credentials> {
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("a GitHub token must be non-empty visible ASCII \
                        (no spaces, control characters, line breaks or non-ASCII)"
                .into());
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))?;
        authorization.set_sensitive(true);
        Ok(Credentials { authorization })
    }

    /// `GH_TOKEN`, else `GITHUB_TOKEN`: the first variable that is set and non-empty wins, as `gh`
    /// reads them; a non-UTF-8 value counts as unset. A winning value that cannot be a token
    /// ([`new`](Self::new)) is warned about by variable name and yields `None` — there is no
    /// fall-through to the second variable. `None` when neither is set.
    pub fn from_env() -> Option<Credentials> {
        Self::from_lookup(|name| std::env::var(name).ok(), &mut |message: &str| {
            crate::warn::warn(message)
        })
    }

    /// The precedence rule behind [`from_env`](Self::from_env) over any lookup and any warning
    /// sink, so the unit tests touch neither the process environment nor the warning channel.
    fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        warn: &mut dyn FnMut(&str),
    ) -> Option<Credentials> {
        let (name, token) = TOKEN_VARS.iter().find_map(|name| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .map(|value| (*name, value))
        })?;
        match Credentials::new(&token) {
            Ok(credentials) => Some(credentials),
            Err(e) => {
                warn(&format!("ignoring `{name}`: {e}; sending no GitHub token"));
                None
            }
        }
    }

    /// The `Authorization` value a token-carrying hop sends.
    fn authorization(&self) -> &HeaderValue {
        &self.authorization
    }

    /// The chain's credential policy with this token: the value for a hop that
    /// [`carries_token`], nothing for any other.
    fn authorize(&self, hop_url: &str) -> Option<&HeaderValue> {
        carries_token(hop_url).then(|| self.authorization())
    }
}

/// The process-wide credentials: unset until the first fetch, then `None` (explicitly none, or
/// nothing in the environment) or `Some`.
static CREDENTIALS: OnceLock<Option<Credentials>> = OnceLock::new();

/// Set (`Some`) or clear (`None`) the process-wide GitHub credentials. Call it before the first
/// fetch, which otherwise reads [`Credentials::from_env`] once; like
/// [`set_timeouts`](super::set_timeouts), a later call is ignored.
pub fn set_credentials(credentials: Option<Credentials>) {
    let _ = CREDENTIALS.set(credentials);
}

/// Whatever [`set_credentials`] stored, else the environment, read on first use and then fixed.
fn credentials() -> Option<&'static Credentials> {
    CREDENTIALS.get_or_init(Credentials::from_env).as_ref()
}

/// Whether `url` has the scheme `https` and an authority that is exactly one of `hosts`: no
/// userinfo, port absent or 443, host compared ASCII-case-insensitively. A subdomain, a trailing
/// dot, an IP literal, another port or an unparseable URL does not match.
fn host_matches(url: &str, hosts: &[&str]) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    let Some(authority) = uri.authority() else {
        return false;
    };
    uri.scheme_str() == Some("https")
        && !authority.as_str().contains('@')
        && matches!(authority.port_u16(), None | Some(443))
        && hosts
            .iter()
            .any(|host| authority.host().eq_ignore_ascii_case(host))
}

/// Whether a request to `url` carries the token: only [`TOKEN_HOST`], over https. The hosts a
/// GitHub redirect lands on — `codeload.github.com`, the `githubusercontent.com` hosts — and
/// `github.com` itself get none.
fn carries_token(url: &str) -> bool {
    host_matches(url, &[TOKEN_HOST])
}

/// Whether a 401, 403 or 404 from `host` (the lowercased host of the answering hop, every hop
/// being https) is GitHub's answer for a private asset.
fn is_github_host(host: &str) -> bool {
    GITHUB_HOSTS.contains(&host)
}

/// The `github.com` browser forms that have an API twin. `tag` and `file` are percent-decoded for
/// the texts and the asset lookup; `tag_path` keeps the tag as written for the API path.
#[derive(Debug, PartialEq, Eq)]
enum BrowserForm {
    /// `https://github.com/{owner}/{repo}/releases/download/{tag}/{file}`.
    ReleaseAsset {
        owner: String,
        repo: String,
        tag: String,
        file: String,
        tag_path: String,
    },
    /// `https://github.com/{owner}/{repo}/archive/{ref}.zip` or `.tar.gz`.
    Archive {
        owner: String,
        repo: String,
        git_ref: String,
        tarball: bool,
    },
}

/// Parse a `github.com` URL into its [`BrowserForm`], matching the path only: a query or a
/// fragment, another host, or any other path shape is `None`. A release tag may contain `/`
/// (everything between `download/` and the last segment); an archive ref is kept verbatim
/// (`refs/tags/v1` included).
fn browser_form(url: &str) -> Option<BrowserForm> {
    if url.contains(['?', '#']) || !host_matches(url, &["github.com"]) {
        return None;
    }
    let uri = url.parse::<Uri>().ok()?;
    let mut segments = uri.path().strip_prefix('/')?.split('/');
    let owner = segments.next().filter(|s| !s.is_empty())?.to_owned();
    let repo = segments.next().filter(|s| !s.is_empty())?.to_owned();
    let rest: Vec<&str> = segments.collect();
    if rest.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    match rest.as_slice() {
        ["releases", "download", tag @ .., file] if !tag.is_empty() => {
            let tag_path = tag.join("/");
            Some(BrowserForm::ReleaseAsset {
                owner,
                repo,
                tag: percent_decode(&tag_path)?,
                file: percent_decode(file)?,
                tag_path,
            })
        }
        ["archive", reference @ ..] if !reference.is_empty() => {
            let joined = reference.join("/");
            let (git_ref, tarball) = match joined.strip_suffix(".tar.gz") {
                Some(r) => (r, true),
                None => (joined.strip_suffix(".zip")?, false),
            };
            if git_ref.is_empty() {
                return None;
            }
            Some(BrowserForm::Archive {
                owner,
                repo,
                git_ref: git_ref.to_owned(),
                tarball,
            })
        }
        _ => None,
    }
}

/// `%XX`-decode one path segment; a `%` that no two hex digits follow stays as written, and
/// `None` is bytes that are not UTF-8.
fn percent_decode(segment: &str) -> Option<String> {
    percent_decode_str(segment)
        .decode_utf8()
        .ok()
        .map(Cow::into_owned)
}

/// Fetch a browser form through the API, the only form GitHub serves to a token. A release asset
/// is looked up by tag (`Accept: application/vnd.github+json`), the numeric `id` of the asset whose
/// `name` is `file` is taken from the document, and the asset is fetched by that id with
/// `Accept: application/octet-stream` — the URL is built here, never taken from the document. An
/// archive is the `zipball` or `tarball` endpoint with the caller's `Accept`. Draft releases are
/// not reachable through the by-tag lookup. `url` is the caller's, for the texts.
fn fetch_through_api(
    send: &mut Transport<'_>,
    url: &str,
    form: &BrowserForm,
    accept: Option<&str>,
    credentials: &Credentials,
) -> Result<Vec<u8>, ChainError> {
    let authorize = |hop: &str| credentials.authorize(hop);
    match form {
        BrowserForm::ReleaseAsset {
            owner,
            repo,
            tag,
            file,
            tag_path,
        } => {
            let lookup =
                format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag_path}");
            let document = fetch_chain(
                send,
                &lookup,
                Some("application/vnd.github+json"),
                &authorize,
            )?;
            let release: Value = serde_json::from_slice(&document).map_err(|_| {
                ChainError::Other(format!("unreadable release document for `{url}`").into())
            })?;
            let id = release
                .get("assets")
                .and_then(Value::as_array)
                .and_then(|assets| {
                    assets
                        .iter()
                        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(file))
                })
                .and_then(|asset| asset.get("id"))
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    ChainError::Other(
                        format!(
                            "no asset `{file}` in the GitHub release `{tag}` of `{owner}/{repo}`"
                        )
                        .into(),
                    )
                })?;
            let asset = format!("https://api.github.com/repos/{owner}/{repo}/releases/assets/{id}");
            fetch_chain(send, &asset, Some("application/octet-stream"), &authorize)
        }
        BrowserForm::Archive {
            owner,
            repo,
            git_ref,
            tarball,
        } => {
            let kind = if *tarball { "tarball" } else { "zipball" };
            let archive = format!("https://api.github.com/repos/{owner}/{repo}/{kind}/{git_ref}");
            fetch_chain(send, &archive, accept, &authorize)
        }
    }
}

/// The text for a chain's failure: a 401, 403 or 404 from a GitHub host is GitHub's answer for
/// what it will not show — a missing name or a private asset, which it does not tell apart —
/// and names the token or the variables; everything else keeps its text. Takes a flag, never the
/// credentials, so the token cannot reach the text; `url` is the caller's.
fn describe(url: &str, error: ChainError, has_token: bool) -> crate::Error {
    match error {
        ChainError::Status {
            status: status @ (401 | 403 | 404),
            ref host,
            ..
        } if is_github_host(host) => {
            if has_token {
                format!(
                    "GitHub refused `{url}` for the token (status {status}): \
                     not found, or a private asset the token has no access to"
                )
                .into()
            } else {
                format!(
                    "GitHub refused `{url}` (status {status}): \
                     not found, or a private asset that needs `GH_TOKEN` or `GITHUB_TOKEN`"
                )
                .into()
            }
        }
        other => other.into_error(),
    }
}

/// One attempt of a fetch: the chain on the caller's URL with the token on `api.github.com`
/// hops; a 404 while a credential is set and the URL is a [`BrowserForm`] goes through the API
/// instead, since `github.com` serves a private asset to no token.
pub(super) fn attempt(
    send: &mut Transport<'_>,
    url: &str,
    accept: Option<&str>,
) -> Result<Vec<u8>, Failed> {
    let credentials = credentials();
    attempt_with(send, url, accept, credentials)
}

/// [`attempt`] over explicit credentials, so the tests never touch the process-wide ones.
fn attempt_with(
    send: &mut Transport<'_>,
    url: &str,
    accept: Option<&str>,
    credentials: Option<&Credentials>,
) -> Result<Vec<u8>, Failed> {
    let authorize = |hop: &str| credentials.and_then(|c| c.authorize(hop));
    let mut result = fetch_chain(send, url, accept, &authorize);
    if let (Err(ChainError::Status { status: 404, .. }), Some(credentials)) = (&result, credentials)
    {
        if let Some(form) = browser_form(url) {
            result = fetch_through_api(send, url, &form, accept, credentials);
        }
    }
    result.map_err(|error| Failed {
        pause: error.retry_pause(),
        error: describe(url, error, credentials.is_some()),
    })
}

#[cfg(test)]
mod tests {
    use super::super::script::*;
    use super::super::Hop;
    use super::*;

    /// A token that must never show up in anything printed.
    const SENTINEL: &str = "ghp_SENTINEL_never_printed";

    fn token() -> Credentials {
        Credentials::new(SENTINEL).unwrap()
    }

    fn bearer() -> String {
        format!("Bearer {SENTINEL}")
    }

    fn chain(
        script: &mut Script,
        url: &str,
        accept: Option<&str>,
        credentials: &Credentials,
    ) -> Result<Vec<u8>, ChainError> {
        fetch_chain(&mut |hop: &Hop<'_>| script.send(hop), url, accept, &|hop| {
            credentials.authorize(hop)
        })
    }

    fn attempt(
        script: &mut Script,
        url: &str,
        credentials: Option<&Credentials>,
    ) -> crate::Result<Vec<u8>> {
        attempt_with(
            &mut |hop: &Hop<'_>| script.send(hop),
            url,
            None,
            credentials,
        )
        .map_err(|failed| failed.error)
    }

    /// The precedence rule over fixed variables: the header value it yields and the warnings.
    fn from_vars(vars: &[(&str, &str)]) -> (Option<String>, Vec<String>) {
        let mut warnings = Vec::new();
        let credentials = Credentials::from_lookup(
            |name| {
                vars.iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| (*v).to_owned())
            },
            &mut |message: &str| warnings.push(message.to_owned()),
        );
        (
            credentials.map(|c| c.authorization().to_str().unwrap().to_owned()),
            warnings,
        )
    }

    #[test]
    fn carries_token_only_for_api_github_over_https() {
        for url in [
            "https://api.github.com/repos/o/r/releases/assets/1",
            "https://API.GITHUB.COM/repos/o/r",
            "https://api.github.com:443/repos/o/r",
        ] {
            assert!(carries_token(url), "{url} carries the token");
        }
        for url in [
            "https://github.com/o/r/releases/download/v1/f.tgz",
            "https://GITHUB.COM/o/r",
            "https://user@api.github.com/repos/o/r",
            "https://api.github.com:8443/repos/o/r",
            "https://api.github.com./repos/o/r",
            "https://github.com.evil.example/x",
            "https://api.github.com.evil.example/x",
            "https://objects.githubusercontent.com/x",
            "https://release-assets.githubusercontent.com/x",
            "https://codeload.github.com/o/r/zip/main",
            "https://registry.npmjs.org/lit",
            "https://140.82.121.6/x",
            "http://api.github.com/x",
            "api.github.com/x",
        ] {
            assert!(!carries_token(url), "{url} carries no token");
        }
    }

    #[test]
    fn github_hosts_for_the_messages() {
        for host in ["github.com", "api.github.com"] {
            assert!(is_github_host(host), "{host} is GitHub");
        }
        for host in [
            "github.com.evil.example",
            "codeload.github.com",
            "objects.githubusercontent.com",
            "registry.npmjs.org",
            "",
        ] {
            assert!(!is_github_host(host), "{host:?} is not GitHub");
        }
    }

    #[test]
    fn credentials_precedence() {
        let (value, warnings) = from_vars(&[("GH_TOKEN", "a"), ("GITHUB_TOKEN", "b")]);
        assert_eq!((value.as_deref(), warnings.len()), (Some("Bearer a"), 0));
        let (value, _) = from_vars(&[("GH_TOKEN", ""), ("GITHUB_TOKEN", "b")]);
        assert_eq!(value.as_deref(), Some("Bearer b"));
        let (value, _) = from_vars(&[("GITHUB_TOKEN", "b")]);
        assert_eq!(value.as_deref(), Some("Bearer b"));
        let (value, warnings) = from_vars(&[]);
        assert_eq!((value, warnings.len()), (None, 0));
        // An unusable winner is warned about by name and does not fall through.
        let (value, warnings) = from_vars(&[("GH_TOKEN", "bad\n"), ("GITHUB_TOKEN", SENTINEL)]);
        assert_eq!(value, None);
        assert_eq!(
            warnings,
            vec![
                "ignoring `GH_TOKEN`: a GitHub token must be non-empty visible ASCII \
                 (no spaces, control characters, line breaks or non-ASCII); sending no GitHub token"
            ]
        );
        assert!(!warnings[0].contains(SENTINEL) && !warnings[0].contains("bad"));
    }

    #[test]
    fn credentials_refuse_unusable_tokens() {
        for token in ["", "x\n", "a b", "ghp_é", "\tx"] {
            let error = Credentials::new(token).unwrap_err().to_string();
            assert!(
                error.contains("non-empty visible ASCII"),
                "{token:?} is refused"
            );
            assert!(
                token.is_empty() || !error.contains(token.trim()),
                "the text echoes nothing"
            );
        }
        assert!(Credentials::new("ghp_abc-DEF_123").is_ok());
    }

    #[test]
    fn credentials_debug_is_redacted() {
        let debug = format!("{:?}", token());
        assert!(debug.contains("[redacted]"), "{debug}");
        assert!(!debug.contains(SENTINEL), "{debug}");
    }

    #[test]
    fn chain_sends_the_token_to_api_github_only_and_drops_it_on_the_hop() {
        let mut script = Script::new([
            redirect(
                302,
                "https://release-assets.githubusercontent.com/x?sig=signed",
            ),
            ok(b"bytes"),
        ]);
        let credentials = token();
        let body = chain(
            &mut script,
            "https://api.github.com/repos/o/r/releases/assets/1",
            Some("application/octet-stream"),
            &credentials,
        )
        .unwrap();
        assert_eq!(body, b"bytes");
        assert_eq!(script.authorizations(), vec![Some(bearer().as_str()), None]);
        assert_eq!(
            script.seen[1],
            seen(
                "https://release-assets.githubusercontent.com/x?sig=signed",
                Some("application/octet-stream"),
                None
            )
        );
    }

    #[test]
    fn chain_sends_no_token_to_github_com() {
        let mut script = Script::new([ok(b"public")]);
        let credentials = token();
        let url = "https://github.com/o/r/archive/main.zip";
        assert_eq!(
            chain(&mut script, url, None, &credentials).unwrap(),
            b"public"
        );
        assert_eq!(script.seen, vec![seen(url, None, None)]);
    }

    #[test]
    fn github_status_messages() {
        let url = "https://github.com/o/r/releases/download/v1/f.tgz";
        let failure = |status: u16, host: &str| ChainError::Status {
            status,
            host: host.to_owned(),
            retry_after: None,
        };
        for code in [401, 403, 404] {
            for host in ["github.com", "api.github.com"] {
                let with = describe(url, failure(code, host), true).to_string();
                assert_eq!(
                    with,
                    format!(
                        "GitHub refused `{url}` for the token (status {code}): \
                         not found, or a private asset the token has no access to"
                    )
                );
                let without = describe(url, failure(code, host), false).to_string();
                assert_eq!(
                    without,
                    format!(
                        "GitHub refused `{url}` (status {code}): \
                         not found, or a private asset that needs `GH_TOKEN` or `GITHUB_TOKEN`"
                    )
                );
            }
        }
        let server_error = describe(url, failure(500, "api.github.com"), true);
        assert_eq!(server_error.to_string(), "http status: 500");
        assert!(server_error.downcast_ref::<ureq::Error>().is_some());
        assert_eq!(
            describe(url, failure(404, "registry.npmjs.org"), true).to_string(),
            "http status: 404"
        );
        assert_eq!(
            describe(url, ChainError::Other("other".into()), true).to_string(),
            "other"
        );
    }

    #[test]
    fn browser_forms() {
        let asset = |tag: &str, file: &str, tag_path: &str| BrowserForm::ReleaseAsset {
            owner: "o".into(),
            repo: "r".into(),
            tag: tag.into(),
            file: file.into(),
            tag_path: tag_path.into(),
        };
        assert_eq!(
            browser_form("https://github.com/o/r/releases/download/v1.2.3/lib-1.2.3.tgz"),
            Some(asset("v1.2.3", "lib-1.2.3.tgz", "v1.2.3"))
        );
        assert_eq!(
            browser_form("https://github.com/o/r/releases/download/release/v1/f.tgz"),
            Some(asset("release/v1", "f.tgz", "release/v1"))
        );
        assert_eq!(
            browser_form(
                "https://github.com/o/r/releases/download/llvmorg-17/clang%2Bllvm-x.tar.xz"
            ),
            Some(asset("llvmorg-17", "clang+llvm-x.tar.xz", "llvmorg-17"))
        );
        assert_eq!(
            browser_form("https://github.com/o/r/releases/download/%40astrojs/react%403.0.0/f"),
            Some(asset(
                "@astrojs/react@3.0.0",
                "f",
                "%40astrojs/react%403.0.0"
            ))
        );
        // A stray `%` is not an escape; GitHub itself writes a literal one as `%25`.
        assert_eq!(
            browser_form("https://github.com/o/r/releases/download/v1/100%.tgz"),
            Some(asset("v1", "100%.tgz", "v1"))
        );
        let archive = |git_ref: &str, tarball: bool| BrowserForm::Archive {
            owner: "o".into(),
            repo: "r".into(),
            git_ref: git_ref.into(),
            tarball,
        };
        assert_eq!(
            browser_form("https://github.com/o/r/archive/main.zip"),
            Some(archive("main", false))
        );
        assert_eq!(
            browser_form("https://github.com/o/r/archive/refs/tags/v1.zip"),
            Some(archive("refs/tags/v1", false))
        );
        assert_eq!(
            browser_form("https://github.com/o/r/archive/v1.2.3.tar.gz"),
            Some(archive("v1.2.3", true))
        );
        assert_eq!(
            browser_form(&super::super::github_archive_url("o", "r", "9f0cb54")),
            Some(archive("9f0cb54", false))
        );
        for url in [
            "https://github.com/o/r",
            "https://github.com/o/r/releases/download/v1",
            "https://github.com/o/r/releases/download//f.tgz",
            "https://github.com/o/r/archive/main.rar",
            "https://github.com/o/r/archive/.zip",
            "https://github.com/o/r/archive/main.zip?x=1",
            "https://github.com/o/r/archive/main.zip#frag",
            "https://github.com/o/r/releases/download/v1/f%FF.tgz",
            "https://codeload.github.com/o/r/zip/main",
            "https://api.github.com/repos/o/r/zipball/main",
            "https://github.com.evil.example/o/r/archive/main.zip",
            "http://github.com/o/r/archive/main.zip",
        ] {
            assert_eq!(browser_form(url), None, "{url}");
        }
    }

    #[test]
    fn a_private_release_asset_resolves_through_the_api() {
        let release = br#"{"tag_name":"v1","assets":[{"id":7,"name":"other.tgz"},{"id":42,"name":"lib-1.0.0.tgz"}]}"#;
        let mut script = Script::new([
            status(404),
            ok(release),
            redirect(
                302,
                "https://release-assets.githubusercontent.com/blob?sig=signed",
            ),
            ok(b"tarball"),
        ]);
        let credentials = token();
        let url = "https://github.com/o/r/releases/download/v1/lib-1.0.0.tgz";
        assert_eq!(
            attempt(&mut script, url, Some(&credentials)).unwrap(),
            b"tarball"
        );
        assert_eq!(
            script.seen,
            vec![
                seen(url, None, None),
                seen(
                    "https://api.github.com/repos/o/r/releases/tags/v1",
                    Some("application/vnd.github+json"),
                    Some(&bearer())
                ),
                seen(
                    "https://api.github.com/repos/o/r/releases/assets/42",
                    Some("application/octet-stream"),
                    Some(&bearer())
                ),
                seen(
                    "https://release-assets.githubusercontent.com/blob?sig=signed",
                    Some("application/octet-stream"),
                    None
                ),
            ]
        );

        // The browser URL is percent-encoded; the document's name is not.
        let release = br#"{"assets":[{"id":9,"name":"clang+llvm-x.tar.xz"}]}"#;
        let mut script = Script::new([status(404), ok(release), ok(b"bytes")]);
        let url = "https://github.com/o/r/releases/download/llvmorg-17/clang%2Bllvm-x.tar.xz";
        assert_eq!(
            attempt(&mut script, url, Some(&credentials)).unwrap(),
            b"bytes"
        );
        assert_eq!(
            script.seen[2].url,
            "https://api.github.com/repos/o/r/releases/assets/9"
        );
    }

    #[test]
    fn a_private_archive_resolves_through_the_api() {
        let mut script = Script::new([
            status(404),
            redirect(
                302,
                "https://codeload.github.com/o/r/legacy.zip/refs/tags/v1?token=signed",
            ),
            ok(b"zip"),
        ]);
        let credentials = token();
        let url = "https://github.com/o/r/archive/refs/tags/v1.zip";
        assert_eq!(
            attempt(&mut script, url, Some(&credentials)).unwrap(),
            b"zip"
        );
        assert_eq!(
            script.seen,
            vec![
                seen(url, None, None),
                seen(
                    "https://api.github.com/repos/o/r/zipball/refs/tags/v1",
                    None,
                    Some(&bearer())
                ),
                seen(
                    "https://codeload.github.com/o/r/legacy.zip/refs/tags/v1?token=signed",
                    None,
                    None
                ),
            ]
        );
    }

    #[test]
    fn no_translation_without_a_token() {
        let mut script = Script::new([status(404)]);
        let url = "https://github.com/o/r/releases/download/v1/lib.tgz";
        let error = attempt(&mut script, url, None).unwrap_err().to_string();
        assert_eq!(script.seen, vec![seen(url, None, None)]);
        assert_eq!(
            error,
            format!(
                "GitHub refused `{url}` (status 404): \
                 not found, or a private asset that needs `GH_TOKEN` or `GITHUB_TOKEN`"
            )
        );
    }

    #[test]
    fn no_translation_for_other_statuses_or_hosts() {
        let credentials = token();
        let mut script = Script::new([status(403)]);
        let url = "https://github.com/o/r/releases/download/v1/lib.tgz";
        let error = attempt(&mut script, url, Some(&credentials))
            .unwrap_err()
            .to_string();
        assert_eq!(script.seen.len(), 1);
        assert!(
            error.starts_with("GitHub refused `") && error.contains("status 403"),
            "{error}"
        );

        let mut script = Script::new([status(404)]);
        let error = attempt(
            &mut script,
            "https://registry.npmjs.org/lit/-/lit-3.0.0.tgz",
            Some(&credentials),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(script.seen.len(), 1);
        assert_eq!(error, "http status: 404");
    }

    #[test]
    fn a_missing_asset_name_is_reported() {
        let credentials = token();
        let release = br#"{"assets":[{"id":7,"name":"other.tgz"}]}"#;
        let mut script = Script::new([status(404), ok(release)]);
        let url = "https://github.com/o/r/releases/download/v1/lib.tgz";
        let error = attempt(&mut script, url, Some(&credentials))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "no asset `lib.tgz` in the GitHub release `v1` of `o/r`"
        );
        assert_eq!(script.seen.len(), 2);

        let mut script = Script::new([status(404), ok(b"not json")]);
        let error = attempt(&mut script, url, Some(&credentials))
            .unwrap_err()
            .to_string();
        assert_eq!(error, format!("unreadable release document for `{url}`"));

        // A 404 from the lookup itself is the private-asset text, naming the caller's URL.
        let mut script = Script::new([status(404), status(404)]);
        let error = attempt(&mut script, url, Some(&credentials))
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(&format!(
                "GitHub refused `{url}` for the token (status 404)"
            )),
            "{error}"
        );
    }

    #[test]
    fn errors_and_warnings_never_carry_the_token() {
        let credentials = token();
        let url = "https://github.com/o/r/releases/download/v1/lib.tgz";
        let scenarios: Vec<(&str, Vec<super::super::Reply>)> = vec![
            ("github 404 then api 401", vec![status(404), status(401)]),
            ("github 404 then api 403", vec![status(404), status(403)]),
            (
                "github 404 then no such asset",
                vec![status(404), ok(br#"{"assets":[]}"#)],
            ),
            (
                "github 404 then unreadable",
                vec![status(404), ok(b"<html>")],
            ),
            ("github 500", vec![status(500)]),
            ("redirect to http", vec![redirect(302, "http://x/y")]),
            ("redirect without location", vec![status(301)]),
            (
                "too many redirects",
                (0..10)
                    .map(|_| redirect(302, "https://api.github.com/x"))
                    .collect(),
            ),
            ("script exhausted", vec![]),
        ];
        let mut texts = Vec::new();
        for (name, replies) in scenarios {
            let mut script = Script::new(replies);
            let error = attempt(&mut script, url, Some(&credentials)).unwrap_err();
            texts.push((name, error.to_string()));
        }
        // The retry line over a failing attempt.
        let mut warnings = Vec::new();
        // The throttling answer names a zero pause, so the sweep does not sleep.
        let mut script = Script::new([throttled(429, 0), throttled(429, 0)]);
        let _ = super::super::with_retry(
            url,
            &mut |message: &str| warnings.push(message.to_owned()),
            || {
                attempt_with(
                    &mut |hop: &Hop<'_>| script.send(hop),
                    url,
                    None,
                    Some(&credentials),
                )
            },
        );
        assert_eq!(warnings.len(), 1);
        texts.extend(warnings.into_iter().map(|w| ("retry line", w)));
        texts.push(("debug", format!("{credentials:?}")));
        for (name, text) in texts {
            assert!(!text.contains(SENTINEL), "{name}: {text}");
            assert!(!text.contains("Bearer"), "{name}: {text}");
        }
    }
}
