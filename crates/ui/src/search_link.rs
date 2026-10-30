//! Names that lead to a search: a project or a person in a change list or on
//! the change page turns into a link to the changes of that project or person.
//!
//! The query builders are pure; [`search_link`] is the clickable text.

use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;
use model::{Account, GitPerson};

/// `op:value`, with the value quoted when the query syntax would otherwise
/// split or misread it.
fn term(op: &str, value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '(' | ')' | '\\'));
    if plain {
        format!("{op}:{value}")
    } else {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{op}:\"{escaped}\"")
    }
}

/// The changes of a project.
pub fn project_query(project: &str) -> String {
    term("project", project)
}

/// The changes `account` relates to through `op` (`owner`, `reviewer`, `cc`),
/// named by the most precise handle the server gave: username, email, or the
/// account id.
pub fn account_query(op: &str, account: &Account) -> String {
    let handle = [&account.username, &account.email]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| account.id.to_string());
    term(op, &handle)
}

/// The changes whose commit has `person` as `op` (`author`, `committer`):
/// git people have no account, so the email (or else the name) is matched.
pub fn person_query(op: &str, person: &GitPerson) -> Option<String> {
    [&person.email, &person.name]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
        .map(|handle| term(op, handle))
}

/// `label` as text that underlines on hover and runs `go` on a click, without
/// the click reaching an enclosing clickable row.
pub fn search_link(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
    go: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    // The row keeps the link as wide as its text, so the rest of the cell
    // still belongs to the enclosing row.
    h_flex().min_w_0().child(
        div()
            .id(id)
            .min_w_0()
            .truncate()
            .cursor_pointer()
            .hover(|d| d.underline().text_color(cx.theme().link))
            .on_click(move |ev, window, cx| {
                cx.stop_propagation();
                go(ev, window, cx);
            })
            .child(label.into()),
    )
}

#[cfg(test)]
mod tests {
    use model::{Account, AccountId, GitPerson};

    use super::{account_query, person_query, project_query};

    fn account(id: u64) -> Account {
        Account {
            id: AccountId(id),
            name: Some("Alice A".into()),
            email: None,
            username: None,
            display_name: None,
        }
    }

    #[test]
    fn project_names_are_quoted_only_when_needed() {
        assert_eq!(project_query("tools/gert"), "project:tools/gert");
        assert_eq!(project_query("my project"), "project:\"my project\"");
        assert_eq!(project_query("a\"b"), "project:\"a\\\"b\"");
    }

    #[test]
    fn accounts_prefer_username_then_email_then_id() {
        let mut a = account(7);
        assert_eq!(account_query("owner", &a), "owner:7");
        a.email = Some("a@example.com".into());
        assert_eq!(account_query("reviewer", &a), "reviewer:a@example.com");
        a.username = Some("alice".into());
        assert_eq!(account_query("cc", &a), "cc:alice");
        a.username = Some("  ".into());
        assert_eq!(account_query("owner", &a), "owner:a@example.com");
    }

    #[test]
    fn git_people_match_by_email_then_name() {
        let mut p = GitPerson {
            name: Some("Alice A".into()),
            email: None,
            date: None,
            tz: None,
        };
        assert_eq!(person_query("author", &p).unwrap(), "author:\"Alice A\"");
        p.email = Some("a@example.com".into());
        assert_eq!(
            person_query("committer", &p).unwrap(),
            "committer:a@example.com"
        );
        p.name = None;
        p.email = None;
        assert_eq!(person_query("author", &p), None);
    }
}
