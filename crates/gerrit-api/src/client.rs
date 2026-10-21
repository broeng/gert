use std::{fmt, sync::Arc};

use base64::Engine as _;
use model::{Account, ChangeDetail, ChangeNumber, ChangeSummary};
use serde_json::Value;

use crate::{
    Auth, ChangeOption, ChangeQuery, Error, HttpClient, convert, query::encode_component, wire,
};

/// One page of search results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPage {
    pub changes: Vec<ChangeSummary>,
    /// True if the server has more results after this page
    /// (`_more_changes` on the last element).
    pub more_changes: bool,
}

/// Typed access to the Gerrit REST API of one server.
#[derive(Clone)]
pub struct GerritClient {
    http: Arc<dyn HttpClient>,
    /// Base URL without trailing slash; may contain a path prefix.
    base: String,
    auth: Auth,
}

impl fmt::Debug for GerritClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GerritClient")
            .field("base", &self.base)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl GerritClient {
    /// `base_url` is the server root, e.g. `https://gerrit.example.com` or
    /// `https://host/r/`. Do not include `/a`.
    pub fn new(http: Arc<dyn HttpClient>, base_url: &str, auth: Auth) -> Result<Self, Error> {
        let base = base_url.trim().trim_end_matches('/').to_owned();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(Error::InvalidUrl(format!(
                "{base_url:?} must start with http:// or https://"
            )));
        }
        base.parse::<http::Uri>()
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        Ok(Self { http, base, auth })
    }

    /// Builds `{base}[/a]{path}?{query}`. `path` starts with `/`.
    fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        let prefix = if self.auth.is_authenticated() {
            "/a"
        } else {
            ""
        };
        let mut url = format!("{}{prefix}{path}", self.base);
        for (i, (k, v)) in query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(&encode_component(k));
            url.push('=');
            url.push_str(&encode_component(v));
        }
        url
    }

    async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value, Error> {
        let url = self.url(path, query);
        let mut builder = http::Request::builder()
            .method(http::Method::GET)
            .uri(url)
            .header(http::header::ACCEPT, "application/json");
        if let Auth::Basic { username, password } = &self.auth {
            let token =
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
            builder = builder.header(http::header::AUTHORIZATION, format!("Basic {token}"));
        }
        let req = builder
            .body(Vec::new())
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;

        let resp = self.http.send(req).await?;
        let code = resp.status().as_u16();
        let body = resp.into_body();
        match code {
            200..=299 => Ok(serde_json::from_slice(strip_xssi(&body))?),
            401 | 403 => Err(Error::Unauthorized),
            404 => Err(Error::NotFound),
            _ => Err(Error::Status {
                code,
                body: snippet(&body),
            }),
        }
    }

    /// `GET /config/server/version`, e.g. `"3.9.1"`.
    pub async fn server_version(&self) -> Result<String, Error> {
        let v = self.get_json("/config/server/version", &[]).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `GET /accounts/self/detail`. Use it to verify credentials: a bad
    /// password or anonymous access yields [`Error::Unauthorized`].
    pub async fn account_self(&self) -> Result<Account, Error> {
        let v = self.get_json("/accounts/self/detail", &[]).await?;
        let info: wire::AccountInfo = serde_json::from_value(v)?;
        Ok(convert::account(&info)?)
    }

    /// `GET /changes/?q=...`.
    pub async fn query_changes(&self, query: &ChangeQuery) -> Result<QueryPage, Error> {
        let mut params = vec![("q", query.query.clone())];
        if let Some(n) = query.limit {
            params.push(("n", n.to_string()));
        }
        if let Some(s) = query.start {
            params.push(("S", s.to_string()));
        }
        params.extend(query.options.iter().map(|o| ("o", o.as_str().to_owned())));

        let Value::Array(items) = self.get_json("/changes/", &params).await? else {
            return Err(Error::Conversion(convert::ConversionError::MissingField(
                "<array of changes>",
            )));
        };
        let infos = items
            .into_iter()
            .map(serde_json::from_value::<wire::ChangeInfo>)
            .collect::<Result<Vec<_>, _>>()?;
        let more_changes = infos.last().and_then(|c| c.more_changes).unwrap_or(false);
        let changes = infos
            .iter()
            .map(convert::change_summary)
            .collect::<Result<_, _>>()?;
        Ok(QueryPage {
            changes,
            more_changes,
        })
    }

    /// `GET /changes/{n}` with all patch sets and their commits, the change
    /// log, submit requirements and detailed votes.
    pub async fn get_change(&self, number: ChangeNumber) -> Result<ChangeDetail, Error> {
        let params: Vec<_> = [
            ChangeOption::CurrentRevision,
            ChangeOption::AllRevisions,
            ChangeOption::AllCommits,
            ChangeOption::DetailedLabels,
            ChangeOption::DetailedAccounts,
            ChangeOption::Submittable,
            ChangeOption::Messages,
            ChangeOption::SubmitRequirements,
        ]
        .into_iter()
        .map(|o| ("o", o.as_str().to_owned()))
        .collect();
        let v = self
            .get_json(&format!("/changes/{number}"), &params)
            .await?;
        let info: wire::ChangeInfo = serde_json::from_value(v)?;
        Ok(convert::change_detail(&info)?)
    }
}

/// Removes Gerrit's `)]}'` XSSI guard line.
pub(crate) fn strip_xssi(body: &[u8]) -> &[u8] {
    match body.strip_prefix(b")]}'") {
        Some(rest) => match rest.iter().position(|&b| b == b'\n') {
            Some(i) => &rest[i + 1..],
            None => &[],
        },
        None => body,
    }
}

fn snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    text.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use model::{AccountId, ChangeStatus, PatchSetNumber};

    use super::*;
    use crate::fake::FakeHttpClient;

    const CHANGE: &str = r#"{
        "project": "p", "branch": "main", "change_id": "I1", "subject": "s",
        "status": "NEW", "created": "2020-01-02 03:04:05.000000000",
        "updated": "2020-01-03 03:04:05.500000000", "_number": 5,
        "owner": {"_account_id": 1, "name": "Owner"},
        "attention_set": {"3": {"account": {"_account_id": 3}}, "2": {}},
        "labels": {"Code-Review": {"approved": {"_account_id": 2}}}
    }"#;

    fn client(fake: &FakeHttpClient, base: &str, auth: Auth) -> GerritClient {
        GerritClient::new(Arc::new(fake.clone()), base, auth).unwrap()
    }

    fn basic() -> Auth {
        Auth::Basic {
            username: "alice".into(),
            password: "secret".into(),
        }
    }

    fn xssi(body: &str) -> String {
        format!(")]}}'\n{body}")
    }

    #[test]
    fn anonymous_url_has_no_a_prefix_and_no_auth_header() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi("\"3.9.1\""));
        let c = client(&fake, "https://host/r/", Auth::Anonymous);
        assert_eq!(block_on(c.server_version()).unwrap(), "3.9.1");
        let req = &fake.requests()[0];
        assert_eq!(req.uri(), "https://host/r/config/server/version");
        assert!(req.headers().get("authorization").is_none());
        assert_eq!(req.headers()["accept"], "application/json");
    }

    #[test]
    fn authenticated_url_uses_a_prefix_and_basic_header() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(r#"{"_account_id": 7, "name": "Alice", "email": "a@x"}"#),
        );
        let c = client(&fake, "https://host/r//", basic());
        let me = block_on(c.account_self()).unwrap();
        assert_eq!(me.id, AccountId(7));
        let req = &fake.requests()[0];
        assert_eq!(req.uri(), "https://host/r/a/accounts/self/detail");
        // base64("alice:secret")
        assert_eq!(req.headers()["authorization"], "Basic YWxpY2U6c2VjcmV0");
    }

    #[test]
    fn rejects_bad_base_urls() {
        let fake = FakeHttpClient::new();
        let mk = |u| GerritClient::new(Arc::new(fake.clone()), u, Auth::Anonymous);
        assert!(matches!(
            mk("gerrit.example.com"),
            Err(Error::InvalidUrl(_))
        ));
        assert!(mk("https://gerrit.example.com").is_ok());
    }

    #[test]
    fn client_debug_hides_password() {
        let c = client(&FakeHttpClient::new(), "https://h", basic());
        assert!(!format!("{c:?}").contains("secret"));
    }

    #[test]
    fn xssi_stripping() {
        assert_eq!(strip_xssi(b")]}'\n[1]"), b"[1]");
        assert_eq!(strip_xssi(b")]}'\r\n[1]"), b"[1]");
        assert_eq!(strip_xssi(b"[1]"), b"[1]");
        assert_eq!(strip_xssi(b")]}'"), b"");
    }

    #[test]
    fn body_without_xssi_still_parses() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, "\"3.9.1\"");
        let c = client(&fake, "https://h", Auth::Anonymous);
        assert_eq!(block_on(c.server_version()).unwrap(), "3.9.1");
    }

    #[test]
    fn error_mapping() {
        let fake = FakeHttpClient::new();
        let c = client(&fake, "https://h", Auth::Anonymous);
        for (code, check) in [
            (401, "unauthorized"),
            (403, "unauthorized"),
            (404, "notfound"),
            (500, "status"),
        ] {
            fake.push_response(code, "oops");
            match (block_on(c.server_version()), check) {
                (Err(Error::Unauthorized), "unauthorized") => {}
                (Err(Error::NotFound), "notfound") => {}
                (Err(Error::Status { code: c2, body }), "status") => {
                    assert_eq!((c2, body.as_str()), (500, "oops"));
                }
                (other, _) => panic!("{code}: {other:?}"),
            }
        }
        fake.push_error("connection reset");
        assert!(matches!(block_on(c.server_version()), Err(Error::Http(_))));
        fake.push_response(200, xssi("{not json"));
        assert!(matches!(
            block_on(c.server_version()),
            Err(Error::Decode(_))
        ));
    }

    #[test]
    fn query_url_is_encoded() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi("[]"));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let q = ChangeQuery::new(r#"is:open owner:"a b" (x)"#)
            .limit(25)
            .start(50);
        let page = block_on(c.query_changes(&q)).unwrap();
        assert!(page.changes.is_empty() && !page.more_changes);
        assert_eq!(
            fake.requests()[0].uri().to_string(),
            "https://h/changes/?q=is%3Aopen%20owner%3A%22a%20b%22%20%28x%29\
             &n=25&S=50&o=LABELS&o=DETAILED_ACCOUNTS&o=SUBMITTABLE"
        );
    }

    #[test]
    fn query_converts_and_reports_more_changes() {
        let fake = FakeHttpClient::new();
        let last = CHANGE.replacen('{', r#"{"_more_changes": true,"#, 1);
        fake.push_response(200, xssi(&format!("[{CHANGE}, {last}]")));
        fake.push_response(200, xssi(&format!("[{CHANGE}]")));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let q = ChangeQuery::new("status:open");
        let page = block_on(c.query_changes(&q)).unwrap();
        assert!(page.more_changes);
        assert_eq!(page.changes.len(), 2);
        let ch = &page.changes[0];
        assert_eq!(ch.number.0, 5);
        assert_eq!(ch.status, ChangeStatus::New);
        assert_eq!(ch.attention_set, vec![AccountId(2), AccountId(3)]);
        assert!(ch.labels["Code-Review"].approved);
        assert!(!block_on(c.query_changes(&q)).unwrap().more_changes);
    }

    #[test]
    fn unknown_status_and_submit_type_do_not_fail() {
        let fake = FakeHttpClient::new();
        let json = CHANGE.replace("\"NEW\"", "\"FROBNICATED\"").replacen(
            '{',
            r#"{"submit_type": "TELEPORT","#,
            1,
        );
        fake.push_response(200, xssi(&format!("[{json}]")));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let page = block_on(c.query_changes(&ChangeQuery::new("x"))).unwrap();
        assert_eq!(
            page.changes[0].status,
            ChangeStatus::Unknown("FROBNICATED".into())
        );
    }

    #[test]
    fn real_world_shaped_detail_with_extra_fields_and_unknown_enums() {
        let fake = FakeHttpClient::new();
        let json = r#"{
            "id": "p~main~I1", "triplet_id": "p~main~I1", "project": "p",
            "branch": "main", "change_id": "I1", "subject": "s",
            "status": "NEW", "created": "2020-01-02 03:04:05.000000000",
            "updated": "2020-01-03 03:04:05.500000000", "_number": 5,
            "submit_type": "NEW_THING", "mergeable": true, "future": [1],
            "owner": {"_account_id": 1, "name": "Owner", "avatars": [], "tags": ["SERVICE_USER"]},
            "labels": {"Code-Review": {"optional": true, "all": [], "values": {" 0": "No score"}}},
            "submit_requirements": [{"name": "Code-Review", "status": "WHATEVER",
                "future": 1}],
            "messages": [{"id": "m1", "date": "2020-01-02 03:04:05.000000000",
                "tag": "autogenerated:x", "type": "NEWTYPE"}],
            "current_revision": "abc",
            "revisions": {"abc": {
                "kind": "WHATEVER", "_number": 2, "ref": "refs/changes/05/5/2",
                "actions": {"rebase": {"method": "POST"}},
                "commit": {"message": "msg", "author": {"name": "A", "tz": -120,
                    "date": "2020-01-02 03:04:05.000000000"}, "parents": []}
            }},
            "_more_changes": false
        }"#;
        fake.push_response(200, xssi(json));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let d = block_on(c.get_change(ChangeNumber(5))).unwrap();
        assert_eq!(d.current_patch_set, Some(PatchSetNumber(2)));
        assert_eq!(d.commit_message.as_deref(), Some("msg"));
        assert_eq!(d.summary.status, ChangeStatus::New);
        // Unknown enum values survive; so does a message without a body.
        assert_eq!(
            d.submit_requirements[0].status,
            model::SubmitRequirementStatus::Unknown("WHATEVER".into())
        );
        assert!(d.messages[0].is_autogenerated() && d.messages[0].message.is_empty());
        assert_eq!(
            d.revisions[0].kind,
            Some(model::RevisionKind::Unknown("WHATEVER".into()))
        );
    }

    #[test]
    fn get_change_converts_revisions_messages_requirements_and_votes() {
        let fake = FakeHttpClient::new();
        let json = r#"{
            "project": "p", "branch": "main", "change_id": "I1", "subject": "s",
            "status": "NEW", "created": "2020-01-02 03:04:05.000000000",
            "updated": "2020-01-03 03:04:05.500000000", "_number": 5,
            "owner": {"_account_id": 1},
            "current_revision": "bbb",
            "revisions": {
                "bbb": {"_number": 2, "kind": "TRIVIAL_REBASE", "ref": "refs/changes/05/5/2",
                    "created": "2020-01-03 00:00:00.000000000",
                    "uploader": {"_account_id": 1, "name": "Owner"},
                    "description": "rebased",
                    "commit": {"subject": "s", "message": "s\n\nbody\n",
                        "parents": [{"commit": "ppp", "subject": "parent"}, {"subject": "no sha"}],
                        "author": {"name": "A", "tz": -120, "date": "2020-01-02 03:04:05.000000000"}}},
                "aaa": {"_number": 1, "kind": "REWORK", "description": "",
                    "created": "2020-01-02 00:00:00.000000000"}
            },
            "messages": [
                {"id": "m1", "author": {"_account_id": 2}, "real_author": {"_account_id": 3},
                 "date": "2020-01-02 04:00:00.000000000", "message": "Patch Set 1: Code-Review+1",
                 "_revision_number": 1},
                {"id": "m2", "date": "2020-01-02 05:00:00.000000000", "message": "x",
                 "tag": "autogenerated:gerrit:merged"}
            ],
            "submit_requirements": [
                {"name": "Code-Review", "description": "Needs +2", "status": "UNSATISFIED",
                 "is_legacy": false,
                 "applicability_expression_result": {"expression": "branch:main", "fulfilled": true,
                    "status": "PASS", "passing_atoms": ["branch:main"], "failing_atoms": []},
                 "submittability_expression_result": {"expression": "label:Code-Review=MAX",
                    "fulfilled": false, "status": "FAIL", "failing_atoms": ["label:Code-Review=MAX"],
                    "atom_explanations": {"label:Code-Review=MAX": "needs a +2"}}}
            ],
            "labels": {
                "Code-Review": {"all": [
                    {"_account_id": 2, "name": "Rev", "value": 1,
                     "date": "2020-01-02 04:00:00.000000000"},
                    {"value": 2}
                ], "values": {" 0": "", "+1": "", "+2": ""}},
                "Verified": {"all": []},
                "Other": {"optional": true}
            }
        }"#;
        fake.push_response(200, xssi(json));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let d = block_on(c.get_change(ChangeNumber(5))).unwrap();

        // Ascending by number, whatever the map order.
        let numbers: Vec<_> = d.revisions.iter().map(|r| r.number.0).collect();
        assert_eq!(numbers, [1, 2]);
        assert_eq!(d.current().map(|r| r.sha.as_str()), Some("bbb"));
        let r2 = &d.revisions[1];
        assert_eq!(r2.kind, Some(model::RevisionKind::TrivialRebase));
        assert_eq!(r2.uploader.as_ref().map(|u| u.id), Some(AccountId(1)));
        assert_eq!(r2.description.as_deref(), Some("rebased"));
        let commit = r2.commit.as_ref().unwrap();
        assert_eq!(commit.parents.len(), 1, "parent without a SHA is dropped");
        assert_eq!(commit.parents[0].sha.as_str(), "ppp");
        let author = commit.author.as_ref().unwrap();
        assert_eq!(author.tz, Some(-120));
        assert_eq!(author.date.unwrap().to_string(), "2020-01-02T03:04:05Z");
        assert_eq!(d.revisions[0].description, None);
        assert_eq!(d.revisions[0].commit, None);

        assert_eq!(d.messages.len(), 2);
        assert_eq!(d.messages[0].real_author.as_ref().unwrap().id, AccountId(3));
        assert_eq!(d.messages[0].revision, Some(PatchSetNumber(1)));
        assert!(d.messages[1].author.is_none() && d.messages[1].is_autogenerated());

        let sr = &d.submit_requirements[0];
        assert_eq!(sr.status, model::SubmitRequirementStatus::Unsatisfied);
        assert!(!sr.status.is_ok() && !sr.is_legacy);
        assert!(sr.applicability.as_ref().unwrap().fulfilled);
        let sub = sr.submittability.as_ref().unwrap();
        assert_eq!(sub.status, Some(model::ExpressionStatus::Fail));
        assert_eq!(sub.atom_explanations["label:Code-Review=MAX"], "needs a +2");
        assert!(sr.override_result.is_none());

        let votes = &d.votes["Code-Review"];
        assert_eq!(votes.len(), 1, "vote without a voter is skipped");
        assert_eq!((votes[0].account.id, votes[0].value), (AccountId(2), 1));
        assert!(votes[0].date.is_some());
        assert!(d.votes["Verified"].is_empty());
        assert!(!d.votes.contains_key("Other"));
    }

    #[test]
    fn revision_without_a_number_is_a_conversion_error() {
        let fake = FakeHttpClient::new();
        let json = CHANGE.replacen('{', r#"{"revisions": {"abc": {"kind": "REWORK"}},"#, 1);
        fake.push_response(200, xssi(&json));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let err = block_on(c.get_change(ChangeNumber(5))).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Conversion(convert::ConversionError::MissingField("revisions._number"))
            ),
            "{err:?}"
        );
    }

    #[test]
    fn missing_required_field_is_a_conversion_error() {
        let fake = FakeHttpClient::new();
        let json = CHANGE.replace("\"subject\": \"s\",", "");
        fake.push_response(200, xssi(&format!("[{json}]")));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let err = block_on(c.query_changes(&ChangeQuery::new("x"))).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Conversion(convert::ConversionError::MissingField("subject"))
            ),
            "{err:?}"
        );
    }

    #[test]
    fn bad_timestamp_is_a_conversion_error() {
        let fake = FakeHttpClient::new();
        let json = CHANGE.replace("2020-01-02 03:04:05.000000000", "nope");
        fake.push_response(200, xssi(&format!("[{json}]")));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let err = block_on(c.query_changes(&ChangeQuery::new("x"))).unwrap_err();
        assert!(matches!(err, Error::Conversion(_)), "{err:?}");
    }

    #[test]
    fn get_change_extracts_revision_and_reviewers() {
        let fake = FakeHttpClient::new();
        let json = CHANGE.replacen(
            '{',
            r#"{"current_revision": "abc", "current_revision_number": 2,
            "revisions": {"abc": {"_number": 2, "kind": "WEIRD",
                "commit": {"message": "subj\n\nbody\n"}}},
            "reviewers": {"REVIEWER": [{"_account_id": 4}],
                          "CC": [{"_account_id": 5, "name": "C"}]},"#,
            1,
        );
        fake.push_response(200, xssi(&json));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let d = block_on(c.get_change(ChangeNumber(5))).unwrap();
        assert_eq!(d.current_revision, Some("abc".into()));
        assert_eq!(d.current_patch_set, Some(PatchSetNumber(2)));
        assert_eq!(d.commit_message.as_deref(), Some("subj\n\nbody\n"));
        assert_eq!(d.reviewers[0].id, AccountId(4));
        assert_eq!(d.cc[0].display(), "C");
        assert_eq!(
            fake.requests()[0].uri().to_string(),
            "https://h/changes/5?o=CURRENT_REVISION&o=ALL_REVISIONS&o=ALL_COMMITS\
             &o=DETAILED_LABELS&o=DETAILED_ACCOUNTS&o=SUBMITTABLE&o=MESSAGES\
             &o=SUBMIT_REQUIREMENTS"
        );
    }

    #[test]
    fn get_change_not_found() {
        let fake = FakeHttpClient::new();
        fake.push_response(404, "Not found: 9");
        let c = client(&fake, "https://h", Auth::Anonymous);
        assert!(matches!(
            block_on(c.get_change(ChangeNumber(9))),
            Err(Error::NotFound)
        ));
    }
}
