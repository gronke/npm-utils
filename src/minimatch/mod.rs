//! A port of minimatch 10.2.5, the glob matcher of npm, npm-packlist and ignore-walk, with
//! negation-free runs on the `regex` crate and negations evaluated without a backtracking
//! engine.
//!
//! Minimatch translates a glob to one regular expression per path segment and matches the
//! segments of a path against them, with `**` spanning any number of segments. This module ports
//! that translation (ast.js and brace-expressions.js), the brace expansion (brace-expansion 5.0.9,
//! held by the recorded fixture to the 5.0.12 that minimatch 10.2.5 resolves), the segment matcher
//! (`matchOne`), `matchBase`, the `dot` rules, negation and comments, and pins the port to the
//! JavaScript implementation with a recorded fixture (`tests/minimatch.rs`).
//!
//! Two things differ from the JavaScript on purpose. Budgets: brace expansion stops at
//! [`Options::max_brace_expansions`], [`Options::max_brace_groups`] and
//! [`Options::max_brace_length`] with an error instead of a silent truncation, the segment
//! evaluator stops at [`Options::max_match_steps`] with an error where the JavaScript hangs,
//! a path crossing more than [`Options::max_globstar_recursion`] `**` sections is an error where
//! minimatch answers `false`, more than [`Options::max_extglob_nesting`] nested extglob groups is
//! an error where npm exhausts its stack, and a pattern growing past
//! [`Options::max_extglob_nodes`] nodes once each `!()` group has absorbed what follows it is an
//! error where npm's tree doubles per group without limit.
//! Characters: the port works on Unicode scalar values where JavaScript counts UTF-16 units, so
//! `?` matches one astral character (`😀`) that JavaScript needs two of; `nocase` folds case with
//! the regex engine's Unicode simple folding, where JavaScript canonicalizes through `toUpperCase`
//! without the `u` flag and never folds a non-ASCII character onto an ASCII one (`k` and the Kelvin
//! sign stay apart there and meet here); and the `u`-flag `SyntaxError` a POSIX class beside a
//! literal `-`, `,`, `!`, `#` or space raises in JavaScript is not an error here.
//!
//! Out of scope: Windows paths and `windowsPathsNoEscape`, `preserveMultipleSlashes`,
//! `optimizationLevel` 2, `nocaseMagicOnly`, `makeRe`.

mod ast;
mod braces;
mod chars;
mod class;
mod errors;
mod escaping;
mod matcher;
mod options;
mod quirks;
mod seg;
#[cfg(test)]
mod tests;

pub use errors::{BraceLimit, Error};
pub use escaping::{escape, unescape};
pub use options::{Options, MAX_PATTERN_LENGTH};
pub use quirks::Quirk;

/// minimatch's `braceExpand`: the strings a pattern's braces expand to, in Bash's order, with
/// duplicates; the pattern itself when it holds no brace pair or under `nobrace`.
pub fn brace_expand(pattern: &str, options: Options) -> Result<Vec<String>, Error> {
    braces::brace_expand(pattern, &options)
}

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
        let (glob, stripped) = self.parse_negate();
        if stripped > 0 && glob.starts_with('(') && !self.options.keeps(Quirk::NegationBeforeGroup)
        {
            return Err(Error::Refused {
                pattern: self.pattern.clone(),
                quirk: Quirk::NegationBeforeGroup,
                reason: "a leading `!(` is negation in npm and a group in Bash; write `!@(…)` to \
                         negate a group match or `@(!(…))` for the group"
                    .to_string(),
            });
        }
        let mut seen = std::collections::HashSet::new();
        self.glob_set = braces::brace_expand(&glob, &self.options)?
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .collect();
        // Step 3: each one becomes a series of segment matchers.
        let raw: Vec<Vec<String>> = self.glob_set.iter().map(|s| slash_split(s)).collect();
        self.glob_parts = self.preprocess(raw);
        let mut set = Vec::with_capacity(self.glob_parts.len());
        // One node budget for the whole pattern: brace expansion multiplies the segments.
        let mut budget = ast::NodeBudget::new(self.options.max_extglob_nodes);
        for parts in &self.glob_parts {
            let mut pattern = Vec::with_capacity(parts.len());
            for segment in parts {
                pattern.push(self.parse(segment, &mut budget)?);
            }
            set.push(pattern);
        }
        self.set = set;
        Ok(())
    }

    /// `parseNegate`: leading `!`s toggle negation and leave the pattern; also how many left.
    fn parse_negate(&mut self) -> (String, usize) {
        if self.options.nonegate {
            return (self.pattern.clone(), 0);
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
        (self.pattern.chars().skip(offset).collect(), offset)
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
                        && prev.is_some_and(|p| !p.is_empty() && !chars::is_dots(p) && p != "**")
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
    fn parse(&self, segment: &str, budget: &mut ast::NodeBudget) -> Result<matcher::Part, Error> {
        if segment == "**" {
            return Ok(matcher::Part::GlobStar);
        }
        if segment.is_empty() {
            return Ok(matcher::Part::Literal(String::new()));
        }
        let fast = if self.options.keeps(Quirk::RawExtensionFastPath) {
            matcher::FastTest::of(segment, &self.options)
        } else {
            None
        };
        let mm = ast::Ast::from_glob(segment, self.options, budget)
            .map_err(|e| match e {
                ast::AstError::Refusal(refusal) => refusal.into_error(&self.pattern),
                ast::AstError::Nesting => Error::Nesting {
                    pattern: self.pattern.clone(),
                    limit: self.options.max_extglob_nesting,
                },
                ast::AstError::Nodes => Error::Nodes {
                    pattern: self.pattern.clone(),
                    limit: self.options.max_extglob_nodes,
                },
            })?
            .into_mm_pattern(&self.pattern)?;
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
                        matcher::Fault::Steps => Error::Steps {
                            pattern: self.pattern.clone(),
                            limit: options.max_match_steps,
                        },
                        matcher::Fault::GlobstarRecursion => Error::GlobstarRecursion {
                            pattern: self.pattern.clone(),
                            limit: options.max_globstar_recursion,
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
