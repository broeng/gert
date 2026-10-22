//! Syntax highlighting for diff sides: pure, no GPUI, no I/O.
//!
//! A [`model::Diff`] holds the whole file (Gerrit ignores `context`), so each
//! side is rebuilt (old = `ab` + `a`, new = `ab` + `b`) and highlighted as one
//! text, which keeps constructs that span lines (block comments, raw strings)
//! right on lines the diff happens to show alone. The result is kept per line
//! number and carries syntect-independent highlight names (`keyword`,
//! `string`, ...) that the painter resolves through the theme's
//! `highlight_theme`, so a light/dark switch needs no re-highlight.
//!
//! The scope to highlight-name mapping ([`semantic_name`]) and the parse loop
//! are adapted from gpui-component's
//! `crates/component/src/highlighter/syntect_backend.rs` (Apache-2.0,
//! Copyright Longbridge); see that file for the original.

use std::{collections::HashMap, ops::Range, sync::Arc, sync::LazyLock};

use model::{Diff, DiffChunk, DiffSide};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// A side above this many bytes of text is not highlighted.
pub const MAX_SIDE_BYTES: usize = 2 * 1024 * 1024;
/// A side above this many lines (skipped lines included) is not highlighted.
pub const MAX_SIDE_LINES: usize = 50_000;
/// A line above this many bytes is left plain and not fed to syntect (the
/// parser keeps the state it had before the line), so one minified line cannot
/// stall a whole file.
pub const MAX_LINE_BYTES: usize = 10_000;

/// Deserializing the syntax set takes tens of milliseconds, so it happens once,
/// on the first highlight, and is shared by everything after that.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// A highlighted byte range of a raw line (no terminator) and its theme name.
pub type Span = (Range<usize>, &'static str);

/// Highlight spans for one side, by line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SideSyntax {
    /// Index 0 is line 1. Spans are sorted, non-overlapping, on char
    /// boundaries, with adjacent same-name spans merged and plain text left
    /// out.
    lines: Vec<Vec<Span>>,
}

impl SideSyntax {
    /// The spans of the 1-based line `number` (`Cell::number`); empty for
    /// plain, skipped or out-of-range lines.
    pub fn line(&self, number: u32) -> &[Span] {
        usize::try_from(number)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| self.lines.get(i))
            .map_or(&[], Vec::as_slice)
    }

    /// How many lines the side has, skipped ones included.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Highlighting for both sides of a diff. A side is `None` when it does not
/// exist or is over the limits. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct DiffSyntax {
    pub old: Option<Arc<SideSyntax>>,
    pub new: Option<Arc<SideSyntax>>,
}

impl DiffSyntax {
    /// The old side is [`DiffSide::Left`], the new side [`DiffSide::Right`].
    pub fn side(&self, side: DiffSide) -> Option<&Arc<SideSyntax>> {
        match side {
            DiffSide::Left => self.old.as_ref(),
            DiffSide::Right => self.new.as_ref(),
        }
    }
}

/// Highlights both sides of `diff` for the file at `path`. `None` when there
/// is nothing to color: binary diffs, Gerrit's magic files, unknown languages
/// or both sides over the limits.
pub fn highlight_diff(path: &str, diff: &Diff) -> Option<DiffSyntax> {
    if diff.binary {
        return None;
    }
    let old = side_lines(diff, DiffSide::Left);
    let new = side_lines(diff, DiffSide::Right);
    let first = new.lines.first().or(old.lines.first()).map(|(_, l)| *l);
    let syntax = detect_syntax(path, first)?;
    let result = DiffSyntax {
        old: highlight_side(syntax, &old).map(Arc::new),
        new: highlight_side(syntax, &new).map(Arc::new),
    };
    (result.old.is_some() || result.new.is_some()).then_some(result)
}

/// The syntax for a file path, falling back to a shebang or other first-line
/// match. `None` for Gerrit's magic files (`/COMMIT_MSG`, `/MERGE_LIST`,
/// `/PATCHSET_LEVEL`), plain text and anything unknown.
pub fn detect_syntax(path: &str, first_line: Option<&str>) -> Option<&'static SyntaxReference> {
    if path.starts_with('/') {
        return None;
    }
    let syntaxes = &*SYNTAXES;
    let name = path.rsplit('/').next().unwrap_or(path);
    // Sublime syntaxes list some whole file names (`Makefile`, `Dockerfile`)
    // among their extensions, so the name is tried before the extension.
    let by_name = syntaxes.find_syntax_by_extension(name);
    let by_ext = name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .and_then(|(_, ext)| syntaxes.find_syntax_by_extension(ext));
    let by_alias = || {
        NAME_ALIASES
            .iter()
            .find(|(file, _)| file.eq_ignore_ascii_case(name))
            .and_then(|(_, token)| syntaxes.find_syntax_by_token(token))
    };
    let by_line = || first_line.and_then(|l| syntaxes.find_syntax_by_first_line(l));
    by_name
        .or(by_ext)
        .or_else(by_alias)
        .or_else(by_line)
        .filter(|s| s.name != "Plain Text")
}

/// File names the syntax set does not know, and the token (extension or
/// syntax name) of the syntax that fits.
const NAME_ALIASES: &[(&str, &str)] = &[
    ("BUILD", "py"),
    ("BUILD.bazel", "py"),
    ("WORKSPACE", "py"),
    ("WORKSPACE.bazel", "py"),
    ("MODULE.bazel", "py"),
    ("SConstruct", "py"),
    ("SConscript", "py"),
    ("Dockerfile", "Dockerfile"),
    ("Containerfile", "Dockerfile"),
    ("CMakeLists.txt", "CMake"),
    ("Makefile", "Makefile"),
    ("GNUmakefile", "Makefile"),
    ("Jenkinsfile", "groovy"),
    ("Gemfile", "rb"),
    ("Rakefile", "rb"),
    ("Vagrantfile", "rb"),
    (".bashrc", "sh"),
    (".bash_profile", "sh"),
    (".bash_aliases", "sh"),
    (".zshrc", "sh"),
    (".profile", "sh"),
    (".gitconfig", "ini"),
    (".editorconfig", "ini"),
];

/// The real lines of one side with their 1-based numbers, and the side's
/// total line count (skipped lines included).
struct SideLines<'a> {
    lines: Vec<(u32, &'a str)>,
    total: usize,
    bytes: usize,
}

fn side_lines(diff: &Diff, side: DiffSide) -> SideLines<'_> {
    let mut out = SideLines {
        lines: Vec::new(),
        total: 0,
        bytes: 0,
    };
    for chunk in &diff.chunks {
        let lines: &[String] = match (chunk, side) {
            (DiffChunk::Common { lines }, _) => lines,
            (DiffChunk::Changed(c), DiffSide::Left) => &c.a,
            (DiffChunk::Changed(c), DiffSide::Right) => &c.b,
            (DiffChunk::Skip { lines }, _) => {
                out.total = out.total.saturating_add(*lines as usize);
                continue;
            }
        };
        for line in lines {
            out.total += 1;
            out.bytes += line.len() + 1;
            out.lines
                .push((u32::try_from(out.total).unwrap_or(u32::MAX), line));
        }
    }
    out
}

/// Highlights one side, or `None` if it is empty or over the limits.
fn highlight_side(syntax: &'static SyntaxReference, side: &SideLines<'_>) -> Option<SideSyntax> {
    if side.lines.is_empty() || side.total > MAX_SIDE_LINES || side.bytes > MAX_SIDE_BYTES {
        return None;
    }
    let syntaxes = &*SYNTAXES;
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut names = HashMap::<Scope, Option<&'static str>>::new();
    let mut result = vec![Vec::new(); side.total];
    let mut buf = String::new();

    for &(number, line) in &side.lines {
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        buf.clear();
        buf.push_str(line);
        buf.push('\n');
        // A line syntect cannot parse keeps the scopes it started with; the
        // next line carries on from them rather than losing the file.
        let Ok(ops) = parser.parse_line(&buf, syntaxes) else {
            continue;
        };
        let mut spans: Vec<Span> = Vec::new();
        let mut push = |range: Range<usize>, scopes: &ScopeStack| {
            let end = range.end.min(line.len());
            if range.start >= end
                || !line.is_char_boundary(range.start)
                || !line.is_char_boundary(end)
            {
                return;
            }
            // The innermost scope that names something wins, so the quotes of
            // a string color as the string rather than as punctuation.
            let name =
                scopes.scopes.iter().rev().find_map(|scope| {
                    *names.entry(*scope).or_insert_with(|| semantic_name(*scope))
                });
            if let Some(name) = name {
                match spans.last_mut() {
                    Some((last, last_name)) if last.end == range.start && *last_name == name => {
                        last.end = end;
                    }
                    _ => spans.push((range.start..end, name)),
                }
            }
        };
        let mut cursor = 0;
        for (index, op) in ops {
            push(cursor..index, &scopes);
            let _ = scopes.apply(&op);
            cursor = index;
        }
        push(cursor..buf.len(), &scopes);
        if let Some(slot) = usize::try_from(number)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| result.get_mut(i))
        {
            *slot = spans;
        }
    }
    Some(SideSyntax { lines: result })
}

/// The theme's highlight name for a TextMate scope, or `None` to let an
/// enclosing scope decide.
fn semantic_name(scope: Scope) -> Option<&'static str> {
    let scope = scope.build_string();
    let is = |prefix: &str| {
        scope == prefix
            || scope
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    };

    let name = if is("punctuation.definition.string")
        || is("punctuation.definition.comment")
        || is("invalid")
        || is("meta")
        || is("source")
        || is("text")
    {
        // Delimiters belong to what they delimit; the rest are containers
        // with no color of their own.
        return None;
    } else if is("comment.block.documentation") || is("comment.line.documentation") {
        "comment.doc"
    } else if is("comment") {
        "comment"
    } else if is("constant.character.escape") {
        "string.escape"
    } else if is("string.regexp") {
        "string.regex"
    } else if is("string") {
        "string"
    } else if is("constant.numeric") {
        "number"
    } else if is("constant.language.boolean") {
        "boolean"
    } else if is("constant") || is("support.constant") || is("entity.name.constant") {
        "constant"
    } else if is("keyword.operator") {
        "operator"
    } else if is("keyword.control.import") || is("meta.preprocessor") {
        "preproc"
    } else if is("keyword") || is("storage.modifier") {
        "keyword"
    } else if is("storage.type") {
        // `fn`, `class` and `let` are declarations, `int` and `bool` are types.
        if [
            "class",
            "enum",
            "function",
            "impl",
            "interface",
            "module",
            "namespace",
            "struct",
            "trait",
            "type",
            "let",
            "var",
            "const",
        ]
        .iter()
        .any(|word| scope.contains(word))
        {
            "keyword"
        } else {
            "type"
        }
    } else if is("entity.name.function")
        || is("support.function")
        || is("variable.function")
        || is("meta.function-call")
    {
        "function"
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
        || is("entity.name.trait")
        || is("entity.name.interface")
        || is("entity.other.inherited-class")
        || is("support.type")
        || is("support.class")
    {
        "type"
    } else if is("entity.name.tag") {
        "tag"
    } else if is("entity.other.attribute-name") {
        "attribute"
    } else if is("entity.name.label") {
        "label"
    } else if is("entity.name.section") || is("markup.heading") {
        "title"
    } else if is("variable.language") {
        "variable.special"
    } else if is("variable.other.member")
        || is("variable.other.property")
        || is("support.variable.property")
        || is("meta.mapping.key")
    {
        "property"
    } else if is("variable") {
        "variable"
    } else if is("markup.bold") {
        "emphasis.strong"
    } else if is("markup.italic") {
        "emphasis"
    } else if is("markup.underline.link") {
        "link_uri"
    } else if is("markup.raw") {
        "text.literal"
    } else if is("markup.list") {
        "punctuation.list_marker"
    } else if is("punctuation.section") || is("punctuation.definition.block") {
        "punctuation.bracket"
    } else if is("punctuation.separator") || is("punctuation.terminator") {
        "punctuation.delimiter"
    } else if is("punctuation") {
        "punctuation"
    } else {
        return None;
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use model::ChangedChunk;

    use super::*;
    use crate::diff::{layout::map_spans, tests::fixture};

    fn lang(path: &str, first: Option<&str>) -> Option<&'static str> {
        detect_syntax(path, first).map(|s| s.name.as_str())
    }

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    fn common(l: &[&str]) -> DiffChunk {
        DiffChunk::Common { lines: lines(l) }
    }

    fn changed(a: &[&str], b: &[&str]) -> DiffChunk {
        DiffChunk::Changed(ChangedChunk {
            a: lines(a),
            b: lines(b),
            ..Default::default()
        })
    }

    fn diff(chunks: Vec<DiffChunk>) -> Diff {
        Diff {
            meta_a: None,
            meta_b: None,
            change_type: None,
            intraline_status: None,
            diff_header: Vec::new(),
            chunks,
            binary: false,
        }
    }

    /// `(name, text)` of the spans of line `n` on the new side.
    fn new_spans(d: &DiffSyntax, text: &[&str], n: u32) -> Vec<(&'static str, String)> {
        let side = d.new.as_ref().expect("new side");
        let line = text[n as usize - 1];
        side.line(n)
            .iter()
            .map(|(r, name)| (*name, line[r.clone()].to_owned()))
            .collect()
    }

    fn highlight(path: &str, text: &[&str]) -> (DiffSyntax, Vec<(&'static str, String)>) {
        let d = highlight_diff(path, &diff(vec![common(text)])).expect("a known language");
        let all = (1..=text.len() as u32)
            .flat_map(|n| new_spans(&d, text, n))
            .collect();
        (d, all)
    }

    fn has(spans: &[(&'static str, String)], name: &str, text: &str) -> bool {
        spans.iter().any(|(n, t)| *n == name && t == text)
    }

    #[test]
    fn languages_are_found_by_extension_and_file_name() {
        assert_eq!(lang("src/main.rs", None), Some("Rust"));
        assert_eq!(lang("a/b/tool.py", None), Some("Python"));
        assert_eq!(lang("x.cc", None), Some("C++"));
        assert_eq!(lang("Foo.java", None), Some("Java"));
        assert_eq!(lang("UPPER.RS", None), Some("Rust"));
        assert_eq!(lang("Makefile", None), Some("Makefile"));
        assert_eq!(lang("sub/Dockerfile", None), Some("Dockerfile"));
        assert_eq!(lang("CMakeLists.txt", None), Some("CMake"));
        for name in [
            "BUILD",
            "BUILD.bazel",
            "WORKSPACE",
            "x/.bashrc",
            "Gemfile",
            "Jenkinsfile",
        ] {
            assert!(lang(name, None).is_some(), "{name}");
        }
        assert_eq!(lang("BUILD.bazel", None), Some("Python"));
    }

    #[test]
    fn a_shebang_decides_when_the_name_does_not() {
        assert_eq!(
            lang("bin/run", Some("#!/usr/bin/env python3")),
            Some("Python")
        );
        assert_eq!(
            lang("bin/run", Some("#!/bin/bash")),
            Some("Bourne Again Shell (bash)")
        );
        assert_eq!(lang("bin/run", Some("just text")), None);
        assert_eq!(lang("bin/run", None), None);
    }

    #[test]
    fn magic_and_unknown_files_are_not_highlighted() {
        for path in ["/COMMIT_MSG", "/MERGE_LIST", "/PATCHSET_LEVEL"] {
            assert_eq!(lang(path, None), None);
            assert!(highlight_diff(path, &diff(vec![common(&["fn main() {}"])])).is_none());
        }
        assert_eq!(lang("notes.unknownext", None), None);
        assert_eq!(lang("LICENSE", None), None);
        assert!(highlight_diff("a.unknownext", &diff(vec![common(&["x"])])).is_none());
        let mut d = diff(vec![common(&["fn main() {}"])]);
        d.binary = true;
        assert!(highlight_diff("a.rs", &d).is_none());
    }

    #[test]
    fn rust_tokens_get_theme_names() {
        let (_, s) = highlight(
            "a.rs",
            &[
                "fn main() {",
                "    let s = \"hi\"; // note",
                "    let n = 42;",
                "}",
            ],
        );
        assert!(has(&s, "keyword", "fn"), "{s:?}");
        assert!(has(&s, "function", "main"), "{s:?}");
        assert!(has(&s, "string", "\"hi\""), "{s:?}");
        assert!(has(&s, "comment", "// note"), "{s:?}");
        assert!(has(&s, "number", "42"), "{s:?}");
    }

    #[test]
    fn python_cpp_and_java_tokens_get_theme_names() {
        let (_, s) = highlight("a.py", &["def f(x):", "    return \"s\"  # c"]);
        assert!(has(&s, "keyword", "def"), "{s:?}");
        assert!(has(&s, "string", "\"s\""), "{s:?}");
        assert!(has(&s, "comment", "# c"), "{s:?}");

        let (_, s) = highlight(
            "a.cpp",
            &["#include <vector>", "int main() { return 0; } // x"],
        );
        assert!(s.iter().any(|(n, _)| *n == "preproc"), "{s:?}");
        assert!(has(&s, "keyword", "return"), "{s:?}");
        assert!(has(&s, "number", "0"), "{s:?}");
        assert!(has(&s, "comment", "// x"), "{s:?}");

        let (_, s) = highlight(
            "A.java",
            &["public class A {", "  String s = \"x\"; /* c */", "}"],
        );
        assert!(has(&s, "keyword", "class"), "{s:?}");
        assert!(has(&s, "string", "\"x\""), "{s:?}");
        assert!(has(&s, "comment", "/* c */"), "{s:?}");
    }

    #[test]
    fn a_block_comment_colors_its_middle_lines() {
        // The reason for highlighting whole sides: line 2 alone is not
        // recognizably a comment.
        let text = ["/* start", "   middle words", "   end */ int x;"];
        let (_, s) = highlight("a.c", &text);
        assert!(has(&s, "comment", "   middle words"), "{s:?}");
        // The side was parsed as a whole, so a diff showing only line 2 as an
        // addition still gets the color.
        let d = highlight_diff(
            "a.c",
            &diff(vec![
                common(&[text[0]]),
                changed(&[], &[text[1]]),
                common(&[text[2]]),
            ]),
        )
        .expect("c");
        assert!(has(&new_spans(&d, &text, 2), "comment", "   middle words"));
    }

    #[test]
    fn sides_are_rebuilt_from_ab_a_and_b() {
        let d = diff(vec![
            common(&["/* c"]),
            changed(&["old */ int a;"], &["new */ int b;"]),
            common(&["int z;"]),
        ]);
        let s = highlight_diff("a.c", &d).expect("c");
        let (old, new) = (s.old.as_ref().expect("old"), s.new.as_ref().expect("new"));
        assert_eq!((old.len(), new.len()), (3, 3));
        assert_eq!(s.side(DiffSide::Left).map(|x| x.len()), Some(3));
        // Line 2 of both sides closes the comment from line 1.
        assert_eq!(old.line(2)[0], (0..6, "comment"));
        assert_eq!(new.line(2)[0], (0..6, "comment"));
        assert!(
            old.line(2)
                .iter()
                .any(|(r, n)| r.start == 7 && *n != "comment")
        );
        assert!(
            new.line(2)
                .iter()
                .any(|(r, n)| r.start == 7 && *n != "comment")
        );
        // Out of range lookups are empty.
        assert!(new.line(0).is_empty() && new.line(99).is_empty());
    }

    #[test]
    fn an_added_file_has_only_a_new_side() {
        let d = diff(vec![changed(&[], &["fn a() {}"])]);
        let s = highlight_diff("a.rs", &d).expect("rs");
        assert!(s.old.is_none() && s.new.is_some());
    }

    #[test]
    fn skipped_lines_keep_the_numbering() {
        let d = diff(vec![
            common(&["/* open"]),
            DiffChunk::Skip { lines: 3 },
            common(&["close */ fn x() {}", "// tail"]),
        ]);
        let s = highlight_diff("a.c", &d).expect("c");
        let new = s.new.expect("new");
        assert_eq!(new.len(), 6);
        assert!(new.line(1).iter().any(|(_, n)| *n == "comment"));
        assert!(new.line(2).is_empty() && new.line(4).is_empty());
        // Parsing runs across the gap: line 5 still ends the comment.
        assert_eq!(new.line(5)[0], (0..8, "comment"));
        assert_eq!(new.line(6), [(0..7, "comment")]);
    }

    #[test]
    fn spans_are_on_char_boundaries_and_merged() {
        let text = ["let s = \"na\u{ef}ve caf\u{e9} \u{1f600}\"; // \u{e5}\u{e4}\u{f6}"];
        let (d, all) = highlight("a.rs", &text);
        assert!(
            has(&all, "string", "\"na\u{ef}ve caf\u{e9} \u{1f600}\""),
            "{all:?}"
        );
        for (r, _) in d.new.as_ref().expect("new").line(1) {
            assert!(text[0].is_char_boundary(r.start) && text[0].is_char_boundary(r.end));
        }
        let spans = d.new.as_ref().expect("new").line(1);
        for w in spans.windows(2) {
            assert!(w[0].0.end <= w[1].0.start);
            assert!(
                w[0].0.end != w[1].0.start || w[0].1 != w[1].1,
                "unmerged {spans:?}"
            );
        }
    }

    #[test]
    fn spans_map_onto_tab_expanded_text() {
        let line = "\tlet s = \"x\";";
        let (d, _) = highlight("a.rs", &[line]);
        let spans = d.new.as_ref().expect("new").line(1);
        let mapped = map_spans(line, spans, 4, false);
        let shown = "    let s = \"x\";";
        let got: Vec<_> = mapped
            .iter()
            .map(|(r, n)| (*n, &shown[r.clone()]))
            .collect();
        assert!(got.contains(&("keyword", "let")), "{got:?}");
        assert!(got.contains(&("string", "\"x\"")), "{got:?}");
    }

    #[test]
    fn limits_and_long_lines_degrade_to_plain() {
        let long = format!("let s = \"{}\";", "x".repeat(MAX_LINE_BYTES));
        let d = highlight_diff("a.rs", &diff(vec![common(&[&long, "fn f() {}"])])).expect("rs");
        let new = d.new.expect("new");
        assert!(new.line(1).is_empty());
        assert!(!new.line(2).is_empty());

        let many: Vec<String> = (0..MAX_SIDE_LINES + 1).map(|_| String::new()).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(highlight_diff("a.rs", &diff(vec![common(&refs)])).is_none());
        let big = "x".repeat(MAX_SIDE_BYTES + 1);
        assert!(highlight_diff("a.rs", &diff(vec![common(&[&big])])).is_none());
        // One side over the limit leaves the other.
        let d = highlight_diff("a.rs", &diff(vec![changed(&[&big], &["fn f() {}"])])).expect("rs");
        assert!(d.old.is_none() && d.new.is_some());
    }

    #[test]
    fn odd_input_does_not_panic() {
        for path in [
            "a.rs", "a.py", "a.c", "a.html", "a.json", "a.md", "a.sh", "a.js",
        ] {
            let d = diff(vec![
                common(&[
                    "\"unterminated",
                    "'",
                    "/*",
                    "<!--",
                    "\u{0}\u{1}\u{fffd}",
                    "```",
                    "\\",
                ]),
                DiffChunk::Skip { lines: 2 },
                changed(&["\t\t"], &["\u{200d}\u{301}e\u{301}"]),
            ]);
            let _ = highlight_diff(path, &d);
        }
        let _ = highlight_diff("a.rs", &diff(Vec::new()));
    }

    #[test]
    #[ignore = "timing report; run with --ignored --nocapture"]
    fn timing_on_the_large_fixture() {
        let d = fixture("large_diff.response.json");
        let t = Instant::now();
        let warm = highlight_diff("big.rs", &diff(vec![common(&["fn a() {}"])]));
        eprintln!("syntax set load + tiny diff: {:?}", t.elapsed());
        assert!(warm.is_some());
        let t = Instant::now();
        let s = highlight_diff("big.rs", &d).expect("rs");
        let took = t.elapsed();
        let (o, n) = (s.old.map_or(0, |s| s.len()), s.new.map_or(0, |s| s.len()));
        eprintln!(
            "large diff highlight (old {o} lines, new {n} lines): {took:?} ({} build)",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
        assert!(n > 1_000);
    }
}
