//! minimatch's `escape` and `unescape`: a backslash before every glob magic character, and
//! its removal.

use super::chars::{is_line_terminator, BRACES, GLOB_MAGIC};

/// minimatch's `escape`: a backslash before every glob magic character (`?*()[]\`), and before
/// `{` and `}` when braces are magical.
pub fn escape(s: &str, magical_braces: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let magic = GLOB_MAGIC.contains(c) || (magical_braces && BRACES.contains(c));
        if magic {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// minimatch's `unescape`: `[x]` around one character (not `/` or `\`) becomes `x` unless a
/// backslash precedes it, then a backslash before any character but `/` goes; `{` and `}` keep
/// their escapes when braces are not magical. Ported as the two global replacements it is, so
/// `[a][b]` becomes `a[b]` here too.
pub fn unescape(s: &str, magical_braces: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let allowed = |c: char| c != '/' && c != '\\' && (magical_braces || (c != '{' && c != '}'));
    let mut pass1 = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\'
            && !is_line_terminator(c)
            && chars.get(i + 1) == Some(&'[')
            && chars.get(i + 2).is_some_and(|&x| allowed(x))
            && chars.get(i + 3) == Some(&']')
        {
            pass1.push(c);
            pass1.push(chars[i + 2]);
            i += 4;
            continue;
        }
        if i == 0
            && c == '['
            && chars.get(1).is_some_and(|&x| allowed(x))
            && chars.get(2) == Some(&']')
        {
            pass1.push(chars[1]);
            i += 3;
            continue;
        }
        pass1.push(c);
        i += 1;
    }
    let chars: Vec<char> = pass1.chars().collect();
    let kept = |c: char| c == '/' || (!magical_braces && (c == '{' || c == '}'));
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1).is_some_and(|&x| !kept(x)) {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{escape, unescape};

    #[test]
    fn escape_and_unescape() {
        assert_eq!(
            escape("a*b?[c]\\(d){e}", false),
            "a\\*b\\?\\[c\\]\\\\\\(d\\){e}"
        );
        assert_eq!(escape("{a}", true), "\\{a\\}");
        assert_eq!(unescape("\\*", true), "*");
        assert_eq!(unescape("[*]", true), "*");
        assert_eq!(unescape("a[b]c", true), "abc");
        assert_eq!(unescape("[a][b]", true), "a[b]");
        assert_eq!(unescape("\\[a]", true), "[a]");
        assert_eq!(unescape("a\\/b", true), "a\\/b");
        assert_eq!(unescape("\\{a\\}", true), "{a}");
        assert_eq!(unescape("\\{a\\}", false), "\\{a\\}");
        assert_eq!(unescape("[{]", false), "[{]");
        assert_eq!(unescape("[[a]", true), "[a");
    }
}
