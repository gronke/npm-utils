//! A grammar of glob patterns as proptest strategies. A pattern is built as a small tree and
//! rendered to text; where its language is simple enough, a path it matches is drawn in the same
//! step, which gives the property tests a positive oracle without a second matcher.

use npm_utils::minimatch::Options;
use proptest::prelude::*;

/// The literal characters: `é` is two bytes, `A` has a case, `.` meets the dot rules.
const LETTERS: &[char] = &['a', 'b', 'c', 'x', 'A', 'é', '.'];
/// What the oracle instantiates wildcards with: no dot, so the dot rules never decide an answer.
const PLAIN: &[char] = &['a', 'b', 'c'];
/// Syntax minimatch reads in its own way, one shape per quirk, verbatim.
const QUIRKY: &[&str] = &[
    "[abc",
    "x*(",
    "*\\.js",
    "a*\\|b",
    "[[:print:]]",
    "[[:punct:]]",
    "[[:nope:]]",
    "[z-a]",
    "a\\\\*{b,c}",
    "!(a|b)",
    "a*\\|b!(c)\\|d",
    "x!(a\\|b\\|c)y",
    "a\\|b\\|c",
];
/// The POSIX classes whose members strict mode and quirks mode agree on.
const POSIX: &[&str] = &["alpha", "digit", "lower", "upper", "alnum", "xdigit"];
/// The ones strict mode corrects, and the one both leave a literal.
const POSIX_QUIRKY: &[&str] = &["space", "punct", "print", "nope"];

/// The names every generated pattern meets in the live differential: dot corners, alternation
/// fodder and the names the pinned cases ask about.
pub const NAMES: &[&str] = &[
    "",
    "a",
    "b",
    "c",
    "x",
    "y",
    ".",
    "..",
    ".a",
    ".x",
    "a.",
    "aa",
    "ab",
    "ba",
    "aab",
    "aba",
    "abc",
    "abd",
    "ac",
    "axb",
    "bc",
    "xb",
    "xbc",
    "xy",
    "xyz",
    "ababab",
    "a|b",
    "(a)",
    "a.js",
    "a.ts",
    "a.jsx",
    "a.b",
    "readme.md~",
    "secret.pem",
    "certs",
    "xay",
    "xby",
    "xaby",
    "bbc",
    "d",
    "aaay",
];

#[derive(Debug, Clone)]
pub enum ClassItem {
    Single(char),
    Range(char, char),
    Posix(&'static str),
}

#[derive(Debug, Clone)]
pub enum Brace {
    Set(Vec<String>),
    Seq(i64, i64),
}

#[derive(Debug, Clone)]
pub enum Node {
    Lit(String),
    Star,
    Qmark,
    Escape(char),
    Class { negate: bool, items: Vec<ClassItem> },
    Ext { kind: char, alts: Vec<Vec<Node>> },
    Brace(Brace),
    Quirky(&'static str),
}

#[derive(Debug, Clone)]
pub enum Seg {
    Globstar,
    Plain(Vec<Node>),
}

#[derive(Debug, Clone)]
pub struct Pattern {
    pub comment: bool,
    pub negate: bool,
    pub segs: Vec<Seg>,
    pub trailing_slash: bool,
}

/// What the generator may produce.
#[derive(Debug, Clone, Copy)]
pub struct Grammar {
    pub max_segments: usize,
    pub globstar: bool,
    pub classes: bool,
    pub posix: bool,
    pub extglobs: bool,
    pub negation_groups: bool,
    pub escapes: bool,
    pub braces: bool,
    pub quirky: bool,
    pub negate: bool,
    pub comments: bool,
    pub trailing_slash: bool,
    pub dots: bool,
    /// Three alternatives per group instead of two, enough for a three-way negation (two pipes
    /// in one segment come from the quirk shapes), at a cost the live differential pays once
    /// and the property suite would pay on every run.
    pub wide: bool,
}

impl Grammar {
    /// Everything the grammar knows.
    pub const FULL: Grammar = Grammar {
        max_segments: 3,
        globstar: true,
        classes: true,
        posix: true,
        extglobs: true,
        negation_groups: true,
        escapes: true,
        braces: true,
        quirky: true,
        negate: true,
        comments: true,
        trailing_slash: true,
        dots: true,
        wide: false,
    };
    /// The patterns whose language the oracle instantiates: no quirk shapes, no escapes, no
    /// dots, and no negation, comment or trailing slash, which invert or void the answer.
    pub const ORACLE: Grammar = Grammar {
        quirky: false,
        escapes: false,
        dots: false,
        negate: false,
        comments: false,
        trailing_slash: false,
        ..Grammar::FULL
    };
    /// What strict mode and quirks mode must agree on: no quirk shapes, no escapes (they feed
    /// the raw-extension fast path and brace expansion strips them), no POSIX classes.
    pub const QUIRK_FREE: Grammar = Grammar {
        quirky: false,
        escapes: false,
        posix: false,
        ..Grammar::FULL
    };
    /// The live differential against the JavaScript: everything, quirks included, wide.
    pub const DIFFERENTIAL: Grammar = Grammar {
        wide: true,
        ..Grammar::FULL
    };
}

impl Node {
    fn render(&self, out: &mut String) {
        match self {
            Node::Lit(s) => out.push_str(s),
            Node::Star => out.push('*'),
            Node::Qmark => out.push('?'),
            Node::Escape(c) => {
                out.push('\\');
                out.push(*c);
            }
            Node::Class { negate, items } => {
                out.push('[');
                if *negate {
                    out.push('!');
                }
                for item in items {
                    match item {
                        ClassItem::Single(c) => out.push(*c),
                        ClassItem::Range(lo, hi) => {
                            out.push(*lo);
                            out.push('-');
                            out.push(*hi);
                        }
                        ClassItem::Posix(name) => {
                            out.push_str("[:");
                            out.push_str(name);
                            out.push_str(":]");
                        }
                    }
                }
                out.push(']');
            }
            Node::Ext { kind, alts } => {
                out.push(*kind);
                out.push('(');
                for (i, alt) in alts.iter().enumerate() {
                    if i > 0 {
                        out.push('|');
                    }
                    for node in alt {
                        node.render(out);
                    }
                }
                out.push(')');
            }
            Node::Brace(Brace::Set(alts)) => {
                out.push('{');
                out.push_str(&alts.join(","));
                out.push('}');
            }
            Node::Brace(Brace::Seq(lo, hi)) => out.push_str(&format!("{{{lo}..{hi}}}")),
            Node::Quirky(s) => out.push_str(s),
        }
    }
}

impl Pattern {
    /// The pattern as minimatch reads it.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.comment {
            out.push('#');
        }
        if self.negate {
            out.push('!');
        }
        for (i, seg) in self.segs.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            match seg {
                Seg::Globstar => out.push_str("**"),
                Seg::Plain(nodes) => {
                    for node in nodes {
                        node.render(&mut out);
                    }
                }
            }
        }
        if self.trailing_slash {
            out.push('/');
        }
        out
    }
}

fn letter(dots: bool) -> impl Strategy<Value = char> {
    let set: Vec<char> = LETTERS
        .iter()
        .copied()
        .filter(|c| dots || *c != '.')
        .collect();
    prop::sample::select(set)
}

fn plain_text(min: usize, max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(PLAIN.to_vec()), min..=max)
        .prop_map(|cs| cs.into_iter().collect())
}

fn class_item(g: Grammar) -> BoxedStrategy<ClassItem> {
    let mut items: Vec<BoxedStrategy<ClassItem>> = vec![
        prop::sample::select(vec!['a', 'b', 'c', 'x', 'A'])
            .prop_map(ClassItem::Single)
            .boxed(),
        prop::sample::select(vec![('a', 'c'), ('x', 'z'), ('A', 'C'), ('0', '9')])
            .prop_map(|(lo, hi)| ClassItem::Range(lo, hi))
            .boxed(),
    ];
    if g.posix {
        let mut names = POSIX.to_vec();
        if g.quirky {
            names.extend_from_slice(POSIX_QUIRKY);
        }
        items.push(
            prop::sample::select(names)
                .prop_map(ClassItem::Posix)
                .boxed(),
        );
    }
    prop::strategy::Union::new(items).boxed()
}

/// A member of a class item the oracle can rely on in both modes.
fn class_member(item: &ClassItem) -> char {
    match item {
        ClassItem::Single(c) => *c,
        ClassItem::Range(lo, _) => *lo,
        ClassItem::Posix("digit") => '7',
        ClassItem::Posix("upper") => 'A',
        ClassItem::Posix("xdigit") => 'f',
        ClassItem::Posix(_) => 'a',
    }
}

fn brace() -> impl Strategy<Value = Brace> {
    prop_oneof![
        prop::collection::vec(plain_text(1, 2), 2..=3).prop_map(Brace::Set),
        (1i64..=9, 0i64..=3).prop_map(|(lo, n)| Brace::Seq(lo, lo + n)),
    ]
}

fn brace_members(brace: &Brace) -> Vec<String> {
    match brace {
        Brace::Set(alts) => alts.clone(),
        Brace::Seq(lo, hi) => (*lo..=*hi).map(|n| n.to_string()).collect(),
    }
}

/// One node of a segment, groups nested to depth two. Literals and wildcards are the
/// common case; a class, an escape, a brace set or a quirk shape appears about once in ten
/// nodes, so a pattern stays cheap to compile and the braces do not multiply the expansions.
pub fn node(g: Grammar) -> BoxedStrategy<Node> {
    let mut leaves: Vec<(u32, BoxedStrategy<Node>)> = vec![
        (
            4,
            prop::collection::vec(letter(g.dots), 1..=3)
                .prop_map(|cs| Node::Lit(cs.into_iter().collect()))
                .boxed(),
        ),
        (2, Just(Node::Star).boxed()),
        (2, Just(Node::Qmark).boxed()),
    ];
    if g.classes {
        leaves.push((
            1,
            (any::<bool>(), prop::collection::vec(class_item(g), 1..=3))
                .prop_map(|(negate, items)| Node::Class { negate, items })
                .boxed(),
        ));
    }
    if g.escapes {
        leaves.push((
            1,
            prop::sample::select(vec![
                '*', '?', '[', ']', '(', ')', '\\', '.', '!', '{', '}', '|', 'a',
            ])
            .prop_map(Node::Escape)
            .boxed(),
        ));
    }
    if g.braces {
        leaves.push((1, brace().prop_map(Node::Brace).boxed()));
    }
    if g.quirky {
        leaves.push((
            1,
            prop::sample::select(QUIRKY.to_vec())
                .prop_map(Node::Quirky)
                .boxed(),
        ));
    }
    let leaf = prop::strategy::Union::new_weighted(leaves);
    if !g.extglobs {
        return leaf.boxed();
    }
    let kinds: Vec<char> = if g.negation_groups {
        vec!['@', '?', '*', '+', '!']
    } else {
        vec!['@', '?', '*', '+']
    };
    let alternatives = if g.wide { 3 } else { 2 };
    leaf.prop_recursive(2, 6, 2, move |inner| {
        (
            prop::sample::select(kinds.clone()),
            prop::collection::vec(prop::collection::vec(inner, 1..=2), 1..=alternatives),
        )
            .prop_map(|(kind, alts)| Node::Ext { kind, alts })
    })
    .boxed()
}

fn segment(g: Grammar) -> BoxedStrategy<Seg> {
    let plain = prop::collection::vec(node(g), 1..=2).prop_map(Seg::Plain);
    if !g.globstar {
        return plain.boxed();
    }
    prop_oneof![4 => plain, 1 => Just(Seg::Globstar)].boxed()
}

/// A whole pattern under the grammar.
pub fn pattern(g: Grammar) -> BoxedStrategy<Pattern> {
    let flag = |on: bool, weight: f64| {
        if on {
            prop::bool::weighted(weight).boxed()
        } else {
            Just(false).boxed()
        }
    };
    (
        flag(g.comments, 0.05),
        flag(g.negate, 0.2),
        prop::collection::vec(segment(g), 1..=g.max_segments),
        flag(g.trailing_slash, 0.1),
    )
        .prop_map(|(comment, negate, segs, trailing_slash)| Pattern {
            comment,
            negate,
            segs,
            trailing_slash,
        })
        .boxed()
}

/// A node together with a text in its language, drawn in one step so a shrink keeps them
/// consistent. Every witness is non-empty and dot-free, so the empty-segment and dot rules
/// never decide the answer.
type Witnessed = (Node, String);

fn witnessed_leaf(g: Grammar) -> BoxedStrategy<Witnessed> {
    let mut leaves: Vec<(u32, BoxedStrategy<Witnessed>)> = vec![
        (
            4,
            plain_text(1, 3)
                .prop_map(|s| (Node::Lit(s.clone()), s))
                .boxed(),
        ),
        (2, plain_text(1, 4).prop_map(|s| (Node::Star, s)).boxed()),
        (
            2,
            prop::sample::select(PLAIN.to_vec())
                .prop_map(|c| (Node::Qmark, c.to_string()))
                .boxed(),
        ),
    ];
    if g.classes {
        leaves.push((
            1,
            (any::<bool>(), prop::collection::vec(class_item(g), 1..=3))
                .prop_map(|(negate, items)| {
                    // `_` is in no item the oracle grammar produces.
                    let witness = if negate { '_' } else { class_member(&items[0]) };
                    (Node::Class { negate, items }, witness.to_string())
                })
                .boxed(),
        ));
    }
    if g.braces {
        leaves.push((
            1,
            (brace(), any::<prop::sample::Index>())
                .prop_map(|(brace, pick)| {
                    let members = brace_members(&brace);
                    let witness = members[pick.index(members.len())].clone();
                    (Node::Brace(brace), witness)
                })
                .boxed(),
        ));
    }
    if g.extglobs && g.negation_groups {
        // A negation over literals: the sentinel matches none of them, in any position.
        leaves.push((
            1,
            prop::collection::vec(plain_text(1, 2), 1..=3)
                .prop_map(|alts| {
                    let alts = alts.into_iter().map(|a| vec![Node::Lit(a)]).collect();
                    (Node::Ext { kind: '!', alts }, "zz9".to_string())
                })
                .boxed(),
        ));
    }
    prop::strategy::Union::new_weighted(leaves).boxed()
}

fn witnessed_node(g: Grammar) -> BoxedStrategy<Witnessed> {
    let leaf = witnessed_leaf(g);
    if !g.extglobs {
        return leaf;
    }
    leaf.prop_recursive(2, 6, 2, |inner| {
        (
            prop::sample::select(vec!['@', '?', '*', '+']),
            prop::collection::vec(prop::collection::vec(inner, 1..=2), 1..=2),
            prop::collection::vec(any::<prop::sample::Index>(), 1..=3),
        )
            .prop_map(|(kind, alts, picks)| {
                let pick = |i: &prop::sample::Index| -> String {
                    alts[i.index(alts.len())]
                        .iter()
                        .map(|(_, w)| w.as_str())
                        .collect()
                };
                let witness: String = match kind {
                    '@' | '?' => pick(&picks[0]),
                    _ => picks.iter().map(pick).collect(),
                };
                let alts = alts
                    .iter()
                    .map(|alt| alt.iter().map(|(n, _)| n.clone()).collect())
                    .collect();
                (Node::Ext { kind, alts }, witness)
            })
    })
    .boxed()
}

fn witnessed_segment(g: Grammar) -> BoxedStrategy<(Seg, Vec<String>)> {
    let plain = prop::collection::vec(witnessed_node(g), 1..=2).prop_map(|nodes| {
        let witness: String = nodes.iter().map(|(_, w)| w.as_str()).collect();
        let nodes = nodes.into_iter().map(|(n, _)| n).collect();
        (Seg::Plain(nodes), vec![witness])
    });
    if !g.globstar {
        return plain.boxed();
    }
    prop_oneof![
        4 => plain,
        1 => prop::collection::vec(plain_text(1, 3), 1..=2).prop_map(|names| (Seg::Globstar, names)),
    ]
    .boxed()
}

/// A pattern and a path in its language, for the grammars without quirks, escapes and dots.
pub fn pattern_with_path(g: Grammar) -> BoxedStrategy<(Pattern, String)> {
    prop::collection::vec(witnessed_segment(g), 1..=g.max_segments)
        .prop_map(|mut segs| {
            // A pattern may not open with `!(`: that is negation in npm's reading and a
            // refusal in the strict one, so a literal goes first.
            if let Some((Seg::Plain(nodes), names)) = segs.first_mut() {
                if matches!(nodes.first(), Some(Node::Ext { kind: '!', .. })) {
                    nodes.insert(0, Node::Lit("a".to_string()));
                    names[0].insert(0, 'a');
                }
            }
            let path = segs
                .iter()
                .flat_map(|(_, names)| names.iter().cloned())
                .collect::<Vec<_>>()
                .join("/");
            let pattern = Pattern {
                comment: false,
                negate: false,
                segs: segs.into_iter().map(|(seg, _)| seg).collect(),
                trailing_slash: false,
            };
            (pattern, path)
        })
        .boxed()
}

/// Paths of the grammar's letters, slashes included: empty, dotted and doubled-slash corners.
pub fn arbitrary_path() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec!['a', 'b', 'c', 'x', 'A', 'é', '.', '/']),
        0..=12,
    )
    .prop_map(|cs| cs.into_iter().collect())
}

/// Pattern text with no grammar behind it: glob symbol soup, printable Unicode and anything.
pub fn raw_pattern() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[a-c.*?\\[\\]!(){}|\\\\/,@+^$#:-]{0,24}",
        1 => "\\PC{0,64}",
        1 => any::<String>().prop_map(|s| s.chars().take(64).collect()),
    ]
}

/// The shapes that amplify a cost: deep nesting, sequential groups, wide alternation and brace
/// fan-out, each up to and past a budget.
pub fn bomb() -> impl Strategy<Value = String> {
    prop_oneof![
        (
            prop::sample::select(vec!["@(", "?(", "*(", "+(", "!("]),
            1usize..=140,
            any::<bool>()
        )
            .prop_map(|(open, depth, prefixed)| {
                let open = if prefixed {
                    format!("a{open}")
                } else {
                    open.to_string()
                };
                format!("{}a{}", open.repeat(depth), ")".repeat(depth))
            }),
        (
            prop::sample::select(vec!["!(a)", "+(a)", "@(a|b)", "*(a|b)"]),
            1usize..=48
        )
            .prop_map(|(group, k)| format!("x{}y", group.repeat(k))),
        (1usize..=600).prop_map(|n| {
            let alts: Vec<String> = (0..n).map(|i| format!("a{i}")).collect();
            format!("@({})", alts.join("|"))
        }),
        (1usize..=4, 1u32..=10_001).prop_map(|(groups, n)| format!("{{1..{n}}}").repeat(groups)),
    ]
}

/// The option presets the properties run under.
pub fn preset() -> impl Strategy<Value = Options> {
    let d = Options::DEFAULT;
    prop::sample::select(vec![
        d,
        Options { dot: true, ..d },
        Options { nocase: true, ..d },
        Options {
            match_base: true,
            ..d
        },
        Options::ignore_walk(),
        Options { quirks: false, ..d },
        Options {
            flip_negate: true,
            ..d
        },
    ])
}
