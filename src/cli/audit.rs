//! `audit` — check packages against vulnerability advisories from multiple sources.
//!
//! Mirrors `npm audit`: prints a report and exits `1` when an advisory at or above the
//! `--audit-level` threshold is found. The positional source names what to audit — a project
//! directory (its `package-lock.json`, else its `package.json`), an explicit manifest or lockfile
//! path, or a registry spec `name=range` (e.g. `lit=^3`). Manifest and spec sources resolve their
//! production tree against the registry **in memory**; `audit` never writes a lockfile. A
//! missing/garbage source is a real error (nonzero with a message). One unreachable advisory
//! source degrades and continues; if *every* selected source fails the audit is incomplete and
//! exits `2` (fail closed) unless `--allow-incomplete` is given.

use std::io::Write as _;

use clap::ValueEnum;
use serde_json::Value;

use super::source::{classify_file, BareToken, FileKind, Source};
use super::Res;
use crate::audit::npm::NpmRegistrySource;
use crate::audit::osv::OsvSource;
use crate::audit::{self, AdvisorySource, Severity};
use crate::package_json::lock::{registry_roots, Lockfile};
use crate::package_json::manifest;
use crate::registry::Registry;
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
/// `audit_level` is found (after printing), `0` when clean.
pub(super) fn run(
    source: &str,
    audit_level: AuditLevel,
    format: Format,
    sources: Option<&[SourceKind]>,
    registry: Option<&str>,
    allow_incomplete: bool,
) -> Res {
    let src = Source::parse(source, BareToken::Path)?;
    let reg = Registry::with_base_url(registry.unwrap_or(DEFAULT_REGISTRY));
    let components = load_components(&src, &reg)?;

    let active = build_sources(sources, registry);
    let report = audit::run_audit(&components, &active);

    let out = match format {
        Format::Summary => audit::render_summary(&report),
        Format::Json => audit::render_json(&report),
    };
    print!("{out}");

    // A finding at/above the threshold, and a fully-incomplete audit, are both nonzero *results*
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
    Ok(())
}

/// The packages a source names, as audit components — always assembled in memory (`audit` never
/// writes a lockfile or anything else to disk).
///
/// A lockfile audits exactly what it pins (dev deps included when the lock records them); a
/// manifest or `name=range` spec resolves its **production** tree against `registry` first —
/// npm-style nested, so requirements that disagree keep and audit *every* resolved version —
/// while non-registry deps (git / `file:`) are skipped, as in the lockfile writer.
fn load_components(source: &Source, registry: &Registry) -> Res<Vec<Component>> {
    match source {
        Source::Dir(dir) => {
            let lock = dir.join("package-lock.json");
            if lock.exists() {
                return Ok(sbom::components(&Lockfile::parse(&read_text(&lock)?)?));
            }
            if dir.join("package.json").exists() {
                return components_from_manifest(&crate::project::read_manifest(dir)?, registry);
            }
            Err(format!("no package-lock.json or package.json in {}", dir.display()).into())
        }
        Source::File(path) => {
            let text = read_text(path)?;
            match classify_file(path, &text) {
                FileKind::Lockfile => Ok(sbom::components(&Lockfile::parse(&text)?)),
                FileKind::Manifest => {
                    let doc: Value = serde_json::from_str(&text)
                        .map_err(|e| format!("parsing {}: {e}", path.display()))?;
                    components_from_manifest(&doc, registry)
                }
            }
        }
        // A spec is a one-dependency virtual manifest, so `lit=^3` audits the full transitive
        // production tree — "what would this package pull into my app?".
        Source::Spec { name, range } => {
            let mut doc = manifest::scaffold("npm-utils-audit", "0.0.0");
            manifest::upsert_dependency(&mut doc, name, range.as_deref().unwrap_or("*"));
            components_from_manifest(&doc, registry)
        }
    }
}

/// Resolve a manifest's production tree in memory and adapt it to components. The resolution is
/// npm-style **nested** ([`Registry::resolve_tree_nested`]): where requirements disagree, every
/// resolved version is kept and audited — an audit flags issues rather than installs, and when
/// the tree would contain two versions of a package, both versions' advisories matter. Nothing
/// touches the filesystem.
fn components_from_manifest(doc: &Value, registry: &Registry) -> Res<Vec<Component>> {
    let resolved = registry.resolve_tree_nested(&registry_roots(doc)?)?;
    Ok(sbom::components_from_resolved(&resolved))
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
