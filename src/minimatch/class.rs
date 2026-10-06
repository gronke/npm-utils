//! Bracket classes: brace-expressions.js ported, the POSIX names translated to Unicode
//! properties as minimatch does, quirks included (`[:print:]` is `\p{C}`, `[:graph:]` its
//! negation, `[:ascii:]` and `[:xdigit:]` plain ranges). The regex crate reads `&&`, `~~` and
//! `--` inside a class as set operations, so the port escapes `&` and `~` there where the
//! JavaScript does not; that is the one difference in the sources. The strict mode corrects
//! `[:print:]` and `[:punct:]` and refuses what can match nothing, never closes or names no
//! POSIX class ([`Quirk`]).

use super::chars::{class_escape, is_line_terminator, regexp_escape};
use super::quirks::{Quirk, Refusal};
use super::Options;

/// One parsed class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Class {
    /// The regex source; `$.` for a class that can match nothing, empty when unclosed.
    pub src: String,
    /// The JavaScript would need its `u` flag: a Unicode property was used.
    pub uflag: bool,
    /// Characters consumed from the `[` on; zero when the class is unclosed and `[` stays literal.
    pub consumed: usize,
    /// Whether the source is more than one literal character.
    pub magic: bool,
}

/// `[:name:]`, its translation, whether it needs the `u` flag, whether it is negated.
const POSIX_CLASSES: &[(&str, &str, bool, bool)] = &[
    ("[:alnum:]", r"\p{L}\p{Nl}\p{Nd}", true, false),
    ("[:alpha:]", r"\p{L}\p{Nl}", true, false),
    ("[:ascii:]", r"\x00-\x7f", false, false),
    ("[:blank:]", r"\p{Zs}\t", true, false),
    ("[:cntrl:]", r"\p{Cc}", true, false),
    ("[:digit:]", r"\p{Nd}", true, false),
    ("[:graph:]", r"\p{Z}\p{C}", true, true),
    ("[:lower:]", r"\p{Ll}", true, false),
    ("[:print:]", r"\p{C}", true, false),
    ("[:punct:]", r"\p{P}", true, false),
    ("[:space:]", r"\p{Z}\t\r\n\v\f", true, false),
    ("[:upper:]", r"\p{Lu}", true, false),
    ("[:word:]", r"\p{L}\p{Nl}\p{Nd}\p{Pc}", true, false),
    ("[:xdigit:]", "A-Fa-f0-9", false, false),
];

fn starts_with(glob: &[char], at: usize, needle: &str) -> bool {
    (at..)
        .zip(needle.chars())
        .all(|(i, n)| glob.get(i) == Some(&n))
}

/// `/^\\?.$/`: one character, or a backslash and one character, neither a line terminator.
fn is_single(range: &str) -> bool {
    let mut chars = range.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(c), None, None) => !is_line_terminator(c),
        (Some('\\'), Some(c), None) => !is_line_terminator(c),
        _ => false,
    }
}

/// The text from `pos` on, shortened, for an error message.
fn excerpt(glob: &[char], pos: usize) -> String {
    let mut text: String = glob[pos..].iter().take(40).collect();
    if glob.len() - pos > 40 {
        text.push('…');
    }
    text
}

/// `parseClass`: the class at `glob[pos]` (a `[`) as a regex source.
pub(super) fn parse_class(glob: &[char], pos: usize, options: &Options) -> Result<Class, Refusal> {
    debug_assert_eq!(glob.get(pos), Some(&'['), "not in a brace expression");
    let poison = || -> Result<Class, Refusal> {
        if !options.keeps(Quirk::UnmatchableClassPoisons) {
            return Err(Refusal::new(
                Quirk::UnmatchableClassPoisons,
                format!("the class `{}` can match nothing", excerpt(glob, pos)),
            ));
        }
        Ok(Class {
            src: "$.".to_string(),
            uflag: false,
            consumed: glob.len() - pos,
            magic: true,
        })
    };
    let mut ranges: Vec<String> = Vec::new();
    let mut negs: Vec<String> = Vec::new();
    let mut i = pos + 1;
    let mut saw_start = false;
    let mut uflag = false;
    let mut escaping = false;
    let mut negate = false;
    let mut end_pos = pos;
    let mut range_start: Option<char> = None;
    'class: while i < glob.len() {
        let c = glob[i];
        if (c == '!' || c == '^') && i == pos + 1 {
            negate = true;
            i += 1;
            continue;
        }
        if c == ']' && saw_start && !escaping {
            end_pos = i + 1;
            break;
        }
        saw_start = true;
        if c == '\\' && !escaping {
            escaping = true;
            i += 1;
            continue;
        }
        // An escaped `\` falls through as a normal character.
        if c == '[' && !escaping {
            // Either a POSIX class, a collation equivalent, or just a `[`.
            for &(name, translation, needs_u, negated) in POSIX_CLASSES {
                if starts_with(glob, i, name) {
                    // `[a-[]` is fine, `[a-[:alpha:]]` is not.
                    if range_start.is_some() {
                        return poison();
                    }
                    let (translation, negated) = match name {
                        "[:print:]" if !options.keeps(Quirk::PosixPrintIsControl) => {
                            (r"\p{Zl}\p{Zp}\p{C}", true)
                        }
                        "[:punct:]" if !options.keeps(Quirk::PosixPunctSkipsSymbols) => {
                            (r"\p{P}\p{S}", false)
                        }
                        _ => (translation, negated),
                    };
                    i += name.len();
                    if negated {
                        negs.push(translation.to_string());
                    } else {
                        ranges.push(translation.to_string());
                    }
                    uflag = uflag || needs_u;
                    continue 'class;
                }
            }
            if !options.keeps(Quirk::UnknownPosixClassIsLiteral) && glob.get(i + 1) == Some(&':') {
                let mut j = i + 2;
                while glob.get(j).is_some_and(char::is_ascii_alphabetic) {
                    j += 1;
                }
                if j > i + 2 && glob.get(j) == Some(&':') && glob.get(j + 1) == Some(&']') {
                    let name: String = glob[i + 2..j].iter().collect();
                    return Err(Refusal::new(
                        Quirk::UnknownPosixClassIsLiteral,
                        format!("`[:{name}:]` is no POSIX class"),
                    ));
                }
            }
        }
        // Now it is a normal character, effectively.
        escaping = false;
        if let Some(start) = range_start {
            // A reversed range is thrown away; the others still match.
            if c > start {
                ranges.push(format!("{}-{}", class_escape(start), class_escape(c)));
            } else if c == start {
                ranges.push(class_escape(c));
            }
            range_start = None;
            i += 1;
            continue;
        }
        // Maybe the start of a range: `c-d`, `c-]`, `c<more>]` or `c]`.
        if starts_with(glob, i + 1, "-]") {
            ranges.push(format!("{}\\-", class_escape(c)));
            i += 2;
            continue;
        }
        if glob.get(i + 1) == Some(&'-') {
            range_start = Some(c);
            i += 2;
            continue;
        }
        ranges.push(class_escape(c));
        i += 1;
    }
    if end_pos < i {
        // No end of the class: not a class, maybe a literal `[`.
        if !options.keeps(Quirk::UnclosedClassIsLiteral) {
            return Err(Refusal::new(
                Quirk::UnclosedClassIsLiteral,
                format!("the class `{}` never closes", excerpt(glob, pos)),
            ));
        }
        return Ok(Class {
            src: String::new(),
            uflag: false,
            consumed: 0,
            magic: false,
        });
    }
    // No ranges and no negations cannot match anything, and that poisons the whole glob.
    if ranges.is_empty() && negs.is_empty() {
        return poison();
    }
    // One positive single character is that literal, not magic: `[_]` escapes glob magic.
    if negs.is_empty() && ranges.len() == 1 && !negate && is_single(&ranges[0]) {
        let literal = ranges[0].chars().last().unwrap_or_default();
        let mut src = String::new();
        regexp_escape(literal, &mut src);
        return Ok(Class {
            src,
            uflag: false,
            consumed: end_pos - pos,
            magic: false,
        });
    }
    let sranges = format!("[{}{}]", if negate { "^" } else { "" }, ranges.concat());
    let snegs = format!("[{}{}]", if negate { "" } else { "^" }, negs.concat());
    let src = if !ranges.is_empty() && !negs.is_empty() {
        format!("({sranges}|{snegs})")
    } else if !ranges.is_empty() {
        sranges
    } else {
        snegs
    };
    Ok(Class {
        src,
        uflag,
        consumed: end_pos - pos,
        magic: true,
    })
}

#[cfg(test)]
mod tests {
    use super::super::Options;
    use super::{parse_class, Class};

    fn parse(glob: &str) -> Class {
        let chars: Vec<char> = glob.chars().collect();
        parse_class(&chars, 0, &Options::DEFAULT).unwrap_or_else(|r| panic!("{}", r.reason))
    }

    /// The class translations of minimatch 10.2.5, recorded with node, as `(class, regex source,
    /// needs the u flag, characters consumed, magic)`; the two `&` escapes are the port's.
    const TABLE: &[(&str, &str, bool, usize, bool)] = &[
        ("[abc]", "[abc]", false, 5, true),
        ("[a-z]", "[a-z]", false, 5, true),
        ("[!a-z]", "[^a-z]", false, 6, true),
        ("[^a-z]", "[^a-z]", false, 6, true),
        ("[]a]", "[\\]a]", false, 4, true),
        ("[]]", "\\]", false, 3, false),
        ("[a-]", "[a\\-]", false, 4, true),
        ("[a", "", false, 0, false),
        ("[z-a]", "$.", false, 5, true),
        ("[a-a]", "a", false, 5, false),
        ("[[:alpha:]]", "[\\p{L}\\p{Nl}]", true, 11, true),
        (
            "[[:alpha:][:digit:]]",
            "[\\p{L}\\p{Nl}\\p{Nd}]",
            true,
            20,
            true,
        ),
        ("[![:alpha:]]", "[^\\p{L}\\p{Nl}]", true, 12, true),
        ("[[:graph:]]", "[^\\p{Z}\\p{C}]", true, 11, true),
        ("[a[:graph:]]", "([a]|[^\\p{Z}\\p{C}])", true, 12, true),
        ("[a-[:alpha:]]", "$.", false, 13, true),
        ("[_]", "_", false, 3, false),
        ("[\\]]", "\\]", false, 4, false),
        ("[\\\\]", "\\\\", false, 4, false),
        ("[a\\-z]", "[a\\-z]", false, 6, true),
        ("[--0]", "[\\--0]", false, 5, true),
        ("[a&&b]", "[a\\&\\&b]", false, 6, true),
        ("[~]", "~", false, 3, false),
        ("[!]", "", false, 0, false),
        ("[!]a]", "[^\\]a]", false, 5, true),
        ("[^]", "", false, 0, false),
        ("[[]", "\\[", false, 3, false),
        ("[[a]", "[\\[a]", false, 4, true),
        ("[.]", "\\.", false, 3, false),
        ("[*]", "\\*", false, 3, false),
        ("[?]", "\\?", false, 3, false),
        ("[]-a]", "[\\]-a]", false, 5, true),
        ("[a-]]", "[a\\-]", false, 4, true),
        ("[ ]", "\\ ", false, 3, false),
        ("[,]", "\\,", false, 3, false),
        ("[#]", "\\#", false, 3, false),
        ("[é]", "é", false, 3, false),
        ("[a-z0-9_]", "[a-z0-9_]", false, 9, true),
        ("[[:xdigit:]]", "[A-Fa-f0-9]", false, 12, true),
        ("[[:ascii:]]", "[\\x00-\\x7f]", false, 11, true),
        ("[[:space:]]", "[\\p{Z}\\t\\r\\n\\v\\f]", true, 11, true),
        (
            "[[:word:]]",
            "[\\p{L}\\p{Nl}\\p{Nd}\\p{Pc}]",
            true,
            10,
            true,
        ),
        ("[[:print:]]", "[\\p{C}]", true, 11, true),
        ("[[:punct:]]", "[\\p{P}]", true, 11, true),
        ("[[:upper:][:lower:]]", "[\\p{Lu}\\p{Ll}]", true, 20, true),
        ("[[:blank:]]", "[\\p{Zs}\\t]", true, 11, true),
        ("[[:cntrl:]]", "[\\p{Cc}]", true, 11, true),
        ("[[:alnum:]]", "[\\p{L}\\p{Nl}\\p{Nd}]", true, 11, true),
        ("[a-c-e]", "[a-c\\-e]", false, 7, true),
        ("[-a]", "[\\-a]", false, 4, true),
        ("[a-\\]]", "$.", false, 6, true),
        ("[[:alpha:]-]", "[\\p{L}\\p{Nl}\\-]", true, 12, true),
        ("[!-]", "[^\\-]", false, 4, true),
        ("[\\a]", "a", false, 4, false),
        ("[a]b", "a", false, 3, false),
        ("[]", "", false, 0, false),
        ("[!", "", false, 0, false),
        ("[a-z", "", false, 0, false),
        ("[[:alpha:", "", false, 0, false),
        ("[[:nope:]]", "[\\[:nope:]", false, 9, true),
        ("[é-ü]", "[é-ü]", false, 5, true),
        ("[\\[]", "\\[", false, 4, false),
        ("[\\-]", "\\-", false, 4, false),
        ("[a\\]b]", "[a\\]b]", false, 6, true),
        ("[+]", "\\+", false, 3, false),
        ("[$]", "\\$", false, 3, false),
        ("[|]", "\\|", false, 3, false),
        ("[(]", "\\(", false, 3, false),
        ("[{]", "\\{", false, 3, false),
        ("[\\n]", "n", false, 4, false),
        ("[\t]", "\\\t", false, 3, false),
        ("[a\tb]", "[a\tb]", false, 5, true),
        ("[$.]", "[$.]", false, 4, true),
    ];

    #[test]
    fn parses_as_minimatch_does() {
        for &(glob, src, uflag, consumed, magic) in TABLE {
            let class = parse(glob);
            assert_eq!(
                class,
                Class {
                    src: src.to_string(),
                    uflag,
                    consumed,
                    magic
                },
                "{glob:?}"
            );
        }
    }

    #[test]
    fn every_source_compiles_on_the_engine() {
        for &(glob, src, _, consumed, _) in TABLE {
            if consumed == 0 {
                continue;
            }
            let re = format!("^{src}$");
            regex::Regex::new(&re).unwrap_or_else(|e| panic!("{glob:?} -> {re:?}: {e}"));
        }
    }

    #[test]
    fn the_engine_accepts_the_escapes_the_translation_emits() {
        // regex-syntax escapes any ASCII punctuation but `<` and `>`, and whitespace by the
        // character itself; the regex engine hands these through.
        for src in [
            "\\,",
            "\\ ",
            "\\#",
            "\\-",
            "\\\t",
            "\\\n",
            "\\\r",
            "\\\x0b",
            "\\\x0c",
            "\\/",
            "\\!",
            "\\@",
            "\\%",
            "\\:",
            "\\;",
            "\\=",
            "\\'",
            "\\\"",
            "\\`",
            "\\_",
            "[a\\&\\&b]",
            "[\\~a]",
            "[\\--0]",
            "[a\\-]",
            "[\\]a]",
            "[^\\]a]",
            "\\p{L}",
            "[\\p{Z}\\t\\r\\n\\v\\f]",
            "[\\x00-\\x7f]",
            "$.",
            "[^/]*?",
            "[^/]+?",
        ] {
            let re = format!("^{src}$");
            regex::Regex::new(&re).unwrap_or_else(|e| panic!("{re:?}: {e}"));
        }
    }

    #[test]
    fn a_class_matches_what_it_says() {
        let matches = |glob: &str, s: &str| {
            let class = parse(glob);
            let re = regex::Regex::new(&format!("^{}$", class.src)).unwrap();
            re.is_match(s)
        };
        assert!(matches("[a-z]", "q"));
        assert!(!matches("[a-z]", "Q"));
        assert!(matches("[!a-z]", "Q"));
        assert!(matches("[[:alpha:]]", "ß"));
        assert!(!matches("[[:alpha:]]", "1"));
        assert!(matches("[[:digit:]]", "٣"));
        assert!(matches("[[:ascii:]]", "~"));
        assert!(!matches("[[:ascii:]]", "é"));
        assert!(matches("[[:xdigit:]]", "F"));
        assert!(!matches("[[:xdigit:]]", "G"));
        assert!(matches("[[:graph:]]", "x"));
        assert!(!matches("[[:graph:]]", " "));
        assert!(matches("[a[:graph:]]", "a"));
        assert!(matches("[a&&b]", "&"));
        assert!(matches("[a&&b]", "a"));
        assert!(!matches("[a&&b]", "c"));
        assert!(matches("[--0]", "."));
        assert!(!matches("[z-a]", "m"));
        assert!(matches("[ ]", " "));
    }

    proptest::proptest! {
        /// Any text after a `[` parses or refuses without a panic; a class that parses stays
        /// within the text and its source compiles.
        #[test]
        fn any_class_text_parses_or_refuses_without_panicking(
            text in proptest::prelude::prop_oneof![
                "[a-c\\[\\]:^!\\\\x\\-]{0,10}",
                "\\[:(alpha|digit|nope|print|punct|space):\\][a-c\\]]{0,3}",
            ],
            strict in proptest::prelude::any::<bool>(),
        ) {
            let glob: Vec<char> = std::iter::once('[').chain(text.chars()).collect();
            let options = Options { quirks: !strict, ..Options::DEFAULT };
            if let Ok(class) = parse_class(&glob, 0, &options) {
                proptest::prop_assert!(class.consumed <= glob.len());
                if !class.src.is_empty() {
                    let src = format!("^{}$", class.src);
                    proptest::prop_assert!(regex::Regex::new(&src).is_ok(), "{src} does not compile");
                }
            }
        }
    }
}
