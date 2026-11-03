//! Porting: where each draft of a local version that never reached Gerrit
//! goes on a Gerrit patch set, or why it is held.
//!
//! The rules (a user decision, and the plan's "Porting"):
//!
//! - A comment ports only where its lines are unchanged: an unchanged line
//!   maps 1:1 (shifted by what was inserted or deleted before it), a range
//!   ports when every line it touches is unchanged and nothing was inserted
//!   inside. Renames are followed.
//! - A changed or deleted line, a range crossing a change, a deleted file
//!   or a binary file that changed is held.
//! - A comment on a file as a whole ports while the file is there. One on
//!   a file the local version deletes ports when the target deletes it too.
//! - `/PATCHSET_LEVEL` always ports; `/COMMIT_MSG` maps through the two
//!   commits' synthetic texts, as Gerrit builds them.
//! - A comment on the parent side ports only when that side is unchanged:
//!   the two parents have the same content for the file.
//! - Gerrit takes a comment only on a file that its patch set changes (or a
//!   magic file such as `/COMMIT_MSG`) and refuses the whole post for any
//!   other, so a comment whose file the target does not change is held.
//! - What the checkout cannot read (a commit removed by `git gc`, a file
//!   whose content a partial clone lacks) is held, with the reason.

use model::{
    COMMIT_MSG_PATH, Draft, FileEntry, FileStatus, Location, PATCHSET_LEVEL_PATH, PatchSetNumber,
    Side,
};
use store::DraftAnchor;

use crate::port::{NotLocalPath, PathMap, PortMaps};

/// The checkout's answer for the drafts of one side of a port: the maps, or
/// why the drafts on that side are held.
pub(super) type SideMaps = Result<PortMaps, String>;

/// Where draft `d` goes on patch set `ps`, whose files (against its parent,
/// as Gerrit lists them) are `files`, given the maps of the revision side
/// (`revision`) and of the parent side (`parent`); else why it is held.
pub(super) fn port_draft(
    d: &Draft,
    ps: PatchSetNumber,
    files: &[FileEntry],
    revision: &SideMaps,
    parent: &SideMaps,
) -> Result<DraftAnchor, String> {
    let at = |path: &str, location: Location| DraftAnchor {
        path: path.to_owned(),
        side: d.side,
        parent: d.parent,
        location,
    };
    if d.path == PATCHSET_LEVEL_PATH {
        return Ok(at(&d.path, d.location));
    }
    let ps = ps.0;
    match d.side {
        Side::Parent => {
            if let Some(p) = d.parent.filter(|p| *p > 1) {
                return Err(format!(
                    "Comments on parent {p} of a merge commit are not carried over."
                ));
            }
            if d.path == COMMIT_MSG_PATH {
                return Err(
                    "Comments on the base of the commit message are not carried over.".to_owned(),
                );
            }
            let maps = parent.as_ref().map_err(Clone::clone)?;
            let same = match maps.get(&d.path) {
                Some(PathMap::Lines(m)) => {
                    m.is_identity() && m.target_path() == Some(d.path.as_str())
                }
                Some(PathMap::NotLocal(why)) => return Err(not_local(&d.path, why)),
                None => false,
            };
            if !same {
                return Err(format!(
                    "The base of {} differs in PS {ps}, and a comment on the base side is \
                     carried over only when it is the same.",
                    d.path
                ));
            }
            // The base side of the file in the target: the file must be in
            // its list, under the same name (not renamed or copied there).
            match files.iter().find(|f| f.path == d.path) {
                Some(f) if f.old_path.is_none() => Ok(at(&d.path, d.location)),
                Some(_) => Err(format!(
                    "{} is renamed in PS {ps}, so its base side is another file.",
                    d.path
                )),
                None => Err(unchanged(&d.path, ps)),
            }
        }
        Side::Revision => {
            let maps = revision.as_ref().map_err(Clone::clone)?;
            match maps.get(&d.path) {
                Some(PathMap::Lines(m)) => {
                    let Some(target) = m.target_path() else {
                        return Err(format!("{} is deleted in PS {ps}.", d.path));
                    };
                    let Some(location) = m.map_location(d.location) else {
                        return Err(if m.is_opaque() {
                            format!("{} is a binary file that changed in PS {ps}.", d.path)
                        } else {
                            match d.location {
                                Location::Line(l) => {
                                    format!("Line {l} of {} changed in PS {ps}.", d.path)
                                }
                                _ => {
                                    format!("The commented lines of {} changed in PS {ps}.", d.path)
                                }
                            }
                        });
                    };
                    if target == COMMIT_MSG_PATH {
                        return Ok(at(target, location));
                    }
                    match files.iter().find(|f| f.path == target) {
                        Some(f) if f.status != FileStatus::Deleted => Ok(at(target, location)),
                        Some(_) => Err(format!("{target} is deleted in PS {ps}.")),
                        None => Err(unchanged(target, ps)),
                    }
                }
                Some(PathMap::NotLocal(NotLocalPath::NotAFile)) => {
                    // Deleted by the local version: a comment on the file as
                    // a whole goes where the target deletes it too.
                    let deleted_there = files
                        .iter()
                        .any(|f| f.path == d.path && f.status == FileStatus::Deleted);
                    if d.location == Location::File && deleted_there {
                        Ok(at(&d.path, d.location))
                    } else if d.location == Location::File {
                        Err(format!(
                            "{} is not a file in the local version, and PS {ps} does not \
                             delete it.",
                            d.path
                        ))
                    } else {
                        Err(format!("{} is not a file in the local version.", d.path))
                    }
                }
                Some(PathMap::NotLocal(why)) => Err(not_local(&d.path, why)),
                None => Err(format!(
                    "The checkout did not say where {} went in PS {ps}.",
                    d.path
                )),
            }
        }
    }
}

/// Why a comment on `path` is held when the checkout cannot map it.
fn not_local(path: &str, why: &NotLocalPath) -> String {
    match why {
        NotLocalPath::NotAFile => format!("{path} is not a file in the local version."),
        NotLocalPath::MissingContent => {
            format!("The content of {path} is not in the checkout (a partial clone?).")
        }
        NotLocalPath::Other(text) => format!("{}.", capitalized(text.trim_end_matches('.'))),
    }
}

/// Why a comment on `path` is held when patch set `ps` does not change the
/// file: Gerrit would refuse the whole post.
fn unchanged(path: &str, ps: u32) -> String {
    format!("PS {ps} does not change {path}, and Gerrit takes comments only on changed files.")
}

fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
