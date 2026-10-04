//! minimatch's quirks by name: what npm does, and what the port does instead when
//! [`Options::quirks`](super::Options::quirks) is off, the strict mode. The matcher keeps every
//! quirk by default, so a pattern means what it means to npm; `pack` runs strict by default and
//! keeps them under `--npm-quirks`. The strict mode refuses by name what minimatch guesses at,
//! and honours escapes everywhere.

use super::Error;

/// One behaviour of minimatch 10.2.5 the port reproduces by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Quirk {
    /// A leading `!` before `(` is negation: `!(a|b)` negates a literal `(a|b)`, which no path
    /// matches. Strict: refused; `!@(a|b)` negates a group match, `@(!(a|b))` is the group.
    NegationBeforeGroup,
    /// `*<ext>` and `?<ext>` compare the raw extension text, so `*\.js` matches `a\.js` and not
    /// `a.js`, and under `dot` the comparison takes `.` and `..` (`*.` matches `..`), which no
    /// wildcard does. Strict: the regex applies, the escape is honoured, `.` and `..` stay out.
    RawExtensionFastPath,
    /// With other magic in the pattern, `\|` reaches the regex as a bare `|`, an alternation:
    /// `a*\|b` matches `a` and `xb`. Strict: a literal `|`.
    EscapedPipeAlternates,
    /// Brace expansion strips the escapes it knows (`\\`, `\{`, `\}`, `\,`, `\.`), but only when
    /// the pattern holds a `{…}` pair, so `a\\*` and `a\\*{b,c}` read the backslashes
    /// differently. Strict: the escapes survive expansion.
    BracesStripEscapes,
    /// `[[:print:]]` translates to `\p{C}`, the control and unassigned characters, the opposite
    /// of printable. Strict: everything but `\p{C}` and the line and paragraph separators.
    PosixPrintIsControl,
    /// `[[:punct:]]` translates to `\p{P}` alone, so `$`, `+`, `<`, `=`, `>`, `^`, `` ` ``, `|`
    /// and `~` are no punctuation. Strict: `\p{P}\p{S}`.
    PosixPunctSkipsSymbols,
    /// A class that can match nothing (`[z-a]`, `[a-[:alpha:]]`) becomes a regex that never
    /// matches, so the segment silently matches nothing. Strict: refused.
    UnmatchableClassPoisons,
    /// An unclosed `[` is a literal `[`. Strict: refused.
    UnclosedClassIsLiteral,
    /// An unclosed group (`x*(`) is literal text. Strict: refused.
    UnclosedGroupIsLiteral,
    /// `[[:nope:]]` is a class of `[`, `:`, `n`, `o`, `p`, `e` and `:`, then a literal `]`.
    /// Strict: refused.
    UnknownPosixClassIsLiteral,
}

impl Quirk {
    /// Every quirk, in the order of the enum.
    pub const ALL: &'static [Quirk] = &[
        Quirk::NegationBeforeGroup,
        Quirk::RawExtensionFastPath,
        Quirk::EscapedPipeAlternates,
        Quirk::BracesStripEscapes,
        Quirk::PosixPrintIsControl,
        Quirk::PosixPunctSkipsSymbols,
        Quirk::UnmatchableClassPoisons,
        Quirk::UnclosedClassIsLiteral,
        Quirk::UnclosedGroupIsLiteral,
        Quirk::UnknownPosixClassIsLiteral,
    ];

    /// The kebab-case name, as error messages cite it.
    pub const fn name(self) -> &'static str {
        match self {
            Quirk::NegationBeforeGroup => "negation-before-group",
            Quirk::RawExtensionFastPath => "raw-extension-fast-path",
            Quirk::EscapedPipeAlternates => "escaped-pipe-alternates",
            Quirk::BracesStripEscapes => "braces-strip-escapes",
            Quirk::PosixPrintIsControl => "posix-print-is-control",
            Quirk::PosixPunctSkipsSymbols => "posix-punct-skips-symbols",
            Quirk::UnmatchableClassPoisons => "unmatchable-class-poisons",
            Quirk::UnclosedClassIsLiteral => "unclosed-class-is-literal",
            Quirk::UnclosedGroupIsLiteral => "unclosed-group-is-literal",
            Quirk::UnknownPosixClassIsLiteral => "unknown-posix-class-is-literal",
        }
    }
}

impl std::fmt::Display for Quirk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What the strict mode refused, before the pattern it belongs to is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Refusal {
    pub quirk: Quirk,
    pub reason: String,
}

impl Refusal {
    pub(super) fn new(quirk: Quirk, reason: impl Into<String>) -> Refusal {
        Refusal {
            quirk,
            reason: reason.into(),
        }
    }

    pub(super) fn into_error(self, pattern: &str) -> Error {
        Error::Refused {
            pattern: pattern.to_string(),
            quirk: self.quirk,
            reason: self.reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Error, Minimatch, Options};
    use super::Quirk;

    fn npm() -> Options {
        Options::DEFAULT
    }

    fn strict() -> Options {
        Options {
            quirks: false,
            ..Options::DEFAULT
        }
    }

    fn hit(pattern: &str, path: &str, options: Options) -> bool {
        Minimatch::new(pattern, options)
            .unwrap_or_else(|e| panic!("{e}"))
            .is_match(path)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn refused(pattern: &str) -> Quirk {
        match Minimatch::new(pattern, strict()) {
            Err(Error::Refused { quirk, .. }) => quirk,
            other => panic!("{pattern:?}: expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn every_quirk_has_a_distinct_name() {
        let mut names: Vec<&str> = Quirk::ALL.iter().map(|q| q.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Quirk::ALL.len());
        assert_eq!(
            Quirk::NegationBeforeGroup.to_string(),
            "negation-before-group"
        );
    }

    #[test]
    fn negation_before_group() {
        // npm: `!(a|b)` is negation of the literal `(a|b)`, so the rule matches nothing.
        assert!(hit("!(a|b)", "a", npm()));
        assert!(!hit("!(a|b)", "(a|b)", npm()));
        assert_eq!(refused("!(a|b)"), Quirk::NegationBeforeGroup);
        assert_eq!(refused("!!(a)"), Quirk::NegationBeforeGroup);
        // The unambiguous spellings work in both modes.
        assert!(!hit("!@(a|b)", "a", strict()));
        assert!(hit("!@(a|b)", "c", strict()));
        assert!(!hit("@(!(a|b))", "a", strict()));
        assert!(hit("@(!(a|b))", "c", strict()));
        // Under nonegate a leading `!` is a literal and `!(…)` a group: no ambiguity.
        let nonegate = Options {
            nonegate: true,
            ..strict()
        };
        assert!(hit("!(a)", "b", nonegate));
        assert!(!hit("!(a)", "a", nonegate));
    }

    #[test]
    fn raw_extension_fast_path() {
        assert!(hit("*\\.js", "a\\.js", npm()));
        assert!(!hit("*\\.js", "a.js", npm()));
        // Strict: the regex applies, so `*` also absorbs a backslash before the `.js`.
        assert!(hit("*\\.js", "a.js", strict()));
        assert!(hit("*\\.js", "a\\.js", strict()));
        assert!(hit("?\\.js", "a.js", strict()));
        assert!(!hit("?\\.js", "a\\.js", strict()));
        // Plain extensions read the same either way.
        for options in [npm(), strict()] {
            assert!(hit("*.js", "a.js", options));
            assert!(!hit("*.js", ".a.js", options));
            assert!(hit("??.js", "ab.js", options));
        }
    }

    #[test]
    fn escaped_pipe_alternates() {
        // With magic in the pattern npm's regex reads `\|` as an alternation.
        assert!(hit("a*\\|b", "a", npm()));
        assert!(hit("a*\\|b", "xb", npm()));
        // (`*\|` itself is a fast path, an extension compare; `x*\|` is not.)
        assert!(hit("x*\\|", "anything", npm()));
        assert!(!hit("a*\\|b", "a", strict()));
        assert!(!hit("a*\\|b", "xb", strict()));
        assert!(hit("a*\\|b", "a|b", strict()));
        assert!(hit("a*\\|b", "axx|b", strict()));
        assert!(!hit("x*\\|", "anything", strict()));
        assert!(hit("x*\\|", "x|", strict()));
        // Without magic the pattern is a literal in both modes.
        for options in [npm(), strict()] {
            assert!(hit("a\\|b", "a|b", options));
            assert!(!hit("a\\|b", "a", options));
        }
    }

    #[test]
    fn braces_strip_escapes() {
        // Without a brace pair `\\` is an escaped backslash and `*` a star in both modes.
        for options in [npm(), strict()] {
            assert!(hit("a\\\\*", "a\\xyz", options));
            assert!(!hit("a\\\\*", "a*b", options));
        }
        // With a pair npm turns `\\` into one backslash that then escapes the star.
        assert!(hit("a\\\\*{b,c}", "a*b", npm()));
        assert!(!hit("a\\\\*{b,c}", "a\\xyzb", npm()));
        assert!(hit("a\\\\*{b,c}", "a\\xyzb", strict()));
        assert!(!hit("a\\\\*{b,c}", "a*b", strict()));
        // Escaped braces and commas keep meaning literal text.
        for options in [npm(), strict()] {
            assert!(hit("\\{a,b\\}", "{a,b}", options));
            assert!(hit("{a\\,b,c}", "a,b", options));
            assert!(hit("{a\\,b,c}", "c", options));
            assert!(hit("x{,.y}", "x.y", options));
        }
    }

    #[test]
    fn posix_print_and_punct() {
        assert!(!hit("[[:print:]]", "a", npm()));
        assert!(hit("[[:print:]]", "\u{1}", npm()));
        assert!(hit("[[:print:]]", "a", strict()));
        assert!(hit("[[:print:]]", " ", strict()));
        assert!(!hit("[[:print:]]", "\u{1}", strict()));
        assert!(!hit("[[:print:]]", "\u{2028}", strict()));
        assert!(!hit("[[:punct:]]", "$", npm()));
        assert!(hit("[[:punct:]]", "!", npm()));
        assert!(hit("[[:punct:]]", "$", strict()));
        assert!(hit("[[:punct:]]", "~", strict()));
        assert!(hit("[[:punct:]]", "!", strict()));
        assert!(!hit("[[:punct:]]", "a", strict()));
        // The other classes translate the same either way.
        assert!(hit("[[:alpha:]]", "ß", strict()));
        assert!(hit("[[:digit:]]x", "3x", strict()));
    }

    #[test]
    fn unmatchable_class_poisons() {
        assert!(!hit("[z-a]", "m", npm()));
        assert!(!hit("[z-a]x", "x", npm()));
        assert_eq!(refused("[z-a]"), Quirk::UnmatchableClassPoisons);
        assert_eq!(refused("[a-[:alpha:]]"), Quirk::UnmatchableClassPoisons);
        // A reversed range beside a live one is dropped, not poison, in both modes.
        assert!(hit("[z-ab]", "b", npm()));
        assert!(hit("[z-ab]", "b", strict()));
    }

    #[test]
    fn unclosed_class_and_group() {
        assert!(hit("[a", "[a", npm()));
        assert!(hit("x*(", "x*(", npm()));
        assert_eq!(refused("[a"), Quirk::UnclosedClassIsLiteral);
        assert_eq!(refused("a[b-c"), Quirk::UnclosedClassIsLiteral);
        assert_eq!(refused("x*("), Quirk::UnclosedGroupIsLiteral);
        assert_eq!(refused("@(a|b"), Quirk::UnclosedGroupIsLiteral);
        // Escaped, they are literal in both modes.
        assert!(hit("\\[a", "[a", strict()));
        assert!(hit("x*\\(", "x*(", npm()));
        assert!(hit("x\\*\\(", "x*(", strict()));
    }

    #[test]
    fn unknown_posix_class() {
        assert!(hit("[[:nope:]]", "n]", npm()));
        assert!(!hit("[[:nope:]]", "n", npm()));
        assert_eq!(refused("[[:nope:]]"), Quirk::UnknownPosixClassIsLiteral);
        // A class that merely starts with `[:` is not a name.
        assert!(hit("[[:a]", "[", strict()));
        assert!(hit("[[:a]", ":", strict()));
    }

    #[test]
    fn a_refusal_names_the_pattern_and_the_reason() {
        let error = Minimatch::new("!(dist)", strict()).unwrap_err();
        let message = error.to_string();
        assert!(message.starts_with("\"!(dist)\": "), "{message}");
        assert!(message.contains("negation"), "{message}");
        assert!(message.contains("!@("), "{message}");
    }

    #[test]
    fn the_strict_mode_changes_nothing_else() {
        for (pattern, path) in [
            ("*.js", "a.js"),
            ("**/*.d.ts", "lib/types.d.ts"),
            ("!/readme{,.*[^~$]}", "/README.md"),
            ("*.@(pem|key)", "secret.pem"),
            ("a.!(js)", "a.ts"),
            ("[[:digit:]]*.log", "1abc.log"),
            ("{a,b}/c", "b/c"),
            ("**/.git/**", "a/.git/HEAD"),
        ] {
            let npm_says = Minimatch::new(pattern, Options::ignore_walk())
                .unwrap()
                .is_match(path)
                .unwrap();
            let strict_says = Minimatch::new(
                pattern,
                Options {
                    quirks: false,
                    ..Options::ignore_walk()
                },
            )
            .unwrap()
            .is_match(path)
            .unwrap();
            assert_eq!(npm_says, strict_says, "{pattern:?} against {path:?}");
            assert!(npm_says, "{pattern:?} against {path:?}");
        }
    }
}
