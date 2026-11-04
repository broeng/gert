//! Commits on local branches: discovery of unpushed commits for local
//! changes, where they are headed, and full commit data for a change page.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::Mutex;

use model::{
    BranchTargets, CommitInfo, LocalCommit, LocalScan, RevisionId, Signature, Timestamp, Upstream,
};

use crate::git::{self, Object};
use crate::{Error, Local, NotLocal, Repo, is_object_id};

/// One record per ref, fields separated by NUL. Ref names cannot contain
/// control characters, so the newline between records is safe.
const REF_FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(symref)%00\
                          %(upstream:remotename)%00%(upstream:remoteref)%00%(HEAD)";

const HEADS: &str = "refs/heads/";

/// How many local commits HEAD may reach before [`Repo::scan`] gives up on
/// listing them ([`LocalScan::head_truncated`]): far more than a chain of
/// changes, so it only guards against remote-tracking branches far behind.
const HEAD_LIMIT: usize = 5000;
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
    /// The branch checked out in this worktree.
    head: bool,
}

/// Checkouts already logged as having no remote-tracking branches, so that
/// is said once per checkout and process, not on every scan.
static NO_REMOTES_LOGGED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

impl Repo {
    /// Looks for the commits of local changes, from a single read of the
    /// refs so that the commits, their branches, the targets and the
    /// fingerprint all describe the same moment.
    ///
    /// The commits are those reachable from a named local branch
    /// (`refs/heads/*`) and from no remote-tracking branch
    /// (`refs/remotes/*`): the `limit` newest by committer date, newest
    /// first but never a parent before its children (`--date-order`;
    /// `--topo-order` would finish one line of history before the next, so
    /// the limit could drop a newer branch). [`LocalScan::truncated`] says
    /// whether there were more. Apart from that, every local commit the
    /// checked-out branch reaches is listed in [`LocalScan::head_commits`]
    /// (one more `rev-list`, without that limit), and those among the
    /// commits are [`LocalCommit::on_head`].
    ///
    /// A detached HEAD does not count ([`LocalScan::detached_head`]: no
    /// commit is on HEAD then). Merge commits are included (see
    /// [`LocalCommit::is_merge`]); nothing is filtered by author. A
    /// repository without any remote-tracking branch is not scanned at all
    /// ([`LocalScan::no_remotes`]): its whole history would count as local.
    pub fn scan(&self, limit: usize) -> Result<LocalScan, Error> {
        let raw = self.refs_raw()?;
        let records = parse_refs(&raw)?;
        let mut scan = LocalScan {
            fingerprint: fingerprint(&raw),
            targets: branch_targets(&records),
            ..LocalScan::default()
        };
        // Only full object ids go to git, so nothing can pass as an option.
        let tips = |prefix: &str| -> Vec<&RefRecord> {
            records
                .iter()
                .filter(|r| r.name.starts_with(prefix) && is_object_id(&r.oid))
                .collect()
        };
        let heads = tips(HEADS);
        let remotes: BTreeSet<&str> = tips(REMOTES).iter().map(|r| r.oid.as_str()).collect();
        if remotes.is_empty() {
            scan.no_remotes = true;
            if !heads.is_empty()
                && let Ok(mut logged) = NO_REMOTES_LOGGED.lock()
                && logged.insert(self.path().to_owned())
            {
                log::info!(
                    "{} has no remote-tracking branches; not looking for local changes",
                    self.path().display()
                );
            }
            return Ok(scan);
        }
        if heads.is_empty() {
            return Ok(scan);
        }

        let mut input = String::new();
        for head in &heads {
            input.push_str(&head.oid);
            input.push('\n');
        }
        for oid in &remotes {
            input.push('^');
            input.push_str(oid);
            input.push('\n');
        }
        // One more than asked for, to know whether the limit cut anything.
        let max = format!("--max-count={}", limit.saturating_add(1));
        let out = self.git.run(
            &["rev-list", "--date-order", &max, "--stdin"],
            Some(input.as_bytes()),
        )?;
        let mut shas: Vec<String> = String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| is_object_id(l))
            .map(str::to_owned)
            .collect();
        if shas.len() > limit {
            shas.truncate(limit);
            scan.truncated = true;
        }

        let objects = self.git.read_objects(&shas)?;
        for (sha, object) in shas.iter().zip(objects) {
            match object {
                Object::Found { kind, data, .. } if kind == "commit" => {
                    scan.commits.push(LocalCommit {
                        commit: parse_commit(sha, &data),
                        branches: Vec::new(),
                        on_head: false,
                    });
                }
                // Gone since rev-list listed it (a gc in between, say).
                _ => log::debug!(
                    "{}: skipping {sha}, which is no longer a commit",
                    self.path().display()
                ),
            }
        }
        fill_branches(&mut scan.commits, &heads);
        let Some(head) = heads.iter().find(|r| r.head) else {
            scan.detached_head = true;
            return Ok(scan);
        };
        let mut input = format!("{}\n", head.oid);
        for oid in &remotes {
            input.push('^');
            input.push_str(oid);
            input.push('\n');
        }
        let max = format!("--max-count={}", HEAD_LIMIT.saturating_add(1));
        let out = self
            .git
            .run(&["rev-list", &max, "--stdin"], Some(input.as_bytes()))?;
        let mut on_head: Vec<RevisionId> = String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| is_object_id(l))
            .map(RevisionId::from)
            .collect();
        if on_head.len() > HEAD_LIMIT {
            on_head.truncate(HEAD_LIMIT);
            scan.head_truncated = true;
        }
        let set: HashSet<&RevisionId> = on_head.iter().collect();
        for c in &mut scan.commits {
            c.on_head = set.contains(&c.commit.sha);
        }
        scan.head_commits = on_head;
        Ok(scan)
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

    /// The addresses this checkout's own commits may carry as author:
    /// `author.email` (which git 2.21 and later prefer for new commits),
    /// then `user.email`, trimmed, without duplicates and empty values.
    /// Both count, since older commits may have been made with either. As
    /// git sees them here: repository, global and system configuration,
    /// includes followed.
    pub fn author_emails(&self) -> Result<Vec<String>, Error> {
        let mut emails: Vec<String> = Vec::new();
        for key in ["author.email", "user.email"] {
            if let Some(email) = self.config_value(key)?
                && !emails.contains(&email)
            {
                emails.push(email);
            }
        }
        Ok(emails)
    }

    /// `git config --get <key>`, trimmed; `None` if unset or empty.
    fn config_value(&self, key: &str) -> Result<Option<String>, Error> {
        let args = ["config", "--get", key];
        let out = self.git.run_raw(&args, None)?;
        match out.status.code() {
            Some(0) => {
                let value = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                Ok((!value.is_empty()).then_some(value))
            }
            // 1: the key is not set.
            Some(1) => Ok(None),
            _ => Err(git::failure(&args, &out)),
        }
    }

    /// A hash of every local and remote-tracking branch (name, commit,
    /// symbolic target, upstream and whether it is checked out, so a
    /// switch of branches counts), the same as [`LocalScan::fingerprint`]
    /// of a scan that saw the same refs. One cheap `for-each-ref`, so a
    /// scanner can skip [`scan`](Self::scan) when nothing changed. Stable
    /// within one process run only.
    pub fn refs_fingerprint(&self) -> Result<u64, Error> {
        Ok(fingerprint(&self.refs_raw()?))
    }

    fn refs_raw(&self) -> Result<Vec<u8>, Error> {
        self.git
            .run(&["for-each-ref", REF_FORMAT, HEADS, REMOTES], None)
    }
}

fn fingerprint(refs_raw: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    refs_raw.hash(&mut hasher);
    hasher.finish()
}

fn parse_refs(raw: &[u8]) -> Result<Vec<RefRecord>, Error> {
    let mut records = Vec::new();
    for line in raw.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let fields: Vec<String> = line
            .split(|&b| b == 0)
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();
        let Ok([name, oid, symref, upstream_remote, upstream_ref, head]) =
            <[String; 6]>::try_from(fields)
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
            head: head == "*",
        });
    }
    Ok(records)
}

/// Sets [`LocalCommit::branches`] by walking parents from each branch tip
/// inside the listed commits, without asking git again.
///
/// That is exact: a branch whose tip is not listed is either on a remote
/// (and so is everything it reaches) or was cut off by the limit, and the
/// order puts all its ancestors after it, so they were cut off too. For the
/// same reason a walk never has to step through an unlisted commit to reach
/// a listed one. (A commit skipped because it vanished mid-scan does cut
/// the walks through it short.)
fn fill_branches(commits: &mut [LocalCommit], heads: &[&RefRecord]) {
    let index: HashMap<String, usize> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.commit.sha.0.clone(), i))
        .collect();
    for head in heads {
        let Some(branch) = head.name.strip_prefix(HEADS) else {
            continue;
        };
        let Some(&tip) = index.get(&head.oid) else {
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
}

/// Each local branch's upstream branch on its remote (`branch.<b>.remote`
/// and `branch.<b>.merge`), leaving out upstreams that are local branches,
/// and each remote's default branch from `refs/remotes/<remote>/HEAD` where
/// that exists.
fn branch_targets(records: &[RefRecord]) -> BranchTargets {
    let mut targets = BranchTargets::default();
    for record in records {
        if let Some(branch) = record.name.strip_prefix(HEADS) {
            // git leaves both atoms empty unless `branch.<b>.merge` is a
            // full ref name and the remote is a configured one.
            let remote = &record.upstream_remote;
            if remote.is_empty() || remote == "." {
                continue;
            }
            // Another ref namespace is not a branch to target.
            let Some(upstream) = record.upstream_ref.strip_prefix(HEADS) else {
                continue;
            };
            if upstream.is_empty() {
                continue;
            }
            targets.upstreams.insert(
                branch.to_owned(),
                Upstream {
                    remote: remote.clone(),
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
    targets
}

/// Parses a raw commit object. Malformed identities give empty fields and
/// the Unix epoch rather than an error.
///
/// The message is always read as UTF-8, lossily: honouring the `encoding`
/// header (commits written in Latin-1 and the like) is out of scope.
pub(crate) fn parse_commit(sha: &str, raw: &[u8]) -> CommitInfo {
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

/// The `Change-Id: I<40 hex>` trailer of a commit message. As in JGit's
/// footer parsing, only lines of the last paragraph count, never the
/// message's first line, and only an empty line ends a paragraph (one with
/// spaces does not); unlike JGit, an empty CRLF line does too. The key is
/// matched without regard to case.
///
/// Gerrit rejects a push whose footer has several Change-Id lines, so such
/// a commit never becomes a change as it is; taking the last one is just a
/// harmless choice.
pub(crate) fn change_id_trailer(message: &str) -> Option<String> {
    // `lines` drops `\n` and `\r\n`.
    let lines: Vec<&str> = message.lines().collect();
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
