//! Which remotes the push dialog offers, and which first.
//!
//! Only a push rule with `HEAD` as its source (`remote.<name>.push =
//! HEAD:refs/for/main`) is offered: the push is that rule with `HEAD`
//! replaced by the commit ([`local_git::push::head_rule`]). Each such rule
//! is one choice; a remote with two rules is two. The choices are ranked
//! by what makes a rule look like a push to this Gerrit server for this
//! change, the configuration order breaking ties.

use local_git::push::{Remote, head_rule};
use model::RevisionId;

/// One rule the dialog offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub remote: String,
    pub push_url: String,
    /// The rule as configured.
    pub rule: String,
    /// The branch of a `refs/for/<branch>` destination.
    pub branch: Option<String>,
    /// Why it ranks where it does, for the dialog.
    pub reasons: Vec<&'static str>,
    pub score: u32,
}

/// What the dialog offers of `remotes` for pushing `sha` of `project`
/// (target `branch`, if known) to the Gerrit server at `server_url`, best
/// first, and the names of the remotes it cannot offer (no `HEAD` rule).
pub fn choices(
    remotes: &[Remote],
    sha: &RevisionId,
    server_url: &str,
    project: &str,
    branch: Option<&str>,
) -> (Vec<Choice>, Vec<String>) {
    let server_host = parse_url(server_url).host;
    let mut offered = Vec::new();
    let mut without = Vec::new();
    for remote in remotes {
        let rules: Vec<&String> = remote
            .push_rules
            .iter()
            .filter(|r| head_rule(r, sha).is_some())
            .collect();
        if rules.is_empty() {
            without.push(remote.name.clone());
            continue;
        }
        let url = parse_url(&remote.push_url);
        for rule in rules {
            let mut reasons = Vec::new();
            let mut score = 0;
            let target = review_branch(rule);
            if target.is_some() {
                score += 8;
                reasons.push("pushes for review (refs/for/)");
            }
            if target.is_some() && target.as_deref() == branch {
                score += 4;
                reasons.push("to the change's branch");
            }
            if url.host.is_some() && url.host == server_host {
                score += 4;
                reasons.push("same host as the Gerrit server");
            }
            if url.port == Some(29418) {
                score += 2;
                reasons.push("Gerrit's SSH port");
            }
            if names_project(&url.path, project) {
                score += 2;
                reasons.push("names the project");
            }
            offered.push(Choice {
                remote: remote.name.clone(),
                push_url: remote.push_url.clone(),
                rule: rule.clone(),
                branch: target,
                reasons,
                score,
            });
        }
    }
    // Stable: equal scores keep the configuration's order.
    offered.sort_by_key(|c| std::cmp::Reverse(c.score));
    (offered, without)
}

/// The branch of a rule that pushes to `refs/for/<branch>` (options after
/// `%` left out; `refs/for/refs/heads/<branch>` is the same branch).
pub fn review_branch(rule: &str) -> Option<String> {
    let (_, dst) = rule.split_once(':')?;
    let rest = dst.strip_prefix("refs/for/")?;
    let rest = rest.split('%').next().unwrap_or_default();
    let rest = rest.strip_prefix("refs/heads/").unwrap_or(rest);
    (!rest.is_empty()).then(|| rest.to_owned())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct UrlParts {
    host: Option<String>,
    port: Option<u16>,
    path: String,
}

/// The host, port and path of a git URL: `scheme://[user@]host[:port]/path`
/// or the scp-like `[user@]host:path`. A local path has no host.
fn parse_url(url: &str) -> UrlParts {
    let (authority, path) = if let Some((_, rest)) = url.split_once("://") {
        match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        }
    } else {
        // scp-like when a colon comes before any slash.
        match url.split_once(':') {
            Some((host, path)) if !host.contains('/') && !host.is_empty() => {
                let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
                return UrlParts {
                    host: Some(host.to_ascii_lowercase()),
                    port: None,
                    path: path.to_owned(),
                };
            }
            _ => {
                return UrlParts {
                    path: url.to_owned(),
                    ..Default::default()
                };
            }
        }
    };
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // `[::1]:29418`: the brackets keep the address's colons apart.
    let (host, port) = match host_port.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((h, p)) => (h, p.strip_prefix(':').and_then(|p| p.parse().ok())),
            None => (rest, None),
        },
        None => match host_port.rsplit_once(':') {
            Some((h, p)) => match p.parse() {
                Ok(port) => (h, Some(port)),
                Err(_) => (host_port, None),
            },
            None => (host_port, None),
        },
    };
    UrlParts {
        host: (!host.is_empty()).then(|| host.to_ascii_lowercase()),
        port,
        path: path.to_owned(),
    }
}

/// Whether the URL path ends with the project's name (`.git` and
/// slashes aside).
fn names_project(path: &str, project: &str) -> bool {
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_start_matches('/');
    !project.is_empty() && (path == project || path.ends_with(&format!("/{project}")))
}

#[cfg(test)]
mod tests {
    use local_git::push::Remote;
    use model::RevisionId;

    use super::{UrlParts, choices, parse_url, review_branch};

    fn remote(name: &str, url: &str, rules: &[&str]) -> Remote {
        Remote {
            name: name.into(),
            push_url: url.into(),
            push_rules: rules.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    fn sha() -> RevisionId {
        RevisionId("a".repeat(40))
    }

    #[test]
    fn urls_of_every_form_give_their_host() {
        let p = |host: &str, port, path: &str| UrlParts {
            host: Some(host.into()),
            port,
            path: path.into(),
        };
        assert_eq!(
            parse_url("ssh://me@Review.example.com:29418/demo/p"),
            p("review.example.com", Some(29418), "/demo/p")
        );
        assert_eq!(
            parse_url("https://review.example.com/a/demo/p.git"),
            p("review.example.com", None, "/a/demo/p.git")
        );
        assert_eq!(
            parse_url("git@review.example.com:demo/p"),
            p("review.example.com", None, "demo/p")
        );
        assert_eq!(
            parse_url("ssh://[::1]:29418/p"),
            p("::1", Some(29418), "/p")
        );
        assert_eq!(parse_url("/srv/git/p").host, None);
        assert_eq!(parse_url("file:///srv/git/p").host, None);
    }

    #[test]
    fn a_review_branch_is_read_from_refs_for() {
        assert_eq!(review_branch("HEAD:refs/for/main").as_deref(), Some("main"));
        assert_eq!(
            review_branch("HEAD:refs/for/refs/heads/stable%topic=x").as_deref(),
            Some("stable")
        );
        assert_eq!(review_branch("HEAD:refs/heads/main"), None);
        assert_eq!(review_branch("HEAD:refs/for/"), None);
    }

    #[test]
    fn only_head_rules_are_offered_and_the_gerrit_like_one_first() {
        let remotes = [
            remote("origin", "https://github.com/demo/p", &[]),
            remote(
                "mirror",
                "https://mirror.example.com/p",
                &["HEAD:refs/heads/main"],
            ),
            remote(
                "gerrit",
                "ssh://me@review.example.com:29418/demo/p",
                &[
                    "HEAD:refs/for/stable",
                    "HEAD:refs/for/main",
                    "refs/heads/*:refs/for/*",
                ],
            ),
        ];
        let (offered, without) = choices(
            &remotes,
            &sha(),
            "https://review.example.com",
            "demo/p",
            Some("main"),
        );
        assert_eq!(without, ["origin"]);
        let order: Vec<_> = offered
            .iter()
            .map(|c| (c.remote.as_str(), c.rule.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("gerrit", "HEAD:refs/for/main"),
                ("gerrit", "HEAD:refs/for/stable"),
                ("mirror", "HEAD:refs/heads/main"),
            ]
        );
        assert_eq!(
            offered[0].reasons,
            [
                "pushes for review (refs/for/)",
                "to the change's branch",
                "same host as the Gerrit server",
                "Gerrit's SSH port",
                "names the project",
            ]
        );
        assert!(offered[2].reasons.is_empty());
    }

    #[test]
    fn equal_choices_keep_the_configuration_order() {
        let remotes = [
            remote("b", "/srv/b", &["HEAD:refs/for/main"]),
            remote("a", "/srv/a", &["HEAD:refs/for/main"]),
        ];
        let (offered, _) = choices(&remotes, &sha(), "https://x", "p", None);
        let names: Vec<_> = offered.iter().map(|c| c.remote.as_str()).collect();
        assert_eq!(names, ["b", "a"]);
    }
}
