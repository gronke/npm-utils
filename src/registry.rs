//! npm registry interaction: tarball URLs, package metadata, and version
//! resolution against a semver range.

use crate::download;
use crate::package_json::spec::{Range, Spec};
use semver::Version;
use serde_json::Value;

/// How much of a packument to request from the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PackumentDetail {
    /// The abbreviated install document (`application/vnd.npm.install-v1+json`) — much smaller and
    /// faster, but it omits each version's `license`.
    #[default]
    Abbreviated,
    /// The full document, including `license`.
    Full,
}

impl PackumentDetail {
    /// The `Accept` header for this detail level (`None` = the registry's default JSON).
    fn accept(self) -> Option<&'static str> {
        match self {
            PackumentDetail::Abbreviated => Some("application/vnd.npm.install-v1+json"),
            PackumentDetail::Full => None,
        }
    }
}

/// An npm-compatible registry. Defaults to the public registry and the abbreviated packument.
pub struct Registry {
    pub base_url: String,
    /// How much of each packument to fetch. Abbreviated by default (faster); `Full` includes
    /// per-version `license`.
    pub detail: PackumentDetail,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            base_url: "https://registry.npmjs.org".to_string(),
            detail: PackumentDetail::Abbreviated,
        }
    }
}

/// A resolved package version: the exact version, the tarball to fetch, and the
/// registry's `dist.integrity` SRI for that tarball (when the packument publishes one).
///
/// `#[non_exhaustive]` so further fields can be added without a breaking change — this
/// type is only ever *constructed* inside the crate; callers receive and read it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Resolved {
    pub name: String,
    pub version: Version,
    pub tarball_url: String,
    /// The registry's Subresource-Integrity hash (`sha512-<base64>`), when the packument
    /// carries one — verified against the downloaded bytes before extraction. `None` for a
    /// synthesized tarball URL or a packument entry without `dist.integrity`.
    pub integrity: Option<String>,
    /// The version's declared license, normalized to a single SPDX-ish string from the
    /// packument's `license` string / legacy `{ "type": … }` object / `licenses[]` array.
    /// `None` when the packument declares none. Carried so a generated lockfile can record
    /// it for license/compliance tooling (npm's own lockfiles do the same).
    pub license: Option<String>,
}

/// A dependency the audit resolution *skipped* rather than resolved — a non-registry spec
/// (git / path / tarball / `workspace:` / `link:` / alias to one of those) or an optional
/// dependency that failed to resolve — so an audit can report what it did **not** check.
///
/// `#[non_exhaustive]` like [`Resolved`]: constructed only inside the crate, read by callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Omission {
    pub name: String,
    /// The spec/range text that couldn't be followed — verbatim for a manifest root; a parsed
    /// range's display form for a transitive edge (the verbatim text is gone after parsing);
    /// empty when no spec applies (e.g. the `workspaces` marker).
    pub spec: String,
    /// Prose for reports: e.g. `git dependency`, `local path`, `remote tarball`,
    /// `workspace: protocol`, `optional dependency failed to fetch: <cause>`.
    pub reason: String,
}

impl Omission {
    pub(crate) fn new(
        name: impl Into<String>,
        spec: impl Into<String>,
        reason: impl Into<String>,
    ) -> Omission {
        Omission {
            name: name.into(),
            spec: spec.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Omission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.spec.is_empty() {
            write!(f, "{} ({})", self.name, self.reason)
        } else {
            write!(f, "{} ({}: {})", self.name, self.spec, self.reason)
        }
    }
}

/// A single npm registry search result (from the `/-/v1/search` endpoint): the package name, its
/// latest published version, and the descriptive metadata the registry indexes.
///
/// `#[non_exhaustive]` like [`Resolved`] — constructed only inside the crate, read by callers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SearchResult {
    pub name: String,
    /// The latest published version the registry indexes for this package.
    pub version: String,
    /// The package's `description`, when it publishes one.
    pub description: Option<String>,
    /// The package's keywords (empty when none are published).
    pub keywords: Vec<String>,
    /// Publication date of this version (RFC 3339), when the registry reports it.
    pub date: Option<String>,
    /// The package homepage (`links.homepage`), when published.
    pub homepage: Option<String>,
}

impl Registry {
    /// The public npm registry (`https://registry.npmjs.org`).
    pub fn npm() -> Self {
        Self::default()
    }

    /// A registry at a custom base URL (e.g. a private mirror).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            ..Self::default()
        }
    }

    /// Set how much of each packument to fetch (e.g. [`PackumentDetail::Full`] to capture license).
    pub fn with_detail(mut self, detail: PackumentDetail) -> Self {
        self.detail = detail;
        self
    }

    /// Conventional tarball URL for an exact `version`. Handles scoped names:
    /// `@scope/pkg` → `<base>/@scope/pkg/-/pkg-<version>.tgz`.
    pub fn tarball_url(&self, name: &str, version: &str) -> String {
        let unscoped = name.rsplit('/').next().unwrap_or(name);
        format!("{}/{}/-/{}-{}.tgz", self.base_url, name, unscoped, version)
    }

    /// Fetch the package metadata document ("packument").
    pub fn packument(&self, name: &str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        // Scoped names are URL-encoded in the path: `@scope/pkg` → `@scope%2fpkg`.
        let encoded = match name.strip_prefix('@') {
            Some(rest) => format!("@{}", rest.replacen('/', "%2f", 1)),
            None => name.to_string(),
        };
        let url = format!("{}/{}", self.base_url, encoded);
        let bytes = download::fetch_with_accept(&url, self.detail.accept())?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Resolve the newest published version of `name` matching the `range`.
    pub fn resolve(
        &self,
        name: &str,
        range: &Range,
    ) -> Result<Resolved, Box<dyn std::error::Error + Send + Sync>> {
        let doc = self.packument(name)?;
        let (version, tarball, integrity, license) = select_version(&doc, range)
            .ok_or_else(|| format!("no published version of {name} matches {range}"))?;
        let tarball_url = tarball.unwrap_or_else(|| self.tarball_url(name, &version.to_string()));
        Ok(Resolved {
            name: name.to_string(),
            version,
            tarball_url,
            integrity,
            license,
        })
    }

    /// Search the registry for `query`, returning up to `limit` results ranked by the registry's own
    /// relevance score (npm's `/-/v1/search` endpoint). `limit` is clamped to the registry's
    /// `1..=250` window. Each result carries the package's *latest* version and indexed metadata, not
    /// a range resolution — follow up with [`resolve`](Self::resolve) to pin a version.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, Box<dyn std::error::Error + Send + Sync>> {
        let size = limit.clamp(1, 250);
        let url = format!(
            "{}/-/v1/search?text={}&size={size}",
            self.base_url,
            encode_query(query)
        );
        let bytes = download::fetch(&url)?;
        let doc: Value = serde_json::from_slice(&bytes)?;
        Ok(parse_search(&doc))
    }

    /// Resolve the transitive dependency graph of `roots` into a **flat** set — one
    /// version per package name (the npm v3+ `node_modules` layout). Each package's
    /// `dependencies` are read straight from the registry metadata (no tarball
    /// extraction), every child resolved to its newest matching version, and the set
    /// de-duplicated by name. Cyclic graphs terminate (a name is resolved once).
    /// Returns the packages sorted by name. Packuments are prefetched concurrently
    /// (bounded, level by level); resolution order, version selection, errors, and
    /// output are deterministic and identical to a sequential walk.
    ///
    /// MVP limitation: a single version per package name. Two *incompatible*
    /// requirements on the same package — a genuine conflict npm would resolve by
    /// nesting — is reported as an error rather than silently mis-resolved;
    /// [`resolve_tree_nested`](Self::resolve_tree_nested) is the conflict-tolerant
    /// sibling that keeps a version per disagreeing range instead.
    pub fn resolve_tree(
        &self,
        roots: &[(String, Range)],
    ) -> Result<Vec<Resolved>, Box<dyn std::error::Error + Send + Sync>> {
        self.resolve_tree_from(roots, |name| self.packument(name))
    }

    /// Resolve the transitive dependency graph of `roots` the way npm's **nested**
    /// `node_modules` does: a requirement an already-resolved version satisfies reuses it
    /// (npm's dedupe), and requirements that disagree keep one resolved version *per range*
    /// instead of erroring like [`resolve_tree`](Self::resolve_tree). Returns the packages
    /// sorted by name, then version — possibly several versions of one name.
    ///
    /// This is the resolution an *audit* wants — a nested-compatible package/version set, **not**
    /// npm's placement algorithm: no hoisting, no peer-dependency handling, and no os/cpu
    /// filtering (every platform's optional deps are included — their advisories matter
    /// regardless of the auditing machine). `optionalDependencies` are traversed alongside
    /// `dependencies`; non-registry specs (git / path / tarball / `workspace:` / `link:`) and
    /// optional deps that fail to resolve are skipped — this method drops the [`Omission`]
    /// records, the crate-internal observed variant returns them. Installs keep
    /// [`resolve_tree`](Self::resolve_tree)'s flat one-version-per-name guarantee.
    /// Packuments are prefetched concurrently (bounded, level by level); the result is
    /// deterministic and identical to a sequential walk.
    pub fn resolve_tree_nested(
        &self,
        roots: &[(String, Range)],
    ) -> Result<Vec<Resolved>, Box<dyn std::error::Error + Send + Sync>> {
        self.resolve_tree_nested_observed(&required_roots(roots), |_| {})
            .map(|(packages, _omissions)| packages)
    }

    /// [`resolve_tree_nested`](Self::resolve_tree_nested) with a progress observer and the
    /// skipped dependencies reported. `on_event` sees the walk's [`ResolveEvent`]s: packument
    /// fetches beginning and ending on the prefetch pool's worker threads (hence the `Sync`
    /// bound), and each resolved package on the calling thread — the CLI's `audit` verb drives
    /// its `[resolve]` task from them. Roots carry an `optional` flag (an
    /// `optionalDependencies` root may fail to resolve without failing the walk). Resolution
    /// behavior (walk order, nested semantics, errors, sorting) is identical to
    /// [`resolve_tree_nested`](Self::resolve_tree_nested), which is this with a no-op observer,
    /// required roots, and the omissions dropped.
    pub(crate) fn resolve_tree_nested_observed<O>(
        &self,
        roots: &[(String, Range, bool)],
        on_event: O,
    ) -> Result<(Vec<Resolved>, Vec<Omission>), Box<dyn std::error::Error + Send + Sync>>
    where
        O: Fn(ResolveEvent<'_>) + Sync,
    {
        self.resolve_walk(roots, |name| self.packument(name), on_event, NESTED_AUDIT)
    }

    /// [`resolve_tree`](Self::resolve_tree) with an injectable packument source, so the
    /// graph walk can be unit-tested without the network.
    fn resolve_tree_from<F>(
        &self,
        roots: &[(String, Range)],
        get_packument: F,
    ) -> Result<Vec<Resolved>, Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(&str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> + Sync,
    {
        self.resolve_walk(&required_roots(roots), get_packument, |_| {}, FLAT_INSTALL)
            .map(|(packages, _omissions)| packages)
    }

    /// [`resolve_tree_nested`](Self::resolve_tree_nested) with an injectable packument source —
    /// a test-only seam ([`resolve_tree_nested`] itself routes through
    /// [`resolve_tree_nested_observed`](Self::resolve_tree_nested_observed), so nothing outside
    /// the tests calls this). Passes the same [`NESTED_AUDIT`] policy as the public method, so
    /// offline tests exercise the real audit walk.
    #[cfg(test)]
    fn resolve_tree_nested_from<F>(
        &self,
        roots: &[(String, Range)],
        get_packument: F,
    ) -> Result<Vec<Resolved>, Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(&str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> + Sync,
    {
        self.resolve_walk(&required_roots(roots), get_packument, |_| {}, NESTED_AUDIT)
            .map(|(packages, _omissions)| packages)
    }

    /// The shared graph walk. `policy.nested: false` errors on a version conflict (one version
    /// per name — an installable flat tree); `true` resolves the conflicting range to its own
    /// version instead, as npm would by nesting a second copy. Under the audit policy the walk
    /// also traverses `optionalDependencies` and records what it cannot follow as [`Omission`]s
    /// — non-registry specs, and optional edges whose fetch or version selection fails — where
    /// the install policy keeps every failure fatal. `on_event` reports fetch begin/done from
    /// the prefetch workers and each resolution on this thread (a deduped requirement does not
    /// tick); it returns `()` and cannot affect control flow. Terminates on any graph: each
    /// iteration either reuses an already-resolved satisfying version, records a
    /// `(name, version)` pair that wasn't there before, or skips.
    ///
    /// The walk proceeds in rounds: each takes the whole BFS frontier, prefetches its packuments
    /// concurrently ([`prefetch_packuments`]), and then processes the entries in their original
    /// FIFO order — the exact sequence the one-at-a-time walk consumed, merely chunked — so
    /// version selection, dedupe, errors, omissions, and output are identical to a sequential
    /// walk.
    fn resolve_walk<F, O>(
        &self,
        roots: &[(String, Range, bool)],
        get_packument: F,
        on_event: O,
        policy: WalkPolicy,
    ) -> Result<(Vec<Resolved>, Vec<Omission>), Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(&str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> + Sync,
        O: Fn(ResolveEvent<'_>) + Sync,
    {
        use std::collections::{HashMap, VecDeque};
        let mut packuments: HashMap<String, Value> = HashMap::new();
        let mut fetch_errors: HashMap<String, String> = HashMap::new();
        let mut resolved: HashMap<String, Vec<Resolved>> = HashMap::new();
        let mut omissions: Vec<Omission> = Vec::new();
        let mut count = 0usize;
        let mut queue: VecDeque<(String, Range, bool)> = roots.iter().cloned().collect();

        while !queue.is_empty() {
            let batch: Vec<(String, Range, bool)> = queue.drain(..).collect();
            prefetch_packuments(
                &batch,
                &mut packuments,
                &mut fetch_errors,
                !policy.skip_unsupported,
                &get_packument,
                &on_event,
            )?;
            for (name, range, optional) in batch {
                if let Some(existing) = resolved.get(&name) {
                    if existing.iter().any(|r| range.matches(&r.version)) {
                        continue; // already resolved to a satisfying version — dedup
                    }
                    if !policy.nested {
                        return Err(format!(
                            "version conflict for `{name}`: resolved {} but also required \
                             `{range}` (flat node_modules install resolves one version per \
                             package)",
                            existing[0].version
                        )
                        .into());
                    }
                    // Nested: fall through and resolve this range's own version. The pick can't
                    // duplicate an existing one — a satisfying existing version was handled above.
                }
                // A name whose prefetch failed: fatal on a required edge, an omission on an
                // optional one — npm likewise tolerates an optional dep that fails to resolve.
                if let Some(cause) = fetch_errors.get(&name) {
                    if policy.skip_unsupported && optional {
                        push_unique(
                            &mut omissions,
                            Omission::new(
                                &name,
                                range.to_string(),
                                format!("optional dependency failed to fetch: {cause}"),
                            ),
                        );
                        continue;
                    }
                    return Err(cause.clone().into());
                }
                // The prefetch has already cached every batch name; kept as a defensive fallback.
                if !packuments.contains_key(&name) {
                    let doc = get_packument(&name)?;
                    packuments.insert(name.clone(), doc);
                }
                let doc = &packuments[&name];
                let (version, tarball, integrity, license) = match select_version(doc, &range) {
                    Some(selected) => selected,
                    None if policy.skip_unsupported && optional => {
                        push_unique(
                            &mut omissions,
                            Omission::new(
                                &name,
                                range.to_string(),
                                "optional dependency: no published version matches",
                            ),
                        );
                        continue;
                    }
                    None => {
                        return Err(format!("no published version of {name} matches {range}").into())
                    }
                };
                let deps = dependencies_of(doc, &version, policy.include_optional);
                let tarball_url =
                    tarball.unwrap_or_else(|| self.tarball_url(&name, &version.to_string()));
                for (dep_name, dep_spec, dep_optional) in deps {
                    if policy.skip_unsupported {
                        // Non-registry child specs become omissions; a child of an optional
                        // package is itself optional (npm skips the whole optional subtree).
                        let action = classify_dep(&dep_name, &dep_spec).map_err(|e| {
                            format!(
                                "{name}@{version} dependency `{dep_name}`: unsupported version \
                                 {dep_spec:?}: {e}"
                            )
                        })?;
                        match action {
                            EdgeAction::Resolve {
                                name: target,
                                range: dep_range,
                            } => queue.push_back((target, dep_range, optional || dep_optional)),
                            EdgeAction::Omit(omission) => push_unique(&mut omissions, omission),
                        }
                    } else {
                        // Transitive deps routinely use npm `||`/space ranges; parse the full
                        // grammar.
                        let dep_range = Range::parse(&dep_spec).map_err(|e| {
                            format!(
                                "{name}@{version} dependency `{dep_name}`: unsupported version \
                                 {dep_spec:?}: {e}"
                            )
                        })?;
                        queue.push_back((dep_name, dep_range, false));
                    }
                }
                let package = Resolved {
                    name,
                    version,
                    tarball_url,
                    integrity,
                    license,
                };
                count += 1;
                on_event(ResolveEvent::Resolved {
                    count,
                    package: &package,
                });
                resolved
                    .entry(package.name.clone())
                    .or_default()
                    .push(package);
            }
        }
        let mut out: Vec<Resolved> = resolved.into_values().flatten().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
        omissions.sort_by(|a, b| (&a.name, &a.spec).cmp(&(&b.name, &b.spec)));
        Ok((out, omissions))
    }
}

/// What the graph walk does with the dependency classes an install can't or shouldn't follow;
/// see [`FLAT_INSTALL`] and [`NESTED_AUDIT`].
#[derive(Clone, Copy)]
struct WalkPolicy {
    /// Keep one version per disagreeing range (npm's nesting) instead of erroring on a conflict.
    nested: bool,
    /// Traverse `optionalDependencies` alongside `dependencies`.
    include_optional: bool,
    /// Record non-registry specs and failed optional edges as [`Omission`]s instead of aborting.
    skip_unsupported: bool,
}

/// The install walk: flat, `dependencies` only, every failure aborts.
const FLAT_INSTALL: WalkPolicy = WalkPolicy {
    nested: false,
    include_optional: false,
    skip_unsupported: false,
};

/// The audit walk: nested, optional deps included, unauditable edges recorded as omissions.
const NESTED_AUDIT: WalkPolicy = WalkPolicy {
    nested: true,
    include_optional: true,
    skip_unsupported: true,
};

/// Adapt plain `(name, range)` roots to required (non-optional) walk edges.
fn required_roots(roots: &[(String, Range)]) -> Vec<(String, Range, bool)> {
    roots
        .iter()
        .map(|(name, range)| (name.clone(), range.clone(), false))
        .collect()
}

/// A progress event from the shared graph walk
/// ([`Registry::resolve_tree_nested_observed`]). `FetchBegin`/`FetchDone` fire on the prefetch
/// pool's worker threads — cross-name order is nondeterministic, but per name Begin precedes
/// Done, and Done fires on success *and* failure ("no longer in flight"). `Resolved` fires on
/// the calling thread once per newly resolved package version (a deduped requirement does not
/// tick), never concurrently with fetch events — the walk alternates prefetch rounds with
/// sequential processing.
pub(crate) enum ResolveEvent<'a> {
    FetchBegin {
        #[cfg_attr(not(feature = "cli"), allow(dead_code))]
        name: &'a str,
    },
    FetchDone {
        #[cfg_attr(not(feature = "cli"), allow(dead_code))]
        name: &'a str,
    },
    Resolved {
        /// The running total — the walk's own count, pinned by the determinism tests;
        /// renderers keep their own counters.
        #[allow(dead_code)]
        count: usize,
        #[cfg_attr(not(feature = "cli"), allow(dead_code))]
        package: &'a Resolved,
    },
}

/// Record `omission` unless an identical one (same name, spec, reason) is already recorded —
/// the same unsupported dep reached from several parents reports once.
fn push_unique(omissions: &mut Vec<Omission>, omission: Omission) {
    if !omissions.contains(&omission) {
        omissions.push(omission);
    }
}

/// One classified dependency edge under the audit policy ([`classify_dep`]).
pub(crate) enum EdgeAction {
    /// A registry-resolvable edge — for an `npm:` alias, `name` is the alias **target**.
    Resolve { name: String, range: Range },
    /// An edge the audit resolution cannot follow, recorded rather than resolved.
    Omit(Omission),
}

/// Classify one dependency edge for the audit walk: registry ranges (and `npm:` aliases to
/// registry ranges) resolve; git / path / tarball / `workspace:` / `link:` specs — and aliases
/// to them — are omissions. A malformed registry range is a hard error (fail clearly), never an
/// omission. The `workspace:`/`link:` prefixes are checked before [`Spec::parse`], which has no
/// variants for them (`link:../x` would misread as a git shorthand).
pub(crate) fn classify_dep(
    name: &str,
    spec: &str,
) -> Result<EdgeAction, Box<dyn std::error::Error + Send + Sync>> {
    let spec = spec.trim();
    if spec.starts_with("workspace:") {
        return Ok(EdgeAction::Omit(Omission::new(
            name,
            spec,
            "workspace: protocol",
        )));
    }
    if spec.starts_with("link:") {
        return Ok(EdgeAction::Omit(Omission::new(
            name,
            spec,
            "link: protocol",
        )));
    }
    match Spec::parse(spec) {
        Spec::Git { .. } => Ok(EdgeAction::Omit(Omission::new(
            name,
            spec,
            "git dependency",
        ))),
        Spec::Tarball(_) => Ok(EdgeAction::Omit(Omission::new(
            name,
            spec,
            "remote tarball",
        ))),
        Spec::Path(_) => Ok(EdgeAction::Omit(Omission::new(name, spec, "local path"))),
        Spec::Alias {
            name: target,
            spec: inner,
        } => match *inner {
            // The alias target is what gets installed — resolve and audit it.
            Spec::Registry(range) => Ok(EdgeAction::Resolve {
                name: target,
                range: Range::parse(&range)?,
            }),
            _ => Ok(EdgeAction::Omit(Omission::new(
                name,
                spec,
                "npm: alias to a non-registry target",
            ))),
        },
        Spec::Registry(range) => Ok(EdgeAction::Resolve {
            name: name.to_string(),
            range: Range::parse(&range)?,
        }),
    }
}

/// How many packuments [`prefetch_packuments`] fetches concurrently per round. The shared
/// agent's per-host idle pool ([`crate::download`]) is sized to match — keep the two in sync.
const PACKUMENT_CONCURRENCY: usize = 8;

/// One prefetched packument outcome (a private alias keeping the slot type readable).
type FetchedPackument = Result<Value, Box<dyn std::error::Error + Send + Sync>>;

/// Fetch the packuments a round's `batch` needs — the distinct names not already in
/// `packuments` (nor already recorded as failed), in first-appearance order — with up to
/// [`PACKUMENT_CONCURRENCY`] threads, and insert them in that same order.
///
/// Failure handling follows the walk policy. `fail_fast` (the install walk): the first failure
/// in batch order aborts with the original error — exactly the sequential walk's first hit.
/// Otherwise (the audit walk) every failure is recorded in `fetch_errors` and the walk decides
/// per entry: fatal for a required edge, an [`Omission`] for an optional one. A failed name is
/// never re-fetched — later rounds see it in `fetch_errors` and skip it in the missing set — so
/// every name is fetched exactly once regardless of outcome. Each attempted fetch is bracketed
/// by [`ResolveEvent::FetchBegin`]/[`ResolveEvent::FetchDone`], emitted on the worker thread.
///
/// No over-fetch on success paths: a name enters the walk's `resolved` map only via the
/// fetch-and-select path, so every resolved name is already cached and a dedupe-bound batch
/// entry never lands in the missing set.
fn prefetch_packuments<F, O>(
    batch: &[(String, Range, bool)],
    packuments: &mut std::collections::HashMap<String, Value>,
    fetch_errors: &mut std::collections::HashMap<String, String>,
    fail_fast: bool,
    get_packument: &F,
    on_event: &O,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(&str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> + Sync,
    O: Fn(ResolveEvent<'_>) + Sync,
{
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let mut seen: HashSet<&str> = HashSet::new();
    let missing: Vec<&str> = batch
        .iter()
        .map(|(name, _, _)| name.as_str())
        .filter(|name| {
            !packuments.contains_key(*name)
                && !fetch_errors.contains_key(*name)
                && seen.insert(*name)
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    // One slot per missing name; workers claim indices through the atomic counter, so each slot
    // is written exactly once. Relaxed suffices: the counter only partitions indices, and result
    // visibility is ordered by each slot's Mutex and the scope's join.
    let results: Vec<Mutex<Option<FetchedPackument>>> =
        missing.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..missing.len().min(PACKUMENT_CONCURRENCY) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(name) = missing.get(i) else { break };
                on_event(ResolveEvent::FetchBegin { name });
                let result = get_packument(name); // fetched before the slot lock is taken
                on_event(ResolveEvent::FetchDone { name });
                *results[i]
                    .lock()
                    .expect("no panic while holding a slot lock") = Some(result);
            });
        }
    });

    for (name, slot) in missing.iter().zip(results) {
        let result = slot
            .into_inner()
            .expect("a panicking worker re-panics out of the scope before this runs")
            .expect("every index below missing.len() was claimed and filled");
        match result {
            Ok(doc) => {
                packuments.insert((*name).to_string(), doc);
            }
            Err(e) if fail_fast => return Err(e),
            Err(e) => {
                fetch_errors.insert((*name).to_string(), e.to_string());
            }
        }
    }
    Ok(())
}

/// The fields [`select_version`] extracts for the newest matching version:
/// `(version, dist.tarball, dist.integrity, license)`.
type SelectedVersion = (Version, Option<String>, Option<String>, Option<String>);

/// Pick the newest version in a packument's `versions` map that satisfies the `range`,
/// returning it with the `dist.tarball` URL, the `dist.integrity` SRI, and the declared
/// `license` the registry advertises (each `None` if absent). Factored out for unit testing
/// without network access.
fn select_version(doc: &Value, range: &Range) -> Option<SelectedVersion> {
    let versions = doc.get("versions")?.as_object()?;
    let mut best: Option<SelectedVersion> = None;
    for (ver_str, meta) in versions {
        let Ok(ver) = Version::parse(ver_str) else {
            continue;
        };
        if !range.matches(&ver) {
            continue;
        }
        if best.as_ref().map(|(b, ..)| ver > *b).unwrap_or(true) {
            let dist = meta.get("dist");
            let string_at = |key: &str| {
                dist.and_then(|d| d.get(key))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            best = Some((
                ver,
                string_at("tarball"),
                string_at("integrity"),
                license_of(meta),
            ));
        }
    }
    best
}

/// Normalize a packument version entry's license to a single SPDX-ish string. npm uses a
/// `license` string today; older packages used a `{ "type": … }` object or a
/// `licenses: [{ "type": … }]` array — collapse all three (joining a multi-entry array with
/// `" OR "`), returning `None` when none is declared.
fn license_of(meta: &Value) -> Option<String> {
    crate::package_json::normalize_license(meta)
}

/// The npm dependency-spec → `VersionReq` parser lives in the [`crate::package_json`] module
/// (the package-spec grammar); re-exported here for back-compat as `registry::version_req`.
pub use crate::package_json::spec::version_req;

/// The `dependencies` — and, when `include_optional`, the `optionalDependencies` — of a
/// specific version, read from a packument, as `(name, spec, optional)` triples. Both maps ride
/// inline in each version's metadata (the abbreviated install packument included), so the
/// transitive walk discovers children without extracting any tarball. An `optionalDependencies`
/// entry overrides a same-name `dependencies` entry **in place** (npm applies optional entries
/// over regular ones), keeping one triple per name in a deterministic order.
fn dependencies_of(
    doc: &Value,
    version: &Version,
    include_optional: bool,
) -> Vec<(String, String, bool)> {
    let meta = doc.get("versions").and_then(|v| v.get(version.to_string()));
    let read = |key: &str| -> Vec<(String, String)> {
        meta.and_then(|m| m.get(key))
            .and_then(|d| d.as_object())
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out: Vec<(String, String, bool)> = read("dependencies")
        .into_iter()
        .map(|(name, spec)| (name, spec, false))
        .collect();
    if include_optional {
        for (name, spec) in read("optionalDependencies") {
            match out.iter_mut().find(|(existing, ..)| *existing == name) {
                Some(entry) => *entry = (name, spec, true),
                None => out.push((name, spec, true)),
            }
        }
    }
    out
}

/// Parse the registry's `/-/v1/search` response into [`SearchResult`]s, skipping any malformed
/// entry. The response shape is `{ "objects": [ { "package": { name, version, description,
/// keywords, date, links: { homepage } } }, … ] }`. Factored out for unit testing without network.
fn parse_search(doc: &Value) -> Vec<SearchResult> {
    let Some(objects) = doc.get("objects").and_then(Value::as_array) else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|obj| {
            let pkg = obj.get("package")?;
            let name = pkg.get("name")?.as_str()?.to_string();
            let string_field = |key: &str| pkg.get(key).and_then(Value::as_str).map(str::to_string);
            let keywords = pkg
                .get("keywords")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let homepage = pkg
                .get("links")
                .and_then(|links| links.get("homepage"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(SearchResult {
                name,
                version: string_field("version").unwrap_or_default(),
                description: string_field("description"),
                keywords,
                date: string_field("date"),
                homepage,
            })
        })
        .collect()
}

/// Percent-encode a search query for use as a URL query-string value: keep the RFC 3986 unreserved
/// set, `%`-escape everything else (the query may contain spaces, scopes `@`, or slashes).
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tarball_url_handles_scoped_and_unscoped() {
        let reg = Registry::npm();
        assert_eq!(
            reg.tarball_url("lit", "3.3.3"),
            "https://registry.npmjs.org/lit/-/lit-3.3.3.tgz"
        );
        assert_eq!(
            reg.tarball_url("@lit/context", "1.1.6"),
            "https://registry.npmjs.org/@lit/context/-/context-1.1.6.tgz"
        );
    }

    #[test]
    fn select_version_picks_newest_matching() {
        let doc = json!({
            "versions": {
                "3.1.0": { "dist": { "tarball": "https://r/lit-3.1.0.tgz" } },
                "3.3.3": {
                    "license": "BSD-3-Clause",
                    "dist": {
                        "tarball": "https://r/lit-3.3.3.tgz",
                        "integrity": "sha512-deadbeef"
                    }
                },
                "4.0.0": { "dist": { "tarball": "https://r/lit-4.0.0.tgz" } },
                "2.9.9": {}
            }
        });
        let (ver, tarball, integrity, license) =
            select_version(&doc, &"^3".parse().unwrap()).unwrap();
        assert_eq!(ver, Version::parse("3.3.3").unwrap());
        assert_eq!(tarball.as_deref(), Some("https://r/lit-3.3.3.tgz"));
        // The registry's dist.integrity rides along so node_modules can verify the tarball.
        assert_eq!(integrity.as_deref(), Some("sha512-deadbeef"));
        // The declared license rides along too, so a generated lockfile can record it.
        assert_eq!(license.as_deref(), Some("BSD-3-Clause"));
    }

    #[test]
    fn select_version_integrity_is_none_when_absent() {
        // A dist with a tarball but no integrity → integrity None. node_modules then refuses
        // to install it unverified (from_lockfile is likewise strict on a missing sha512).
        let doc = json!({ "versions": {
            "1.0.0": { "dist": { "tarball": "https://r/x-1.0.0.tgz" } }
        }});
        let (_, tarball, integrity, _license) =
            select_version(&doc, &"^1".parse().unwrap()).unwrap();
        assert_eq!(tarball.as_deref(), Some("https://r/x-1.0.0.tgz"));
        assert!(integrity.is_none());
    }

    #[test]
    fn select_version_none_when_no_match() {
        let doc = json!({ "versions": { "1.0.0": {}, "2.0.0": {} } });
        assert!(select_version(&doc, &"^5".parse().unwrap()).is_none());
    }

    #[test]
    fn license_of_normalizes_string_object_and_array_forms() {
        // Modern SPDX string (what nearly every package publishes today).
        assert_eq!(
            license_of(&json!({ "license": "MIT" })).as_deref(),
            Some("MIT")
        );
        // Legacy `{ type }` object.
        assert_eq!(
            license_of(&json!({ "license": { "type": "Apache-2.0", "url": "x" } })).as_deref(),
            Some("Apache-2.0")
        );
        // Legacy `licenses: [{ type }]` array → joined with " OR ".
        assert_eq!(
            license_of(&json!({ "licenses": [{ "type": "MIT" }, { "type": "Apache-2.0" }] }))
                .as_deref(),
            Some("MIT OR Apache-2.0")
        );
        // None declared.
        assert_eq!(license_of(&json!({ "dist": {} })), None);
    }

    /// A one-version packument carrying a `dependencies` map, mirroring the registry's
    /// shape, so the graph walk can be exercised without the network.
    fn packument_with(version: &str, deps: &[(&str, &str)]) -> Value {
        let dep_map: serde_json::Map<String, Value> = deps
            .iter()
            .map(|(n, s)| (n.to_string(), json!(*s)))
            .collect();
        let mut versions = serde_json::Map::new();
        versions.insert(
            version.to_string(),
            json!({
                "dist": {
                    "tarball": format!("https://r/{version}.tgz"),
                    "integrity": format!("sha512-{version}"),
                },
                "dependencies": Value::Object(dep_map),
            }),
        );
        json!({ "versions": Value::Object(versions) })
    }

    #[test]
    fn resolve_tree_walks_transitively_dedups_and_handles_cycles() {
        // a@1 → {b ^1, c ^1}; b@1 → {c ^1} (shared); c@1 → {a ^1} (cycle back to root).
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "a".into(),
            packument_with("1.0.0", &[("b", "^1"), ("c", "^1")]),
        );
        pkgs.insert("b".into(), packument_with("1.2.0", &[("c", "^1")]));
        pkgs.insert("c".into(), packument_with("1.5.0", &[("a", "^1")]));

        let roots = vec![("a".to_string(), "^1".parse().unwrap())];
        let resolved = Registry::npm()
            .resolve_tree_from(&roots, |name| {
                pkgs.get(name)
                    .cloned()
                    .ok_or_else(|| format!("no packument for {name}").into())
            })
            .unwrap();

        // Each of a, b, c resolved exactly once (cycle + shared dep deduped), sorted by name.
        let names: Vec<&str> = resolved.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        let ver = |n: &str| {
            resolved
                .iter()
                .find(|r| r.name == n)
                .unwrap()
                .version
                .to_string()
        };
        assert_eq!(ver("b"), "1.2.0");
        assert_eq!(ver("c"), "1.5.0");

        // dist.integrity threads through the transitive walk, ready for verification.
        let integrity = |n: &str| {
            resolved
                .iter()
                .find(|r| r.name == n)
                .unwrap()
                .integrity
                .clone()
        };
        assert_eq!(integrity("b").as_deref(), Some("sha512-1.2.0"));
    }

    #[test]
    fn resolve_tree_resolves_a_transitive_or_range() {
        // Regression: a transitive dep with an npm `||` range (e.g. @lit/context →
        // @lit/reactive-element `^1.6.2 || ^2.1.0`) must resolve, not fail to parse the `||`.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "ctx".into(),
            packument_with("1.1.6", &[("re", "^1.6.2 || ^2.1.0")]),
        );
        pkgs.insert("re".into(), packument_with("2.1.0", &[]));

        let roots = vec![("ctx".to_string(), "^1".parse().unwrap())];
        let resolved = Registry::npm()
            .resolve_tree_from(&roots, |name| {
                pkgs.get(name)
                    .cloned()
                    .ok_or_else(|| format!("no packument for {name}").into())
            })
            .unwrap();

        let names: Vec<&str> = resolved.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["ctx", "re"],
            "the `||`-ranged transitive dep resolved"
        );
        assert_eq!(
            resolved
                .iter()
                .find(|r| r.name == "re")
                .unwrap()
                .version
                .to_string(),
            "2.1.0"
        );
    }

    #[test]
    fn resolve_tree_errors_on_version_conflict() {
        // root requires x ^1; root also requires y, and y requires x ^2 → incompatible.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "x".into(),
            json!({ "versions": {
                "1.0.0": { "dist": { "tarball": "https://r/x1.tgz" } },
                "2.0.0": { "dist": { "tarball": "https://r/x2.tgz" } }
            }}),
        );
        pkgs.insert("y".into(), packument_with("1.0.0", &[("x", "^2")]));

        let roots = vec![
            ("x".to_string(), "^1".parse().unwrap()),
            ("y".to_string(), "^1".parse().unwrap()),
        ];
        let err = Registry::npm()
            .resolve_tree_from(&roots, |name| {
                pkgs.get(name)
                    .cloned()
                    .ok_or_else(|| format!("no packument for {name}").into())
            })
            .unwrap_err();
        assert!(err.to_string().contains("version conflict"), "got: {err}");
    }

    #[test]
    fn resolve_tree_nested_keeps_a_version_per_conflicting_range() {
        // The exact graph the flat resolver rejects above: root wants x ^1, y wants x ^2.
        // Nested resolution keeps both x versions — what an installed tree would contain.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "x".into(),
            json!({ "versions": {
                "1.0.0": { "dist": { "tarball": "https://r/x1.tgz" } },
                "2.0.0": { "dist": { "tarball": "https://r/x2.tgz" } }
            }}),
        );
        pkgs.insert("y".into(), packument_with("1.0.0", &[("x", "^2")]));

        let roots = vec![
            ("x".to_string(), "^1".parse().unwrap()),
            ("y".to_string(), "^1".parse().unwrap()),
        ];
        let resolved = Registry::npm()
            .resolve_tree_nested_from(&roots, |name| {
                pkgs.get(name)
                    .cloned()
                    .ok_or_else(|| format!("no packument for {name}").into())
            })
            .unwrap();

        let pairs: Vec<String> = resolved
            .iter()
            .map(|r| format!("{}@{}", r.name, r.version))
            .collect();
        assert_eq!(
            pairs,
            ["x@1.0.0", "x@2.0.0", "y@1.0.0"],
            "both conflicting x versions kept, sorted by name then version"
        );
    }

    #[test]
    fn resolve_tree_nested_dedupes_and_terminates_on_a_conflicting_cycle() {
        // a@1 → b ^1; b@1 → a ^2 (conflicts with the root's a@1); a@2 → b ^1 (cycle, already
        // satisfied). The walk must dedupe satisfying ranges, nest the conflicting one exactly
        // once, and terminate.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "a".into(),
            json!({ "versions": {
                "1.0.0": { "dist": { "tarball": "https://r/a1.tgz" }, "dependencies": { "b": "^1" } },
                "2.0.0": { "dist": { "tarball": "https://r/a2.tgz" }, "dependencies": { "b": "^1" } }
            }}),
        );
        pkgs.insert("b".into(), packument_with("1.0.0", &[("a", "^2")]));

        let roots = vec![("a".to_string(), "^1".parse().unwrap())];
        let resolved = Registry::npm()
            .resolve_tree_nested_from(&roots, |name| {
                pkgs.get(name)
                    .cloned()
                    .ok_or_else(|| format!("no packument for {name}").into())
            })
            .unwrap();

        let pairs: Vec<String> = resolved
            .iter()
            .map(|r| format!("{}@{}", r.name, r.version))
            .collect();
        assert_eq!(pairs, ["a@1.0.0", "a@2.0.0", "b@1.0.0"]);
    }

    /// [`packument_with`], plus an `optionalDependencies` map.
    fn packument_with_optional(
        version: &str,
        deps: &[(&str, &str)],
        optional: &[(&str, &str)],
    ) -> Value {
        let mut doc = packument_with(version, deps);
        let opt_map: serde_json::Map<String, Value> = optional
            .iter()
            .map(|(n, s)| (n.to_string(), json!(*s)))
            .collect();
        doc["versions"][version]["optionalDependencies"] = Value::Object(opt_map);
        doc
    }

    /// A `HashMap`-backed packument source for the walk tests.
    fn source_from(
        pkgs: &std::collections::HashMap<String, Value>,
    ) -> impl Fn(&str) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> + Sync + '_ {
        |name| {
            pkgs.get(name)
                .cloned()
                .ok_or_else(|| format!("no packument for {name}").into())
        }
    }

    fn pairs(resolved: &[Resolved]) -> Vec<String> {
        resolved
            .iter()
            .map(|r| format!("{}@{}", r.name, r.version))
            .collect()
    }

    #[test]
    fn resolve_tree_nested_terminates_on_a_mutual_cycle() {
        // a@1 ↔ b@1 require each other with satisfiable ranges: dedupe breaks the cycle.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert("a".into(), packument_with("1.0.0", &[("b", "^1")]));
        pkgs.insert("b".into(), packument_with("1.0.0", &[("a", "^1")]));

        let roots = vec![("a".to_string(), "^1".parse().unwrap())];
        let resolved = Registry::npm()
            .resolve_tree_nested_from(&roots, source_from(&pkgs))
            .unwrap();
        assert_eq!(pairs(&resolved), ["a@1.0.0", "b@1.0.0"]);
    }

    #[test]
    fn resolve_tree_nested_reuses_a_satisfying_version_across_parents() {
        // The plain diamond under the nested policy: a → {b, c}, b → c — c resolves once.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "a".into(),
            packument_with("1.0.0", &[("b", "^1"), ("c", "^1")]),
        );
        pkgs.insert("b".into(), packument_with("1.2.0", &[("c", "^1")]));
        pkgs.insert("c".into(), packument_with("1.5.0", &[]));

        let roots = vec![("a".to_string(), "^1".parse().unwrap())];
        let resolved = Registry::npm()
            .resolve_tree_nested_from(&roots, source_from(&pkgs))
            .unwrap();
        assert_eq!(pairs(&resolved), ["a@1.0.0", "b@1.2.0", "c@1.5.0"]);
    }

    #[test]
    fn optional_dependencies_traverse_nested_and_stay_ignored_flat() {
        // root has no regular deps and one optional dep; opt@1 pulls a regular child of its own,
        // which inherits the optional flag transitively.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "root".into(),
            packument_with_optional("1.0.0", &[], &[("opt", "^1")]),
        );
        pkgs.insert("opt".into(), packument_with("1.1.0", &[("leaf", "^1")]));
        pkgs.insert("leaf".into(), packument_with("1.0.0", &[]));

        let roots = vec![("root".to_string(), "^1".parse().unwrap())];
        let resolved = Registry::npm()
            .resolve_tree_nested_from(&roots, source_from(&pkgs))
            .unwrap();
        assert_eq!(pairs(&resolved), ["leaf@1.0.0", "opt@1.1.0", "root@1.0.0"]);

        // The flat install walk ignores optionalDependencies entirely (unchanged behavior).
        let resolved = Registry::npm()
            .resolve_tree_from(&roots, source_from(&pkgs))
            .unwrap();
        assert_eq!(pairs(&resolved), ["root@1.0.0"]);
    }

    #[test]
    fn dependencies_of_lets_an_optional_entry_override_in_place() {
        let doc = packument_with_optional("1.0.0", &[("x", "^1"), ("y", "^1")], &[("x", "^2")]);
        let version = Version::parse("1.0.0").unwrap();
        assert_eq!(
            dependencies_of(&doc, &version, true),
            [
                ("x".to_string(), "^2".to_string(), true),
                ("y".to_string(), "^1".to_string(), false),
            ],
            "the optional entry replaces the regular one in place, flag flipped"
        );
        assert_eq!(
            dependencies_of(&doc, &version, false),
            [
                ("x".to_string(), "^1".to_string(), false),
                ("y".to_string(), "^1".to_string(), false),
            ],
            "the install walk never sees optionalDependencies"
        );
    }

    #[test]
    fn optional_fetch_failure_is_an_omission_and_siblings_still_resolve() {
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "root".into(),
            packument_with_optional("1.0.0", &[("b", "^1")], &[("gone", "^2")]),
        );
        pkgs.insert("b".into(), packument_with("1.0.0", &[]));

        let roots = vec![("root".to_string(), "^1".parse().unwrap(), false)];
        let (resolved, omissions) = Registry::npm()
            .resolve_walk(&roots, source_from(&pkgs), |_| {}, NESTED_AUDIT)
            .unwrap();
        assert_eq!(pairs(&resolved), ["b@1.0.0", "root@1.0.0"]);
        assert_eq!(omissions.len(), 1, "{omissions:?}");
        assert_eq!(omissions[0].name, "gone");
        assert!(
            omissions[0]
                .reason
                .contains("optional dependency failed to fetch"),
            "{}",
            omissions[0]
        );
    }

    #[test]
    fn optional_version_mismatch_is_an_omission() {
        // `opt` exists but publishes no version matching the optional range.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "root".into(),
            packument_with_optional("1.0.0", &[], &[("opt", "^9")]),
        );
        pkgs.insert("opt".into(), packument_with("1.0.0", &[]));

        let roots = vec![("root".to_string(), "^1".parse().unwrap(), false)];
        let (resolved, omissions) = Registry::npm()
            .resolve_walk(&roots, source_from(&pkgs), |_| {}, NESTED_AUDIT)
            .unwrap();
        assert_eq!(pairs(&resolved), ["root@1.0.0"]);
        assert_eq!(omissions.len(), 1);
        assert!(
            omissions[0].reason.contains("no published version matches"),
            "{}",
            omissions[0]
        );
    }

    #[test]
    fn required_fetch_failure_still_aborts_the_audit_walk() {
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert("root".into(), packument_with("1.0.0", &[("gone", "^1")]));

        let roots = vec![("root".to_string(), "^1".parse().unwrap(), false)];
        let err = Registry::npm()
            .resolve_walk(&roots, source_from(&pkgs), |_| {}, NESTED_AUDIT)
            .unwrap_err();
        assert!(err.to_string().contains("no packument for gone"), "{err}");
    }

    #[test]
    fn a_name_needed_by_a_required_edge_aborts_even_after_an_optional_omission() {
        // Round 2: a's optional edge on `x` fails → omission. Round 3: b's *required* edge on
        // `x` must abort from the stored error — and `x` is fetched exactly once overall.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert("root".into(), packument_with("1.0.0", &[("a", "^1")]));
        pkgs.insert(
            "a".into(),
            packument_with_optional("1.0.0", &[("b", "^1")], &[("x", "^1")]),
        );
        pkgs.insert("b".into(), packument_with("1.0.0", &[("x", "^1")]));

        let attempts: std::sync::Mutex<std::collections::HashMap<String, usize>> =
            std::sync::Mutex::new(std::collections::HashMap::new());
        let get = |name: &str| {
            *attempts
                .lock()
                .unwrap()
                .entry(name.to_string())
                .or_insert(0) += 1;
            pkgs.get(name)
                .cloned()
                .ok_or_else(|| format!("no packument for {name}").into())
        };

        let roots = vec![("root".to_string(), "^1".parse().unwrap(), false)];
        let err = Registry::npm()
            .resolve_walk(&roots, get, |_| {}, NESTED_AUDIT)
            .unwrap_err();
        assert!(err.to_string().contains("no packument for x"), "{err}");
        assert_eq!(
            attempts.into_inner().unwrap().get("x"),
            Some(&1),
            "a failed name is never re-fetched"
        );
    }

    #[test]
    fn transitive_git_spec_is_an_omission_nested_and_an_error_flat() {
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "a".into(),
            packument_with(
                "1.0.0",
                &[("g", "git+https://github.com/x/y.git"), ("b", "^1")],
            ),
        );
        pkgs.insert("b".into(), packument_with("1.0.0", &[]));

        let roots3 = vec![("a".to_string(), "^1".parse().unwrap(), false)];
        let (resolved, omissions) = Registry::npm()
            .resolve_walk(&roots3, source_from(&pkgs), |_| {}, NESTED_AUDIT)
            .unwrap();
        assert_eq!(pairs(&resolved), ["a@1.0.0", "b@1.0.0"]);
        assert_eq!(omissions.len(), 1);
        assert_eq!(
            omissions[0].to_string(),
            "g (git+https://github.com/x/y.git: git dependency)"
        );

        // The install walk keeps failing loudly on the same spec.
        let roots = vec![("a".to_string(), "^1".parse().unwrap())];
        let err = Registry::npm()
            .resolve_tree_from(&roots, source_from(&pkgs))
            .unwrap_err();
        assert!(err.to_string().contains("unsupported version"), "{err}");
    }

    #[test]
    fn npm_alias_to_a_registry_range_resolves_the_target() {
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "a".into(),
            packument_with("1.0.0", &[("aliased", "npm:real@^1")]),
        );
        pkgs.insert("real".into(), packument_with("1.5.0", &[]));

        let roots = vec![("a".to_string(), "^1".parse().unwrap())];
        let resolved = Registry::npm()
            .resolve_tree_nested_from(&roots, source_from(&pkgs))
            .unwrap();
        assert_eq!(
            pairs(&resolved),
            ["a@1.0.0", "real@1.5.0"],
            "the alias target resolves under its real name"
        );
    }

    #[test]
    fn garbage_transitive_range_stays_a_hard_error_under_the_audit_policy() {
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "a".into(),
            packument_with("1.0.0", &[("bad", "%% nope %%")]),
        );

        let roots = vec![("a".to_string(), "^1".parse().unwrap())];
        let err = Registry::npm()
            .resolve_tree_nested_from(&roots, source_from(&pkgs))
            .unwrap_err();
        assert!(
            err.to_string().contains("unsupported version"),
            "malformed semver must fail clearly, not become an omission: {err}"
        );
    }

    #[test]
    fn classify_dep_routes_protocols_aliases_and_ranges() {
        let omit_reason = |spec: &str| match classify_dep("p", spec).unwrap() {
            EdgeAction::Omit(o) => o.reason,
            EdgeAction::Resolve { .. } => panic!("{spec:?} should be an omission"),
        };
        assert_eq!(omit_reason("workspace:*"), "workspace: protocol");
        // `link:../x` would misread as a git shorthand through Spec::parse — the prefix check
        // runs first.
        assert_eq!(omit_reason("link:../x"), "link: protocol");
        assert_eq!(
            omit_reason("git+ssh://git@github.com/x/y.git"),
            "git dependency"
        );
        assert_eq!(omit_reason("user/repo"), "git dependency");
        assert_eq!(omit_reason("https://x.example/p.tgz"), "remote tarball");
        assert_eq!(omit_reason("file:../local"), "local path");
        assert_eq!(
            omit_reason("npm:x@file:../y"),
            "npm: alias to a non-registry target"
        );

        match classify_dep("aliased", "npm:target@^2").unwrap() {
            EdgeAction::Resolve { name, .. } => assert_eq!(name, "target"),
            EdgeAction::Omit(o) => panic!("alias to registry must resolve, got {o}"),
        }
        match classify_dep("plain", "^1.2").unwrap() {
            EdgeAction::Resolve { name, .. } => assert_eq!(name, "plain"),
            EdgeAction::Omit(o) => panic!("registry range must resolve, got {o}"),
        }
        assert!(classify_dep("bad", "%% nope %%").is_err());
    }

    #[test]
    fn omission_display_names_the_spec_when_present() {
        assert_eq!(
            Omission::new("g", "git+ssh://x/y", "git dependency").to_string(),
            "g (git+ssh://x/y: git dependency)"
        );
        assert_eq!(
            Omission::new("workspaces", "", "not traversed").to_string(),
            "workspaces (not traversed)"
        );
    }

    #[test]
    fn prefetch_concurrency_stays_within_the_cap() {
        // A 12-name frontier (over the 8-thread cap); the fake fetch tracks its in-flight
        // high-water mark. The tiny sleep gives workers a chance to overlap — the assertion is
        // one-sided (never above the cap), so scheduling noise can't flake it.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        let leaves: Vec<String> = (1..=12).map(|i| format!("l{i:02}")).collect();
        pkgs.insert(
            "root".into(),
            packument_with(
                "1.0.0",
                &leaves
                    .iter()
                    .map(|l| (l.as_str(), "^1"))
                    .collect::<Vec<_>>(),
            ),
        );
        for leaf in &leaves {
            pkgs.insert(leaf.clone(), packument_with("1.0.0", &[]));
        }

        let in_flight = AtomicUsize::new(0);
        let high_water = AtomicUsize::new(0);
        let get = |name: &str| {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            high_water.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(2));
            in_flight.fetch_sub(1, Ordering::SeqCst);
            pkgs.get(name)
                .cloned()
                .ok_or_else(|| format!("no packument for {name}").into())
        };

        let roots = vec![("root".to_string(), "^1".parse().unwrap(), false)];
        let (resolved, _) = Registry::npm()
            .resolve_walk(&roots, get, |_| {}, NESTED_AUDIT)
            .unwrap();
        assert_eq!(resolved.len(), 13);
        assert!(
            high_water.load(Ordering::SeqCst) <= PACKUMENT_CONCURRENCY,
            "at most {PACKUMENT_CONCURRENCY} concurrent fetches, saw {}",
            high_water.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn parallel_prefetch_is_deterministic_and_fetches_each_name_once() {
        // Three levels, 12 distinct names — more than the 8-thread cap: root@1 → p01..p10 (^1
        // each), each pNN@1 → shared ^1, shared@1 → {}. The per-name fetch counter lives behind
        // a Mutex so the packument source stays `Fn + Sync` for the parallel prefetch.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        let parents: Vec<String> = (1..=10).map(|i| format!("p{i:02}")).collect();
        pkgs.insert(
            "root".into(),
            packument_with(
                "1.0.0",
                &parents
                    .iter()
                    .map(|p| (p.as_str(), "^1"))
                    .collect::<Vec<_>>(),
            ),
        );
        for p in &parents {
            pkgs.insert(p.clone(), packument_with("1.0.0", &[("shared", "^1")]));
        }
        pkgs.insert("shared".into(), packument_with("1.0.0", &[]));

        let fetches: std::sync::Mutex<std::collections::HashMap<String, usize>> =
            std::sync::Mutex::new(std::collections::HashMap::new());
        let get = |name: &str| {
            *fetches.lock().unwrap().entry(name.to_string()).or_insert(0) += 1;
            pkgs.get(name)
                .cloned()
                .ok_or_else(|| format!("no packument for {name}").into())
        };

        let roots = vec![("root".to_string(), "^1".parse().unwrap(), false)];
        let seen: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
        let begins = std::sync::atomic::AtomicUsize::new(0);
        let dones = std::sync::atomic::AtomicUsize::new(0);
        let (resolved, _) = Registry::npm()
            .resolve_walk(
                &roots,
                get,
                |event| match event {
                    // Fetch events arrive from worker threads in nondeterministic cross-name
                    // order — count them, never sequence them.
                    ResolveEvent::FetchBegin { .. } => {
                        begins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    ResolveEvent::FetchDone { .. } => {
                        dones.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    ResolveEvent::Resolved { count, .. } => seen.lock().unwrap().push(count),
                },
                NESTED_AUDIT,
            )
            .unwrap();

        // Every name fetched exactly once, despite ten same-round requirements on `shared` —
        // and every fetch bracketed by one Begin and one Done.
        let fetches = fetches.into_inner().unwrap();
        assert_eq!(fetches.len(), 12);
        assert!(
            fetches.values().all(|&n| n == 1),
            "duplicate fetches: {fetches:?}"
        );
        assert_eq!(begins.into_inner(), 12);
        assert_eq!(dones.into_inner(), 12);
        // Deterministic ticks and output: 12 resolutions in FIFO order, sorted result.
        assert_eq!(seen.into_inner().unwrap(), (1..=12).collect::<Vec<_>>());
        let names: Vec<&str> = resolved.iter().map(|r| r.name.as_str()).collect();
        let expected: Vec<String> = parents
            .iter()
            .cloned()
            .chain(["root".to_string(), "shared".to_string()])
            .collect();
        assert_eq!(
            names,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolve_walk_reports_each_resolution_to_the_observer() {
        // a@1 → {b ^1, c ^1}; b@1 → {c ^1} — c's second requirement dedupes and must NOT tick.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert(
            "a".into(),
            packument_with("1.0.0", &[("b", "^1"), ("c", "^1")]),
        );
        pkgs.insert("b".into(), packument_with("1.2.0", &[("c", "^1")]));
        pkgs.insert("c".into(), packument_with("1.5.0", &[]));

        let roots = vec![("a".to_string(), "^1".parse().unwrap(), false)];
        let seen: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let (resolved, _) = Registry::npm()
            .resolve_walk(
                &roots,
                |name| {
                    pkgs.get(name)
                        .cloned()
                        .ok_or_else(|| format!("no packument for {name}").into())
                },
                |event| {
                    if let ResolveEvent::Resolved { count, package } = event {
                        seen.lock()
                            .unwrap()
                            .push(format!("{count} {}@{}", package.name, package.version));
                    }
                },
                NESTED_AUDIT,
            )
            .unwrap();

        // One event per resolution with the running total and the package, none for the deduped
        // requeue.
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen, ["1 a@1.0.0", "2 b@1.2.0", "3 c@1.5.0"]);
        assert_eq!(seen.len(), resolved.len());
    }

    #[test]
    fn fetch_events_bracket_each_fetch_once_including_failures() {
        // `ok` resolves; `bad` is an *optional* root whose fetch fails → an omission under the
        // audit policy. Every attempted name gets exactly one Begin and one Done — the failed
        // fetch included ("no longer in flight") — and Resolved counts stay the walk's 1..=n.
        let mut pkgs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        pkgs.insert("ok".into(), packument_with("1.0.0", &[]));

        let events: std::sync::Mutex<std::collections::HashMap<String, (usize, usize)>> =
            std::sync::Mutex::new(std::collections::HashMap::new());
        let counts: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
        let roots = vec![
            ("ok".to_string(), "^1".parse().unwrap(), false),
            ("bad".to_string(), "^1".parse().unwrap(), true),
        ];
        let (resolved, omissions) = Registry::npm()
            .resolve_walk(
                &roots,
                |name| {
                    pkgs.get(name)
                        .cloned()
                        .ok_or_else(|| format!("no packument for {name}").into())
                },
                |event| match event {
                    ResolveEvent::FetchBegin { name } => {
                        events
                            .lock()
                            .unwrap()
                            .entry(name.to_string())
                            .or_default()
                            .0 += 1;
                    }
                    ResolveEvent::FetchDone { name } => {
                        events
                            .lock()
                            .unwrap()
                            .entry(name.to_string())
                            .or_default()
                            .1 += 1;
                    }
                    ResolveEvent::Resolved { count, .. } => counts.lock().unwrap().push(count),
                },
                NESTED_AUDIT,
            )
            .unwrap();

        let events = events.into_inner().unwrap();
        assert_eq!(events.get("ok"), Some(&(1, 1)));
        assert_eq!(
            events.get("bad"),
            Some(&(1, 1)),
            "a failed fetch still closes"
        );
        assert_eq!(counts.into_inner().unwrap(), [1]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(omissions.len(), 1);
        assert!(
            omissions[0]
                .reason
                .contains("optional dependency failed to fetch"),
            "{}",
            omissions[0]
        );
    }

    #[test]
    fn parse_search_extracts_packages_and_skips_malformed() {
        let doc = json!({
            "objects": [
                { "package": {
                    "name": "lodash",
                    "version": "4.17.21",
                    "description": "Lodash modular utilities.",
                    "keywords": ["util", "functional"],
                    "date": "2021-02-20T15:42:16.891Z",
                    "links": { "npm": "https://www.npmjs.com/package/lodash", "homepage": "https://lodash.com/" }
                }},
                { "package": { "version": "1.0.0" } }, // no name → skipped
                { "score": {} }                         // no package → skipped
            ],
            "total": 1
        });
        let results = parse_search(&doc);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.name, "lodash");
        assert_eq!(r.version, "4.17.21");
        assert_eq!(r.description.as_deref(), Some("Lodash modular utilities."));
        assert_eq!(r.keywords, ["util", "functional"]);
        assert_eq!(r.homepage.as_deref(), Some("https://lodash.com/"));
        assert!(r.date.is_some());
    }

    #[test]
    fn parse_search_empty_when_no_objects() {
        assert!(parse_search(&json!({})).is_empty());
        assert!(parse_search(&json!({ "objects": "nope" })).is_empty());
    }

    #[test]
    fn encode_query_escapes_reserved_characters() {
        assert_eq!(encode_query("lodash"), "lodash");
        assert_eq!(encode_query("react dom"), "react%20dom");
        assert_eq!(encode_query("@lit/context"), "%40lit%2Fcontext");
    }
}
