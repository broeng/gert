//! Keeps the numbers of local changes off the wire.
//!
//! Changes and patch sets that exist only in a local checkout get numbers
//! from reserved ranges ([`ChangeNumber::LOCAL_BASE`],
//! [`PatchSetNumber::LOCAL_BASE`]), so the pages, the cache and the drafts
//! can treat them like any other. Such a number must never reach a server:
//! there it would name nothing, or, in a query, something else. The client
//! looks for them in every request target it builds and refuses the request
//! ([`crate::Error::LocalNumber`]); the fake HTTP clients panic on one, so a
//! test that lets one through fails loudly.
//!
//! The check reads the request target, not the typed arguments, so that
//! every method is covered by one place, also future ones. It looks where
//! the client puts numbers:
//!
//! - the change id after `/changes/` (also the number of a
//!   `project~number` id): refused above `i32::MAX`, which no Gerrit change
//!   number can be;
//! - a numeric revision after `/revisions/` (up to 10 digits; longer is a
//!   SHA without letters), and the `base` parameter (the files and diff
//!   endpoints): patch set numbers, refused at or above
//!   [`PatchSetNumber::LOCAL_BASE`];
//! - the search `q` of the change query (`/changes/?q=`): `change:N` terms
//!   (also `project~branch~N` and a quoted value), as the client builds
//!   them for the relation chain, refused above `i32::MAX`; bare numbers
//!   only from [`ChangeNumber::LOCAL_BASE`] on, since a user's search may
//!   hold any long number. Quoted (`"…"`) and braced (`{…}`) text is the
//!   value of some other operator and is skipped, and so are other
//!   operators that take numbers: the client builds none of them from
//!   change numbers, and local numbers never reach text the user types
//!   (they are never displayed).
//!
//! The rebase `base` in a request body is checked by [`local_rebase_base`].
//!
//! [`ChangeNumber::LOCAL_BASE`]: model::ChangeNumber::LOCAL_BASE

use model::{ChangeNumber, PatchSetNumber};

/// The first reserved number in the request target `target` (a path with
/// an optional `?query`, percent-encoded as sent), described for an error
/// message ("change 281474976710657"); `None` if there is none.
pub(crate) fn local_number_in(target: &str) -> Option<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let segments: Vec<String> = path.split('/').map(|s| decode(s, false)).collect();
    // `q` is a change search only on the collection; elsewhere (the
    // reviewer suggestions) it is free text.
    let change_query = segments
        .iter()
        .rev()
        .find(|s| !s.is_empty())
        .is_some_and(|s| s == "changes");
    for pair in segments.windows(2) {
        let found = match pair[0].as_str() {
            "changes" => {
                let id = pair[1].rsplit('~').next().unwrap_or_default();
                local_change(id)
            }
            "revisions" => local_patch_set(&pair[1]),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    for param in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = param.split_once('=').unwrap_or((param, ""));
        let value = decode(value, true);
        let found = match decode(key, true).as_str() {
            "base" => local_patch_set(&value),
            "q" if change_query => local_number_in_query(&value),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// The first reserved change number in the Gerrit search `query`: a
/// `change:N` term (also negated, in parentheses, or with a quoted value)
/// or a bare number from the local range. See the module comment.
fn local_number_in_query(query: &str) -> Option<String> {
    terms(query).into_iter().find_map(|(text, quoted)| {
        let text = text.trim_start_matches(['-', '!']);
        match text.split_once(':') {
            Some((op, value)) if op.eq_ignore_ascii_case("change") => {
                let value = if value.is_empty() {
                    quoted.as_deref().unwrap_or_default()
                } else {
                    value
                };
                // `project~branch~number` names a change by number too.
                local_change(value.rsplit('~').next().unwrap_or_default())
            }
            Some(_) => None,
            None if quoted.is_some() => None,
            None => in_local_range(text),
        }
    })
}

/// The terms of a Gerrit search: split on blanks and parentheses outside
/// quotes and braces. Each is its unquoted text and the content of its
/// (last) quoted or braced part, if it has one.
fn terms(query: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut quoted: Option<String> = None;
    let mut chars = query.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() || c == '(' || c == ')' => {
                if !text.is_empty() || quoted.is_some() {
                    out.push((std::mem::take(&mut text), quoted.take()));
                }
            }
            '"' | '{' => {
                let close = if c == '"' { '"' } else { '}' };
                let mut inner = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' if close == '"' => inner.extend(chars.next()),
                        c if c == close => break,
                        c => inner.push(c),
                    }
                }
                quoted = Some(inner);
            }
            c => text.push(c),
        }
    }
    if !text.is_empty() || quoted.is_some() {
        out.push((text, quoted));
    }
    out
}

/// `"change N"` if `id` is a number in the local change range.
fn in_local_range(id: &str) -> Option<String> {
    let local = is_number(id)
        && id
            .parse::<u64>()
            .is_ok_and(|n| n >= ChangeNumber::LOCAL_BASE);
    local.then(|| format!("change {id}"))
}

/// The local number named by the rebase `base` (a commit SHA, a change
/// number, or `refs/changes/NN/<change>/<patch set>`), if it names one. A
/// bare run of digits counts only from [`ChangeNumber::LOCAL_BASE`] on:
/// a shorter one may be a SHA prefix the user typed.
pub(crate) fn local_rebase_base(base: &str) -> Option<String> {
    if let Some(rest) = base.strip_prefix("refs/changes/") {
        let mut parts = rest.split('/').skip(1);
        let change = parts.next().and_then(local_change);
        return change.or_else(|| parts.next().and_then(local_patch_set));
    }
    in_local_range(base)
}

/// `"change N"` if `id` is a number no Gerrit change can have: one that
/// fits a [`model::ChangeNumber`] but not Gerrit's `int`. Longer runs of
/// digits are not numbers this client builds.
fn local_change(id: &str) -> Option<String> {
    let reserved = is_number(id) && id.parse::<u64>().is_ok_and(|n| n > i32::MAX as u64);
    reserved.then(|| format!("change {id}"))
}

/// `"patch set N"` if `id` is a number from the local patch set range. At
/// most 10 digits, the width of a [`PatchSetNumber`]: a longer run of digits
/// is a commit SHA (or a prefix of one) that happens to have no letters.
fn local_patch_set(id: &str) -> Option<String> {
    let reserved = is_number(id)
        && id.len() <= 10
        && id
            .parse::<u64>()
            .is_ok_and(|n| n >= u64::from(PatchSetNumber::LOCAL_BASE));
    reserved.then(|| format!("patch set {id}"))
}

fn is_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Percent-decodes `s` (lossy for invalid UTF-8); in a query, `+` is a
/// space.
fn decode(s: &str, query: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' if query => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: u64 = ChangeNumber::LOCAL_BASE + 1;
    const LOCAL_PS: u32 = PatchSetNumber::LOCAL_BASE;

    #[test]
    fn change_ids_in_paths() {
        assert_eq!(
            local_number_in(&format!("/a/changes/{LOCAL}/comments")),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_number_in(&format!("/changes/demo%2Fx~{LOCAL}")),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_number_in("/changes/2147483648"),
            Some("change 2147483648".into())
        );
        assert_eq!(local_number_in("/changes/99999999999999999999999"), None);
        assert_eq!(local_number_in("/changes/2147483647/detail"), None);
        assert_eq!(local_number_in("/changes/demo%2Fx~5"), None);
        assert_eq!(local_number_in("/changes/I0123abc"), None);
        // A number elsewhere in the path is not a change id.
        assert_eq!(local_number_in(&format!("/accounts/{LOCAL}/detail")), None);
    }

    #[test]
    fn patch_sets_in_revisions_and_base() {
        assert_eq!(
            local_number_in(&format!("/changes/5/revisions/{LOCAL_PS}/files")),
            Some(format!("patch set {LOCAL_PS}"))
        );
        assert_eq!(
            local_number_in(&format!("/changes/5/revisions/abc/files?base={LOCAL_PS}")),
            Some(format!("patch set {LOCAL_PS}"))
        );
        assert_eq!(
            local_number_in(&format!(
                "/changes/5/revisions/abc/files/a%2Fb/diff?intraline&base={LOCAL_PS}&whitespace=X"
            )),
            Some(format!("patch set {LOCAL_PS}"))
        );
        let below = LOCAL_PS - 1;
        assert_eq!(
            local_number_in(&format!("/changes/5/revisions/{below}/files?base={below}")),
            None
        );
        assert_eq!(
            local_number_in("/changes/5/revisions/0123456789abcdef/files?base=3"),
            None
        );
        // A SHA without letters is not a patch set number.
        assert_eq!(
            local_number_in("/changes/5/revisions/0123456789012345678901234567890123456789/files"),
            None
        );
        assert_eq!(
            local_number_in("/changes/5/revisions/current/mergeable"),
            None
        );
    }

    #[test]
    fn change_numbers_in_queries() {
        let q = |s: &str| format!("/changes/?q={}&o=LABELS", crate::query::encode_component(s));
        assert_eq!(
            local_number_in(&q(&format!("change:5 OR change:{LOCAL}"))),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_number_in(&q(&format!("is:open ({LOCAL})"))),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_number_in(&q(&format!("-CHANGE:\"{LOCAL}\""))),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_number_in(&format!("/changes/?q=change%3A{LOCAL}+is%3Aopen")),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_number_in(&q(&format!("change:demo%2Fx~main~{LOCAL}"))),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(local_number_in(&q("change:5 OR change:2147483647")), None);
        assert_eq!(
            local_number_in(&q(&format!("message:\"x\" change:\"{LOCAL}\""))),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_number_in(&q(&format!("{LOCAL}"))),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(local_number_in(&q(&format!("message:{LOCAL}"))), None);
        assert_eq!(local_number_in(&q("change:I0123abcd project:p")), None);
        // Only `q` is a query, and only on the change collection.
        assert_eq!(
            local_number_in(&format!("/changes/5/suggest_reviewers?n={LOCAL}")),
            None
        );
        assert_eq!(
            local_number_in(&format!("/a/changes/5/suggest_reviewers?q={LOCAL}&n=5")),
            None
        );
        assert_eq!(
            local_number_in(&format!("/r/a/changes/?q={LOCAL}")),
            Some(format!("change {LOCAL}"))
        );
    }

    /// What users write in searches is theirs: long numbers in it are not
    /// change numbers, quoted or braced text is not looked at.
    #[test]
    fn ordinary_searches_with_long_numbers_are_sent() {
        let q = |s: &str| format!("/changes/?q={}", crate::query::encode_component(s));
        for search in [
            "message:\"fixes bug 12345678901\"",
            "message:{build 20231102123456}",
            "owner:self 1699999999999",
            "message:\"see change:99999999999\"",
            "message:\"a \\\" change:99999999999\"",
            "(is:open OR 99999999999) -2147483648",
        ] {
            assert_eq!(local_number_in(&q(search)), None, "{search}");
        }
        // A `change:` term with a number no Gerrit change can have still is
        // refused.
        assert_eq!(
            local_number_in(&q("change:99999999999")),
            Some("change 99999999999".into())
        );
    }

    #[test]
    fn rebase_bases() {
        assert_eq!(
            local_rebase_base(&LOCAL.to_string()),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_rebase_base(&format!("refs/changes/01/{LOCAL}/1")),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(
            local_rebase_base(&format!("refs/changes/05/5/{LOCAL_PS}")),
            Some(format!("patch set {LOCAL_PS}"))
        );
        // A SHA prefix of digits, a real change, a real ref, a SHA.
        assert_eq!(local_rebase_base("12345678901234"), None);
        assert_eq!(local_rebase_base("5"), None);
        assert_eq!(local_rebase_base("refs/changes/05/5/2"), None);
        assert_eq!(local_rebase_base("0123abcd"), None);
    }

    #[test]
    fn an_encoded_tilde_still_separates_the_number() {
        assert_eq!(
            local_number_in(&format!("/changes/demo%2Fx%7E{LOCAL}/detail")),
            Some(format!("change {LOCAL}"))
        );
        assert_eq!(local_number_in("/changes/demo%2Fx%7E5/detail"), None);
    }

    #[test]
    fn decoding_tolerates_stray_percent_signs() {
        assert_eq!(decode("a%2Fb", false), "a/b");
        assert_eq!(decode("a%", false), "a%");
        assert_eq!(decode("a%2", false), "a%2");
        assert_eq!(decode("%zz+", true), "%zz ");
        assert_eq!(decode("+", false), "+");
    }
}
