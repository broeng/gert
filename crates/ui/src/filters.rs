//! File filter rules: which files of a change are hidden, dimmed or
//! highlighted. Pure data and matching; the pages only draw the result.
//!
//! Globs are gitignore-style: a pattern without `/` matches the file name at
//! any depth, a pattern with `/` is anchored at the repository root (a
//! leading `/` is dropped), `*` does not cross `/` and `**` does.

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use gpui_kit::{Rgba, rgb};
use model::{COMMIT_MSG_PATH, MERGE_LIST_PATH};

/// What a matching rule does to a file.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterAction {
    Hide,
    Dim,
    Highlight { color: Option<Rgba>, bold: bool },
}

/// One user-made rule.
#[derive(Clone, Debug, PartialEq)]
pub struct FileFilter {
    /// Unique; shown on the toggle button.
    pub name: String,
    pub globs: Vec<String>,
    /// Exact project names; empty means every project.
    pub projects: Vec<String>,
    pub action: FilterAction,
    /// Whether the rule is on at the start of a session.
    pub enabled: bool,
}

/// What the rules together do to one file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileStyle {
    pub hidden: bool,
    /// Never set together with `hidden`.
    pub dimmed: bool,
    /// Only for files that are neither hidden nor dimmed.
    pub color: Option<Rgba>,
    pub bold: bool,
    /// Names of the matching rules, in rule order.
    pub rules: Vec<String>,
}

/// Whether file navigation (`]` / `[`) passes over the file.
pub fn skips(style: &FileStyle) -> bool {
    style.hidden || style.dimmed
}

/// The nearest index strictly after (`forward`) or before `from` in a list
/// of `len` entries for which `skipped` is false. No wrap-around.
pub fn next_unskipped(
    len: usize,
    from: usize,
    forward: bool,
    skipped: impl Fn(usize) -> bool,
) -> Option<usize> {
    if forward {
        (from.saturating_add(1)..len).find(|&i| !skipped(i))
    } else {
        (0..from.min(len)).rev().find(|&i| !skipped(i))
    }
}

/// Whether the rule is scoped to `project` (or to every project).
pub fn applies_to(rule: &FileFilter, project: &str) -> bool {
    rule.projects.is_empty() || rule.projects.iter().any(|p| p == project)
}

/// `#rrggbb` (case-insensitive) as a color.
pub fn parse_hex_color(text: &str) -> Option<Rgba> {
    let hex = text.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(rgb)
}

/// The color as lowercase `#rrggbb` (alpha is ignored).
pub fn format_hex_color(color: Rgba) -> String {
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        byte(color.r),
        byte(color.g),
        byte(color.b)
    )
}

/// The glob as gitignore would read it, or `None` for a blank one.
fn build_glob(pattern: &str) -> Option<Result<Glob, globset::Error>> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return None;
    }
    let full = if pattern.contains('/') {
        pattern.trim_start_matches('/').to_owned()
    } else {
        format!("**/{pattern}")
    };
    Some(GlobBuilder::new(&full).literal_separator(true).build())
}

struct CompiledRule {
    name: String,
    set: GlobSet,
    action: FilterAction,
}

/// The active rules of one project, ready to style paths.
#[derive(Default)]
pub struct FileMatcher {
    rules: Vec<CompiledRule>,
}

/// Compiles the rules that apply to `project` and are `active`. A glob that
/// does not compile is skipped with a warning.
pub fn compile(
    rules: &[FileFilter],
    project: &str,
    active: impl Fn(&FileFilter) -> bool,
) -> FileMatcher {
    let mut out = Vec::new();
    for rule in rules {
        if !applies_to(rule, project) || !active(rule) {
            continue;
        }
        let mut builder = GlobSetBuilder::new();
        let mut any = false;
        for pattern in &rule.globs {
            match build_glob(pattern) {
                None => {}
                Some(Ok(glob)) => {
                    builder.add(glob);
                    any = true;
                }
                Some(Err(e)) => {
                    log::warn!(
                        "file filter {:?}: skipping glob {pattern:?}: {e}",
                        rule.name
                    )
                }
            }
        }
        if !any {
            continue;
        }
        match builder.build() {
            Ok(set) => out.push(CompiledRule {
                name: rule.name.clone(),
                set,
                action: rule.action.clone(),
            }),
            Err(e) => log::warn!("file filter {:?}: {e}", rule.name),
        }
    }
    FileMatcher { rules: out }
}

impl FileMatcher {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Hide beats dim, and dim beats highlight. Highlights merge: the later
    /// rule's color wins and bold is OR'ed. The commit message and the merge
    /// list are never matched.
    pub fn style(&self, path: &str) -> FileStyle {
        let mut style = FileStyle::default();
        if path == COMMIT_MSG_PATH || path == MERGE_LIST_PATH {
            return style;
        }
        for rule in &self.rules {
            if !rule.set.is_match(path) {
                continue;
            }
            style.rules.push(rule.name.clone());
            match &rule.action {
                FilterAction::Hide => style.hidden = true,
                FilterAction::Dim => style.dimmed = true,
                FilterAction::Highlight { color, bold } => {
                    if color.is_some() {
                        style.color = *color;
                    }
                    style.bold |= bold;
                }
            }
        }
        if style.hidden {
            style.dimmed = false;
        }
        if skips(&style) {
            style.color = None;
            style.bold = false;
        }
        style
    }
}

/// Whether one glob compiles. A blank one is fine: it is ignored.
pub fn validate_glob(pattern: &str) -> Result<(), String> {
    match build_glob(pattern) {
        Some(Err(e)) => Err(e.kind().to_string()),
        _ => Ok(()),
    }
}

/// Checks a rule before it is saved: `others` are the other rules of the
/// list (without this one).
pub fn validate(rule: &FileFilter, others: &[FileFilter]) -> Result<(), String> {
    let name = rule.name.trim();
    if name.is_empty() {
        return Err("The name is empty.".into());
    }
    if others.iter().any(|o| o.name == rule.name) {
        return Err(format!("There is already a filter named {name:?}."));
    }
    for pattern in &rule.globs {
        validate_glob(pattern).map_err(|e| format!("Invalid glob {:?}: {e}", pattern.trim()))?;
    }
    if rule.globs.iter().all(|g| g.trim().is_empty()) {
        return Err("Add at least one glob.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, globs: &[&str], action: FilterAction) -> FileFilter {
        FileFilter {
            name: name.into(),
            globs: globs.iter().map(|g| (*g).into()).collect(),
            projects: Vec::new(),
            action,
            enabled: true,
        }
    }

    fn hl(color: Option<u32>, bold: bool) -> FilterAction {
        FilterAction::Highlight {
            color: color.map(rgb),
            bold,
        }
    }

    fn matcher(rules: &[FileFilter]) -> FileMatcher {
        compile(rules, "proj", |r| r.enabled)
    }

    fn names(m: &FileMatcher, path: &str) -> Vec<String> {
        m.style(path).rules
    }

    #[test]
    fn a_pattern_without_slash_matches_the_name_at_any_depth() {
        let m = matcher(&[rule("pb", &["*.pb.go"], FilterAction::Hide)]);
        assert!(m.style("a.pb.go").hidden);
        assert!(m.style("x/y/a.pb.go").hidden);
        assert!(!m.style("x/a.pb.gox").hidden);
        assert!(!m.style("x/pb.go").hidden);
    }

    #[test]
    fn a_literal_name_matches_at_any_depth_but_not_as_a_directory_prefix() {
        let m = matcher(&[rule("lock", &["Cargo.lock"], FilterAction::Dim)]);
        assert!(m.style("Cargo.lock").dimmed);
        assert!(m.style("sub/Cargo.lock").dimmed);
        assert!(!m.style("Cargo.lock.bak").dimmed);
    }

    #[test]
    fn a_pattern_with_slash_is_anchored_at_the_root() {
        let m = matcher(&[rule("gen", &["gen/*.go"], FilterAction::Hide)]);
        assert!(m.style("gen/a.go").hidden);
        assert!(!m.style("x/gen/a.go").hidden);
    }

    #[test]
    fn star_does_not_cross_a_slash_but_double_star_does() {
        let m = matcher(&[
            rule("one", &["a/*.txt"], FilterAction::Hide),
            rule("deep", &["b/**"], FilterAction::Dim),
            rule("mid", &["c/**/z.txt"], hl(None, true)),
        ]);
        assert!(m.style("a/x.txt").hidden);
        assert!(!m.style("a/x/y.txt").hidden);
        assert!(m.style("b/x/y/z").dimmed);
        assert!(!m.style("b").dimmed);
        assert!(m.style("c/z.txt").bold);
        assert!(m.style("c/p/q/z.txt").bold);
    }

    #[test]
    fn a_leading_slash_is_dropped() {
        let m = matcher(&[rule("root", &["/vendor/**"], FilterAction::Hide)]);
        assert!(m.style("vendor/a/b.rs").hidden);
        assert!(!m.style("x/vendor/a.rs").hidden);
        let m = matcher(&[rule("root", &["/Makefile"], FilterAction::Hide)]);
        assert!(m.style("Makefile").hidden);
        assert!(!m.style("sub/Makefile").hidden);
    }

    #[test]
    fn projects_scope_the_rules() {
        let mut scoped = rule("only", &["*.rs"], FilterAction::Hide);
        scoped.projects = vec!["a/b".into()];
        assert!(applies_to(&scoped, "a/b"));
        assert!(!applies_to(&scoped, "a/bc"));
        assert!(!applies_to(&scoped, "a"));
        let everywhere = rule("all", &["*.rs"], FilterAction::Hide);
        assert!(applies_to(&everywhere, "anything"));

        let rules = [scoped];
        assert!(compile(&rules, "a/b", |_| true).style("x.rs").hidden);
        assert!(compile(&rules, "other", |_| true).is_empty());
    }

    #[test]
    fn inactive_rules_are_not_compiled() {
        let rules = [
            rule("a", &["*.rs"], FilterAction::Hide),
            rule("b", &["*.md"], FilterAction::Hide),
        ];
        let m = compile(&rules, "p", |r| r.name == "b");
        assert!(!m.style("x.rs").hidden);
        assert!(m.style("x.md").hidden);
        assert!(compile(&rules, "p", |_| false).is_empty());
    }

    #[test]
    fn hide_beats_dim_beats_highlight() {
        let m = matcher(&[
            rule("hl", &["*.rs"], hl(Some(0xff0000), true)),
            rule("dim", &["*.rs"], FilterAction::Dim),
        ]);
        let s = m.style("a.rs");
        assert!(s.dimmed && !s.hidden);
        assert_eq!((s.color, s.bold), (None, false));
        assert_eq!(s.rules, ["hl", "dim"]);

        let m = matcher(&[
            rule("dim", &["*.rs"], FilterAction::Dim),
            rule("hide", &["a.*"], FilterAction::Hide),
            rule("hl", &["*.rs"], hl(Some(0xff0000), true)),
        ]);
        let s = m.style("a.rs");
        assert!(s.hidden && !s.dimmed);
        assert_eq!((s.color, s.bold), (None, false));
        assert!(skips(&s));
    }

    #[test]
    fn highlights_merge_with_the_later_color_winning_and_bold_or_ed() {
        let m = matcher(&[
            rule("red", &["*.rs"], hl(Some(0xff0000), true)),
            rule("plain", &["*.rs"], hl(None, false)),
            rule("blue", &["a.*"], hl(Some(0x0000ff), false)),
        ]);
        let s = m.style("a.rs");
        assert_eq!(s.color, Some(rgb(0x0000ff)));
        assert!(s.bold);
        assert!(!skips(&s));
        // A later rule without a color keeps the earlier color.
        let s = m.style("b.rs");
        assert_eq!(s.color, Some(rgb(0xff0000)));
        assert_eq!(s.rules, ["red", "plain"]);
        assert_eq!(names(&m, "b.txt"), Vec::<String>::new());
    }

    #[test]
    fn the_commit_message_and_merge_list_never_match() {
        let m = matcher(&[rule("all", &["*", "**"], FilterAction::Hide)]);
        assert!(!m.style(COMMIT_MSG_PATH).hidden);
        assert!(!m.style(MERGE_LIST_PATH).hidden);
        assert!(m.style("a.rs").hidden);
    }

    #[test]
    fn an_invalid_glob_is_skipped_and_the_rest_still_match() {
        let m = matcher(&[rule("r", &["[", "*.rs"], FilterAction::Hide)]);
        assert!(m.style("a.rs").hidden);
        // A rule with no usable glob is dropped.
        assert!(matcher(&[rule("r", &["[", "  "], FilterAction::Hide)]).is_empty());
    }

    #[test]
    fn next_unskipped_stops_at_the_ends() {
        let skipped = [true, false, true, true, false, true];
        let skip = |i: usize| skipped[i];
        assert_eq!(next_unskipped(6, 0, true, skip), Some(1));
        assert_eq!(next_unskipped(6, 1, true, skip), Some(4));
        assert_eq!(next_unskipped(6, 4, true, skip), None);
        assert_eq!(next_unskipped(6, 5, true, skip), None);
        assert_eq!(next_unskipped(6, 5, false, skip), Some(4));
        assert_eq!(next_unskipped(6, 4, false, skip), Some(1));
        assert_eq!(next_unskipped(6, 1, false, skip), None);
        assert_eq!(next_unskipped(6, 0, false, skip), None);
        assert_eq!(next_unskipped(0, 0, true, |_| false), None);
        assert_eq!(next_unskipped(0, 0, false, |_| false), None);
    }

    #[test]
    fn validation_checks_name_globs_and_uniqueness() {
        let ok = rule("Generated", &["*.pb.go"], FilterAction::Hide);
        assert_eq!(validate(&ok, &[]), Ok(()));
        let other = rule("Other", &["x"], FilterAction::Dim);
        assert_eq!(validate(&ok, std::slice::from_ref(&other)), Ok(()));

        let mut blank = ok.clone();
        blank.name = "  ".into();
        assert!(validate(&blank, &[]).is_err());

        let dup = rule("Generated", &["y"], FilterAction::Dim);
        assert!(validate(&ok, &[dup]).is_err());
        // Names are case-sensitive.
        let lower = rule("generated", &["y"], FilterAction::Dim);
        assert_eq!(validate(&ok, &[lower]), Ok(()));

        let none = rule("n", &[], FilterAction::Hide);
        assert!(validate(&none, &[]).is_err());
        let blank_globs = rule("n", &["", "   "], FilterAction::Hide);
        assert!(validate(&blank_globs, &[]).is_err());
        let bad = rule("n", &["*.rs", "["], FilterAction::Hide);
        assert!(validate(&bad, &[]).is_err());
    }

    #[test]
    fn hex_colors_parse_and_format() {
        assert_eq!(parse_hex_color("#e5a50a"), Some(rgb(0xe5a50a)));
        assert_eq!(parse_hex_color(" #E5A50A "), Some(rgb(0xe5a50a)));
        assert_eq!(parse_hex_color("e5a50a"), None);
        assert_eq!(parse_hex_color("#e5a50"), None);
        assert_eq!(parse_hex_color("#e5a50a0"), None);
        assert_eq!(parse_hex_color("#gggggg"), None);
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(format_hex_color(rgb(0xe5a50a)), "#e5a50a");
        assert_eq!(format_hex_color(rgb(0x000000)), "#000000");
        assert_eq!(format_hex_color(rgb(0xffffff)), "#ffffff");
        for v in [0x010203, 0x7f8081, 0xfeff00] {
            let hex = format!("#{v:06x}");
            assert_eq!(parse_hex_color(&hex).map(format_hex_color), Some(hex));
        }
    }
}
