use super::{BraceLimit, Error, Minimatch, Options};

fn rule(line: &str) -> Minimatch {
    Minimatch::new(line, Options::ignore_walk()).unwrap_or_else(|e| panic!("{e}"))
}

fn hit(line: &str, path: &str) -> bool {
    rule(line).is_match(path).unwrap_or_else(|e| panic!("{e}"))
}

fn partial(line: &str, path: &str) -> bool {
    rule(line)
        .is_match_partial(path)
        .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn a_deep_adoption_chain_compiles_in_linear_time() {
    // `@` inside `@` adopts, so the chain never charges the grammar's depth guard and the
    // nesting budget is the only bound. The end check asked its parent twice per plain node,
    // which doubled the work per level: depth 22 took 0.6 s, depth 40 would have taken hours.
    let depth = 100;
    let pattern = format!("{}a{}", "a@(".repeat(depth), ")".repeat(depth));
    let start = std::time::Instant::now();
    let mm = Minimatch::new(&pattern, Options::DEFAULT).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "took {:?}",
        start.elapsed()
    );
    assert!(mm.is_match(&"a".repeat(depth + 1)).unwrap());
    assert!(!mm.is_match(&"a".repeat(depth)).unwrap());
}

#[test]
fn nesting_past_the_budget_is_an_error_in_both_modes() {
    // Adoption chains never charge the grammar's depth guard, so without this budget 800
    // nested groups recurse the parser past a small caller stack and abort the process,
    // where npm throws a RangeError a few thousand in.
    let nested = |depth: usize| {
        let mut p = "+(".repeat(depth);
        p.push('a');
        p.push_str(&")".repeat(depth));
        p
    };
    for options in [
        Options::DEFAULT,
        Options {
            quirks: false,
            ..Options::DEFAULT
        },
    ] {
        assert!(Minimatch::new(&nested(128), options).is_ok());
        match Minimatch::new(&nested(129), options) {
            Err(Error::Nesting { pattern, limit }) => {
                assert_eq!(limit, 128);
                assert!(pattern.starts_with("+("), "{pattern}");
            }
            other => panic!("expected a nesting error, got {other:?}"),
        }
    }
    // Sequential groups do not nest; the node budget bounds them instead, two nodes each.
    assert!(Minimatch::new(&"+(a)".repeat(1000), Options::DEFAULT).is_ok());
}

#[test]
fn sequential_negations_past_the_node_budget_are_an_error() {
    // Each `!()` group takes a copy of everything after it, last group first, so k groups
    // build about 2^(k+1) nodes, as in the JavaScript: twelve groups compile (8192 nodes, a
    // few seconds in a debug build), thirteen fail in milliseconds; without the budget twenty
    // groups would take minutes and gigabytes.
    let line = |k: usize| format!("x{}y", "!(a)".repeat(k));
    let start = std::time::Instant::now();
    assert!(Minimatch::new(&line(8), Options::DEFAULT).is_ok());
    match Minimatch::new(&line(13), Options::DEFAULT) {
        Err(Error::Nodes { pattern, limit }) => {
            assert_eq!(limit, 10_000);
            assert!(pattern.starts_with("x!(a)"), "{pattern}");
        }
        other => panic!("expected a node budget error, got {other:?}"),
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
    // A small budget shows the boundary moving with the count.
    let small = Options {
        max_extglob_nodes: 50,
        ..Options::DEFAULT
    };
    assert!(Minimatch::new(&line(4), small).is_ok());
    assert!(matches!(
        Minimatch::new(&line(5), small),
        Err(Error::Nodes { limit: 50, .. })
    ));
}

#[test]
fn the_node_budget_spans_the_brace_expansions() {
    // Plain segments cost no nodes, so a wide expansion alone is fine; a group in every
    // expansion counts across all of them.
    assert!(Minimatch::new("{1..9000}", Options::DEFAULT).is_ok());
    assert!(Minimatch::new("{1..1000}.@(a|b)", Options::DEFAULT).is_ok());
    match Minimatch::new("{1..5000}.@(a|b)", Options::DEFAULT) {
        Err(Error::Nodes { limit, .. }) => assert_eq!(limit, 10_000),
        other => panic!("expected a node budget error, got {other:?}"),
    }
}

#[test]
fn a_few_trailing_negations_still_match() {
    let mm = Minimatch::new("x!(a)!(b)!(c)y", Options::DEFAULT).unwrap_or_else(|e| panic!("{e}"));
    assert!(mm.is_match("xy").unwrap());
    assert!(mm.is_match("xzy").unwrap());
    assert!(!mm.is_match("x/y").unwrap());
}

#[test]
fn slashless_patterns_match_the_basename_at_any_depth() {
    assert!(hit("node_modules", "/node_modules"));
    assert!(hit("node_modules", "node_modules"));
    assert!(hit("node_modules", "a/b/node_modules"));
    assert!(!hit("node_modules", "a/node_modules_x"));
    assert!(hit("*.orig", "deep/er/file.orig"));
    assert!(hit(".npmrc", "/.npmrc"));
}

#[test]
fn anchored_patterns_match_the_walkers_own_level_only() {
    assert!(hit("/.git", "/.git"));
    assert!(!hit("/.git", ".git"));
    assert!(!hit("/.git", "/sub/.git"));
    assert!(hit("/build/config.gypi", "/build/config.gypi"));
}

#[test]
fn negation_is_a_flag_and_the_case_is_folded() {
    let r = rule("!/readme{,.*[^~$]}");
    assert!(r.negate());
    assert!(r.is_match("/README").unwrap());
    assert!(r.is_match("/README.md").unwrap());
    assert!(r.is_match("/Readme.txt").unwrap());
    assert!(!r.is_match("/README.md~").unwrap());
    assert!(!r.is_match("/README.md$").unwrap());
    assert!(!r.is_match("/readme-first.md").unwrap());
    assert!(!rule("!!foo").negate());
    assert_eq!(rule("!!foo").glob_set(), ["foo"]);
}

#[test]
fn globstar_and_partials() {
    assert!(hit("**/.git/**", "/.git/HEAD"));
    assert!(hit("**/.git/**", "a/b/.git/objects/x"));
    assert!(!hit("**/.git/**", "a/.gitignore"));
    assert!(hit("!dist/**", "dist/index.js"));
    assert!(hit("!dist/**", "dist/nested/x.d.ts"));
    assert!(!hit("!dist/**", "dist"));
    assert!(hit("!dist/**", "dist/"));
    assert!(hit("!lib/*.js", "lib/a.js"));
    assert!(!hit("!lib/*.js", "lib/deep/a.js"));
    assert!(
        partial("!lib/*.js", "lib"),
        "a directory on the way to a match"
    );
    assert!(
        !partial("!lib/*.js", "lib/"),
        "the walker asks without the slash"
    );
    assert!(!partial("!lib/*.js", "src"));
}

#[test]
fn wildcards_never_take_the_dot_directories() {
    assert!(hit("*", ".hidden"));
    assert!(!hit("*", "."));
    assert!(!hit("*", ".."));
    assert!(
        !hit("*", ""),
        "a lone star needs one character, as minimatch has it"
    );
    assert!(hit("?ile", "file"));
    assert!(!hit("?ile", "fille"));
    assert!(hit("[a-c]x", "Bx"));
    assert!(!hit("[!a-c]x", "bx"));
    assert!(hit("\\*literal", "*literal"));
    assert!(!hit("\\*literal", "xliteral"));
    assert!(hit("[[:digit:]]*.log", "1abc.log"));
    assert!(!hit("[[:digit:]]*.log", "abc.log"));
    assert!(hit("[[:alpha:]-]x", "-x"));
}

#[test]
fn extglobs_match_like_minimatch() {
    assert!(hit("*.@(pem|key)", "secret.pem"));
    assert!(hit("*.@(pem|key)", "certs/secret.key"));
    assert!(!hit("*.@(pem|key)", "secret.pemx"));
    assert!(!hit("*.@(pem|key)", "secret.txt"));
    // A leading `!` is the negation flag even before `(`, as in minimatch: the rule then
    // matches a literal `(…)`.
    let not_js = rule("!(*.js)");
    assert!(not_js.negate());
    assert!(!not_js.is_match("a.ts").unwrap());
    assert!(!not_js.is_match("a.js").unwrap());
    assert!(not_js.is_match("(a.js)").unwrap());
    assert!(hit("a.!(js)", "a.ts"));
    assert!(!hit("a.!(js)", "a.js"));
    assert!(hit("a.!(js)", "a.jsx"));
    assert!(!hit("x!(a|b)", "xa"));
    assert!(hit("x!(a|b)", "xc"));
    assert!(hit("x!(a|b)", "x"));
    assert!(hit("+(ab)", "ab"));
    assert!(hit("+(ab)", "ababab"));
    assert!(!hit("+(ab)", ""));
    assert!(!hit("+(ab)", "aba"));
    assert!(hit("*(a|b)c", "c"));
    assert!(hit("*(a|b)c", "abbac"));
    assert!(!hit("*(a|b)c", "abd"));
    assert!(hit("?(x)y", "y"));
    assert!(hit("?(x)y", "xy"));
    assert!(!hit("?(x)y", "xxy"));
    assert!(hit("@(a|b)c", "bc"));
    assert!(!hit("@(a|b)c", "c"));
    assert!(hit("@(a|@(b|c))", "c"), "nested groups");
    assert!(hit("x*(", "x*("), "an unclosed group is literal");
    assert!(!hit("@(a|b)", "."));
}

#[test]
fn brace_sets_and_glob_parts() {
    assert_eq!(rule("a{b,c}d").glob_set(), ["abd", "acd"]);
    assert_eq!(rule("x{,.y}").glob_set(), ["x", "x.y"]);
    assert_eq!(rule("a{,}").glob_set(), ["a"], "the set is deduplicated");
    assert_eq!(
        rule("{foo,bar/baz}").glob_parts(),
        [
            vec!["foo".to_string()],
            vec!["bar".to_string(), "baz".to_string()]
        ]
    );
    assert_eq!(
        rule("foo/").glob_parts(),
        [vec!["foo".to_string(), String::new()]]
    );
    assert_eq!(
        rule("a//b").glob_parts(),
        [vec!["a".to_string(), "b".to_string()]]
    );
    assert_eq!(rule("a/../b").glob_parts(), [vec!["b".to_string()]]);
    assert_eq!(
        rule("**/**/a").glob_parts(),
        [vec!["**".to_string(), "a".to_string()]]
    );
    assert!(rule("*").has_magic());
    assert!(
        rule("a").has_magic(),
        "under nocase a cased letter needs the engine"
    );
    assert!(!rule("1").has_magic());
    assert!(!Minimatch::new("a", Options::DEFAULT).unwrap().has_magic());
    assert!(rule("#x").comment());
    assert!(rule("").empty());
}

#[test]
fn a_brace_bomb_is_an_error_not_an_allocation() {
    // `{1..100000000}` names a hundred million entries; the count is arithmetic, so the
    // error comes back before anything is allocated.
    let new = |p: &str| Minimatch::new(p, Options::ignore_walk());
    assert!(new("{1..100000000}").is_err());
    assert!(new("{1..10001}").is_err());
    assert!(new("{1..10000}").is_ok());
    assert!(new("{9999..1}").is_ok());
    // Multiplicative groups are capped the same way: 3^16 alternatives.
    assert!(new(&"{a,b,c}".repeat(16)).is_err());
    assert!(new(&"{a,b,c}".repeat(4)).is_ok());
    // The group budget bounds the expansion and its recursion depth.
    assert!(new(&"{a,b}".repeat(101)).is_err());
    // A bound beyond i64 is over any budget.
    assert_eq!(
        new("{0..18446744073709551616}").unwrap_err(),
        Error::Braces {
            pattern: "{0..18446744073709551616}".into(),
            limit: BraceLimit::Expansions(10_000)
        }
    );
    // The error names the offending pattern.
    let error = new("dist/{1..100000000}.tgz").unwrap_err().to_string();
    assert!(error.starts_with("\"dist/{1..100000000}.tgz\""), "{error}");
    assert!(error.contains("brace expansion"), "{error}");
}

#[test]
fn the_old_backtracking_bombs_run_in_linear_time() {
    // Against a 200-char name these needed ~1e13 steps in a backtracking matcher; on the
    // engine the pattern body runs on regex-automata and answers at once.
    let long = "b".repeat(200);
    let started = std::time::Instant::now();
    assert!(!hit("*b*b*b*b*b*b*b*c", &long));
    assert!(!hit("+(b|bb)+(b|bb)+(b|bb)+(b|bb)c", &long));
    assert!(hit("*b*b*b*b*b*b*b*", &long));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn a_pathological_pattern_is_an_error_not_a_hang() {
    // The negation classes of the JavaScript simply answer now; the step budget remains a
    // backstop for the polynomial worst case and trips when that runs away.
    let long = "a".repeat(200);
    let started = std::time::Instant::now();
    assert!(!hit("*(!(a))y", &long));
    assert!(!hit("+(!(a)|b)c", &long));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    let tight = Options {
        max_match_steps: 100,
        ..Options::ignore_walk()
    };
    let error = Minimatch::new("*(!(a))y", tight)
        .unwrap()
        .is_match(&long)
        .unwrap_err();
    assert_eq!(
        error,
        Error::Steps {
            pattern: "*(!(a))y".into(),
            limit: 100
        }
    );
    let message = error.to_string();
    assert!(message.starts_with(r#""*(!(a))y""#), "{message}");
    assert!(message.contains("step limit"), "{message}");
    // Stock rules sit far under the budget.
    for fine in [
        "!/readme{,.*[^~$]}",
        "**/node_modules/**",
        "*.@(pem|key)",
        "lib/*.js",
    ] {
        assert!(
            rule(fine).is_match("deep/er/path/README.md").is_ok(),
            "{fine}"
        );
    }
    // A nasty-but-legal pattern still evaluates correctly.
    let xs = "x".repeat(40);
    assert!(hit("*x*x*x*", &xs));
    assert!(!hit("*x*x*x*y", &xs));
}

#[test]
fn a_closing_run_costs_one_step_per_start() {
    // Only the end of the text counts after the last consuming piece, so a segment without
    // negation is one regex call, and `!(b)c` is a check, its alternative and one run.
    let long = "a".repeat(200);
    let plain = |max_match_steps| {
        Minimatch::new(
            "a*b*c",
            Options {
                max_match_steps,
                ..Options::DEFAULT
            },
        )
        .unwrap()
        .is_match(&long)
    };
    assert_eq!(plain(1), Ok(false));
    assert!(matches!(plain(0), Err(Error::Steps { .. })));
    let negated = Minimatch::new(
        "!(b)c",
        Options {
            nonegate: true,
            max_match_steps: 3,
            ..Options::DEFAULT
        },
    )
    .unwrap();
    assert_eq!(negated.is_match(&format!("{long}c")), Ok(true));
}

#[test]
fn errors_name_the_pattern_first() {
    let error = Error::Steps {
        pattern: "a".into(),
        limit: 5,
    };
    assert_eq!(
        error.to_string(),
        "\"a\": step limit of 5 reached while matching"
    );
    let error = Error::GlobstarRecursion {
        pattern: "a".into(),
        limit: 200,
    };
    assert_eq!(error.to_string(), "\"a\": more than 200 globstar sections");
    let error = Error::Nodes {
        pattern: "a".into(),
        limit: 10_000,
    };
    assert_eq!(error.to_string(), "\"a\": more than 10000 extglob nodes");
    assert!(Minimatch::new(&"a".repeat(65_537), Options::DEFAULT).is_err());
}
