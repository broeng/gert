//! Wire types: what the Gerrit REST API sends, as far as we decode it.
//!
//! These are deliberately tolerant (see CLAUDE.md, "Wire types and the
//! OpenAPI spec"):
//!
//! * Unknown fields are ignored (serde's default; never `deny_unknown_fields`).
//! * Enums we read keep unknown values ([`Status::Unknown`]); enums we do not
//!   read are simply not declared, so their values cannot fail a page.
//! * Anything a server may omit is an `Option` (or defaulted).
//! * Timestamps stay strings here; [`crate::convert`] parses them.
//!
//! Every struct derives `Default` and, in tests, `Serialize`. The conformance
//! test at the bottom serializes a default value (so every field appears,
//! `None` as `null`) and checks each key against the property list of the
//! matching schema in the vendored `spec/rest-api-openapi.json`. That catches
//! typos and wrong `rename`s without macros or a field list to keep in sync.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `ChangeInfo.status`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(from = "String")]
#[cfg_attr(test, serde(into = "String"))]
pub(crate) enum Status {
    New,
    Merged,
    Abandoned,
    /// A status this client does not know; the raw wire value.
    Unknown(String),
}

impl From<String> for Status {
    fn from(s: String) -> Self {
        match s.as_str() {
            "NEW" => Self::New,
            "MERGED" => Self::Merged,
            "ABANDONED" => Self::Abandoned,
            _ => Self::Unknown(s),
        }
    }
}

#[cfg(test)]
impl From<Status> for String {
    fn from(s: Status) -> Self {
        match s {
            Status::New => "NEW".into(),
            Status::Merged => "MERGED".into(),
            Status::Abandoned => "ABANDONED".into(),
            Status::Unknown(s) => s,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct AccountInfo {
    #[serde(rename = "_account_id")]
    pub account_id: Option<u64>,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct ApprovalInfo {
    pub value: Option<i32>,
    pub date: Option<String>,
    // `ApprovalInfo` also carries the voter's `AccountInfo` fields.
    #[serde(rename = "_account_id")]
    pub account_id: Option<u64>,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct LabelInfo {
    // Summary slots: whoever holds the label's best/worst vote. Only their
    // presence matters to us.
    pub approved: Option<AccountInfo>,
    pub rejected: Option<AccountInfo>,
    pub recommended: Option<AccountInfo>,
    pub disliked: Option<AccountInfo>,
    /// The current user's vote (`LABELS` option).
    pub value: Option<i32>,
    /// All votes (`DETAILED_LABELS` option).
    pub all: Option<Vec<ApprovalInfo>>,
    /// Permitted vote values, e.g. `{"-1": "This shall not be merged"}`.
    pub values: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct AttentionSetInfo {
    pub account: Option<AccountInfo>,
    pub last_update: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct GitPerson {
    pub name: Option<String>,
    pub email: Option<String>,
    pub date: Option<String>,
    /// Time zone offset in minutes east of UTC.
    pub tz: Option<i32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct CommitInfo {
    pub commit: Option<String>,
    pub parents: Option<Vec<CommitInfo>>,
    pub subject: Option<String>,
    pub message: Option<String>,
    pub author: Option<GitPerson>,
    pub committer: Option<GitPerson>,
}

/// `ActionInfo`. Its id is the key of the map it comes in.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct ActionInfo {
    pub method: Option<String>,
    pub label: Option<String>,
    pub title: Option<String>,
    pub enabled: Option<bool>,
    pub enabled_options: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct RevisionInfo {
    #[serde(rename = "_number")]
    pub number: Option<u32>,
    /// A `ChangeKind`, kept as a string so unknown kinds cannot fail a page.
    pub kind: Option<String>,
    pub created: Option<String>,
    pub uploader: Option<AccountInfo>,
    #[serde(rename = "ref")]
    pub ref_name: Option<String>,
    pub description: Option<String>,
    pub commit: Option<CommitInfo>,
    /// Only with `CURRENT_ACTIONS` (current revision) or `ALL_REVISIONS`
    /// with `CURRENT_ACTIONS`; keyed by action id.
    pub actions: Option<BTreeMap<String, ActionInfo>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct ChangeMessageInfo {
    pub id: Option<String>,
    pub tag: Option<String>,
    pub author: Option<AccountInfo>,
    pub real_author: Option<AccountInfo>,
    pub date: Option<String>,
    pub message: Option<String>,
    #[serde(rename = "_revision_number")]
    pub revision_number: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct SubmitRequirementExpressionInfo {
    pub expression: Option<String>,
    pub fulfilled: Option<bool>,
    /// `PASS`, `FAIL`, ...; a string so unknown values are kept.
    pub status: Option<String>,
    pub passing_atoms: Option<Vec<String>>,
    pub failing_atoms: Option<Vec<String>>,
    pub atom_explanations: Option<BTreeMap<String, String>>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct SubmitRequirementResultInfo {
    pub name: Option<String>,
    pub description: Option<String>,
    /// `SATISFIED`, `UNSATISFIED`, ...; a string so unknown values are kept.
    pub status: Option<String>,
    pub is_legacy: Option<bool>,
    pub applicability_expression_result: Option<SubmitRequirementExpressionInfo>,
    pub submittability_expression_result: Option<SubmitRequirementExpressionInfo>,
    pub override_expression_result: Option<SubmitRequirementExpressionInfo>,
}

/// One value of the `files` map; the path is the map key.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct CommonFileInfo {
    /// `A`, `D`, `R`, `C`, `W`; absent for a modified file.
    pub status: Option<String>,
    pub binary: Option<bool>,
    pub old_path: Option<String>,
    pub lines_inserted: Option<u32>,
    pub lines_deleted: Option<u32>,
    pub size_delta: Option<i64>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct FileMeta {
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub lines: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct ContentEntry {
    pub ab: Option<Vec<String>>,
    pub a: Option<Vec<String>>,
    pub b: Option<Vec<String>>,
    /// `[skip, mark]` pairs.
    pub edit_a: Option<Vec<Vec<u32>>>,
    pub edit_b: Option<Vec<Vec<u32>>>,
    pub due_to_rebase: Option<bool>,
    pub common: Option<bool>,
    pub skip: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct DiffInfo {
    pub meta_a: Option<FileMeta>,
    pub meta_b: Option<FileMeta>,
    /// `OK`, `TIMEOUT`, `FAILURE`; a string so unknown values are kept.
    pub intraline_status: Option<String>,
    /// `ADDED`, `MODIFIED`, ...; a string so unknown values are kept.
    pub change_type: Option<String>,
    pub diff_header: Option<Vec<String>>,
    pub content: Option<Vec<ContentEntry>>,
    pub binary: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct RelatedChangeAndCommitInfo {
    pub project: Option<String>,
    pub change_id: Option<String>,
    pub commit: Option<CommitInfo>,
    #[serde(rename = "_change_number")]
    pub change_number: Option<u64>,
    #[serde(rename = "_revision_number")]
    pub revision_number: Option<u32>,
    #[serde(rename = "_current_revision_number")]
    pub current_revision_number: Option<u32>,
    pub status: Option<String>,
    pub submittable: Option<bool>,
    pub work_in_progress: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct RelatedChangesInfo {
    pub changes: Option<Vec<RelatedChangeAndCommitInfo>>,
}

/// The subset of `DiffPreferencesInfo` a diff view uses.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct DiffPreferencesInfo {
    /// Number of context lines; negative means the entire file.
    pub context: Option<i64>,
    pub tab_size: Option<u32>,
    pub line_length: Option<u32>,
    pub expand_all_comments: Option<bool>,
    pub intraline_difference: Option<bool>,
    pub show_tabs: Option<bool>,
    pub show_whitespace_errors: Option<bool>,
    pub syntax_highlighting: Option<bool>,
    pub hide_line_numbers: Option<bool>,
    pub line_wrapping: Option<bool>,
    /// A `Whitespace`; a string so unknown values are kept.
    pub ignore_whitespace: Option<String>,
}

/// The subset of `GeneralPreferencesInfo` we read.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct GeneralPreferencesInfo {
    /// `SIDE_BY_SIDE` or `UNIFIED_DIFF`.
    pub diff_view: Option<String>,
}

/// `Range`: lines are 1-based, characters 0-based.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct Range {
    pub start_line: Option<u32>,
    pub start_character: Option<u32>,
    pub end_line: Option<u32>,
    pub end_character: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct ContextLineInfo {
    pub line_number: Option<u32>,
    pub context_line: Option<String>,
}

/// `CommentInfo`. The `path` is missing in the by-file lists
/// (`/comments`, `/drafts`), where the map key carries it.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct CommentInfo {
    pub author: Option<AccountInfo>,
    pub tag: Option<String>,
    pub change_message_id: Option<String>,
    pub unresolved: Option<bool>,
    pub context_lines: Option<Vec<ContextLineInfo>>,
    pub patch_set: Option<u32>,
    pub id: Option<String>,
    pub path: Option<String>,
    /// `PARENT` or `REVISION`; absent means `REVISION`.
    pub side: Option<String>,
    pub parent: Option<u32>,
    pub line: Option<u32>,
    pub range: Option<Range>,
    pub in_reply_to: Option<String>,
    pub updated: Option<String>,
    pub message: Option<String>,
    pub commit_id: Option<String>,
}

/// `RobotCommentInfo`: a comment plus what identifies the robot. The
/// vendored spec's schema lists only the `CommentInfo` fields; `robot_id`
/// is documented in Gerrit's REST API reference.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct RobotCommentInfo {
    #[serde(flatten)]
    pub comment: CommentInfo,
    pub robot_id: Option<String>,
}

/// The request body for creating or updating a draft (`DraftInput`).
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DraftInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<bool>,
}

/// `AttentionSetInput`, the part we send.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct AttentionSetInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The request body of a review (`ReviewInput`), the part we send.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ReviewInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drafts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_in_progress: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_to_attention_set: Option<Vec<AttentionSetInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove_from_attention_set: Option<Vec<AttentionSetInput>>,
}

/// The part of `ReviewResult` we read.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct ReviewResult {
    pub labels: Option<BTreeMap<String, i32>>,
    pub ready: Option<bool>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct ChangeInfo {
    #[serde(rename = "_number")]
    pub number: Option<u64>,
    pub change_id: Option<String>,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub subject: Option<String>,
    pub status: Option<Status>,
    pub owner: Option<AccountInfo>,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub insertions: Option<u32>,
    pub deletions: Option<u32>,
    pub labels: Option<BTreeMap<String, LabelInfo>>,
    pub work_in_progress: Option<bool>,
    pub is_private: Option<bool>,
    pub unresolved_comment_count: Option<u32>,
    pub total_comment_count: Option<u32>,
    pub topic: Option<String>,
    pub hashtags: Option<Vec<String>>,
    /// Keyed by account id.
    pub attention_set: Option<BTreeMap<String, AttentionSetInfo>>,
    pub submittable: Option<bool>,
    /// Label name to the vote values the user may set, as strings like
    /// `"-1"`, `" 0"` and `"+2"` (`DETAILED_LABELS`, authenticated only).
    pub permitted_labels: Option<BTreeMap<String, Vec<String>>>,
    /// Keyed by reviewer state (`REVIEWER`, `CC`, ...).
    pub reviewers: Option<BTreeMap<String, Vec<AccountInfo>>>,
    pub current_revision: Option<String>,
    pub current_revision_number: Option<u32>,
    /// Keyed by commit SHA.
    pub revisions: Option<BTreeMap<String, RevisionInfo>>,
    pub messages: Option<Vec<ChangeMessageInfo>>,
    pub submit_requirements: Option<Vec<SubmitRequirementResultInfo>>,
    /// Keyed by action id (`CHANGE_ACTIONS`).
    pub actions: Option<BTreeMap<String, ActionInfo>>,
    /// Set on the last element of a query page that has a successor.
    #[serde(rename = "_more_changes")]
    pub more_changes: Option<bool>,
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use serde_json::{Value, json};

    use super::*;

    fn spec() -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/rest-api-openapi.json");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_str(&text).unwrap()
    }

    /// Checks that every JSON key `T` deserializes exists as a property of
    /// `components.schemas.<schema>`.
    fn conforms<T: Default + Serialize>(spec: &Value, schema: &str) {
        let Value::Object(fields) = serde_json::to_value(T::default()).unwrap() else {
            panic!("{schema}: wire struct does not serialize to an object");
        };
        assert!(!fields.is_empty(), "{schema}: no fields");
        let props = spec["components"]["schemas"][schema]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("schema {schema} not found in spec"));
        for key in fields.keys() {
            assert!(
                props.contains_key(key),
                "{schema}: field `{key}` is not in the spec"
            );
        }
    }

    #[test]
    fn wire_structs_conform_to_the_spec() {
        let spec = spec();
        conforms::<AccountInfo>(&spec, "AccountInfo");
        conforms::<ApprovalInfo>(&spec, "ApprovalInfo");
        conforms::<LabelInfo>(&spec, "LabelInfo");
        conforms::<AttentionSetInfo>(&spec, "AttentionSetInfo");
        conforms::<ActionInfo>(&spec, "ActionInfo");
        conforms::<GitPerson>(&spec, "GitPerson");
        conforms::<CommitInfo>(&spec, "CommitInfo");
        conforms::<RevisionInfo>(&spec, "RevisionInfo");
        conforms::<ChangeMessageInfo>(&spec, "ChangeMessageInfo");
        conforms::<SubmitRequirementExpressionInfo>(&spec, "SubmitRequirementExpressionInfo");
        conforms::<SubmitRequirementResultInfo>(&spec, "SubmitRequirementResultInfo");
        conforms::<ChangeInfo>(&spec, "ChangeInfo");
        conforms::<CommonFileInfo>(&spec, "CommonFileInfo");
        conforms::<FileMeta>(&spec, "FileMeta");
        conforms::<ContentEntry>(&spec, "ContentEntry");
        conforms::<DiffInfo>(&spec, "DiffInfo");
        conforms::<RelatedChangeAndCommitInfo>(&spec, "RelatedChangeAndCommitInfo");
        conforms::<RelatedChangesInfo>(&spec, "RelatedChangesInfo");
        conforms::<DiffPreferencesInfo>(&spec, "DiffPreferencesInfo");
        conforms::<GeneralPreferencesInfo>(&spec, "GeneralPreferencesInfo");
        conforms::<Range>(&spec, "Range");
        conforms::<ContextLineInfo>(&spec, "ContextLineInfo");
        conforms::<CommentInfo>(&spec, "CommentInfo");
        conforms::<ReviewResult>(&spec, "ReviewResult");
        // The spec's schema omits the robot fields (see the struct).
        conforms_except::<RobotCommentInfo>(&spec, "RobotCommentInfo", &["robot_id"]);
    }

    /// The request bodies skip absent fields, so a default value shows no
    /// keys; check a value with every field set instead.
    #[test]
    fn request_structs_conform_to_the_spec() {
        let spec = spec();
        let range = Range {
            start_line: Some(1),
            start_character: Some(2),
            end_line: Some(3),
            end_character: Some(4),
        };
        let draft = DraftInput {
            path: Some("p".into()),
            side: Some("PARENT".into()),
            parent: Some(1),
            line: Some(3),
            range: Some(range),
            in_reply_to: Some("x".into()),
            message: Some("m".into()),
            unresolved: Some(true),
        };
        conforms_value(&spec, "DraftInput", &draft);
        let att = AttentionSetInput {
            user: Some("1".into()),
            reason: Some("r".into()),
        };
        conforms_value(&spec, "AttentionSetInput", &att);
        let review = ReviewInput {
            message: Some("m".into()),
            tag: Some("t".into()),
            labels: Some(BTreeMap::new()),
            drafts: Some("KEEP".into()),
            notify: Some("NONE".into()),
            work_in_progress: Some(true),
            ready: Some(true),
            add_to_attention_set: Some(vec![att.clone()]),
            remove_from_attention_set: Some(vec![att]),
        };
        conforms_value(&spec, "ReviewInput", &review);
    }

    fn conforms_value<T: Serialize>(spec: &Value, schema: &str, value: &T) {
        let Value::Object(fields) = serde_json::to_value(value).unwrap() else {
            panic!("{schema}: not an object");
        };
        let props = spec["components"]["schemas"][schema]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("schema {schema} not found in spec"));
        for key in fields.keys() {
            assert!(
                props.contains_key(key),
                "{schema}: field `{key}` is not in the spec"
            );
        }
        assert!(!fields.is_empty(), "{schema}: no fields");
    }

    /// Like [`conforms`], for fields the spec's schema leaves out.
    fn conforms_except<T: Default + Serialize>(spec: &Value, schema: &str, extra: &[&str]) {
        let Value::Object(fields) = serde_json::to_value(T::default()).unwrap() else {
            panic!("{schema}: wire struct does not serialize to an object");
        };
        let props = spec["components"]["schemas"][schema]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("schema {schema} not found in spec"));
        for key in fields.keys().filter(|k| !extra.contains(&k.as_str())) {
            assert!(
                props.contains_key(key),
                "{schema}: field `{key}` is not in the spec"
            );
        }
    }

    #[test]
    fn conformance_check_catches_a_typo() {
        #[derive(Default, Serialize)]
        struct Typo {
            subjct: Option<String>,
        }
        let spec = spec();
        let r = std::panic::catch_unwind(|| conforms::<Typo>(&spec, "ChangeInfo"));
        assert!(r.is_err());
    }

    fn status_of(v: Value) -> Option<Status> {
        serde_json::from_value::<ChangeInfo>(v).unwrap().status
    }

    #[test]
    fn status_keeps_unknown_values() {
        assert_eq!(status_of(json!({"status": "NEW"})), Some(Status::New));
        assert_eq!(
            status_of(json!({"status": "ARCHIVED"})),
            Some(Status::Unknown("ARCHIVED".into()))
        );
        assert_eq!(status_of(json!({})), None);
    }

    #[test]
    fn unmodeled_fields_with_unknown_enum_values_are_ignored() {
        let c: ChangeInfo = serde_json::from_value(json!({
            "_number": 7,
            "submit_type": "NEW_THING",
            "future_field": {"a": [1, 2]},
            "revisions": {"abc": {
                "_number": 3,
                "kind": "WHATEVER",
                "actions": {"x": {"enabled": true}},
                "new_thing": 1,
            }},
            "labels": {"Code-Review": {"optional": true, "blocking": false, "x": 1}},
        }))
        .unwrap();
        assert_eq!(c.number, Some(7));
        assert_eq!(c.revisions.unwrap()["abc"].number, Some(3));
    }

    #[test]
    fn negative_ids_do_not_decode() {
        assert!(serde_json::from_value::<AccountInfo>(json!({"_account_id": -1})).is_err());
        assert!(serde_json::from_value::<ChangeInfo>(json!({"_number": -1})).is_err());
    }
}
