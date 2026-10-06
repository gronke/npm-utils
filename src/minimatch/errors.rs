//! The error type: why a pattern was refused or a match given up.

use super::options::MAX_PATTERN_LENGTH;
use super::quirks::Quirk;

/// Which brace budget a pattern crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BraceLimit {
    /// More expanded strings than [`Options::max_brace_expansions`](super::Options::max_brace_expansions).
    Expansions(usize),
    /// More brace groups than [`Options::max_brace_groups`](super::Options::max_brace_groups).
    Groups(usize),
    /// More characters in the expansions than [`Options::max_brace_length`](super::Options::max_brace_length).
    Length(usize),
}

/// Why a pattern was refused or a match given up. The message starts with the quoted pattern,
/// so a caller matching many rules can report the one at fault as is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Brace expansion crossed a budget.
    Braces { pattern: String, limit: BraceLimit },
    /// The segment evaluator spent [`Options::max_match_steps`](super::Options::max_match_steps) steps on one segment.
    Steps { pattern: String, limit: usize },
    /// A translated run the engine refused: a class it cannot hold, a run past its compiled-size
    /// limit (10 MB), or a bug in the port.
    Regex { pattern: String, source: String },
    /// More `**` sections than [`Options::max_globstar_recursion`](super::Options::max_globstar_recursion).
    GlobstarRecursion { pattern: String, limit: usize },
    /// More nested extglob groups than [`Options::max_extglob_nesting`](super::Options::max_extglob_nesting); npm exhausts its stack
    /// on these instead of erroring cleanly.
    Nesting { pattern: String, limit: usize },
    /// More extglob nodes than [`Options::max_extglob_nodes`](super::Options::max_extglob_nodes)
    /// once each `!()` group has absorbed what follows it; npm's tree doubles per sequential
    /// group without limit.
    Nodes { pattern: String, limit: usize },
    /// Syntax the strict mode refuses where minimatch guesses; the quirk names the behaviour.
    Refused {
        pattern: String,
        quirk: Quirk,
        reason: String,
    },
    /// Longer than [`MAX_PATTERN_LENGTH`](super::MAX_PATTERN_LENGTH).
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
            Error::Steps { pattern, limit } => {
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
            Error::Nesting { pattern, limit } => {
                write!(f, "{pattern:?}: more than {limit} nested groups")
            }
            Error::Nodes { pattern, limit } => {
                write!(f, "{pattern:?}: more than {limit} extglob nodes")
            }
            Error::Refused {
                pattern, reason, ..
            } => write!(f, "{pattern:?}: {reason}"),
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
