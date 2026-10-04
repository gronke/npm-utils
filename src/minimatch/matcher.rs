//! The segment matcher: index.js `matchOne`, `#matchGlobstar`, `#matchGlobStarBodySections`
//! and `#matchOne` ported, over the parts `parse` produces for each segment.

use super::ast::MmPattern;
use super::chars::{is_dots, FAST_PATH_BREAKERS};
use super::seg::SegProgram;
use super::Options;

/// One segment of a pattern: a literal to compare, the globstar, or a program.
#[derive(Debug)]
pub(super) enum Part {
    Literal(String),
    GlobStar,
    Program {
        prog: SegProgram,
        /// The shortcut minimatch installs as `test` for the most common shapes.
        fast: Option<FastTest>,
    },
}

/// The optimized checks for `*`, `*.<ext>`, `?<ext>`, `*.*` and `.*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FastTest {
    Star {
        dot: bool,
    },
    StarDotExt {
        ext: String,
        dot: bool,
        nocase: bool,
    },
    Qmarks {
        len: usize,
        ext: String,
        dot: bool,
        nocase: bool,
    },
    StarDotStar {
        dot: bool,
    },
    DotStar,
}

/// What a match can fail with; `Minimatch` names the pattern.
#[derive(Debug)]
pub(super) enum Fault {
    Steps,
    GlobstarRecursion,
}

impl FastTest {
    /// The shortcut for a segment, if it has one of the shapes.
    pub(super) fn of(pattern: &str, options: &Options) -> Option<FastTest> {
        let (dot, nocase) = (options.dot, options.nocase);
        let ext_ok = |ext: &str| !ext.chars().any(|c| FAST_PATH_BREAKERS.contains(c));
        let lower = |ext: &str| {
            if nocase {
                ext.to_lowercase()
            } else {
                ext.to_string()
            }
        };
        let stars = pattern.trim_start_matches('*');
        if pattern.starts_with('*') {
            // starRE, then starDotExtRE
            if stars.is_empty() {
                return Some(FastTest::Star { dot });
            }
            if ext_ok(stars) {
                return Some(FastTest::StarDotExt {
                    ext: lower(stars),
                    dot,
                    nocase,
                });
            }
        }
        let qmarks = pattern.trim_start_matches('?');
        if pattern.starts_with('?') && ext_ok(qmarks) {
            return Some(FastTest::Qmarks {
                len: pattern.chars().count(),
                ext: lower(qmarks),
                dot,
                nocase,
            });
        }
        if pattern.starts_with('*') {
            // starDotStarRE
            if let Some(rest) = stars.strip_prefix('.') {
                if !rest.is_empty() && rest.chars().all(|c| c == '*') {
                    return Some(FastTest::StarDotStar { dot });
                }
            }
        }
        if let Some(rest) = pattern.strip_prefix('.') {
            if !rest.is_empty() && rest.chars().all(|c| c == '*') {
                return Some(FastTest::DotStar);
            }
        }
        None
    }

    pub(super) fn test(&self, f: &str) -> bool {
        match self {
            FastTest::Star { dot: true } => !f.is_empty() && !is_dots(f),
            FastTest::Star { dot: false } => !f.is_empty() && !f.starts_with('.'),
            FastTest::StarDotExt { ext, dot, nocase } => {
                let ends = if *nocase {
                    f.to_lowercase().ends_with(ext.as_str())
                } else {
                    f.ends_with(ext.as_str())
                };
                ends && (*dot || !f.starts_with('.'))
            }
            FastTest::Qmarks {
                len,
                ext,
                dot,
                nocase,
            } => {
                let noext = f.chars().count() == *len
                    && if *dot {
                        !is_dots(f)
                    } else {
                        !f.starts_with('.')
                    };
                if ext.is_empty() {
                    return noext;
                }
                noext
                    && if *nocase {
                        f.to_lowercase().ends_with(ext.as_str())
                    } else {
                        f.ends_with(ext.as_str())
                    }
            }
            FastTest::StarDotStar { dot: true } => !is_dots(f) && f.contains('.'),
            FastTest::StarDotStar { dot: false } => !f.starts_with('.') && f.contains('.'),
            FastTest::DotStar => !is_dots(f) && f.starts_with('.'),
        }
    }
}

impl Part {
    pub(super) fn from_pattern(mm: MmPattern, fast: Option<FastTest>) -> Part {
        match mm {
            MmPattern::Literal(s) => Part::Literal(s),
            MmPattern::Program(prog) => Part::Program { prog, fast },
        }
    }

    fn test(&self, f: &str, options: &Options) -> Result<bool, Fault> {
        match self {
            Part::Literal(s) => Ok(f == s),
            Part::GlobStar => Ok(false),
            Part::Program { prog, fast } => match fast {
                Some(fast) => Ok(fast.test(f)),
                None => {
                    let mut steps = options.max_match_steps as u64;
                    prog.is_match(f, &mut steps).map_err(|_| Fault::Steps)
                }
            },
        }
    }
}

/// `matchOne`: a split path against one pattern of the set.
pub(super) fn match_one(
    file: &[String],
    pattern: &[Part],
    partial: bool,
    options: &Options,
) -> Result<bool, Fault> {
    if pattern.iter().any(|p| matches!(p, Part::GlobStar)) {
        return match_globstar(file, pattern, partial, options);
    }
    match_plain(file, pattern, partial, 0, 0, options)
}

/// `#matchOne`: segment by segment, no globstar involved.
fn match_plain(
    file: &[String],
    pattern: &[Part],
    partial: bool,
    file_index: usize,
    pattern_index: usize,
    options: &Options,
) -> Result<bool, Fault> {
    let (fl, pl) = (file.len(), pattern.len());
    let (mut fi, mut pi) = (file_index, pattern_index);
    while fi < fl && pi < pl {
        if !pattern[pi].test(&file[fi], options)? {
            return Ok(false);
        }
        fi += 1;
        pi += 1;
    }
    if fi == fl && pi == pl {
        // Ran out of pattern and file at the same time: an exact hit.
        Ok(true)
    } else if fi == fl {
        // Ran out of file with pattern left: fine during a walk.
        Ok(partial)
    } else if pi == pl {
        // Ran out of pattern with file left: only the trailing empty segment of a path with a
        // trailing slash may remain, so `a/*` matches `a/b/`.
        Ok(fi + 1 == fl && file[fi].is_empty())
    } else {
        Ok(false)
    }
}

/// `#matchGlobstar`: head, globstar-delimited body sections, tail.
fn match_globstar(
    file: &[String],
    pattern: &[Part],
    partial: bool,
    options: &Options,
) -> Result<bool, Fault> {
    let is_gs = |p: &Part| matches!(p, Part::GlobStar);
    let firstgs = pattern.iter().position(is_gs).unwrap_or(pattern.len());
    let lastgs = pattern.iter().rposition(is_gs).unwrap_or(pattern.len());
    let head = &pattern[..firstgs];
    let (body, tail): (&[Part], &[Part]) = if partial {
        (&pattern[firstgs + 1..], &[])
    } else if firstgs < lastgs {
        (&pattern[firstgs + 1..lastgs], &pattern[lastgs + 1..])
    } else {
        (&[], &pattern[lastgs + 1..])
    };
    let mut file_index = 0usize;
    if !head.is_empty() {
        let start = file_index.min(file.len());
        let end = (file_index + head.len()).min(file.len());
        if !match_plain(&file[start..end], head, partial, 0, 0, options)? {
            return Ok(false);
        }
        file_index += head.len();
    }
    // The tail, if any, must match the end.
    let mut file_tail_match = 0usize;
    if !tail.is_empty() {
        if tail.len() + file_index > file.len() {
            return Ok(false);
        }
        let mut tail_start = file.len() - tail.len();
        if match_plain(file, tail, partial, tail_start, 0, options)? {
            file_tail_match = tail.len();
        } else {
            // An affordance for `a/**/*` matching `a/b/`: without the trailing '' segment.
            if file.last().is_some_and(|f| !f.is_empty()) || file_index + tail.len() == file.len() {
                return Ok(false);
            }
            tail_start -= 1;
            if !match_plain(file, tail, partial, tail_start, 0, options)? {
                return Ok(false);
            }
            file_tail_match = tail.len() + 1;
        }
    }
    let bad_dot = |f: &str| is_dots(f) || (!options.dot && f.starts_with('.'));
    if body.is_empty() {
        // `a/**/b`: only verify there are no bad dots in between; with no tail, something must
        // follow the head.
        let mut saw_some = file_tail_match != 0;
        for f in &file[file_index.min(file.len())..file.len().saturating_sub(file_tail_match)] {
            saw_some = true;
            if bad_dot(f) {
                return Ok(false);
            }
        }
        return Ok(partial || saw_some);
    }
    // The body sections and the last position each may start at.
    let mut segments: Vec<(&[Part], isize)> = Vec::new();
    let mut non_gs_sums = vec![0usize];
    let mut non_gs_parts = 0usize;
    let mut section_start = 0usize;
    for (i, b) in body.iter().enumerate() {
        if is_gs(b) {
            segments.push((&body[section_start..i], 0));
            section_start = i + 1;
            non_gs_sums.push(non_gs_parts);
        } else {
            non_gs_parts += 1;
        }
    }
    segments.push((&body[section_start..], 0));
    let file_length = file.len() - file_tail_match;
    let mut i = segments.len() - 1;
    for seg in &mut segments {
        seg.1 = file_length as isize - (non_gs_sums[i] + seg.0.len()) as isize;
        i = i.wrapping_sub(1);
    }
    let hit = match_body_sections(
        file,
        &segments,
        file_index,
        0,
        partial,
        0,
        file_tail_match != 0,
        options,
    )?;
    Ok(hit == Some(true))
}

/// `#matchGlobStarBodySections`: `Some(false)` for "not here", `None` for "not matching, no
/// point continuing".
#[allow(clippy::too_many_arguments)]
fn match_body_sections(
    file: &[String],
    segments: &[(&[Part], isize)],
    file_index: usize,
    body_index: usize,
    partial: bool,
    depth: usize,
    saw_tail: bool,
    options: &Options,
) -> Result<Option<bool>, Fault> {
    let bad_dot = |f: &str| is_dots(f) || (!options.dot && f.starts_with('.'));
    let Some(&(body, after)) = segments.get(body_index) else {
        // No section left: just make sure there are no bad dots.
        let mut saw_tail = saw_tail;
        for f in &file[file_index.min(file.len())..] {
            saw_tail = true;
            if bad_dot(f) {
                return Ok(Some(false));
            }
        }
        return Ok(Some(saw_tail));
    };
    let mut file_index = file_index;
    while file_index as isize <= after && file_index <= file.len() {
        let end = (file_index + body.len()).min(file.len());
        let m = match_plain(&file[..end], body, partial, file_index, 0, options)?;
        if m {
            if depth >= options.max_globstar_recursion {
                return Err(Fault::GlobstarRecursion);
            }
            let sub = match_body_sections(
                file,
                segments,
                file_index + body.len(),
                body_index + 1,
                partial,
                depth + 1,
                saw_tail,
                options,
            )?;
            if sub != Some(false) {
                return Ok(sub);
            }
        }
        if file.get(file_index).is_some_and(|f| bad_dot(f)) {
            return Ok(Some(false));
        }
        file_index += 1;
    }
    // Walked off: no point continuing.
    Ok(if partial { Some(true) } else { None })
}
