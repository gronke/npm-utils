//! The switches and budgets of a pattern.

use super::quirks::Quirk;

/// The longest pattern minimatch accepts (`assertValidPattern`), in characters.
pub const MAX_PATTERN_LENGTH: usize = 64 * 1024;

/// The switches and budgets of a pattern, minimatch's `MinimatchOptions` less the ones out of
/// scope. [`Options::default`] is minimatch's default: case-sensitive, `.`-files hidden from
/// wildcards, braces, extglobs, globstars, negation and comments on. Build one with
/// `..Options::DEFAULT`, so a budget added later keeps the literal compiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Case-insensitive matching (`nocase`).
    pub nocase: bool,
    /// Wildcards match names starting with `.` (`dot`); `.` and `..` stay unmatched by `*` and
    /// `**` either way.
    pub dot: bool,
    /// A pattern without a slash matches the last segment of the path (`matchBase`).
    pub match_base: bool,
    /// `is_match` answers whether the pattern body matched, ignoring a leading `!`
    /// (`flipNegate`); ignore-walk reads the negation itself.
    pub flip_negate: bool,
    /// No brace expansion (`nobrace`).
    pub nobrace: bool,
    /// No extglob groups `@(…)`, `!(…)`, `?(…)`, `*(…)`, `+(…)` (`noext`).
    pub noext: bool,
    /// `**` is an ordinary `*` (`noglobstar`).
    pub noglobstar: bool,
    /// A leading `!` is a literal (`nonegate`).
    pub nonegate: bool,
    /// A leading `#` is a literal (`nocomment`).
    pub nocomment: bool,
    /// The most strings one pattern may expand to.
    pub max_brace_expansions: usize,
    /// The most brace groups one pattern may hold.
    pub max_brace_groups: usize,
    /// The most characters all expansions of one pattern may total; brace-expansion's own cap.
    pub max_brace_length: usize,
    /// The most steps the segment evaluator spends on one segment before it gives up with
    /// [`Error::Steps`](super::Error::Steps); a backstop for the polynomial worst case, where the JavaScript hangs.
    pub max_match_steps: usize,
    /// The most `**` sections one path may cross while it is matched, minimatch's
    /// `maxGlobstarRecursion`; checked at match time, so the error surfaces at the first path
    /// the rule meets.
    pub max_globstar_recursion: usize,
    /// The most extglob groups one pattern may nest. Adoption chains never charge the grammar's
    /// own depth guard, so without a budget a `+(+(…))` pattern recurses the parser past the
    /// caller's stack; npm throws a RangeError a few thousand groups in. Sequential groups do
    /// not nest.
    pub max_extglob_nesting: usize,
    /// The most extglob nodes one pattern may build across its brace expansions: every group,
    /// every alternative and every copy a `!()` group takes of what follows it, each at most one
    /// compiled run. Sequential `!()` groups double the tree per group in minimatch's algorithm,
    /// which npm shares, so a twenty-group line would cost millions of regexes without this.
    pub max_extglob_nodes: usize,
    /// minimatch's quirks kept, the default here, so a pattern means what it means to npm. Off
    /// is the strict mode: ambiguous and unclosed syntax is refused by name, escapes hold
    /// everywhere, the POSIX class translations are corrected. [`Quirk`] lists the ten.
    pub quirks: bool,
}

impl Options {
    /// Minimatch's defaults with the budgets of this port.
    pub const DEFAULT: Options = Options {
        nocase: false,
        dot: false,
        match_base: false,
        flip_negate: false,
        nobrace: false,
        noext: false,
        noglobstar: false,
        nonegate: false,
        nocomment: false,
        max_brace_expansions: 10_000,
        max_brace_groups: 100,
        max_brace_length: 4_000_000,
        max_match_steps: 1_000_000,
        max_globstar_recursion: 200,
        max_extglob_nesting: 128,
        max_extglob_nodes: 10_000,
        quirks: true,
    };

    /// Whether a quirk is in force; every quirk follows [`Options::quirks`].
    pub const fn keeps(&self, _quirk: Quirk) -> bool {
        self.quirks
    }

    /// The options ignore-walk hands minimatch for an ignore-file rule: `matchBase`, `dot`,
    /// `flipNegate` and `nocase`.
    pub const fn ignore_walk() -> Options {
        Options {
            match_base: true,
            dot: true,
            flip_negate: true,
            nocase: true,
            ..Options::DEFAULT
        }
    }
}

impl Default for Options {
    fn default() -> Options {
        Options::DEFAULT
    }
}
