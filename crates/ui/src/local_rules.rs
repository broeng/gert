//! What the pages may do on a local change or a local version, in one
//! place. There are three rules, one per kind of request:
//!
//! * **Actions** (the action menu) are requests about the change, so only
//!   a local *change* refuses them; a Gerrit change shown at a local
//!   patch set still has its menu.
//! * A **reply** or quick vote on a local change or a local target becomes
//!   a local review of that version (never a reply to Gerrit, which would
//!   take it for its current patch set). The base does not matter.
//! * **Reviewed marks** are Gerrit's, per comparison: refused when the
//!   change, the target or the base is local (as
//!   [`FilesKey::is_local`](model::FilesKey::is_local)).

use model::{ChangeNumber, PatchSetNumber};

use crate::{action_menu, reply, reviewed, session::Session};

/// A kind of request a page makes about what it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Actions,
    Reply,
    Reviewed,
}

/// What a page shows: the change and, once known, the patch set pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub change: ChangeNumber,
    pub target: Option<PatchSetNumber>,
    /// `None` also for the parent commit.
    pub base: Option<PatchSetNumber>,
}

impl Target {
    /// Whether `op` concerns something only the checkout has (see the
    /// module comment).
    pub fn is_local_for(&self, op: Op) -> bool {
        let target = self.target.is_some_and(PatchSetNumber::is_local);
        let base = self.base.is_some_and(PatchSetNumber::is_local);
        match op {
            Op::Actions => self.change.is_local(),
            Op::Reply => self.change.is_local() || target,
            Op::Reviewed => self.change.is_local() || target || base,
        }
    }
}

/// Why `op` cannot be done now, as a note for the user: a local target
/// first (except for a reply, which becomes a local review), then the
/// session.
pub fn refusal(op: Op, target: &Target, session: &Session) -> Option<&'static str> {
    let local = target.is_local_for(op);
    match op {
        Op::Actions if local => Some(action_menu::LOCAL),
        Op::Actions => action_menu::unavailable(session),
        Op::Reply => reply::unavailable(session),
        Op::Reviewed if local => Some(reviewed::LOCAL),
        Op::Reviewed => reviewed::unavailable(session),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{URL, env_with, setup};

    fn target(change: ChangeNumber, target: u32, base: Option<u32>) -> Target {
        Target {
            change,
            target: Some(PatchSetNumber(target)),
            base: base.map(PatchSetNumber),
        }
    }

    #[test]
    fn a_gerrit_change_at_a_local_version_keeps_its_menu_but_not_its_marks() {
        let local = PatchSetNumber::LOCAL_BASE + 1;
        let t = target(ChangeNumber(7), local, None);
        assert!(!t.is_local_for(Op::Actions));
        assert!(t.is_local_for(Op::Reply));
        assert!(t.is_local_for(Op::Reviewed));
    }

    #[test]
    fn a_local_base_refuses_only_the_marks() {
        let local = PatchSetNumber::LOCAL_BASE + 1;
        let t = target(ChangeNumber(7), 2, Some(local));
        assert!(!t.is_local_for(Op::Actions));
        assert!(!t.is_local_for(Op::Reply));
        assert!(t.is_local_for(Op::Reviewed));
    }

    #[test]
    fn a_local_change_is_local_for_everything_also_before_its_detail() {
        let t = Target {
            change: ChangeNumber(ChangeNumber::LOCAL_BASE + 1),
            target: None,
            base: None,
        };
        for op in [Op::Actions, Op::Reply, Op::Reviewed] {
            assert!(t.is_local_for(op), "{op:?}");
        }
    }

    #[test]
    fn a_local_target_is_refused_before_the_session_is_asked() {
        let mut store = store::Store::open_in_memory().expect("store");
        store.server_id(URL).expect("server");
        let http = gerrit_api::fake::RouteHttpClient::new();
        let signed_out = Session::new(&env_with(store, &http), &setup(), None).expect("session");
        let local = PatchSetNumber::LOCAL_BASE + 1;
        let t = target(ChangeNumber(7), local, None);
        assert_eq!(
            refusal(Op::Reviewed, &t, &signed_out),
            Some(reviewed::LOCAL)
        );
        // A reply on it is a local review: only the session can refuse it.
        assert_eq!(
            refusal(Op::Reply, &t, &signed_out),
            reply::unavailable(&signed_out)
        );
        assert!(refusal(Op::Actions, &t, &signed_out).is_some());
        assert_ne!(
            refusal(Op::Actions, &t, &signed_out),
            Some(action_menu::LOCAL)
        );
    }

    #[test]
    fn a_gerrit_comparison_is_local_for_nothing() {
        let t = target(ChangeNumber(7), 3, Some(1));
        for op in [Op::Actions, Op::Reply, Op::Reviewed] {
            assert!(!t.is_local_for(op), "{op:?}");
        }
    }
}
