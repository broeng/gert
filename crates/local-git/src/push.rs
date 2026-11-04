//! Pushing a commit for review: the one command here that writes, and only
//! when the user asks for it.
//!
//! What is pushed comes from the remote's own push rules
//! (`remote.<name>.push`): a rule with `HEAD` as its source, such as
//! `HEAD:refs/for/main%topic=x`, is used with `HEAD` replaced by the
//! commit, `git push <remote> <sha>:refs/for/main%topic=x`
//! ([`head_rule`]). The configuration is only read, never changed. The
//! push itself writes to the remote; locally git updates at most a
//! remote-tracking branch, when a rule names a branch the remote fetches.

use std::time::Duration;

use model::RevisionId;

use crate::{Error, Repo, git, is_object_id};

/// A configured remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    /// Where pushes go (`remote.<name>.pushurl`, else `url`, with the
    /// `insteadOf` rules applied), as `git remote get-url --push` says.
    pub push_url: String,
    /// The push rules (`remote.<name>.push`), as written.
    pub push_rules: Vec<String>,
}

/// One commit [`Repo::outgoing`] lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingCommit {
    pub sha: RevisionId,
    pub subject: String,
}

/// The commits a push of a commit would send, as far as the checkout
/// knows: those no remote-tracking branch of the remote has. Gerrit may
/// have some of them already as patch sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// Newest first.
    pub commits: Vec<OutgoingCommit>,
    /// There were more than the limit.
    pub more: bool,
}

/// What a push said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushOutcome {
    /// git exited successfully.
    pub success: bool,
    /// Gerrit refused because it has every commit already
    /// (`no new changes`).
    pub no_new_changes: bool,
    /// The lines of the remote's answer that start with a link: Gerrit
    /// lists each change it created or updated that way
    /// (`https://host/c/project/+/42 Subject [NEW]`).
    pub changes: Vec<String>,
    /// Everything git wrote, stderr first.
    pub output: String,
}

/// The refspec push rule `rule` makes for commit `sha`: a rule whose
/// source is `HEAD` (`HEAD:refs/for/main`, also with a leading `+`), with
/// `HEAD` replaced by the commit. `None` for any other rule.
pub fn head_rule(rule: &str, sha: &RevisionId) -> Option<String> {
    let (force, rest) = match rule.strip_prefix('+') {
        Some(rest) => ("+", rest),
        None => ("", rule),
    };
    let rest = rest.strip_prefix("HEAD")?;
    let dst = rest.strip_prefix(':')?;
    if dst.is_empty() {
        return None;
    }
    Some(format!("{force}{}{rest}", sha.as_str()))
}

impl Repo {
    /// The configured remotes, in the order of the configuration.
    pub fn remotes(&self) -> Result<Vec<Remote>, Error> {
        let args = [
            "config",
            "--null",
            "--get-regexp",
            r"^remote\..*\.(url|pushurl|push)$",
        ];
        let out = self.git.run_raw(&args, None)?;
        // 1: nothing matched.
        match out.status.code() {
            Some(0) => {}
            Some(1) => return Ok(Vec::new()),
            _ => return Err(git::failure(&args, &out)),
        }
        let mut remotes: Vec<Remote> = Vec::new();
        for entry in String::from_utf8_lossy(&out.stdout).split('\0') {
            // `remote.<name>.<key>\n<value>`; a name may contain dots.
            let Some((key, value)) = entry.split_once('\n') else {
                continue;
            };
            let Some((name, var)) = key.strip_prefix("remote.").and_then(|k| k.rsplit_once('.'))
            else {
                continue;
            };
            let at = match remotes.iter().position(|r| r.name == name) {
                Some(at) => at,
                None => {
                    remotes.push(Remote {
                        name: name.to_owned(),
                        push_url: String::new(),
                        push_rules: Vec::new(),
                    });
                    remotes.len() - 1
                }
            };
            if var == "push" {
                remotes[at].push_rules.push(value.to_owned());
            }
        }
        for remote in &mut remotes {
            if remote.name.starts_with('-') {
                continue;
            }
            let args = ["remote", "get-url", "--push", remote.name.as_str()];
            let out = self.git.run_raw(&args, None)?;
            if out.status.success() {
                remote.push_url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            }
        }
        remotes.retain(|r| !r.name.starts_with('-'));
        Ok(remotes)
    }

    /// The commits a push of `sha` to `remote` would send (see
    /// [`Outgoing`]), at most `limit` of them.
    pub fn outgoing(
        &self,
        sha: &RevisionId,
        remote: &str,
        limit: usize,
    ) -> Result<Outgoing, Error> {
        check_args(sha, remote)?;
        let count = format!("-n{}", limit.saturating_add(1));
        let remotes = format!("--remotes={remote}");
        let args = [
            "log",
            "--format=%H%x00%s",
            count.as_str(),
            sha.as_str(),
            "--not",
            remotes.as_str(),
        ];
        let out = self.git.run(&args, None)?;
        let mut commits: Vec<OutgoingCommit> = String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                let (sha, subject) = l.split_once('\0')?;
                Some(OutgoingCommit {
                    sha: RevisionId(sha.to_owned()),
                    subject: subject.to_owned(),
                })
            })
            .collect();
        let more = commits.len() > limit;
        commits.truncate(limit);
        Ok(Outgoing { commits, more })
    }

    /// Pushes `sha` to `remote` with push rule `rule` of that remote (see
    /// [`head_rule`]), stopping git after `timeout`. A push the remote
    /// refuses is an outcome, not an error.
    pub fn push(
        &self,
        remote: &str,
        sha: &RevisionId,
        rule: &str,
        timeout: Duration,
    ) -> Result<PushOutcome, Error> {
        check_args(sha, remote)?;
        let spec = head_rule(rule, sha).ok_or_else(|| Error::Parse {
            command: "push",
            detail: format!("the push rule {rule:?} does not push HEAD"),
        })?;
        let out = self
            .git
            .run_timed(&["push", remote, spec.as_str()], timeout)?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let changes = stderr
            .lines()
            .filter_map(|l| l.strip_prefix("remote:"))
            .map(str::trim)
            .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
            .map(str::to_owned)
            .collect();
        let output = [stderr.trim(), stdout.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(PushOutcome {
            success: out.status.success(),
            no_new_changes: stderr.contains("no new changes"),
            changes,
            output,
        })
    }
}

/// Keeps anything that would read as an option or a revision expression
/// away from git.
fn check_args(sha: &RevisionId, remote: &str) -> Result<(), Error> {
    if !is_object_id(sha.as_str()) {
        return Err(Error::Parse {
            command: "push",
            detail: format!("{sha} is not a commit id"),
        });
    }
    if remote.is_empty() || remote.starts_with('-') {
        return Err(Error::Parse {
            command: "push",
            detail: format!("{remote:?} is not a remote name"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
