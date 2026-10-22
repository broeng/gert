use std::{collections::BTreeMap, fmt, sync::Arc};

use base64::Engine as _;
use model::{
    Account, AddReviewerResult, ChangeDetail, ChangeNumber, ChangeSummary, Comment, Diff,
    DiffPreferences, DraftInput, DraftReceipt, FileEntry, PatchSetNumber, RelatedChange,
    ReviewInput, ReviewResult, ReviewerState, RevisionId, SuggestedReviewer,
};
use serde_json::Value;

use crate::{
    Auth, ChangeOption, ChangeQuery, DiffParams, Error, HttpClient, convert,
    query::encode_component, wire,
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
            // A flag parameter (`?reviewed`) has no value.
            if !v.is_empty() {
                url.push('=');
                url.push_str(&encode_component(v));
            }
        }
        url
    }

    /// Sends a request and returns the body of a 2xx answer (XSSI guard
    /// not yet removed). `body` is sent as JSON.
    async fn send(
        &self,
        method: http::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, Error> {
        let url = self.url(path, query);
        let mut builder = http::Request::builder()
            .method(method)
            .uri(url)
            .header(http::header::ACCEPT, "application/json");
        if body.is_some() {
            builder = builder.header(
                http::header::CONTENT_TYPE,
                "application/json; charset=UTF-8",
            );
        }
        if let Auth::Basic { username, password } = &self.auth {
            let token =
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
            builder = builder.header(http::header::AUTHORIZATION, format!("Basic {token}"));
        }
        let req = builder
            .body(body.unwrap_or_default())
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;

        let resp = self.http.send(req).await?;
        let code = resp.status().as_u16();
        let body = resp.into_body();
        match code {
            200..=299 => Ok(body),
            // Without credentials the server has no better word than 403
            // (a real Gerrit answers 403 to an anonymous `/accounts/self`),
            // and what it means is "log in".
            403 if !self.is_authenticated() => Err(Error::Unauthorized),
            _ => Err(error_for_status(code, &body)),
        }
    }

    async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value, Error> {
        let body = self.send(http::Method::GET, path, query, None).await?;
        Ok(serde_json::from_slice(strip_xssi(&body))?)
    }

    /// A write with a JSON body (`POST`, `PUT`). Anonymous sessions are
    /// refused up front.
    async fn write_json<B: serde::Serialize>(
        &self,
        method: http::Method,
        path: &str,
        body: &B,
    ) -> Result<Value, Error> {
        self.require_auth()?;
        let bytes = serde_json::to_vec(body)?;
        let body = self.send(method, path, &[], Some(bytes)).await?;
        Ok(serde_json::from_slice(strip_xssi(&body))?)
    }

    /// A write without a request or response body (`DELETE`, or a `PUT`
    /// that only toggles a flag). The answer's body is ignored.
    async fn write_empty(&self, method: http::Method, path: &str) -> Result<(), Error> {
        self.require_auth()?;
        self.send(method, path, &[], None).await?;
        Ok(())
    }

    /// Fails with [`Error::Unauthorized`] for anonymous sessions, for
    /// everything that needs an account: no request is made.
    fn require_auth(&self) -> Result<(), Error> {
        if self.is_authenticated() {
            Ok(())
        } else {
            Err(Error::Unauthorized)
        }
    }

    /// Whether requests carry credentials.
    pub fn is_authenticated(&self) -> bool {
        self.auth.is_authenticated()
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
    /// log, submit requirements, detailed votes and the actions the user may
    /// run (`CURRENT_ACTIONS`, `CHANGE_ACTIONS`).
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
            ChangeOption::CurrentActions,
            ChangeOption::ChangeActions,
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

    /// `GET /changes/{n}/revisions/{rev}/files[?base=]`: the files changed
    /// in `target`, compared with patch set `base` (or the parent commit).
    /// Sorted for display: `/COMMIT_MSG`, `/MERGE_LIST`, then by path.
    pub async fn get_files(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
        base: Option<PatchSetNumber>,
    ) -> Result<Vec<FileEntry>, Error> {
        let params: Vec<_> = base.map(|b| ("base", b.to_string())).into_iter().collect();
        let v = self
            .get_json(&revision_path(change, target, "/files"), &params)
            .await?;
        let files: BTreeMap<String, wire::CommonFileInfo> = serde_json::from_value(v)?;
        Ok(convert::file_list(files))
    }

    /// `GET /changes/{n}/revisions/{rev}/files?reviewed`: paths the current
    /// user marked as reviewed. Anonymous sessions have none, and the
    /// server would answer 401, so no request is made.
    pub async fn get_reviewed_files(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
    ) -> Result<Vec<String>, Error> {
        if !self.is_authenticated() {
            return Ok(Vec::new());
        }
        let v = self
            .get_json(
                &revision_path(change, target, "/files"),
                &[("reviewed", String::new())],
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `GET /changes/{n}/revisions/{rev}/files/{file}/diff`.
    pub async fn get_diff(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
        path: &str,
        params: &DiffParams,
    ) -> Result<Diff, Error> {
        let path = revision_path(
            change,
            target,
            &format!("/files/{}/diff", encode_component(path)),
        );
        let v = self.get_json(&path, &params.to_query()).await?;
        let info: wire::DiffInfo = serde_json::from_value(v)?;
        Ok(convert::diff(&info)?)
    }

    /// `GET /changes/{n}/revisions/{rev}/related`, in server order.
    pub async fn get_related(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
    ) -> Result<Vec<RelatedChange>, Error> {
        let v = self
            .get_json(&revision_path(change, target, "/related"), &[])
            .await?;
        let info: wire::RelatedChangesInfo = serde_json::from_value(v)?;
        Ok(convert::related(&info)?)
    }

    /// `GET /changes/{n}/comments`: the published comments of all patch
    /// sets, by path (special files first), in server order within a path.
    pub async fn get_comments(&self, change: ChangeNumber) -> Result<Vec<Comment>, Error> {
        let v = self
            .get_json(&format!("/changes/{change}/comments"), &[])
            .await?;
        let map: BTreeMap<String, Vec<wire::CommentInfo>> = serde_json::from_value(v)?;
        Ok(convert::comments_by_path(map, convert::comment)?)
    }

    /// `GET /changes/{n}/drafts`: the signed-in user's drafts on all patch
    /// sets, as [`Comment`]s without an author. Anonymous sessions have
    /// none, and no request is made.
    pub async fn get_drafts(&self, change: ChangeNumber) -> Result<Vec<Comment>, Error> {
        if !self.is_authenticated() {
            return Ok(Vec::new());
        }
        let v = self
            .get_json(&format!("/changes/{change}/drafts"), &[])
            .await?;
        let map: BTreeMap<String, Vec<wire::CommentInfo>> = serde_json::from_value(v)?;
        Ok(convert::comments_by_path(map, convert::comment)?)
    }

    /// `GET /changes/{n}/robotcomments`: comments of automated reviewers,
    /// with [`Comment::robot_id`] set. Gerrit 3.14 answers 404 to this
    /// request ([`Error::NotFound`]), so callers must not treat that as a
    /// missing change.
    pub async fn get_robot_comments(&self, change: ChangeNumber) -> Result<Vec<Comment>, Error> {
        let v = self
            .get_json(&format!("/changes/{change}/robotcomments"), &[])
            .await?;
        let map: BTreeMap<String, Vec<wire::RobotCommentInfo>> = serde_json::from_value(v)?;
        Ok(convert::comments_by_path(map, convert::robot_comment)?)
    }

    /// `PUT /changes/{n}/revisions/{rev}/drafts`: creates a draft and
    /// returns its server id. The server's answer leaves out the patch
    /// set, so it is not a full [`Comment`]. A patch set that does not
    /// exist is [`Error::NotFound`]; an empty message is
    /// [`Error::BadRequest`] with the server's text. (The server accepts a
    /// path that is not in the patch set and any line number.)
    pub async fn create_draft(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
        draft: &DraftInput,
    ) -> Result<DraftReceipt, Error> {
        let v = self
            .write_json(
                http::Method::PUT,
                &revision_path(change, target, "/drafts"),
                &convert::draft_input(draft),
            )
            .await?;
        let info: wire::CommentInfo = serde_json::from_value(v)?;
        Ok(convert::draft_receipt(&info)?)
    }

    /// `PUT /changes/{n}/revisions/{rev}/drafts/{id}`: replaces a draft.
    /// [`Error::NotFound`] if the draft is gone (published or deleted
    /// elsewhere).
    pub async fn update_draft(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
        draft_id: &str,
        draft: &DraftInput,
    ) -> Result<DraftReceipt, Error> {
        let v = self
            .write_json(
                http::Method::PUT,
                &revision_path(
                    change,
                    target,
                    &format!("/drafts/{}", encode_component(draft_id)),
                ),
                &convert::draft_input(draft),
            )
            .await?;
        let info: wire::CommentInfo = serde_json::from_value(v)?;
        Ok(convert::draft_receipt(&info)?)
    }

    /// `DELETE /changes/{n}/revisions/{rev}/drafts/{id}`. [`Error::NotFound`]
    /// means the draft does not exist (any more).
    pub async fn delete_draft(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
        draft_id: &str,
    ) -> Result<(), Error> {
        self.write_empty(
            http::Method::DELETE,
            &revision_path(
                change,
                target,
                &format!("/drafts/{}", encode_component(draft_id)),
            ),
        )
        .await
    }

    /// `POST /changes/{n}/revisions/{rev}/review`: posts a message, votes
    /// and drafts. Problems with the request (a label value the user may
    /// not set, a closed change) come back as [`Error::BadRequest`] or
    /// [`Error::Conflict`] carrying the server's text; a part of the
    /// request that failed although the review was accepted is in
    /// [`ReviewResult::error`].
    pub async fn post_review(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
        review: &ReviewInput,
    ) -> Result<ReviewResult, Error> {
        let v = self
            .write_json(
                http::Method::POST,
                &revision_path(change, target, "/review"),
                &convert::review_input(review),
            )
            .await?;
        let result: wire::ReviewResult = serde_json::from_value(v)?;
        Ok(convert::review_result(&result))
    }

    /// `PUT` or `DELETE /changes/{n}/revisions/{rev}/files/{file}/reviewed`:
    /// marks the file reviewed (or not) for the signed-in user. Marking an
    /// already marked file is not an error.
    pub async fn set_reviewed(
        &self,
        change: ChangeNumber,
        target: &RevisionId,
        path: &str,
        reviewed: bool,
    ) -> Result<(), Error> {
        let method = if reviewed {
            http::Method::PUT
        } else {
            http::Method::DELETE
        };
        self.write_empty(
            method,
            &revision_path(
                change,
                target,
                &format!("/files/{}/reviewed", encode_component(path)),
            ),
        )
        .await
    }

    /// `POST /changes/{n}/submit`: submits (merges) the change. A change
    /// that is not submittable, or a merge that fails, is
    /// [`Error::Conflict`] with the server's explanation. Returns the
    /// change as the server reports it afterwards.
    pub async fn submit(&self, change: ChangeNumber) -> Result<ChangeSummary, Error> {
        self.change_action(change, "/submit", &serde_json::json!({}))
            .await
    }

    /// `POST /changes/{n}/rebase`: rebases the current patch set onto
    /// `base` (a commit SHA or change number; `None` for the tip of the
    /// target branch). A conflict, or a change that is already up to date,
    /// is [`Error::Conflict`] with the server's message.
    pub async fn rebase(
        &self,
        change: ChangeNumber,
        base: Option<&str>,
    ) -> Result<ChangeSummary, Error> {
        let body = wire::RebaseInput {
            base: base
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(Into::into),
        };
        self.change_action(change, "/rebase", &body).await
    }

    /// `POST /changes/{n}/abandon` with an optional message.
    pub async fn abandon(
        &self,
        change: ChangeNumber,
        message: Option<&str>,
    ) -> Result<ChangeSummary, Error> {
        self.change_action(change, "/abandon", &message_input(message))
            .await
    }

    /// `POST /changes/{n}/restore` with an optional message.
    pub async fn restore(
        &self,
        change: ChangeNumber,
        message: Option<&str>,
    ) -> Result<ChangeSummary, Error> {
        self.change_action(change, "/restore", &message_input(message))
            .await
    }

    /// `POST /changes/{n}/wip` (`work_in_progress`) or `/ready`, with an
    /// optional message.
    pub async fn set_work_in_progress(
        &self,
        change: ChangeNumber,
        work_in_progress: bool,
        message: Option<&str>,
    ) -> Result<(), Error> {
        let path = if work_in_progress { "/wip" } else { "/ready" };
        self.require_auth()?;
        let bytes = serde_json::to_vec(&message_input(message))?;
        self.send(
            http::Method::POST,
            &format!("/changes/{change}{path}"),
            &[],
            Some(bytes),
        )
        .await?;
        Ok(())
    }

    /// `POST /changes/{n}/private` or `DELETE /changes/{n}/private`: marks
    /// the change private (or not).
    pub async fn set_private(&self, change: ChangeNumber, private: bool) -> Result<(), Error> {
        let path = format!("/changes/{change}/private");
        if private {
            self.require_auth()?;
            self.send(http::Method::POST, &path, &[], Some(b"{}".to_vec()))
                .await?;
            Ok(())
        } else {
            self.write_empty(http::Method::DELETE, &path).await
        }
    }

    /// `GET /changes/{n}/suggest_reviewers?q=..&n=..`: accounts and groups
    /// matching `query`, at most `limit` of them. Read-only.
    pub async fn suggest_reviewers(
        &self,
        change: ChangeNumber,
        query: &str,
        limit: u32,
    ) -> Result<Vec<SuggestedReviewer>, Error> {
        let v = self
            .get_json(
                &format!("/changes/{change}/suggest_reviewers"),
                &[("q", query.to_owned()), ("n", limit.to_string())],
            )
            .await?;
        let items: Vec<wire::SuggestedReviewerInfo> = serde_json::from_value(v)?;
        Ok(convert::suggested_reviewers(&items))
    }

    /// `POST /changes/{n}/reviewers`: adds `reviewer` (an account id, name,
    /// email or username, or a group UUID) as a reviewer or CC. Adding
    /// someone who is already on the change moves them to `state`.
    ///
    /// The server answers 200 even when it added nobody; its `error` and
    /// `confirm` are in the result (see [`AddReviewerResult::is_added`]).
    /// Pass `confirmed` to repeat the request after a `confirm`.
    pub async fn add_reviewer(
        &self,
        change: ChangeNumber,
        reviewer: &str,
        state: ReviewerState,
        confirmed: bool,
    ) -> Result<AddReviewerResult, Error> {
        let body = wire::ReviewerInput {
            reviewer: reviewer.trim().to_owned(),
            state: state.as_wire().to_owned(),
            confirmed: confirmed.then_some(true),
        };
        let v = self
            .write_json(
                http::Method::POST,
                &format!("/changes/{change}/reviewers"),
                &body,
            )
            .await?;
        let r: wire::ReviewerResult = serde_json::from_value(v)?;
        Ok(convert::add_reviewer_result(&r)?)
    }

    /// `DELETE /changes/{n}/reviewers/{account}`: removes a reviewer or CC.
    /// `reviewer` is an account id, email or username.
    pub async fn remove_reviewer(&self, change: ChangeNumber, reviewer: &str) -> Result<(), Error> {
        self.write_empty(
            http::Method::DELETE,
            &format!(
                "/changes/{change}/reviewers/{}",
                encode_component(reviewer.trim())
            ),
        )
        .await
    }

    /// `PUT /changes/{n}/topic`, or `DELETE` for a blank (or `None`) topic.
    pub async fn set_topic(&self, change: ChangeNumber, topic: Option<&str>) -> Result<(), Error> {
        let path = format!("/changes/{change}/topic");
        match topic.map(str::trim).filter(|t| !t.is_empty()) {
            Some(topic) => {
                self.require_auth()?;
                let body = serde_json::to_vec(&wire::TopicInput {
                    topic: topic.to_owned(),
                })?;
                self.send(http::Method::PUT, &path, &[], Some(body)).await?;
                Ok(())
            }
            None => self.write_empty(http::Method::DELETE, &path).await,
        }
    }

    /// `POST /changes/{n}/hashtags`: adds and removes hashtags and returns
    /// the change's hashtags afterwards. Blank entries are dropped.
    pub async fn edit_hashtags(
        &self,
        change: ChangeNumber,
        add: &[String],
        remove: &[String],
    ) -> Result<Vec<String>, Error> {
        let clean = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .collect()
        };
        let body = wire::HashtagsInput {
            add: clean(add),
            remove: clean(remove),
        };
        let v = self
            .write_json(
                http::Method::POST,
                &format!("/changes/{change}/hashtags"),
                &body,
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// A `POST` that answers with the changed `ChangeInfo`.
    async fn change_action<B: serde::Serialize>(
        &self,
        change: ChangeNumber,
        suffix: &str,
        body: &B,
    ) -> Result<ChangeSummary, Error> {
        let v = self
            .write_json(
                http::Method::POST,
                &format!("/changes/{change}{suffix}"),
                body,
            )
            .await?;
        let info: wire::ChangeInfo = serde_json::from_value(v)?;
        Ok(convert::change_summary(&info)?)
    }

    /// The signed-in user's diff settings: `GET /accounts/self/preferences.diff`,
    /// plus the diff view mode, which Gerrit keeps in
    /// `GET /accounts/self/preferences`. Anonymous access yields
    /// [`Error::Unauthorized`].
    pub async fn get_diff_preferences(&self) -> Result<DiffPreferences, Error> {
        // Preferences belong to an account. Say so without a request (the
        // server may answer 401 or 403 depending on its configuration).
        self.require_auth()?;
        let v = self
            .get_json("/accounts/self/preferences.diff", &[])
            .await?;
        let diff: wire::DiffPreferencesInfo = serde_json::from_value(v)?;
        let general = match self.get_json("/accounts/self/preferences", &[]).await {
            Ok(v) => Some(serde_json::from_value::<wire::GeneralPreferencesInfo>(v)?),
            Err(Error::NotFound) => None,
            Err(e) => return Err(e),
        };
        Ok(convert::diff_preferences(&diff, general.as_ref()))
    }
}

/// The body of abandon, restore, wip and ready: a blank message is left out.
fn message_input(message: Option<&str>) -> wire::MessageInput {
    wire::MessageInput {
        message: message
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(Into::into),
    }
}

/// `/changes/{n}/revisions/{sha}{suffix}`.
fn revision_path(change: ChangeNumber, rev: &RevisionId, suffix: &str) -> String {
    format!(
        "/changes/{change}/revisions/{}{suffix}",
        encode_component(rev.as_str())
    )
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

/// Maps an error response. 400 and 409 carry the server's explanation as a
/// plain-text body; it is kept whole (up to a sane limit) because the UI
/// shows it to the user.
fn error_for_status(code: u16, body: &[u8]) -> Error {
    let message = || {
        let text = String::from_utf8_lossy(body);
        text.trim().chars().take(2000).collect::<String>()
    };
    match code {
        400 => Error::BadRequest { message: message() },
        401 => Error::Unauthorized,
        403 => Error::Forbidden { message: message() },
        404 => Error::NotFound,
        409 => Error::Conflict { message: message() },
        _ => Error::Status {
            code,
            body: snippet(body),
        },
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
    use serde_json::json;

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
            (400, "badrequest"),
            (401, "unauthorized"),
            (404, "notfound"),
            (409, "conflict"),
            (500, "status"),
        ] {
            fake.push_response(code, "oops\n");
            match (block_on(c.server_version()), check) {
                (Err(Error::Unauthorized), "unauthorized") => {}
                (Err(Error::BadRequest { message }), "badrequest")
                | (Err(Error::Conflict { message }), "conflict") => assert_eq!(message, "oops"),
                (Err(Error::NotFound), "notfound") => {}
                (Err(Error::Status { code: c2, body }), "status") => {
                    assert_eq!((c2, body.as_str()), (500, "oops\n"));
                }
                (other, _) => panic!("{code}: {other:?}"),
            }
        }
        // Without credentials a 403 means "log in"; with them it is a
        // refusal that logging in again cannot fix.
        fake.push_response(403, "denied\n");
        assert!(matches!(
            block_on(c.server_version()),
            Err(Error::Unauthorized)
        ));
        let signed_in = client(&fake, "https://h", basic());
        fake.push_response(403, "denied\n");
        match block_on(signed_in.server_version()) {
            Err(Error::Forbidden { message }) => assert_eq!(message, "denied"),
            other => panic!("{other:?}"),
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
    fn get_change_reads_permitted_labels_label_values_and_attention_set() {
        let fake = FakeHttpClient::new();
        let json = CHANGE.replacen(
            '{',
            r#"{"permitted_labels": {"Code-Review": ["-1"," 0","+1","+2","bogus"], "Verified": []},"#,
            1,
        );
        let json = json.replace(
            r#""attention_set": {"3": {"account": {"_account_id": 3}}, "2": {}}"#,
            r#""attention_set": {
                "3": {"account": {"_account_id": 3, "name": "Three"},
                      "last_update": "2020-01-03 03:04:05.000000000",
                      "reason": "Added by Owner using the hovercard menu"},
                "2": {}}"#,
        );
        let json = json.replace(
            r#""labels": {"Code-Review": {"approved": {"_account_id": 2}}}"#,
            r#""labels": {"Code-Review": {"approved": {"_account_id": 2},
                "values": {"-1": "Needs work", " 0": "No score", "+2": "Looks good", "x": "?"}}}"#,
        );
        fake.push_response(200, xssi(&json));
        let c = client(&fake, "https://h", basic());
        let d = block_on(c.get_change(ChangeNumber(5))).unwrap();
        assert_eq!(d.permitted_labels["Code-Review"], [-1, 0, 1, 2]);
        assert!(d.permitted_labels["Verified"].is_empty());
        let values: Vec<_> = d.label_values["Code-Review"]
            .iter()
            .map(|v| (v.value, v.description.as_str()))
            .collect();
        assert_eq!(
            values,
            [(-1, "Needs work"), (0, "No score"), (2, "Looks good")]
        );
        // Sorted by account; the id comes from the key when the entry is empty.
        assert_eq!(d.attention.len(), 2);
        assert_eq!(d.attention[0].account.id, AccountId(2));
        assert_eq!(d.attention[0].reason, None);
        assert_eq!(d.attention[1].account.name.as_deref(), Some("Three"));
        assert_eq!(
            d.attention[1].reason.as_deref(),
            Some("Added by Owner using the hovercard menu")
        );
        assert!(d.attention[1].last_update.is_some());
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
             &o=SUBMIT_REQUIREMENTS&o=CURRENT_ACTIONS&o=CHANGE_ACTIONS"
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

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn sha() -> RevisionId {
        RevisionId::from(SHA)
    }

    #[test]
    fn files_are_sorted_and_mapped() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"{
                "src/z.rs": {"lines_inserted": 3, "size_delta": 40, "size": 400},
                "/COMMIT_MSG": {"lines_inserted": 7, "size_delta": 120, "size": 120},
                "new name.rs": {"status": "R", "old_path": "old/name.rs"},
                "assets/logo.png": {"status": "A", "binary": true, "size": 10, "size_delta": 10},
                "gone.rs": {"status": "D", "lines_deleted": 9, "size_delta": -90},
                "odd.rs": {"status": "Q", "future": 1},
                "/MERGE_LIST": {"lines_inserted": 2},
                ".gitignore": {}
            }"#,
            ),
        );
        let c = client(&fake, "https://h", Auth::Anonymous);
        let files =
            block_on(c.get_files(ChangeNumber(5), &sha(), Some(PatchSetNumber(2)))).unwrap();
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/COMMIT_MSG",
                "/MERGE_LIST",
                ".gitignore",
                "assets/logo.png",
                "gone.rs",
                "new name.rs",
                "odd.rs",
                "src/z.rs"
            ]
        );
        let by = |p: &str| files.iter().find(|f| f.path == p).unwrap();
        assert!(by("/COMMIT_MSG").is_special());
        assert_eq!(by("src/z.rs").status, model::FileStatus::Modified);
        assert_eq!(by("src/z.rs").lines_inserted, 3);
        assert_eq!(by("new name.rs").old_path.as_deref(), Some("old/name.rs"));
        assert_eq!(by("new name.rs").status, model::FileStatus::Renamed);
        assert!(by("assets/logo.png").binary);
        assert_eq!(by("gone.rs").size_delta, -90);
        assert_eq!(by("odd.rs").status, model::FileStatus::Unknown("Q".into()));
        assert_eq!(by(".gitignore").lines_deleted, 0);
        assert_eq!(
            fake.requests()[0].uri().to_string(),
            format!("https://h/changes/5/revisions/{SHA}/files?base=2")
        );
    }

    #[test]
    fn files_without_base_has_no_query() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi("{}"));
        let c = client(&fake, "https://h", Auth::Anonymous);
        assert!(
            block_on(c.get_files(ChangeNumber(5), &sha(), None))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fake.requests()[0].uri().to_string(),
            format!("https://h/changes/5/revisions/{SHA}/files")
        );
    }

    #[test]
    fn reviewed_files_use_a_bare_flag_and_skip_anonymous_sessions() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi(r#"["/COMMIT_MSG", "src/a b.rs"]"#));
        let c = client(&fake, "https://h", basic());
        let paths = block_on(c.get_reviewed_files(ChangeNumber(5), &sha())).unwrap();
        assert_eq!(paths, ["/COMMIT_MSG", "src/a b.rs"]);
        assert_eq!(
            fake.requests()[0].uri().to_string(),
            format!("https://h/a/changes/5/revisions/{SHA}/files?reviewed")
        );

        let anon = FakeHttpClient::new();
        let c = client(&anon, "https://h", Auth::Anonymous);
        assert!(
            block_on(c.get_reviewed_files(ChangeNumber(5), &sha()))
                .unwrap()
                .is_empty()
        );
        assert!(anon.requests().is_empty());
    }

    #[test]
    fn diff_paths_are_percent_encoded() {
        for (path, enc) in [
            ("/COMMIT_MSG", "%2FCOMMIT_MSG"),
            ("src/main/Foo.java", "src%2Fmain%2FFoo.java"),
            ("docs/my file (1).md", "docs%2Fmy%20file%20%281%29.md"),
            ("ü/日本.txt", "%C3%BC%2F%E6%97%A5%E6%9C%AC.txt"),
            ("a+b#c?d%e.txt", "a%2Bb%23c%3Fd%25e.txt"),
        ] {
            let fake = FakeHttpClient::new();
            fake.push_response(200, xssi("{}"));
            let c = client(&fake, "https://h", Auth::Anonymous);
            block_on(c.get_diff(ChangeNumber(5), &sha(), path, &DiffParams::default())).unwrap();
            let uri = fake.requests()[0].uri().to_string();
            assert!(
                uri.starts_with(&format!(
                    "https://h/changes/5/revisions/{SHA}/files/{enc}/diff?"
                )),
                "{path}: {uri}"
            );
            // The encoded path is a single path segment.
            assert_eq!(fake.requests()[0].uri().path().matches('/').count(), 7);
        }
    }

    #[test]
    fn diff_parameters() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi("{}"));
        fake.push_response(200, xssi("{}"));
        fake.push_response(200, xssi("{}"));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let get = |p: DiffParams| block_on(c.get_diff(ChangeNumber(5), &sha(), "f", &p)).unwrap();
        get(DiffParams::default());
        get(DiffParams {
            base: Some(PatchSetNumber(3)),
            intraline: true,
            whitespace: model::Whitespace::IgnoreAll,
        });
        get(DiffParams {
            base: None,
            intraline: false,
            whitespace: model::Whitespace::Unknown("X".into()),
        });
        let queries: Vec<_> = fake
            .requests()
            .iter()
            .map(|r| r.uri().query().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            queries,
            [
                "intraline&whitespace=IGNORE_NONE",
                "base=3&intraline&whitespace=IGNORE_ALL",
                "whitespace=X",
            ]
        );
    }

    #[test]
    fn diff_params_follow_the_cache_key() {
        let key = model::DiffKey {
            change: ChangeNumber(5),
            base: Some(model::PatchSetRef {
                number: PatchSetNumber(1),
                sha: "aaa".into(),
            }),
            target: model::PatchSetRef {
                number: PatchSetNumber(2),
                sha: "bbb".into(),
            },
            path: "f".into(),
            whitespace: model::Whitespace::IgnoreTrailing,
        };
        let p = DiffParams::for_key(&key);
        assert_eq!(p.base, Some(PatchSetNumber(1)));
        assert!(p.intraline);
        assert_eq!(p.whitespace, model::Whitespace::IgnoreTrailing);
    }

    #[test]
    fn diff_converts_chunks_and_edits() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"{
                "meta_a": {"name": "old.rs", "content_type": "text/x-rust", "lines": 5},
                "meta_b": {"name": "new.rs", "content_type": "text/x-rust", "lines": 6},
                "change_type": "RENAMED", "intraline_status": "OK",
                "diff_header": ["diff --git a/old.rs b/new.rs", "rename from old.rs"],
                "content": [
                    {"ab": ["fn main() {"]},
                    {"a": ["    let x = 1;"], "b": ["    let y = 2;", "    z();"],
                     "edit_a": [[8, 1], [4, 1]], "edit_b": [[8, 1], [4, 1]],
                     "due_to_rebase": true},
                    {"b": ["added"]},
                    {"skip": 40},
                    {"ab": ["}"]}
                ],
                "web_links": [{"name": "x", "url": "y"}]
            }"#,
            ),
        );
        let c = client(&fake, "https://h", Auth::Anonymous);
        let d = block_on(c.get_diff(ChangeNumber(5), &sha(), "new.rs", &DiffParams::default()))
            .unwrap();
        assert_eq!(d.change_type, Some(model::DiffChangeType::Renamed));
        assert_eq!(d.intraline_status, Some(model::IntralineStatus::Ok));
        assert_eq!(d.meta_a.as_ref().unwrap().name, "old.rs");
        assert_eq!(d.meta_b.as_ref().unwrap().lines, 6);
        assert_eq!(d.diff_header.len(), 2);
        assert!(!d.binary);
        assert_eq!(d.chunks.len(), 5);
        assert!(matches!(&d.chunks[0], model::DiffChunk::Common { lines } if lines.len() == 1));
        let model::DiffChunk::Changed(ch) = &d.chunks[1] else {
            panic!("{:?}", d.chunks[1]);
        };
        assert!(ch.due_to_rebase && !ch.common);
        assert_eq!(ch.a.len(), 1);
        assert_eq!(ch.b.len(), 2);
        assert_eq!(
            ch.edits_a,
            [
                model::IntralineEdit { skip: 8, mark: 1 },
                model::IntralineEdit { skip: 4, mark: 1 }
            ]
        );
        // "    let x = 1;": the marks are x (offset 8) and 1 (offset 12).
        assert_eq!(ch.marks_a(), vec![vec![8..9, 13..14]]);
        let model::DiffChunk::Changed(only_b) = &d.chunks[2] else {
            panic!();
        };
        assert!(only_b.a.is_empty() && only_b.b == ["added"]);
        assert_eq!(d.chunks[3], model::DiffChunk::Skip { lines: 40 });
        // 1 + 1 + 0 + 40 + 1 lines old side, 1 + 2 + 1 + 40 + 1 new side.
        assert_eq!((d.a_len(), d.b_len()), (43, 45));
    }

    #[test]
    fn diff_with_unknown_enums_and_binary_files() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"{"meta_b": {"name": "logo.png", "content_type": "image/png", "lines": 0},
                "change_type": "TELEPORTED", "intraline_status": "MAYBE", "binary": true}"#,
            ),
        );
        let c = client(&fake, "https://h", Auth::Anonymous);
        let d = block_on(c.get_diff(ChangeNumber(5), &sha(), "logo.png", &DiffParams::default()))
            .unwrap();
        assert!(d.binary && d.chunks.is_empty() && d.meta_a.is_none());
        assert_eq!(
            d.change_type,
            Some(model::DiffChangeType::Unknown("TELEPORTED".into()))
        );
        assert_eq!(
            d.intraline_status,
            Some(model::IntralineStatus::Unknown("MAYBE".into()))
        );
    }

    #[test]
    fn malformed_edit_pair_is_a_conversion_error() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(r#"{"content": [{"a": ["x"], "b": ["y"], "edit_a": [[1, 2, 3]]}]}"#),
        );
        let c = client(&fake, "https://h", Auth::Anonymous);
        let err =
            block_on(c.get_diff(ChangeNumber(5), &sha(), "f", &DiffParams::default())).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Conversion(convert::ConversionError::Invalid("content.edit_a"))
            ),
            "{err:?}"
        );
    }

    #[test]
    fn diff_of_a_missing_file_is_not_found() {
        let fake = FakeHttpClient::new();
        fake.push_response(404, "Not found: nope");
        let c = client(&fake, "https://h", Auth::Anonymous);
        assert!(matches!(
            block_on(c.get_diff(ChangeNumber(5), &sha(), "nope", &DiffParams::default())),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn related_changes() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"{"changes": [
                {"project": "p", "change_id": "I2", "_change_number": 8, "_revision_number": 1,
                 "_current_revision_number": 3, "status": "NEW", "submittable": true,
                 "commit": {"commit": "ccc", "subject": "child", "parents": [{"commit": "bbb"}]}},
                {"project": "p", "_change_number": 5, "_revision_number": 2,
                 "_current_revision_number": 2, "status": "WEIRD", "work_in_progress": true}
            ]}"#,
            ),
        );
        let c = client(&fake, "https://h", Auth::Anonymous);
        let r = block_on(c.get_related(ChangeNumber(5), &sha())).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].number, ChangeNumber(8));
        assert!(r[0].is_outdated() && !r[1].is_outdated());
        assert_eq!(r[0].status, Some(ChangeStatus::New));
        assert_eq!(r[0].submittable, Some(true));
        let commit = r[0].commit.as_ref().unwrap();
        assert_eq!(commit.sha.as_ref().map(|s| s.as_str()), Some("ccc"));
        assert_eq!(commit.subject, "child");
        assert_eq!(commit.parents[0].sha.as_str(), "bbb");
        assert!(r[1].work_in_progress && r[1].commit.is_none());
        assert_eq!(r[1].status, Some(ChangeStatus::Unknown("WEIRD".into())));
        assert_eq!(
            fake.requests()[0].uri().to_string(),
            format!("https://h/changes/5/revisions/{SHA}/related")
        );

        fake.push_response(200, xssi("{}"));
        assert!(
            block_on(c.get_related(ChangeNumber(5), &sha()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn related_without_change_number_is_a_conversion_error() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi(r#"{"changes": [{"project": "p"}]}"#));
        let c = client(&fake, "https://h", Auth::Anonymous);
        assert!(matches!(
            block_on(c.get_related(ChangeNumber(5), &sha())),
            Err(Error::Conversion(_))
        ));
    }

    #[test]
    fn diff_preferences_combine_diff_and_general_preferences() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"{"context": -1, "tab_size": 4, "line_length": 80, "show_tabs": false,
                "ignore_whitespace": "IGNORE_LEADING_AND_TRAILING", "intraline_difference": true,
                "syntax_highlighting": true, "font_size": 12, "theme": "DEFAULT"}"#,
            ),
        );
        fake.push_response(
            200,
            xssi(r#"{"diff_view": "UNIFIED_DIFF", "theme": "DARK"}"#),
        );
        let c = client(&fake, "https://h", basic());
        let p = block_on(c.get_diff_preferences()).unwrap();
        assert_eq!(p.context, Some(model::DiffContext::All));
        assert_eq!(p.tab_size, Some(4));
        assert_eq!(p.line_length, Some(80));
        assert_eq!(p.show_tabs, Some(false));
        assert_eq!(
            p.ignore_whitespace,
            Some(model::Whitespace::IgnoreLeadingAndTrailing)
        );
        assert_eq!(p.view_mode, Some(model::DiffViewMode::Unified));
        assert_eq!(p.line_wrapping, None);
        let uris: Vec<_> = fake
            .requests()
            .iter()
            .map(|r| r.uri().to_string())
            .collect();
        assert_eq!(
            uris,
            [
                "https://h/a/accounts/self/preferences.diff",
                "https://h/a/accounts/self/preferences"
            ]
        );
    }

    #[test]
    fn diff_preferences_tolerate_missing_general_preferences_and_odd_values() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(r#"{"context": 5, "ignore_whitespace": "IGNORE_SOME"}"#),
        );
        fake.push_response(404, "");
        let c = client(&fake, "https://h", basic());
        let p = block_on(c.get_diff_preferences()).unwrap();
        assert_eq!(p.context, Some(model::DiffContext::Lines(5)));
        assert_eq!(
            p.ignore_whitespace,
            Some(model::Whitespace::Unknown("IGNORE_SOME".into()))
        );
        assert_eq!(p.view_mode, None);

        // An unknown diff view is not an error either.
        fake.push_response(200, xssi("{}"));
        fake.push_response(200, xssi(r#"{"diff_view": "HOLOGRAM"}"#));
        assert_eq!(block_on(c.get_diff_preferences()).unwrap().view_mode, None);
    }

    #[test]
    fn diff_preferences_anonymous_is_unauthorized() {
        let fake = FakeHttpClient::new();
        let c = client(&fake, "https://h", Auth::Anonymous);
        assert!(matches!(
            block_on(c.get_diff_preferences()),
            Err(Error::Unauthorized)
        ));
        assert!(fake.requests().is_empty());
    }

    const COMMENTS: &str = r#"{
        "src/b.rs": [
            {"patch_set": 1, "id": "c1", "line": 4, "updated": "2024-03-01 10:00:00.000000000",
             "message": "Why?", "unresolved": true, "author": {"_account_id": 7, "name": "Bob"},
             "commit_id": "aaa"},
            {"patch_set": 1, "id": "c2", "line": 4, "in_reply_to": "c1", "side": "PARENT",
             "parent": 2, "updated": "2024-03-01 11:00:00.000000000", "message": "Because.",
             "unresolved": false, "tag": "mytag", "change_message_id": "m1"}
        ],
        "/COMMIT_MSG": [
            {"patch_set": 2, "id": "c3", "updated": "2024-03-02 10:00:00.000000000",
             "message": "typo", "range": {"start_line": 2, "start_character": 3,
              "end_line": 2, "end_character": 9}, "line": 2,
             "context_lines": [{"line_number": 2, "context_line": "Subject"}, {}]}
        ],
        "/PATCHSET_LEVEL": [
            {"patch_set": 3, "id": "c4", "updated": "2024-03-03 10:00:00.000000000",
             "message": "Overall fine", "in_reply_to": ""}
        ]
    }"#;

    fn sha_rev() -> RevisionId {
        RevisionId::from("0123abcd")
    }

    #[test]
    fn comments_are_flattened_by_path_with_special_files_first() {
        use model::{Location, Side};
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi(COMMENTS));
        let c = client(&fake, "https://h", Auth::Anonymous);
        let cs = block_on(c.get_comments(ChangeNumber(5))).unwrap();
        assert_eq!(fake.requests()[0].uri(), "https://h/changes/5/comments");
        let order: Vec<_> = cs
            .iter()
            .map(|c| (c.path.as_str(), c.id.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("/COMMIT_MSG", "c3"),
                ("/PATCHSET_LEVEL", "c4"),
                ("src/b.rs", "c1"),
                ("src/b.rs", "c2")
            ]
        );
        // c3: range wins over line; context lines without a number dropped.
        assert!(matches!(cs[0].location, Location::Range(r) if r.start_character == 3));
        assert_eq!(cs[0].context.len(), 1);
        assert_eq!(cs[0].side, Side::Revision);
        // c4: an empty in_reply_to is none; no location is a file comment.
        assert_eq!(cs[1].in_reply_to, None);
        assert_eq!(cs[1].location, Location::File);
        assert!(cs[1].is_patch_set_level());
        // c1/c2.
        assert_eq!(cs[2].location, Location::Line(4));
        assert!(cs[2].unresolved);
        assert_eq!(cs[2].author.as_ref().map(|a| a.id.0), Some(7));
        assert_eq!(cs[2].commit_id, Some(RevisionId::from("aaa")));
        assert_eq!(cs[3].side, Side::Parent);
        assert_eq!(cs[3].parent, Some(2));
        assert_eq!(cs[3].in_reply_to.as_deref(), Some("c1"));
        assert_eq!(cs[3].tag.as_deref(), Some("mytag"));
        assert_eq!(cs[3].change_message_id.as_deref(), Some("m1"));
        assert!(!cs[3].unresolved);
    }

    #[test]
    fn comments_with_unknown_side_and_extra_fields_still_load() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"{"a": [{"patch_set": 1, "id": "x", "updated": "2024-03-01 10:00:00.000000000",
                    "side": "SIDEWAYS", "future": {"a": 1}, "fix_suggestions": [{"fix_id": "f"}]}]}"#,
            ),
        );
        let c = client(&fake, "https://h", Auth::Anonymous);
        let cs = block_on(c.get_comments(ChangeNumber(1))).unwrap();
        assert_eq!(cs[0].side, model::Side::Revision);
        assert_eq!(cs[0].message, "");
    }

    #[test]
    fn comment_missing_required_fields_is_a_conversion_error() {
        for bad in [
            r#"{"a": [{"id": "x", "updated": "2024-03-01 10:00:00.000000000"}]}"#,
            r#"{"a": [{"patch_set": 1, "updated": "2024-03-01 10:00:00.000000000"}]}"#,
            r#"{"a": [{"patch_set": 1, "id": "x"}]}"#,
            r#"{"a": [{"patch_set": 1, "id": "x", "updated": "2024-03-01 10:00:00.000000000",
                "range": {"start_line": 1}}]}"#,
        ] {
            let fake = FakeHttpClient::new();
            fake.push_response(200, xssi(bad));
            let c = client(&fake, "https://h", Auth::Anonymous);
            assert!(
                matches!(
                    block_on(c.get_comments(ChangeNumber(1))),
                    Err(Error::Conversion(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn drafts_need_an_account_and_come_without_authors() {
        let fake = FakeHttpClient::new();
        let anon = client(&fake, "https://h", Auth::Anonymous);
        assert!(
            block_on(anon.get_drafts(ChangeNumber(5)))
                .unwrap()
                .is_empty()
        );
        assert!(fake.requests().is_empty());

        fake.push_response(
            200,
            xssi(
                r#"{"f.rs": [{"patch_set": 2, "id": "d1", "line": 3,
                    "updated": "2024-03-01 10:00:00.000000000", "message": "wip",
                    "unresolved": true, "in_reply_to": "c1"}]}"#,
            ),
        );
        let c = client(&fake, "https://h", basic());
        let ds = block_on(c.get_drafts(ChangeNumber(5))).unwrap();
        assert_eq!(fake.requests()[0].uri(), "https://h/a/changes/5/drafts");
        assert_eq!((ds[0].id.as_str(), ds[0].path.as_str()), ("d1", "f.rs"));
        assert!(ds[0].author.is_none() && ds[0].unresolved);
    }

    #[test]
    fn robot_comments_carry_the_robot_id() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"{"f.rs": [{"patch_set": 1, "id": "r1", "robot_id": "lint", "robot_run_id": "9",
                    "url": "https://x", "properties": {"a": "b"}, "line": 1,
                    "updated": "2024-03-01 10:00:00.000000000", "message": "unused"}]}"#,
            ),
        );
        let c = client(&fake, "https://h", Auth::Anonymous);
        let cs = block_on(c.get_robot_comments(ChangeNumber(5))).unwrap();
        assert_eq!(
            fake.requests()[0].uri(),
            "https://h/changes/5/robotcomments"
        );
        assert!(cs[0].is_robot());
        assert_eq!(cs[0].robot_id.as_deref(), Some("lint"));
    }

    fn draft_input() -> DraftInput {
        DraftInput {
            path: "src/a b.rs".into(),
            side: model::Side::Revision,
            parent: None,
            location: model::Location::Line(7),
            in_reply_to: Some("c1".into()),
            message: "Fix \"this\"\nüñí".into(),
            unresolved: true,
        }
    }

    fn body_json(req: &http::Request<Vec<u8>>) -> Value {
        serde_json::from_slice(req.body()).unwrap()
    }

    #[test]
    fn create_draft_puts_json_to_the_revision() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            201,
            xssi(
                // As the real server answers: no patch set, no author.
                r#"{"id": "d9", "path": "src/a b.rs", "line": 7, "commit_id": "0123abcd",
                    "updated": "2024-03-01 10:00:00.000000000", "message": "m",
                    "in_reply_to": "c1", "unresolved": true}"#,
            ),
        );
        let c = client(&fake, "https://h", basic());
        let got = block_on(c.create_draft(ChangeNumber(5), &sha_rev(), &draft_input())).unwrap();
        assert_eq!(got.id, "d9");
        let req = &fake.requests()[0];
        assert_eq!(req.method(), http::Method::PUT);
        assert_eq!(req.uri(), "https://h/a/changes/5/revisions/0123abcd/drafts");
        assert_eq!(
            req.headers()["content-type"],
            "application/json; charset=UTF-8"
        );
        assert!(req.headers().contains_key("authorization"));
        assert_eq!(
            body_json(req),
            json!({"path": "src/a b.rs", "side": "REVISION", "line": 7, "in_reply_to": "c1",
                   "message": "Fix \"this\"\nüñí", "unresolved": true})
        );
    }

    #[test]
    fn range_drafts_send_the_range_and_its_end_line() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            201,
            xssi(r#"{"id": "d1", "patch_set": 1, "updated": "2024-03-01 10:00:00.000000000"}"#),
        );
        let c = client(&fake, "https://h", basic());
        let mut d = draft_input();
        d.side = model::Side::Parent;
        d.parent = Some(2);
        d.in_reply_to = None;
        d.location = model::Location::Range(model::CommentRange {
            start_line: 3,
            start_character: 1,
            end_line: 5,
            end_character: 4,
        });
        let got = block_on(c.create_draft(ChangeNumber(5), &sha_rev(), &d)).unwrap();
        assert_eq!(got.id, "d1");
        assert_eq!(
            body_json(&fake.requests()[0]),
            json!({"path": "src/a b.rs", "side": "PARENT", "parent": 2, "line": 5,
                   "range": {"start_line": 3, "start_character": 1, "end_line": 5,
                             "end_character": 4},
                   "message": "Fix \"this\"\nüñí", "unresolved": true})
        );
        // A file comment has neither line nor range.
        fake.push_response(
            201,
            xssi(r#"{"id": "d2", "patch_set": 1, "updated": "2024-03-01 10:00:00.000000000"}"#),
        );
        d.location = model::Location::File;
        block_on(c.create_draft(ChangeNumber(5), &sha_rev(), &d)).unwrap();
        let sent = body_json(&fake.requests()[1]);
        assert!(sent.get("line").is_none() && sent.get("range").is_none());
    }

    #[test]
    fn patch_set_level_drafts_send_no_side() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            201,
            xssi(r#"{"id": "d1", "updated": "2024-03-01 10:00:00.000000000"}"#),
        );
        let c = client(&fake, "https://h", basic());
        let mut d = draft_input();
        d.path = "/PATCHSET_LEVEL".into();
        d.location = model::Location::File;
        d.parent = Some(1);
        block_on(c.create_draft(ChangeNumber(5), &sha_rev(), &d)).unwrap();
        assert_eq!(
            body_json(&fake.requests()[0]),
            json!({"path": "/PATCHSET_LEVEL", "in_reply_to": "c1",
                   "message": "Fix \"this\"\nüñí", "unresolved": true})
        );
    }

    #[test]
    fn update_and_delete_draft_address_the_draft_id() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(r#"{"id": "d/1", "patch_set": 2, "updated": "2024-03-01 10:00:00.000000000"}"#),
        );
        fake.push_response(204, "");
        fake.push_response(404, "Not Found: d1");
        let c = client(&fake, "https://h", basic());
        block_on(c.update_draft(ChangeNumber(5), &sha_rev(), "d/1", &draft_input())).unwrap();
        block_on(c.delete_draft(ChangeNumber(5), &sha_rev(), "d/1")).unwrap();
        assert!(matches!(
            block_on(c.delete_draft(ChangeNumber(5), &sha_rev(), "d/1")),
            Err(Error::NotFound)
        ));
        let reqs = fake.requests();
        assert_eq!(reqs[0].method(), http::Method::PUT);
        assert_eq!(
            reqs[0].uri(),
            "https://h/a/changes/5/revisions/0123abcd/drafts/d%2F1"
        );
        assert_eq!(reqs[1].method(), http::Method::DELETE);
        assert!(reqs[1].body().is_empty());
        assert!(!reqs[1].headers().contains_key("content-type"));
    }

    #[test]
    fn writes_are_refused_for_anonymous_sessions_without_a_request() {
        let fake = FakeHttpClient::new();
        let c = client(&fake, "https://h", Auth::Anonymous);
        let n = ChangeNumber(5);
        assert!(matches!(
            block_on(c.create_draft(n, &sha_rev(), &draft_input())),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(c.update_draft(n, &sha_rev(), "d", &draft_input())),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(c.delete_draft(n, &sha_rev(), "d")),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(c.post_review(n, &sha_rev(), &ReviewInput::default())),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(c.set_reviewed(n, &sha_rev(), "f", true)),
            Err(Error::Unauthorized)
        ));
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn write_errors_carry_the_servers_message() {
        let fake = FakeHttpClient::new();
        let c = client(&fake, "https://h", basic());
        fake.push_response(409, "change is closed\n");
        match block_on(c.create_draft(ChangeNumber(5), &sha_rev(), &draft_input())) {
            Err(Error::Conflict { message }) => assert_eq!(message, "change is closed"),
            other => panic!("{other:?}"),
        }
        fake.push_response(400, "label \"Code-Review\": 2 is not a permitted value");
        match block_on(c.post_review(ChangeNumber(5), &sha_rev(), &ReviewInput::default())) {
            Err(Error::BadRequest { message }) => {
                assert!(message.contains("not a permitted value"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        fake.push_response(403, "not permitted: write on refs/heads/x");
        assert!(matches!(
            block_on(c.set_reviewed(ChangeNumber(5), &sha_rev(), "f", true)),
            Err(Error::Forbidden { .. })
        ));
        fake.push_response(401, "");
        assert!(matches!(
            block_on(c.delete_draft(ChangeNumber(5), &sha_rev(), "d")),
            Err(Error::Unauthorized)
        ));
        fake.push_error("connection reset");
        let e = block_on(c.delete_draft(ChangeNumber(5), &sha_rev(), "d")).unwrap_err();
        assert!(e.is_retryable());
    }

    #[test]
    fn post_review_sends_labels_drafts_and_attention_set() {
        use model::{AttentionSetInput, DraftHandling, NotifyHandling};
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(r#"{"labels": {"Code-Review": 1}, "ready": true, "reviewers": {}}"#),
        );
        let c = client(&fake, "https://h", basic());
        let mut review = ReviewInput::with_message("Looks good");
        review.labels.insert("Code-Review".into(), 1);
        review.notify = Some(NotifyHandling::Owner);
        review.tag = Some("t".into());
        review.add_to_attention_set = vec![AttentionSetInput {
            user: AccountId(4),
            reason: "please look".into(),
        }];
        review.remove_from_attention_set = vec![AttentionSetInput {
            user: AccountId(5),
            reason: "done".into(),
        }];
        review.ready = Some(true);
        let res = block_on(c.post_review(ChangeNumber(5), &sha_rev(), &review)).unwrap();
        assert_eq!(res.labels["Code-Review"], 1);
        assert_eq!(res.ready, Some(true));
        assert_eq!(res.error, None);
        let req = &fake.requests()[0];
        assert_eq!(req.method(), http::Method::POST);
        assert_eq!(req.uri(), "https://h/a/changes/5/revisions/0123abcd/review");
        assert_eq!(
            body_json(req),
            json!({
                "message": "Looks good",
                "labels": {"Code-Review": 1},
                "drafts": "PUBLISH_ALL_REVISIONS",
                "tag": "t",
                "notify": "OWNER",
                "ready": true,
                "add_to_attention_set": [{"user": "4", "reason": "please look"}],
                "remove_from_attention_set": [{"user": "5", "reason": "done"}],
            })
        );

        // Empty message and labels are left out; drafts handling is kept.
        fake.push_response(200, xssi("{}"));
        let mut quiet = ReviewInput::with_message("  ");
        quiet.drafts = DraftHandling::Keep;
        block_on(c.post_review(ChangeNumber(5), &sha_rev(), &quiet)).unwrap();
        assert_eq!(body_json(&fake.requests()[1]), json!({"drafts": "KEEP"}));
    }

    #[test]
    fn review_result_error_is_reported_not_raised() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(r#"{"error": "reviewer x not found", "labels": {}}"#),
        );
        let c = client(&fake, "https://h", basic());
        let res =
            block_on(c.post_review(ChangeNumber(5), &sha_rev(), &ReviewInput::default())).unwrap();
        assert_eq!(res.error.as_deref(), Some("reviewer x not found"));
    }

    #[test]
    fn reviewed_marks_use_put_and_delete_with_an_encoded_path() {
        let fake = FakeHttpClient::new();
        fake.push_response(201, "");
        fake.push_response(200, "OK");
        fake.push_response(204, "");
        let c = client(&fake, "https://h", basic());
        block_on(c.set_reviewed(ChangeNumber(5), &sha_rev(), "src/a b.rs", true)).unwrap();
        // Already marked: 200 with a plain-text body is fine.
        block_on(c.set_reviewed(ChangeNumber(5), &sha_rev(), "src/a b.rs", true)).unwrap();
        block_on(c.set_reviewed(ChangeNumber(5), &sha_rev(), "src/a b.rs", false)).unwrap();
        let reqs = fake.requests();
        let methods: Vec<_> = reqs.iter().map(|r| r.method().as_str()).collect();
        assert_eq!(methods, ["PUT", "PUT", "DELETE"]);
        assert_eq!(
            reqs[0].uri(),
            "https://h/a/changes/5/revisions/0123abcd/files/src%2Fa%20b.rs/reviewed"
        );
        assert!(reqs[0].body().is_empty());
    }

    const ACTION_CHANGE: &str = r#"{
        "project": "p", "branch": "main", "change_id": "I1", "subject": "s",
        "status": "ABANDONED", "created": "2020-01-02 03:04:05.000000000",
        "updated": "2020-01-03 03:04:05.500000000", "_number": 5,
        "owner": {"_account_id": 1}}"#;

    fn action_fake(response: &str) -> (FakeHttpClient, GerritClient) {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi(response));
        let c = client(&fake, "https://h", basic());
        (fake, c)
    }

    #[test]
    fn get_change_reads_change_and_revision_actions() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"{
            "project": "p", "branch": "main", "change_id": "I1", "subject": "s",
            "status": "NEW", "created": "2020-01-02 03:04:05.000000000",
            "updated": "2020-01-03 03:04:05.500000000", "_number": 5,
            "owner": {"_account_id": 1}, "current_revision": "bbb",
            "actions": {
                "abandon": {"method": "POST", "label": "Abandon", "title": "Abandon", "enabled": true},
                "plugin~odd": {"enabled_options": ["a"], "future": 1}
            },
            "revisions": {
                "bbb": {"_number": 2, "created": "2020-01-03 00:00:00.000000000",
                    "actions": {
                        "submit": {"method": "POST", "label": "Submit", "enabled": true},
                        "rebase": {"method": "POST", "enabled_options": ["rebase"]}
                    }},
                "aaa": {"_number": 1, "created": "2020-01-02 00:00:00.000000000",
                    "actions": {"submit": {"enabled": true}}}
            }}"#,
            ),
        );
        let c = client(&fake, "https://h", basic());
        let d = block_on(c.get_change(ChangeNumber(5))).unwrap();
        let uri = fake.requests()[0].uri().to_string();
        assert!(uri.contains("o=CURRENT_ACTIONS") && uri.contains("o=CHANGE_ACTIONS"));
        assert!(d.action("abandon").unwrap().enabled);
        assert_eq!(d.action("submit").unwrap().label.as_deref(), Some("Submit"));
        // Absent `enabled` means disabled; unknown ids are kept.
        assert!(!d.action("rebase").unwrap().enabled);
        assert_eq!(d.action("plugin~odd").unwrap().enabled_options, ["a"]);
        // Only the current revision's actions count.
        assert_eq!(d.revision_actions.len(), 2);
    }

    #[test]
    fn change_without_actions_has_empty_maps() {
        let (_fake, c) = action_fake(ACTION_CHANGE);
        let d = block_on(c.get_change(ChangeNumber(5))).unwrap();
        assert!(d.change_actions.is_empty() && d.revision_actions.is_empty());
    }

    #[test]
    fn abandon_and_restore_post_an_optional_message() {
        let (fake, c) = action_fake(ACTION_CHANGE);
        let s = block_on(c.abandon(ChangeNumber(5), Some(" gone "))).unwrap();
        assert_eq!(s.status, ChangeStatus::Abandoned);
        fake.push_response(200, xssi(ACTION_CHANGE));
        block_on(c.restore(ChangeNumber(5), Some("back"))).unwrap();
        fake.push_response(200, xssi(ACTION_CHANGE));
        block_on(c.abandon(ChangeNumber(5), Some("  "))).unwrap();
        let reqs = fake.requests();
        assert_eq!(reqs[0].method(), http::Method::POST);
        assert_eq!(reqs[0].uri(), "https://h/a/changes/5/abandon");
        assert_eq!(body_json(&reqs[0]), json!({"message": "gone"}));
        assert_eq!(reqs[1].uri(), "https://h/a/changes/5/restore");
        assert_eq!(body_json(&reqs[1]), json!({"message": "back"}));
        assert_eq!(body_json(&reqs[2]), json!({}));
    }

    #[test]
    fn submit_posts_an_empty_object() {
        let (fake, c) = action_fake(ACTION_CHANGE);
        block_on(c.submit(ChangeNumber(5))).unwrap();
        let req = &fake.requests()[0];
        assert_eq!(req.method(), http::Method::POST);
        assert_eq!(req.uri(), "https://h/a/changes/5/submit");
        assert_eq!(body_json(req), json!({}));
    }

    #[test]
    fn rebase_sends_the_base_only_when_given() {
        let (fake, c) = action_fake(ACTION_CHANGE);
        block_on(c.rebase(ChangeNumber(5), None)).unwrap();
        fake.push_response(200, xssi(ACTION_CHANGE));
        block_on(c.rebase(ChangeNumber(5), Some("abc123"))).unwrap();
        fake.push_response(200, xssi(ACTION_CHANGE));
        block_on(c.rebase(ChangeNumber(5), Some(" "))).unwrap();
        let reqs = fake.requests();
        assert_eq!(reqs[0].uri(), "https://h/a/changes/5/rebase");
        assert_eq!(body_json(&reqs[0]), json!({}));
        assert_eq!(body_json(&reqs[1]), json!({"base": "abc123"}));
        assert_eq!(body_json(&reqs[2]), json!({}));
    }

    #[test]
    fn wip_ready_and_private_use_their_endpoints() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, "");
        fake.push_response(200, "");
        fake.push_response(201, "");
        fake.push_response(204, "");
        let c = client(&fake, "https://h", basic());
        block_on(c.set_work_in_progress(ChangeNumber(5), true, Some("later"))).unwrap();
        block_on(c.set_work_in_progress(ChangeNumber(5), false, None)).unwrap();
        block_on(c.set_private(ChangeNumber(5), true)).unwrap();
        block_on(c.set_private(ChangeNumber(5), false)).unwrap();
        let reqs = fake.requests();
        let seen: Vec<_> = reqs
            .iter()
            .map(|r| (r.method().as_str(), r.uri().path()))
            .collect();
        assert_eq!(
            seen,
            [
                ("POST", "/a/changes/5/wip"),
                ("POST", "/a/changes/5/ready"),
                ("POST", "/a/changes/5/private"),
                ("DELETE", "/a/changes/5/private"),
            ]
        );
        assert_eq!(body_json(&reqs[0]), json!({"message": "later"}));
        assert_eq!(body_json(&reqs[1]), json!({}));
        assert!(reqs[3].body().is_empty());
    }

    #[test]
    fn refused_actions_keep_401_403_and_409_apart() {
        let fake = FakeHttpClient::new();
        let c = client(&fake, "https://h", basic());
        fake.push_response(409, "change is not submittable\n");
        match block_on(c.submit(ChangeNumber(5))) {
            Err(Error::Conflict { message }) => assert_eq!(message, "change is not submittable"),
            other => panic!("{other:?}"),
        }
        fake.push_response(409, "The change could not be rebased due to a conflict\n");
        assert!(matches!(
            block_on(c.rebase(ChangeNumber(5), None)),
            Err(Error::Conflict { message }) if message.contains("conflict")
        ));
        fake.push_response(403, "restore not permitted\n");
        assert!(matches!(
            block_on(c.restore(ChangeNumber(5), None)),
            Err(Error::Forbidden { message }) if message == "restore not permitted"
        ));
        fake.push_response(401, "");
        assert!(matches!(
            block_on(c.set_private(ChangeNumber(5), true)),
            Err(Error::Unauthorized)
        ));
        // Anonymous sessions never send a request.
        let anon_fake = FakeHttpClient::new();
        let anon = client(&anon_fake, "https://h", Auth::Anonymous);
        assert!(matches!(
            block_on(anon.submit(ChangeNumber(5))),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(anon.set_work_in_progress(ChangeNumber(5), true, None)),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(anon.set_private(ChangeNumber(5), true)),
            Err(Error::Unauthorized)
        ));
        assert!(anon_fake.requests().is_empty());
    }

    #[test]
    fn suggest_reviewers_sends_q_and_n_and_keeps_groups() {
        let fake = FakeHttpClient::new();
        fake.push_response(
            200,
            xssi(
                r#"[{"account":{"_account_id":3,"name":"Bob","email":"b@x"},"count":1},
                    {"group":{"id":"abc","name":"Joiners"},"count":50,"confirm":true},
                    {"count":2},
                    {"group":{"name":"no id"}}]"#,
            ),
        );
        let c = client(&fake, "https://h", basic());
        let got = block_on(c.suggest_reviewers(ChangeNumber(5), "b o&", 7)).unwrap();
        let req = &fake.requests()[0];
        assert_eq!(req.method(), http::Method::GET);
        assert_eq!(
            req.uri(),
            "https://h/a/changes/5/suggest_reviewers?q=b%20o%26&n=7"
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].reviewer_input(), "3");
        match &got[1] {
            SuggestedReviewer::Group {
                group,
                count,
                confirm,
            } => {
                assert_eq!(
                    (group.id.as_str(), *count, *confirm),
                    ("abc", Some(50), true)
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn add_reviewer_posts_state_and_surfaces_error_and_confirm() {
        let fake = FakeHttpClient::new();
        let c = client(&fake, "https://h", basic());
        fake.push_response(
            200,
            xssi(r#"{"input":"bob","reviewers":[{"_account_id":3,"name":"Bob"}]}"#),
        );
        let r = block_on(c.add_reviewer(ChangeNumber(5), " bob ", ReviewerState::Reviewer, false))
            .unwrap();
        assert!(r.is_added());
        assert_eq!(r.reviewers[0].id.0, 3);
        fake.push_response(
            200,
            xssi(r#"{"input":"zed","error":"Account 'zed' not found"}"#),
        );
        let r = block_on(c.add_reviewer(ChangeNumber(5), "zed", ReviewerState::Cc, false)).unwrap();
        assert!(!r.is_added());
        assert_eq!(r.error.as_deref(), Some("Account 'zed' not found"));
        fake.push_response(
            200,
            xssi(r#"{"input":"grp","confirm":true,"error":"has 50 members"}"#),
        );
        let r = block_on(c.add_reviewer(ChangeNumber(5), "grp", ReviewerState::Reviewer, false))
            .unwrap();
        assert!(r.confirm && !r.is_added());
        fake.push_response(200, xssi(r#"{"input":"grp","reviewers":[]}"#));
        block_on(c.add_reviewer(ChangeNumber(5), "grp", ReviewerState::Reviewer, true)).unwrap();

        let reqs = fake.requests();
        assert_eq!(reqs[0].method(), http::Method::POST);
        assert_eq!(reqs[0].uri(), "https://h/a/changes/5/reviewers");
        assert_eq!(
            body_json(&reqs[0]),
            json!({"reviewer": "bob", "state": "REVIEWER"})
        );
        assert_eq!(
            body_json(&reqs[1]),
            json!({"reviewer": "zed", "state": "CC"})
        );
        assert_eq!(
            body_json(&reqs[3]),
            json!({"reviewer": "grp", "state": "REVIEWER", "confirmed": true})
        );
    }

    #[test]
    fn remove_reviewer_deletes_with_an_encoded_account() {
        let fake = FakeHttpClient::new();
        fake.push_response(204, "");
        fake.push_response(204, "");
        let c = client(&fake, "https://h", basic());
        block_on(c.remove_reviewer(ChangeNumber(5), "3")).unwrap();
        block_on(c.remove_reviewer(ChangeNumber(5), "a+b@x.org")).unwrap();
        let reqs = fake.requests();
        assert_eq!(reqs[0].method(), http::Method::DELETE);
        assert_eq!(reqs[0].uri(), "https://h/a/changes/5/reviewers/3");
        assert_eq!(
            reqs[1].uri(),
            "https://h/a/changes/5/reviewers/a%2Bb%40x.org"
        );
        assert!(reqs[0].body().is_empty());
    }

    #[test]
    fn topic_is_put_or_deleted_when_blank() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi(r#""my topic""#));
        fake.push_response(204, "");
        fake.push_response(204, "");
        fake.push_response(204, "");
        let c = client(&fake, "https://h", basic());
        block_on(c.set_topic(ChangeNumber(5), Some(" my topic "))).unwrap();
        block_on(c.set_topic(ChangeNumber(5), None)).unwrap();
        block_on(c.set_topic(ChangeNumber(5), Some("  "))).unwrap();
        let reqs = fake.requests();
        assert_eq!(reqs[0].method(), http::Method::PUT);
        assert_eq!(reqs[0].uri(), "https://h/a/changes/5/topic");
        assert_eq!(body_json(&reqs[0]), json!({"topic": "my topic"}));
        assert_eq!(reqs[1].method(), http::Method::DELETE);
        assert_eq!(reqs[2].method(), http::Method::DELETE);
        assert!(reqs[1].body().is_empty());
    }

    #[test]
    fn hashtags_post_add_and_remove_and_return_the_list() {
        let fake = FakeHttpClient::new();
        fake.push_response(200, xssi(r#"["a","b"]"#));
        fake.push_response(200, xssi("[]"));
        let c = client(&fake, "https://h", basic());
        let tags = block_on(c.edit_hashtags(
            ChangeNumber(5),
            &["a".into(), " ".into(), " b ".into()],
            &["old".into()],
        ))
        .unwrap();
        assert_eq!(tags, ["a", "b"]);
        block_on(c.edit_hashtags(ChangeNumber(5), &[], &["a".into()])).unwrap();
        let reqs = fake.requests();
        assert_eq!(reqs[0].method(), http::Method::POST);
        assert_eq!(reqs[0].uri(), "https://h/a/changes/5/hashtags");
        assert_eq!(
            body_json(&reqs[0]),
            json!({"add": ["a", "b"], "remove": ["old"]})
        );
        assert_eq!(body_json(&reqs[1]), json!({"remove": ["a"]}));
    }

    #[test]
    fn reviewer_topic_and_hashtag_refusals_and_anonymous() {
        let fake = FakeHttpClient::new();
        let c = client(&fake, "https://h", basic());
        fake.push_response(403, "not allowed\n");
        assert!(matches!(
            block_on(c.remove_reviewer(ChangeNumber(5), "3")),
            Err(Error::Forbidden { .. })
        ));
        fake.push_response(409, "topic is frozen\n");
        assert!(matches!(
            block_on(c.set_topic(ChangeNumber(5), Some("t"))),
            Err(Error::Conflict { message }) if message == "topic is frozen"
        ));
        fake.push_response(403, "no\n");
        assert!(matches!(
            block_on(c.edit_hashtags(ChangeNumber(5), &["a".into()], &[])),
            Err(Error::Forbidden { .. })
        ));
        let anon_fake = FakeHttpClient::new();
        let anon = client(&anon_fake, "https://h", Auth::Anonymous);
        assert!(matches!(
            block_on(anon.add_reviewer(ChangeNumber(5), "bob", ReviewerState::Cc, false)),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(anon.remove_reviewer(ChangeNumber(5), "3")),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(anon.set_topic(ChangeNumber(5), None)),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            block_on(anon.edit_hashtags(ChangeNumber(5), &[], &[])),
            Err(Error::Unauthorized)
        ));
        assert!(anon_fake.requests().is_empty());
    }
}
