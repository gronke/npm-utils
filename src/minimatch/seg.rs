//! One path segment as a program: the negation-free runs compile to linear regexes and the
//! `!(…)` groups evaluate as zero-width checks on the remainder, so no backtracking regex
//! engine is involved anywhere. Emission lives in ast.rs ([`Ast::into_seg_program`]); the
//! JavaScript shape (a lookahead inside one big regex per segment) is not kept, the language is.

use super::ast::STAR;
use super::chars::is_dots;
use super::Options;

/// A segment matcher: the pieces of one path segment, applied left to right.
#[derive(Debug)]
pub(super) struct SegProgram {
    pieces: Vec<SegPiece>,
}

/// Building a program failed: a strict-mode refusal, or a translated run the engine rejects
/// (a bug in the emission, or a class the engine cannot hold).
#[derive(Debug)]
pub(super) enum SegError {
    Refusal(super::quirks::Refusal),
    Regex(String),
}

impl From<super::quirks::Refusal> for SegError {
    fn from(refusal: super::quirks::Refusal) -> SegError {
        SegError::Refusal(refusal)
    }
}

/// The anchor a run compiles with, mirroring how the JavaScript's `^…$` binds a bare
/// alternation's branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Anchor {
    /// `^…$`: matches the slice exactly.
    Exact,
    /// `^…`: matches a prefix of the slice (the alternation's first branch).
    Prefix,
    /// `…`: matches anywhere in the slice (a middle branch).
    Free,
    /// `…$`: matches a suffix of the slice (the alternation's last branch).
    Suffix,
}

/// The regex crate's nesting allowance for a run; its default (250) is far beyond what the
/// extglob nesting budget can emit, and the budget is the documented limit.
const REGEX_NEST_LIMIT: u32 = 250;

#[derive(Debug)]
pub(super) enum SegPiece {
    /// A maximal negation-free run of the segment as one anchored regex.
    Run { re: regex::Regex, src: String },
    /// A run's source before [`SegProgram::compile`] turns it into a [`SegPiece::Run`]: the
    /// emission assembles and folds text, and every run compiles once, at the end.
    Text { src: String, anchor: Anchor },
    /// A `!(…)` group: zero-width, the remainder to the end of the segment must match none of
    /// the alternatives.
    Neg(Vec<SegProgram>),
    /// A group whose alternatives hold a negation further down, so it cannot be one regex:
    /// each alternative applied as a prefix, the pieces after the group continue from there.
    Any(Vec<SegProgram>),
    /// The quirk `escaped-pipe-alternates`: a run's top-level `|` splits the whole segment,
    /// where a group splits only itself. Evaluated like [`SegPiece::Any`], rendered as the
    /// JavaScript's bare alternation.
    Alternation(Vec<SegProgram>),
    /// Such a group under `*`, `+` or `?`: the alternation applied to fixpoint (the positions
    /// set is finite, so empty iterations cannot loop). `first` is the dot-constrained first
    /// iteration minimatch emits for a repeat at the start of a segment.
    Repeat {
        prog: Box<SegProgram>,
        first: Option<Box<SegProgram>>,
        min: usize,
        max: Option<usize>,
    },
    /// A top-level `|` in the quirk's reading, before the assembly turns the program into an
    /// [`SegPiece::Alternation`]; the branch texts it splits into.
    Pipe(Vec<String>),
    /// minimatch's `(?!\.)`: the text from here must not start with `.`.
    NoDotStart,
    /// minimatch's `(?!(?:^|/)\.\.?(?:$|/))` within a segment: the text from here must not be
    /// exactly `.` or `..`.
    NoTraversal,
    /// minimatch's `(?:$|\/)` tail of a negation alternative: holds at the end of the segment.
    EndOfSegment,
}

/// The evaluator ran past its step allowance.
#[derive(Debug)]
pub(super) struct OutOfSteps;

impl SegProgram {
    pub(super) fn new(pieces: Vec<SegPiece>) -> SegProgram {
        SegProgram { pieces }
    }

    /// Compile every [`SegPiece::Text`] in the program and its sub-programs into a
    /// [`SegPiece::Run`]: the one place a regex is built, once per run, after the emission
    /// has folded groups into text and the assembly has set every anchor.
    pub(super) fn compile(self, options: &Options) -> Result<SegProgram, SegError> {
        let pieces = self
            .pieces
            .into_iter()
            .map(|piece| piece.compile(options))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SegProgram { pieces })
    }

    /// `true` when the whole text matches. `steps` is the backstop for the polynomial worst
    /// case (positions times pieces times text length, nested negations multiplying).
    pub(super) fn is_match(&self, text: &str, steps: &mut u64) -> Result<bool, OutOfSteps> {
        Ok(self
            .run(text, &[0], Some(text.len()), steps)?
            .contains(&text.len()))
    }

    /// All positions after matching the pieces from each of `positions` (prefix semantics, for
    /// `Any` and `Repeat` above). With `end`, only that position counts after the last
    /// consuming piece, so a closing run is one regex call per start instead of one per
    /// candidate end; the zero-width pieces behind it filter as before.
    fn run(
        &self,
        text: &str,
        positions: &[usize],
        end: Option<usize>,
        steps: &mut u64,
    ) -> Result<Vec<usize>, OutOfSteps> {
        let last = self.pieces.iter().rposition(SegPiece::consumes);
        let needs_ends = self.pieces.iter().enumerate().any(|(i, piece)| {
            matches!(piece, SegPiece::Run { .. }) && (end.is_none() || Some(i) != last)
        });
        let ends: Vec<usize> = if needs_ends {
            std::iter::once(0)
                .chain(text.char_indices().skip(1).map(|(i, _)| i))
                .chain(std::iter::once(text.len()))
                .collect()
        } else {
            Vec::new()
        };
        let mut current: Vec<usize> = positions.to_vec();
        for (i, piece) in self.pieces.iter().enumerate() {
            let want = if Some(i) == last { end } else { None };
            current = piece.step(text, &current, &ends, want, steps)?;
            if current.is_empty() {
                break;
            }
        }
        Ok(current)
    }
}

fn compile_all(programs: Vec<SegProgram>, options: &Options) -> Result<Vec<SegProgram>, SegError> {
    programs
        .into_iter()
        .map(|program| program.compile(options))
        .collect()
}

/// One run as a regex: the source wrapped for its anchor, case folded under `nocase`.
fn compile_run(src: &str, anchor: Anchor, options: &Options) -> Result<regex::Regex, SegError> {
    let wrapped = match anchor {
        Anchor::Exact => format!("^{src}$"),
        Anchor::Prefix => format!("^{src}"),
        Anchor::Free => src.to_string(),
        Anchor::Suffix => format!("{src}$"),
    };
    regex::RegexBuilder::new(&wrapped)
        .case_insensitive(options.nocase)
        .nest_limit(REGEX_NEST_LIMIT)
        .build()
        .map_err(|e| SegError::Regex(e.to_string()))
}

impl SegPiece {
    fn compile(self, options: &Options) -> Result<SegPiece, SegError> {
        Ok(match self {
            SegPiece::Text { src, anchor } => SegPiece::Run {
                re: compile_run(&src, anchor, options)?,
                src,
            },
            SegPiece::Neg(alternatives) => SegPiece::Neg(compile_all(alternatives, options)?),
            SegPiece::Any(alternatives) => SegPiece::Any(compile_all(alternatives, options)?),
            SegPiece::Alternation(branches) => {
                SegPiece::Alternation(compile_all(branches, options)?)
            }
            SegPiece::Repeat {
                prog,
                first,
                min,
                max,
            } => SegPiece::Repeat {
                prog: Box::new(prog.compile(options)?),
                first: match first {
                    Some(first) => Some(Box::new(first.compile(options)?)),
                    None => None,
                },
                min,
                max,
            },
            other => other,
        })
    }

    /// The source of a run, compiled or not.
    fn src(&self) -> Option<&str> {
        match self {
            SegPiece::Run { src, .. } | SegPiece::Text { src, .. } => Some(src),
            _ => None,
        }
    }

    /// Whether the piece can advance the position; the zero-width checks cannot. `Pipe` never
    /// reaches evaluation (the assembly replaces it with an [`SegPiece::Alternation`]).
    fn consumes(&self) -> bool {
        matches!(
            self,
            SegPiece::Run { .. }
                | SegPiece::Text { .. }
                | SegPiece::Any(_)
                | SegPiece::Alternation(_)
                | SegPiece::Repeat { .. }
        )
    }

    /// All positions after this piece, given the positions before it; with `end`, only that
    /// position.
    fn step(
        &self,
        text: &str,
        positions: &[usize],
        ends: &[usize],
        end: Option<usize>,
        steps: &mut u64,
    ) -> Result<Vec<usize>, OutOfSteps> {
        let mut next: Vec<usize> = Vec::new();
        for &p in positions {
            match self {
                SegPiece::Run { re, .. } => match end {
                    Some(e) => {
                        if e >= p {
                            spend(steps)?;
                            if re.is_match(&text[p..e]) {
                                next.push(e);
                            }
                        }
                    }
                    None => {
                        for &e in ends {
                            if e < p {
                                continue;
                            }
                            spend(steps)?;
                            if re.is_match(&text[p..e]) {
                                next.push(e);
                            }
                        }
                    }
                },
                SegPiece::Neg(alternatives) => {
                    spend(steps)?;
                    let mut hit = false;
                    for alt in alternatives {
                        if alt.is_match(&text[p..], steps)? {
                            hit = true;
                            break;
                        }
                    }
                    if !hit {
                        next.push(p);
                    }
                }
                SegPiece::Pipe(_) | SegPiece::Text { .. } => {
                    unreachable!("the assembly and the compile pass replace the markers")
                }
                SegPiece::Any(alternatives) | SegPiece::Alternation(alternatives) => {
                    for alt in alternatives {
                        next.extend(alt.run(text, &[p], end, steps)?);
                    }
                }
                SegPiece::Repeat {
                    prog,
                    first,
                    min,
                    max,
                } => {
                    // `(?:first)(?:prog)*?`: the first iteration may be the dot-constrained
                    // start, and its results seed the plain iterations even where it matched
                    // nothing, as the JavaScript's regex reads; `seen` holds the positions
                    // `prog` has been applied from, so the fixpoint ends.
                    let mut reached: Vec<usize> = if *min == 0 { vec![p] } else { Vec::new() };
                    spend(steps)?;
                    let mut frontier =
                        first
                            .as_deref()
                            .unwrap_or(prog)
                            .run(text, &[p], None, steps)?;
                    frontier.sort_unstable();
                    frontier.dedup();
                    let mut applied = 1usize;
                    if applied >= *min {
                        reached.extend(frontier.iter().copied());
                    }
                    let mut seen = frontier.clone();
                    while !frontier.is_empty() && !max.is_some_and(|m| applied >= m) {
                        spend(steps)?;
                        let mut grown: Vec<usize> = Vec::new();
                        for &f in &frontier {
                            grown.extend(prog.run(text, &[f], None, steps)?);
                        }
                        applied += 1;
                        grown.sort_unstable();
                        grown.dedup();
                        grown.retain(|e| !seen.contains(e));
                        if applied >= *min {
                            reached.extend(grown.iter().copied());
                        }
                        seen.extend(grown.iter().copied());
                        frontier = grown;
                    }
                    next.extend(reached);
                }
                SegPiece::NoDotStart => {
                    if !text[p..].starts_with('.') {
                        next.push(p);
                    }
                }
                SegPiece::NoTraversal => {
                    // The JavaScript's `(?:^|/)` can only hold at the start of a segment
                    // (segments contain no slashes), so the guard fires at position 0 only.
                    if p != 0 || !is_dots(text) {
                        next.push(p);
                    }
                }
                SegPiece::EndOfSegment => {
                    if p == text.len() {
                        next.push(p);
                    }
                }
            }
        }
        next.sort_unstable();
        next.dedup();
        Ok(next)
    }
}

fn spend(steps: &mut u64) -> Result<(), OutOfSteps> {
    *steps = steps.checked_sub(1).ok_or(OutOfSteps)?;
    Ok(())
}

impl SegProgram {
    /// The program rendered back to the text the JavaScript would emit, for tests and error
    /// output. Negations render as the lookahead group they stand for (`Neg` followed by its
    /// dot guard and star run becomes `(?:(?!…))…[^/]*?`); a repeat renders its quantifier and
    /// the start-split body. This is a debug view; matching never parses it.
    pub(super) fn render(&self) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < self.pieces.len() {
            if let SegPiece::Neg(alternatives) = &self.pieces[i] {
                // The group's tail: an optional dot guard and its star, which opens the run
                // that follows; the rest of that run comes after the group's close.
                let mut close = String::new();
                let mut rest = "";
                let mut advance = i + 1;
                if matches!(self.pieces.get(advance), Some(SegPiece::NoDotStart)) {
                    close.push_str("(?!\\.)");
                    advance += 1;
                }
                if let Some(src) = self.pieces.get(advance).and_then(SegPiece::src) {
                    if let Some(after) = src.strip_prefix(STAR) {
                        close.push_str(STAR);
                        rest = after;
                        advance += 1;
                    }
                }
                out.push_str("(?:(?!(?:");
                render_join(&mut out, alternatives, "|");
                out.push_str("))");
                out.push_str(&close);
                out.push(')');
                out.push_str(rest);
                i = advance;
                continue;
            }
            self.pieces[i].render_into(&mut out);
            i += 1;
        }
        out
    }

    /// `true` when the program holds nothing (matches only the empty text).
    pub(super) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// The run's source when the program is exactly one bare, exactly anchored run (the fold
    /// in ast.rs).
    pub(super) fn single_run_src(&self) -> Option<&str> {
        match &self.pieces[..] {
            [SegPiece::Text {
                src,
                anchor: Anchor::Exact,
            }] => Some(src),
            _ => None,
        }
    }

    /// The alternatives' text when the program is a lone `Any` (a repeat body renders them
    /// without the group's own wrapper, as the JavaScript joins them).
    fn inner_render(&self) -> String {
        match &self.pieces[..] {
            [SegPiece::Any(alternatives)] => {
                let mut out = String::new();
                render_join(&mut out, alternatives, "|");
                out
            }
            _ => self.render(),
        }
    }
}

impl SegPiece {
    fn render_into(&self, out: &mut String) {
        match self {
            SegPiece::Run { src, .. } | SegPiece::Text { src, .. } => out.push_str(src),
            SegPiece::Neg(alternatives) => {
                out.push_str("(?:(?!");
                render_join(out, alternatives, "|");
                out.push(')');
            }
            SegPiece::Any(alternatives) => {
                out.push_str("(?:");
                render_join(out, alternatives, "|");
                out.push(')');
            }
            SegPiece::Alternation(branches) => {
                render_join(out, branches, "|");
            }
            SegPiece::Pipe(_) => unreachable!("a Pipe marker must become an Alternation"),
            SegPiece::Repeat {
                prog,
                first,
                min,
                max,
            } => {
                if let Some(first) = first {
                    out.push_str("(?:(?:");
                    out.push_str(&first.inner_render());
                    out.push_str(")(?:");
                    out.push_str(&prog.inner_render());
                    out.push_str(")*?");
                    if *min == 0 {
                        out.push_str(")?");
                    } else {
                        out.push(')');
                    }
                } else {
                    out.push_str("(?:");
                    out.push_str(&prog.inner_render());
                    out.push(')');
                    out.push_str(match (min, max) {
                        (0, None) => "*",
                        (1, None) => "+",
                        (0, Some(1)) => "?",
                        _ => "{n,m}",
                    });
                }
            }
            SegPiece::NoDotStart => out.push_str("(?!\\.)"),
            SegPiece::NoTraversal => out.push_str("(?!(?:^|/)\\.\\.?(?:$|/))"),
            SegPiece::EndOfSegment => out.push_str("(?:$|\\/)"),
        }
    }
}

fn render_join(out: &mut String, programs: &[SegProgram], sep: &str) {
    for (i, program) in programs.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        out.push_str(&program.render());
    }
}
