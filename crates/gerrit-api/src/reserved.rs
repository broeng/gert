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
//! - the search `q`: `change:N` terms and bare numbers (which Gerrit reads
//!   as change numbers), refused above `i32::MAX`. Other operators that take
//!   numbers are not built from change numbers by this client.
//!
//! [`ChangeNumber::LOCAL_BASE`]: model::ChangeNumber::LOCAL_BASE

use model::PatchSetNumber;

/// The first reserved number in the request target `target` (a path with
/// an optional `?query`, percent-encoded as sent), described for an error
/// message ("change 281474976710657"); `None` if there is none.
pub(crate) fn local_number_in(target: &str) -> Option<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let segments: Vec<String> = path.split('/').map(|s| decode(s, false)).collect();
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
            "q" => local_number_in_query(&value),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// The first reserved change number in the Gerrit search `query`: a
/// `change:N` term or a bare number, also negated or in parentheses.
fn local_number_in_query(query: &str) -> Option<String> {
    query
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .find_map(|term| {
            let term = term.trim_start_matches(['-', '!']);
            let value = match term.split_once(':') {
                Some((op, value)) if op.eq_ignore_ascii_case("change") => value,
                Some(_) => return None,
                None => term,
            };
            let value = value.trim_matches(['"', '{', '}']);
            // `project~branch~number` names a change by number too.
            local_change(value.rsplit('~').next().unwrap_or_default())
        })
}

/// `"change N"` if `id` is a number no Gerrit change can have: one that
/// fits a [`model::ChangeNumber`] but not Gerrit's `int`. Longer runs of
/// digits are not numbers this client builds.
pub(crate) fn local_change(id: &str) -> Option<String> {
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
    use model::ChangeNumber;

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
        assert_eq!(local_number_in(&q(&format!("message:{LOCAL}"))), None);
        assert_eq!(local_number_in(&q("change:I0123abcd project:p")), None);
        // Only `q` is a query.
        assert_eq!(
            local_number_in(&format!("/changes/5/suggest_reviewers?n={LOCAL}")),
            None
        );
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
