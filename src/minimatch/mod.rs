//! A port of minimatch 10.2.5, the glob matcher of npm, npm-packlist and ignore-walk, on
//! [`fancy_regex`].
//!
//! Minimatch translates a glob to one regular expression per path segment and matches the
//! segments of a path against them, with `**` spanning any number of segments. This module ports
//! that translation (ast.js and brace-expressions.js), the brace expansion (brace-expansion 5.0.9),
//! the segment matcher (`matchOne`), `matchBase`, the `dot` rules, negation and comments, and pins
//! the port to the JavaScript implementation with a recorded fixture (`tests/minimatch.rs`).
//!
//! Two things differ from the JavaScript on purpose. Budgets: brace expansion stops at
//! [`Options::max_brace_expansions`], [`Options::max_brace_groups`] and
//! [`Options::max_brace_length`] with an error instead of a silent truncation, the regex engine
//! stops at [`Options::backtrack_limit`] steps with an error instead of hanging, and more than
//! [`Options::max_globstar_recursion`] `**` sections is an error where minimatch answers `false`.
//! Characters: the port works on Unicode scalar values where JavaScript counts UTF-16 units, so
//! `?` matches one astral character (`😀`) that JavaScript needs two of; `nocase` folds case with
//! the regex engine's Unicode simple folding, which JavaScript does only for ASCII in this
//! configuration; and the `u`-flag `SyntaxError` a POSIX class beside a literal `-`, `,`, `!`, `#`
//! or space raises in JavaScript is not an error here.
//!
//! Out of scope: Windows paths and `windowsPathsNoEscape`, `preserveMultipleSlashes`,
//! `optimizationLevel` 2, `nocaseMagicOnly`, `makeRe`.

mod ast;
mod braces;
mod class;
mod matcher;

/// minimatch's `braceExpand`: the strings a pattern's braces expand to, in Bash's order, with
/// duplicates; the pattern itself when it holds no brace pair or under `nobrace`.
pub fn brace_expand(pattern: &str, options: Options) -> Result<Vec<String>, Error> {
    braces::brace_expand(pattern, &options)
}

/// minimatch's `escape`: a backslash before every glob magic character (`?*()[]\`), and before
/// `{` and `}` when braces are magical.
pub fn escape(s: &str, magical_braces: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let magic = matches!(c, '?' | '*' | '(' | ')' | '[' | ']' | '\\')
            || (magical_braces && matches!(c, '{' | '}'));
        if magic {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// minimatch's `unescape`: `[x]` around one character (not `/` or `\`) becomes `x` unless a
/// backslash precedes it, then a backslash before any character but `/` goes; `{` and `}` keep
/// their escapes when braces are not magical. Ported as the two global replacements it is, so
/// `[a][b]` becomes `a[b]` here too.
pub fn unescape(s: &str, magical_braces: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let allowed = |c: char| c != '/' && c != '\\' && (magical_braces || (c != '{' && c != '}'));
    let mut pass1 = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\'
            && !class::is_line_terminator(c)
            && chars.get(i + 1) == Some(&'[')
            && chars.get(i + 2).is_some_and(|&x| allowed(x))
            && chars.get(i + 3) == Some(&']')
        {
            pass1.push(c);
            pass1.push(chars[i + 2]);
            i += 4;
            continue;
        }
        if i == 0
            && c == '['
            && chars.get(1).is_some_and(|&x| allowed(x))
            && chars.get(2) == Some(&']')
        {
            pass1.push(chars[1]);
            i += 3;
            continue;
        }
        pass1.push(c);
        i += 1;
    }
    let chars: Vec<char> = pass1.chars().collect();
    let kept = |c: char| c == '/' || (!magical_braces && (c == '{' || c == '}'));
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1).is_some_and(|&x| !kept(x)) {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The longest pattern minimatch accepts (`assertValidPattern`), in characters.
pub const MAX_PATTERN_LENGTH: usize = 64 * 1024;

/// The switches and budgets of a pattern, minimatch's `MinimatchOptions` less the ones out of
/// scope. [`Options::default`] is minimatch's default: case-sensitive, `.`-files hidden from
/// wildcards, braces, extglobs, globstars, negation and comments on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Case-insensitive matching (`nocase`).
    pub nocase: bool,
    /// Wildcards match names starting with `.` (`dot`); `.` and `..` stay unmatched by `*` and
    /// `**` either way.
    pub dot: bool,
    /// A pattern without a slash matches the last segment of the path (`matchBase`).
    pub match_base: bool,
    /// `is_match` answers whether the pattern body matched, ignoring a leading `!`
    /// (`flipNegate`); ignore-walk reads the negation itself.
    pub flip_negate: bool,
    /// No brace expansion (`nobrace`).
    pub nobrace: bool,
    /// No extglob groups `@(…)`, `!(…)`, `?(…)`, `*(…)`, `+(…)` (`noext`).
    pub noext: bool,
    /// `**` is an ordinary `*` (`noglobstar`).
    pub noglobstar: bool,
    /// A leading `!` is a literal (`nonegate`).
    pub nonegate: bool,
    /// A leading `#` is a literal (`nocomment`).
    pub nocomment: bool,
    /// The most strings one pattern may expand to.
    pub max_brace_expansions: usize,
    /// The most brace groups one pattern may hold.
    pub max_brace_groups: usize,
    /// The most characters all expansions of one pattern may total; brace-expansion's own cap.
    pub max_brace_length: usize,
    /// The most backtracking steps the regex engine spends on one segment before it gives up
    /// with [`Error::Backtrack`].
    pub backtrack_limit: usize,
    /// The most `**` sections one pattern may hold; minimatch's `maxGlobstarRecursion`.
    pub max_globstar_recursion: usize,
}

impl Options {
    /// Minimatch's defaults with the budgets of this port.
    pub const DEFAULT: Options = Options {
        nocase: false,
        dot: false,
        match_base: false,
        flip_negate: false,
        nobrace: false,
        noext: false,
        noglobstar: false,
        nonegate: false,
        nocomment: false,
        max_brace_expansions: 10_000,
        max_brace_groups: 100,
        max_brace_length: 4_000_000,
        backtrack_limit: 1_000_000,
        max_globstar_recursion: 200,
    };

    /// The options ignore-walk hands minimatch for an ignore-file rule: `matchBase`, `dot`,
    /// `flipNegate` and `nocase`.
    pub const fn ignore_walk() -> Options {
        Options {
            match_base: true,
            dot: true,
            flip_negate: true,
            nocase: true,
            ..Options::DEFAULT
        }
    }
}

impl Default for Options {
    fn default() -> Options {
        Options::DEFAULT
    }
}

/// Which brace budget a pattern crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BraceLimit {
    /// More expanded strings than [`Options::max_brace_expansions`].
    Expansions(usize),
    /// More brace groups than [`Options::max_brace_groups`].
    Groups(usize),
    /// More characters in the expansions than [`Options::max_brace_length`].
    Length(usize),
}

/// Why a pattern was refused or a match given up. The message starts with the quoted pattern,
/// so a caller matching many rules can report the one at fault as is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Brace expansion crossed a budget.
    Braces { pattern: String, limit: BraceLimit },
    /// The regex engine spent [`Options::backtrack_limit`] steps on one segment.
    Backtrack { pattern: String, limit: usize },
    /// The translated segment did not compile; a bug in the port, or a class the engine rejects.
    Regex { pattern: String, source: String },
    /// More `**` sections than [`Options::max_globstar_recursion`].
    GlobstarRecursion { pattern: String, limit: usize },
    /// Longer than [`MAX_PATTERN_LENGTH`].
    PatternTooLong { len: usize },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Braces { pattern, limit } => {
                write!(f, "{pattern:?}: brace expansion exceeds ")?;
                match limit {
                    BraceLimit::Expansions(n) => write!(f, "{n} expansions"),
                    BraceLimit::Groups(n) => write!(f, "{n} groups"),
                    BraceLimit::Length(n) => write!(f, "{n} characters"),
                }
            }
            Error::Backtrack { pattern, limit } => {
                write!(
                    f,
                    "{pattern:?}: step limit of {limit} reached while matching"
                )
            }
            Error::Regex { pattern, source } => {
                write!(f, "{pattern:?}: the pattern did not compile: {source}")
            }
            Error::GlobstarRecursion { pattern, limit } => {
                write!(f, "{pattern:?}: more than {limit} globstar sections")
            }
            Error::PatternTooLong { len } => {
                write!(
                    f,
                    "pattern too long: {len} characters, the limit is {MAX_PATTERN_LENGTH}"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// A compiled pattern: minimatch's `Minimatch`.
#[derive(Debug)]
pub struct Minimatch {
    pattern: String,
    options: Options,
    negate: bool,
    comment: bool,
    empty: bool,
    glob_set: Vec<String>,
    glob_parts: Vec<Vec<String>>,
    set: Vec<Vec<matcher::Part>>,
}

impl Minimatch {
    /// Parse a pattern; brace expansion and every segment's regex happen here.
    pub fn new(pattern: &str, options: Options) -> Result<Minimatch, Error> {
        let len = pattern.chars().count();
        if len > MAX_PATTERN_LENGTH {
            return Err(Error::PatternTooLong { len });
        }
        let mut mm = Minimatch {
            pattern: pattern.to_string(),
            options,
            negate: false,
            comment: false,
            empty: false,
            glob_set: Vec::new(),
            glob_parts: Vec::new(),
            set: Vec::new(),
        };
        mm.make()?;
        Ok(mm)
    }

    /// `make`.
    fn make(&mut self) -> Result<(), Error> {
        // Empty patterns and comments match nothing.
        if !self.options.nocomment && self.pattern.starts_with('#') {
            self.comment = true;
            return Ok(());
        }
        if self.pattern.is_empty() {
            self.empty = true;
            return Ok(());
        }
        // Step 1: negation. Step 2: braces, deduplicated in order.
        let glob = self.parse_negate();
        let mut seen = std::collections::HashSet::new();
        self.glob_set = braces::brace_expand(&glob, &self.options)?
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .collect();
        // Step 3: each one becomes a series of segment matchers.
        let raw: Vec<Vec<String>> = self.glob_set.iter().map(|s| slash_split(s)).collect();
        self.glob_parts = self.preprocess(raw);
        let mut set = Vec::with_capacity(self.glob_parts.len());
        for parts in &self.glob_parts {
            let mut pattern = Vec::with_capacity(parts.len());
            for segment in parts {
                pattern.push(self.parse(segment)?);
            }
            set.push(pattern);
        }
        self.set = set;
        Ok(())
    }

    /// `parseNegate`: leading `!`s toggle negation and leave the pattern.
    fn parse_negate(&mut self) -> String {
        if self.options.nonegate {
            return self.pattern.clone();
        }
        let mut negate = false;
        let mut offset = 0;
        for c in self.pattern.chars() {
            if c != '!' {
                break;
            }
            negate = !negate;
            offset += 1;
        }
        self.negate = negate;
        self.pattern.chars().skip(offset).collect()
    }

    /// `preprocess` at optimization level 1: `**` becomes `*` under `noglobstar`, adjacent
    /// `**` collapse, and a `..` after a plain segment removes both.
    fn preprocess(&self, glob_parts: Vec<Vec<String>>) -> Vec<Vec<String>> {
        glob_parts
            .into_iter()
            .map(|parts| {
                let mut set: Vec<String> = Vec::with_capacity(parts.len());
                for mut part in parts {
                    if self.options.noglobstar && part == "**" {
                        part = "*".to_string();
                    }
                    let prev = set.last().map(String::as_str);
                    if part == "**" && prev == Some("**") {
                        continue;
                    }
                    if part == ".."
                        && prev.is_some_and(|p| !p.is_empty() && p != ".." && p != "." && p != "**")
                    {
                        set.pop();
                        continue;
                    }
                    set.push(part);
                }
                if set.is_empty() {
                    vec![String::new()]
                } else {
                    set
                }
            })
            .collect()
    }

    /// `parse`: one segment as a part.
    fn parse(&self, segment: &str) -> Result<matcher::Part, Error> {
        if segment == "**" {
            return Ok(matcher::Part::GlobStar);
        }
        if segment.is_empty() {
            return Ok(matcher::Part::Literal(String::new()));
        }
        let fast = matcher::FastTest::of(segment, &self.options);
        let mm = ast::Ast::from_glob(segment, self.options).into_mm_pattern(&self.pattern)?;
        Ok(matcher::Part::from_pattern(mm, fast))
    }

    /// `match(f)`: whether the path matches, negation applied unless `flip_negate`.
    pub fn is_match(&self, path: &str) -> Result<bool, Error> {
        self.matches(path, false)
    }

    /// `match(f, true)`: partial mode, where a path that ends before the pattern does still
    /// matches, for deciding whether to descend into a directory.
    pub fn is_match_partial(&self, path: &str) -> Result<bool, Error> {
        self.matches(path, true)
    }

    fn matches(&self, path: &str, partial: bool) -> Result<bool, Error> {
        if self.comment {
            return Ok(false);
        }
        if self.empty {
            return Ok(path.is_empty());
        }
        if path == "/" && partial {
            return Ok(true);
        }
        let ff = slash_split(path);
        // The basename: the last non-empty segment.
        let filename = ff
            .iter()
            .rev()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default();
        let options = &self.options;
        for pattern in &self.set {
            let file: &[String] = if options.match_base && pattern.len() == 1 {
                std::slice::from_ref(&filename)
            } else {
                &ff
            };
            let hit =
                matcher::match_one(file, pattern, partial, options).map_err(
                    |fault| match fault {
                        matcher::Fault::Backtrack => Error::Backtrack {
                            pattern: self.pattern.clone(),
                            limit: options.backtrack_limit,
                        },
                        matcher::Fault::GlobstarRecursion => Error::GlobstarRecursion {
                            pattern: self.pattern.clone(),
                            limit: options.max_globstar_recursion,
                        },
                        matcher::Fault::Engine(source) => Error::Regex {
                            pattern: self.pattern.clone(),
                            source,
                        },
                    },
                )?;
            if hit {
                return Ok(options.flip_negate || !self.negate);
            }
        }
        // No hit: success for a negated pattern, failure otherwise.
        Ok(!options.flip_negate && self.negate)
    }

    /// The pattern as given.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    /// A leading `!` negated the pattern.
    pub fn negate(&self) -> bool {
        self.negate
    }

    /// A leading `#` made it a comment, which matches nothing.
    pub fn comment(&self) -> bool {
        self.comment
    }

    /// The empty pattern, which matches only the empty path.
    pub fn empty(&self) -> bool {
        self.empty
    }

    /// Whether any segment needs more than a literal comparison.
    pub fn has_magic(&self) -> bool {
        self.set.iter().any(|pattern| {
            pattern
                .iter()
                .any(|p| !matches!(p, matcher::Part::Literal(_)))
        })
    }

    /// The brace expansions of the pattern, negation stripped, in order, deduplicated.
    pub fn glob_set(&self) -> &[String] {
        &self.glob_set
    }

    /// Each expansion split on `/` and simplified: what ignore-walk reads to tell a relative
    /// rule from an anchored one.
    pub fn glob_parts(&self) -> &[Vec<String>] {
        &self.glob_parts
    }
}

/// `slashSplit`: runs of `/` coalesce into one separator.
fn slash_split(p: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut in_run = false;
    for c in p.chars() {
        if c == '/' {
            if !in_run {
                parts.push(String::new());
                in_run = true;
            }
        } else {
            in_run = false;
            if let Some(last) = parts.last_mut() {
                last.push(c);
            }
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::{BraceLimit, Error, Minimatch, Options};

    fn rule(line: &str) -> Minimatch {
        Minimatch::new(line, Options::ignore_walk()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn hit(line: &str, path: &str) -> bool {
        rule(line).is_match(path).unwrap_or_else(|e| panic!("{e}"))
    }

    fn partial(line: &str, path: &str) -> bool {
        rule(line)
            .is_match_partial(path)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn slashless_patterns_match_the_basename_at_any_depth() {
        assert!(hit("node_modules", "/node_modules"));
        assert!(hit("node_modules", "node_modules"));
        assert!(hit("node_modules", "a/b/node_modules"));
        assert!(!hit("node_modules", "a/node_modules_x"));
        assert!(hit("*.orig", "deep/er/file.orig"));
        assert!(hit(".npmrc", "/.npmrc"));
    }

    #[test]
    fn anchored_patterns_match_the_walkers_own_level_only() {
        assert!(hit("/.git", "/.git"));
        assert!(!hit("/.git", ".git"));
        assert!(!hit("/.git", "/sub/.git"));
        assert!(hit("/build/config.gypi", "/build/config.gypi"));
    }

    #[test]
    fn negation_is_a_flag_and_the_case_is_folded() {
        let r = rule("!/readme{,.*[^~$]}");
        assert!(r.negate());
        assert!(r.is_match("/README").unwrap());
        assert!(r.is_match("/README.md").unwrap());
        assert!(r.is_match("/Readme.txt").unwrap());
        assert!(!r.is_match("/README.md~").unwrap());
        assert!(!r.is_match("/README.md$").unwrap());
        assert!(!r.is_match("/readme-first.md").unwrap());
        assert!(!rule("!!foo").negate());
        assert_eq!(rule("!!foo").glob_set(), ["foo"]);
    }

    #[test]
    fn globstar_and_partials() {
        assert!(hit("**/.git/**", "/.git/HEAD"));
        assert!(hit("**/.git/**", "a/b/.git/objects/x"));
        assert!(!hit("**/.git/**", "a/.gitignore"));
        assert!(hit("!dist/**", "dist/index.js"));
        assert!(hit("!dist/**", "dist/nested/x.d.ts"));
        assert!(!hit("!dist/**", "dist"));
        assert!(hit("!dist/**", "dist/"));
        assert!(hit("!lib/*.js", "lib/a.js"));
        assert!(!hit("!lib/*.js", "lib/deep/a.js"));
        assert!(
            partial("!lib/*.js", "lib"),
            "a directory on the way to a match"
        );
        assert!(
            !partial("!lib/*.js", "lib/"),
            "the walker asks without the slash"
        );
        assert!(!partial("!lib/*.js", "src"));
    }

    #[test]
    fn wildcards_never_take_the_dot_directories() {
        assert!(hit("*", ".hidden"));
        assert!(!hit("*", "."));
        assert!(!hit("*", ".."));
        assert!(
            !hit("*", ""),
            "a lone star needs one character, as minimatch has it"
        );
        assert!(hit("?ile", "file"));
        assert!(!hit("?ile", "fille"));
        assert!(hit("[a-c]x", "Bx"));
        assert!(!hit("[!a-c]x", "bx"));
        assert!(hit("\\*literal", "*literal"));
        assert!(!hit("\\*literal", "xliteral"));
        assert!(hit("[[:digit:]]*.log", "1abc.log"));
        assert!(!hit("[[:digit:]]*.log", "abc.log"));
        assert!(hit("[[:alpha:]-]x", "-x"));
    }

    #[test]
    fn extglobs_match_like_minimatch() {
        assert!(hit("*.@(pem|key)", "secret.pem"));
        assert!(hit("*.@(pem|key)", "certs/secret.key"));
        assert!(!hit("*.@(pem|key)", "secret.pemx"));
        assert!(!hit("*.@(pem|key)", "secret.txt"));
        // A leading `!` is the negation flag even before `(`, as in minimatch: the rule then
        // matches a literal `(…)`.
        let not_js = rule("!(*.js)");
        assert!(not_js.negate());
        assert!(!not_js.is_match("a.ts").unwrap());
        assert!(!not_js.is_match("a.js").unwrap());
        assert!(not_js.is_match("(a.js)").unwrap());
        assert!(hit("a.!(js)", "a.ts"));
        assert!(!hit("a.!(js)", "a.js"));
        assert!(hit("a.!(js)", "a.jsx"));
        assert!(!hit("x!(a|b)", "xa"));
        assert!(hit("x!(a|b)", "xc"));
        assert!(hit("x!(a|b)", "x"));
        assert!(hit("+(ab)", "ab"));
        assert!(hit("+(ab)", "ababab"));
        assert!(!hit("+(ab)", ""));
        assert!(!hit("+(ab)", "aba"));
        assert!(hit("*(a|b)c", "c"));
        assert!(hit("*(a|b)c", "abbac"));
        assert!(!hit("*(a|b)c", "abd"));
        assert!(hit("?(x)y", "y"));
        assert!(hit("?(x)y", "xy"));
        assert!(!hit("?(x)y", "xxy"));
        assert!(hit("@(a|b)c", "bc"));
        assert!(!hit("@(a|b)c", "c"));
        assert!(hit("@(a|@(b|c))", "c"), "nested groups");
        assert!(hit("x*(", "x*("), "an unclosed group is literal");
        assert!(!hit("@(a|b)", "."));
    }

    #[test]
    fn brace_sets_and_glob_parts() {
        assert_eq!(rule("a{b,c}d").glob_set(), ["abd", "acd"]);
        assert_eq!(rule("x{,.y}").glob_set(), ["x", "x.y"]);
        assert_eq!(rule("a{,}").glob_set(), ["a"], "the set is deduplicated");
        assert_eq!(
            rule("{foo,bar/baz}").glob_parts(),
            [
                vec!["foo".to_string()],
                vec!["bar".to_string(), "baz".to_string()]
            ]
        );
        assert_eq!(
            rule("foo/").glob_parts(),
            [vec!["foo".to_string(), String::new()]]
        );
        assert_eq!(
            rule("a//b").glob_parts(),
            [vec!["a".to_string(), "b".to_string()]]
        );
        assert_eq!(rule("a/../b").glob_parts(), [vec!["b".to_string()]]);
        assert_eq!(
            rule("**/**/a").glob_parts(),
            [vec!["**".to_string(), "a".to_string()]]
        );
        assert!(rule("*").has_magic());
        assert!(
            rule("a").has_magic(),
            "under nocase a cased letter needs the engine"
        );
        assert!(!rule("1").has_magic());
        assert!(!Minimatch::new("a", Options::DEFAULT).unwrap().has_magic());
        assert!(rule("#x").comment());
        assert!(rule("").empty());
    }

    #[test]
    fn a_brace_bomb_is_an_error_not_an_allocation() {
        // `{1..100000000}` names a hundred million entries; the count is arithmetic, so the
        // error comes back before anything is allocated.
        let new = |p: &str| Minimatch::new(p, Options::ignore_walk());
        assert!(new("{1..100000000}").is_err());
        assert!(new("{1..10001}").is_err());
        assert!(new("{1..10000}").is_ok());
        assert!(new("{9999..1}").is_ok());
        // Multiplicative groups are capped the same way: 3^16 alternatives.
        assert!(new(&"{a,b,c}".repeat(16)).is_err());
        assert!(new(&"{a,b,c}".repeat(4)).is_ok());
        // The group budget bounds the expansion and its recursion depth.
        assert!(new(&"{a,b}".repeat(101)).is_err());
        // A bound beyond i64 is over any budget.
        assert_eq!(
            new("{0..18446744073709551616}").unwrap_err(),
            Error::Braces {
                pattern: "{0..18446744073709551616}".into(),
                limit: BraceLimit::Expansions(10_000)
            }
        );
        // The error names the offending pattern.
        let error = new("dist/{1..100000000}.tgz").unwrap_err().to_string();
        assert!(error.starts_with("\"dist/{1..100000000}.tgz\""), "{error}");
        assert!(error.contains("brace expansion"), "{error}");
    }

    #[test]
    fn the_old_backtracking_bombs_run_in_linear_time() {
        // Against a 200-char name these needed ~1e13 steps in a backtracking matcher; on the
        // engine the pattern body runs on regex-automata and answers at once.
        let long = "b".repeat(200);
        let started = std::time::Instant::now();
        assert!(!hit("*b*b*b*b*b*b*b*c", &long));
        assert!(!hit("+(b|bb)+(b|bb)+(b|bb)+(b|bb)c", &long));
        assert!(hit("*b*b*b*b*b*b*b*", &long));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_pathological_pattern_is_an_error_not_a_hang() {
        // A negated group under a repeat keeps the backtracking engine busy; the step budget
        // errors in well under a second where minimatch hangs.
        let long = "a".repeat(200);
        let started = std::time::Instant::now();
        for pattern in ["*(!(a))y", "+(!(a)|b)c"] {
            let error = rule(pattern).is_match(&long).unwrap_err();
            assert_eq!(
                error,
                Error::Backtrack {
                    pattern: pattern.into(),
                    limit: 1_000_000
                }
            );
            let message = error.to_string();
            assert!(message.starts_with(&format!("{pattern:?}")), "{message}");
            assert!(message.contains("step limit"), "{message}");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        // Stock rules sit far under the budget.
        for fine in [
            "!/readme{,.*[^~$]}",
            "**/node_modules/**",
            "*.@(pem|key)",
            "lib/*.js",
        ] {
            assert!(
                rule(fine).is_match("deep/er/path/README.md").is_ok(),
                "{fine}"
            );
        }
        // A nasty-but-legal pattern still evaluates correctly.
        let xs = "x".repeat(40);
        assert!(hit("*x*x*x*", &xs));
        assert!(!hit("*x*x*x*y", &xs));
    }

    #[test]
    fn errors_name_the_pattern_first() {
        let error = Error::Backtrack {
            pattern: "a".into(),
            limit: 5,
        };
        assert_eq!(
            error.to_string(),
            "\"a\": step limit of 5 reached while matching"
        );
        let error = Error::GlobstarRecursion {
            pattern: "a".into(),
            limit: 200,
        };
        assert_eq!(error.to_string(), "\"a\": more than 200 globstar sections");
        assert!(Minimatch::new(&"a".repeat(65_537), Options::DEFAULT).is_err());
    }

    #[test]
    fn escape_and_unescape() {
        use super::{escape, unescape};
        assert_eq!(
            escape("a*b?[c]\\(d){e}", false),
            "a\\*b\\?\\[c\\]\\\\\\(d\\){e}"
        );
        assert_eq!(escape("{a}", true), "\\{a\\}");
        assert_eq!(unescape("\\*", true), "*");
        assert_eq!(unescape("[*]", true), "*");
        assert_eq!(unescape("a[b]c", true), "abc");
        assert_eq!(unescape("[a][b]", true), "a[b]");
        assert_eq!(unescape("\\[a]", true), "[a]");
        assert_eq!(unescape("a\\/b", true), "a\\/b");
        assert_eq!(unescape("\\{a\\}", true), "{a}");
        assert_eq!(unescape("\\{a\\}", false), "\\{a\\}");
        assert_eq!(unescape("[{]", false), "[{]");
        assert_eq!(unescape("[[a]", true), "[a");
    }
}
