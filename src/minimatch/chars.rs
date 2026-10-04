//! The character sets of minimatch's grammar, each named after the JavaScript it ports. A set is
//! written once, here, and every test of membership reads it from here, so the escape rules of
//! the port cannot drift apart between the parser, the class translator and the escapers.

/// `escape` (escape.js): the glob magic characters a backslash protects, `/[?*()[\]\\]/g`.
pub(super) const GLOB_MAGIC: &str = "?*()[]\\";

/// `escape` with `magicalBraces`: the braces join [`GLOB_MAGIC`].
pub(super) const BRACES: &str = "{}";

/// `reSpecials` (ast.js): the characters an escape keeps escaped in the regex source; a
/// backslash before any other character goes, the character stays literal.
pub(super) const RE_SPECIALS: &str = "().*{}+?[]^$\\!";

/// `regExpEscape` (ast.js and brace-expressions.js): `/[-[\]{}()*+?.,\\^$|#\s]/g`. JavaScript's
/// `\s` also covers the non-ASCII spaces, which the regex crate cannot escape and need not,
/// since such a character is a literal in a regex as it is.
pub(super) const REGEXP_ESCAPED: &str = "-[]{}()*+?.,\\^$|# \t\n\r\x0b\x0c";

/// `braceEscape` (brace-expressions.js): `/[[\]\\-]/g`, plus `&` and `~`, which the regex crate
/// reads as set operations inside a class (`&&`, `~~`) where JavaScript does not.
pub(super) const CLASS_ESCAPED: &str = "[]\\-&~";

/// `types` (ast.js): the extglob kinds, each opening a group with `(`.
pub(super) const EXT_KINDS: &str = "!?+*@";

/// `starDotExtRE`'s exclusion (minimatch.js): a segment holding any of these takes no fast path.
pub(super) const FAST_PATH_BREAKERS: &str = "+@!?*[(";

/// Whether `c` opens an extglob group when a `(` follows.
pub(super) fn is_ext_kind(c: char) -> bool {
    EXT_KINDS.contains(c)
}

/// `regExpEscape` for one character, appended to `out`.
pub(super) fn regexp_escape(c: char, out: &mut String) {
    if REGEXP_ESCAPED.contains(c) {
        out.push('\\');
    }
    out.push(c);
}

/// `braceEscape` for one character: how it is written inside a class.
pub(super) fn class_escape(c: char) -> String {
    if CLASS_ESCAPED.contains(c) {
        format!("\\{c}")
    } else {
        c.to_string()
    }
}

/// JavaScript's `.` without the `s` flag stops at these.
pub(super) fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// The two names no wildcard matches: `.` and `..`.
pub(super) fn is_dots(s: &str) -> bool {
    s == "." || s == ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOLS: &[(&str, &str)] = &[
        ("GLOB_MAGIC", GLOB_MAGIC),
        ("BRACES", BRACES),
        ("RE_SPECIALS", RE_SPECIALS),
        ("REGEXP_ESCAPED", REGEXP_ESCAPED),
        ("CLASS_ESCAPED", CLASS_ESCAPED),
        ("EXT_KINDS", EXT_KINDS),
        ("FAST_PATH_BREAKERS", FAST_PATH_BREAKERS),
    ];

    #[test]
    fn no_pool_repeats_a_character() {
        for (name, pool) in POOLS {
            let mut seen = std::collections::HashSet::new();
            for c in pool.chars() {
                assert!(seen.insert(c), "{name} lists {c:?} twice");
            }
        }
    }

    #[test]
    fn what_escape_protects_stays_escaped_in_the_regex() {
        // `escape` output reaches `parse_glob`, which keeps a backslash only before a special.
        for c in GLOB_MAGIC.chars().chain(BRACES.chars()) {
            assert!(
                RE_SPECIALS.contains(c),
                "{c:?} is glob magic but no regex special"
            );
        }
    }

    #[test]
    fn the_class_escapes_are_regex_escapes_too() {
        for c in CLASS_ESCAPED.chars().filter(|c| !matches!(c, '&' | '~')) {
            assert!(REGEXP_ESCAPED.contains(c), "{c:?}");
        }
    }

    #[test]
    fn every_regexp_escape_compiles_and_matches_its_character() {
        for c in REGEXP_ESCAPED.chars() {
            let mut src = String::from("^");
            regexp_escape(c, &mut src);
            src.push('$');
            let re = regex::Regex::new(&src).unwrap_or_else(|e| panic!("{src}: {e}"));
            assert!(re.is_match(&c.to_string()), "{src} must match {c:?}");
            assert!(!re.is_match("x"), "{src} must not match x");
        }
    }

    proptest::proptest! {
        /// Any character, escaped as a run, is a regex matching exactly that character.
        #[test]
        fn any_character_escapes_to_a_regex_matching_itself(c in proptest::prelude::any::<char>()) {
            let mut src = String::from("^");
            regexp_escape(c, &mut src);
            src.push('$');
            let re = regex::Regex::new(&src).map_err(|e| proptest::test_runner::TestCaseError::fail(e.to_string()))?;
            proptest::prop_assert!(re.is_match(&c.to_string()), "{src} must match {c:?}");
            let other = if c == 'x' { 'y' } else { 'x' };
            proptest::prop_assert!(!re.is_match(&other.to_string()), "{src} must not match {other:?}");
        }

        /// Any character, escaped as a class member, compiles and is matched by the class.
        #[test]
        fn any_character_fits_in_a_class(c in proptest::prelude::any::<char>()) {
            let src = format!("^[a{}]$", class_escape(c));
            let re = regex::Regex::new(&src).map_err(|e| proptest::test_runner::TestCaseError::fail(e.to_string()))?;
            proptest::prop_assert!(re.is_match(&c.to_string()), "{src} must match {c:?}");
            proptest::prop_assert!(re.is_match("a"), "{src} must match a");
        }
    }

    #[test]
    fn every_class_escape_compiles_inside_a_class() {
        for c in CLASS_ESCAPED.chars() {
            let src = format!("^[{}]$", class_escape(c));
            let re = regex::Regex::new(&src).unwrap_or_else(|e| panic!("{src}: {e}"));
            assert!(re.is_match(&c.to_string()), "{src} must match {c:?}");
        }
    }
}
