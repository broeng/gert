//! The submit requirements and the votes in the change page's first column.
//!
//! A requirement is one line: a round mark for its state, its name, then the
//! votes on the labels it asks about as small colored boxes, as in the Gerrit
//! web UI. Hovering a box opens a card with who voted and when, and, for
//! those whose vote the user may remove, a trash button. Labels no
//! requirement mentions are listed in a section of their own.
//!
//! On a local version the user's votes in the local reviews of it and of
//! the change's older local versions (a vote carries over to an amended
//! commit, as far as the boxes go) that are not on Gerrit yet are boxes
//! too, each with a `local` pill: on a local
//! change, whose labels (borrowed, see [`crate::change_detail`]) are all
//! in the votes section, and on a local patch set of a Gerrit change next
//! to Gerrit's votes. Their trash button takes the vote back out of the
//! version's pending reviews.

use std::collections::BTreeMap;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{
    Account, LabelVote, LocalApproval, PatchSetNumber, PendingReviewState, SubmitRequirement,
    SubmitRequirementStatus,
};

use super::{ChangeEvent, ChangePage, view::section_title};
use crate::{
    change_detail::{patch_set_label, position},
    local_source::local_chip,
    reply::plan::vote_text,
    session::is_unauthorized,
    time_fmt::relative,
};

const LABEL_ORDER: [&str; 2] = ["Code-Review", "Verified"];

/// Diameter of the round state mark.
const MARK_SIZE: f32 = 18.;
/// Space between the parts of a line (`gap_x_2`).
const NAME_GAP: f32 = 8.;
/// Width per character of a semibold name, and the bounds of the column.
const NAME_CHAR: f32 = 8.;
const NAME_MIN: f32 = 90.;
const NAME_MAX: f32 = 220.;

/// The user's vote on a label in a local review not on Gerrit yet.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalVote {
    value: i32,
    voted_at: model::Timestamp,
    /// Its review is being posted now.
    sending: bool,
    /// The version it was given on.
    patch_set: PatchSetNumber,
    /// Its review's `local_approval` mode.
    strategy: LocalApproval,
}

impl ChangePage {
    pub(super) fn requirements(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let name_width = self.name_width();
        let local = self.local_votes();
        let rows: Vec<AnyElement> = ordered(&d.submit_requirements)
            .into_iter()
            .enumerate()
            .map(|(ix, r)| {
                let chips: Vec<AnyElement> = requirement_labels(r, &d.votes)
                    .iter()
                    .flat_map(|label| {
                        self.vote_chips(ix, label, &d.votes[label], local.get(label), cx)
                    })
                    .collect();
                h_flex()
                    .gap_x_2()
                    .gap_y_0p5()
                    .flex_wrap()
                    .items_center()
                    .text_sm()
                    .child(requirement_mark(r, cx))
                    .child(
                        div()
                            .w(name_width)
                            .flex_none()
                            .truncate()
                            .font_semibold()
                            .child(r.name.clone()),
                    )
                    .children(chips)
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
        let local = self.local_votes();
        let shown: Vec<String> = d
            .submit_requirements
            .iter()
            .flat_map(|r| requirement_labels(r, &d.votes))
            .collect();
        let mut names: Vec<&String> = self
            .label_names(&local)
            .into_iter()
            .filter(|n| !shown.contains(n))
            .collect();
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
        // Lined up with the names of the requirements above.
        let name_width = self.name_width() + px(MARK_SIZE + NAME_GAP);
        let rows: Vec<AnyElement> = names
            .into_iter()
            .enumerate()
            .map(|(ix, name)| {
                let votes = d.votes.get(name).map_or(&[][..], Vec::as_slice);
                let chips = self.vote_chips(1000 + ix, name, votes, local.get(name), cx);
                h_flex()
                    .gap_x_2()
                    .gap_y_0p5()
                    .flex_wrap()
                    .items_center()
                    .text_sm()
                    .child(
                        div()
                            .w(name_width)
                            .flex_none()
                            .truncate()
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

    /// The labels with a line: those of the change, on a local change the
    /// labels it offers (it has no votes from Gerrit), and those of the
    /// user's local votes `local`.
    fn label_names<'a>(&'a self, local: &'a BTreeMap<String, LocalVote>) -> Vec<&'a String> {
        let Some(d) = &self.detail else {
            return Vec::new();
        };
        let offered = d
            .label_values
            .keys()
            .filter(|_| d.summary.number.is_local());
        let mut names: Vec<&String> = d.votes.keys().chain(offered).chain(local.keys()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// The local versions whose votes the boxes show: the one shown and
    /// the change's local versions before it. Empty unless a local version
    /// is shown.
    fn vote_versions(&self) -> Vec<PatchSetNumber> {
        let (Some(d), Some(target)) = (&self.detail, self.local_target()) else {
            return Vec::new();
        };
        let Some(at) = position(&d.revisions, target.number) else {
            return Vec::new();
        };
        d.revisions[..=at]
            .iter()
            .map(|r| r.number)
            .filter(|n| n.is_local())
            .collect()
    }

    /// The user's votes in the local reviews of the [`vote_versions`]
    /// that are not on Gerrit yet (pending or being sent), per label: the
    /// shown version's newest review that has the label, else the newest
    /// of an older version, since the reviews go to Gerrit oldest first.
    ///
    /// [`vote_versions`]: ChangePage::vote_versions
    fn local_votes(&self) -> BTreeMap<String, LocalVote> {
        let versions = self.vote_versions();
        let Some(target) = versions.last().copied() else {
            return BTreeMap::new();
        };
        let mut reviews: Vec<_> = self
            .reviews
            .iter()
            .filter(|r| {
                versions.contains(&r.patch_set)
                    && matches!(
                        r.state,
                        PendingReviewState::Pending | PendingReviewState::Sending
                    )
            })
            .collect();
        // Stable: creation order within each group, the shown version last.
        reviews.sort_by_key(|r| r.patch_set == target);
        let mut votes = BTreeMap::new();
        for r in reviews {
            for (label, value) in &r.labels {
                votes.insert(
                    label.clone(),
                    LocalVote {
                        value: *value,
                        voted_at: r.voted_at,
                        sending: r.state == PendingReviewState::Sending,
                        patch_set: r.patch_set,
                        strategy: r.strategy,
                    },
                );
            }
        }
        votes
    }

    /// The local vote boxes: label, value and the version each was given on.
    #[cfg(test)]
    pub(crate) fn local_votes_for_test(&self) -> Vec<(String, i32, PatchSetNumber)> {
        self.local_votes()
            .into_iter()
            .map(|(label, v)| (label, v.value, v.patch_set))
            .collect()
    }

    /// Width of the name column, so the vote boxes of all lines start at the
    /// same place: the longest requirement or label name, within bounds.
    fn name_width(&self) -> Pixels {
        let Some(d) = &self.detail else {
            return px(NAME_MIN);
        };
        let local = self.local_votes();
        let longest = d
            .submit_requirements
            .iter()
            .map(|r| r.name.as_str())
            .chain(self.label_names(&local).into_iter().map(String::as_str))
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0);
        px((longest as f32 * NAME_CHAR).clamp(NAME_MIN, NAME_MAX))
    }

    /// One colored box per non-zero vote on `label`, then the user's
    /// `local` vote on it, if any. `group` keeps the hover card ids of the
    /// lines apart.
    fn vote_chips(
        &self,
        group: usize,
        label: &str,
        votes: &[LabelVote],
        local: Option<&LocalVote>,
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
            .chain(local.and_then(|v| self.local_vote_chip(group, label, v, cx)))
            .collect()
    }

    /// The box of the user's local vote `v` on `label`, with a `local`
    /// pill; its card says when it goes to Gerrit and, until it is being
    /// sent, has a trash button. Nothing for a 0, which takes a vote back.
    fn local_vote_chip(
        &self,
        group: usize,
        label: &str,
        v: &LocalVote,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if v.value == 0 {
            return None;
        }
        let d = self.detail.as_ref()?;
        let target = self.local_target()?.number;
        let version = patch_set_label(&d.revisions, v.patch_set);
        let theme = cx.theme();
        let color = if v.value > 0 {
            theme.success
        } else {
            theme.danger
        };
        let chip = h_flex()
            .flex_none()
            .gap_0p5()
            .items_center()
            .child(
                div()
                    .px_1p5()
                    .rounded_md()
                    .text_xs()
                    .font_semibold()
                    .bg(color)
                    .text_color(white())
                    .child(vote_text(v.value)),
            )
            .child(local_chip(cx));
        let id = SharedString::from(format!("local-vote-{group}-{label}"));
        let selector = format!("local-vote-{label}");
        let text = format!("{label} {}", vote_text(v.value));
        let when = relative(Timestamp::now(), v.voted_at, &TimeZone::system());
        let status = if v.sending {
            "Being sent to Gerrit now".to_owned()
        } else if v.patch_set != target {
            // The stricter of the review's mode and the project's now, as
            // consolidation judges it.
            let strategy = v.strategy.max(self.local_approval(cx));
            carried_status(&version, strategy, !d.summary.number.is_local())
        } else {
            format!("Sent when {version} is on Gerrit")
        };
        let removable = !v.sending;
        let page = cx.weak_entity();
        let label = label.to_owned();
        Some(
            HoverCard::new(id.clone())
                .open_delay(Duration::from_millis(250))
                .trigger(chip.debug_selector(move || selector.clone()))
                .content(move |_, _, cx| {
                    let trash = removable.then(|| {
                        let page = page.clone();
                        let label = label.clone();
                        Button::new(SharedString::from(format!("{id}-remove")))
                            .icon(gpui_kit::assets::IconName::Trash)
                            .ghost()
                            .small()
                            .tooltip("Remove this vote")
                            .on_click(move |_, _, cx| {
                                page.update(cx, |p, cx| p.withdraw_local_vote(label.clone(), cx))
                                    .ok();
                            })
                    });
                    local_vote_card(&text, color, &when, &status, trash, cx)
                })
                .into_any_element(),
        )
    }

    /// Takes the user's local vote on `label` back on the local version
    /// shown: no review of it or of an older local version (whose votes
    /// the box may show) sends that label (see
    /// `Store::withdraw_local_vote`). Store writes only.
    pub(super) fn withdraw_local_vote(&mut self, label: String, cx: &mut Context<Self>) {
        if self.voting() {
            return;
        }
        let versions = self.vote_versions();
        if versions.is_empty() {
            return;
        }
        let syncer = self.session.syncer().clone();
        self.voting = Some(cx.spawn(async move |this, cx| {
            let what = label.clone();
            let result = cx
                .background_spawn(async move { syncer.withdraw_local_vote(&versions, &what) })
                .await;
            this.update(cx, |this, cx| {
                this.voting = None;
                match result {
                    Ok(_) => {
                        let change = this.number;
                        this.outbox.update(cx, |o, cx| o.published(change, cx));
                        this.hint
                            .say(format!("Removed your local {label} vote"), false, cx, |p| {
                                &mut p.hint
                            });
                    }
                    Err(e) => {
                        let text = format!("Could not remove your local {label} vote: {e}");
                        this.hint.say(text, true, cx, |p| &mut p.hint);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
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

/// What a local vote given on `version`, an older version than the one
/// shown, becomes: the review is ported to the version that reaches
/// Gerrit, and its votes follow only when the mode in force (`strategy`)
/// is `follow` on a local change (`gerrit_change`: a local version of a
/// Gerrit change, which holds them by the strict rule). Kept short; the
/// held card says the rest.
fn carried_status(version: &str, strategy: LocalApproval, gerrit_change: bool) -> String {
    if strategy == LocalApproval::Follow && !gerrit_change {
        format!("Given on {version}; follows the amended commit to Gerrit")
    } else {
        format!("Given on {version}; held for you to decide when the change is on Gerrit")
    }
}

/// The content of a local vote's hover card.
fn local_vote_card(
    text: &str,
    color: Hsla,
    when: &str,
    status: &str,
    trash: Option<Button>,
    cx: &mut App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .min_w(px(200.))
        .gap_3()
        .items_center()
        .text_sm()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_1()
                        .child(div().font_semibold().child("Your local vote"))
                        .child(local_chip(cx)),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(muted)
                        .child(div().text_color(color).child(text.to_owned()))
                        .child(format!("\u{b7} {when}")),
                )
                .child(div().text_xs().text_color(muted).child(status.to_owned())),
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

/// Code-Review first, the rest as the server lists them.
fn ordered(requirements: &[SubmitRequirement]) -> Vec<&SubmitRequirement> {
    let mut v: Vec<_> = requirements.iter().collect();
    v.sort_by_key(|r| r.name != "Code-Review");
    v
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

    use model::{ExpressionResult, LocalApproval, SubmitRequirement, SubmitRequirementStatus};

    use super::{carried_status, ordered, requirement_labels};

    #[test]
    fn a_carried_vote_says_whether_it_follows_the_amended_commit() {
        let follows = "Given on local 1; follows the amended commit to Gerrit";
        let held = "Given on local 1; held for you to decide when the change is on Gerrit";
        assert_eq!(
            carried_status("local 1", LocalApproval::Follow, false),
            follows
        );
        for strategy in [LocalApproval::ExactOrManual, LocalApproval::Exact] {
            assert_eq!(carried_status("local 1", strategy, false), held);
        }
        // A local version of a Gerrit change: the strict rule.
        assert_eq!(carried_status("local 1", LocalApproval::Follow, true), held);
    }

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
    fn code_review_comes_first_and_the_rest_keep_their_order() {
        let rs = [
            requirement("Verified", ""),
            requirement("Style", ""),
            requirement("Code-Review", ""),
        ];
        let names: Vec<_> = ordered(&rs).iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Code-Review", "Verified", "Style"]);
    }
}
