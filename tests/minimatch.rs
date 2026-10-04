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
//!
//! The second ignored test generates 1500 patterns and compares the answers live, since the
//! port evaluates negations structurally where the JavaScript runs a lookahead.

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
    // Negation groups in every position: mid-segment, at the end, under repeats, nested, with
    // dot rules, empty alternatives, folded tails, classes and groups inside, across segments,
    // under matchBase, case folding and partial matching.
    ("x!(a)y", "xay", Default, false),
    ("x!(a)y", "xby", Default, false),
    ("x!(a)y", "xy", Default, false),
    ("x!(a)y", "xaby", Default, false),
    ("x!(a)yz", "xayz", Default, false),
    ("x!(a)yz", "xbyz", Default, false),
    ("!(a)b", "ab", Default, false),
    ("!(a)b", "xb", Default, false),
    ("!(a)b", "b", Default, false),
    ("!(a)bc", "abc", Default, false),
    ("!(a)bc", "xbc", Default, false),
    ("a!(b)c", "abc", Default, false),
    ("a!(b)c", "axc", Default, false),
    ("a!(b)c", "ac", Default, false),
    ("!(a)b!(c)d", "abd", Default, false),
    ("!(a)b!(c)d", "xbyd", Default, false),
    ("!(a)b!(c)d", "xbcd", Default, false),
    ("x!(a)", "xaa", Default, false),
    ("!(a)*", "", Default, false),
    ("!(a)*", "aa", Default, false),
    ("!(a)b*", "ab*", Default, false),
    ("!(a)b*", "xb", Default, false),
    ("*(!(a))y", "y", Default, false),
    ("*(!(a))y", "ay", Default, false),
    ("*(!(a))y", "xy", Default, false),
    ("*(!(a))y", "aay", Default, false),
    ("+(!(a)|b)c", "bc", Default, false),
    ("+(!(a)|b)c", "ac", Default, false),
    ("+(!(a)|b)c", "bbc", Default, false),
    ("!(!(!(a)))", "a", Default, false),
    ("!(!(!(a)))", "b", Default, false),
    ("!(a!(b))", "ab", Default, false),
    ("!(a!(b))", "b", Default, false),
    ("x!(a!(b))y", "xaby", Default, false),
    ("x!(a!(b))y", "xby", Default, false),
    ("!(ab|a)c", "ac", Default, false),
    ("!(ab|a)c", "abc", Default, false),
    ("!(ab|a)c", "c", Default, false),
    ("!(a|)", "a", Default, false),
    ("!(a|)", "b", Default, false),
    ("!()x", "x", Default, false),
    ("!()x", "ax", Default, false),
    ("x!()", "x", Default, false),
    ("x!()", "xa", Default, false),
    ("!([a-c])", "b", Default, false),
    ("!([a-c])", "d", Default, false),
    ("!(@(a|b))", "a", Default, false),
    ("!(@(a|b))", "c", Default, false),
    ("!(*(a))", "aa", Default, false),
    ("!(*(a))", "b", Default, false),
    ("!(+(a))", "aa", Default, false),
    ("!(+(a))", "b", Default, false),
    (".(a|b)", ".a", Default, false),
    ("!(a)", ".a", Default, false),
    ("!(a)", ".a", Dot, false),
    ("!(.*)", ".a", Default, false),
    ("!(.*)", "a", Default, false),
    ("!(.*)", "a", Dot, false),
    ("*(!(a))", ".x", Default, false),
    ("*(!(a))", ".x", Dot, false),
    (".!(a)", ".a", Default, false),
    (".!(a)", ".b", Default, false),
    ("x/!(a)/y", "x/a/y", Default, false),
    ("x/!(a)/y", "x/b/y", Default, false),
    ("x/!(a)/y", "x/ab/y", Default, false),
    ("build/!(index.js)", "build/index.js", Default, false),
    ("build/!(index.js)", "build/other.js", Default, false),
    ("**/!(a)", "a/b/c", Default, false),
    ("**/!(a)", "a/b/a", Default, false),
    ("!(a)", "x/a", Ignore, false),
    ("!(a)", "x/b", Ignore, false),
    ("x!(a)", "x/a", Ignore, false),
    ("x!(a)", "x/b", Ignore, false),
    ("!(A)", "a", Nocase, false),
    ("!(A)", "b", Nocase, false),
    ("x!(A)y", "xay", Nocase, false),
    ("src/!(a).js", "src", Default, true),
    ("src/!(a).js", "src/b.js", Default, true),
    ("!(a)b", "x", Default, true),
    ("!(a)b", "ab", Default, true),
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
    // The ten quirks as npm answers them; the strict-mode refusals live in quirks.rs.
    ("!(a|b)", "a", Default, false),
    ("!(a|b)", "(a|b)", Default, false),
    ("?\\.js", "a.js", Default, false),
    ("a*\\|b", "a", Default, false),
    ("a*\\|b", "xb", Default, false),
    ("a*\\|b", "a|b", Default, false),
    ("a\\|b", "a|b", Default, false),
    ("x*\\|", "anything", Default, false),
    ("a\\\\*{b,c}", "a*b", Default, false),
    ("a\\\\*{b,c}", "a\\xyzb", Default, false),
    ("a\\\\*", "a\\xyz", Default, false),
    ("[[:print:]]", "a", Default, false),
    ("[[:print:]]", "\u{1}", Default, false),
    ("[[:print:]]", " ", Default, false),
    ("[[:punct:]]", "$", Default, false),
    ("[[:punct:]]", "!", Default, false),
    ("[[:punct:]]", "~", Default, false),
    ("[a-[:alpha:]]", "x", Default, false),
    ("@(a|b", "@(a|b", Default, false),
    ("[[:nope:]]", "n]", Default, false),
    ("[[:nope:]]", "n", Default, false),
    // The escaped-pipe alternation beside a negation: it splits the whole segment, not a run.
    ("a*\\|b!(c)", "xb", Default, false),
    ("a*\\|b!(c)", "b", Default, false),
    ("a*\\|b!(c)x", "a", Default, false),
    ("a*\\|b!(c)x", "axb", Default, false),
    ("a*\\|b!(c)x", "bcx", Default, false),
    ("a*\\|b!(c)x", "bc", Default, false),
    ("x!(c)a*\\|b", "b", Default, false),
    ("x!(c)a*\\|b", "xb", Default, false),
    ("x!(c)a*\\|b", "xcb", Default, false),
    ("a*\\|b!(b)", "ab", Default, false),
    ("a*\\|b!(b)", "ac", Default, false),
    ("a*\\|b!(b)x", "ab", Default, false),
    ("a*\\|b!(b)x", "bcx", Default, false),
    ("x!(y)a*\\|b", "b", Default, false),
    ("x!(y)a*\\|b", "xzb", Default, false),
    ("a*\\|bx", "bx", Default, false),
    ("a*\\|bx", "abx", Default, false),
    ("!(c)a*\\|b", "cb", Default, false),
    ("!(c)a*\\|b", "xb", Default, false),
    ("a*\\|b!(c)d!(e)", "acd", Default, false),
    ("a*\\|b!(c)d!(e)", "ad", Default, false),
    ("a*\\|b.c!(x)", "ab.c", Default, false),
    ("a*\\|b.c!(x)", "b.cy", Default, false),
    ("a*\\|b!(c)*", "ab", Default, false),
    ("a*\\|b!(c)*", "acc", Default, false),
    ("a*\\|b*(!(c))y", "axb", Default, false),
    ("a*\\|b*(!(c))y", "aab", Default, false),
    ("a*\\|b*(!(c))y", "abbc", Default, false),
    ("a*\\|b*(!(c))y", "acd", Default, false),
    ("a*\\|b*(!(c))y", "bcy", Default, false),
    ("a*\\|b*(!(c))y", "bc", Default, false),
    ("!(x)@(a*\\|b)", "cb", Default, false),
    ("!(x)@(a*\\|b)", "ab", Default, false),
    ("@(x|a*\\|b!(c))", "cb", Default, false),
    ("@(x|a*\\|b!(c))", "xb", Default, false),
    ("@(x|a*\\|b!(c))", "xab", Default, false),
    ("@(x|a*\\|b!(c))", "cxb", Default, false),
    ("@(x|a*\\|b!(c))", "zxb", Default, false),
    ("@(x|a*\\|b!(c))", "x", Default, false),
    // Sequential negations: each group absorbs the groups after it, copies included.
    ("x!(a)!(b)!(c)y", "xy", Default, false),
    ("x!(a)!(b)!(c)y", "xay", Default, false),
    ("x!(a)!(b)!(c)y", "xby", Default, false),
    ("x!(a)!(b)!(c)y", "xcy", Default, false),
    ("x!(a)!(b)!(c)y", "xaby", Default, false),
    ("x!(a)!(b)!(c)y", "xabcy", Default, false),
    ("x!(a)!(b)!(c)y", "xzy", Default, false),
    ("x!(a)!(b)!(c)y", "xzzy", Default, false),
    ("!(a)!(b)", "", Default, false),
    ("!(a)!(b)", "a", Default, false),
    ("!(a)!(b)", "b", Default, false),
    ("!(a)!(b)", "ba", Default, false),
    ("!(a)!(b)", "c", Default, false),
    // Deep adoption chains.
    ("@(a@(a@(ab)))", "aaab", Default, false),
    ("@(a@(a@(ab)))", "aab", Default, false),
    ("@(a@(a@(ab)))", "ab", Default, false),
    ("a@(a@(a@(a)))", "aaaa", Default, false),
    ("a@(a@(a@(a)))", "aaa", Default, false),
    ("a@(a@(a@(a)))", "aaaaa", Default, false),
    // A repeated group whose first iteration can match nothing: the plain iterations, with
    // their traversal guard instead of the dot guard, still run from there.
    ("*(?(x)?(y)|[!A])", ".a", Default, false),
    ("*(?(x)?(y)|[!A])", ".ab", Default, false),
    ("*(?(x)?(y)|[!A])", "a", Default, false),
    ("*(?(x)?(y)|[!A])", "..", Default, false),
    ("+(?(x)?(y)|[!A])", ".a", Default, false),
    ("*(?(x)?(y)|?)", ".a", Default, false),
    ("*(+(\\|)|[!A])", ".a", Default, false),
    (
        "*(*([[:lower:]])+(\\|)|@([!A-C0-9]|*\\.js))",
        ".a",
        Nocase,
        false,
    ),
    (
        "*(*([[:lower:]])+(\\|)|@([!A-C0-9]|*\\.js))",
        "a",
        Nocase,
        false,
    ),
    // Two pipes in one segment, the quirk's alternation with three branches: a middle branch
    // floats at its edges only, so a negation after its first run evaluates at the real match
    // end; inside a negation every branch starts at the lookahead's position.
    ("a*\\|b!(c)\\|d", "bc", Default, false),
    ("a*\\|b!(c)\\|d", "bcc", Default, false),
    ("a*\\|b!(c)\\|d", "b", Default, false),
    ("a*\\|b!(c)\\|d", "bd", Default, false),
    ("a*\\|b!(c)\\|d", "bcd", Default, false),
    ("a*\\|b!(c)\\|d", "xbx", Default, false),
    ("a*\\|b!(c)\\|d", "a", Default, false),
    ("a*\\|b!(c)\\|d", "d", Default, false),
    ("a\\|zb!(c)\\|d", "bc", Default, false),
    ("a\\|zb!(c)\\|d", "zbc", Default, false),
    ("a\\|zb!(c)\\|d", "zb", Default, false),
    ("a\\|zb!(c)\\|d", "b", Default, false),
    ("a\\|zb!(c)\\|d", "ab", Default, false),
    ("a*\\|b!(c)x\\|d", "bcx", Default, false),
    ("a*\\|xb!(c)\\|d", "xbc", Default, false),
    ("p\\|b!(c)d\\|q", "bcd", Default, false),
    ("p\\|b!(c)d\\|q", "bd", Default, false),
    ("p\\|b!(c)d\\|q", "p", Default, false),
    ("p\\|b!(c)d\\|q", "q", Default, false),
    ("x!(a\\|b\\|c)y", "xzby", Default, false),
    ("x!(a\\|b\\|c)y", "xby", Default, false),
    ("x!(a\\|b\\|c)y", "xzay", Default, false),
    ("x!(a\\|b\\|c)y", "xay", Default, false),
    ("x!(a\\|b\\|c)y", "xcy", Default, false),
    ("x!(a\\|b\\|c)y", "xzcy", Default, false),
    ("x!(a\\|b\\|c)y", "xy", Default, false),
    ("x!(a\\|b\\|c)y", "xzzy", Default, false),
    ("z!(a\\|x\\|b)", "zqx", Default, false),
    ("z!(a\\|x\\|b)", "zx", Default, false),
    ("z!(a\\|x\\|b)", "za", Default, false),
    ("z!(a\\|x\\|b)", "zq", Default, false),
    // Case folding enlarges what a negated group rejects, so `nocase` can lose a match.
    ("@(!(a))", "A", Default, false),
    ("@(!(a))", "A", Nocase, false),
    ("x!(a)y", "xAy", Default, false),
    ("x!(a)y", "xAy", Nocase, false),
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
    "a\\\\*{b,c}",
    "{a,b}{c,d}",
    "{,}",
    "a{,}",
    "readme{,.*[^~$]}",
];

/// Where the port answers differently on purpose: JavaScript counts UTF-16 units where the port
/// counts characters, never folds a non-ASCII character onto an ASCII one (its `i` flag without
/// `u` canonicalizes through `toUpperCase`) where the engine's simple folding joins `k` and the
/// Kelvin sign, and its `u` flag rejects an escaped `-` beside a POSIX class.
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

/// The `node_modules` holding the minimatch to compare against: `MINIMATCH_NODE_PATH`, else the
/// copy bundled in the global npm.
fn node_path() -> PathBuf {
    match std::env::var("MINIMATCH_NODE_PATH") {
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
    }
}

/// The version of the minimatch under `node_path`.
fn minimatch_version(node_path: &Path) -> String {
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(node_path.join("minimatch").join("package.json"))
            .unwrap_or_else(|e| panic!("{}: {e}", node_path.display())),
    )
    .unwrap();
    manifest["version"].as_str().unwrap().to_string()
}

/// One case as the recorder reads it.
fn case_json(pattern: &str, path: &str, preset: Preset, partial: bool) -> serde_json::Value {
    serde_json::json!({
        "pattern": pattern, "path": path, "preset": preset.name(), "partial": partial,
        "options": preset.js(),
    })
}

/// minimatch's answers: `{"cases": [[pattern, path, preset, partial, answer]], "expansions":
/// {pattern: [strings] | "error"}}`.
fn ask_minimatch(
    node_path: &Path,
    cases: &[serde_json::Value],
    expansions: &[&str],
) -> serde_json::Value {
    use std::io::Write as _;
    let input = serde_json::json!({ "cases": cases, "expansions": expansions });
    let mut child = std::process::Command::new("node")
        .args(["-e", RECORDER])
        .env("NODE_PATH", node_path)
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
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Rewrite the fixture from the minimatch under `MINIMATCH_NODE_PATH` or the global npm's.
#[test]
#[ignore = "needs node: records minimatch's answers into the fixture"]
fn record_the_pinned_minimatch() {
    let node_path = node_path();
    let cases: Vec<serde_json::Value> = CASES
        .iter()
        .map(|&(pattern, path, preset, partial)| case_json(pattern, path, preset, partial))
        .collect();
    let recorded = ask_minimatch(&node_path, &cases, EXPANSIONS);
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
        minimatch: minimatch_version(&node_path),
        cases,
        expansions,
    }
    .write();
}

/// A tiny deterministic generator: xorshift over pattern items and short names.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[(self.next() as usize) % items.len()]
    }
}

const ATOMS: &[&str] = &[
    "a", "b", "c", "x", "é", ".", ".a", ".*", "*", "?", "[ab]", "[!a]", "[a-c]", "\\*", "x.js",
    "\\|",
];
const KINDS: &[&str] = &["@", "?", "*", "+", "!"];

/// The names every generated pattern meets: dot corners, alternation fodder and the names the
/// pinned cases ask about.
const NAMES: &[&str] = &[
    "",
    "a",
    "b",
    "c",
    "x",
    "y",
    ".",
    "..",
    ".a",
    ".x",
    "a.",
    "aa",
    "ab",
    "ba",
    "aab",
    "aba",
    "abc",
    "abd",
    "ac",
    "axb",
    "bc",
    "xb",
    "xbc",
    "xy",
    "xyz",
    "ababab",
    "a|b",
    "(a)",
    "a.js",
    "a.ts",
    "a.jsx",
    "a.b",
    "readme.md~",
    "secret.pem",
    "certs",
    "xay",
    "xby",
    "xaby",
    "bbc",
    "d",
    "aaay",
];

fn gen_item(rng: &mut Rng, depth: usize, out: &mut String) {
    let roll = rng.next() % 100;
    if depth >= 3 || roll < 62 {
        out.push_str(rng.pick(ATOMS));
        return;
    }
    let kind = rng.pick(KINDS);
    out.push_str(kind);
    out.push('(');
    let alternatives = 1 + (rng.next() as usize) % 2;
    for a in 0..alternatives {
        if a > 0 {
            out.push('|');
        }
        let items = (rng.next() as usize) % 3;
        for _ in 0..=items {
            gen_item(rng, depth + 1, out);
        }
    }
    out.push(')');
}

fn gen_pattern(rng: &mut Rng) -> String {
    let mut out = String::new();
    let items = 1 + (rng.next() as usize) % 4;
    for _ in 0..items {
        gen_item(rng, 0, &mut out);
    }
    out
}

fn gen_name(rng: &mut Rng) -> String {
    let mut out = String::new();
    let len = (rng.next() as usize) % 5;
    for _ in 0..len {
        out.push_str(rng.pick(&["a", "b", "c", "."]));
    }
    out
}

/// Generated patterns against the live minimatch. The port evaluates negations as zero-width
/// checks where the JavaScript runs a lookahead, so beyond the pinned cases this holds the two
/// to each other on 1500 patterns under the default, `dot` and `nocase` presets.
#[test]
#[ignore = "needs node: compares generated patterns against minimatch live"]
fn agrees_with_minimatch_on_generated_patterns() {
    let node_path = node_path();
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut generated: Vec<(String, Preset, Vec<String>)> = Vec::new();
    for _ in 0..1500 {
        let pattern = gen_pattern(&mut rng);
        let preset = [Default, Dot, Nocase][(rng.next() as usize) % 3];
        let mut names: Vec<String> = NAMES.iter().map(|n| n.to_string()).collect();
        names.extend((0..4).map(|_| gen_name(&mut rng)));
        generated.push((pattern, preset, names));
    }
    let cases: Vec<serde_json::Value> = generated
        .iter()
        .flat_map(|(pattern, preset, names)| {
            names
                .iter()
                .map(move |name| case_json(pattern, name, *preset, false))
        })
        .collect();
    let recorded = ask_minimatch(&node_path, &cases, &[]);
    let answers = recorded["cases"].as_array().unwrap();
    assert_eq!(answers.len(), cases.len());
    let mut answers = answers.iter();
    let mut disagreements = Vec::new();
    for (pattern, preset, names) in &generated {
        let mm = Minimatch::new(pattern, preset.options());
        for name in names {
            let theirs = Answer::from_json(&answers.next().unwrap()[4]);
            let mine = match &mm {
                Ok(mm) => mm.is_match(name).map_or(Answer::Error, Answer::Bool),
                Err(_) => Answer::Error,
            };
            if theirs != mine {
                disagreements.push(format!(
                    "{pattern:?} against {name:?} ({}): minimatch {theirs:?}, port {mine:?}",
                    preset.name()
                ));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} of {} answers differ from minimatch {}:\n{}",
        disagreements.len(),
        cases.len(),
        minimatch_version(&node_path),
        disagreements.join("\n")
    );
}
