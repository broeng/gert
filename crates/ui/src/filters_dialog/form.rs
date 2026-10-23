//! The pure part of the Manage filters dialog: what the form's text and
//! buttons make of a rule, and what it says about it.

use gpui_kit::Rgba;
use model::{COMMIT_MSG_PATH, MERGE_LIST_PATH};

use crate::{
    filters::{FileFilter, FilterAction, applies_to, compile, format_hex_color, validate},
    session::ServerKey,
};

/// The three effects, as the form's choice (the highlight's color and bold
/// are separate controls).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Hide,
    Dim,
    Highlight,
}

impl Kind {
    pub(crate) const ALL: [Kind; 3] = [Kind::Hide, Kind::Dim, Kind::Highlight];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Kind::Hide => "Hide",
            Kind::Dim => "Dim",
            Kind::Highlight => "Highlight",
        }
    }

    pub(crate) fn of(action: &FilterAction) -> Kind {
        match action {
            FilterAction::Hide => Kind::Hide,
            FilterAction::Dim => Kind::Dim,
            FilterAction::Highlight { .. } => Kind::Highlight,
        }
    }

    /// The next (`delta` 1) or previous (-1) choice, without wrapping.
    pub(crate) fn step(self, delta: isize) -> Kind {
        let i = Kind::ALL.iter().position(|k| *k == self).unwrap_or(0);
        let j = i
            .saturating_add_signed(delta)
            .min(Kind::ALL.len().saturating_sub(1));
        Kind::ALL[j]
    }
}

/// What the form holds. The color and bold are kept while another effect is
/// chosen, so switching back does not lose them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Form {
    pub name: String,
    /// One per line.
    pub globs: String,
    /// One per line.
    pub projects: String,
    /// The servers the rule is limited to; empty: all of them.
    pub servers: Vec<ServerKey>,
    pub kind: Kind,
    pub color: Option<Rgba>,
    pub bold: bool,
    pub enabled: bool,
}

impl Form {
    pub(crate) fn from_filter(rule: &FileFilter) -> Form {
        let (color, bold) = match &rule.action {
            FilterAction::Highlight { color, bold } => (*color, *bold),
            _ => (None, false),
        };
        Form {
            name: rule.name.clone(),
            globs: join_lines(&rule.globs),
            projects: join_lines(&rule.projects),
            servers: rule.servers.clone(),
            kind: Kind::of(&rule.action),
            color,
            bold,
            enabled: rule.enabled,
        }
    }

    /// The rule the form stands for: trimmed, with no blank lines.
    pub(crate) fn build(&self) -> FileFilter {
        let action = match self.kind {
            Kind::Hide => FilterAction::Hide,
            Kind::Dim => FilterAction::Dim,
            Kind::Highlight => FilterAction::Highlight {
                color: self.color,
                bold: self.bold,
            },
        };
        FileFilter {
            name: self.name.trim().to_owned(),
            globs: split_lines(&self.globs),
            projects: split_lines(&self.projects),
            servers: self.servers.clone(),
            action,
            enabled: self.enabled,
        }
    }
}

/// The non-blank lines of `text`, trimmed.
pub(crate) fn split_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn join_lines(items: &[String]) -> String {
    items.join("\n")
}

/// The starting point of a rule made with `a`: scoped to the project of the
/// change the dialog was opened from, if any.
pub(crate) fn new_rule(project: Option<&str>) -> FileFilter {
    FileFilter {
        name: String::new(),
        globs: Vec::new(),
        projects: project.map(str::to_owned).into_iter().collect(),
        servers: Vec::new(),
        action: FilterAction::Hide,
        enabled: true,
    }
}

/// Why `rule` cannot be saved in place of the rule at `index` of `rules`
/// (`index` past the end: as a new one).
pub(crate) fn problem(rule: &FileFilter, rules: &[FileFilter], index: usize) -> Option<String> {
    let others: Vec<FileFilter> = rules
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != index)
        .map(|(_, r)| r.clone())
        .collect();
    validate(rule, &others).err()
}

/// A short account of what the rule does, for the list.
pub(crate) fn summary(action: &FilterAction) -> String {
    match action {
        FilterAction::Hide => "hide".to_owned(),
        FilterAction::Dim => "dim".to_owned(),
        FilterAction::Highlight { color, bold } => {
            let mut s = String::from("highlight");
            if let Some(c) = color {
                s.push(' ');
                s.push_str(&format_hex_color(*c));
            }
            if *bold {
                s.push_str(" bold");
            }
            s
        }
    }
}

/// How many of a change's files the rule matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Preview {
    /// The rule is scoped to other projects.
    OtherProject,
    /// The rule is for other servers.
    OtherServer,
    Matches {
        matched: usize,
        total: usize,
    },
}

impl Preview {
    pub(crate) fn text(&self) -> String {
        match self {
            Preview::OtherProject => "This rule does not apply to this change's project".to_owned(),
            Preview::OtherServer => "This rule does not apply to this change's server".to_owned(),
            Preview::Matches { matched, total } => {
                let files = if *total == 1 { "file" } else { "files" };
                format!("Matches {matched} of {total} {files} in this change")
            }
        }
    }
}

/// What `rule` would match among `paths` (the files of a change of
/// `project` on `server`). The commit message and the merge list are not
/// counted.
pub(crate) fn preview(
    rule: &FileFilter,
    server: &ServerKey,
    project: &str,
    paths: &[String],
) -> Preview {
    if !rule.servers.is_empty() && !rule.servers.contains(server) {
        return Preview::OtherServer;
    }
    if !applies_to(rule, server, project) {
        return Preview::OtherProject;
    }
    let matcher = compile(std::slice::from_ref(rule), server, project, |_| true);
    let files = || {
        paths
            .iter()
            .filter(|p| *p != COMMIT_MSG_PATH && *p != MERGE_LIST_PATH)
    };
    Preview::Matches {
        matched: files()
            .filter(|p| !matcher.style(p).rules.is_empty())
            .count(),
        total: files().count(),
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::rgb;

    use super::*;

    fn rule(name: &str, globs: &[&str], action: FilterAction) -> FileFilter {
        FileFilter {
            name: name.into(),
            globs: globs.iter().map(|g| (*g).into()).collect(),
            projects: Vec::new(),
            servers: Vec::new(),
            action,
            enabled: true,
        }
    }

    #[test]
    fn lines_are_trimmed_and_blank_ones_dropped() {
        assert_eq!(split_lines("  a \n\n b\r\n   \nc"), ["a", "b", "c"]);
        assert!(split_lines("").is_empty());
        assert!(split_lines(" \n ").is_empty());
        assert_eq!(join_lines(&["a".into(), "b".into()]), "a\nb");
    }

    #[test]
    fn a_form_round_trips_through_a_rule() {
        let r = FileFilter {
            projects: vec!["p".into()],
            servers: vec![ServerKey::new("s")],
            enabled: false,
            ..rule(
                "Gen",
                &["*.pb.go", "gen/**"],
                FilterAction::Highlight {
                    color: Some(rgb(0xe5a50a)),
                    bold: true,
                },
            )
        };
        assert_eq!(Form::from_filter(&r).build(), r);
    }

    #[test]
    fn building_trims_and_the_effect_picks_what_counts() {
        let mut f = Form::from_filter(&rule(
            "x",
            &["a"],
            FilterAction::Highlight {
                color: Some(rgb(0xff0000)),
                bold: true,
            },
        ));
        f.name = "  Name ".into();
        f.globs = "\n *.lock \n".into();
        assert_eq!(f.build().name, "Name");
        assert_eq!(f.build().globs, ["*.lock"]);
        // The color and bold stay in the form but do nothing for a dim.
        f.kind = Kind::Dim;
        assert_eq!(f.build().action, FilterAction::Dim);
        f.kind = Kind::Highlight;
        assert_eq!(
            f.build().action,
            FilterAction::Highlight {
                color: Some(rgb(0xff0000)),
                bold: true
            }
        );
    }

    #[test]
    fn the_effect_steps_without_wrapping() {
        assert_eq!(Kind::Hide.step(1), Kind::Dim);
        assert_eq!(Kind::Dim.step(1), Kind::Highlight);
        assert_eq!(Kind::Highlight.step(1), Kind::Highlight);
        assert_eq!(Kind::Hide.step(-1), Kind::Hide);
        assert_eq!(Kind::Highlight.step(-1), Kind::Dim);
    }

    #[test]
    fn a_new_rule_is_scoped_to_the_given_project() {
        assert_eq!(new_rule(Some("p")).projects, ["p"]);
        assert!(new_rule(None).projects.is_empty());
        assert!(new_rule(None).enabled);
    }

    #[test]
    fn problems_ignore_the_rule_being_replaced() {
        let rules = vec![
            rule("a", &["*.a"], FilterAction::Hide),
            rule("b", &["*.b"], FilterAction::Hide),
        ];
        // Saving "a" over itself is fine; over "b" it clashes.
        assert_eq!(problem(&rules[0], &rules, 0), None);
        assert!(problem(&rules[0], &rules, 1).is_some());
        // A new rule with a taken name clashes.
        assert!(problem(&rules[1], &rules, 2).is_some());
        assert!(problem(&rule("", &["x"], FilterAction::Hide), &rules, 2).is_some());
        assert!(problem(&rule("c", &["["], FilterAction::Hide), &rules, 2).is_some());
        assert!(problem(&rule("c", &[], FilterAction::Hide), &rules, 2).is_some());
    }

    #[test]
    fn the_summary_names_the_effect() {
        assert_eq!(summary(&FilterAction::Hide), "hide");
        assert_eq!(summary(&FilterAction::Dim), "dim");
        assert_eq!(
            summary(&FilterAction::Highlight {
                color: Some(rgb(0xe5a50a)),
                bold: true
            }),
            "highlight #e5a50a bold"
        );
        assert_eq!(
            summary(&FilterAction::Highlight {
                color: None,
                bold: true
            }),
            "highlight bold"
        );
    }

    #[test]
    fn the_preview_counts_matches_among_the_files() {
        let paths: Vec<String> = ["/COMMIT_MSG", "a.pb.go", "src/b.pb.go", "src/c.rs"]
            .iter()
            .map(|p| (*p).to_owned())
            .collect();
        let r = rule("pb", &["*.pb.go"], FilterAction::Hide);
        assert_eq!(
            preview(&r, &ServerKey::new("s"), "p", &paths),
            Preview::Matches {
                matched: 2,
                total: 3
            }
        );
        assert_eq!(
            preview(&r, &ServerKey::new("s"), "p", &paths).text(),
            "Matches 2 of 3 files in this change"
        );
        let s = ServerKey::new("s");
        let scoped = FileFilter {
            projects: vec!["other".into()],
            ..r.clone()
        };
        assert_eq!(preview(&scoped, &s, "p", &paths), Preview::OtherProject);
        let elsewhere = FileFilter {
            servers: vec![ServerKey::new("t")],
            ..r.clone()
        };
        assert_eq!(preview(&elsewhere, &s, "p", &paths), Preview::OtherServer);
        let here = FileFilter {
            servers: vec![s.clone(), ServerKey::new("t")],
            ..r
        };
        assert!(matches!(
            preview(&here, &s, "p", &paths),
            Preview::Matches { .. }
        ));
    }
}
