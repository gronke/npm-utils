//! The OSV (osv.dev) advisory source.
//!
//! A batched `POST /v1/querybatch` returns vulnerability ids per query (positionally, one result
//! list per queried component); each id is then hydrated with `GET /v1/vulns/{id}` for the full
//! record — structured `affected` ranges, aliases (CVE/GHSA), and severity. The endpoint rejects
//! more than [`QUERYBATCH_LIMIT`] queries per request (`400 "Too many queries."`), so larger
//! component sets are sent as pages. OSV records span many packages and ecosystems, so a record
//! is only relevant when one of its `affected` entries is the npm package we asked about; that
//! entry's SEMVER `events` are turned into a `>=`/`<` range string the shared
//! [`Range`](crate::package_json::spec::Range) matcher can post-filter.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::{Advisory, AdvisorySource, Severity};
use crate::download;
use crate::package_json::spec::Range;
use crate::sbom::Component;

const QUERYBATCH_URL: &str = "https://api.osv.dev/v1/querybatch";
const VULN_URL_BASE: &str = "https://api.osv.dev/v1/vulns";

/// The most queries OSV accepts per `querybatch` request — one more and the endpoint answers
/// `400 {"code":3,"message":"Too many queries."}` (verified empirically), which the audit would
/// misreport as an unreachable source. Bigger component sets are paged at this size.
const QUERYBATCH_LIMIT: usize = 1000;

/// Queries the public OSV database (osv.dev).
pub struct OsvSource;

impl AdvisorySource for OsvSource {
    fn name(&self) -> &'static str {
        "osv"
    }

    fn query(&self, components: &[Component]) -> crate::Result<Vec<Advisory>> {
        if components.is_empty() {
            return Ok(Vec::new());
        }
        // Page the batch at OSV's query cap; each page's positional results are read against
        // that page's slice, so the pairing survives the split.
        let mut wanted: Vec<(String, String, String)> = Vec::new(); // (name, version, vuln id)
        for page in components.chunks(QUERYBATCH_LIMIT) {
            let raw = serde_json::to_vec(&querybatch_body(page))?;
            let Some(resp) =
                download::post_json(QUERYBATCH_URL, &raw, None, Some("application/json"))
            else {
                // Unreachable endpoint (or a rejected request): report it as an error so
                // `run_audit` records OSV as failed rather than treating it as "no
                // vulnerabilities".
                return Err(
                    "OSV querybatch endpoint unreachable or returned no usable data".into(),
                );
            };
            wanted.extend(wanted_ids(&resp, page));
        }

        // Hydrate each distinct id once (a record can apply to several queried packages).
        let mut records: HashMap<String, Option<Value>> = HashMap::new();
        let mut out = Vec::new();
        for (name, version, id) in wanted {
            let record = records.entry(id.clone()).or_insert_with(|| hydrate(&id));
            match record {
                Some(record) => {
                    if let Some(advisory) = parse_osv_vuln(record, &name, &version) {
                        out.push(advisory);
                    }
                }
                None => eprintln!(
                    "npm-utils: OSV record {id} could not be fetched; audit results may be incomplete"
                ),
            }
        }
        Ok(out)
    }
}

/// The `(name, version, vuln id)` triples a querybatch response names: `results` is positional —
/// `results[i]` holds the vuln ids for `queried[i]`, so `queried` must be exactly the slice the
/// request was built from. Each component's version rides along so a confirmed hit can fall back
/// to an exact `=version` range.
fn wanted_ids(resp: &Value, queried: &[Component]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    if let Some(results) = resp.get("results").and_then(Value::as_array) {
        for (i, result) in results.iter().enumerate() {
            let Some(component) = queried.get(i) else {
                continue;
            };
            let Some(vulns) = result.get("vulns").and_then(Value::as_array) else {
                continue;
            };
            for v in vulns {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    out.push((
                        component.name.clone(),
                        component.version.clone(),
                        id.to_string(),
                    ));
                }
            }
        }
    }
    out
}

/// The querybatch body: one `{ package, version }` query per component, in order.
fn querybatch_body(components: &[Component]) -> Value {
    let queries: Vec<Value> = components
        .iter()
        .map(|c| {
            json!({
                "package": { "name": c.name, "ecosystem": "npm" },
                "version": c.version,
            })
        })
        .collect();
    json!({ "queries": queries })
}

fn hydrate(id: &str) -> Option<Value> {
    let bytes = download::fetch(&format!("{VULN_URL_BASE}/{id}")).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

/// Parse a hydrated OSV record into an [`Advisory`] for the npm package `want_name` at
/// `want_version`, or `None` when the record has no `affected` entry for that npm package. Severity
/// is read from `database_specific.severity` (a bucket word); the CVSS vector, when present, is
/// carried for display but not scored — an advisory with only a CVSS vector therefore has an unknown
/// severity, which the audit treats conservatively (it still trips `--audit-level`).
///
/// The vulnerable range is the matching entry's SEMVER events (or its explicit `versions` list).
/// When no range can be reconstructed but the querybatch already confirmed `want_version` is
/// affected, it falls back to an exact `=want_version` so a confirmed hit is never dropped.
pub fn parse_osv_vuln(record: &Value, want_name: &str, want_version: &str) -> Option<Advisory> {
    let id = record.get("id").and_then(Value::as_str)?.to_string();
    let affected = record.get("affected").and_then(Value::as_array)?;
    let entry = affected.iter().find(|a| {
        let pkg = a.get("package");
        pkg.and_then(|p| p.get("ecosystem")).and_then(Value::as_str) == Some("npm")
            && pkg.and_then(|p| p.get("name")).and_then(Value::as_str) == Some(want_name)
    })?;
    // Prefer the record's own affected range, but only when it actually covers the version the
    // querybatch confirmed as affected. Otherwise fall back to an exact `=version`: OSV already told
    // us this version is affected, so a range we can't reconstruct (ECOSYSTEM-only or unparseable)
    // must not turn a confirmed hit into a false negative.
    let vulnerable_range = osv_range_string(entry)
        .filter(|r| range_covers(r, want_version))
        .unwrap_or_else(|| format!("={want_version}"));

    let database_specific = record.get("database_specific");
    let severity = database_specific
        .and_then(|d| d.get("severity"))
        .and_then(Value::as_str)
        .and_then(Severity::from_str_loose);
    let cvss_vector = record
        .get("severity")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find_map(|s| s.get("score").and_then(Value::as_str))
        })
        .map(str::to_string);
    let url = record
        .get("references")
        .and_then(Value::as_array)
        .and_then(|refs| {
            refs.iter()
                .find_map(|r| r.get("url").and_then(Value::as_str))
        })
        .map(str::to_string)
        .or_else(|| Some(format!("https://osv.dev/vulnerability/{id}")));

    Some(Advisory {
        source: "osv",
        id,
        aliases: string_array(record.get("aliases")),
        package: want_name.to_string(),
        vulnerable_range,
        severity,
        title: record
            .get("summary")
            .or_else(|| record.get("details"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url,
        cwe: string_array(database_specific.and_then(|d| d.get("cwe_ids"))),
        cvss_score: None,
        cvss_vector,
        matched_version: String::new(),
    })
}

/// Turn an `affected` entry into an npm-style range string. SEMVER ranges are preferred: within a
/// range the events are an ordered sequence — an `introduced` opens an interval (`>=A`, or nothing
/// for `"0"`), a `fixed`/`last_affected` closes it (`<B` / `<=B`); an interval still open at the end
/// is open-ended. Intervals are ANDed within a range and ORed (`||`) across ranges — so e.g.
/// `[introduced:0, fixed:4.17.12]` → `<4.17.12`. When no SEMVER range is expressed, the explicit
/// affected `versions` list is used instead (each as `=v`). `None` if neither is present.
fn osv_range_string(entry: &Value) -> Option<String> {
    let mut alternatives: Vec<String> = Vec::new();
    if let Some(ranges) = entry.get("ranges").and_then(Value::as_array) {
        for r in ranges {
            if r.get("type").and_then(Value::as_str) != Some("SEMVER") {
                continue;
            }
            let Some(events) = r.get("events").and_then(Value::as_array) else {
                continue;
            };
            let mut lower: Option<String> = None;
            let mut open = false;
            for e in events {
                if let Some(introduced) = e.get("introduced").and_then(Value::as_str) {
                    lower = (introduced != "0").then(|| introduced.to_string());
                    open = true;
                } else if let Some(fixed) = e.get("fixed").and_then(Value::as_str) {
                    alternatives.push(interval(lower.as_deref(), Some(("<", fixed))));
                    lower = None;
                    open = false;
                } else if let Some(last) = e.get("last_affected").and_then(Value::as_str) {
                    alternatives.push(interval(lower.as_deref(), Some(("<=", last))));
                    lower = None;
                    open = false;
                }
            }
            if open {
                alternatives.push(interval(lower.as_deref(), None));
            }
        }
    }
    // Fall back to the explicit affected `versions` list (e.g. an ECOSYSTEM-only advisory that
    // carries no SEMVER range): each exact version becomes an `=v` alternative.
    if alternatives.is_empty() {
        if let Some(versions) = entry.get("versions").and_then(Value::as_array) {
            for v in versions.iter().filter_map(Value::as_str) {
                alternatives.push(format!("={v}"));
            }
        }
    }
    (!alternatives.is_empty()).then(|| alternatives.join(" || "))
}

/// Whether `range` (npm grammar) contains `version` — used to decide whether a synthesized OSV range
/// can be trusted for the querybatch-confirmed version.
fn range_covers(range: &str, version: &str) -> bool {
    match (Range::parse(range), semver::Version::parse(version)) {
        (Ok(r), Ok(v)) => r.matches(&v),
        _ => false,
    }
}

/// One affected interval as comparators: a lower `>=A` (when bounded) ANDed with an upper `<B`/`<=B`
/// (when present). An interval with neither bound is "all versions" (`*`).
fn interval(lower: Option<&str>, upper: Option<(&str, &str)>) -> String {
    let mut parts = Vec::new();
    if let Some(l) = lower {
        parts.push(format!(">={l}"));
    }
    if let Some((op, v)) = upper {
        parts.push(format!("{op}{v}"));
    }
    if parts.is_empty() {
        "*".to_string()
    } else {
        parts.join(" ")
    }
}

fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn querybatch_pages_split_at_the_limit() {
        // 1001 components → two pages of 1000 + 1 queries; one more query per request and OSV
        // answers 400 "Too many queries." (the bug that made a 1077-package audit report OSV as
        // unreachable).
        let components: Vec<Component> = (0..=QUERYBATCH_LIMIT)
            .map(|i| component(&format!("pkg{i}"), "1.0.0"))
            .collect();
        let query_counts: Vec<usize> = components
            .chunks(QUERYBATCH_LIMIT)
            .map(|page| querybatch_body(page)["queries"].as_array().unwrap().len())
            .collect();
        assert_eq!(query_counts, [QUERYBATCH_LIMIT, 1]);
    }

    #[test]
    fn wanted_ids_maps_results_to_the_queried_page_positionally() {
        // results[i] pairs with queried[i] — of the queried PAGE, not any global index.
        let queried = [component("a", "1.0.0"), component("b", "2.0.0")];
        let resp = json!({ "results": [
            { "vulns": [{ "id": "GHSA-aaaa" }, { "id": "GHSA-bbbb" }] },
            {},
        ]});
        let wanted = wanted_ids(&resp, &queried);
        assert_eq!(
            wanted,
            [
                (
                    "a".to_string(),
                    "1.0.0".to_string(),
                    "GHSA-aaaa".to_string()
                ),
                (
                    "a".to_string(),
                    "1.0.0".to_string(),
                    "GHSA-bbbb".to_string()
                ),
            ]
        );
        // A malformed response (no `results`) yields nothing rather than panicking.
        assert!(wanted_ids(&json!({}), &queried).is_empty());
    }

    #[test]
    fn osv_range_from_simple_introduced_zero_fixed() {
        let entry = json!({
            "package": { "name": "lodash", "ecosystem": "npm" },
            "ranges": [{ "type": "SEMVER", "events": [{ "introduced": "0" }, { "fixed": "4.17.12" }] }]
        });
        assert_eq!(osv_range_string(&entry).as_deref(), Some("<4.17.12"));
    }

    #[test]
    fn osv_range_from_bounded_and_multi_interval() {
        let bounded = json!({ "ranges": [{ "type": "SEMVER",
            "events": [{ "introduced": "1.0.0" }, { "fixed": "1.5.0" }] }] });
        assert_eq!(
            osv_range_string(&bounded).as_deref(),
            Some(">=1.0.0 <1.5.0")
        );

        let multi = json!({ "ranges": [{ "type": "SEMVER", "events": [
            { "introduced": "1.0.0" }, { "fixed": "1.2.0" },
            { "introduced": "2.0.0" }, { "fixed": "2.2.0" }
        ] }] });
        assert_eq!(
            osv_range_string(&multi).as_deref(),
            Some(">=1.0.0 <1.2.0 || >=2.0.0 <2.2.0")
        );

        let open =
            json!({ "ranges": [{ "type": "SEMVER", "events": [{ "introduced": "3.0.0" }] }] });
        assert_eq!(osv_range_string(&open).as_deref(), Some(">=3.0.0"));
    }

    #[test]
    fn osv_range_falls_back_to_explicit_versions_list() {
        // No SEMVER range (an ECOSYSTEM range is skipped) but an explicit affected versions list.
        let entry = json!({
            "ranges": [{ "type": "ECOSYSTEM", "events": [{ "introduced": "0" }] }],
            "versions": ["1.2.3", "1.2.4"]
        });
        assert_eq!(
            osv_range_string(&entry).as_deref(),
            Some("=1.2.3 || =1.2.4")
        );
    }

    #[test]
    fn parse_osv_vuln_matches_npm_package_only() {
        let record = json!({
            "id": "GHSA-jf85-cpcp-j695",
            "summary": "Prototype Pollution in lodash",
            "aliases": ["CVE-2019-10744"],
            "database_specific": { "severity": "CRITICAL", "cwe_ids": ["CWE-1321", "CWE-20"] },
            "references": [{ "type": "ADVISORY", "url": "https://nvd.nist.gov/vuln/detail/CVE-2019-10744" }],
            "affected": [
                { "package": { "name": "lodash", "ecosystem": "npm" },
                  "ranges": [{ "type": "SEMVER", "events": [{ "introduced": "0" }, { "fixed": "4.17.12" }] }] },
                { "package": { "name": "lodash-rails", "ecosystem": "RubyGems" },
                  "ranges": [{ "type": "ECOSYSTEM", "events": [{ "introduced": "0" }, { "fixed": "4.17.12" }] }] }
            ]
        });
        let adv = parse_osv_vuln(&record, "lodash", "4.17.11").expect("npm lodash affected");
        assert_eq!(adv.source, "osv");
        assert_eq!(adv.id, "GHSA-jf85-cpcp-j695");
        assert_eq!(adv.aliases, vec!["CVE-2019-10744"]);
        assert_eq!(adv.severity, Some(Severity::Critical));
        assert_eq!(adv.vulnerable_range, "<4.17.12");
        assert_eq!(adv.cwe, vec!["CWE-1321", "CWE-20"]);
        assert_eq!(
            adv.url.as_deref(),
            Some("https://nvd.nist.gov/vuln/detail/CVE-2019-10744")
        );

        // The RubyGems ecosystem and unrelated names are ignored.
        assert!(parse_osv_vuln(&record, "lodash-rails", "4.17.11").is_none());
        assert!(parse_osv_vuln(&record, "express", "1.0.0").is_none());
    }

    #[test]
    fn parse_osv_vuln_keeps_a_confirmed_hit_without_a_semver_range() {
        // An npm entry with no SEMVER range and no versions list, and no severity bucket (only a
        // CVSS vector): the querybatch-confirmed version must still be kept — with an exact fallback
        // range and an unknown (None) severity that the audit treats conservatively.
        let record = json!({
            "id": "GHSA-xxxx-yyyy-zzzz",
            "summary": "Something in demo",
            "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }],
            "affected": [
                { "package": { "name": "demo", "ecosystem": "npm" },
                  "ranges": [{ "type": "ECOSYSTEM", "events": [{ "introduced": "0" }] }] }
            ]
        });
        let adv = parse_osv_vuln(&record, "demo", "1.2.3").expect("confirmed hit is kept");
        assert_eq!(adv.vulnerable_range, "=1.2.3");
        assert_eq!(adv.severity, None);
        assert!(adv.cvss_vector.is_some());
    }
}
