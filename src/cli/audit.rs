//! `audit` — check packages against vulnerability advisories from multiple sources.
//!
//! Mirrors `npm audit`: prints a report and exits `1` when an advisory at or above the
//! `--audit-level` threshold is found. The positional source names what to audit — a project
//! directory (its `package-lock.json`, else its `package.json`), an explicit manifest or lockfile
//! path, or a registry spec `name=range` (e.g. `lit=^3`). The files a directory source discovers
//! may be symlinks as long as they resolve inside that directory ([`contained_file`] is the
//! guard); an explicit file path is used as given. Manifest and spec sources resolve their
//! registry-reachable `dependencies` + `optionalDependencies` tree **in memory**; `audit` never
//! writes a lockfile. Whatever that resolution cannot follow — git / path / tarball /
//! `workspace:` specs, workspace packages, optional deps that fail to resolve — is reported as
//! an **omission** (devDependencies are never resolved from a manifest; audit the lockfile to
//! include them). A missing/garbage source is a real error (nonzero with a message). One
//! unreachable advisory source degrades and continues. The audit fails closed with exit `2`
//! when it is incomplete — every selected source failed, or any dependency went unaudited —
//! unless `--allow-incomplete` is given.

use std::io::Write as _;

use clap::ValueEnum;
use serde_json::Value;

use super::common::{host_of, ResolveTicker};
use super::progress::{Progress, TaskStatus};
use super::source::{classify_file, contained_file, BareToken, FileKind, Source};
use super::Res;
use crate::audit::npm::NpmRegistrySource;
use crate::audit::osv::OsvSource;
use crate::audit::{self, AdvisorySource, Severity, SourceEvent};
use crate::package_json::lock::{audit_roots, Lockfile};
use crate::package_json::manifest;
use crate::registry::{Omission, Registry};
use crate::sbom::{self, Component};

const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// Minimum severity that makes `audit` exit non-zero.
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum AuditLevel {
    Low,
    Moderate,
    High,
    Critical,
}

impl From<AuditLevel> for Severity {
    fn from(level: AuditLevel) -> Severity {
        match level {
            AuditLevel::Low => Severity::Low,
            AuditLevel::Moderate => Severity::Moderate,
            AuditLevel::High => Severity::High,
            AuditLevel::Critical => Severity::Critical,
        }
    }
}

/// Output format for the `audit` verb.
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Format {
    /// Human-readable summary (the default)
    Summary,
    /// `npm audit --json`-shaped JSON
    Json,
}

/// An advisory source selectable via `--sources`.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum SourceKind {
    Npm,
    Osv,
}

/// Load the source's packages (a lockfile as pinned; a manifest or spec resolved in memory), query
/// the selected advisory sources, and print the report. Exits `1` when an advisory at or above
/// `audit_level` is found (after printing), `2` when the audit is incomplete — every source
/// failed, or some dependencies could not be audited — unless `--allow-incomplete`, and `0` when
/// clean. Progress — a `[resolve]` task for manifest/spec sources and one task per advisory
/// source, emitted even for an empty component set — goes to stderr per the global
/// `--progress` mode; `--progress=none` (or `-q`) silences it, never the report or errors.
pub(super) fn run(
    source: &str,
    audit_level: AuditLevel,
    format: Format,
    sources: Option<&[SourceKind]>,
    registry: Option<&str>,
    allow_incomplete: bool,
    progress: &Progress,
) -> Res {
    let src = Source::parse(source, BareToken::Path)?;
    let reg = Registry::with_base_url(registry.unwrap_or(DEFAULT_REGISTRY));
    let (components, omissions) = load_components(&src, &reg, progress)?;

    let active = build_sources(sources, registry);
    // Render each source's Begin/Done/Failed as a task; every Begin is followed by exactly one
    // Done/Failed, so `task` is None again when run_audit_observed returns — nothing stays open
    // past this call (the exits below run no destructors).
    let mut task: Option<TaskStatus> = None;
    let mut report = audit::run_audit_observed(&components, &active, |event| match event {
        SourceEvent::Begin { name } => {
            task = Some(progress.task(name, format!("querying {name} advisories")));
        }
        SourceEvent::Done { advisories } => {
            if let Some(t) = task.take() {
                let plural = if advisories == 1 { "y" } else { "ies" };
                t.finish(&format!("{advisories} advisor{plural}"));
            }
        }
        // Failed fires before the library's warn line, so the task closes first.
        SourceEvent::Failed => {
            if let Some(t) = task.take() {
                t.fail("failed");
            }
        }
    });

    report.omissions = omissions;
    let out = match format {
        Format::Summary => audit::render_summary(&report),
        Format::Json => audit::render_json(&report),
    };
    print!("{out}");

    // A finding at/above the threshold, and an incomplete audit, are both nonzero *results*
    // (not tool errors): print to stdout and exit directly, bypassing `main_with`'s
    // `npm-utils: <err>` path. Flush first — `process::exit` runs no destructors.
    let all_sources_failed = !active.is_empty() && report.failed_sources.len() == active.len();
    if all_sources_failed && !allow_incomplete {
        // Every advisory source failed: the audit did not actually run, so fail closed with a
        // distinct exit code rather than let a caller mistake it for clean (override with
        // --allow-incomplete).
        let _ = std::io::stdout().flush();
        std::process::exit(2);
    }
    if report.exceeds(audit_level.into()) {
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }
    if !report.omissions.is_empty() && !allow_incomplete {
        // Unaudited dependencies: otherwise clean, but incomplete — fail closed like the
        // all-sources-failed path (a real finding's exit 1 takes precedence above; override
        // with --allow-incomplete).
        let _ = std::io::stdout().flush();
        std::process::exit(2);
    }
    Ok(())
}

/// The packages a source names, as audit components plus the [`Omission`]s the loading could
/// not cover — always assembled in memory (`audit` never writes a lockfile or anything else to
/// disk).
///
/// A lockfile audits exactly what it pins (dev deps included when the lock records them; no
/// omissions); a manifest or `name=range` spec resolves its registry-reachable `dependencies` +
/// `optionalDependencies` tree against `registry` — npm-style nested, so requirements that
/// disagree keep and audit *every* resolved version — and reports everything else (non-registry
/// specs, workspaces, failed optional deps) as omissions. devDependencies are not resolved from
/// a manifest.
fn load_components(
    source: &Source,
    registry: &Registry,
    progress: &Progress,
) -> Res<(Vec<Component>, Vec<Omission>)> {
    match source {
        // The files *discovered* inside a directory source may be symlinks, but must resolve
        // inside that directory (containment on canonicalized paths — the path-traversal
        // guard); an explicit `Source::File` path is the user's own choice, used as given.
        Source::Dir(dir) => {
            if let Some(lock) = contained_file(dir, "package-lock.json")? {
                let components = sbom::components(&Lockfile::parse(&read_text(&lock)?)?);
                return Ok((components, Vec::new()));
            }
            if contained_file(dir, "package.json")?.is_some() {
                return components_from_manifest(
                    &crate::project::read_manifest(dir)?,
                    registry,
                    progress,
                );
            }
            Err(format!("no package-lock.json or package.json in {}", dir.display()).into())
        }
        Source::File(path) => {
            let text = read_text(path)?;
            match classify_file(path, &text) {
                FileKind::Lockfile => Ok((sbom::components(&Lockfile::parse(&text)?), Vec::new())),
                FileKind::Manifest => {
                    let doc: Value = serde_json::from_str(&text)
                        .map_err(|e| format!("parsing {}: {e}", path.display()))?;
                    components_from_manifest(&doc, registry, progress)
                }
            }
        }
        // A spec is a one-dependency virtual manifest, so `lit=^3` audits the package's full
        // transitive dependency tree — "what would this package pull into my app?".
        Source::Spec { name, range } => {
            let mut doc = manifest::scaffold("npm-utils-audit", "0.0.0");
            manifest::upsert_dependency(&mut doc, name, range.as_deref().unwrap_or("*"));
            components_from_manifest(&doc, registry, progress)
        }
    }
}

/// Resolve a manifest's registry-reachable dependency tree in memory and adapt it to
/// components, with the entries the audit cannot cover as omissions (manifest roots first, then
/// the walk's, each set deterministically ordered). The resolution is npm-style **nested**
/// ([`Registry::resolve_tree_nested`]): where requirements disagree, every resolved version is
/// kept and audited — an audit flags issues rather than installs, and when the tree would
/// contain two versions of a package, both versions' advisories matter. Nothing touches the
/// filesystem.
fn components_from_manifest(
    doc: &Value,
    registry: &Registry,
    progress: &Progress,
) -> Res<(Vec<Component>, Vec<Omission>)> {
    // Roots first: a garbage manifest errors before any status line begins.
    let (roots, mut omissions) = audit_roots(doc)?;
    let task = progress.task(
        "resolve",
        format!(
            "resolving dependency tree from {}",
            host_of(&registry.base_url)
        ),
    );
    let ticker = ResolveTicker::new(&task);
    let (resolved, walk_omissions) =
        registry.resolve_tree_nested_observed(&roots, |event| ticker.observe(event))?;
    task.finish(&format!("{} packages", resolved.len()));
    omissions.extend(walk_omissions);
    Ok((sbom::components_from_resolved(&resolved), omissions))
}

fn read_text(path: &std::path::Path) -> Res<String> {
    std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()).into())
}

/// Assemble the active advisory sources: the subset named by `--sources`, or npm + OSV by default.
///
/// This is the single seam for adding sources. A future, optional, API-key'd source slots in here
/// without touching the [`AdvisorySource`] trait or the orchestrator — e.g. a feature-gated Snyk
/// source (`snyk = []` in `Cargo.toml`, `src/audit/snyk.rs` behind `#[cfg(feature = "snyk")]`):
///
/// ```ignore
/// #[cfg(feature = "snyk")]
/// if let Some(token) = std::env::var("SNYK_TOKEN").ok() {
///     active.push(Box::new(crate::audit::snyk::SnykSource::new(token)));
/// }
/// ```
fn build_sources(
    sources: Option<&[SourceKind]>,
    registry: Option<&str>,
) -> Vec<Box<dyn AdvisorySource>> {
    let registry = registry.unwrap_or(DEFAULT_REGISTRY);
    let selected: &[SourceKind] = match sources {
        Some(s) if !s.is_empty() => s,
        _ => &[SourceKind::Npm, SourceKind::Osv],
    };
    selected
        .iter()
        .map(|kind| -> Box<dyn AdvisorySource> {
            match kind {
                SourceKind::Npm => Box::new(NpmRegistrySource::new(registry)),
                SourceKind::Osv => Box::new(OsvSource),
            }
        })
        .collect()
}
