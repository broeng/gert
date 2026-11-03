//! The change page's held card, "Held from your local review": what
//! consolidation held instead of sending (a local review's votes and
//! message, comments that did not port, everything of a change found
//! closed), item by item, each with its reason and what the user may do.
//!
//! - A **vote** can be published onto the change's current patch set, or
//!   dropped. On a merged change it is a post-submit vote, and the card
//!   says so. An approval held as drop-only can only be dropped, and the
//!   row says why ([`DROP_ONLY_NOTE`]).
//! - A review's **message** can be published as the patch-set message on
//!   the current patch set, or dropped.
//! - A **comment** can be posted as a patch-set comment on the current
//!   patch set that quotes where it was ("On local 2, src/a.rs line 40:
//!   …"; never at a guessed line), copied, or dropped.
//!
//! Publishing is the user's own post, through `sync::Syncer::publish_held`
//! (which refuses a drop-only vote whatever is asked, and waits for no
//! consolidation run: it refuses while one runs). It runs on the server's
//! [`crate::outbox::DraftOutbox`], so leaving the page does not cancel it;
//! the outcome comes back as an outbox event, which this page (or another
//! page of the change opened since) says. Sending notifies the reviewers
//! and dropping discards text, so both ask once: the first press arms it,
//! the second acts; the question expires.
//!
//! Keys (`w` for "withheld"; `h` and `l` scroll an inline diff): `w n` /
//! `w p` select, `w s` sends, `w d` drops and `w y` copies a comment's
//! text. The items are the shared [`crate::comments::ChangeComments`]'
//! held and failed reviews and held comments, read from the store.

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{ChangeStatus, Draft, PatchSetNumber, PendingReview, PendingReviewState};
use sync::{HeldItem, HeldPublished, PostOutcome};

use super::{
    ChangePage,
    pending::state_text,
    view::{pill, section_title},
};
use crate::{actions::*, change_detail::patch_set_label, reply};

/// How long a first "send" or "drop" waits for the second.
const ASK: std::time::Duration = std::time::Duration::from_secs(5);

/// What a first press asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HeldAsk {
    Send,
    Drop,
}

/// Why a drop-only vote offers nothing but Drop.
pub const DROP_ONLY_NOTE: &str = "approvals are only sent on the exact commit they were given on";

/// One row of the held card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HeldRow {
    /// A held or failed review: why, above its votes and message. Not
    /// selectable.
    Review(PendingReview),
    Vote {
        review: PendingReview,
        label: String,
        value: i32,
        /// May only be dropped (see [`DROP_ONLY_NOTE`]).
        drop_only: bool,
    },
    Message(PendingReview),
    Comment(Draft),
}

impl HeldRow {
    /// The item the row stands for; `None` for a review's heading.
    pub(super) fn item(&self) -> Option<HeldItem> {
        match self {
            Self::Review(_) => None,
            Self::Vote { review, label, .. } => Some(HeldItem::Vote {
                review: review.id,
                label: label.clone(),
            }),
            Self::Message(r) => Some(HeldItem::Message(r.id)),
            Self::Comment(d) => Some(HeldItem::Comment(d.local_id)),
        }
    }
}

/// The rows of the held card: each held or failed review (in creation
/// order) with its reason, its votes (by label) and its message, then the
/// held comments in creation order. `drop_only` says which votes may only
/// be dropped.
pub(super) fn held_rows(
    reviews: &[PendingReview],
    drafts: &[Draft],
    drop_only: impl Fn(&PendingReview, &str, i32) -> bool,
) -> Vec<HeldRow> {
    let mut rows = Vec::new();
    for r in reviews.iter().filter(|r| {
        matches!(
            r.state,
            PendingReviewState::Held | PendingReviewState::Failed
        )
    }) {
        if r.labels.is_empty() && r.message.is_none() {
            continue;
        }
        rows.push(HeldRow::Review(r.clone()));
        for (label, value) in &r.labels {
            rows.push(HeldRow::Vote {
                review: r.clone(),
                label: label.clone(),
                value: *value,
                drop_only: drop_only(r, label, *value),
            });
        }
        if r.message.as_deref().is_some_and(|m| !m.trim().is_empty()) {
            rows.push(HeldRow::Message(r.clone()));
        }
    }
    rows.extend(drafts.iter().cloned().map(HeldRow::Comment));
    rows
}

/// The first non-blank line of `text`.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

/// What the page says after a publish of `item`.
fn published_text(item: &HeldItem, out: &HeldPublished) -> (String, bool) {
    let ps = out.patch_set.0;
    match &out.outcome {
        PostOutcome::Sent => (
            match item {
                HeldItem::Comment(_) => format!("Posted as a patch-set comment on PS {ps}"),
                HeldItem::Message(_) => format!("Message published on PS {ps}"),
                HeldItem::Vote { .. } => format!("Vote published onto PS {ps}"),
            },
            false,
        ),
        PostOutcome::VotesFailed { error, .. } => {
            (format!("Gerrit did not take the vote: {error}"), true)
        }
        PostOutcome::VotesHeld { reason, .. } => (reason.clone(), true),
        PostOutcome::Refused { error } => (format!("Gerrit refused it: {error}"), true),
    }
}

impl ChangePage {
    /// The held card's rows (see [`held_rows`]). A vote may only be
    /// dropped as `sync::held_vote_drop_only` judges it against Gerrit's
    /// current patch set (also what `Syncer::publish_held` refuses), with
    /// the project's `local_approval` as last read (`held_setting`); with
    /// no Gerrit patch set to publish onto, as recorded.
    pub(super) fn held_rows(&self) -> Vec<HeldRow> {
        let target = self.publish_target().and_then(|ps| {
            self.detail
                .as_ref()
                .and_then(|d| d.revision(ps))
                .map(|r| r.sha.clone())
        });
        let detail = self.detail.as_ref();
        held_rows(&self.reviews, &self.held_drafts, |r, label, value| {
            match (detail, &target) {
                (Some(d), Some(sha)) => {
                    sync::held_vote_drop_only(d, r, label, value, sha, self.held_setting)
                }
                _ => r.drop_only.contains(label),
            }
        })
    }

    /// Reads the project's `local_approval` again for the held card.
    pub(super) fn read_held_setting(&mut self, cx: &App) {
        self.held_setting = self.local_approval(cx);
    }

    /// The selected row: the one under the selection, else the first item.
    fn selected_held(&self) -> Option<HeldRow> {
        let rows = self.held_rows();
        let at = |item: &HeldItem| rows.iter().find(|r| r.item().as_ref() == Some(item));
        self.held_cursor
            .as_ref()
            .and_then(at)
            .or_else(|| rows.iter().find(|r| r.item().is_some()))
            .cloned()
    }

    /// The held items changed: forget a selection or a drop question about
    /// one that is gone.
    pub(super) fn held_changed(&mut self) {
        let items: Vec<HeldItem> = self.held_rows().iter().filter_map(HeldRow::item).collect();
        if self
            .held_cursor
            .as_ref()
            .is_some_and(|i| !items.contains(i))
        {
            self.held_cursor = None;
        }
        if self
            .confirm_held
            .as_ref()
            .is_some_and(|(i, _)| !items.contains(i))
        {
            self.confirm_held = None;
            self.held_timer = None;
        }
    }

    /// The patch set a publish goes to: Gerrit's current one, named so the
    /// user sees it; `None` on a local change (nothing to publish to).
    fn publish_target(&self) -> Option<PatchSetNumber> {
        if self.number.is_local() {
            return None;
        }
        self.detail
            .as_ref()
            .and_then(|d| d.current_patch_set)
            .filter(|ps| !ps.is_local())
    }

    fn no_held(&mut self, cx: &mut Context<Self>) {
        self.hint.say(
            "Nothing is held from your local review here",
            false,
            cx,
            |p| &mut p.hint,
        );
    }

    fn step_held(&mut self, forward: bool, cx: &mut Context<Self>) {
        let items: Vec<HeldItem> = self.held_rows().iter().filter_map(HeldRow::item).collect();
        if items.is_empty() {
            return self.no_held(cx);
        }
        let at = self
            .selected_held()
            .and_then(|r| r.item())
            .and_then(|i| items.iter().position(|x| *x == i))
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % items.len()
        } else {
            (at + items.len() - 1) % items.len()
        };
        self.held_cursor = Some(items[next].clone());
        self.disarm_held(cx);
    }

    pub(super) fn next_held_action(
        &mut self,
        _: &NextHeldItem,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_held(true, cx);
    }

    pub(super) fn prev_held_action(
        &mut self,
        _: &PrevHeldItem,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_held(false, cx);
    }

    pub(super) fn publish_held_action(
        &mut self,
        _: &PublishHeldItem,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_held() {
            Some(row) => self.publish_held(&row, cx),
            None => self.no_held(cx),
        }
    }

    pub(super) fn drop_held_action(
        &mut self,
        _: &DropHeldItem,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_held() {
            Some(row) => self.drop_held(&row, cx),
            None => self.no_held(cx),
        }
    }

    pub(super) fn copy_held_action(
        &mut self,
        _: &CopyHeldItem,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_held() {
            Some(row) => self.copy_held(&row, cx),
            None => self.no_held(cx),
        }
    }

    /// Sends `row` to Gerrit at the user's request: a vote or message onto
    /// the current patch set, a comment as a patch-set comment there.
    fn publish_held(&mut self, row: &HeldRow, cx: &mut Context<Self>) {
        let Some(item) = row.item() else {
            return;
        };
        self.read_held_setting(cx);
        // Judged again with the setting as it is now.
        let Some(row) = self.held_row_by_item(&item) else {
            return;
        };
        let row = &row;
        if let HeldRow::Vote {
            drop_only: true, ..
        } = row
        {
            let why = format!("This approval can only be dropped: {DROP_ONLY_NOTE}");
            return self.hint.say(why, false, cx, |p| &mut p.hint);
        }
        let Some(onto) = self.publish_target() else {
            let why = "There is no patch set on Gerrit to publish onto";
            return self.hint.say(why, false, cx, |p| &mut p.hint);
        };
        if !self.session.can_sync() {
            let why = "Offline: publishing needs a connection to Gerrit";
            return self.hint.say(why, true, cx, |p| &mut p.hint);
        }
        // It notifies the reviewers: asked once, like a drop.
        if self.confirm_held.as_ref() != Some(&(item.clone(), HeldAsk::Send)) {
            self.arm_held(item, HeldAsk::Send, cx);
            let ask = match row {
                HeldRow::Comment(_) => format!(
                    "Post this comment on PS {}? Press w s (or the button) again",
                    onto.0
                ),
                HeldRow::Message(_) => format!(
                    "Publish this message on PS {}? Press w s (or the button) again",
                    onto.0
                ),
                _ => format!(
                    "Publish this vote onto PS {}? Press w s (or the button) again",
                    onto.0
                ),
            };
            return self.hint.say(ask, false, cx, |p| &mut p.hint);
        }
        self.disarm_held(cx);
        let (change, setting) = (self.number, self.held_setting);
        let started = self
            .outbox
            .update(cx, |o, cx| o.publish_held(change, onto, item, setting, cx));
        if started {
            self.hint.say("Sending\u{2026}", false, cx, |p| &mut p.hint);
        }
    }

    /// A held publish of this change ended (it ran on the outbox, so it
    /// also ends after the page was left and opened again): says how.
    pub(super) fn held_published(
        &mut self,
        item: &HeldItem,
        result: &Result<HeldPublished, sync::Error>,
        cx: &mut Context<Self>,
    ) {
        let (text, error) = match result {
            Ok(out) => published_text(item, out),
            Err(sync::Error::MaybePosted(why)) => (
                format!("May have reached Gerrit ({why}); it will be checked and finished"),
                true,
            ),
            Err(e) => (format!("Not sent: {e}"), true),
        };
        if result.is_ok() {
            // New votes and messages on the change.
            self.reload(cx);
        }
        self.hint.say(text, error, cx, |p| &mut p.hint);
        cx.notify();
    }

    /// Drops `row`'s item on the second ask.
    fn drop_held(&mut self, row: &HeldRow, cx: &mut Context<Self>) {
        let Some(item) = row.item() else {
            return;
        };
        if self.confirm_held.as_ref() != Some(&(item.clone(), HeldAsk::Drop)) {
            self.arm_held(item, HeldAsk::Drop, cx);
            let ask = match row {
                HeldRow::Comment(_) => "Drop this comment? Press w d (or Drop) again",
                HeldRow::Message(_) => "Drop this message? Press w d (or Drop) again",
                _ => "Drop this vote? Press w d (or Drop) again",
            };
            return self.hint.say(ask, false, cx, |p| &mut p.hint);
        }
        self.disarm_held(cx);
        if self.held_task.is_some() {
            return;
        }
        let syncer = self.session.syncer().clone();
        let change = self.number;
        self.held_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { syncer.drop_held(&item) })
                .await;
            this.update(cx, |this, cx| {
                this.held_task = None;
                let (text, error) = match result {
                    Ok(true) => ("Dropped".to_owned(), false),
                    Ok(false) => (
                        "It changed meanwhile; nothing was dropped".to_owned(),
                        false,
                    ),
                    Err(e) => (format!("Could not drop it: {e}"), true),
                };
                this.outbox.update(cx, |o, cx| o.published(change, cx));
                this.hint.say(text, error, cx, |p| &mut p.hint);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Copies a held comment's text (or a message's).
    fn copy_held(&mut self, row: &HeldRow, cx: &mut Context<Self>) {
        let text = match row {
            HeldRow::Comment(d) => d.message.clone(),
            HeldRow::Message(r) => r.message.clone().unwrap_or_default(),
            _ => {
                let why = "Only a comment or a message has text to copy";
                return self.hint.say(why, false, cx, |p| &mut p.hint);
            }
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.hint.say("Copied", false, cx, |p| &mut p.hint);
    }

    /// Asks about sending or dropping `item`; the question expires after
    /// [`ASK`], so a later `w s` or `w d` asks again.
    fn arm_held(&mut self, item: HeldItem, ask: HeldAsk, cx: &mut Context<Self>) {
        let asked = (item, ask);
        self.confirm_held = Some(asked.clone());
        self.remeasure(super::state::Block::Held);
        self.held_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ASK).await;
            this.update(cx, |this, cx| {
                if this.confirm_held.as_ref() == Some(&asked) {
                    this.disarm_held(cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn disarm_held(&mut self, cx: &mut Context<Self>) {
        self.confirm_held = None;
        self.held_timer = None;
        self.remeasure(super::state::Block::Held);
        cx.notify();
    }

    fn held_row_by_item(&self, item: &HeldItem) -> Option<HeldRow> {
        self.held_rows()
            .into_iter()
            .find(|r| r.item().as_ref() == Some(item))
    }

    // ---- rendering ----

    pub(super) fn held_card(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.read_held_setting(cx);
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let rows = self.held_rows();
        let selected = self.selected_held().and_then(|r| r.item());
        let target = self.publish_target();
        let revisions = self.detail.as_ref().map_or(&[][..], |d| &d.revisions[..]);
        let status = self.detail.as_ref().map(|d| d.summary.status.clone());
        let items = rows.iter().filter(|r| r.item().is_some()).count();
        let elements: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let item = row.item();
                let is_selected = item.is_some() && item == selected;
                let asked = |ask| {
                    item.as_ref()
                        .is_some_and(|i| self.confirm_held == Some((i.clone(), ask)))
                };
                let arm = asked(HeldAsk::Drop);
                let arm_send = asked(HeldAsk::Send);
                let publish = |label: String| {
                    let item = item.clone();
                    Button::new(("held-publish", i))
                        .label(if arm_send { "Send?".to_owned() } else { label })
                        .xsmall()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(r) = item.as_ref().and_then(|i| this.held_row_by_item(i)) {
                                this.publish_held(&r, cx);
                            }
                        }))
                };
                let drop = {
                    let item = item.clone();
                    Button::new(("held-drop", i))
                        .label(if arm { "Drop?" } else { "Drop" })
                        .xsmall()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(r) = item.as_ref().and_then(|i| this.held_row_by_item(i)) {
                                this.drop_held(&r, cx);
                            }
                        }))
                };
                let line = h_flex()
                    .id(("held-row", i))
                    .gap_2()
                    .px_1()
                    .py_0p5()
                    .items_center()
                    .text_sm()
                    .border_l_2()
                    .border_color(if is_selected {
                        theme.list_active_border
                    } else {
                        theme.transparent
                    })
                    .when(is_selected, |d| d.bg(theme.list_active));
                match row {
                    HeldRow::Review(r) => {
                        let (color, what) = match r.state {
                            PendingReviewState::Failed => (theme.danger, "failed"),
                            _ => (theme.warning, "held"),
                        };
                        let text = state_text(r);
                        let reason = text
                            .strip_prefix(what)
                            .map(|t| t.trim_start_matches(':').trim())
                            .unwrap_or_default()
                            .to_owned();
                        h_flex()
                            .id(("held-review", i))
                            .debug_selector(move || format!("held-review-{i}: {text}"))
                            .gap_2()
                            .px_1()
                            .pt_1()
                            .items_start()
                            .text_xs()
                            .child(
                                div()
                                    .w(px(64.))
                                    .flex_none()
                                    .truncate()
                                    .text_color(muted)
                                    .child(patch_set_label(revisions, r.patch_set)),
                            )
                            .child(pill(what, color))
                            .child(div().flex_1().min_w_0().text_color(muted).child(reason))
                            .into_any_element()
                    }
                    HeldRow::Vote {
                        label,
                        value,
                        drop_only,
                        ..
                    } => {
                        let vote = format!("{label} {}", reply::plan::vote_text(*value));
                        let desc = vote.clone();
                        line.debug_selector(move || format!("held-row-{i}: vote {desc}"))
                            .child(div().w(px(64.)).flex_none())
                            .child(div().flex_none().child(vote))
                            .when(*drop_only, |d| {
                                d.child(pill("drop only", theme.warning)).child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(muted)
                                        .debug_selector(|| "held-drop-only".into())
                                        .child(DROP_ONLY_NOTE),
                                )
                            })
                            .when(!*drop_only, |d| d.child(div().flex_1()))
                            .when_some(target.filter(|_| !*drop_only), |d, ps| {
                                d.child(publish(format!("Publish onto PS {}", ps.0)))
                            })
                            .child(drop)
                            .into_any_element()
                    }
                    HeldRow::Message(r) => {
                        let text = first_line(r.message.as_deref().unwrap_or_default()).to_owned();
                        line.debug_selector(move || format!("held-row-{i}: message"))
                            .child(div().w(px(64.)).flex_none())
                            .child(div().flex_none().text_color(muted).child("Message:"))
                            .child(div().flex_1().min_w_0().truncate().child(text))
                            .when_some(target, |d, ps| {
                                d.child(publish(format!("Publish onto PS {}", ps.0)))
                            })
                            .child(drop)
                            .into_any_element()
                    }
                    HeldRow::Comment(d) => {
                        let version = patch_set_label(revisions, d.patch_set);
                        let place = sync::held_place(&version, d);
                        let reason = d.held_reason.clone().unwrap_or_default();
                        let desc = place.clone();
                        let copy = {
                            let item = item.clone();
                            Button::new(("held-copy", i))
                                .label("Copy text")
                                .xsmall()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(r) =
                                        item.as_ref().and_then(|i| this.held_row_by_item(i))
                                    {
                                        this.copy_held(&r, cx);
                                    }
                                }))
                        };
                        v_flex()
                            .child(
                                line.debug_selector(move || {
                                    format!("held-row-{i}: comment {desc}")
                                })
                                .child(
                                    div()
                                        .flex_none()
                                        .max_w(px(260.))
                                        .truncate()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(place),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(first_line(&d.message).to_owned()),
                                )
                                .when(target.is_some(), |d| {
                                    d.child(publish("Post as patch-set comment".to_owned()))
                                })
                                .child(copy)
                                .child(drop),
                            )
                            .child(
                                div()
                                    .pl_3()
                                    .text_xs()
                                    .text_color(theme.warning)
                                    .child(reason),
                            )
                            .into_any_element()
                    }
                }
            })
            .collect();
        let note = match status {
            Some(ChangeStatus::Merged) => {
                "the change is merged: a vote published now is a post-submit vote"
            }
            Some(ChangeStatus::Abandoned) => "the change is abandoned: Gerrit may refuse votes",
            _ => "sent only when you publish them",
        };
        let closed = matches!(
            self.detail.as_ref().map(|d| &d.summary.status),
            Some(ChangeStatus::Merged | ChangeStatus::Abandoned)
        );
        let keys = if items > 1 {
            "w n / w p select \u{b7} w s send \u{b7} w d drop \u{b7} w y copy"
        } else {
            "w s send \u{b7} w d drop \u{b7} w y copy"
        };
        v_flex()
            .child(
                super::view::card_title_bar(cx)
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(section_title("Held from your local review", cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(if closed { theme.warning } else { muted })
                            .when(closed, |d| d.debug_selector(|| "held-closed-note".into()))
                            .child(note),
                    )
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(muted).child(keys)),
            )
            .child(v_flex().px_2().py_1().gap_0p5().children(elements))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use model::{
        ChangeNumber, DraftSync, LocalApproval, LocalChangeId, LocalDraftId, Location,
        PatchSetNumber, PendingReviewId, RevisionId, Side, Timestamp,
    };

    use super::{
        Draft, HeldItem, HeldPublished, HeldRow, PendingReview, PendingReviewState, PostOutcome,
        held_rows, published_text,
    };

    fn review(id: i64, state: PendingReviewState) -> PendingReview {
        PendingReview {
            id: PendingReviewId(id),
            local_change: LocalChangeId(1),
            change: ChangeNumber(7),
            patch_set: PatchSetNumber(1),
            revision: RevisionId("s1".into()),
            given_on_sha: RevisionId("s1".into()),
            labels: BTreeMap::from([("Code-Review".to_owned(), 2), ("Verified".to_owned(), 1)]),
            strategy: LocalApproval::Exact,
            state,
            error: None,
            held_reason: Some("not current".into()),
            drop_only: ["Code-Review".to_owned()].into(),
            created: Timestamp::UNIX_EPOCH,
            voted_at: Timestamp::UNIX_EPOCH,
            nonce: None,
            message: Some("LGTM".into()),
            unresolved: false,
            message_draft: Some(LocalDraftId(9)),
            author: None,
            author_name: None,
        }
    }

    fn draft(id: i64) -> Draft {
        Draft {
            local_id: LocalDraftId(id),
            server_id: None,
            change: ChangeNumber(7),
            patch_set: PatchSetNumber(PatchSetNumber::LOCAL_BASE + 1),
            revision: RevisionId("l1".into()),
            path: "src/a.rs".into(),
            side: Side::Revision,
            parent: None,
            location: Location::Line(40),
            in_reply_to: None,
            message: "why?".into(),
            unresolved: true,
            updated: Timestamp::UNIX_EPOCH,
            sync: DraftSync::PendingCreate,
            attempts: 0,
            last_error: None,
            version: 0,
            pending_review: None,
            reserved_by: None,
            held_reason: Some("line 40 changed".into()),
            author: None,
        }
    }

    #[test]
    fn rows_are_each_reviews_votes_and_message_then_the_comments() {
        let held = review(1, PendingReviewState::Held);
        let pending = review(2, PendingReviewState::Pending);
        let rows = held_rows(&[held.clone(), pending], &[draft(5)], |r, l, _| {
            r.drop_only.contains(l)
        });
        let items: Vec<Option<HeldItem>> = rows.iter().map(HeldRow::item).collect();
        assert_eq!(
            items,
            [
                None,
                Some(HeldItem::Vote {
                    review: held.id,
                    label: "Code-Review".into()
                }),
                Some(HeldItem::Vote {
                    review: held.id,
                    label: "Verified".into()
                }),
                Some(HeldItem::Message(held.id)),
                Some(HeldItem::Comment(LocalDraftId(5))),
            ]
        );
        assert!(matches!(
            &rows[1],
            HeldRow::Vote {
                drop_only: true,
                ..
            }
        ));
        assert!(matches!(
            &rows[2],
            HeldRow::Vote {
                drop_only: false,
                ..
            }
        ));
        // A review with nothing left shows nothing.
        let mut empty = review(3, PendingReviewState::Failed);
        empty.labels.clear();
        empty.message = None;
        assert!(held_rows(&[empty], &[], |_, _, _| false).is_empty());
    }

    #[test]
    fn a_publish_says_what_happened() {
        let row = HeldItem::Comment(LocalDraftId(5));
        let out = |outcome| HeldPublished {
            patch_set: PatchSetNumber(3),
            votes: Vec::new(),
            comments: 1,
            outcome,
        };
        assert_eq!(
            published_text(&row, &out(PostOutcome::Sent)),
            ("Posted as a patch-set comment on PS 3".to_owned(), false)
        );
        assert_eq!(
            published_text(
                &row,
                &out(PostOutcome::Refused {
                    error: "nope".into()
                })
            ),
            ("Gerrit refused it: nope".to_owned(), true)
        );
    }
}
