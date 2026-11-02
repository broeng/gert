//! Commits on local branches: discovery of unpushed commits for local
//! changes, where they are headed, and full commit data for a change page.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use model::{BranchTargets, CommitInfo, LocalCommit, RevisionId, Signature, Timestamp, Upstream};

use crate::git::{self, Object};
use crate::{Error, Local, NotLocal, Repo, is_object_id};

/// One record per ref, fields separated by NUL. Ref names cannot contain
/// control characters, so the newline between records is safe.
const REF_FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(symref)%00\
                          %(upstream:remotename)%00%(upstream:remoteref)";

const HEADS: &str = "refs/heads/";
const REMOTES: &str = "refs/remotes/";

/// One line of [`REF_FORMAT`] output.
struct RefRecord {
    name: String,
    oid: String,
    /// The target of a symbolic ref (`refs/remotes/origin/HEAD`).
    symref: String,
    /// `branch.<b>.remote` of a local branch; `.` for a local upstream.
    upstream_remote: String,
    /// `branch.<b>.merge` of a local branch (`refs/heads/main`).
    upstream_ref: String,
}

impl Repo {
    /// The commits reachable from a named local branch (`refs/heads/*`) and
    /// from no remote-tracking branch (`refs/remotes/*`), newest first in
    /// topological order (no parent before its children), at most `limit`.
    ///
    /// A detached HEAD does not count. Merge commits are included (see
    /// [`LocalCommit::is_merge`]); nothing is filtered by author.
    pub fn local_commits(&self, limit: usize) -> Result<Vec<LocalCommit>, Error> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let max = format!("--max-count={limit}");
        let out = self.git.run(
            &[
                "rev-list",
                "--topo-order",
                &max,
                "--branches",
                "--not",
                "--remotes",
            ],
            None,
        )?;
        let shas: Vec<String> = String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| is_object_id(l))
            .map(str::to_owned)
            .collect();
        let objects = self.git.read_objects(&shas)?;
        let mut commits: Vec<LocalCommit> = Vec::with_capacity(shas.len());
        for (sha, object) in shas.iter().zip(objects) {
            match object {
                Object::Found { kind, data, .. } if kind == "commit" => commits.push(LocalCommit {
                    commit: parse_commit(sha, &data),
                    branches: Vec::new(),
                }),
                _ => {
                    return Err(Error::Parse {
                        command: "cat-file --batch",
                        detail: format!("rev-list listed {sha}, which is not a commit"),
                    });
                }
            }
        }
        self.fill_branches(&mut commits)?;
        Ok(commits)
    }

    /// Sets [`LocalCommit::branches`] by walking parents from each branch
    /// tip inside the listed commits, without asking git again.
    ///
    /// That is exact: a branch whose tip is not listed is either on a
    /// remote (and so is everything it reaches) or was cut off by the
    /// limit, and topological order puts all its ancestors after it, so
    /// they were cut off too. For the same reason a walk never has to
    /// step through an unlisted commit to reach a listed one.
    fn fill_branches(&self, commits: &mut [LocalCommit]) -> Result<(), Error> {
        let index: HashMap<String, usize> = commits
            .iter()
            .enumerate()
            .map(|(i, c)| (c.commit.sha.0.clone(), i))
            .collect();
        for record in self.refs()? {
            let Some(branch) = record.name.strip_prefix(HEADS) else {
                continue;
            };
            let Some(&tip) = index.get(&record.oid) else {
                continue;
            };
            let mut seen = HashSet::from([tip]);
            let mut stack = vec![tip];
            while let Some(i) = stack.pop() {
                commits[i].branches.push(branch.to_owned());
                for parent in &commits[i].commit.parents {
                    if let Some(&p) = index.get(parent.as_str())
                        && seen.insert(p)
                    {
                        stack.push(p);
                    }
                }
            }
        }
        for c in commits {
            c.branches.sort();
        }
        Ok(())
    }

    /// Each local branch's upstream branch on its remote
    /// (`branch.<b>.remote` and `branch.<b>.merge`), leaving out upstreams
    /// that are local branches, and each remote's default branch from
    /// `refs/remotes/<remote>/HEAD` where that exists.
    pub fn branch_targets(&self) -> Result<BranchTargets, Error> {
        let mut targets = BranchTargets::default();
        for record in self.refs()? {
            if let Some(branch) = record.name.strip_prefix(HEADS) {
                let remote = record.upstream_remote;
                if remote.is_empty() || remote == "." {
                    continue;
                }
                let upstream = match record.upstream_ref.strip_prefix(HEADS) {
                    Some(b) => b,
                    // Some other ref namespace: not a branch to target.
                    None if record.upstream_ref.starts_with("refs/") => continue,
                    // A short name, which git also accepts in the config.
                    None => record.upstream_ref.as_str(),
                };
                if upstream.is_empty() {
                    continue;
                }
                targets.upstreams.insert(
                    branch.to_owned(),
                    Upstream {
                        remote,
                        branch: upstream.to_owned(),
                    },
                );
            } else if let Some(remote) = record
                .name
                .strip_prefix(REMOTES)
                .and_then(|r| r.strip_suffix("/HEAD"))
                && let Some(default) = record
                    .symref
                    .strip_prefix(REMOTES)
                    .and_then(|s| s.strip_prefix(remote))
                    .and_then(|s| s.strip_prefix('/'))
                && !default.is_empty()
            {
                targets
                    .defaults
                    .insert(remote.to_owned(), default.to_owned());
            }
        }
        Ok(targets)
    }

    /// The commit object `sha`: message, author, committer and parents as
    /// stored (raw parents, so a shallow checkout does not hide them).
    /// [`NotLocal::MissingCommit`] when the checkout has no such commit.
    pub fn commit(&self, sha: &RevisionId) -> Result<Local<CommitInfo>, Error> {
        let missing = || Local::NotLocal(NotLocal::MissingCommit(sha.as_str().to_owned()));
        if !is_object_id(sha.as_str()) {
            return Ok(missing());
        }
        let objects = self.git.read_objects(&[sha.as_str().to_owned()])?;
        match objects.into_iter().next() {
            Some(Object::Found { kind, data, .. }) if kind == "commit" => {
                Ok(Local::Available(parse_commit(sha.as_str(), &data)))
            }
            _ => Ok(missing()),
        }
    }

    /// `user.email` as git sees it in this repository (repository, global
    /// and system configuration, includes followed), or `None` if unset.
    pub fn user_email(&self) -> Result<Option<String>, Error> {
        let args = ["config", "--get", "user.email"];
        let out = self.git.run_raw(&args, None)?;
        match out.status.code() {
            Some(0) => {
                let email = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                Ok((!email.is_empty()).then_some(email))
            }
            // 1: the key is not set.
            Some(1) => Ok(None),
            _ => Err(git::failure(&args, &out)),
        }
    }

    /// A hash of every local and remote-tracking branch (name, commit,
    /// symbolic target and upstream), so a scan can be skipped when none of
    /// them changed. Stable within one process run only.
    pub fn refs_fingerprint(&self) -> Result<u64, Error> {
        let out = self.refs_raw()?;
        let mut hasher = DefaultHasher::new();
        out.hash(&mut hasher);
        Ok(hasher.finish())
    }

    fn refs_raw(&self) -> Result<Vec<u8>, Error> {
        self.git
            .run(&["for-each-ref", REF_FORMAT, HEADS, REMOTES], None)
    }

    fn refs(&self) -> Result<Vec<RefRecord>, Error> {
        let out = self.refs_raw()?;
        let mut records = Vec::new();
        for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let fields: Vec<String> = line
                .split(|&b| b == 0)
                .map(|f| String::from_utf8_lossy(f).into_owned())
                .collect();
            let Ok([name, oid, symref, upstream_remote, upstream_ref]) =
                <[String; 5]>::try_from(fields)
            else {
                return Err(Error::Parse {
                    command: "for-each-ref",
                    detail: format!("unrecognised line {:?}", String::from_utf8_lossy(line)),
                });
            };
            records.push(RefRecord {
                name,
                oid,
                symref,
                upstream_remote,
                upstream_ref,
            });
        }
        Ok(records)
    }
}

/// Parses a raw commit object. The message is read as UTF-8, lossily (the
/// `encoding` header is ignored); malformed identities give empty fields
/// and the Unix epoch rather than an error.
fn parse_commit(sha: &str, raw: &[u8]) -> CommitInfo {
    let text = String::from_utf8_lossy(raw);
    let (headers, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let mut parents = Vec::new();
    let (mut author, mut committer) = (None, None);
    for line in headers.lines() {
        if let Some(p) = line.strip_prefix("parent ") {
            parents.push(RevisionId(p.to_owned()));
        } else if let Some(a) = line.strip_prefix("author ") {
            author = Some(parse_signature(a));
        } else if let Some(c) = line.strip_prefix("committer ") {
            committer = Some(parse_signature(c));
        }
    }
    let author = author.unwrap_or_else(|| parse_signature(""));
    let committer = committer.unwrap_or_else(|| parse_signature(""));
    CommitInfo {
        sha: RevisionId(sha.to_owned()),
        parents,
        author,
        committer,
        change_id: change_id_trailer(message),
        message: message.to_owned(),
    }
}

/// Parses `Name <email> <unix seconds> <+hhmm>`.
fn parse_signature(s: &str) -> Signature {
    let (name, rest) = s.split_once('<').unwrap_or((s, ""));
    let email = rest.split_once('>').map_or(rest, |(e, _)| e);
    let date = rest.rsplit_once('>').map_or("", |(_, d)| d);
    let mut date = date.split_whitespace();
    let time = date
        .next()
        .and_then(|secs| secs.parse::<i64>().ok())
        .and_then(|secs| Timestamp::from_second(secs).ok())
        .unwrap_or(Timestamp::UNIX_EPOCH);
    let tz = date.next().and_then(parse_tz).unwrap_or(0);
    Signature {
        name: name.trim().to_owned(),
        email: email.trim().to_owned(),
        time,
        tz,
    }
}

/// `+0130` → 90, `-0800` → -480 (minutes east of UTC).
fn parse_tz(s: &str) -> Option<i32> {
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    Some(sign * (hours * 60 + minutes))
}

/// The `Change-Id: I<40 hex>` trailer of a commit message: only lines in
/// the last paragraph count (never the subject line), and the last such
/// line wins, as with Gerrit's footer parsing. The key is matched without
/// regard to case.
pub(crate) fn change_id_trailer(message: &str) -> Option<String> {
    // `lines` drops `\n` and `\r\n`; trimming also drops a stray `\r` and
    // makes whitespace-only lines blank.
    let lines: Vec<&str> = message.lines().map(str::trim_end).collect();
    let end = lines.iter().rposition(|l| !l.is_empty())?;
    let start = lines[..end]
        .iter()
        .rposition(|l| l.is_empty())
        .map_or(0, |i| i + 1)
        .max(1);
    lines
        .get(start..=end)?
        .iter()
        .rev()
        .find_map(|l| change_id_line(l))
}

fn change_id_line(line: &str) -> Option<String> {
    let (key, value) = line.split_once(':')?;
    if !key.eq_ignore_ascii_case("Change-Id") {
        return None;
    }
    let value = value.trim();
    let hex = value.strip_prefix('I')?;
    (hex.len() == 40 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| value.to_owned())
}

#[cfg(test)]
mod tests;
