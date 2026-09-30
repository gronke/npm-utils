//! The pattern tree of one path segment: ast.js ported. Extglob groups nest as nodes of an
//! arena; a node records the index it had in its parent when it was created and never updates
//! it, as the JavaScript does, because `isStart` reads that stale value after an adoption
//! (`@(a|@(*|b))` depends on it).

use super::class::{parse_class, regexp_escape};
use super::{unescape, Error, Options};

const START_NO_TRAVERSAL: &str = r"(?!(?:^|/)\.\.?(?:$|/))";
const START_NO_DOT: &str = r"(?!\.)";
const QMARK: &str = "[^/]";
const STAR: &str = "[^/]*?";
const STAR_NO_EMPTY: &str = "[^/]+?";
/// `reSpecials`: the characters an escape keeps escaped in the regex.
const RE_SPECIALS: &str = "().*{}+?[]^$\\!";
const ROOT: usize = 0;

fn is_ext_type(c: char) -> bool {
    matches!(c, '!' | '?' | '+' | '*' | '@')
}

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
        ('@', c) if is_ext_type(c) => Some(c),
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
    uflag: bool,
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

/// `toMMPattern`'s result: a literal to compare, or a compiled regex.
#[derive(Debug)]
pub(super) enum MmPattern {
    Literal(String),
    Regex {
        re: fancy_regex::Regex,
        src: String,
        glob: String,
    },
}

pub(super) struct Ast {
    nodes: Vec<Node>,
    negs: Vec<usize>,
    filled_negs: bool,
    options: Options,
}

impl Ast {
    /// `AST.fromGlob`.
    pub(super) fn from_glob(pattern: &str, options: Options) -> Ast {
        let mut ast = Ast {
            nodes: Vec::new(),
            negs: Vec::new(),
            filled_negs: false,
            options,
        };
        ast.new_node(None, None);
        let chars: Vec<char> = pattern.chars().collect();
        ast.parse_ast(&chars, ROOT, 0, 0);
        ast
    }

    fn new_node(&mut self, kind: Option<char>, parent: Option<usize>) -> usize {
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
            uflag: false,
            empty_ext: false,
        });
        id
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
            return self.is_end(parent);
        }
        let pl = self.nodes[parent].parts.len();
        pl > 0 && self.nodes[n].parent_index == pl - 1
    }

    fn copy_in(&mut self, dest: usize, part: Piece) {
        match part {
            Piece::Str(s) => self.push_str(dest, s),
            Piece::Node(id) => {
                let clone = self.clone_node(id, dest);
                self.push_node(dest, clone);
            }
        }
    }

    fn clone_node(&mut self, src: usize, parent: usize) -> usize {
        let clone = self.new_node(self.nodes[src].kind, Some(parent));
        let parts = self.nodes[src].parts.clone();
        for p in parts {
            self.copy_in(clone, p);
        }
        clone
    }

    /// `#fillNegs`: every `!` group gets what follows it (up to the end of each plain ancestor)
    /// appended to each of its alternatives, so `!(a)b` means "not `ab`", last group first.
    fn fill_negs(&mut self) {
        if self.filled_negs {
            return;
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
                        self.copy_in(part, sibling.clone());
                    }
                    i += 1;
                }
                p = ppi;
                pp = self.nodes[p].parent;
            }
        }
    }

    /// `#parseAST`: the segment from `pos`, into `ast`; returns where it stopped.
    fn parse_ast(&mut self, s: &[char], ast: usize, pos: usize, ext_depth: usize) -> usize {
        const MAX_DEPTH: usize = 2;
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
                    !noext && is_ext_type(c) && s.get(i) == Some(&'(') && ext_depth <= MAX_DEPTH;
                if do_recurse {
                    self.push_str(ast, std::mem::take(&mut acc));
                    let ext = self.new_node(Some(c), Some(ast));
                    i = self.parse_ast(s, ext, i, ext_depth + 1);
                    self.push_node(ast, ext);
                    continue;
                }
                acc.push(c);
            }
            self.push_str(ast, acc);
            return i;
        }
        // Some kind of extglob; pos is at the `(`. Find the next `|` or `)`.
        let kind = self.nodes[ast].kind;
        let mut i = pos + 1;
        let mut part = self.new_node(None, Some(ast));
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
                && is_ext_type(c)
                && s.get(i) == Some(&'(')
                && (ext_depth <= MAX_DEPTH || can_adopt);
            if do_recurse {
                let depth_add = if can_adopt { 0 } else { 1 };
                self.push_str(part, std::mem::take(&mut acc));
                let ext = self.new_node(Some(c), Some(part));
                self.push_node(part, ext);
                i = self.parse_ast(s, ext, i, ext_depth + depth_add);
                continue;
            }
            if c == '|' {
                self.push_str(part, std::mem::take(&mut acc));
                parts.push(part);
                part = self.new_node(None, Some(ast));
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
                return i;
            }
            acc.push(c);
        }
        // An unfinished extglob: not an extglob, maybe something else in there.
        self.nodes[ast].kind = None;
        self.nodes[ast].has_magic = None;
        self.nodes[ast].parts = vec![Piece::Str(s[pos - 1..].iter().collect())];
        i
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

    fn adopt_with_space(&mut self, n: usize, child: usize, index: usize) {
        let Piece::Node(gc) = self.nodes[child].parts[0] else {
            return;
        };
        let blank = self.new_node(None, Some(gc));
        self.nodes[blank].parts.push(Piece::Str(String::new()));
        self.push_node(gc, blank);
        self.adopt(n, child, index);
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
    fn flatten(&mut self, n: usize) {
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
                self.flatten(c);
            }
            return;
        }
        let mut iterations = 0;
        loop {
            let mut done = true;
            let mut i = 0;
            while i < self.nodes[n].parts.len() {
                if let Piece::Node(c) = self.nodes[n].parts[i] {
                    self.flatten(c);
                    if self.can_adopt(n, c, adoption) {
                        done = false;
                        self.adopt(n, c, i);
                    } else if self.can_adopt(n, c, adoption_with_space) {
                        done = false;
                        self.adopt_with_space(n, c, i);
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
    }

    /// `toMMPattern`: the literal when nothing is magic, else the compiled regex.
    pub(super) fn into_mm_pattern(mut self, pattern: &str) -> Result<MmPattern, Error> {
        let glob = self.to_string(ROOT);
        let src = self.regexp_source(ROOT, None);
        // Under nocase a pattern with cased letters needs the engine even without magic.
        let any_magic = src.has_magic
            || self.nodes[ROOT].has_magic.unwrap_or(false)
            || (self.options.nocase
                && glob
                    .chars()
                    .any(|c| c.to_uppercase().to_string() != c.to_lowercase().to_string()));
        if !any_magic {
            return Ok(MmPattern::Literal(src.body));
        }
        let re = fancy_regex::RegexBuilder::new(&format!("^{}$", src.re))
            .case_insensitive(self.options.nocase)
            .backtrack_limit(self.options.backtrack_limit)
            .build()
            .map_err(|e| Error::Regex {
                pattern: pattern.to_string(),
                source: e.to_string(),
            })?;
        Ok(MmPattern::Regex {
            re,
            src: src.re,
            glob,
        })
    }

    /// `toRegExpSource`.
    pub(super) fn regexp_source(&mut self, n: usize, allow_dot: Option<bool>) -> Src {
        let dot = allow_dot.unwrap_or(self.options.dot);
        if n == ROOT {
            self.flatten(ROOT);
            self.fill_negs();
        }
        let Some(kind) = self.nodes[n].kind else {
            return self.plain_source(n, allow_dot, dot);
        };
        // The body is computed twice for a repeat at the start, once without dots and once
        // with, so `*(?)` can match `x.y`.
        let repeated = kind == '*' || kind == '+';
        let start = if kind == '!' { "(?:(?!(?:" } else { "(?:" };
        let mut body = self.parts_to_regexp(n, dot);
        if self.is_start(n) && self.is_end(n) && body.is_empty() && kind != '!' {
            // An invalid extglob: something has to be present when it is the whole segment.
            let s = self.to_string(n);
            self.nodes[n].parts = vec![Piece::Str(s.clone())];
            self.nodes[n].kind = None;
            self.nodes[n].has_magic = None;
            return Src {
                re: s.clone(),
                body: unescape(&s, true),
                has_magic: false,
                uflag: false,
            };
        }
        let mut body_dot_allowed = if !repeated || allow_dot == Some(true) || dot {
            String::new()
        } else {
            self.parts_to_regexp(n, true)
        };
        if body_dot_allowed == body {
            body_dot_allowed.clear();
        }
        if !body_dot_allowed.is_empty() {
            body = format!("(?:{body})(?:{body_dot_allowed})*?");
        }
        let re = if kind == '!' && self.nodes[n].empty_ext {
            // An empty `!()` is exactly a star that matches something.
            let no_dot = if self.is_start(n) && !dot {
                START_NO_DOT
            } else {
                ""
            };
            format!("{no_dot}{STAR_NO_EMPTY}")
        } else {
            let close = match kind {
                // `!()` must match something, but `!(x)` can match ''.
                '!' => {
                    let no_dot = if self.is_start(n) && !dot && allow_dot != Some(true) {
                        START_NO_DOT
                    } else {
                        ""
                    };
                    format!("{}{no_dot}{STAR})", "))")
                }
                '@' => ")".to_string(),
                '?' => ")?".to_string(),
                '+' if !body_dot_allowed.is_empty() => ")".to_string(),
                '*' if !body_dot_allowed.is_empty() => ")?".to_string(),
                kind => format!("){kind}"),
            };
            format!("{start}{body}{close}")
        };
        let has_magic = self.nodes[n].has_magic.unwrap_or(false);
        self.nodes[n].has_magic = Some(has_magic);
        Src {
            re,
            body: unescape(&body, true),
            has_magic,
            uflag: self.nodes[n].uflag,
        }
    }

    fn plain_source(&mut self, n: usize, allow_dot: Option<bool>, dot: bool) -> Src {
        let parts = self.nodes[n].parts.clone();
        let no_empty =
            self.is_start(n) && self.is_end(n) && parts.iter().all(|p| matches!(p, Piece::Str(_)));
        let mut src = String::new();
        for p in parts {
            let piece = match p {
                Piece::Str(s) => parse_glob(&s, no_empty),
                Piece::Node(id) => self.regexp_source(id, allow_dot),
            };
            let node = &mut self.nodes[n];
            node.has_magic = Some(node.has_magic.unwrap_or(false) || piece.has_magic);
            node.uflag |= piece.uflag;
            src.push_str(&piece.re);
        }
        let mut start = "";
        if self.is_start(n) {
            if let Some(Piece::Str(first)) = self.nodes[n].parts.first() {
                // This string matches the start of the pattern, so dots need protecting: `.`
                // and `..` never match unless the pattern is exactly that.
                let dot_trav_allowed =
                    self.nodes[n].parts.len() == 1 && (first == "." || first == "..");
                if !dot_trav_allowed {
                    let chars: Vec<char> = src.chars().collect();
                    let aps = |c: Option<&char>| matches!(c, Some('[') | Some('.'));
                    let need_no_trav = (dot && aps(chars.first()))
                        || (src.starts_with("\\.") && aps(chars.get(2)))
                        || (src.starts_with("\\.\\.") && aps(chars.get(4)));
                    let need_no_dot = !dot && allow_dot != Some(true) && aps(chars.first());
                    start = if need_no_trav {
                        START_NO_TRAVERSAL
                    } else if need_no_dot {
                        START_NO_DOT
                    } else {
                        ""
                    };
                }
            }
        }
        // The "end of path portion" pattern binds negation tails.
        let in_negation = self.nodes[n]
            .parent
            .is_some_and(|p| self.nodes[p].kind == Some('!'));
        let end = if self.is_end(n) && self.filled_negs && in_negation {
            r"(?:$|\/)"
        } else {
            ""
        };
        let has_magic = self.nodes[n].has_magic.unwrap_or(false);
        self.nodes[n].has_magic = Some(has_magic);
        Src {
            re: format!("{start}{src}{end}"),
            body: unescape(&src, true),
            has_magic,
            uflag: self.nodes[n].uflag,
        }
    }

    /// `#partsToRegExp`: the alternatives joined, empties dropped when the group is the whole
    /// segment.
    fn parts_to_regexp(&mut self, n: usize, dot: bool) -> String {
        let parts = self.nodes[n].parts.clone();
        let keep_empties = !(self.is_start(n) && self.is_end(n));
        let mut out = Vec::new();
        for p in parts {
            let Piece::Node(id) = p else {
                continue;
            };
            let src = self.regexp_source(id, Some(dot));
            self.nodes[n].uflag |= src.uflag;
            if keep_empties || !src.re.is_empty() {
                out.push(src.re);
            }
        }
        out.join("|")
    }
}

/// `#parseGlob`: one plain string of a segment as a regex source.
fn parse_glob(glob: &str, no_empty: bool) -> Src {
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
            if RE_SPECIALS.contains(c) {
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
            let class = parse_class(&chars, i);
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
    Src {
        re,
        body: unescape(glob, true),
        has_magic,
        uflag,
    }
}

#[cfg(test)]
mod tests {
    use super::super::Options;
    use super::{Ast, MmPattern};

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
            let mm = Ast::from_glob(pattern, options(preset))
                .into_mm_pattern(pattern)
                .unwrap_or_else(|e| panic!("{pattern:?} ({preset}): {e}"));
            match (expect, &mm) {
                (Expect::Literal(want), MmPattern::Literal(got)) => {
                    assert_eq!(got, want, "{pattern:?} ({preset})");
                }
                (Expect::Src(want), MmPattern::Regex { src, .. }) => {
                    assert_eq!(src, want, "{pattern:?} ({preset})");
                }
                (Expect::Literal(want), MmPattern::Regex { src, .. }) => {
                    panic!("{pattern:?} ({preset}): expected the literal {want:?}, got the regex {src:?}");
                }
                (Expect::Src(want), MmPattern::Literal(got)) => {
                    panic!("{pattern:?} ({preset}): expected the regex {want:?}, got the literal {got:?}");
                }
            }
        }
    }

    #[test]
    fn the_glob_is_reconstructed_before_flattening() {
        let mm = Ast::from_glob("+(a|+(b|c)|d)", Options::DEFAULT)
            .into_mm_pattern("+(a|+(b|c)|d)")
            .unwrap();
        match mm {
            MmPattern::Regex { glob, src, .. } => {
                assert_eq!(glob, "+(a|+(b|c)|d)");
                assert_eq!(src, "(?:a|b|c|d)+");
            }
            MmPattern::Literal(l) => panic!("literal {l:?}"),
        }
    }
}
