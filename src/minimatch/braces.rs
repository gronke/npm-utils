//! Brace expansion: brace-expansion 5.0.9 and balanced-match 4.0.4 ported line by line, held by
//! the recorded fixture to the brace-expansion 5.0.12 that minimatch 10.2.5 resolves, with the
//! budgets of [`Options`] where the JavaScript truncates at `EXPANSION_MAX` (100 000 strings) and
//! `EXPANSION_MAX_LENGTH` (4 000 000 characters): a pattern over a budget is an error naming it.
//!
//! The JavaScript hides escaped characters behind sentinel strings while it works; the port uses
//! the Unicode noncharacters U+FDD0 to U+FDD4, which exist for process-internal use. A sequence
//! bound beyond `i64` is over any budget and an error, where the JavaScript loops on a rounded
//! float until its cap.

use super::chars::is_line_terminator;
use super::{BraceLimit, Error, Options, Quirk, MAX_PATTERN_LENGTH};

const ESC_SLASH: char = '\u{FDD0}';
const ESC_OPEN: char = '\u{FDD1}';
const ESC_CLOSE: char = '\u{FDD2}';
const ESC_COMMA: char = '\u{FDD3}';
const ESC_PERIOD: char = '\u{FDD4}';

/// minimatch's `braceExpand`: the pattern itself under `nobrace` or when it holds no `{…}` pair
/// free of `{` inside (the shortcut also leaves escapes intact), else brace-expansion's `expand`.
pub(super) fn brace_expand(pattern: &str, options: &Options) -> Result<Vec<String>, Error> {
    let len = pattern.chars().count();
    if len > MAX_PATTERN_LENGTH {
        return Err(Error::PatternTooLong { len });
    }
    if options.nobrace || !has_brace_pair(pattern) {
        return Ok(vec![pattern.to_string()]);
    }
    expand(pattern, options).map_err(|limit| Error::Braces {
        pattern: pattern.to_string(),
        limit,
    })
}

/// The shortcut test `/\{(?:(?!\{).)*\}/`: a `{`, then characters that are neither `{` nor a line
/// terminator, then a `}`.
fn has_brace_pair(pattern: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '{' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        loop {
            match chars.get(j) {
                Some('}') => return true,
                Some('{') => {
                    i = j;
                    break;
                }
                Some(&c) if is_line_terminator(c) => {
                    i = j + 1;
                    break;
                }
                Some(_) => j += 1,
                None => return false,
            }
        }
    }
    false
}

/// The budgets of one expansion; the group count also bounds the recursion depth, since every
/// nesting level is a group.
struct Budget<'a> {
    options: &'a Options,
    groups: usize,
}

impl Budget<'_> {
    fn max(&self) -> usize {
        self.options.max_brace_expansions
    }

    fn max_length(&self) -> usize {
        self.options.max_brace_length
    }

    fn group(&mut self) -> Result<(), BraceLimit> {
        self.groups += 1;
        if self.groups > self.options.max_brace_groups {
            return Err(BraceLimit::Groups(self.options.max_brace_groups));
        }
        Ok(())
    }
}

/// brace-expansion's `expand`.
fn expand(pattern: &str, options: &Options) -> Result<Vec<String>, BraceLimit> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }
    // Bash keeps a leading `{}` literal at the top level: `{},a}b` stays, `a{},b}c` expands.
    let pattern = match pattern.strip_prefix("{}") {
        Some(rest) => format!("\\{{\\}}{rest}"),
        None => pattern.to_string(),
    };
    let mut budget = Budget { options, groups: 0 };
    let out = expand_(&escape_braces(&pattern), &mut budget, true)?;
    let keep_escapes = !options.keeps(Quirk::BracesStripEscapes);
    Ok(out
        .iter()
        .map(|s| unescape_braces(s, keep_escapes))
        .collect())
}

/// `escapeBraces`: `\\`, `\{`, `\}`, `\,` and `\.` become sentinels, left to right.
fn escape_braces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let sentinel = match chars.peek() {
            Some('\\') => ESC_SLASH,
            Some('{') => ESC_OPEN,
            Some('}') => ESC_CLOSE,
            Some(',') => ESC_COMMA,
            Some('.') => ESC_PERIOD,
            _ => {
                out.push('\\');
                continue;
            }
        };
        chars.next();
        out.push(sentinel);
    }
    out
}

/// `unescapeBraces`: the sentinels become the characters they stood for, `\\` a single `\`;
/// with `keep_escapes` they become the escape sequences again, so the glob parser sees them.
fn unescape_braces(s: &str, keep_escapes: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let plain = match c {
            ESC_SLASH => '\\',
            ESC_OPEN => '{',
            ESC_CLOSE => '}',
            ESC_COMMA => ',',
            ESC_PERIOD => '.',
            c => {
                out.push(c);
                continue;
            }
        };
        if keep_escapes {
            out.push('\\');
        }
        out.push(plain);
    }
    out
}

/// balanced-match's result for `{` and `}`.
struct Balanced<'a> {
    pre: &'a str,
    body: &'a str,
    post: &'a str,
}

fn balanced(s: &str) -> Option<Balanced<'_>> {
    let (start, end) = range(s)?;
    Some(Balanced {
        pre: &s[..start],
        body: &s[start + 1..end],
        post: &s[end + 1..],
    })
}

/// balanced-match's `range`, with `-1` for "not found" as in the JavaScript.
fn range(s: &str) -> Option<(usize, usize)> {
    let index_of = |c: char, from: i64| -> i64 {
        if from < 0 || from as usize > s.len() {
            return -1;
        }
        s[from as usize..]
            .find(c)
            .map_or(-1, |k| (k + from as usize) as i64)
    };
    let mut ai = index_of('{', 0);
    let mut bi = index_of('}', ai + 1);
    let mut i = ai;
    if ai < 0 || bi <= 0 {
        return None;
    }
    let mut begs: Vec<i64> = Vec::new();
    let mut left = s.len() as i64;
    let mut right: i64 = -1;
    let mut result = None;
    while i >= 0 && result.is_none() {
        if i == ai {
            begs.push(i);
            ai = index_of('{', i + 1);
        } else if begs.len() == 1 {
            let r = begs.pop().unwrap_or_default();
            result = Some((r as usize, bi as usize));
        } else {
            if let Some(beg) = begs.pop() {
                if beg < left {
                    left = beg;
                    right = bi;
                }
            }
            bi = index_of('}', i + 1);
        }
        i = if ai < bi && ai >= 0 { ai } else { bi };
    }
    if !begs.is_empty() && right >= 0 {
        result = Some((left as usize, right as usize));
    }
    result
}

/// `parseCommaParts`: `str.split(",")` that keeps a nested `{…}` whole, so `{a,{b,c},d}` has
/// three members. Iterative where the JavaScript recurses once per sibling group.
fn parse_comma_parts(s: &str) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut tail = String::new();
    let mut rest = s;
    loop {
        let Some(m) = balanced(rest) else {
            let mut split = rest.split(',');
            tail.push_str(split.next().unwrap_or_default());
            parts.push(tail);
            parts.extend(split.map(str::to_string));
            return parts;
        };
        let mut p: Vec<String> = m.pre.split(',').map(str::to_string).collect();
        p[0] = format!("{tail}{}", p[0]);
        tail = p.pop().unwrap_or_default();
        tail.push('{');
        tail.push_str(m.body);
        tail.push('}');
        parts.extend(p);
        if m.post.is_empty() {
            parts.push(tail);
            return parts;
        }
        rest = m.post;
    }
}

/// `isPadded`: `/^-?0\d/`.
fn is_padded(part: &str) -> bool {
    let digits = part.strip_prefix('-').unwrap_or(part);
    let mut it = digits.chars();
    it.next() == Some('0') && it.next().is_some_and(|c| c.is_ascii_digit())
}

/// `-?\d+` with JavaScript's ASCII `\d`.
fn is_number(part: &str) -> bool {
    let digits = part.strip_prefix('-').unwrap_or(part);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn is_letter(part: &str) -> bool {
    part.len() == 1 && part.as_bytes()[0].is_ascii_alphabetic()
}

/// `/^-?\d+\.\.-?\d+(?:\.\.-?\d+)?$/`.
fn is_numeric_sequence(body: &str) -> bool {
    let n: Vec<&str> = body.split("..").collect();
    (n.len() == 2 || n.len() == 3) && n.iter().all(|part| is_number(part))
}

/// `/^[a-zA-Z]\.\.[a-zA-Z](?:\.\.-?\d+)?$/`.
fn is_alpha_sequence(body: &str) -> bool {
    let n: Vec<&str> = body.split("..").collect();
    (n.len() == 2 || n.len() == 3)
        && is_letter(n[0])
        && is_letter(n[1])
        && n.get(2).is_none_or(|part| is_number(part))
}

/// `/,(?!,).*\}/` on the text after a group: a comma not followed by a comma, then a `}` with
/// no line terminator between. Read right to left so a run of commas costs one pass.
fn has_comma_then_close(post: &str) -> bool {
    let chars: Vec<char> = post.chars().collect();
    let mut close_ahead = false;
    for k in (0..chars.len()).rev() {
        let c = chars[k];
        if c == ',' && chars.get(k + 1) != Some(&',') && close_ahead {
            return true;
        }
        if c == '}' {
            close_ahead = true;
        } else if is_line_terminator(c) {
            close_ahead = false;
        }
    }
    false
}

/// `combine`: every `acc[a] + pre + values[v]`, empties dropped when asked, the budgets checked
/// on every kept string.
fn combine(
    acc: &[String],
    pre: &str,
    values: &[String],
    budget: &Budget<'_>,
    drop_empties: bool,
) -> Result<Vec<String>, BraceLimit> {
    let (max, max_length) = (budget.max(), budget.max_length());
    let mut out = Vec::new();
    let mut length = 0usize;
    for a in acc {
        for v in values {
            let expansion = format!("{a}{pre}{v}");
            if drop_empties && expansion.is_empty() {
                continue;
            }
            if out.len() >= max {
                return Err(BraceLimit::Expansions(max));
            }
            let n = expansion.chars().count();
            if length + n > max_length {
                return Err(BraceLimit::Length(max_length));
            }
            out.push(expansion);
            length += n;
        }
    }
    Ok(out)
}

/// `expandSequence`: the members of `x..y` or `x..y..incr`, numeric or alphabetic, with the zero
/// padding Bash applies when a bound is written with a leading zero.
fn expand_sequence(
    body: &str,
    is_alpha: bool,
    budget: &Budget<'_>,
) -> Result<Vec<String>, BraceLimit> {
    let (max, max_length) = (budget.max(), budget.max_length());
    let over = BraceLimit::Expansions(max);
    let n: Vec<&str> = body.split("..").collect();
    let bound = |part: &str| -> Option<i64> {
        if is_alpha {
            part.chars().next().map(|c| c as i64)
        } else {
            part.parse().ok()
        }
    };
    let x = bound(n[0]).ok_or(over)?;
    let y = bound(n[1]).ok_or(over)?;
    let width = n[0].len().max(n[1].len());
    let mut incr: i64 = match n.get(2) {
        Some(step) => {
            let step = step
                .parse::<i64>()
                .map_or(u64::MAX, i64::unsigned_abs)
                .min(i64::MAX as u64) as i64;
            step.max(1)
        }
        None => 1,
    };
    let reverse = y < x;
    if reverse {
        incr = -incr;
    }
    let pad = n.iter().any(|part| is_padded(part));
    let count = (i128::from(y) - i128::from(x)).abs() / i128::from(incr.abs()) + 1;
    if count > max as i128 {
        return Err(over);
    }
    let mut out = Vec::with_capacity(count as usize);
    let mut length = 0usize;
    let (mut i, y) = (i128::from(x), i128::from(y));
    while if reverse { i >= y } else { i <= y } {
        let c = if is_alpha {
            match char::from_u32(i as u32) {
                Some('\\') | None => String::new(),
                Some(c) => c.to_string(),
            }
        } else {
            let mut c = i.to_string();
            if pad {
                let need = width.saturating_sub(c.len());
                if need > 0 {
                    let zeros = "0".repeat(need);
                    c = if i < 0 {
                        format!("-{zeros}{}", &c[1..])
                    } else {
                        format!("{zeros}{c}")
                    };
                }
            }
            c
        };
        if length + c.len() > max_length {
            return Err(BraceLimit::Length(max_length));
        }
        length += c.len();
        out.push(c);
        i += i128::from(incr);
    }
    Ok(out)
}

/// `expand_`: the top-level groups of the string left to right, threading the combined prefixes.
fn expand_(input: &str, budget: &mut Budget<'_>, is_top: bool) -> Result<Vec<String>, BraceLimit> {
    let mut is_top = is_top;
    let mut s = input.to_string();
    let mut acc = vec![String::new()];
    // Bash drops empty results, but only when the first top-level group is a comma set: a
    // sequence such as `{Z..a}` may legitimately yield ''. The drop applies to the final strings.
    let mut drop_empties = false;
    let mut first_group = true;
    loop {
        let Some(m) = balanced(&s) else {
            // No brace set left: the rest of the string is literal.
            return combine(&acc, &s, &[String::new()], budget, drop_empties);
        };
        let (pre, body, post) = (m.pre.to_string(), m.body.to_string(), m.post.to_string());
        if pre.ends_with('$') {
            let literal = format!("{pre}{{{body}}}");
            acc = combine(
                &acc,
                &literal,
                &[String::new()],
                budget,
                drop_empties && post.is_empty(),
            )?;
            first_group = false;
            if post.is_empty() {
                break;
            }
            s = post;
            continue;
        }
        let is_numeric = is_numeric_sequence(&body);
        let is_alpha = is_alpha_sequence(&body);
        let is_sequence = is_numeric || is_alpha;
        let is_options = body.contains(',');
        if !is_sequence && !is_options {
            // `{a},b}`: the first `}` was the wrong one, hide it and look again.
            if has_comma_then_close(&post) {
                s = format!("{pre}{{{body}{ESC_CLOSE}{post}");
                is_top = true;
                continue;
            }
            // Nothing here expands, so the whole remaining string is literal.
            let literal = format!("{pre}{{{body}}}{post}");
            return combine(&acc, &literal, &[String::new()], budget, drop_empties);
        }
        budget.group()?;
        if first_group {
            drop_empties = is_top && !is_sequence;
            first_group = false;
        }
        let values = if is_sequence {
            expand_sequence(&body, is_alpha, budget)?
        } else {
            let mut n = parse_comma_parts(&body);
            if n.len() == 1 {
                // x{{a,b}}y ==> x{a}y x{b}y
                n = expand_(&n[0], budget, false)?
                    .into_iter()
                    .map(|member| format!("{{{member}}}"))
                    .collect();
                if n.len() == 1 {
                    let literal = format!("{pre}{}", n[0]);
                    acc = combine(
                        &acc,
                        &literal,
                        &[String::new()],
                        budget,
                        drop_empties && post.is_empty(),
                    )?;
                    if post.is_empty() {
                        break;
                    }
                    s = post;
                    continue;
                }
            }
            // Members that `combine` would drop as empty produce no result, so they count against
            // no budget either.
            let drops_empties = drop_empties
                && post.is_empty()
                && pre.is_empty()
                && acc.iter().all(String::is_empty);
            let (max, max_length) = (budget.max(), budget.max_length());
            let mut values = Vec::new();
            let mut values_length = 0usize;
            for member in &n {
                for v in expand_(member, budget, false)? {
                    if drops_empties && v.is_empty() {
                        continue;
                    }
                    if values.len() >= max {
                        return Err(BraceLimit::Expansions(max));
                    }
                    let len = v.chars().count();
                    if values_length + len > max_length {
                        return Err(BraceLimit::Length(max_length));
                    }
                    values_length += len;
                    values.push(v);
                }
            }
            values
        };
        acc = combine(&acc, &pre, &values, budget, drop_empties && post.is_empty())?;
        if post.is_empty() {
            break;
        }
        s = post;
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::super::{BraceLimit, Error, Options};
    use super::{brace_expand, escape_braces, unescape_braces};

    fn expand(pattern: &str) -> Vec<String> {
        super::brace_expand(pattern, &Options::DEFAULT)
            .unwrap_or_else(|e| panic!("{pattern:?}: {e}"))
    }

    fn expand_with(pattern: &str, options: Options) -> Result<Vec<String>, Error> {
        super::brace_expand(pattern, &options)
    }

    /// The expansions minimatch 10.2.5 produces, recorded with node; Bash's rules.
    const TABLE: &[(&str, &[&str])] = &[
        ("a{b,c}d", &["abd", "acd"]),
        ("a{b,}c", &["abc", "ac"]),
        ("a{0..3}d", &["a0d", "a1d", "a2d", "a3d"]),
        ("a{b,c{d,e}f}g", &["abg", "acdfg", "acefg"]),
        ("a{b,c}d{e,f}g", &["abdeg", "abdfg", "acdeg", "acdfg"]),
        ("a{2..}b", &["a{2..}b"]),
        ("a{b}c", &["a{b}c"]),
        ("{}", &["{}"]),
        ("{},a}b", &["{},a}b"]),
        ("a{},b}c", &["a}c", "abc"]),
        ("x{{a,b}}y", &["x{a}y", "x{b}y"]),
        ("{a,b{c,d}}", &["a", "bc", "bd"]),
        ("{01..3}", &["01", "02", "03"]),
        ("{1..3..01}", &["1", "2", "3"]),
        ("{-2..2}", &["-2", "-1", "0", "1", "2"]),
        ("{5..1}", &["5", "4", "3", "2", "1"]),
        ("{a..e..2}", &["a", "c", "e"]),
        ("{e..a}", &["e", "d", "c", "b", "a"]),
        ("{Z..a}", &["Z", "[", "", "]", "^", "_", "`", "a"]),
        ("\\{a,b\\}", &["{a,b}"]),
        ("a\\\\{b,c}", &["a\\b", "a\\c"]),
        ("{a\\,b,c}", &["a,b", "c"]),
        ("{1\\..3}", &["{1..3}"]),
        ("{,a}", &["a"]),
        ("{a,,b}", &["a", "b"]),
        ("x{a,,b}", &["xa", "x", "xb"]),
        ("{a,,b}c", &["ac", "c", "bc"]),
        ("${a,b}", &["${a,b}"]),
        ("x${a,b}{c,d}", &["x${a,b}c", "x${a,b}d"]),
        ("{a},b}", &["a}", "b"]),
        ("{{a,b},{c,d}}", &["a", "b", "c", "d"]),
        ("{a,b}{1,2}", &["a1", "a2", "b1", "b2"]),
        ("{1..1}", &["1"]),
        ("{0..10..3}", &["0", "3", "6", "9"]),
        ("{10..0..3}", &["10", "7", "4", "1"]),
        ("{-05..05..5}", &["-05", "000", "005"]),
        ("{a.b,c}", &["a.b", "c"]),
        ("{a{b,c}", &["{ab", "{ac"]),
        ("a{b,c}}", &["ab}", "ac}"]),
        ("{{a,b}", &["{a", "{b"]),
        ("{a..b..c}", &["{a..b..c}"]),
        ("{1..2..0}", &["1", "2"]),
        ("{2..1..-1}", &["2", "1"]),
        ("{a,b}{}", &["a{}", "b{}"]),
        ("{,}", &[]),
        ("{,,}", &[]),
        ("a{,}", &["a", "a"]),
        (
            "{a,b}{c,d}{e,f}",
            &["ace", "acf", "ade", "adf", "bce", "bcf", "bde", "bdf"],
        ),
        ("{a,{b,c},d}", &["a", "b", "c", "d"]),
        ("{a,b\\}c,d}", &["a", "b}c", "d"]),
        ("{-1..1}", &["-1", "0", "1"]),
        (
            "{01..010}",
            &[
                "001", "002", "003", "004", "005", "006", "007", "008", "009", "010",
            ],
        ),
        ("{1..-3}", &["1", "0", "-1", "-2", "-3"]),
        ("{a..c}{1..2}", &["a1", "a2", "b1", "b2", "c1", "c2"]),
        ("{a,b}c{d,e}", &["acd", "ace", "bcd", "bce"]),
        ("a{{b,c}}", &["a{b}", "a{c}"]),
        ("{{{a,b}}}", &["{{a}}", "{{b}}"]),
        ("x{a,b}$", &["xa$", "xb$"]),
        ("{a\\\\,b}", &["a\\", "b"]),
        ("\\\\{a,b}", &["\\a", "\\b"]),
        ("{a,b}\\{c,d}", &["a{c,d}", "b{c,d}"]),
        ("{😀,b}", &["😀", "b"]),
        ("{a..b}", &["a", "b"]),
        ("{A..C}", &["A", "B", "C"]),
        ("{10..1..3}", &["10", "7", "4", "1"]),
        (
            "{1..10..0}",
            &["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"],
        ),
        ("{a,b,}", &["a", "b"]),
        ("{,a,}", &["a"]),
        ("x{,a,}", &["x", "xa", "x"]),
        ("{a,}{b,}", &["ab", "a", "b"]),
        ("{0..1}{0..1}", &["00", "01", "10", "11"]),
        ("{1..3}x{a,b}", &["1xa", "1xb", "2xa", "2xb", "3xa", "3xb"]),
        ("pre{a,b}post", &["preapost", "prebpost"]),
        ("{a}{b,c}", &["{a}b", "{a}c"]),
        ("{a,b", &["{a,b"]),
        ("a,b}", &["a,b}"]),
        ("{}{a,b}", &["{}a", "{}b"]),
        ("{}a{b,c}", &["{}ab", "{}ac"]),
        ("a{}", &["a{}"]),
        // The shortcut keeps a pattern without a brace pair as is, escapes included.
        ("a\\\\b", &["a\\\\b"]),
        ("\\{a\\}", &["{a}"]),
        ("", &[""]),
        // JavaScript's `.` stops at a line terminator, so this pair is never seen.
        ("{a\nb,c}", &["{a\nb,c}"]),
    ];

    #[test]
    fn expands_as_minimatch_does() {
        for (pattern, expected) in TABLE {
            assert_eq!(expand(pattern), *expected, "{pattern:?}");
        }
    }

    #[test]
    fn nobrace_leaves_the_pattern() {
        let options = Options {
            nobrace: true,
            ..Options::DEFAULT
        };
        assert_eq!(expand_with("a{b,c}d", options).unwrap(), ["a{b,c}d"]);
    }

    #[test]
    fn the_expansion_budget_is_an_error_not_a_truncation() {
        let options = Options {
            max_brace_expansions: 3,
            ..Options::DEFAULT
        };
        assert_eq!(expand_with("{a,b,c}", options).unwrap(), ["a", "b", "c"]);
        assert_eq!(
            expand_with("{a,b,c,d}", options),
            Err(Error::Braces {
                pattern: "{a,b,c,d}".into(),
                limit: BraceLimit::Expansions(3),
            })
        );
        // Empties that Bash drops count against nothing.
        assert_eq!(expand_with("{a,,,b,c}", options).unwrap(), ["a", "b", "c"]);
        assert_eq!(expand_with("{1..3}", options).unwrap(), ["1", "2", "3"]);
        assert!(matches!(
            expand_with("{1..4}", options),
            Err(Error::Braces {
                limit: BraceLimit::Expansions(3),
                ..
            })
        ));
    }

    #[test]
    fn the_default_budgets_stop_the_bombs() {
        let bomb = "{a,b}".repeat(14);
        let error = super::brace_expand(&bomb, &Options::DEFAULT).unwrap_err();
        assert_eq!(
            error,
            Error::Braces {
                pattern: bomb.clone(),
                limit: BraceLimit::Expansions(10_000)
            }
        );
        let message = error.to_string();
        assert!(message.starts_with(&format!("{bomb:?}")), "{message}");
        assert!(message.contains("brace expansion"), "{message}");
        // A bound beyond i64 is over any budget; the JavaScript loops on a rounded float.
        assert!(matches!(
            expand_with("{0..18446744073709551616}", Options::DEFAULT),
            Err(Error::Braces {
                limit: BraceLimit::Expansions(10_000),
                ..
            })
        ));
        assert!(matches!(
            expand_with("{1..20000}", Options::DEFAULT),
            Err(Error::Braces {
                limit: BraceLimit::Expansions(10_000),
                ..
            })
        ));
        assert_eq!(expand("{1..10000}").len(), 10_000);
        assert_eq!(expand(&"{a,b}".repeat(13)).len(), 8192);
    }

    #[test]
    fn the_group_budget_bounds_groups_and_nesting() {
        let hundred = "{1..1}".repeat(100);
        assert_eq!(expand(&hundred), ["1".repeat(100)]);
        let over = "{1..1}".repeat(101);
        assert!(matches!(
            expand_with(&over, Options::DEFAULT),
            Err(Error::Braces {
                limit: BraceLimit::Groups(100),
                ..
            })
        ));
        let one = Options {
            max_brace_groups: 1,
            ..Options::DEFAULT
        };
        assert_eq!(expand_with("{a,b}", one).unwrap(), ["a", "b"]);
        assert!(matches!(
            expand_with("{a,b}{c,d}", one),
            Err(Error::Braces {
                limit: BraceLimit::Groups(1),
                ..
            })
        ));
        assert!(matches!(
            expand_with("{a,{b,c}}", one),
            Err(Error::Braces {
                limit: BraceLimit::Groups(1),
                ..
            })
        ));
        // Literal groups cost nothing.
        assert_eq!(
            expand_with("{a}{b}{c,d}", one).unwrap(),
            ["{a}{b}c", "{a}{b}d"]
        );
        // Deep nesting is bounded by the same budget, so it never exhausts the stack.
        let deep = format!("{}a,b{}", "{".repeat(30_000), "}".repeat(30_000));
        assert!(matches!(
            expand_with(&deep, Options::DEFAULT),
            Err(Error::Braces {
                limit: BraceLimit::Groups(100),
                ..
            })
        ));
    }

    #[test]
    fn the_length_budget_is_an_error() {
        let options = Options {
            max_brace_length: 5,
            ..Options::DEFAULT
        };
        assert_eq!(expand_with("{a,b}", options).unwrap(), ["a", "b"]);
        assert!(matches!(
            expand_with("{ab,cd}x", options),
            Err(Error::Braces {
                limit: BraceLimit::Length(5),
                ..
            })
        ));
        assert!(matches!(
            expand_with("{100..102}", options),
            Err(Error::Braces {
                limit: BraceLimit::Length(5),
                ..
            })
        ));
    }

    #[test]
    fn a_pattern_over_64k_characters_is_refused() {
        let long = "a".repeat(64 * 1024);
        assert_eq!(expand(&long), std::slice::from_ref(&long));
        let too_long = format!("{long}a");
        assert_eq!(
            super::brace_expand(&too_long, &Options::DEFAULT),
            Err(Error::PatternTooLong { len: 64 * 1024 + 1 })
        );
    }

    #[test]
    fn a_wide_pattern_of_sibling_groups_stays_flat() {
        // Thousands of sibling groups recurse once per group in the JavaScript's
        // `parseCommaParts`; the port loops.
        let wide = format!("{{{}a}}", "{a},".repeat(5_000));
        assert_eq!(expand(&wide).len(), 5_001);
    }

    proptest::proptest! {
        /// Any text expands or errors without a panic, within the expansion budget; `nobrace`
        /// returns the text itself.
        #[test]
        fn any_text_expands_within_the_budget_or_errors(
            pattern in "[a-c{},.\\\\0-9\\-]{0,16}",
            quirks in proptest::prelude::any::<bool>(),
        ) {
            let options = Options { quirks, ..Options::DEFAULT };
            if let Ok(out) = brace_expand(&pattern, &options) {
                proptest::prop_assert!(out.len() <= options.max_brace_expansions);
            }
            let nobrace = Options { nobrace: true, ..options };
            proptest::prop_assert_eq!(brace_expand(&pattern, &nobrace).unwrap(), vec![pattern.clone()]);
        }

        /// The sentinels hide every escape and give it back.
        #[test]
        fn the_brace_escapes_round_trip(s in "[a-c{},.\\\\]{0,12}") {
            proptest::prop_assert_eq!(unescape_braces(&escape_braces(&s), true), s);
        }
    }
}
