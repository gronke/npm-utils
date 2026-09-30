//! The pinned differential specification of the matcher: every case below is a pattern, a path,
//! an option preset and the partial flag, and `tests/fixtures/minimatch/minimatch.json` records
//! what a pinned minimatch answers for it. The ordinary test holds `npm_utils::minimatch` to that
//! record on every run, with no node around; the ignored test rewrites the record from the
//! minimatch a `node_modules` holds, so a new minimatch release becomes a reviewable diff of the
//! fixture rather than a drift the port acquires in silence.
//!
//! ```text
//! MINIMATCH_NODE_PATH=/path/to/node_modules cargo test --test minimatch -- --ignored
//! ```
//!
//! Without the variable the recording uses the minimatch bundled in the global npm
//! (`$(npm root -g)/npm/node_modules`). The few answers the port gives on purpose against the
//! record are listed in `KNOWN_DIVERGENCES` with their reason.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use npm_utils::minimatch::{brace_expand, Minimatch, Options};

const FIXTURE: &str = "tests/fixtures/minimatch/minimatch.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preset {
    Default,
    Dot,
    Nocase,
    DotNocase,
    MatchBase,
    Ignore,
    Flip,
    Nobrace,
    Noext,
    Noglobstar,
    Nonegate,
    Nocomment,
}

impl Preset {
    fn name(self) -> &'static str {
        match self {
            Preset::Default => "default",
            Preset::Dot => "dot",
            Preset::Nocase => "nocase",
            Preset::DotNocase => "dotnocase",
            Preset::MatchBase => "matchbase",
            Preset::Ignore => "ignore",
            Preset::Flip => "flip",
            Preset::Nobrace => "nobrace",
            Preset::Noext => "noext",
            Preset::Noglobstar => "noglobstar",
            Preset::Nonegate => "nonegate",
            Preset::Nocomment => "nocomment",
        }
    }

    fn options(self) -> Options {
        let d = Options::DEFAULT;
        match self {
            Preset::Default => d,
            Preset::Dot => Options { dot: true, ..d },
            Preset::Nocase => Options { nocase: true, ..d },
            Preset::DotNocase => Options {
                dot: true,
                nocase: true,
                ..d
            },
            Preset::MatchBase => Options {
                match_base: true,
                ..d
            },
            Preset::Ignore => Options::ignore_walk(),
            Preset::Flip => Options {
                flip_negate: true,
                ..d
            },
            Preset::Nobrace => Options { nobrace: true, ..d },
            Preset::Noext => Options { noext: true, ..d },
            Preset::Noglobstar => Options {
                noglobstar: true,
                ..d
            },
            Preset::Nonegate => Options {
                nonegate: true,
                ..d
            },
            Preset::Nocomment => Options {
                nocomment: true,
                ..d
            },
        }
    }

    /// The same options as minimatch spells them.
    fn js(self) -> serde_json::Value {
        let o = self.options();
        serde_json::json!({
            "dot": o.dot, "nocase": o.nocase, "matchBase": o.match_base, "flipNegate": o.flip_negate,
            "nobrace": o.nobrace, "noext": o.noext, "noglobstar": o.noglobstar, "nonegate": o.nonegate,
            "nocomment": o.nocomment,
        })
    }
}

use Preset::*;

/// (pattern, path, preset, partial)
const CASES: &[(&str, &str, Preset, bool)] = &[
    // A lone star needs one character and never takes the dot directories.
    ("*", "", Default, false),
    ("*", "a", Default, false),
    ("*", ".a", Default, false),
    ("*", ".a", Dot, false),
    ("*", ".", Dot, false),
    ("*", "..", Dot, false),
    ("*", ".", Default, false),
    ("*", "😀", Default, false),
    (".*", "..", Default, false),
    (".*", ".", Default, false),
    (".*", ".a", Default, false),
    (".*", "..", Dot, false),
    ("..*", "..", Default, false),
    ("..*", "..a", Default, false),
    (".", ".", Default, false),
    ("..", "..", Default, false),
    (".", "..", Default, false),
    ("?a", ".a", Default, false),
    ("?a", ".a", Dot, false),
    ("[.]a", ".a", Default, false),
    ("[.]a", ".a", Dot, false),
    // Trailing slashes and empty segments.
    ("a/*", "a/b/", Default, false),
    ("a/*", "a/b/c", Default, false),
    ("a/*", "a/", Default, false),
    ("a/", "a/", Default, false),
    ("a/", "a", Default, false),
    ("a", "a/", Default, false),
    ("a/b/", "a/b", Default, false),
    ("a/b", "a/b/", Default, false),
    ("a//b", "a/b", Default, false),
    ("a/b", "a//b", Default, false),
    ("/a", "//a", Default, false),
    ("/a", "a", Default, false),
    ("a", "/a", Default, false),
    ("a\\/b", "a/b", Default, false),
    // Globstars.
    ("a/**", "a/b/c", Default, false),
    ("a/**", "a", Default, false),
    ("a/**", "a/", Default, false),
    ("a/**", "a/.b", Default, false),
    ("a/**", "a/.b", Dot, false),
    ("**/a", ".x/a", Default, false),
    ("**/a", ".x/a", Dot, false),
    ("**", "a/b", Default, false),
    ("**", ".", Default, false),
    ("**", "", Default, false),
    ("**", "a/.b/c", Default, false),
    ("a/**/b", "a/b", Default, false),
    ("a/**/b", "a/x/y/b", Default, false),
    ("a/**/b", "a/.x/b", Default, false),
    ("a/**/b", "a/.x/b", Dot, false),
    ("a/**/b", "a/b/b", Default, false),
    ("**/*.js", "a/b/c.js", Default, false),
    ("**/*.js", ".a/c.js", Default, false),
    ("a/**/*", "a/b/", Default, false),
    ("a/**/b/**/c", "a/b/c", Default, false),
    ("a/**/b/**/c", "a/1/b/2/c", Default, false),
    ("a/**/b/**/c", "a/1/c", Default, false),
    ("**/b/**", "a/b/c", Default, false),
    ("**/b/**", "b", Default, false),
    ("**/b/**", "b/", Default, false),
    ("**/a/**/b/**/c/**/d", "a/x/b/y/c/z/d", Default, false),
    ("**/**/a", "x/a", Default, false),
    ("a/**/../b", "a/b", Default, false),
    ("a/**/../b", "a/../b", Default, false),
    ("a/**/", "a/b/", Default, false),
    ("a/../b", "b", Default, false),
    ("a/../b", "a/../b", Default, false),
    ("../a", "../a", Default, false),
    ("a/./b", "a/./b", Default, false),
    ("**/a", "x/y/a", Noglobstar, false),
    ("**/a", "x/a", Noglobstar, false),
    ("**", "a/b", Noglobstar, false),
    ("**", "ab", Noglobstar, false),
    // matchBase: a slash-less pattern against the basename.
    ("*.js", "a/b/c.js", MatchBase, false),
    ("*.js", "a/b/c.js", Default, false),
    ("a/*.js", "x/a/b.js", MatchBase, false),
    ("c.js", "a/b/c.js", MatchBase, false),
    ("*.js", "a/b/c.js/", MatchBase, false),
    // Partial mode.
    ("a/b/c", "a", Default, true),
    ("a/b/c", "a/x", Default, true),
    ("a/b/c", "a/b/c/d", Default, true),
    ("a/*/c", "/", Default, true),
    ("a/**/c", "a/b", Default, true),
    ("a/**", "a", Default, true),
    ("lib/*.js", "lib", Default, true),
    ("lib/*.js", "lib/", Default, true),
    ("lib/*.js", "src", Default, true),
    // Negation, flipNegate, nonegate, comments, the empty pattern.
    ("!a", "a", Default, false),
    ("!a", "b", Default, false),
    ("!!a", "a", Default, false),
    ("!a", "a", Flip, false),
    ("!a", "b", Flip, false),
    ("!a", "a", Nonegate, false),
    ("!a", "!a", Nonegate, false),
    ("#a", "#a", Default, false),
    ("#a", "#a", Nocomment, false),
    ("#", "#", Nocomment, false),
    ("", "", Default, false),
    ("", "a", Default, false),
    // The fast paths.
    ("*.js", "a.js", Default, false),
    ("*.js", ".a.js", Default, false),
    ("*.js", ".a.js", Dot, false),
    ("*.js", "a.JS", Nocase, false),
    ("*.js", "a.jsx", Default, false),
    ("*\\.js", "a.js", Default, false),
    ("*\\.js", "a\\.js", Default, false),
    ("**.js", "a.js", Default, false),
    ("?", "a", Default, false),
    ("?", "ab", Default, false),
    ("?", "😀", Default, false),
    ("??", "ab", Default, false),
    ("??", ".a", Default, false),
    ("??", ".a", Dot, false),
    ("?.js", "a.js", Default, false),
    ("??.js", "a.js", Default, false),
    ("?.js", "a.JS", Nocase, false),
    ("*.*", "a.b", Default, false),
    ("*.*", ".a", Default, false),
    ("*.*", ".a", Dot, false),
    ("*.*", "..", Dot, false),
    // Classes.
    ("[a-c]x", "bx", Default, false),
    ("[a-c]x", "Bx", Nocase, false),
    ("[!a-c]x", "bx", Default, false),
    ("[]a]", "]", Default, false),
    ("[]a]", "a", Default, false),
    ("[z-a]", "m", Default, false),
    ("[a", "[a", Default, false),
    ("[[:digit:]]*.log", "1abc.log", Default, false),
    ("[[:digit:]]*.log", "abc.log", Default, false),
    ("[[:alpha:]-]x", "-x", Default, false),
    ("[[:alpha:]]x", "éx", Default, false),
    ("[[:ascii:]]", "~", Default, false),
    ("[[:ascii:]]", "é", Default, false),
    ("[[:upper:]]", "a", Nocase, false),
    ("[a&&b]", "&", Default, false),
    ("[😀]", "😀", Default, false),
    ("[é]", "é", Default, false),
    // Escapes and literals.
    ("\\*", "*", Default, false),
    ("\\*", "x", Default, false),
    ("a\\*b", "a*b", Default, false),
    ("\\[a\\]", "[a]", Default, false),
    ("a\\", "a\\", Default, false),
    ("\\\\", "\\", Default, false),
    ("\\|", "|", Default, false),
    ("\\|", "anything", Default, false),
    ("\\!x", "!x", Default, false),
    ("\\-", "-", Default, false),
    ("a,b", "a,b", Default, false),
    ("a#b", "a#b", Default, false),
    ("a b", "a b", Default, false),
    ("a$b", "a$b", Default, false),
    ("a^b", "a^b", Default, false),
    ("a?", "ab", Default, false),
    ("a?", "a", Default, false),
    ("a*b", "ab", Default, false),
    ("a*b", "axxb", Default, false),
    ("a*b", "a/b", Default, false),
    ("*/*", "a/b", Default, false),
    ("*/*", "a/.b", Default, false),
    ("é*", "éa", Default, false),
    // Extglobs.
    ("@(a|b)", "a", Default, false),
    ("@(a|b)", "c", Default, false),
    ("@(a|b)", ".", Default, false),
    ("!(a)", "b", Default, false),
    ("!(a)", "a", Default, false),
    ("!(a)", "", Default, false),
    ("!(a)", ".b", Default, false),
    ("!(a)", ".b", Dot, false),
    ("!(a|b)c", "ac", Default, false),
    ("!(a|b)c", "xc", Default, false),
    ("!(a|b)c", "c", Default, false),
    ("!(a)*", "ab", Default, false),
    ("!(a)*", "ba", Default, false),
    ("x!(a)", "xa", Default, false),
    ("x!(a)", "xb", Default, false),
    ("x!(a)", "x", Default, false),
    ("*(a|b)c", "c", Default, false),
    ("*(a|b)c", "abbac", Default, false),
    ("*(a|b)c", "abd", Default, false),
    ("+(ab)", "ababab", Default, false),
    ("+(ab)", "", Default, false),
    ("+(ab)", "aba", Default, false),
    ("?(x)y", "y", Default, false),
    ("?(x)y", "xy", Default, false),
    ("?(x)y", "xxy", Default, false),
    ("@(a|@(b|c))", "c", Default, false),
    ("x*(", "x*(", Default, false),
    ("*.@(pem|key)", "secret.pem", Default, false),
    ("*.@(pem|key)", "secret.pemx", Default, false),
    ("a.!(js)", "a.ts", Default, false),
    ("a.!(js)", "a.js", Default, false),
    ("a.!(js)", "a.jsx", Default, false),
    ("!(*.js)", "a.ts", Default, false),
    ("!(*.js)", "a.js", Default, false),
    ("!(*.js)", "(a.js)", Default, false),
    ("!(*.js)", "a.ts", Ignore, false),
    ("!(*.js)", "(a.js)", Ignore, false),
    ("*(?)", "x.y", Default, false),
    ("*(?)", ".x", Default, false),
    ("+(*|.x*)", ".xy", Default, false),
    ("+(*|.x*)", ".yx", Default, false),
    ("!()", "a", Default, false),
    ("!()", "", Default, false),
    ("@()", "@()", Default, false),
    ("@()", "", Default, false),
    ("*()", "*()", Default, false),
    ("!(a)!(b)", "ab", Default, false),
    ("!(a)!(b)", "cd", Default, false),
    ("!(a)!(b)", "ac", Default, false),
    ("!(!(a))", "a", Default, false),
    ("!(!(a))", "b", Default, false),
    ("@(!(a))", "b", Default, false),
    ("@(!(a))", "a", Default, false),
    ("?(+(a))", "aaa", Default, false),
    ("?(+(a))", "", Default, false),
    ("+(?(a))", "", Default, false),
    ("+(?(a))", "aa", Default, false),
    ("!(?(a)|b)", "", Default, false),
    ("!(?(a)|b)", "c", Default, false),
    ("+(a|+(b|c))", "abc", Default, false),
    ("@(a|@(*|b))", "xyz", Default, false),
    ("@(a|@(*|b))", "", Default, false),
    ("!(a).b", "a.b", Default, false),
    ("!(a).b", "x.b", Default, false),
    ("!(a).b", ".b", Default, false),
    ("@(a|b)/c", "a/c", Default, false),
    ("!(a)/c", "b/c", Default, false),
    ("!(a)/c", "a/c", Default, false),
    ("**/!(a)", "x/b", Default, false),
    ("**/!(a)", "x/a", Default, false),
    ("@(a|@(b|@(c|@(d))))", "d", Default, false),
    ("@(a|@(b|@(c|@(d))))", "@(d)", Default, false),
    ("@(a|b)", "@(a|b)", Noext, false),
    ("@(a|b)", "a", Noext, false),
    ("!(a)", "b", Noext, false),
    ("(a)", "(a)", Noext, false),
    // Braces at the match level.
    ("a{b,c}d", "acd", Default, false),
    ("a{b,c}d", "ad", Default, false),
    ("{a,b}/c", "b/c", Default, false),
    ("x{,.y}", "x", Default, false),
    ("x{,.y}", "x.y", Default, false),
    ("!/readme{,.*[^~$]}", "/README.md", Ignore, false),
    ("!/readme{,.*[^~$]}", "/README", Ignore, false),
    ("!/readme{,.*[^~$]}", "/README.md~", Ignore, false),
    ("!/readme{,.*[^~$]}", "/readme-first.md", Ignore, false),
    ("readme{,.*[^~$]}", "README.md~", Nocase, false),
    ("{a..c}", "b", Default, false),
    ("{a,b}", "{a,b}", Nobrace, false),
    ("{a,b}", "a", Nobrace, false),
    ("\\{a,b\\}", "{a,b}", Default, false),
    ("a{b}c", "a{b}c", Default, false),
    ("{,}", "", Default, false),
    ("{,}", "a", Default, false),
    // Case folding.
    ("a", "A", Nocase, false),
    ("A", "a", Nocase, false),
    ("é", "É", Nocase, false),
    ("k", "\u{212A}", Nocase, false),
    ("ß", "SS", Nocase, false),
    ("[a-c]", "B", Nocase, false),
    ("node_modules", "a/b/NODE_MODULES", Ignore, false),
    ("*.orig", "deep/er/file.ORIG", Ignore, false),
    ("/.git", "/.git", Ignore, false),
    ("/.git", ".git", Ignore, false),
    ("/.git", "/sub/.git", Ignore, false),
    ("**/.git/**", "a/b/.git/objects/x", Ignore, false),
    ("**/.git/**", "a/.gitignore", Ignore, false),
    ("!dist/**", "dist", Ignore, false),
    ("!dist/**", "dist/", Ignore, false),
    ("!dist/**", "dist/nested/x.d.ts", Ignore, false),
    ("*", ".hidden", Ignore, false),
    ("*", "..", Ignore, false),
    ("*.JS", "a.js", DotNocase, false),
];

/// Brace expansions pinned the same way.
const EXPANSIONS: &[&str] = &[
    "a{b,c}d",
    "x{,.y}",
    "a{b{c,d},e}",
    "{01..03}",
    "{3..1}",
    "{1..6..2}",
    "{a..c}",
    "{a..b..c}",
    "\\{a,b\\}",
    "{a,b}{c,d}",
    "{,}",
    "a{,}",
    "readme{,.*[^~$]}",
];

/// Where the port answers differently on purpose: JavaScript counts UTF-16 units where the port
/// counts characters, folds only ASCII case in this configuration where the engine folds by
/// Unicode simple folding, and its `u` flag rejects an escaped `-` beside a POSIX class.
const KNOWN_DIVERGENCES: &[(&str, &str, Preset, bool, Answer)] = &[
    ("?", "😀", Default, false, Answer::Bool(true)),
    ("[😀]", "😀", Default, false, Answer::Bool(true)),
    ("k", "\u{212A}", Nocase, false, Answer::Bool(true)),
    ("[[:alpha:]-]x", "-x", Default, false, Answer::Bool(true)),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Bool(bool),
    Error,
}

impl Answer {
    fn from_json(v: &serde_json::Value) -> Answer {
        match v {
            serde_json::Value::Bool(b) => Answer::Bool(*b),
            _ => Answer::Error,
        }
    }
}

fn ours(pattern: &str, path: &str, preset: Preset, partial: bool) -> Answer {
    let Ok(mm) = Minimatch::new(pattern, preset.options()) else {
        return Answer::Error;
    };
    let result = if partial {
        mm.is_match_partial(path)
    } else {
        mm.is_match(path)
    };
    result.map_or(Answer::Error, Answer::Bool)
}

type Key = (String, String, String, bool);

/// The record: `{"minimatch": "<version>", "cases": [[pattern, path, preset, partial, answer]],
/// "expansions": {pattern: [strings] | "error"}}`.
struct Fixture {
    minimatch: String,
    cases: BTreeMap<Key, Answer>,
    expansions: BTreeMap<String, Option<Vec<String>>>,
}

impl Fixture {
    fn read() -> Fixture {
        let text = std::fs::read_to_string(fixture_path())
            .unwrap_or_else(|e| panic!("{FIXTURE}: {e}; run the ignored recording test"));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let cases = value["cases"]
            .as_array()
            .expect("a list of cases")
            .iter()
            .map(|c| {
                let row = c.as_array().expect("a row");
                let key = (
                    row[0].as_str().unwrap().to_string(),
                    row[1].as_str().unwrap().to_string(),
                    row[2].as_str().unwrap().to_string(),
                    row[3].as_bool().unwrap(),
                );
                (key, Answer::from_json(&row[4]))
            })
            .collect();
        let expansions = value["expansions"]
            .as_object()
            .expect("expansions object")
            .iter()
            .map(|(pattern, v)| {
                let strings = v.as_array().map(|items| {
                    items
                        .iter()
                        .map(|s| s.as_str().unwrap().to_string())
                        .collect()
                });
                (pattern.clone(), strings)
            })
            .collect();
        Fixture {
            minimatch: value["minimatch"]
                .as_str()
                .expect("the version")
                .to_string(),
            cases,
            expansions,
        }
    }

    fn write(&self) {
        let cases: Vec<serde_json::Value> = self
            .cases
            .iter()
            .map(|((pattern, path, preset, partial), answer)| {
                let answer = match answer {
                    Answer::Bool(b) => serde_json::json!(b),
                    Answer::Error => serde_json::json!("error"),
                };
                serde_json::json!([pattern, path, preset, partial, answer])
            })
            .collect();
        let expansions: serde_json::Map<String, serde_json::Value> = self
            .expansions
            .iter()
            .map(|(pattern, strings)| {
                let v = match strings {
                    Some(strings) => serde_json::json!(strings),
                    None => serde_json::json!("error"),
                };
                (pattern.clone(), v)
            })
            .collect();
        let value = serde_json::json!({
            "minimatch": self.minimatch, "cases": cases, "expansions": expansions,
        });
        std::fs::write(
            fixture_path(),
            serde_json::to_string_pretty(&value).unwrap() + "\n",
        )
        .unwrap();
    }
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

fn key(pattern: &str, path: &str, preset: Preset, partial: bool) -> Key {
    (
        pattern.to_string(),
        path.to_string(),
        preset.name().to_string(),
        partial,
    )
}

#[test]
fn matches_the_pinned_minimatch() {
    let fixture = Fixture::read();
    let mut seen = 0;
    for &(pattern, path, preset, partial) in CASES {
        let recorded = *fixture
            .cases
            .get(&key(pattern, path, preset, partial))
            .unwrap_or_else(|| {
                panic!("{pattern:?} against {path:?} ({}, partial {partial}): no record in {FIXTURE}; run the ignored recording test", preset.name())
            });
        let expected = KNOWN_DIVERGENCES
            .iter()
            .find(|d| d.0 == pattern && d.1 == path && d.2 == preset && d.3 == partial)
            .map_or(recorded, |d| d.4);
        assert_eq!(
            ours(pattern, path, preset, partial),
            expected,
            "{pattern:?} against {path:?} ({}, partial {partial}) against minimatch {} (recorded {recorded:?})",
            preset.name(),
            fixture.minimatch
        );
        seen += 1;
    }
    assert_eq!(
        seen,
        fixture.cases.len(),
        "every recorded case is still a case"
    );
}

#[test]
fn expands_as_the_pinned_minimatch() {
    let fixture = Fixture::read();
    for pattern in EXPANSIONS {
        let expected = fixture
            .expansions
            .get(*pattern)
            .unwrap_or_else(|| panic!("{pattern:?}: no record in {FIXTURE}"));
        let got = brace_expand(pattern, Options::DEFAULT).ok();
        assert_eq!(&got, expected, "{pattern:?}");
    }
    assert_eq!(EXPANSIONS.len(), fixture.expansions.len());
}

#[test]
fn every_divergence_is_a_case() {
    for d in KNOWN_DIVERGENCES {
        assert!(
            CASES
                .iter()
                .any(|c| c.0 == d.0 && c.1 == d.1 && c.2 == d.2 && c.3 == d.3),
            "{d:?}"
        );
    }
}

const RECORDER: &str = r#"
const fs = require('fs');
const { Minimatch, braceExpand } = require('minimatch');
const input = JSON.parse(fs.readFileSync(0, 'utf8'));
const out = { cases: [], expansions: {} };
for (const c of input.cases) {
  let result;
  try { result = new Minimatch(c.pattern, c.options).match(c.path, c.partial); } catch (e) { result = 'error'; }
  out.cases.push([c.pattern, c.path, c.preset, c.partial, result]);
}
for (const p of input.expansions) {
  try { out.expansions[p] = braceExpand(p); } catch (e) { out.expansions[p] = 'error'; }
}
process.stdout.write(JSON.stringify(out));
"#;

/// Rewrite the fixture from the minimatch under `MINIMATCH_NODE_PATH` or the global npm's.
#[test]
#[ignore = "needs node: records minimatch's answers into the fixture"]
fn record_the_pinned_minimatch() {
    use std::io::Write as _;
    let node_path = match std::env::var("MINIMATCH_NODE_PATH") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let root = std::process::Command::new("npm")
                .args(["root", "-g"])
                .output()
                .expect("npm on PATH, or MINIMATCH_NODE_PATH set");
            PathBuf::from(String::from_utf8(root.stdout).unwrap().trim())
                .join("npm")
                .join("node_modules")
        }
    };
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(node_path.join("minimatch").join("package.json"))
            .unwrap_or_else(|e| panic!("{}: {e}", node_path.display())),
    )
    .unwrap();
    let version = manifest["version"].as_str().unwrap().to_string();
    let cases: Vec<serde_json::Value> = CASES
        .iter()
        .map(|&(pattern, path, preset, partial)| {
            serde_json::json!({
                "pattern": pattern, "path": path, "preset": preset.name(), "partial": partial,
                "options": preset.js(),
            })
        })
        .collect();
    let input = serde_json::json!({ "cases": cases, "expansions": EXPANSIONS });
    let mut child = std::process::Command::new("node")
        .args(["-e", RECORDER])
        .env("NODE_PATH", &node_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("node on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let recorded: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let cases = recorded["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let row = row.as_array().unwrap();
            let preset = CASES
                .iter()
                .find(|c| c.2.name() == row[2].as_str().unwrap())
                .map(|c| c.2)
                .unwrap();
            (
                key(
                    row[0].as_str().unwrap(),
                    row[1].as_str().unwrap(),
                    preset,
                    row[3].as_bool().unwrap(),
                ),
                Answer::from_json(&row[4]),
            )
        })
        .collect();
    let expansions = recorded["expansions"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(pattern, v)| {
            let strings = v.as_array().map(|items| {
                items
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect()
            });
            (pattern.clone(), strings)
        })
        .collect();
    Fixture {
        minimatch: version,
        cases,
        expansions,
    }
    .write();
}
