//! The minimatch subset behind npm's ignore rules.
//!
//! ignore-walk compiles every rule with `matchBase`, `dot`, `nocase` and `flipNegate`. This is
//! a port of what those options make of a pattern: `*`, `?`, `**`, bracket classes with ranges
//! and POSIX names, the extglob groups `@()`, `?()`, `*()`, `+()` and `!()`, brace alternatives
//! and sequences, backslash escapes; a leading `!` as the negation flag; a slash-less pattern
//! matching the basename at any depth; and the partial mode the walker uses to decide whether
//! to descend into a directory. A `files` entry compiles as a glob instead: whole path, case
//! kept.

use std::cmp::Ordering;

use crate::Result;

/// The brace alternatives one rule line may expand to. Real patterns produce tens; a line past
/// this limit (`{1..999999999}`, dozens of `{a,b,c}` groups) fails the pack by name instead of
/// exhausting memory.
const MAX_BRACE_EXPANSIONS: usize = 10_000;

/// The brace groups one rule line may carry. Each group is a recursion level and a multiplier
/// of the groups after it; this bounds both, independent of the alternative budget.
const MAX_BRACE_GROUPS: usize = 100;

/// The expansion allowances of one rule line: alternatives produced (charged as they
/// materialize) and brace groups (charged up front).
struct Budget {
    expansions: usize,
    groups: usize,
}

impl Budget {
    fn fresh() -> Self {
        Budget {
            expansions: MAX_BRACE_EXPANSIONS,
            groups: MAX_BRACE_GROUPS,
        }
    }

    /// A brace group opens: one group allowance.
    fn group(&mut self) -> Result<(), TooManyExpansions> {
        self.groups = self.groups.checked_sub(1).ok_or(TooManyExpansions)?;
        Ok(())
    }

    /// One more alternative materialized: one expansion allowance.
    fn expansion(&mut self) -> Result<(), TooManyExpansions> {
        self.expansions = self.expansions.checked_sub(1).ok_or(TooManyExpansions)?;
        Ok(())
    }

    /// A sequence about to materialize `count` entries at once: refuse before the allocation
    /// when it can never fit the remaining allowance (the entries charge the allowance as they
    /// are produced downstream, so nothing is deducted here).
    fn check_sequence(&self, count: u128) -> Result<(), TooManyExpansions> {
        if count > self.expansions as u128 {
            return Err(TooManyExpansions);
        }
        Ok(())
    }
}

/// A pattern's brace expansion ran past a budget; the pack fails naming the line rather than
/// hanging, exhausting memory or silently matching less than written.
#[derive(Debug)]
struct TooManyExpansions;

/// The backtracking steps one rule evaluation may take; every `match_one` and `match_tokens`
/// entry is one step. Legitimate patterns need ~1e5 at most; `*b*b*b*b*b*b*b*c` or
/// `+(b|bb)+(b|bb)+…` against a long name needs ~1e13 and trips this in milliseconds. A rule at
/// the full brace limit with wildcard-heavy alternatives can exhaust it too; the error names
/// the rule to fix.
const MAX_MATCH_STEPS: u64 = 1_000_000;

/// A rule evaluation ran past [`MAX_MATCH_STEPS`].
#[derive(Debug)]
struct OutOfSteps;

/// One compiled rule line.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    /// The `!` prefix: a match means *include* (ignore-walk's `flipNegate`).
    pub(crate) negate: bool,
    /// The trimmed source line, kept so a step-budget error can name the rule.
    line: String,
    /// The brace-expanded alternatives, each split into path segments.
    sets: Vec<Vec<Segment>>,
    /// minimatch's `nocase`: fold case while matching (ignore rules do, a `files` glob does
    /// not, as npm's glob runs case-sensitively on Linux).
    nocase: bool,
    /// minimatch's `matchBase`: a slash-less pattern matches the basename at any depth (ignore
    /// rules do; a `files` glob matches the whole path from the package root).
    matchbase: bool,
}

#[derive(Debug, Clone)]
enum Segment {
    /// `**`: any run of segments.
    GlobStar,
    /// One path segment of literal characters, wildcards and groups.
    Plain(Vec<Token>),
}

#[derive(Debug, Clone)]
enum Token {
    Literal(char),
    /// `?`
    One,
    /// `*`
    Many,
    /// `[…]`
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
    /// An extglob group: `@(…)`, `?(…)`, `*(…)`, `+(…)` or `!(…)`.
    Group {
        kind: GroupKind,
        alternatives: Vec<Vec<Token>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    /// `@(…)`: exactly one of the alternatives.
    Exactly,
    /// `?(…)`: zero or one.
    Optional,
    /// `*(…)`: zero or more.
    Any,
    /// `+(…)`: one or more.
    Some,
    /// `!(…)`: anything but one of the alternatives followed by the rest of the segment.
    Not,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Single(char),
    Range(char, char),
    /// `[:alpha:]` and its siblings.
    Posix(String),
}

impl Rule {
    /// Compile one rule line; `None` for a blank line or a `#` comment. A line whose brace
    /// expansion runs past [`MAX_BRACE_EXPANSIONS`] is an error naming it; treating it as a
    /// literal could ship files an exclusion was written to hide.
    pub(crate) fn parse(line: &str) -> Result<Option<Rule>> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }
        let mut negate = false;
        let mut pattern = line;
        while let Some(rest) = pattern.strip_prefix('!') {
            negate = !negate;
            pattern = rest;
        }
        let sets = expand_braces(pattern, &mut Budget::fresh())
            .map_err(|TooManyExpansions| {
                format!(
                    "ignore rule {line:?}: brace expansion over the {MAX_BRACE_EXPANSIONS} limit"
                )
            })?
            .into_iter()
            .map(|alternative| alternative.split('/').map(compile_segment).collect())
            .collect();
        Ok(Some(Rule {
            negate,
            line: line.to_string(),
            sets,
            nocase: true,
            matchbase: true,
        }))
    }

    /// Compile a `files` entry the way npm-packlist 11 hands it to glob: matched against the
    /// whole path from the package root, case-sensitively, `dot` on, no negation prefix (the
    /// caller stripped it), a `#` or blank pattern being a literal like any other. The brace
    /// budget applies as for a rule.
    pub(crate) fn parse_glob(pattern: &str) -> Result<Option<Rule>> {
        if pattern.is_empty() {
            return Ok(None);
        }
        let sets = expand_braces(pattern, &mut Budget::fresh())
            .map_err(|TooManyExpansions| {
                format!(
                    "files entry {pattern:?}: brace expansion over the {MAX_BRACE_EXPANSIONS} limit"
                )
            })?
            .into_iter()
            .map(|alternative| alternative.split('/').map(compile_segment).collect())
            .collect();
        Ok(Some(Rule {
            negate: false,
            line: pattern.to_string(),
            sets,
            nocase: false,
            matchbase: false,
        }))
    }

    /// ignore-walk's "relative rule": some alternative is a single segment (`foo`) or one with a
    /// trailing slash (`foo/`), so it also applies to a bare basename further down the tree.
    pub(crate) fn is_relative(&self) -> bool {
        self.sets.iter().any(|set| {
            let trailing_slash =
                matches!(set.last(), Some(Segment::Plain(tokens)) if tokens.is_empty());
            set.len() <= if trailing_slash { 2 } else { 1 }
        })
    }

    /// minimatch's `match(path, partial)` under the rule's options: `path` splits on `/` (a
    /// leading slash makes an empty first segment, which only an anchored pattern matches), a
    /// slash-less alternative matches the last non-empty segment when `matchBase` is on, and
    /// `partial` accepts a path that is a prefix of a possible match. An evaluation past
    /// [`MAX_MATCH_STEPS`] is an error naming the rule.
    pub(crate) fn matches(&self, path: &str, partial: bool) -> Result<bool> {
        if partial && path == "/" {
            return Ok(true);
        }
        let segments: Vec<&str> = path.split('/').collect();
        let filename = segments
            .iter()
            .rev()
            .find(|s| !s.is_empty())
            .copied()
            .unwrap_or("");
        let mut steps = MAX_MATCH_STEPS;
        for set in &self.sets {
            let hit = if self.matchbase && set.len() == 1 {
                match_one(&[filename], set, partial, self.nocase, &mut steps)
            } else {
                match_one(&segments, set, partial, self.nocase, &mut steps)
            };
            match hit {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(OutOfSteps) => {
                    return Err(format!(
                        "ignore rule {:?}: matching over the {MAX_MATCH_STEPS}-step limit",
                        self.line
                    )
                    .into())
                }
            }
        }
        Ok(false)
    }
}

/// minimatch's `matchOne`: segment by segment, `**` swallowing any run but never `.` or `..`.
/// Every call is one step of the evaluation's budget.
fn match_one(
    file: &[&str],
    pattern: &[Segment],
    partial: bool,
    nocase: bool,
    steps: &mut u64,
) -> Result<bool, OutOfSteps> {
    *steps = steps.checked_sub(1).ok_or(OutOfSteps)?;
    let (mut fi, mut pi) = (0, 0);
    while fi < file.len() && pi < pattern.len() {
        match &pattern[pi] {
            Segment::GlobStar => {
                let pr = pi + 1;
                if pr == pattern.len() {
                    // A trailing ** swallows the rest, except the dot directories.
                    return Ok(file[fi..].iter().all(|f| *f != "." && *f != ".."));
                }
                let mut fr = fi;
                while fr < file.len() {
                    if match_one(&file[fr..], &pattern[pr..], partial, nocase, steps)? {
                        return Ok(true);
                    }
                    if file[fr] == "." || file[fr] == ".." {
                        break;
                    }
                    fr += 1;
                }
                return Ok(partial && fr == file.len());
            }
            Segment::Plain(tokens) => {
                if !match_segment(file[fi], tokens, nocase, steps)? {
                    return Ok(false);
                }
                fi += 1;
                pi += 1;
            }
        }
    }
    if fi == file.len() && pi == pattern.len() {
        Ok(true)
    } else if fi == file.len() {
        Ok(partial)
    } else {
        // Out of pattern: only a trailing empty segment (a trailing slash) is left to match.
        Ok(fi == file.len() - 1 && file[fi].is_empty())
    }
}

/// One path segment against one pattern segment, case-insensitively; a pattern that opens with
/// a wildcard or a group never matches the `.` and `..` directories.
fn match_segment(
    text: &str,
    tokens: &[Token],
    nocase: bool,
    steps: &mut u64,
) -> Result<bool, OutOfSteps> {
    if matches!(
        tokens.first(),
        Some(Token::One | Token::Many | Token::Class { .. } | Token::Group { .. })
    ) && (text == "." || text == "..")
    {
        return Ok(false);
    }
    let chars: Vec<char> = text.chars().collect();
    match_tokens(&chars, tokens, &[], nocase, steps)
}

/// Match `tokens` at the start of `chars`, then each continuation in turn; the whole input must
/// be consumed. Groups branch and backtrack here. Every call is one step of the budget.
fn match_tokens(
    chars: &[char],
    tokens: &[Token],
    continuations: &[&[Token]],
    nocase: bool,
    steps: &mut u64,
) -> Result<bool, OutOfSteps> {
    *steps = steps.checked_sub(1).ok_or(OutOfSteps)?;
    let Some((token, rest)) = tokens.split_first() else {
        return match continuations.split_first() {
            None => Ok(chars.is_empty()),
            Some((next, more)) => match_tokens(chars, next, more, nocase, steps),
        };
    };
    match token {
        Token::Many => {
            for skip in 0..=chars.len() {
                if match_tokens(&chars[skip..], rest, continuations, nocase, steps)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Token::Group { kind, alternatives } => {
            // The alternatives run into the rest of the segment and its continuations.
            let mut then_rest: Vec<&[Token]> = Vec::with_capacity(continuations.len() + 1);
            then_rest.push(rest);
            then_rest.extend_from_slice(continuations);
            let one = |alternative: &Vec<Token>, steps: &mut u64| {
                match_tokens(chars, alternative, &then_rest, nocase, steps)
            };
            match kind {
                GroupKind::Exactly => {
                    for alternative in alternatives {
                        if one(alternative, steps)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                GroupKind::Optional => {
                    if match_tokens(chars, rest, continuations, nocase, steps)? {
                        return Ok(true);
                    }
                    for alternative in alternatives {
                        if one(alternative, steps)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                GroupKind::Any => repeat(chars, alternatives, rest, continuations, nocase, steps),
                GroupKind::Some => {
                    once_then_repeat(chars, alternatives, rest, continuations, nocase, steps)
                }
                // minimatch: a negative lookahead for "an alternative and then the rest, to the
                // end" at this position, then any run of characters before the rest.
                GroupKind::Not => {
                    for alternative in alternatives {
                        if one(alternative, steps)? {
                            return Ok(false);
                        }
                    }
                    for skip in 0..=chars.len() {
                        if match_tokens(&chars[skip..], rest, continuations, nocase, steps)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
            }
        }
        single => match chars.split_first() {
            Some((&c, tail)) if token_matches(single, c, nocase) => {
                match_tokens(tail, rest, continuations, nocase, steps)
            }
            _ => Ok(false),
        },
    }
}

/// Zero or more non-empty occurrences of an alternative, then the rest.
fn repeat(
    chars: &[char],
    alternatives: &[Vec<Token>],
    rest: &[Token],
    continuations: &[&[Token]],
    nocase: bool,
    steps: &mut u64,
) -> Result<bool, OutOfSteps> {
    Ok(match_tokens(chars, rest, continuations, nocase, steps)?
        || once_then_repeat(chars, alternatives, rest, continuations, nocase, steps)?)
}

/// One non-empty occurrence of an alternative, then zero or more, then the rest.
fn once_then_repeat(
    chars: &[char],
    alternatives: &[Vec<Token>],
    rest: &[Token],
    continuations: &[&[Token]],
    nocase: bool,
    steps: &mut u64,
) -> Result<bool, OutOfSteps> {
    for alternative in alternatives {
        for taken in 1..=chars.len() {
            if match_tokens(&chars[..taken], alternative, &[], nocase, steps)?
                && repeat(
                    &chars[taken..],
                    alternatives,
                    rest,
                    continuations,
                    nocase,
                    steps,
                )?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn token_matches(token: &Token, c: char, nocase: bool) -> bool {
    let fold = |x: char| if nocase { fold(x) } else { x };
    match token {
        Token::Literal(expected) => fold(*expected) == fold(c),
        // A single character always satisfies `?`, and `*` on its own (the run case is the
        // caller's).
        Token::One | Token::Many => true,
        Token::Class { negated, items } => {
            let folded = fold(c);
            let hit = items.iter().any(|item| match item {
                ClassItem::Single(x) => fold(*x) == folded,
                ClassItem::Range(lo, hi) => {
                    matches!(
                        (fold(*lo).cmp(&folded), folded.cmp(&fold(*hi))),
                        (
                            Ordering::Less | Ordering::Equal,
                            Ordering::Less | Ordering::Equal
                        )
                    )
                }
                ClassItem::Posix(name) => posix_class(name, c),
            });
            hit != *negated
        }
        // Groups branch in the caller.
        Token::Group { .. } => false,
    }
}

/// The POSIX bracket classes minimatch knows.
fn posix_class(name: &str, c: char) -> bool {
    match name {
        "alpha" => c.is_alphabetic(),
        "digit" => c.is_ascii_digit(),
        "alnum" => c.is_alphanumeric(),
        "upper" | "lower" => c.is_alphabetic(),
        "space" => c.is_whitespace(),
        "blank" => c == ' ' || c == '\t',
        "punct" => c.is_ascii_punctuation(),
        "xdigit" => c.is_ascii_hexdigit(),
        "word" => c.is_alphanumeric() || c == '_',
        "cntrl" => c.is_control(),
        "graph" => !c.is_whitespace() && !c.is_control(),
        "print" => !c.is_control(),
        _ => false,
    }
}

/// `nocase`: one lowercase form per character. Shared with the walker's never-ship veto, so a
/// spelling the rules would catch cannot slip past the veto in a different case.
pub(super) fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn compile_segment(segment: &str) -> Segment {
    if segment == "**" {
        return Segment::GlobStar;
    }
    let chars: Vec<char> = segment.chars().collect();
    let (tokens, end) = parse_tokens(&chars, 0, false);
    debug_assert_eq!(end, chars.len());
    Segment::Plain(tokens)
}

/// Parse tokens from `start`; inside a group, stop at a top-level `|` or `)` and return its
/// index. An extglob opener whose group never closes is literal, as minimatch leaves it.
fn parse_tokens(chars: &[char], start: usize, in_group: bool) -> (Vec<Token>, usize) {
    let mut tokens = Vec::new();
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if in_group && (c == '|' || c == ')') {
            return (tokens, i);
        }
        if matches!(c, '@' | '?' | '*' | '+' | '!') && chars.get(i + 1) == Some(&'(') {
            if let Some((alternatives, close)) = parse_group(chars, i + 2) {
                let kind = match c {
                    '@' => GroupKind::Exactly,
                    '?' => GroupKind::Optional,
                    '*' => GroupKind::Any,
                    '+' => GroupKind::Some,
                    _ => GroupKind::Not,
                };
                tokens.push(Token::Group { kind, alternatives });
                i = close + 1;
                continue;
            }
        }
        match c {
            '\\' if i + 1 < chars.len() => {
                tokens.push(Token::Literal(chars[i + 1]));
                i += 2;
            }
            '*' => {
                if !matches!(tokens.last(), Some(Token::Many)) {
                    tokens.push(Token::Many);
                }
                i += 1;
            }
            '?' => {
                tokens.push(Token::One);
                i += 1;
            }
            '[' => match parse_class(&chars[i + 1..]) {
                Some((token, consumed)) => {
                    tokens.push(token);
                    i += 1 + consumed;
                }
                None => {
                    tokens.push(Token::Literal('['));
                    i += 1;
                }
            },
            c => {
                tokens.push(Token::Literal(c));
                i += 1;
            }
        }
    }
    (tokens, i)
}

/// The alternatives of a group whose `(` sits at `start - 1`, and the index of its `)`; `None`
/// when it never closes.
fn parse_group(chars: &[char], start: usize) -> Option<(Vec<Vec<Token>>, usize)> {
    let mut alternatives = Vec::new();
    let mut i = start;
    loop {
        let (tokens, stop) = parse_tokens(chars, i, true);
        alternatives.push(tokens);
        match chars.get(stop) {
            Some(')') => return Some((alternatives, stop)),
            Some('|') => i = stop + 1,
            _ => return None,
        }
    }
}

/// The class after its opening bracket: the token and the characters consumed, `None` when the
/// bracket never closes (it is then a literal `[`).
fn parse_class(chars: &[char]) -> Option<(Token, usize)> {
    let negated = matches!(chars.first(), Some('!' | '^'));
    let mut i = usize::from(negated);
    let mut items = Vec::new();
    let mut first = true;
    while i < chars.len() {
        let c = chars[i];
        if c == ']' && !first {
            return Some((Token::Class { negated, items }, i + 1));
        }
        first = false;
        if c == '[' && chars.get(i + 1) == Some(&':') {
            if let Some(end) = chars[i + 2..].windows(2).position(|w| w == [':', ']']) {
                let name: String = chars[i + 2..i + 2 + end].iter().collect();
                items.push(ClassItem::Posix(name));
                i += 2 + end + 2;
                continue;
            }
        }
        let literal = if c == '\\' && i + 1 < chars.len() {
            i += 1;
            chars[i]
        } else {
            c
        };
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
            items.push(ClassItem::Range(literal, chars[i + 2]));
            i += 3;
        } else {
            items.push(ClassItem::Single(literal));
            i += 1;
        }
    }
    None
}

/// Brace alternatives and sequences, `{a,b}` → `a` and `b`, `{1..3}` → `1`, `2`, `3` (padded
/// when a bound is), `{a..c}` → `a`, `b`, `c`; nested and escaped braces honored, a group that
/// is neither stays literal, as minimatch leaves it. Every group and every produced alternative
/// draws on `budget`; running out is an error, never a silent truncation.
fn expand_braces(pattern: &str, budget: &mut Budget) -> Result<Vec<String>, TooManyExpansions> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '{' => {
                if let Some((close, alternatives)) = split_group(&chars, i, budget)? {
                    let prefix: String = chars[..i].iter().collect();
                    let suffix: String = chars[close + 1..].iter().collect();
                    let mut out = Vec::new();
                    for alternative in alternatives {
                        out.extend(expand_braces(
                            &format!("{prefix}{alternative}{suffix}"),
                            budget,
                        )?);
                    }
                    return Ok(out);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    budget.expansion()?;
    Ok(vec![pattern.to_string()])
}

/// The group opening at `open`: its closing index and the alternatives it stands for, `None`
/// when it never closes or is neither a comma list nor a sequence.
fn split_group(
    chars: &[char],
    open: usize,
    budget: &mut Budget,
) -> Result<Option<(usize, Vec<String>)>, TooManyExpansions> {
    let mut depth = 0usize;
    let mut alternatives = vec![String::new()];
    let mut i = open + 1;
    while i < chars.len() {
        let c = chars[i];
        let current = alternatives
            .last_mut()
            .expect("one alternative is always open");
        match c {
            '\\' if i + 1 < chars.len() => {
                current.push(c);
                current.push(chars[i + 1]);
                i += 2;
                continue;
            }
            '{' => depth += 1,
            '}' if depth == 0 => {
                if alternatives.len() > 1 {
                    budget.group()?;
                    return Ok(Some((i, alternatives)));
                }
                return Ok(sequence(&alternatives[0], budget)?.map(|items| (i, items)));
            }
            '}' => depth -= 1,
            ',' if depth == 0 => {
                alternatives.push(String::new());
                i += 1;
                continue;
            }
            _ => {}
        }
        current.push(c);
        i += 1;
    }
    Ok(None)
}

/// A brace sequence `x..y` or `x..y..step`: numeric bounds with optional zero padding, or two
/// letters. A numeric range's count is checked against the budget before anything is
/// allocated; an i64 range can name ~1e19 entries.
fn sequence(body: &str, budget: &mut Budget) -> Result<Option<Vec<String>>, TooManyExpansions> {
    let parts: Vec<&str> = body.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return Ok(None);
    }
    let step: i64 = match parts.get(2) {
        Some(s) => match s.parse::<i64>().ok().filter(|n| *n != 0) {
            Some(n) => n.abs(),
            None => return Ok(None),
        },
        None => 1,
    };
    if let (Ok(from), Ok(to)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let padded = [parts[0], parts[1]].iter().any(|p| {
            p.trim_start_matches('-').starts_with('0') && p.trim_start_matches('-').len() > 1
        });
        let width = parts[0].len().max(parts[1].len());
        let count = (to as i128 - from as i128).unsigned_abs() / step as u128 + 1;
        budget.group()?;
        budget.check_sequence(count)?;
        let mut out = Vec::new();
        let mut n = from;
        loop {
            out.push(if padded {
                format!("{n:0width$}")
            } else {
                n.to_string()
            });
            if (from <= to && n + step > to) || (from > to && n - step < to) {
                break;
            }
            n = if from <= to { n + step } else { n - step };
        }
        return Ok(Some(out));
    }
    let (a, b) = (
        parts[0].chars().collect::<Vec<_>>(),
        parts[1].chars().collect::<Vec<_>>(),
    );
    if let (&[from], &[to]) = (a.as_slice(), b.as_slice()) {
        if from.is_ascii_alphabetic() && to.is_ascii_alphabetic() {
            let (lo, hi) = (from.min(to) as u32, from.max(to) as u32);
            let mut items: Vec<String> = (lo..=hi)
                .step_by(step as usize)
                .filter_map(char::from_u32)
                .map(String::from)
                .collect();
            if from > to {
                items.reverse();
            }
            budget.group()?;
            budget.check_sequence(items.len() as u128)?;
            return Ok(Some(items));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(line: &str) -> Rule {
        Rule::parse(line).expect("a valid line").expect("a rule")
    }

    #[test]
    fn slashless_patterns_match_the_basename_at_any_depth() {
        let r = rule("node_modules");
        assert!(r.matches("/node_modules", false).unwrap());
        assert!(r.matches("node_modules", false).unwrap());
        assert!(r.matches("a/b/node_modules", false).unwrap());
        assert!(!r.matches("a/node_modules_x", false).unwrap());
        assert!(rule("*.orig").matches("deep/er/file.orig", false).unwrap());
        assert!(rule(".npmrc").matches("/.npmrc", false).unwrap());
    }

    #[test]
    fn anchored_patterns_match_the_walkers_own_level_only() {
        let r = rule("/.git");
        assert!(r.matches("/.git", false).unwrap());
        assert!(!r.matches(".git", false).unwrap());
        assert!(!r.matches("/sub/.git", false).unwrap());
        assert!(rule("/build/config.gypi")
            .matches("/build/config.gypi", false)
            .unwrap());
    }

    #[test]
    fn negation_is_a_flag_and_the_case_is_folded() {
        let r = rule("!/readme{,.*[^~$]}");
        assert!(r.negate);
        assert!(r.matches("/README", false).unwrap());
        assert!(r.matches("/README.md", false).unwrap());
        assert!(r.matches("/Readme.txt", false).unwrap());
        assert!(!r.matches("/README.md~", false).unwrap());
        assert!(!r.matches("/README.md$", false).unwrap());
        assert!(!r.matches("/readme-first.md", false).unwrap());
        assert!(!rule("!!foo").negate);
    }

    #[test]
    fn globstar_and_partials() {
        let r = rule("**/.git/**");
        assert!(r.matches("/.git/HEAD", false).unwrap());
        assert!(r.matches("a/b/.git/objects/x", false).unwrap());
        assert!(!r.matches("a/.gitignore", false).unwrap());
        let dist = rule("!dist/**");
        assert!(dist.matches("dist/index.js", false).unwrap());
        assert!(dist.matches("dist/nested/x.d.ts", false).unwrap());
        assert!(!dist.matches("dist", false).unwrap());
        assert!(dist.matches("dist/", false).unwrap());
        let js = rule("!lib/*.js");
        assert!(js.matches("lib/a.js", false).unwrap());
        assert!(!js.matches("lib/deep/a.js", false).unwrap());
        assert!(
            js.matches("lib", true).unwrap(),
            "a directory on the way to a match"
        );
        assert!(
            !js.matches("lib/", true).unwrap(),
            "the walker asks without the slash"
        );
        assert!(!js.matches("src", true).unwrap());
    }

    #[test]
    fn wildcards_never_take_the_dot_directories() {
        assert!(rule("*").matches(".hidden", false).unwrap());
        assert!(!rule("*").matches(".", false).unwrap());
        assert!(!rule("*").matches("..", false).unwrap());
        assert!(rule("?ile").matches("file", false).unwrap());
        assert!(!rule("?ile").matches("fille", false).unwrap());
        assert!(rule("[a-c]x").matches("Bx", false).unwrap());
        assert!(!rule("[!a-c]x").matches("bx", false).unwrap());
        assert!(rule("\\*literal").matches("*literal", false).unwrap());
        assert!(!rule("\\*literal").matches("xliteral", false).unwrap());
        assert!(rule("[[:digit:]]*.log").matches("1abc.log", false).unwrap());
        assert!(!rule("[[:digit:]]*.log").matches("abc.log", false).unwrap());
        assert!(rule("[[:alpha:]-]x").matches("-x", false).unwrap());
    }

    #[test]
    fn extglobs_match_like_minimatch() {
        let keys = rule("*.@(pem|key)");
        assert!(keys.matches("secret.pem", false).unwrap());
        assert!(keys.matches("certs/secret.key", false).unwrap());
        assert!(!keys.matches("secret.pemx", false).unwrap());
        assert!(!keys.matches("secret.txt", false).unwrap());
        // A leading `!` is the negation flag even before `(`, as in minimatch: the rule then
        // matches a literal `(…)`.
        let not_js = rule("!(*.js)");
        assert!(not_js.negate);
        assert!(!not_js.matches("a.ts", false).unwrap());
        assert!(!not_js.matches("a.js", false).unwrap());
        assert!(not_js.matches("(a.js)", false).unwrap());
        let not_js = rule("a.!(js)");
        assert!(not_js.matches("a.ts", false).unwrap());
        assert!(!not_js.matches("a.js", false).unwrap());
        assert!(not_js.matches("a.jsx", false).unwrap());
        let not_ab = rule("x!(a|b)");
        assert!(!not_ab.matches("xa", false).unwrap());
        assert!(not_ab.matches("xc", false).unwrap());
        assert!(not_ab.matches("x", false).unwrap());
        let plus = rule("+(ab)");
        assert!(plus.matches("ab", false).unwrap());
        assert!(plus.matches("ababab", false).unwrap());
        assert!(!plus.matches("", false).unwrap());
        assert!(!plus.matches("aba", false).unwrap());
        let star = rule("*(a|b)c");
        assert!(star.matches("c", false).unwrap());
        assert!(star.matches("abbac", false).unwrap());
        assert!(!star.matches("abd", false).unwrap());
        let opt = rule("?(x)y");
        assert!(opt.matches("y", false).unwrap());
        assert!(opt.matches("xy", false).unwrap());
        assert!(!opt.matches("xxy", false).unwrap());
        assert!(rule("@(a|b)c").matches("bc", false).unwrap());
        assert!(!rule("@(a|b)c").matches("c", false).unwrap());
        assert!(
            rule("@(a|@(b|c))").matches("c", false).unwrap(),
            "nested groups"
        );
        assert!(
            rule("x*(").matches("x*(", false).unwrap(),
            "an unclosed group is literal"
        );
        assert!(!rule("@(a|b)").matches(".", false).unwrap());
    }

    #[test]
    fn braces_expand_and_relative_rules_are_recognized() {
        let expand = |pattern: &str| expand_braces(pattern, &mut Budget::fresh()).unwrap();
        assert_eq!(expand("a{b,c}d"), vec!["abd", "acd"]);
        assert_eq!(expand("x{,.y}"), vec!["x", "x.y"]);
        assert_eq!(expand("a{b}c"), vec!["a{b}c"]);
        assert_eq!(expand("a{b{c,d},e}"), vec!["abc", "abd", "ae"]);
        assert_eq!(expand("v{1..3}"), vec!["v1", "v2", "v3"]);
        assert_eq!(expand("{01..03}"), vec!["01", "02", "03"]);
        assert_eq!(expand("{3..1}"), vec!["3", "2", "1"]);
        assert_eq!(expand("{1..6..2}"), vec!["1", "3", "5"]);
        assert_eq!(expand("{a..c}"), vec!["a", "b", "c"]);
        assert_eq!(expand("{a..b..c}"), vec!["{a..b..c}"]);
        assert!(rule("foo").is_relative());
        assert!(rule("foo/").is_relative());
        assert!(!rule("foo/bar").is_relative());
        assert!(rule("{foo,bar/baz}").is_relative());
    }

    #[test]
    fn a_brace_bomb_is_an_error_not_an_allocation() {
        // `{1..100000000}` names a hundred million entries; the count is arithmetic, so the
        // error comes back before anything is allocated.
        assert!(Rule::parse("{1..100000000}").is_err());
        assert!(Rule::parse("{1..10001}").is_err());
        assert!(Rule::parse("{1..10000}").is_ok());
        assert!(Rule::parse("{9999..1}").is_ok());
        // Multiplicative groups are capped the same way: 3^16 alternatives.
        let bomb = "{a,b,c}".repeat(16);
        assert!(Rule::parse(&bomb).is_err());
        let fine = "{a,b,c}".repeat(4);
        assert!(Rule::parse(&fine).is_ok());
        // The group budget bounds the expansion's recursion depth.
        let deep = "{a,b}".repeat(101);
        assert!(Rule::parse(&deep).is_err());
        // A range outside i64 is not a sequence at all and stays a literal, as before.
        assert_eq!(
            expand_braces("{0..18446744073709551616}", &mut Budget::fresh()).unwrap(),
            vec!["{0..18446744073709551616}"]
        );
        // The error names the offending line.
        let error = Rule::parse("dist/{1..100000000}.tgz")
            .unwrap_err()
            .to_string();
        assert!(error.contains("dist/{1..100000000}.tgz"), "{error}");
        assert!(error.contains("brace expansion"), "{error}");
    }

    #[test]
    fn a_pathological_pattern_is_an_error_not_a_hang() {
        // Both need ~1e13 backtracking steps against a 200-char name; the budget errors in
        // milliseconds instead.
        let long = "b".repeat(200);
        let error = rule("*b*b*b*b*b*b*b*c")
            .matches(&long, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("*b*b*b*b*b*b*b*c"), "{error}");
        assert!(error.contains("step limit"), "{error}");
        assert!(rule("+(b|bb)+(b|bb)+(b|bb)+(b|bb)c")
            .matches(&long, false)
            .is_err());
        // Stock rules sit orders of magnitude under the budget.
        for fine in [
            "!/readme{,.*[^~$]}",
            "**/node_modules/**",
            "*.@(pem|key)",
            "lib/*.js",
        ] {
            assert!(
                rule(fine).matches("deep/er/path/README.md", false).is_ok(),
                "{fine}"
            );
        }
        // A nasty-but-legal pattern still evaluates correctly: 40 chars of `x` match `*x*x*x*`
        // and fail `*x*x*x*y`, both well inside the budget.
        let xs = "x".repeat(40);
        assert!(rule("*x*x*x*").matches(&xs, false).unwrap());
        assert!(!rule("*x*x*x*y").matches(&xs, false).unwrap());
    }
}
