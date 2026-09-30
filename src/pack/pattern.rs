//! Ignore rules and `files` globs as [`Minimatch`] patterns under the options npm-packlist and
//! ignore-walk hand minimatch: an ignore rule with `matchBase`, `dot`, `flipNegate` and
//! `nocase`, a `files` entry over the whole path, case kept, `dot` on, no negation or comment
//! prefix; minimatch's quirks off in the strict mode, the packer's default, or kept as npm
//! reads them. An error from the matcher names the rule and its kind.

use crate::minimatch::{Minimatch, Options};
use crate::Result;

/// One compiled rule line.
#[derive(Debug)]
pub(crate) struct Rule {
    /// The `!` prefix: a match means *include* (ignore-walk's `flipNegate`).
    pub(crate) negate: bool,
    /// `ignore rule` or `files entry`: the prefix of an error naming the rule.
    kind: &'static str,
    mm: Minimatch,
}

impl Rule {
    /// Compile one rule line; `None` for a blank line or a `#` comment. A line past a brace
    /// budget, or one the strict mode refuses, is an error naming it; treating it as a literal
    /// could ship files an exclusion was written to hide.
    pub(crate) fn parse(line: &str, quirks: bool) -> Result<Option<Rule>> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }
        let options = Options {
            quirks,
            ..Options::ignore_walk()
        };
        let mm = Minimatch::new(line, options).map_err(|e| format!("ignore rule {e}"))?;
        Ok(Some(Rule {
            negate: mm.negate(),
            kind: "ignore rule",
            mm,
        }))
    }

    /// Compile a `files` entry the way npm-packlist 11 hands it to glob: matched against the
    /// whole path from the package root, case-sensitively, `dot` on, no negation prefix (the
    /// caller stripped it), a `#` pattern being a literal like any other. The budgets and the
    /// mode apply as for a rule.
    pub(crate) fn parse_glob(pattern: &str, quirks: bool) -> Result<Option<Rule>> {
        if pattern.is_empty() {
            return Ok(None);
        }
        let options = Options {
            dot: true,
            nonegate: true,
            nocomment: true,
            quirks,
            ..Options::DEFAULT
        };
        let mm = Minimatch::new(pattern, options).map_err(|e| format!("files entry {e}"))?;
        Ok(Some(Rule {
            negate: false,
            kind: "files entry",
            mm,
        }))
    }

    /// ignore-walk's "relative rule": some alternative is a single segment (`foo`) or one with a
    /// trailing slash (`foo/`), so it also applies to a bare basename further down the tree.
    pub(crate) fn is_relative(&self) -> bool {
        self.mm.glob_parts().iter().any(|parts| {
            let trailing_slash = parts.last().is_some_and(String::is_empty);
            parts.len() <= if trailing_slash { 2 } else { 1 }
        })
    }

    /// minimatch's `match(path, partial)` under the rule's options: `path` splits on `/` (a
    /// leading slash makes an empty first segment, which only an anchored pattern matches), a
    /// slash-less alternative matches the last non-empty segment when `matchBase` is on, and
    /// `partial` accepts a path that is a prefix of a possible match. An evaluation past the
    /// engine's step budget is an error naming the rule.
    pub(crate) fn matches(&self, path: &str, partial: bool) -> Result<bool> {
        let hit = if partial {
            self.mm.is_match_partial(path)
        } else {
            self.mm.is_match(path)
        };
        hit.map_err(|e| format!("{} {e}", self.kind).into())
    }
}

#[cfg(test)]
mod tests {
    use super::Rule;

    fn rule(line: &str) -> Rule {
        Rule::parse(line, true)
            .expect("a valid line")
            .expect("a rule")
    }

    #[test]
    fn blank_and_comment_lines_are_no_rules() {
        assert!(Rule::parse("", true).unwrap().is_none());
        assert!(Rule::parse("   ", true).unwrap().is_none());
        assert!(Rule::parse("# note", true).unwrap().is_none());
        assert!(Rule::parse_glob("", true).unwrap().is_none());
        // A `files` entry has no comment or negation prefix.
        let hash = Rule::parse_glob("#x", true).unwrap().unwrap();
        assert!(hash.matches("#x", false).unwrap());
        let bang = Rule::parse_glob("!x", true).unwrap().unwrap();
        assert!(!bang.negate);
        assert!(bang.matches("!x", false).unwrap());
    }

    #[test]
    fn a_rule_folds_case_at_any_depth_and_a_glob_does_not() {
        assert!(rule("node_modules")
            .matches("a/b/NODE_MODULES", false)
            .unwrap());
        assert!(rule("!/readme{,.*[^~$]}").negate);
        let glob = Rule::parse_glob("lib/*.js", true).unwrap().unwrap();
        assert!(glob.matches("lib/a.js", false).unwrap());
        assert!(!glob.matches("lib/A.JS", false).unwrap());
        assert!(!glob.matches("x/lib/a.js", false).unwrap());
        assert!(glob.matches("lib", true).unwrap());
        let dotted = Rule::parse_glob("*", true).unwrap().unwrap();
        assert!(dotted.matches(".hidden", false).unwrap());
    }

    #[test]
    fn relative_rules_are_recognized() {
        assert!(rule("foo").is_relative());
        assert!(rule("foo/").is_relative());
        assert!(!rule("foo/bar").is_relative());
        assert!(rule("{foo,bar/baz}").is_relative());
        assert!(!rule("/foo").is_relative());
    }

    #[test]
    fn errors_name_the_rule_and_its_kind() {
        let error = Rule::parse("dist/{1..100000000}.tgz", true)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("ignore rule \"dist/{1..100000000}.tgz\": brace expansion"),
            "{error}"
        );
        let error = Rule::parse_glob(
            "{a,b,c}{a,b,c}{a,b,c}{a,b,c}{a,b,c}{a,b,c}{a,b,c}{a,b,c}{a,b,c}",
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("files entry \"{a,b,c}"), "{error}");
        assert!(error.contains("brace expansion"), "{error}");
        let error = rule("*(!(a))y")
            .matches(&"a".repeat(200), false)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("ignore rule \"*(!(a))y\": step limit"),
            "{error}"
        );
    }

    #[test]
    fn the_strict_mode_refuses_by_name() {
        // npm: negation of the literal `(dist)`, a rule that matches nothing.
        let npm = Rule::parse("!(dist)", true).unwrap().unwrap();
        assert!(npm.negate);
        assert!(!npm.matches("dist", false).unwrap());
        let error = Rule::parse("!(dist)", false).unwrap_err().to_string();
        assert!(
            error.starts_with("ignore rule \"!(dist)\": a leading `!(`"),
            "{error}"
        );
        let error = Rule::parse_glob("lib/[a", false).unwrap_err().to_string();
        assert!(
            error.starts_with("files entry \"lib/[a\": the class"),
            "{error}"
        );
        // A plain rule reads the same in both modes.
        for quirks in [true, false] {
            let rule = Rule::parse("!/readme{,.*[^~$]}", quirks).unwrap().unwrap();
            assert!(rule.matches("/README.md", false).unwrap());
        }
    }
}
