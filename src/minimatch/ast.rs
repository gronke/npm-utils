//! The pattern tree of one path segment: ast.js ported. Extglob groups nest as nodes of an
//! arena; a node records the index it had in its parent when it was created and never updates
//! it, as the JavaScript does, because `isStart` reads that stale value after an adoption
//! (`@(a|@(*|b))` depends on it). The strict mode refuses an unclosed group and keeps an
//! escaped `|` literal ([`Quirk`]).

use super::chars::{is_dots, is_ext_kind, regexp_escape, RE_SPECIALS};
use super::class::parse_class;
use super::quirks::{Quirk, Refusal};
use super::seg::{Anchor, SegError, SegPiece, SegProgram};
use super::{unescape, Error, Options};

const QMARK: &str = "[^/]";
pub(super) const STAR: &str = "[^/]*?";
const STAR_NO_EMPTY: &str = "[^/]+?";
const ROOT: usize = 0;

/// `adoptionMap`: which nested extglob types a type takes the children of.
fn adoption(kind: char) -> &'static [char] {
    match kind {
        '!' => &['@'],
        '?' => &['?', '@'],
        '@' => &['@'],
        '*' => &['*', '+', '?', '@'],
        '+' => &['+', '@'],
        _ => &[],
    }
}

/// `adoptionWithSpaceMap`: adopted with a blank alternative added.
fn adoption_with_space(kind: char) -> &'static [char] {
    match kind {
        '!' => &['?'],
        '@' => &['?'],
        '+' => &['?', '*'],
        _ => &[],
    }
}

/// `adoptionAnyMap`: the union of the two.
fn adoption_any(kind: char) -> &'static [char] {
    match kind {
        '!' => &['?', '@'],
        '?' => &['?', '@'],
        '@' => &['?', '@'],
        '*' => &['*', '+', '?', '@'],
        '+' => &['+', '@', '?', '*'],
        _ => &[],
    }
}

/// `usurpMap`: the type a parent becomes when its only child is a nested extglob.
fn usurp_type(parent: char, child: char) -> Option<char> {
    match (parent, child) {
        ('!', '!') => Some('@'),
        ('?', '*') | ('?', '+') => Some('*'),
        ('@', c) if is_ext_kind(c) => Some(c),
        ('+', '?') | ('+', '*') => Some('*'),
        _ => None,
    }
}

#[derive(Debug, Clone)]
enum Piece {
    Str(String),
    Node(usize),
}

#[derive(Debug)]
struct Node {
    /// The extglob type, `None` for a plain sequence.
    kind: Option<char>,
    parts: Vec<Piece>,
    parent: Option<usize>,
    /// The index in the parent's parts at creation; never updated.
    parent_index: usize,
    /// `undefined` until the source is generated, then known.
    has_magic: Option<bool>,
    /// An extglob with no children, which really means one child of `''`.
    empty_ext: bool,
}

/// What `toRegExpSource` returns: the regex source, the unescaped literal, magic, `u` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Src {
    pub re: String,
    pub body: String,
    pub has_magic: bool,
    pub uflag: bool,
}

/// `toMMPattern`'s result: a literal to compare, or a compiled segment program.
#[derive(Debug)]
pub(super) enum MmPattern {
    Literal(String),
    Program(SegProgram),
}

/// What can fail building a pattern tree: a strict-mode refusal, or a nesting depth past
/// [`Options::max_extglob_nesting`] (a budget in both modes; npm exhausts its stack instead).
#[derive(Debug)]
pub(super) enum AstError {
    Refusal(Refusal),
    Nesting,
    /// The pattern built more nodes than [`Options::max_extglob_nodes`] across its expansions.
    Nodes,
}

/// The extglob nodes one pattern may build across its brace expansions and segments: every
/// group, every alternative and every copy [`Ast::fill_negs`] takes, each at most one compiled
/// run. One budget serves every segment of one `Minimatch::new`, since brace expansion
/// multiplies segments.
#[derive(Debug, Default)]
pub(super) struct NodeBudget {
    left: usize,
}

impl NodeBudget {
    pub(super) fn new(max: usize) -> NodeBudget {
        NodeBudget { left: max }
    }

    fn take(&mut self) -> Result<(), AstError> {
        self.left = self.left.checked_sub(1).ok_or(AstError::Nodes)?;
        Ok(())
    }
}

impl From<Refusal> for AstError {
    fn from(refusal: Refusal) -> AstError {
        AstError::Refusal(refusal)
    }
}

pub(super) struct Ast {
    nodes: Vec<Node>,
    negs: Vec<usize>,
    filled_negs: bool,
    options: Options,
    /// The pattern reconstructed before flattening, for the cased check of the magic test.
    glob: String,
    budget: NodeBudget,
}

impl Ast {
    /// `AST.fromGlob`, then the normalization minimatch runs lazily: adoption and usurpation
    /// (`flatten`) and the negation fill. Every node is built here, charged to `budget`, so a
    /// pattern past [`Options::max_extglob_nodes`] fails here and never reaches emission.
    pub(super) fn from_glob(
        pattern: &str,
        options: Options,
        budget: &mut NodeBudget,
    ) -> Result<Ast, AstError> {
        let mut ast = Ast {
            nodes: Vec::new(),
            negs: Vec::new(),
            filled_negs: false,
            options,
            glob: String::new(),
            budget: std::mem::take(budget),
        };
        let chars: Vec<char> = pattern.chars().collect();
        let built = ast.build(&chars);
        *budget = std::mem::take(&mut ast.budget);
        built?;
        Ok(ast)
    }

    fn build(&mut self, chars: &[char]) -> Result<(), AstError> {
        self.new_node(None, None)?;
        self.parse_ast(chars, ROOT, 0, 0, 0)?;
        self.glob = self.to_string(ROOT);
        self.flatten(ROOT)?;
        self.fill_negs()
    }

    /// A node; every one but a segment's root is charged to the budget.
    fn new_node(&mut self, kind: Option<char>, parent: Option<usize>) -> Result<usize, AstError> {
        if parent.is_some() {
            self.budget.take()?;
        }
        let id = self.nodes.len();
        if kind == Some('!') && !self.filled_negs {
            self.negs.push(id);
        }
        let parent_index = parent.map_or(0, |p| self.nodes[p].parts.len());
        self.nodes.push(Node {
            kind,
            parts: Vec::new(),
            parent,
            parent_index,
            // Extglobs are inherently magical.
            has_magic: kind.map(|_| true),
            empty_ext: false,
        });
        Ok(id)
    }

    fn push_str(&mut self, node: usize, s: String) {
        if !s.is_empty() {
            self.nodes[node].parts.push(Piece::Str(s));
        }
    }

    fn push_node(&mut self, node: usize, child: usize) {
        self.nodes[node].parts.push(Piece::Node(child));
    }

    /// `toString`: the pattern reconstructed.
    pub(super) fn to_string(&self, n: usize) -> String {
        let node = &self.nodes[n];
        let parts: Vec<String> = node
            .parts
            .iter()
            .map(|p| match p {
                Piece::Str(s) => s.clone(),
                Piece::Node(id) => self.to_string(*id),
            })
            .collect();
        match node.kind {
            None => parts.concat(),
            Some(kind) => format!("{kind}({})", parts.join("|")),
        }
    }

    fn is_start(&self, n: usize) -> bool {
        let Some(parent) = self.nodes[n].parent else {
            return true;
        };
        if !self.is_start(parent) {
            return false;
        }
        let parent_index = self.nodes[n].parent_index;
        if parent_index == 0 {
            return true;
        }
        // If everything ahead of this is a negation, then it is still the start.
        (0..parent_index).all(|i| match self.nodes[parent].parts.get(i) {
            Some(Piece::Node(id)) => self.nodes[*id].kind == Some('!'),
            _ => false,
        })
    }

    fn is_end(&self, n: usize) -> bool {
        let Some(parent) = self.nodes[n].parent else {
            return true;
        };
        if self.nodes[parent].kind == Some('!') {
            return true;
        }
        if !self.is_end(parent) {
            return false;
        }
        if self.nodes[n].kind.is_none() {
            // The parent's end-ness was just established; the JavaScript asks again here,
            // doubling the work at every nesting level.
            return true;
        }
        let pl = self.nodes[parent].parts.len();
        pl > 0 && self.nodes[n].parent_index == pl - 1
    }

    fn copy_in(&mut self, dest: usize, part: Piece) -> Result<(), AstError> {
        match part {
            Piece::Str(s) => self.push_str(dest, s),
            Piece::Node(id) => {
                let clone = self.clone_node(id, dest)?;
                self.push_node(dest, clone);
            }
        }
        Ok(())
    }

    fn clone_node(&mut self, src: usize, parent: usize) -> Result<usize, AstError> {
        let clone = self.new_node(self.nodes[src].kind, Some(parent))?;
        let parts = self.nodes[src].parts.clone();
        for p in parts {
            self.copy_in(clone, p)?;
        }
        Ok(clone)
    }

    /// `#fillNegs`: every `!` group gets what follows it (up to the end of each plain ancestor)
    /// appended to each of its alternatives, so `!(a)b` means "not `ab`", last group first.
    /// Each group copies the groups after it, copies included, so the tree doubles per
    /// sequential group; the node budget is what bounds it.
    fn fill_negs(&mut self) -> Result<(), AstError> {
        if self.filled_negs {
            return Ok(());
        }
        self.filled_negs = true;
        while let Some(n) = self.negs.pop() {
            if self.nodes[n].kind != Some('!') {
                continue;
            }
            let mut p = n;
            let mut pp = self.nodes[p].parent;
            while let Some(ppi) = pp {
                let mut i = self.nodes[p].parent_index + 1;
                while self.nodes[ppi].kind.is_none() && i < self.nodes[ppi].parts.len() {
                    let sibling = self.nodes[ppi].parts[i].clone();
                    let alternatives: Vec<usize> = self.nodes[n]
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            Piece::Node(id) => Some(*id),
                            Piece::Str(_) => None,
                        })
                        .collect();
                    for part in alternatives {
                        self.copy_in(part, sibling.clone())?;
                    }
                    i += 1;
                }
                p = ppi;
                pp = self.nodes[p].parent;
            }
        }
        Ok(())
    }

    /// `#parseAST`: the segment from `pos`, into `ast`; returns where it stopped. `nesting`
    /// counts every extglob group on the way down, adoption bypass included: the grammar's own
    /// `MAX_DEPTH` is a semantics port, this counter is the stack budget, and past
    /// [`Options::max_extglob_nesting`] the pattern is an error in both modes.
    fn parse_ast(
        &mut self,
        s: &[char],
        ast: usize,
        pos: usize,
        ext_depth: usize,
        nesting: usize,
    ) -> Result<usize, AstError> {
        const MAX_DEPTH: usize = 2;
        if self.nodes[ast].kind.is_some() && nesting > self.options.max_extglob_nesting {
            return Err(AstError::Nesting);
        }
        let noext = self.options.noext;
        let mut escaping = false;
        let mut in_brace = false;
        let mut brace_start = 0usize;
        let mut brace_neg = false;
        if self.nodes[ast].kind.is_none() {
            // Outside an extglob: accumulate until a start.
            let mut i = pos;
            let mut acc = String::new();
            while i < s.len() {
                let c = s[i];
                i += 1;
                // Escapes still accumulate; escaped starts are ignored.
                if escaping || c == '\\' {
                    escaping = !escaping;
                    acc.push(c);
                    continue;
                }
                if in_brace {
                    if i == brace_start + 1 {
                        if c == '^' || c == '!' {
                            brace_neg = true;
                        }
                    } else if c == ']' && !(i == brace_start + 2 && brace_neg) {
                        in_brace = false;
                    }
                    acc.push(c);
                    continue;
                } else if c == '[' {
                    in_brace = true;
                    brace_start = i;
                    brace_neg = false;
                    acc.push(c);
                    continue;
                }
                let do_recurse =
                    !noext && is_ext_kind(c) && s.get(i) == Some(&'(') && ext_depth <= MAX_DEPTH;
                if do_recurse {
                    self.push_str(ast, std::mem::take(&mut acc));
                    let ext = self.new_node(Some(c), Some(ast))?;
                    i = self.parse_ast(s, ext, i, ext_depth + 1, nesting + 1)?;
                    self.push_node(ast, ext);
                    continue;
                }
                acc.push(c);
            }
            self.push_str(ast, acc);
            return Ok(i);
        }
        // Some kind of extglob; pos is at the `(`. Find the next `|` or `)`.
        let kind = self.nodes[ast].kind;
        let mut i = pos + 1;
        let mut part = self.new_node(None, Some(ast))?;
        let mut parts: Vec<usize> = Vec::new();
        let mut acc = String::new();
        while i < s.len() {
            let c = s[i];
            i += 1;
            if escaping || c == '\\' {
                escaping = !escaping;
                acc.push(c);
                continue;
            }
            if in_brace {
                if i == brace_start + 1 {
                    if c == '^' || c == '!' {
                        brace_neg = true;
                    }
                } else if c == ']' && !(i == brace_start + 2 && brace_neg) {
                    in_brace = false;
                }
                acc.push(c);
                continue;
            } else if c == '[' {
                in_brace = true;
                brace_start = i;
                brace_neg = false;
                acc.push(c);
                continue;
            }
            let can_adopt = kind.is_some_and(|k| adoption_any(k).contains(&c));
            let do_recurse = !noext
                && is_ext_kind(c)
                && s.get(i) == Some(&'(')
                && (ext_depth <= MAX_DEPTH || can_adopt);
            if do_recurse {
                let depth_add = if can_adopt { 0 } else { 1 };
                self.push_str(part, std::mem::take(&mut acc));
                let ext = self.new_node(Some(c), Some(part))?;
                self.push_node(part, ext);
                i = self.parse_ast(s, ext, i, ext_depth + depth_add, nesting + 1)?;
                continue;
            }
            if c == '|' {
                self.push_str(part, std::mem::take(&mut acc));
                parts.push(part);
                part = self.new_node(None, Some(ast))?;
                continue;
            }
            if c == ')' {
                if acc.is_empty() && self.nodes[ast].parts.is_empty() {
                    self.nodes[ast].empty_ext = true;
                }
                self.push_str(part, std::mem::take(&mut acc));
                for p in parts {
                    self.push_node(ast, p);
                }
                self.push_node(ast, part);
                return Ok(i);
            }
            acc.push(c);
        }
        // An unfinished extglob: not an extglob, maybe something else in there.
        let rest: String = s[pos - 1..].iter().collect();
        if !self.options.keeps(Quirk::UnclosedGroupIsLiteral) {
            let shown: String = rest.chars().take(40).collect();
            return Err(Refusal::new(
                Quirk::UnclosedGroupIsLiteral,
                format!("the group `{shown}` never closes"),
            )
            .into());
        }
        self.nodes[ast].kind = None;
        self.nodes[ast].has_magic = None;
        self.nodes[ast].parts = vec![Piece::Str(rest)];
        Ok(i)
    }

    fn can_adopt(&self, n: usize, child: usize, map: fn(char) -> &'static [char]) -> bool {
        let Some(kind) = self.nodes[n].kind else {
            return false;
        };
        let ch = &self.nodes[child];
        if ch.kind.is_some() || ch.parts.len() != 1 {
            return false;
        }
        let Piece::Node(gc) = ch.parts[0] else {
            return false;
        };
        self.nodes[gc]
            .kind
            .is_some_and(|gk| map(kind).contains(&gk))
    }

    /// `#adopt`: the grandchild group's alternatives replace the child in this group.
    fn adopt(&mut self, n: usize, child: usize, index: usize) {
        let Piece::Node(gc) = self.nodes[child].parts[0] else {
            return;
        };
        let gc_parts = self.nodes[gc].parts.clone();
        for p in &gc_parts {
            if let Piece::Node(id) = p {
                self.nodes[*id].parent = Some(n);
            }
        }
        self.nodes[n].parts.splice(index..index + 1, gc_parts);
    }

    fn adopt_with_space(&mut self, n: usize, child: usize, index: usize) -> Result<(), AstError> {
        let Piece::Node(gc) = self.nodes[child].parts[0] else {
            return Ok(());
        };
        let blank = self.new_node(None, Some(gc))?;
        self.nodes[blank].parts.push(Piece::Str(String::new()));
        self.push_node(gc, blank);
        self.adopt(n, child, index);
        Ok(())
    }

    fn can_usurp(&self, n: usize, child: usize) -> bool {
        let Some(kind) = self.nodes[n].kind else {
            return false;
        };
        let ch = &self.nodes[child];
        if ch.kind.is_some() || ch.parts.len() != 1 || self.nodes[n].parts.len() != 1 {
            return false;
        }
        let Piece::Node(gc) = ch.parts[0] else {
            return false;
        };
        self.nodes[gc]
            .kind
            .is_some_and(|gk| usurp_type(kind, gk).is_some())
    }

    /// `#usurp`: the grandchild group takes this group over, with the combined type.
    fn usurp(&mut self, n: usize, child: usize) {
        let Piece::Node(gc) = self.nodes[child].parts[0] else {
            return;
        };
        let (Some(kind), Some(gk)) = (self.nodes[n].kind, self.nodes[gc].kind) else {
            return;
        };
        let Some(new_type) = usurp_type(kind, gk) else {
            return;
        };
        let parts = self.nodes[gc].parts.clone();
        for p in &parts {
            if let Piece::Node(id) = p {
                self.nodes[*id].parent = Some(n);
            }
        }
        self.nodes[n].parts = parts;
        self.nodes[n].kind = Some(new_type);
        self.nodes[n].empty_ext = false;
    }

    /// `#flatten`: up to ten passes of adoption and usurpation over each group.
    fn flatten(&mut self, n: usize) -> Result<(), AstError> {
        if self.nodes[n].kind.is_none() {
            let children: Vec<usize> = self.nodes[n]
                .parts
                .iter()
                .filter_map(|p| match p {
                    Piece::Node(id) => Some(*id),
                    Piece::Str(_) => None,
                })
                .collect();
            for c in children {
                self.flatten(c)?;
            }
            return Ok(());
        }
        let mut iterations = 0;
        loop {
            let mut done = true;
            let mut i = 0;
            while i < self.nodes[n].parts.len() {
                if let Piece::Node(c) = self.nodes[n].parts[i] {
                    self.flatten(c)?;
                    if self.can_adopt(n, c, adoption) {
                        done = false;
                        self.adopt(n, c, i);
                    } else if self.can_adopt(n, c, adoption_with_space) {
                        done = false;
                        self.adopt_with_space(n, c, i)?;
                    } else if self.can_usurp(n, c) {
                        done = false;
                        self.usurp(n, c);
                    }
                }
                i += 1;
            }
            iterations += 1;
            if done || iterations >= 10 {
                break;
            }
        }
        Ok(())
    }

    /// `toMMPattern`: the literal when nothing is magic, else the segment program.
    pub(super) fn into_mm_pattern(self, pattern: &str) -> Result<MmPattern, Error> {
        let (prog, magic, body) = self.into_seg_program(ROOT, None).map_err(|e| match e {
            SegError::Refusal(refusal) => refusal.into_error(pattern),
            SegError::Regex(source) => Error::Regex {
                pattern: pattern.to_string(),
                source,
            },
        })?;
        if !magic {
            return Ok(MmPattern::Literal(body));
        }
        Ok(MmPattern::Program(prog))
    }

    /// The segment as a [`SegProgram`]: negation-free runs stay single linear regexes, `!`
    /// groups become zero-width checks, groups become alternation or fixpoint pieces only
    /// where a run cannot hold them. Returns the program, whether it holds any magic, and the
    /// unescaped literal for the magic-free case.
    pub(super) fn into_seg_program(
        mut self,
        n: usize,
        allow_dot: Option<bool>,
    ) -> Result<(SegProgram, bool, String), SegError> {
        // into_mm_pattern's cased check reads the reconstruction from before the normalization.
        let glob = if n == ROOT {
            self.glob.clone()
        } else {
            self.to_string(n)
        };
        let mut em = Emission::new();
        self.emit_node(n, allow_dot, &mut em)?;
        // into_mm_pattern's any_magic: a nocase pattern with cased letters needs the engine
        // even without magic. The literal path never compiles: an invalid extglob's raw text
        // can be no regex at all. The body is unescape of the accumulated source, as in
        // plain_source.
        let cased = glob
            .chars()
            .any(|c| c.to_uppercase().to_string() != c.to_lowercase().to_string());
        let magic =
            em.magic || self.nodes[n].has_magic.unwrap_or(false) || (self.options.nocase && cased);
        let body = if magic {
            String::new()
        } else {
            unescape(&em.run, true)
        };
        let prog = if magic {
            em.into_program(&self.options, Lift::Segment)?
                .compile(&self.options)?
        } else {
            SegProgram::new(Vec::new())
        };
        Ok((prog, magic, body))
    }

    fn emit_node(
        &mut self,
        n: usize,
        allow_dot: Option<bool>,
        em: &mut Emission,
    ) -> Result<(), SegError> {
        let dot = allow_dot.unwrap_or(self.options.dot);
        debug_assert!(self.filled_negs, "from_glob normalizes the tree");
        match self.nodes[n].kind {
            None => self.emit_plain(n, allow_dot, dot, em),
            Some('!') => self.emit_negation(n, allow_dot, dot, em),
            Some(kind) => self.emit_group(n, kind, allow_dot, dot, em),
        }
    }

    /// plain_source, mirrored into pieces: string parts and negation-free groups accumulate
    /// into the run, the start guards become zero-width pieces where the node starts.
    fn emit_plain(
        &mut self,
        n: usize,
        allow_dot: Option<bool>,
        dot: bool,
        em: &mut Emission,
    ) -> Result<(), SegError> {
        let parts = self.nodes[n].parts.clone();
        let no_empty =
            self.is_start(n) && self.is_end(n) && parts.iter().all(|p| matches!(p, Piece::Str(_)));
        let at = em.pieces.len();
        let run_at = em.run.len();
        for p in &parts {
            match p {
                Piece::Str(s) => {
                    let piece = parse_glob(s, no_empty, &self.options)?;
                    em.magic |= piece.has_magic;
                    em.run.push_str(&piece.re);
                }
                Piece::Node(id) => self.emit_node(*id, allow_dot, em)?,
            }
        }
        if self.is_start(n) {
            if let Some(Piece::Str(first)) = parts.first() {
                let dot_trav_allowed = parts.len() == 1 && is_dots(first);
                if !dot_trav_allowed {
                    // The node's first source characters: run text and, for a structural
                    // opening, the `(?:` the JavaScript text would have (it can never open
                    // with `.` or `[`).
                    let mut head = String::new();
                    for piece in &em.pieces[at..] {
                        match piece {
                            SegPiece::Text { src, .. } => head.push_str(src),
                            _ => head.push_str("(?:"),
                        }
                    }
                    head.push_str(&em.run[run_at..]);
                    let head: String = head.chars().take(5).collect();
                    let chars: Vec<char> = head.chars().collect();
                    let aps = |c: Option<&char>| matches!(c, Some('[') | Some('.'));
                    let need_no_trav = (dot && aps(chars.first()))
                        || (head.starts_with("\\.") && aps(chars.get(2)))
                        || (head.starts_with("\\.\\.") && aps(chars.get(4)));
                    let need_no_dot = !dot && allow_dot != Some(true) && aps(chars.first());
                    if need_no_trav {
                        em.pieces.insert(at, SegPiece::NoTraversal);
                    } else if need_no_dot {
                        em.pieces.insert(at, SegPiece::NoDotStart);
                    }
                }
            }
        }
        // plain_source's end anchor: a negation alternative must reach the end of the segment.
        // The anchor also keeps an otherwise empty alternative from being dropped, as the
        // `(?:$|\/)` text does in the JavaScript.
        let in_negation = self.nodes[n]
            .parent
            .is_some_and(|p| self.nodes[p].kind == Some('!'));
        if self.filled_negs && in_negation && self.is_end(n) {
            em.flush(&self.options)?;
            em.pieces.push(SegPiece::EndOfSegment);
        }
        Ok(())
    }

    /// A `!(…)` node: the run before it flushes, the alternatives become programs of a zero-
    /// width check, and the group's own star opens the run that follows (with the dot guard
    /// minimatch gives it at a segment start). An empty `!()` is the star alone.
    fn emit_negation(
        &mut self,
        n: usize,
        allow_dot: Option<bool>,
        dot: bool,
        em: &mut Emission,
    ) -> Result<(), SegError> {
        em.flush(&self.options)?;
        let no_dot = self.is_start(n) && !dot;
        em.magic = true;
        if self.nodes[n].empty_ext {
            if no_dot {
                em.pieces.push(SegPiece::NoDotStart);
            }
            em.run.push_str(STAR_NO_EMPTY);
            return Ok(());
        }
        let alternatives = self.child_programs(n, Some(dot), Lift::Negation)?;
        em.pieces.push(SegPiece::Neg(alternatives));
        if no_dot && allow_dot != Some(true) {
            em.pieces.push(SegPiece::NoDotStart);
        }
        em.run.push_str(STAR);
        Ok(())
    }

    /// A `@`/`?`/`*`/`+` group: alternatives as programs; folded back into run text when every
    /// alternative is a single bare run (regex alternation carries it), structural pieces only
    /// where a guard or a negation makes the text form impossible. The start-repeat split of
    /// the JavaScript is kept, as text when folded, as a constrained first iteration else.
    fn emit_group(
        &mut self,
        n: usize,
        kind: char,
        allow_dot: Option<bool>,
        dot: bool,
        em: &mut Emission,
    ) -> Result<(), SegError> {
        let alternatives = self.child_programs(n, Some(dot), Lift::Group)?;
        if self.is_start(n) && self.is_end(n) && alternatives.iter().all(SegProgram::is_empty) {
            // An invalid extglob, literal text as in regexp_source.
            let s = self.to_string(n);
            self.nodes[n].parts = vec![Piece::Str(s.clone())];
            self.nodes[n].kind = None;
            self.nodes[n].has_magic = None;
            em.run.push_str(&s);
            return Ok(());
        }
        em.magic = true;
        let repeated = kind == '*' || kind == '+';
        let dotted = if repeated && !dot && allow_dot != Some(true) {
            self.child_programs(n, Some(true), Lift::Group)?
        } else {
            Vec::new()
        };
        if let Some(folded) = fold_group(kind, &alternatives, &dotted) {
            em.run.push_str(&folded);
            return Ok(());
        }
        em.flush(&self.options)?;
        match kind {
            '@' => em.pieces.push(SegPiece::Any(alternatives)),
            '?' => em.pieces.push(SegPiece::Repeat {
                prog: Box::new(SegProgram::new(vec![SegPiece::Any(alternatives)])),
                first: None,
                min: 0,
                max: Some(1),
            }),
            repeated => {
                let min = usize::from(repeated == '+');
                let any = |alts: Vec<SegProgram>| SegProgram::new(vec![SegPiece::Any(alts)]);
                let (first, prog) = if !dotted.is_empty() {
                    let dotted = any(dotted);
                    let plain = any(alternatives);
                    if plain.render() == dotted.render() {
                        (None, dotted)
                    } else {
                        (Some(Box::new(plain)), dotted)
                    }
                } else {
                    (None, any(alternatives))
                };
                em.pieces.push(SegPiece::Repeat {
                    prog: Box::new(prog),
                    first,
                    min,
                    max: None,
                });
            }
        }
        Ok(())
    }

    /// Each child part-node of `n` as its own program, empties dropped when the group is the
    /// whole segment (parts_to_regexp's rule).
    fn child_programs(
        &mut self,
        n: usize,
        allow_dot: Option<bool>,
        context: Lift,
    ) -> Result<Vec<SegProgram>, SegError> {
        let keep_empties = !(self.is_start(n) && self.is_end(n));
        let ids: Vec<usize> = self.nodes[n]
            .parts
            .iter()
            .filter_map(|p| match p {
                Piece::Node(id) => Some(*id),
                Piece::Str(_) => None,
            })
            .collect();
        let mut out = Vec::new();
        for id in ids {
            let mut local = Emission::new();
            self.emit_node(id, allow_dot, &mut local)?;
            let prog = local.into_program(&self.options, context)?;
            if keep_empties || !prog.is_empty() {
                out.push(prog);
            }
        }
        Ok(out)
    }
}

/// Fold a group back into run text when every alternative is a single bare run: regex
/// alternation and quantifiers carry it, wrapped exactly as `toRegExpSource` wraps it.
fn fold_group(kind: char, alternatives: &[SegProgram], dotted: &[SegProgram]) -> Option<String> {
    let srcs: Option<Vec<&str>> = alternatives
        .iter()
        .map(SegProgram::single_run_src)
        .collect();
    let mut body = srcs?.join("|");
    let repeated = kind == '*' || kind == '+';
    if !repeated {
        let close = if kind == '@' { ")" } else { ")?" };
        return Some(format!("(?:{body}{close}"));
    }
    let mut body_dot_allowed = String::new();
    if !dotted.is_empty() {
        let dsrcs: Option<Vec<&str>> = dotted.iter().map(SegProgram::single_run_src).collect();
        body_dot_allowed = dsrcs?.join("|");
    }
    if body_dot_allowed == body {
        body_dot_allowed.clear();
    }
    if !body_dot_allowed.is_empty() {
        body = format!("(?:{body})(?:{body_dot_allowed})*?");
    }
    let close = match kind {
        '+' if !body_dot_allowed.is_empty() => ")",
        '*' if !body_dot_allowed.is_empty() => ")?",
        kind => return Some(format!("(?:{body}){kind}")),
    };
    Some(format!("(?:{body}{close}"))
}

/// The pieces of one segment in flight: regex source accumulates into `run` and flushes into a
/// `Run` piece when a negation-bearing construct needs the sequence to break.
struct Emission {
    pieces: Vec<SegPiece>,
    run: String,
    magic: bool,
}

impl Emission {
    fn new() -> Emission {
        Emission {
            pieces: Vec::new(),
            run: String::new(),
            magic: false,
        }
    }

    fn flush(&mut self, options: &Options) -> Result<(), SegError> {
        if self.run.is_empty() {
            return Ok(());
        }
        let src = std::mem::take(&mut self.run);
        if !options.keeps(Quirk::EscapedPipeAlternates) {
            self.push_run(src);
            return Ok(());
        }
        let pipes = top_level_pipes(&src);
        if pipes.is_empty() {
            self.push_run(src);
            return Ok(());
        }
        // The quirk's bare alternation: it spans the whole segment, not just this run, so the
        // branches wait as a marker until the assembly sees every piece of the segment.
        let mut branches = Vec::with_capacity(pipes.len() + 1);
        let mut at = 0;
        for &pipe in &pipes {
            branches.push(src[at..pipe].to_string());
            at = pipe + 1;
        }
        branches.push(src[at..].to_string());
        self.pieces.push(SegPiece::Pipe(branches));
        Ok(())
    }

    fn push_run(&mut self, src: String) {
        self.pieces.push(SegPiece::Text {
            src,
            anchor: Anchor::Exact,
        });
    }

    /// Flush and assemble: the runs stay as they are unless the quirk's bare alternation is
    /// present. Then the `Pipe` markers split the segment the way the JavaScript's top-level
    /// `|` does: everything before the first marker opens the first branch (its last run may
    /// end anywhere, as `^` binds the branch alone), every text after a pipe starts the next
    /// branch (floating, so it may match anywhere), and the last branch reaches the end (its
    /// first run starts anywhere, as `$` binds it alone).
    fn into_program(mut self, options: &Options, context: Lift) -> Result<SegProgram, SegError> {
        self.flush(options)?;
        if !self.pieces.iter().any(|p| matches!(p, SegPiece::Pipe(_))) {
            return Ok(SegProgram::new(self.pieces));
        }
        let mut branches: Vec<Vec<SegPiece>> = Vec::new();
        let mut current: Vec<SegPiece> = Vec::new();
        for piece in std::mem::take(&mut self.pieces) {
            match piece {
                SegPiece::Pipe(texts) => {
                    let mut texts = texts.into_iter();
                    if let Some(first) = texts.next() {
                        current.push(text_piece(first));
                    }
                    for text in texts {
                        branches.push(std::mem::take(&mut current));
                        current.push(text_piece(text));
                    }
                }
                other => current.push(other),
            }
        }
        branches.push(current);
        let total = branches.len();
        let mut programs = Vec::with_capacity(total);
        for (i, pieces) in branches.into_iter().enumerate() {
            programs.push(SegProgram::new(reflavor(pieces, i, total, context)));
        }
        Ok(SegProgram::new(vec![SegPiece::Alternation(programs)]))
    }
}

/// The offsets of the quirk's top-level `|`: unescaped, outside a class, outside a group.
fn top_level_pipes(src: &str) -> Vec<usize> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let mut out = Vec::new();
    let mut in_class = false;
    let mut depth = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        i += 1;
        match c {
            // The escaped character carries no meaning.
            '\\' => i += 1,
            '[' => in_class = true,
            ']' => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => depth = depth.saturating_sub(1),
            '|' if !in_class && depth == 0 => out.push(at),
            _ => {}
        }
    }
    out
}

/// Where a bare alternation sits; how much of it floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lift {
    /// The whole segment, `^A|B|C$`: the first branch's end floats, the last branch's start
    /// floats, a middle branch floats at both edges and nowhere else.
    Segment,
    /// Inside a group alternative: every branch binds to the group's span.
    Group,
    /// Inside a negation alternative, `(?!(?:A|B|C(?:$|/)))`: the lookahead fixes where every
    /// branch starts, the `(?:$|/)` binds the last branch's end, so the first and the middle
    /// branches float their end only.
    Negation,
}

/// A run's source as an exactly anchored piece, compiled with the rest of the program.
fn text_piece(src: String) -> SegPiece {
    SegPiece::Text {
        src,
        anchor: Anchor::Exact,
    }
}

/// Set a branch's boundary anchors for the alternation's scope. A floating edge belongs to
/// one run only, the one at that edge: at the segment level the first branch's end floats
/// (`^` binds it alone), so its last run is a prefix match; the last branch's start floats
/// (`$` binds it alone), so its first run is a suffix match; a middle branch floats at both
/// edges, so its first run is a suffix match, its last run a prefix match, and a lone run
/// matches anywhere. Every other run matches its slice exactly, so a zero-width check after
/// it evaluates at the real match end and not at every end that merely contains a match. In a
/// group alternative every branch binds to the group's span. In a negation alternative the
/// lookahead fixes every branch's start, the first and the middle branches float their end,
/// and the last branch reaches the `(?:$|/)` that follows the group.
fn reflavor(pieces: Vec<SegPiece>, index: usize, total: usize, context: Lift) -> Vec<SegPiece> {
    let first = index == 0;
    let last = index + 1 == total;
    let runs: Vec<usize> = pieces
        .iter()
        .enumerate()
        .filter_map(|(i, p)| matches!(p, SegPiece::Text { .. }).then_some(i))
        .collect();
    let mut out = pieces;
    for (at, &i) in runs.iter().enumerate() {
        let first_run = at == 0;
        let last_run = at + 1 == runs.len();
        let anchor = match (context, first, last) {
            (Lift::Segment, true, true) => Anchor::Exact,
            (Lift::Segment, true, false) if last_run => Anchor::Prefix,
            (Lift::Segment, false, true) if first_run => Anchor::Suffix,
            (Lift::Segment, false, false) => match (first_run, last_run) {
                (true, true) => Anchor::Free,
                (true, false) => Anchor::Suffix,
                (false, true) => Anchor::Prefix,
                (false, false) => Anchor::Exact,
            },
            (Lift::Negation, _, false) if last_run => Anchor::Prefix,
            _ => Anchor::Exact,
        };
        if let SegPiece::Text { anchor: a, .. } = &mut out[i] {
            *a = anchor;
        }
    }
    out
}

/// `#parseGlob`: one plain string of a segment as a regex source.
fn parse_glob(glob: &str, no_empty: bool, options: &Options) -> Result<Src, Refusal> {
    let chars: Vec<char> = glob.chars().collect();
    let all_stars = !chars.is_empty() && chars.iter().all(|&c| c == '*');
    let mut escaping = false;
    let mut re = String::new();
    let mut uflag = false;
    let mut has_magic = false;
    // Several stars that are not a globstar coalesce into one.
    let mut in_star = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if escaping {
            escaping = false;
            let pipe = c == '|' && !options.keeps(Quirk::EscapedPipeAlternates);
            if RE_SPECIALS.contains(c) || pipe {
                re.push('\\');
            }
            re.push(c);
            i += 1;
            continue;
        }
        if c == '*' {
            if !in_star {
                in_star = true;
                re.push_str(if no_empty && all_stars {
                    STAR_NO_EMPTY
                } else {
                    STAR
                });
                has_magic = true;
            }
            i += 1;
            continue;
        }
        in_star = false;
        if c == '\\' {
            if i == chars.len() - 1 {
                re.push_str("\\\\");
            } else {
                escaping = true;
            }
            i += 1;
            continue;
        }
        if c == '[' {
            let class = parse_class(&chars, i, options)?;
            if class.consumed > 0 {
                re.push_str(&class.src);
                uflag |= class.uflag;
                has_magic |= class.magic;
                i += class.consumed;
                continue;
            }
        }
        if c == '?' {
            re.push_str(QMARK);
            has_magic = true;
            i += 1;
            continue;
        }
        regexp_escape(c, &mut re);
        i += 1;
    }
    Ok(Src {
        re,
        body: unescape(glob, true),
        has_magic,
        uflag,
    })
}

#[cfg(test)]
mod tests {
    use super::super::Options;
    use super::{Ast, AstError, MmPattern, NodeBudget};

    enum Expect {
        Literal(&'static str),
        Src(&'static str),
    }

    fn options(preset: &str) -> Options {
        match preset {
            "default" => Options::DEFAULT,
            "dot" => Options {
                dot: true,
                ..Options::DEFAULT
            },
            "nocase" => Options {
                nocase: true,
                ..Options::DEFAULT
            },
            "noext" => Options {
                noext: true,
                ..Options::DEFAULT
            },
            "nocasedot" => Options {
                nocase: true,
                dot: true,
                ..Options::DEFAULT
            },
            other => panic!("unknown preset {other}"),
        }
    }

    /// What minimatch 10.2.5 makes of each pattern under each preset, recorded with node: the
    /// literal it compares when nothing is magic, else the source of the regex it builds. The
    /// port has to produce the same text.
    const TABLE: &[(&str, &str, Expect)] = &[
        ("a", "default", Expect::Literal("a")),
        ("a*", "default", Expect::Src("a[^/]*?")),
        ("*", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("**", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("*.js", "default", Expect::Src("(?!\\.)[^/]*?\\.js")),
        (".*", "default", Expect::Src("(?!(?:^|/)\\.\\.?(?:$|/))\\.[^/]*?")),
        ("..*", "default", Expect::Src("(?!(?:^|/)\\.\\.?(?:$|/))\\.\\.[^/]*?")),
        (".", "default", Expect::Literal(".")),
        ("..", "default", Expect::Literal("..")),
        (".a", "default", Expect::Literal(".a")),
        ("a.b", "default", Expect::Literal("a.b")),
        ("?", "default", Expect::Src("(?!\\.)[^/]")),
        ("??", "default", Expect::Src("(?!\\.)[^/][^/]")),
        ("a?b", "default", Expect::Src("a[^/]b")),
        ("[abc]", "default", Expect::Src("(?!\\.)[abc]")),
        ("[a-z]*", "default", Expect::Src("(?!\\.)[a-z][^/]*?")),
        ("[!a]", "default", Expect::Src("(?!\\.)[^a]")),
        ("[.]a", "default", Expect::Literal(".a")),
        ("\\*", "default", Expect::Literal("*")),
        ("\\?", "default", Expect::Literal("?")),
        ("a\\*b", "default", Expect::Literal("a*b")),
        ("\\[a\\]", "default", Expect::Literal("[a]")),
        ("a\\", "default", Expect::Literal("a\\")),
        ("\\\\", "default", Expect::Literal("\\")),
        ("\\|", "default", Expect::Literal("|")),
        ("\\-", "default", Expect::Literal("-")),
        ("\\!x", "default", Expect::Literal("!x")),
        ("a|b", "default", Expect::Literal("a|b")),
        ("(a)", "default", Expect::Literal("(a)")),
        ("a(b)", "default", Expect::Literal("a(b)")),
        ("{a,b}", "default", Expect::Literal("{a,b}")),
        ("#a", "default", Expect::Literal("#a")),
        ("!a", "default", Expect::Literal("!a")),
        ("@(a|b)", "default", Expect::Src("(?:a|b)")),
        ("!(a)", "default", Expect::Src("(?:(?!(?:a(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(a|b)", "default", Expect::Src("(?:(?!(?:a(?:$|\\/)|b(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(a)x", "default", Expect::Src("(?:(?!(?:ax(?:$|\\/)))(?!\\.)[^/]*?)x")),
        ("x!(a)", "default", Expect::Src("x(?:(?!(?:a(?:$|\\/)))[^/]*?)")),
        ("!(a)*", "default", Expect::Src("(?:(?!(?:a[^/]+?(?:$|\\/)))(?!\\.)[^/]*?)[^/]*?")),
        ("*(a|b)", "default", Expect::Src("(?:a|b)*")),
        ("+(a|b)", "default", Expect::Src("(?:a|b)+")),
        ("?(a|b)", "default", Expect::Src("(?:a|b)?")),
        ("@()", "default", Expect::Literal("@()")),
        ("!()", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("*()", "default", Expect::Literal("*()")),
        ("+()", "default", Expect::Literal("+()")),
        ("?()", "default", Expect::Literal("?()")),
        ("!(a|b)c", "default", Expect::Src("(?:(?!(?:ac(?:$|\\/)|bc(?:$|\\/)))(?!\\.)[^/]*?)c")),
        ("a!(b)c", "default", Expect::Src("a(?:(?!(?:bc(?:$|\\/)))[^/]*?)c")),
        ("!(!(a))", "default", Expect::Src("(?:a)")),
        ("@(!(a))", "default", Expect::Src("(?:(?!(?:a(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("?(+(a))", "default", Expect::Src("(?:a)*")),
        ("+(?(a))", "default", Expect::Src("(?:a)+")),
        ("*(+(a)|b)", "default", Expect::Src("(?:a|b)*")),
        ("+(@(a)|b)", "default", Expect::Src("(?:a|b)+")),
        ("+(*(a)|b)", "default", Expect::Src("(?:a|b)+")),
        ("!(@(a)|b)", "default", Expect::Src("(?:(?!(?:a(?:$|\\/)|b(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(?(a)|b)", "default", Expect::Src("(?:(?!(?:a(?:$|\\/)|(?:$|\\/)|b(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("@(?(a)|b)", "default", Expect::Src("(?:a|b)")),
        ("+(?(a)|b)", "default", Expect::Src("(?:a|b)+")),
        ("!(*(a)|b)", "default", Expect::Src("(?:(?!(?:(?:a)*(?:$|\\/)|b(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("@(a|@(*|b))", "default", Expect::Src("(?:a|(?!\\.)[^/]+?|b)")),
        ("*(?)", "default", Expect::Src("(?:(?:(?!\\.)[^/])(?:(?!(?:^|/)\\.\\.?(?:$|/))[^/])*?)?")),
        ("+(*|.x*)", "default", Expect::Src("(?:(?:(?!\\.)[^/]+?|\\.x[^/]*?)(?:(?!(?:^|/)\\.\\.?(?:$|/))[^/]+?|\\.x[^/]*?)*?)")),
        (".@(a|b)", "default", Expect::Src("\\.(?:a|b)")),
        ("@(.a|b)", "default", Expect::Src("(?:\\.a|b)")),
        ("!(a).b", "default", Expect::Src("(?:(?!(?:a\\.b(?:$|\\/)))(?!\\.)[^/]*?)\\.b")),
        ("@(a", "default", Expect::Literal("@(a")),
        ("x*(", "default", Expect::Src("x[^/]*?\\(")),
        ("*(a|b", "default", Expect::Src("(?!\\.)[^/]*?\\(a\\|b")),
        ("@(a|[)]|b)", "default", Expect::Src("(?:a|\\)|b)")),
        ("@([!)]a)", "default", Expect::Src("(?:(?!\\.)[^)]a)")),
        ("!(a)!(b)", "default", Expect::Src("(?:(?!(?:a(?:(?!(?:b(?:$|\\/)))[^/]*?)(?:$|\\/)))(?!\\.)[^/]*?)(?:(?!(?:b(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(a)@(b)", "default", Expect::Src("(?:(?!(?:a(?:b)(?:$|\\/)))(?!\\.)[^/]*?)(?:b)")),
        ("@(a)!(b)", "default", Expect::Src("(?:a)(?:(?!(?:b(?:$|\\/)))[^/]*?)")),
        ("!(a|b)!(c)", "default", Expect::Src("(?:(?!(?:a(?:(?!(?:c(?:$|\\/)))[^/]*?)(?:$|\\/)|b(?:(?!(?:c(?:$|\\/)))[^/]*?)(?:$|\\/)))(?!\\.)[^/]*?)(?:(?!(?:c(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("a@(b|c)d", "default", Expect::Src("a(?:b|c)d")),
        ("@(a|b)@(c|d)", "default", Expect::Src("(?:a|b)(?:c|d)")),
        ("+(a)+(b)", "default", Expect::Src("(?:a)+(?:b)+")),
        ("!(a)?(b)", "default", Expect::Src("(?:(?!(?:a(?:b)?(?:$|\\/)))(?!\\.)[^/]*?)(?:b)?")),
        ("?(a)!(b)c", "default", Expect::Src("(?:a)?(?:(?!(?:bc(?:$|\\/)))[^/]*?)c")),
        ("!([a-z])", "default", Expect::Src("(?:(?!(?:(?!\\.)[a-z](?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(*.js)", "default", Expect::Src("(?:(?!(?:(?!\\.)[^/]*?\\.js(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("*.!(js)", "default", Expect::Src("(?!\\.)[^/]*?\\.(?:(?!(?:js(?:$|\\/)))[^/]*?)")),
        ("!(*.js|*.ts)", "default", Expect::Src("(?:(?!(?:(?!\\.)[^/]*?\\.js(?:$|\\/)|(?!\\.)[^/]*?\\.ts(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("**(a)", "default", Expect::Src("(?!\\.)[^/]*?(?:a)*")),
        ("a**", "default", Expect::Src("a[^/]*?")),
        ("***", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("*a*", "default", Expect::Src("(?!\\.)[^/]*?a[^/]*?")),
        ("a*b*c", "default", Expect::Src("a[^/]*?b[^/]*?c")),
        ("@(a|@(b|@(c|@(d))))", "default", Expect::Src("(?:a|b|c|d)")),
        ("!(a|!(b))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("[[:alpha:]]x", "default", Expect::Src("(?!\\.)[\\p{L}\\p{Nl}]x")),
        // JavaScript: Invalid regular expression (the u flag rejects the escaped `-`); the port compiles it.
        ("[[:alpha:]]-", "default", Expect::Src("(?!\\.)[\\p{L}\\p{Nl}]\\-")),
        ("!(a)", "dot", Expect::Src("(?:(?!(?:a(?:$|\\/)))[^/]*?)")),
        ("*(a)", "dot", Expect::Src("(?:a)*")),
        ("+(a)", "dot", Expect::Src("(?:a)+")),
        ("@(a|b)", "dot", Expect::Src("(?:a|b)")),
        ("*", "dot", Expect::Src("(?!(?:^|/)\\.\\.?(?:$|/))[^/]+?")),
        (".*", "dot", Expect::Src("(?!(?:^|/)\\.\\.?(?:$|/))\\.[^/]*?")),
        ("?", "dot", Expect::Src("(?!(?:^|/)\\.\\.?(?:$|/))[^/]")),
        ("[.]a", "dot", Expect::Literal(".a")),
        ("*(?)", "dot", Expect::Src("(?:(?!(?:^|/)\\.\\.?(?:$|/))[^/])*")),
        ("+(*|.x*)", "dot", Expect::Src("(?:(?!(?:^|/)\\.\\.?(?:$|/))[^/]+?|\\.x[^/]*?)+")),
        ("!()", "dot", Expect::Src("[^/]+?")),
        (".", "dot", Expect::Literal(".")),
        ("..", "dot", Expect::Literal("..")),
        ("a", "nocase", Expect::Src("a")),
        ("A", "nocase", Expect::Src("A")),
        ("1", "nocase", Expect::Literal("1")),
        ("[a]", "nocase", Expect::Src("a")),
        ("*", "nocase", Expect::Src("(?!\\.)[^/]+?")),
        ("é", "nocase", Expect::Src("é")),
        ("ß", "nocase", Expect::Src("ß")),
        ("@(a|b)", "noext", Expect::Literal("@(a|b)")),
        ("!(a)", "noext", Expect::Literal("!(a)")),
        ("*(a)", "noext", Expect::Src("(?!\\.)[^/]*?\\(a\\)")),
        ("*.JS", "nocasedot", Expect::Src("(?!(?:^|/)\\.\\.?(?:$|/))[^/]*?\\.JS")),
        ("a\\ b", "default", Expect::Literal("a b")),
        ("a,b", "default", Expect::Literal("a,b")),
        ("a#b", "default", Expect::Literal("a#b")),
        ("a-b", "default", Expect::Literal("a-b")),
        ("a$b", "default", Expect::Literal("a$b")),
        ("a^b", "default", Expect::Literal("a^b")),
        ("a{b", "default", Expect::Literal("a{b")),
        ("a}b", "default", Expect::Literal("a}b")),
        ("a b", "default", Expect::Literal("a b")),
        ("a\tb", "default", Expect::Literal("a\tb")),
        ("[]", "default", Expect::Literal("[]")),
        ("[a", "default", Expect::Literal("[a")),
        ("a[b", "default", Expect::Literal("a[b")),
        ("a]b", "default", Expect::Literal("a]b")),
        ("[a]b", "default", Expect::Literal("ab")),
        ("[a][b]", "default", Expect::Literal("ab")),
        ("@(a|)", "default", Expect::Src("(?:a)")),
        ("@(|a)", "default", Expect::Src("(?:a)")),
        ("!(|a)", "default", Expect::Src("(?:(?!(?:(?:$|\\/)|a(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("+(a|)", "default", Expect::Src("(?:a)+")),
        ("a@()b", "default", Expect::Src("a(?:)b")),
        ("a!()b", "default", Expect::Src("a[^/]+?b")),
        ("!()a", "default", Expect::Src("(?!\\.)[^/]+?a")),
        ("@()a", "default", Expect::Src("(?:)a")),
        ("!(a|b|c)", "default", Expect::Src("(?:(?!(?:a(?:$|\\/)|b(?:$|\\/)|c(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(a)b", "default", Expect::Src("(?:(?!(?:ab(?:$|\\/)))(?!\\.)[^/]*?)b")),
        ("x!(a)b", "default", Expect::Src("x(?:(?!(?:ab(?:$|\\/)))[^/]*?)b")),
        ("!(a)!(b)c", "default", Expect::Src("(?:(?!(?:a(?:(?!(?:bc(?:$|\\/)))[^/]*?)c(?:$|\\/)))(?!\\.)[^/]*?)(?:(?!(?:bc(?:$|\\/)))(?!\\.)[^/]*?)c")),
        ("!(.)", "default", Expect::Src("(?:(?!(?:\\.(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(..)", "default", Expect::Src("(?:(?!(?:\\.\\.(?:$|\\/)))(?!\\.)[^/]*?)")),
        (".!(a)", "default", Expect::Src("\\.(?:(?!(?:a(?:$|\\/)))[^/]*?)")),
        ("..!(a)", "default", Expect::Src("\\.\\.(?:(?!(?:a(?:$|\\/)))[^/]*?)")),
        ("@(.)", "default", Expect::Src("(?:\\.)")),
        ("@(..)", "default", Expect::Src("(?:\\.\\.)")),
        ("@(.|..)", "default", Expect::Src("(?:\\.|\\.\\.)")),
        ("*(.)", "default", Expect::Src("(?:\\.)*")),
        ("+(.)", "default", Expect::Src("(?:\\.)+")),
        ("?(.)a", "default", Expect::Src("(?:\\.)?a")),
        ("!(*)", "default", Expect::Src("(?:(?!(?:(?!\\.)[^/]+?(?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(?)", "default", Expect::Src("(?:(?!(?:(?!\\.)[^/](?:$|\\/)))(?!\\.)[^/]*?)")),
        ("!(*|?)", "default", Expect::Src("(?:(?!(?:(?!\\.)[^/]+?(?:$|\\/)|(?!\\.)[^/](?:$|\\/)))(?!\\.)[^/]*?)")),
        ("?(*)", "default", Expect::Src("(?:(?!\\.)[^/]+?)?")),
        ("*(*)", "default", Expect::Src("(?:(?:(?!\\.)[^/]+?)(?:(?!(?:^|/)\\.\\.?(?:$|/))[^/]+?)*?)?")),
        ("+(*)", "default", Expect::Src("(?:(?:(?!\\.)[^/]+?)(?:(?!(?:^|/)\\.\\.?(?:$|/))[^/]+?)*?)")),
        ("@(*)", "default", Expect::Src("(?:(?!\\.)[^/]+?)")),
        ("*(a|*(b|c))", "default", Expect::Src("(?:a|b|c)*")),
        ("+(a|+(b|c))", "default", Expect::Src("(?:a|b|c)+")),
        ("@(a|@(b|c))", "default", Expect::Src("(?:a|b|c)")),
        ("?(a|?(b|c))", "default", Expect::Src("(?:a|b|c)?")),
        ("!(a|@(b|c))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("@(a|?(b))", "default", Expect::Src("(?:a|b)")),
        ("!(a|?(b))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("+(a|?(b))", "default", Expect::Src("(?:a|b)+")),
        ("+(a|*(b))", "default", Expect::Src("(?:a|b)+")),
        ("?(a|*(b))", "default", Expect::Src("(?:a|(?:b)*)?")),
        ("?(a|+(b))", "default", Expect::Src("(?:a|(?:b)+)?")),
        ("@(a|*(b))", "default", Expect::Src("(?:a|(?:b)*)")),
        ("@(a|+(b))", "default", Expect::Src("(?:a|(?:b)+)")),
        ("!(a|*(b))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("!(a|+(b))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("!(a|!(b))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("?(a|!(b))", "default", Expect::Src("(?:a|(?:(?!(?:b(?:$|\\/)))(?!\\.)[^/]*?))?")),
        ("@(@(a))", "default", Expect::Src("(?:a)")),
        ("@(*(a))", "default", Expect::Src("(?:a)*")),
        ("@(+(a))", "default", Expect::Src("(?:a)+")),
        ("@(?(a))", "default", Expect::Src("(?:a)")),
        ("!(@(a))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("*(*(a))", "default", Expect::Src("(?:a)*")),
        ("*(?(a))", "default", Expect::Src("(?:a)*")),
        ("*(@(a))", "default", Expect::Src("(?:a)*")),
        ("+(+(a))", "default", Expect::Src("(?:a)+")),
        ("+(@(a))", "default", Expect::Src("(?:a)+")),
        ("?(?(a))", "default", Expect::Src("(?:a)?")),
        ("?(@(a))", "default", Expect::Src("(?:a)?")),
        ("?(*(a))", "default", Expect::Src("(?:a)*")),
        ("?(!(a))", "default", Expect::Src("(?:(?:(?!(?:a(?:$|\\/)))(?!\\.)[^/]*?))?")),
        ("*(!(a))", "default", Expect::Src("(?:(?:(?:(?!(?:a(?:$|\\/)))(?!\\.)[^/]*?))(?:(?:(?!(?:a(?:$|\\/)))[^/]*?))*?)?")),
        ("+(!(a))", "default", Expect::Src("(?:(?:(?:(?!(?:a(?:$|\\/)))(?!\\.)[^/]*?))(?:(?:(?!(?:a(?:$|\\/)))[^/]*?))*?)")),
        ("!(?(a))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("!(*(a))", "default", Expect::Src("(?!\\.)[^/]+?")),
        ("!(+(a))", "default", Expect::Src("(?!\\.)[^/]+?")),
    ];

    #[test]
    fn sources_are_minimatchs() {
        for (pattern, preset, expect) in TABLE {
            let mm = Ast::from_glob(pattern, options(preset), &mut NodeBudget::new(10_000))
                .unwrap_or_else(|e| match e {
                    AstError::Refusal(r) => panic!("{pattern:?} ({preset}): {}", r.reason),
                    AstError::Nesting => panic!("{pattern:?} ({preset}): nesting budget"),
                    AstError::Nodes => panic!("{pattern:?} ({preset}): node budget"),
                })
                .into_mm_pattern(pattern)
                .unwrap_or_else(|e| panic!("{pattern:?} ({preset}): {e}"));
            match (expect, &mm) {
                (Expect::Literal(want), MmPattern::Literal(got)) => {
                    assert_eq!(got, want, "{pattern:?} ({preset})");
                }
                (Expect::Src(want), MmPattern::Program(prog)) => {
                    assert_eq!(&prog.render(), want, "{pattern:?} ({preset})");
                }
                (Expect::Literal(want), MmPattern::Program(prog)) => {
                    panic!(
                        "{pattern:?} ({preset}): expected the literal {want:?}, got {:?}",
                        prog.render()
                    );
                }
                (Expect::Src(want), MmPattern::Literal(got)) => {
                    panic!("{pattern:?} ({preset}): expected the program {want:?}, got the literal {got:?}");
                }
            }
        }
    }

    #[test]
    fn the_glob_is_reconstructed_before_flattening() {
        let ast = Ast::from_glob(
            "+(a|+(b|c)|d)",
            Options::DEFAULT,
            &mut NodeBudget::new(10_000),
        )
        .unwrap();
        // The stored reconstruction predates the normalization; the tree itself is flat now.
        assert_eq!(ast.glob, "+(a|+(b|c)|d)");
        assert_eq!(ast.to_string(super::ROOT), "+(a|b|c|d)");
        match ast.into_mm_pattern("+(a|+(b|c)|d)").unwrap() {
            MmPattern::Program(prog) => {
                assert_eq!(prog.render(), "(?:a|b|c|d)+");
            }
            MmPattern::Literal(l) => panic!("literal {l:?}"),
        }
    }

    proptest::proptest! {
        /// The reconstruction the cased check reads is the segment as written, for every
        /// segment minimatch accepts.
        #[test]
        fn the_glob_is_reconstructed_from_any_segment(seg in "[a-c*?!@+\\[\\]()|\\\\.]{0,12}") {
            let mut budget = NodeBudget::new(10_000);
            if let Ok(ast) = Ast::from_glob(&seg, Options::DEFAULT, &mut budget) {
                proptest::prop_assert_eq!(&ast.glob, &seg);
            }
        }
    }
}
