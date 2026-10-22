//! The reply dialog without the UI: which votes can be cast, who is in the
//! attention set and who should be, which drafts the reply publishes, and
//! the `ReviewInput` all that adds up to.

use std::collections::BTreeMap;

use model::{
    Account, AccountId, AttentionSetInput, ChangeDetail, ChangeStatus, Draft, DraftSync, Entry,
    LocalDraftId, PATCHSET_LEVEL_PATH, PatchSetNumber, ReviewInput, Thread,
};

use crate::pages::change::threads::location_text;

// ---- votes ----

/// One value a label can be given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteChoice {
    pub value: i32,
    /// What the value means ("Looks good to me, approved"); may be empty.
    pub description: String,
}

/// One label the user may vote on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteRow {
    pub label: String,
    /// The permitted values, ascending.
    pub choices: Vec<VoteChoice>,
    /// The user's vote now (0 if none).
    pub current: i32,
    /// The vote the reply will leave; `current` until the user picks.
    pub selected: i32,
}

impl VoteRow {
    pub fn changed(&self) -> bool {
        self.selected != self.current
    }

    /// The choice the keyboard focus starts on: the selected value (the
    /// user's own vote until they pick another), else 0, else the first.
    pub fn focus_index(&self) -> usize {
        let at = |v: i32| self.choices.iter().position(|c| c.value == v);
        at(self.selected).or_else(|| at(0)).unwrap_or(0)
    }
}

/// Text of a vote value: `+1`, `-2`, `0`.
pub fn vote_text(v: i32) -> String {
    if v > 0 {
        format!("+{v}")
    } else {
        v.to_string()
    }
}

/// The labels the user may vote on, Code-Review and Verified first, the rest
/// alphabetical. Labels without a permitted value are left out (the server
/// would drop the vote), and so are all labels for anonymous sessions, whose
/// `permitted_labels` is empty. `me` is the user's login name, to find their
/// own vote.
pub fn vote_rows(detail: &ChangeDetail, me: Option<&str>) -> Vec<VoteRow> {
    let mut rows: Vec<VoteRow> = detail
        .permitted_labels
        .iter()
        .filter(|(_, values)| !values.is_empty())
        .map(|(label, values)| {
            let meanings = detail.label_values.get(label);
            let mut values = values.clone();
            values.sort_unstable();
            values.dedup();
            let current = own_vote(detail, label, me);
            VoteRow {
                label: label.clone(),
                choices: values
                    .into_iter()
                    .map(|value| VoteChoice {
                        value,
                        description: meanings
                            .and_then(|m| m.iter().find(|m| m.value == value))
                            .map(|m| m.description.clone())
                            .unwrap_or_default(),
                    })
                    .collect(),
                current,
                selected: current,
            }
        })
        .collect();
    let rank = |l: &str| ["Code-Review", "Verified"].iter().position(|n| *n == l);
    rows.sort_by(|a, b| {
        rank(&a.label)
            .unwrap_or(usize::MAX)
            .cmp(&rank(&b.label).unwrap_or(usize::MAX))
            .then_with(|| a.label.cmp(&b.label))
    });
    rows
}

/// The user's vote on `label`; 0 when they have not voted.
fn own_vote(detail: &ChangeDetail, label: &str, me: Option<&str>) -> i32 {
    let Some(me) = me else {
        return 0;
    };
    detail
        .votes
        .get(label)
        .and_then(|votes| {
            votes
                .iter()
                .find(|v| v.account.username.as_deref() == Some(me))
        })
        .map_or(0, |v| v.value)
}

/// The votes to send: only the labels whose value the user changed. Labels
/// that are left out keep the current vote.
pub fn labels_to_send(rows: &[VoteRow]) -> BTreeMap<String, i32> {
    rows.iter()
        .filter(|r| r.changed())
        .map(|r| (r.label.clone(), r.selected))
        .collect()
}

// ---- attention set ----

/// Someone who can be in the attention set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub account: Account,
    pub owner: bool,
    pub reviewer: bool,
    pub cc: bool,
    /// The signed-in user.
    pub me: bool,
    /// In the attention set now (as the server has it).
    pub in_set: bool,
    /// Why, if the server said.
    pub reason: Option<String>,
    /// Whether the reply leaves them in the set.
    pub want: bool,
}

impl Person {
    /// "owner", "reviewer", "CC", for the roles the person has.
    pub fn roles(&self) -> String {
        let mut v = Vec::new();
        if self.me {
            v.push("you");
        }
        if self.owner {
            v.push("owner");
        }
        if self.reviewer {
            v.push("reviewer");
        }
        if self.cc {
            v.push("CC");
        }
        v.join(", ")
    }
}

/// Everyone who matters for the attention set (the owner, reviewers, CCs
/// and whoever is in the set already, the user only if they are in it),
/// with the set a reply would leave by default.
///
/// The rules are PolyGerrit's, kept short:
/// * the replier leaves the set;
/// * when somebody else replies, the owner is added (the ball is back with
///   them);
/// * when the owner replies, the reviewers who wait for an answer are added,
///   those whose comment ended an unresolved thread;
/// * everybody else stays as they are;
/// * on a closed change nobody is added, and on a work-in-progress change
///   the owner is not added.
pub fn people(detail: &ChangeDetail, me: Option<&str>, threads: &[Thread]) -> Vec<Person> {
    let summary = &detail.summary;
    let is_me = |a: &Account| me.is_some_and(|me| a.username.as_deref() == Some(me));
    let mut out: Vec<Person> = Vec::new();
    let mut add = |account: &Account, owner: bool, reviewer: bool, cc: bool| {
        if let Some(p) = out.iter_mut().find(|p| p.account.id == account.id) {
            p.owner |= owner;
            p.reviewer |= reviewer;
            p.cc |= cc;
            return;
        }
        let entry = detail.attention.iter().find(|e| e.account.id == account.id);
        let in_set = entry.is_some() || summary.attention_set.contains(&account.id);
        out.push(Person {
            account: account.clone(),
            owner,
            reviewer,
            cc,
            me: is_me(account),
            in_set,
            reason: entry.and_then(|e| e.reason.clone()),
            want: in_set,
        });
    };
    add(&summary.owner, true, false, false);
    for a in &detail.reviewers {
        add(a, false, true, false);
    }
    for a in &detail.cc {
        add(a, false, false, true);
    }
    for e in &detail.attention {
        add(&e.account, false, false, false);
    }
    // The user is listed only when there is something to take off.
    out.retain(|p| !p.me || p.in_set);
    out.sort_by_key(|p| (!p.me, !p.owner, !p.reviewer));

    let closed = !matches!(summary.status, ChangeStatus::New);
    let me_is_owner = out.iter().any(|p| p.me && p.owner) || is_me(&summary.owner);
    for p in &mut out {
        if p.me {
            p.want = false;
        }
    }
    if !closed {
        if me_is_owner {
            let waiting = waiting_on(threads, me);
            for p in out.iter_mut().filter(|p| !p.me) {
                if waiting.contains(&p.account.id) {
                    p.want = true;
                }
            }
        } else if !summary.work_in_progress
            && let Some(owner) = out.iter_mut().find(|p| p.owner)
        {
            owner.want = true;
        }
    }
    out
}

/// The authors who wrote the last published comment of a thread that is
/// still unresolved, other than `me`.
fn waiting_on(threads: &[Thread], me: Option<&str>) -> Vec<AccountId> {
    threads
        .iter()
        .filter(|t| t.is_unresolved())
        .filter_map(|t| {
            t.entries.iter().rev().find_map(|e| match e {
                Entry::Published(c) => c.author.as_ref(),
                Entry::Draft(_) => None,
            })
        })
        .filter(|a| me.is_none_or(|me| a.username.as_deref() != Some(me)))
        .map(|a| a.id)
        .collect()
}

/// The attention set operations that take the set from what the server has
/// to what the user chose: `(add, remove)`. `who` names the replier in the
/// reason that Gerrit puts in the change log.
pub fn attention_changes(
    people: &[Person],
    who: &str,
) -> (Vec<AttentionSetInput>, Vec<AttentionSetInput>) {
    let reason = format!("{who} replied");
    let op = |p: &Person| AttentionSetInput {
        user: p.account.id,
        reason: reason.clone(),
    };
    let add = people.iter().filter(|p| p.want && !p.in_set).map(op);
    let remove = people.iter().filter(|p| !p.want && p.in_set).map(op);
    (add.collect(), remove.collect())
}

// ---- drafts ----

/// The patch-set level draft on `current` that is not a reply: the text of
/// the reply dialog.
pub fn message_draft(threads: &[Thread], current: PatchSetNumber) -> Option<&Draft> {
    threads
        .iter()
        .flat_map(|t| &t.entries)
        .find_map(|e| match e {
            Entry::Draft(d)
                if d.path == PATCHSET_LEVEL_PATH
                    && d.patch_set == current
                    && d.in_reply_to.is_none() =>
            {
                Some(d)
            }
            _ => None,
        })
}

/// A draft the reply will publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftItem {
    pub id: LocalDraftId,
    /// `src/a.rs:12`, `src/a.rs (file)` or `Patchset`.
    pub place: String,
    /// The first line of the text.
    pub text: String,
    pub patch_set: PatchSetNumber,
    /// On a patch set other than the current one.
    pub other_patch_set: bool,
    pub sync: DraftSync,
    pub last_error: Option<String>,
}

impl DraftItem {
    /// The server does not have the draft as it is.
    pub fn pending(&self) -> bool {
        self.sync.is_pending()
    }

    pub fn failed(&self) -> bool {
        matches!(self.sync, DraftSync::Failed { .. })
    }

    /// Why it is not uploaded, for the list.
    pub fn problem(&self) -> Option<String> {
        match &self.sync {
            DraftSync::Synced => None,
            DraftSync::Failed { error, .. } => Some(format!("failed: {error}")),
            _ => Some(match &self.last_error {
                Some(e) => format!("not uploaded: {e}"),
                None => "not uploaded yet".to_owned(),
            }),
        }
    }
}

/// Every draft of the change except `message` (the dialog's own text),
/// in the order of the threads.
pub fn draft_items(
    threads: &[Thread],
    current: PatchSetNumber,
    message: Option<LocalDraftId>,
) -> Vec<DraftItem> {
    threads
        .iter()
        .flat_map(|t| {
            t.entries.iter().filter_map(move |e| match e {
                Entry::Draft(d) if Some(d.local_id) != message => Some(DraftItem {
                    id: d.local_id,
                    place: location_text(t),
                    text: first_line(&d.message),
                    patch_set: d.patch_set,
                    other_patch_set: d.patch_set != current,
                    sync: d.sync.clone(),
                    last_error: d.last_error.clone(),
                }),
                _ => None,
            })
        })
        .collect()
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned()
}

// ---- the review ----

/// The review to post: the changed votes, the attention set operations and
/// every draft. The message is the patch-set level draft, which `Publish
/// AllRevisions` publishes; `ReviewInput::message` stays empty.
pub fn review_input(
    votes: &[VoteRow],
    add: Vec<AttentionSetInput>,
    remove: Vec<AttentionSetInput>,
) -> ReviewInput {
    ReviewInput {
        labels: labels_to_send(votes),
        add_to_attention_set: add,
        remove_from_attention_set: remove,
        ..ReviewInput::default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use model::{
        Account, AttentionEntry, ChangeStatus, DraftSync, LabelValue, LabelVote, Location,
        ReviewInput, Thread,
    };

    use super::*;
    use crate::{
        diff::tests::threads::{draft, thread},
        testing::detail,
    };

    fn acct(id: u64, user: &str) -> Account {
        Account {
            id: AccountId(id),
            name: Some(user.to_owned()),
            email: None,
            username: Some(user.to_owned()),
            display_name: None,
        }
    }

    fn change() -> ChangeDetail {
        let mut d = detail(7, 3, 3);
        d.summary.owner = acct(1, "alice");
        d.reviewers = vec![acct(2, "bob"), acct(3, "carol")];
        d.cc = vec![acct(4, "dave")];
        d
    }

    fn in_set(id: u64, user: &str, reason: Option<&str>) -> AttentionEntry {
        AttentionEntry {
            account: acct(id, user),
            reason: reason.map(str::to_owned),
            last_update: None,
        }
    }

    fn with_labels(d: &mut ChangeDetail) {
        d.permitted_labels = BTreeMap::from([
            ("Verified".to_owned(), vec![-1, 0, 1]),
            ("Code-Review".to_owned(), vec![-2, -1, 0, 1, 2]),
            ("Blocked".to_owned(), vec![]),
        ]);
        d.label_values = BTreeMap::from([(
            "Code-Review".to_owned(),
            vec![
                LabelValue {
                    value: -2,
                    description: "This shall not be merged".into(),
                },
                LabelValue {
                    value: 2,
                    description: "Looks good to me, approved".into(),
                },
            ],
        )]);
        d.votes = BTreeMap::from([(
            "Code-Review".to_owned(),
            vec![
                LabelVote {
                    account: acct(3, "carol"),
                    value: -1,
                    date: None,
                },
                LabelVote {
                    account: acct(2, "bob"),
                    value: 1,
                    date: None,
                },
            ],
        )]);
    }

    #[test]
    fn rows_are_the_permitted_labels_with_the_own_vote_selected() {
        let mut d = change();
        with_labels(&mut d);
        let rows = vote_rows(&d, Some("bob"));
        let names: Vec<_> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(names, ["Code-Review", "Verified"], "no row for Blocked");
        let cr = &rows[0];
        assert_eq!((cr.current, cr.selected), (1, 1));
        let values: Vec<_> = cr.choices.iter().map(|c| c.value).collect();
        assert_eq!(values, [-2, -1, 0, 1, 2]);
        assert_eq!(cr.choices[0].description, "This shall not be merged");
        assert_eq!(cr.choices[1].description, "");
        assert_eq!(rows[1].current, 0);
        // Somebody who has not voted starts at 0; so does an unknown user.
        assert_eq!(vote_rows(&d, Some("erin"))[0].current, 0);
        assert_eq!(vote_rows(&d, None)[0].current, 0);
    }

    #[test]
    fn only_changed_votes_are_sent() {
        let mut d = change();
        with_labels(&mut d);
        let mut rows = vote_rows(&d, Some("bob"));
        assert!(labels_to_send(&rows).is_empty());
        rows[0].selected = 2;
        rows[1].selected = 0; // the same as before
        assert_eq!(
            labels_to_send(&rows),
            BTreeMap::from([("Code-Review".to_owned(), 2)])
        );
        // Taking the vote back is a change too.
        rows[0].selected = 0;
        assert_eq!(
            labels_to_send(&rows),
            BTreeMap::from([("Code-Review".to_owned(), 0)])
        );
    }

    #[test]
    fn anonymous_sessions_have_no_rows() {
        assert!(vote_rows(&change(), None).is_empty());
    }

    fn want(people: &[Person]) -> Vec<(&str, bool)> {
        people
            .iter()
            .map(|p| (p.account.username.as_deref().unwrap_or(""), p.want))
            .collect()
    }

    #[test]
    fn a_reviewer_replying_leaves_and_hands_the_ball_to_the_owner() {
        let mut d = change();
        d.attention = vec![
            in_set(2, "bob", Some("review requested")),
            in_set(3, "carol", None),
        ];
        let p = people(&d, Some("bob"), &[]);
        // bob (you) first, then the owner, then the other reviewer, the CC.
        assert_eq!(
            want(&p),
            [
                ("bob", false),
                ("alice", true),
                ("carol", true),
                ("dave", false)
            ]
        );
        assert_eq!(p[0].roles(), "you, reviewer");
        assert_eq!(p[0].reason.as_deref(), Some("review requested"));
        let (add, remove) = attention_changes(&p, "bob");
        assert_eq!(
            add.iter().map(|a| a.user).collect::<Vec<_>>(),
            [AccountId(1)]
        );
        assert_eq!(
            remove.iter().map(|a| a.user).collect::<Vec<_>>(),
            [AccountId(2)]
        );
        assert_eq!(add[0].reason, "bob replied");
    }

    #[test]
    fn nothing_changes_for_the_owner_already_in_the_set() {
        let mut d = change();
        d.attention = vec![in_set(1, "alice", None)];
        let p = people(&d, Some("bob"), &[]);
        let (add, remove) = attention_changes(&p, "bob");
        assert!(add.is_empty() && remove.is_empty());
        // bob is not in the set, so he is not listed.
        assert!(p.iter().all(|p| !p.me));
    }

    fn answered_by(mut t: Thread, who: Account) -> Thread {
        if let Some(model::Entry::Published(c)) = t.entries.last_mut() {
            c.author = Some(who);
        }
        t
    }

    #[test]
    fn the_owner_replying_calls_the_reviewers_who_wait() {
        let mut d = change();
        d.attention = vec![in_set(1, "alice", None)];
        let line = Location::Line(1);
        let waiting = answered_by(thread("t1", line, "fix", true), acct(2, "bob"));
        let resolved = answered_by(thread("t2", line, "ok", false), acct(3, "carol"));
        let p = people(&d, Some("alice"), &[waiting, resolved]);
        assert_eq!(
            want(&p),
            [
                ("alice", false),
                ("bob", true),
                ("carol", false),
                ("dave", false)
            ]
        );
    }

    #[test]
    fn a_closed_change_only_loses_the_replier() {
        let mut d = change();
        d.summary.status = ChangeStatus::Merged;
        d.attention = vec![in_set(2, "bob", None)];
        let (add, remove) = attention_changes(&people(&d, Some("bob"), &[]), "bob");
        assert!(add.is_empty());
        assert_eq!(remove.len(), 1);
        // WIP: the owner is not called either.
        let mut d = change();
        d.summary.work_in_progress = true;
        let (add, _) = attention_changes(&people(&d, Some("bob"), &[]), "bob");
        assert!(add.is_empty());
    }

    #[test]
    fn people_are_listed_once_even_with_several_roles() {
        let mut d = change();
        d.reviewers.push(acct(1, "alice")); // the owner reviews too
        let p = people(&d, Some("bob"), &[]);
        assert_eq!(p.iter().filter(|p| p.account.id == AccountId(1)).count(), 1);
        let owner = p.iter().find(|p| p.owner).expect("owner");
        assert_eq!(owner.roles(), "owner, reviewer");
    }

    /// A thread with one draft on the patch-set level of patch set `ps`.
    fn ps_level(n: i64, ps: u32, message: &str) -> Thread {
        let mut t = thread(&format!("t{n}"), Location::File, message, true);
        let mut d = draft(n, Location::File, message, DraftSync::Synced);
        d.path = model::PATCHSET_LEVEL_PATH.to_owned();
        d.patch_set = PatchSetNumber(ps);
        t.path = d.path.clone();
        t.patch_set = d.patch_set;
        t.entries = vec![model::Entry::Draft(d)];
        t
    }

    #[test]
    fn the_message_is_the_patchset_draft_that_is_not_a_reply() {
        let current = PatchSetNumber(3);
        assert!(message_draft(&[], current).is_none());
        let old = ps_level(1, 2, "from before");
        let mine = ps_level(2, 3, "Looks good\nmore");
        let threads = [old, mine];
        let found = message_draft(&threads, current).expect("draft");
        assert_eq!(found.message, "Looks good\nmore");
        // A reply is not the message.
        let mut reply = ps_level(3, 3, "re");
        if let Some(model::Entry::Draft(d)) = reply.entries.first_mut() {
            d.in_reply_to = Some("c1".into());
        }
        assert!(message_draft(&[reply], current).is_none());
    }

    #[test]
    fn the_list_has_every_draft_but_the_message_and_marks_other_patch_sets() {
        let current = PatchSetNumber(3);
        let mut inline = thread("t9", Location::Line(12), "x", true);
        let mut d = draft(
            5,
            Location::Line(12),
            "\nFix this\nplease",
            DraftSync::PendingCreate,
        );
        d.patch_set = PatchSetNumber(2);
        d.path = "src/a.rs".into();
        d.last_error = Some("offline".into());
        inline.path = d.path.clone();
        inline.entries = vec![model::Entry::Draft(d)];
        let msg = ps_level(2, 3, "the message");
        let items = draft_items(&[inline, msg], current, Some(LocalDraftId(2)));
        assert_eq!(items.len(), 1);
        let i = &items[0];
        assert_eq!(
            (i.place.as_str(), i.text.as_str()),
            ("src/a.rs:12", "Fix this")
        );
        assert!(i.other_patch_set && i.pending() && !i.failed());
        assert_eq!(i.problem().as_deref(), Some("not uploaded: offline"));
    }

    #[test]
    fn the_focus_starts_on_the_selected_vote_else_zero() {
        let mut d = change();
        with_labels(&mut d);
        let mut rows = vote_rows(&d, Some("bob"));
        let by = |rows: &[VoteRow], l: &str| rows.iter().position(|r| r.label == l).unwrap();
        let (cr, v) = (by(&rows, "Code-Review"), by(&rows, "Verified"));
        // bob has no vote on Verified: 0 is the third of -1 0 +1.
        assert_eq!(rows[v].focus_index(), 1);
        assert_eq!(
            rows[cr].choices[rows[cr].focus_index()].value,
            rows[cr].selected
        );
        // After a pick the focus follows the selection.
        rows[cr].selected = 2;
        assert_eq!(rows[cr].focus_index(), 4);
        // A vote the user may not set (any more): back to 0.
        rows[cr].selected = 7;
        assert_eq!(rows[cr].focus_index(), 2);
        // Without a 0 among the values: the first.
        rows[v].choices.retain(|c| c.value != 0);
        rows[v].selected = 0;
        assert_eq!(rows[v].focus_index(), 0);
    }

    #[test]
    fn the_review_carries_changed_votes_drafts_and_no_message() {
        let mut d = change();
        with_labels(&mut d);
        let mut rows = vote_rows(&d, Some("bob"));
        rows[0].selected = 2;
        let input = review_input(&rows, vec![], vec![]);
        assert_eq!(
            input,
            ReviewInput {
                labels: BTreeMap::from([("Code-Review".to_owned(), 2)]),
                ..ReviewInput::default()
            }
        );
        assert_eq!(input.message, None);
        assert_eq!(input.drafts, model::DraftHandling::PublishAllRevisions);
    }
}
