//! Audit parsing / dedup / range-matching over recorded fixtures (offline, deterministic), an
//! in-process orchestrator test via a fake source, and `cli`-gated end-to-end exit-code tests.
//!
//! The `*.json` fixtures under `tests/fixtures/` are frozen real responses (npm's bulk endpoint and
//! an OSV record), so these tests assert on a snapshot and never touch the network. The live audit
//! is `#[ignore]`d behind the `cli` feature.

use npm_utils::audit::npm::parse_npm_bulk;
use npm_utils::audit::osv::parse_osv_vuln;
use npm_utils::audit::{self, Advisory, AdvisorySource, Severity};
use npm_utils::sbom::Component;

fn component(name: &str, version: &str) -> Component {
    Component {
        name: name.into(),
        version: version.into(),
        purl: format!("pkg:npm/{name}@{version}"),
        license: None,
        resolved: None,
        integrity: None,
    }
}

#[test]
fn parses_real_npm_bulk_fixture() {
    let body: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/npm-bulk-lodash.json")).unwrap();
    let advisories = parse_npm_bulk(&body);
    assert!(!advisories.is_empty());

    let a = advisories
        .iter()
        .find(|a| a.id == "GHSA-35jh-r3h4-6jhm")
        .expect("GHSA recovered from the advisory url");
    assert_eq!(a.source, "npm");
    assert_eq!(a.package, "lodash");
    assert_eq!(a.severity, Some(Severity::High));
    assert_eq!(a.vulnerable_range, "<4.17.21");
    assert!(a.aliases.iter().any(|x| x == "GHSA-35jh-r3h4-6jhm"));
    assert!(a.cwe.iter().any(|c| c == "CWE-77"));
    assert!(a.cvss_score.is_some());
}

#[test]
fn parses_real_osv_fixture_and_synthesizes_range() {
    let record: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/osv-GHSA-jf85-cpcp-j695.json")).unwrap();

    let adv =
        parse_osv_vuln(&record, "lodash", "4.17.11").expect("npm/lodash is an affected package");
    assert_eq!(adv.source, "osv");
    assert_eq!(adv.id, "GHSA-jf85-cpcp-j695");
    assert!(adv.aliases.iter().any(|a| a == "CVE-2019-10744"));
    assert_eq!(adv.severity, Some(Severity::Critical)); // database_specific.severity == "CRITICAL"
    assert_eq!(adv.vulnerable_range, "<4.17.12"); // events [introduced:0, fixed:4.17.12]

    // The record also lists a RubyGems package and other npm packages — neither matches "lodash".
    assert!(parse_osv_vuln(&record, "lodash-rails", "1.0.0").is_none());
    assert!(parse_osv_vuln(&record, "not-a-package", "1.0.0").is_none());
}

#[test]
fn build_report_filters_by_installed_version_against_fixture() {
    let body: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/npm-bulk-lodash.json")).unwrap();
    let advisories = audit::dedup_advisories(parse_npm_bulk(&body));

    // 4.17.20 is below the <4.17.21 ceiling → that advisory applies.
    let old = audit::build_report(&advisories, &[component("lodash", "4.17.20")]);
    assert!(old.vulnerabilities.total > 0);
    assert!(old.exceeds(Severity::High));
    assert!(!old.exceeds(Severity::Critical)); // npm rates lodash issues high/moderate, not critical
    assert!(old
        .findings
        .iter()
        .flat_map(|f| f.advisories.iter())
        .any(|a| a.id == "GHSA-35jh-r3h4-6jhm"));

    // 4.17.21 clears the <4.17.21 advisory (others with higher ceilings may remain).
    let newer = audit::build_report(&advisories, &[component("lodash", "4.17.21")]);
    assert!(newer
        .findings
        .iter()
        .flat_map(|f| f.advisories.iter())
        .all(|a| a.id != "GHSA-35jh-r3h4-6jhm"));

    // A package the fixture doesn't mention is clean.
    let clean = audit::build_report(&advisories, &[component("left-pad", "1.3.0")]);
    assert_eq!(clean.vulnerabilities.total, 0);
    assert!(clean.findings.is_empty());
}

/// A test double standing in for a network source — proves `run_audit` works against the trait with
/// no IO, and that an empty source yields a clean report.
struct FakeSource(Vec<Advisory>);

impl AdvisorySource for FakeSource {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn query(&self, _components: &[Component]) -> npm_utils::Result<Vec<Advisory>> {
        Ok(self.0.clone())
    }
}

#[test]
fn run_audit_groups_counts_and_sets_matched_version() {
    let advisory = Advisory {
        source: "fake",
        id: "GHSA-zzzz".into(),
        aliases: vec!["GHSA-zzzz".into()],
        package: "lodash".into(),
        vulnerable_range: "<5.0.0".into(),
        severity: Some(Severity::Moderate),
        title: "synthetic".into(),
        url: None,
        cwe: vec![],
        cvss_score: None,
        cvss_vector: None,
        matched_version: String::new(),
    };
    let sources: Vec<Box<dyn AdvisorySource>> = vec![Box::new(FakeSource(vec![advisory]))];
    let report = audit::run_audit(&[component("lodash", "4.17.20")], &sources);
    assert_eq!(report.vulnerabilities.total, 1);
    assert_eq!(report.vulnerabilities.moderate, 1);
    assert_eq!(report.findings[0].advisories[0].matched_version, "4.17.20");

    // An empty source is a clean run.
    let empty: Vec<Box<dyn AdvisorySource>> = vec![Box::new(FakeSource(vec![]))];
    let clean = audit::run_audit(&[component("lodash", "4.17.20")], &empty);
    assert_eq!(clean.vulnerabilities.total, 0);
    assert_eq!(audit::render_summary(&clean), "found 0 vulnerabilities\n");
}

/// A source that always fails — proves `run_audit` records the failure and marks the report
/// incomplete rather than presenting an empty result as clean.
struct FailingSource;

impl AdvisorySource for FailingSource {
    fn name(&self) -> &'static str {
        "failing"
    }
    fn query(&self, _components: &[Component]) -> npm_utils::Result<Vec<Advisory>> {
        Err("simulated endpoint failure".into())
    }
}

#[test]
fn run_audit_records_failed_sources_and_marks_incomplete() {
    let sources: Vec<Box<dyn AdvisorySource>> = vec![Box::new(FailingSource)];
    let report = audit::run_audit(&[component("lodash", "4.17.20")], &sources);

    assert!(!report.is_complete());
    assert_eq!(report.failed_sources, vec!["failing".to_string()]);
    assert_eq!(report.vulnerabilities.total, 0);

    // The summary flags incompleteness instead of a bare "found 0 vulnerabilities".
    let summary = audit::render_summary(&report);
    assert!(
        summary.contains("INCOMPLETE"),
        "summary flags incompleteness: {summary}"
    );

    // JSON carries machine-readable flags a caller can gate on.
    let doc: serde_json::Value = serde_json::from_str(&audit::render_json(&report)).unwrap();
    assert_eq!(doc["incomplete"], true);
    assert_eq!(doc["failed_sources"], serde_json::json!(["failing"]));
}

#[test]
fn unknown_severity_confirmed_finding_trips_the_threshold() {
    // A confirmed finding whose severity could not be determined must stay actionable: it trips
    // even the default `low` threshold rather than passing silently, and renders as UNKNOWN.
    let advisory = Advisory {
        source: "osv",
        id: "GHSA-unknown".into(),
        aliases: vec![],
        package: "demo".into(),
        vulnerable_range: "=1.2.3".into(),
        severity: None,
        title: "unknown severity".into(),
        url: None,
        cwe: vec![],
        cvss_score: None,
        cvss_vector: None,
        matched_version: String::new(),
    };
    let report = audit::build_report(&[advisory], &[component("demo", "1.2.3")]);

    assert_eq!(report.vulnerabilities.total, 1);
    assert!(
        report.exceeds(Severity::Low),
        "an unknown-severity confirmed finding trips --audit-level low"
    );
    assert!(
        report.exceeds(Severity::Critical),
        "and any higher level, conservatively"
    );
    assert!(report.is_complete(), "no sources failed");
    assert!(audit::render_summary(&report).contains("UNKNOWN"));
}

// ----- CLI end-to-end (the `cli` bin and exit codes) -----------------------------------------

#[cfg(feature = "cli")]
mod cli {
    use std::process::Command;

    fn write_lock(dir: &std::path::Path, packages_json: &str) {
        let lock = format!(
            r#"{{ "name": "demo", "version": "1.0.0", "lockfileVersion": 3, "packages": {packages_json} }}"#
        );
        std::fs::write(dir.join("package-lock.json"), lock).unwrap();
    }

    /// Run `npm-utils audit <source> <extra…>`; `source` is any dir path, file path, or spec token.
    fn audit(source: impl AsRef<std::ffi::OsStr>, extra: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_npm-utils"))
            .arg("audit")
            .arg(source.as_ref())
            .args(extra)
            .output()
            .expect("spawn npm-utils audit")
    }

    fn write_manifest(dir: &std::path::Path, dependencies_json: &str) {
        let manifest = format!(
            r#"{{ "name": "demo", "version": "1.0.0", "dependencies": {dependencies_json} }}"#
        );
        std::fs::write(dir.join("package.json"), manifest).unwrap();
    }

    /// An empty tree makes no network calls (the sources short-circuit on empty input), so this is
    /// a deterministic, offline check of the clean / exit-0 path.
    #[test]
    fn empty_tree_is_clean_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(
            dir.path(),
            r#"{ "": { "name": "demo", "version": "1.0.0" } }"#,
        );
        let out = audit(dir.path(), &[]);
        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stdout).contains("found 0 vulnerabilities"));
    }

    /// A directory with neither a lockfile nor a manifest is a real error: nonzero exit with an
    /// `npm-utils:` message naming both candidates.
    #[test]
    fn missing_lockfile_and_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let out = audit(dir.path(), &[]);
        assert_ne!(out.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("npm-utils:"), "{stderr}");
        assert!(
            stderr.contains("no package-lock.json or package.json"),
            "{stderr}"
        );
    }

    /// The regression for `audit <path>/package.json` (formerly ENOTDIR): an explicit manifest path
    /// audits that manifest, resolved in memory — a zero-dep manifest needs no network — and never
    /// writes a lockfile.
    #[test]
    fn explicit_manifest_path_audits_in_memory_without_writing_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "{}");
        let out = audit(dir.path().join("package.json"), &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("found 0 vulnerabilities"));
        assert!(
            !dir.path().join("package-lock.json").exists(),
            "a manifest audit must not write a lockfile"
        );
    }

    /// An explicit lockfile path is used as given.
    #[test]
    fn explicit_lockfile_path_is_used_as_given() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(
            dir.path(),
            r#"{ "": { "name": "demo", "version": "1.0.0" } }"#,
        );
        let out = audit(dir.path().join("package-lock.json"), &[]);
        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stdout).contains("found 0 vulnerabilities"));
    }

    /// A lock under a nonstandard name is still recognized — by its `lockfileVersion` content.
    #[test]
    fn renamed_lockfile_is_classified_by_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mylock.json"),
            r#"{ "name": "demo", "version": "1.0.0", "lockfileVersion": 3, "packages": { "": { "name": "demo", "version": "1.0.0" } } }"#,
        )
        .unwrap();
        let out = audit(dir.path().join("mylock.json"), &[]);
        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stdout).contains("found 0 vulnerabilities"));
    }

    /// Given a directory holding both files, the lockfile wins. The manifest names a package that
    /// can never resolve, so a clean exit is only explainable by the (empty) lock — online or off.
    #[test]
    fn directory_prefers_the_lockfile_over_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(
            dir.path(),
            r#"{ "": { "name": "demo", "version": "1.0.0" } }"#,
        );
        write_manifest(
            dir.path(),
            r#"{ "this-package-does-not-exist-npm-utils-e2e": "^1" }"#,
        );
        let out = audit(dir.path(), &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "the lock, not the manifest, decides: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A directory with only a manifest falls back to resolving it in memory.
    #[test]
    fn directory_falls_back_to_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "{}");
        let out = audit(dir.path(), &[]);
        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stdout).contains("found 0 vulnerabilities"));
        assert!(!dir.path().join("package-lock.json").exists());
    }

    /// When every selected source fails, the audit is incomplete: it exits `2` and marks the output
    /// incomplete rather than reporting a clean tree. Uses an unreachable registry (a refused
    /// localhost port), so it needs no external network.
    #[test]
    fn all_sources_failed_is_incomplete_and_exits_two() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(
            dir.path(),
            r#"{
                "": { "name": "demo", "version": "1.0.0" },
                "node_modules/lodash": {
                    "version": "4.17.20",
                    "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz"
                }
            }"#,
        );

        // The sole source (npm) points at an unreachable registry, so it fails.
        let out = audit(
            dir.path(),
            &["--sources", "npm", "--registry", "https://localhost:1"],
        );
        assert_eq!(out.status.code(), Some(2), "all sources failed → exit 2");
        assert!(String::from_utf8_lossy(&out.stdout).contains("INCOMPLETE"));

        // --allow-incomplete opts back into fail-open (exit 0).
        let out = audit(
            dir.path(),
            &[
                "--sources",
                "npm",
                "--registry",
                "https://localhost:1",
                "--allow-incomplete",
            ],
        );
        assert_eq!(out.status.code(), Some(0), "--allow-incomplete → exit 0");
    }

    /// Status lines land on stderr by default: the resolution phase for a manifest source and one
    /// begin/done pair per advisory source (emitted even for an empty component set). Offline —
    /// a zero-dep manifest resolves without network and empty components short-circuit the
    /// sources' queries.
    #[test]
    fn status_lines_appear_on_stderr_by_default() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "{}");
        let out = audit(dir.path().join("package.json"), &[]);
        assert_eq!(out.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("resolving dependency tree"), "{stderr}");
        assert!(stderr.contains("querying npm advisories"), "{stderr}");
        assert!(stderr.contains("querying osv advisories"), "{stderr}");
        // The report itself stays on stdout, unchanged.
        assert!(String::from_utf8_lossy(&out.stdout).contains("found 0 vulnerabilities"));
    }

    /// `-q`/`--quiet` silences the status lines — and only them.
    #[test]
    fn quiet_suppresses_status_lines() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "{}");
        let out = audit(dir.path().join("package.json"), &["-q"]);
        assert_eq!(out.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("resolving"), "{stderr}");
        assert!(!stderr.contains("querying"), "{stderr}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("found 0 vulnerabilities"));
    }

    /// `--quiet` never suppresses real errors: the `npm-utils:` line still reaches stderr.
    #[test]
    fn quiet_never_suppresses_errors() {
        let dir = tempfile::tempdir().unwrap();
        let out = audit(dir.path(), &["--quiet"]);
        assert_ne!(out.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("npm-utils:"), "{stderr}");
        assert!(
            stderr.contains("no package-lock.json or package.json"),
            "{stderr}"
        );
    }

    /// The resolution status line names the effective `--registry`, and a resolution failure
    /// arrives after the begin line, on its own line. Offline: a refused localhost port.
    #[test]
    fn resolution_line_names_the_effective_registry() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), r#"{ "lodash": "^4" }"#);
        let out = audit(
            dir.path().join("package.json"),
            &["--registry", "https://localhost:1"],
        );
        assert_ne!(out.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("resolving dependency tree from localhost:1"),
            "{stderr}"
        );
        assert!(stderr.contains("npm-utils:"), "{stderr}");
    }

    /// A manifest whose parents disagree on a package (a direct `glob` 5 pin vs globby@0.1.1's
    /// frozen `glob ^4.0.2`) resolves nested like npm and audits every version — the flat
    /// installer's "version conflict" error must not reach the audit.
    #[test]
    #[ignore = "network: resolves via the npm registry and hits the advisory endpoints"]
    fn live_audit_manifest_with_conflicting_requirements_still_audits() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), r#"{ "glob": "5.0.15", "globby": "0.1.1" }"#);
        let out = audit(dir.path().join("package.json"), &[]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("version conflict"),
            "nested resolution tolerates the glob 4/5 split: {stderr}"
        );
        // Reaching a report at all (clean, findings, or incomplete) proves resolution succeeded.
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("found "),
            "a report was printed; stderr: {stderr}"
        );
    }

    /// A `name=range` source resolves the full production tree via the registry and audits it —
    /// an exact vulnerable pin keeps the outcome deterministic.
    #[test]
    #[ignore = "network: resolves via the npm registry and hits the advisory endpoints"]
    fn live_audit_spec_source_flags_a_vulnerable_pin() {
        let out = audit("lodash=4.17.11", &[]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "vulns at default level → exit 1"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("lodash@4.17.11"), "{stdout}");
        assert!(stdout.contains("GHSA-"), "{stdout}");
    }

    #[test]
    #[ignore = "network: hits the npm advisory + OSV endpoints"]
    fn live_audit_flags_a_known_vulnerable_package() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(
            dir.path(),
            r#"{
                "": { "name": "demo", "version": "1.0.0" },
                "node_modules/lodash": {
                    "version": "4.17.11",
                    "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.11.tgz"
                }
            }"#,
        );

        // Default level (low) → a known-vulnerable lodash makes the command exit 1.
        let out = audit(dir.path(), &[]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "vulns at default level → exit 1"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("GHSA-"),
            "report names an advisory:\n{stdout}"
        );
        assert!(stdout.contains("lodash@4.17.11"));

        // --format json stays valid and carries the metadata count block.
        let json_out = audit(dir.path(), &["--format", "json"]);
        let doc: serde_json::Value =
            serde_json::from_slice(&json_out.stdout).expect("audit --json emits valid JSON");
        assert!(
            doc["metadata"]["vulnerabilities"]["total"]
                .as_u64()
                .unwrap()
                > 0
        );
    }
}
