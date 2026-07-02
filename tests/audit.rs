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

    fn audit(dir: &std::path::Path, extra: &[&str]) -> std::process::Output {
        let mut args = vec!["audit", dir.to_str().unwrap()];
        args.extend_from_slice(extra);
        Command::new(env!("CARGO_BIN_EXE_npm-utils"))
            .args(args)
            .output()
            .expect("spawn npm-utils audit")
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

    /// A missing lockfile is a real error: nonzero exit with an `npm-utils:` message on stderr.
    #[test]
    fn missing_lockfile_errors() {
        let dir = tempfile::tempdir().unwrap();
        let out = audit(dir.path(), &[]);
        assert_ne!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stderr).contains("npm-utils:"));
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
