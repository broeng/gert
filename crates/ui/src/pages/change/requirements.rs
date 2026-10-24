//! The submit requirements and the votes in the change page's first column.
//!
//! A requirement is one line: a round mark for its state, its name, then the
//! votes on the labels it asks about as small colored boxes, as in the Gerrit
//! web UI. Hovering a box opens a card with who voted and when, and, for
//! those whose vote the user may remove, a trash button. Labels no
//! requirement mentions are listed in a section of their own.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{Account, LabelVote, SubmitRequirement, SubmitRequirementStatus};

use super::{ChangeEvent, ChangePage, view::section_title};
use crate::{reply::plan::vote_text, session::is_unauthorized, time_fmt::relative};

const LABEL_ORDER: [&str; 2] = ["Code-Review", "Verified"];

/// Diameter of the round state mark.
const MARK_SIZE: f32 = 18.;

impl ChangePage {
    pub(super) fn requirements(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let rows: Vec<AnyElement> = d
            .submit_requirements
            .iter()
            .enumerate()
            .map(|(ix, r)| {
                let chips: Vec<AnyElement> = requirement_labels(r, &d.votes)
                    .iter()
                    .flat_map(|label| self.vote_chips(ix, label, &d.votes[label], cx))
                    .collect();
                let missing = missing_note(r);
                h_flex()
                    .gap_x_2()
                    .gap_y_0p5()
                    .flex_wrap()
                    .items_center()
                    .text_sm()
                    .child(requirement_mark(r, cx))
                    .child(div().font_semibold().child(r.name.clone()))
                    .children(chips)
                    .when_some(missing, |row, note| {
                        row.child(div().text_xs().text_color(muted).child(note))
                    })
                    .into_any_element()
            })
            .collect();
        v_flex()
            .gap_1()
            .child(section_title("Submit requirements", cx))
            .children(rows)
            .into_any_element()
    }

    /// The labels no requirement shows the votes of.
    pub(super) fn votes(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.detail.as_ref()?;
        let shown: Vec<String> = d
            .submit_requirements
            .iter()
            .flat_map(|r| requirement_labels(r, &d.votes))
            .collect();
        let mut names: Vec<&String> = d.votes.keys().filter(|n| !shown.contains(n)).collect();
        if names.is_empty() {
            return None;
        }
        names.sort_by_key(|n| {
            (
                LABEL_ORDER
                    .iter()
                    .position(|l| l == n)
                    .unwrap_or(LABEL_ORDER.len()),
                (*n).clone(),
            )
        });
        let muted = cx.theme().muted_foreground;
        let rows: Vec<AnyElement> = names
            .into_iter()
            .enumerate()
            .map(|(ix, name)| {
                let chips = self.vote_chips(1000 + ix, name, &d.votes[name], cx);
                h_flex()
                    .gap_x_2()
                    .gap_y_0p5()
                    .flex_wrap()
                    .items_center()
                    .text_sm()
                    .child(
                        div()
                            .w(px(110.))
                            .flex_none()
                            .text_color(muted)
                            .child(name.clone()),
                    )
                    .when(chips.is_empty(), |row| {
                        row.child(div().text_color(muted).child("no votes"))
                    })
                    .children(chips)
                    .into_any_element()
            })
            .collect();
        Some(
            v_flex()
                .gap_1()
                .child(section_title("Votes", cx))
                .children(rows)
                .into_any_element(),
        )
    }

    /// One colored box per non-zero vote on `label`. `group` keeps the
    /// hover card ids of the lines apart.
    fn vote_chips(
        &self,
        group: usize,
        label: &str,
        votes: &[LabelVote],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(d) = &self.detail else {
            return Vec::new();
        };
        let page = cx.weak_entity();
        let theme = cx.theme().clone();
        let tz = TimeZone::system();
        let now = Timestamp::now();
        let may_remove = self.session.can_comment();
        votes
            .iter()
            .filter(|v| v.value != 0)
            .map(|v| {
                let color = if v.value > 0 {
                    theme.success
                } else {
                    theme.danger
                };
                let removable = may_remove && d.removable_reviewers.contains(&v.account.id);
                let chip = div()
                    .flex_none()
                    .px_1p5()
                    .rounded_md()
                    .text_xs()
                    .font_semibold()
                    .bg(color)
                    .text_color(white())
                    .child(vote_text(v.value));
                let id = SharedString::from(format!("vote-{group}-{label}-{}", v.account.id.0));
                let who = v.account.clone();
                let label = label.to_owned();
                let value = v.value;
                let when = v.date.map(|t| relative(now, t, &tz));
                let page = page.clone();
                HoverCard::new(id.clone())
                    .open_delay(Duration::from_millis(250))
                    .trigger(chip)
                    .content(move |_, _, cx| {
                        vote_card(
                            &id,
                            &label,
                            value,
                            &who,
                            when.as_deref(),
                            removable,
                            &page,
                            cx,
                        )
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// Removes `who`'s vote on `label`.
    fn delete_vote(&mut self, label: String, who: Account, cx: &mut Context<Self>) {
        if self.voting() {
            return;
        }
        let syncer = self.session.syncer().clone();
        let change = self.number;
        let text = format!("{}'s {label} vote", who.display());
        self.hint
            .say(format!("Removing {text}\u{2026}"), false, cx, |p| {
                &mut p.hint
            });
        self.voting = Some(cx.spawn(async move |this, cx| {
            let id = who.id.0.to_string();
            let result = cx
                .background_spawn(async move { syncer.delete_vote(change, &id, &label).await })
                .await;
            this.update(cx, |this, cx| {
                this.voting = None;
                match result {
                    Ok(detail) => {
                        this.take_detail(detail, cx);
                        this.hint
                            .say(format!("Removed {text}"), false, cx, |p| &mut p.hint);
                    }
                    Err(e) if is_unauthorized(&e) => cx.emit(ChangeEvent::Unauthorized),
                    Err(e) => {
                        let why = match &e {
                            sync::Error::Api(gerrit_api::Error::Forbidden { .. }) => {
                                "you may not remove it".to_owned()
                            }
                            other => other.to_string(),
                        };
                        this.hint
                            .say(format!("Could not remove {text}: {why}"), true, cx, |p| {
                                &mut p.hint
                            });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// The content of a vote's hover card.
#[allow(clippy::too_many_arguments)]
fn vote_card(
    id: &SharedString,
    label: &str,
    value: i32,
    who: &Account,
    when: Option<&str>,
    removable: bool,
    page: &WeakEntity<ChangePage>,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme().clone();
    let color = if value > 0 {
        theme.success
    } else {
        theme.danger
    };
    let trash = removable.then(|| {
        let page = page.clone();
        let label = label.to_owned();
        let who = who.clone();
        Button::new(SharedString::from(format!("{id}-remove")))
            .icon(gpui_kit::assets::IconName::Trash)
            .ghost()
            .small()
            .tooltip("Remove this vote")
            .on_click(move |_, _, cx| {
                page.update(cx, |p, cx| p.delete_vote(label.clone(), who.clone(), cx))
                    .ok();
            })
    });
    h_flex()
        .min_w(px(200.))
        .gap_3()
        .items_center()
        .text_sm()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().font_semibold().child(who.display()))
                .child(
                    h_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(
                            div()
                                .text_color(color)
                                .child(format!("{label} {}", vote_text(value))),
                        )
                        .when_some(when, |row, w| row.child(format!("\u{b7} {w}"))),
                ),
        )
        .children(trash)
        .into_any_element()
}

/// The round mark of a requirement: a green disc with a check when it
/// holds, a flat grey one with a dash when it does not (red for an error).
fn requirement_mark(r: &SubmitRequirement, cx: &App) -> Div {
    let t = cx.theme();
    let (icon, color) = match &r.status {
        SubmitRequirementStatus::Satisfied => (IconName::Check, t.success),
        SubmitRequirementStatus::Overridden | SubmitRequirementStatus::Forced => {
            (IconName::Check, t.warning)
        }
        SubmitRequirementStatus::Error | SubmitRequirementStatus::Timeout => {
            (IconName::Minus, t.danger)
        }
        SubmitRequirementStatus::Unsatisfied
        | SubmitRequirementStatus::NotApplicable
        | SubmitRequirementStatus::Unknown(_) => (IconName::Minus, t.muted_foreground),
    };
    div()
        .flex_none()
        .size(px(MARK_SIZE))
        .rounded_full()
        .bg(color)
        .flex()
        .items_center()
        .justify_center()
        .child(
            gpui_kit::component::Icon::new(icon)
                .size_3()
                .text_color(white()),
        )
}

/// What a blocking requirement is missing, as small print. Nothing for
/// those that hold.
fn missing_note(r: &SubmitRequirement) -> Option<String> {
    match (&r.status, &r.submittability) {
        (SubmitRequirementStatus::Unsatisfied, Some(e)) if !e.failing_atoms.is_empty() => {
            Some(format!("needs {}", e.failing_atoms.join(", ")))
        }
        (SubmitRequirementStatus::Error, _) => Some("error".to_owned()),
        (SubmitRequirementStatus::Timeout, _) => Some("timed out".to_owned()),
        _ => None,
    }
}

/// The labels (that exist on the change) whose votes belong to `r`: the
/// label named like it, and those its condition mentions as `label:Name=...`.
pub(super) fn requirement_labels<V>(
    r: &SubmitRequirement,
    labels: &std::collections::BTreeMap<String, V>,
) -> Vec<String> {
    let mut found = Vec::new();
    let mut add = |name: &str| {
        if labels.contains_key(name) && !found.iter().any(|f| f == name) {
            found.push(name.to_owned());
        }
    };
    add(&r.name);
    if let Some(e) = &r.submittability {
        for token in e
            .expression
            .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        {
            let token = token.trim_start_matches('-');
            if let Some(rest) = token
                .strip_prefix("label:")
                .or_else(|| token.strip_prefix("label2:"))
            {
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                    .unwrap_or(rest.len());
                add(&rest[..end]);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use model::{ExpressionResult, SubmitRequirement, SubmitRequirementStatus};

    use super::{missing_note, requirement_labels};

    fn requirement(name: &str, expression: &str) -> SubmitRequirement {
        SubmitRequirement {
            name: name.to_owned(),
            description: None,
            status: SubmitRequirementStatus::Unsatisfied,
            is_legacy: false,
            applicability: None,
            submittability: Some(ExpressionResult {
                expression: expression.to_owned(),
                fulfilled: false,
                status: None,
                passing_atoms: vec![],
                failing_atoms: vec![],
                atom_explanations: BTreeMap::new(),
                error_message: None,
            }),
            override_result: None,
        }
    }

    #[test]
    fn labels_come_from_the_name_and_the_condition() {
        let labels: BTreeMap<String, ()> = ["Code-Review", "Verified", "Style"]
            .into_iter()
            .map(|n| (n.to_owned(), ()))
            .collect();
        let r = requirement(
            "Code-Review",
            "label:Code-Review=MAX -label:Code-Review=MIN",
        );
        assert_eq!(requirement_labels(&r, &labels), ["Code-Review"]);
        let r = requirement(
            "Ready",
            "(label:Verified=+1 AND label2:Style>=1) OR label:Gone=1",
        );
        assert_eq!(requirement_labels(&r, &labels), ["Verified", "Style"]);
        let r = requirement("Verified", "is:mergeable");
        assert_eq!(requirement_labels(&r, &labels), ["Verified"]);
        let r = requirement("Other", "is:mergeable");
        assert!(requirement_labels(&r, &labels).is_empty());
    }

    #[test]
    fn only_blocking_requirements_have_small_print() {
        let mut r = requirement("Code-Review", "label:Code-Review=MAX");
        assert_eq!(missing_note(&r), None);
        r.submittability.as_mut().unwrap().failing_atoms = vec!["label:Code-Review=MAX".into()];
        assert_eq!(
            missing_note(&r).as_deref(),
            Some("needs label:Code-Review=MAX")
        );
        r.status = SubmitRequirementStatus::Satisfied;
        assert_eq!(missing_note(&r), None);
    }
}
