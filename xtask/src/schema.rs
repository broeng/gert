//! Synthetic JSON generation from OpenAPI component schemas.
//!
//! The spec only uses a small subset of JSON Schema: `type`, `properties`,
//! `items`, `additionalProperties`, `enum`, `$ref` and the custom string
//! format `gerrit-timestamp`.

use serde_json::{Map, Value, json};

pub const TIMESTAMP: &str = "2013-02-01 09:59:32.126000000";
pub const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
pub const CHANGE_ID: &str = "I8473b95934b5732ac55d26311a706c9c2bde9940";

/// How many times one schema may appear on the `$ref` path before
/// recursion is cut off.
const MAX_REPEAT: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every property filled in.
    Full,
    /// Only what Gerrit realistically always sends (see [`minimal_fields`]).
    Minimal,
}

pub struct Generator<'a> {
    schemas: &'a Map<String, Value>,
}

impl<'a> Generator<'a> {
    pub fn new(schemas: &'a Map<String, Value>) -> Self {
        Self { schemas }
    }

    /// Generates an instance of the named component schema.
    pub fn generate(&self, name: &str, mode: Mode) -> Option<Value> {
        self.gen_named(name, mode, &mut Vec::new())
    }

    fn gen_named(&self, name: &str, mode: Mode, stack: &mut Vec<String>) -> Option<Value> {
        if stack.iter().filter(|s| *s == name).count() >= MAX_REPEAT {
            return None;
        }
        let schema = self.schemas.get(name)?;
        stack.push(name.to_owned());
        let v = self.gen_schema(schema, Some(name), "", name, mode, stack);
        stack.pop();
        v
    }

    /// `own_name` is the component schema's name when `schema` is one,
    /// `parent` the component schema a property belongs to, and `hint` the
    /// property name (both steer realistic values).
    fn gen_schema(
        &self,
        schema: &Value,
        own_name: Option<&str>,
        parent: &str,
        hint: &str,
        mode: Mode,
        stack: &mut Vec<String>,
    ) -> Option<Value> {
        if let Some(r) = schema["$ref"].as_str() {
            return self.gen_named(r.rsplit('/').next()?, mode, stack);
        }
        if let Some(values) = schema["enum"].as_array() {
            let pick = hint.bytes().map(usize::from).sum::<usize>() % values.len().max(1);
            return values.get(pick).cloned();
        }
        let owner = own_name.unwrap_or("");
        match schema["type"].as_str()? {
            "string" => Some(if schema["format"] == "gerrit-timestamp" {
                json!(TIMESTAMP)
            } else {
                json!(string_for(hint))
            }),
            "integer" => Some(json!(int_for(parent, hint))),
            "boolean" => Some(json!(true)),
            "array" => {
                let item = self.gen_schema(&schema["items"], None, parent, hint, mode, stack);
                Some(Value::Array(item.into_iter().collect()))
            }
            "object" => {
                if let Some(props) = schema["properties"].as_object() {
                    let mut out = Map::new();
                    for (k, p) in props {
                        if mode == Mode::Minimal && !minimal_fields(owner).contains(&k.as_str()) {
                            continue;
                        }
                        if let Some(v) = self.gen_schema(p, None, owner, k, mode, stack) {
                            out.insert(k.clone(), v);
                        }
                    }
                    Some(Value::Object(out))
                } else if schema["additionalProperties"].is_object() {
                    let mut out = Map::new();
                    if mode == Mode::Full {
                        for key in map_keys(hint) {
                            if let Some(v) = self.gen_schema(
                                &schema["additionalProperties"],
                                None,
                                parent,
                                hint,
                                mode,
                                stack,
                            ) {
                                out.insert((*key).to_owned(), v);
                            }
                        }
                    }
                    Some(Value::Object(out))
                } else {
                    Some(json!({}))
                }
            }
            _ => None,
        }
    }
}

/// Properties Gerrit 3.x sends for every instance of a schema in a plain
/// change query. Schemas not listed here are generated empty when minimal.
pub fn minimal_fields(schema: &str) -> &'static [&'static str] {
    match schema {
        "ChangeInfo" => &[
            "id",
            "project",
            "branch",
            "change_id",
            "subject",
            "status",
            "created",
            "updated",
            "_number",
            "owner",
        ],
        "AccountInfo" => &["_account_id"],
        "ApprovalInfo" => &["_account_id", "value"],
        "RevisionInfo" => &["_number", "created", "ref", "commit"],
        "CommitInfo" => &["commit", "subject", "message", "author", "committer"],
        "GitPerson" => &["name", "email", "date", "tz"],
        "AttentionSetInfo" => &["account", "last_update", "reason"],
        // The file list always has these (a modified file has no `status`).
        "CommonFileInfo" => &["lines_inserted", "lines_deleted", "size_delta", "size"],
        "FileMeta" => &["name", "content_type", "lines"],
        "ContentEntry" => &["ab"],
        "DiffInfo" => &[
            "meta_a",
            "meta_b",
            "intraline_status",
            "change_type",
            "content",
        ],
        "RelatedChangesInfo" => &["changes"],
        "RelatedChangeAndCommitInfo" => &[
            "project",
            "change_id",
            "commit",
            "_change_number",
            "_revision_number",
            "_current_revision_number",
            "status",
        ],
        "ChangeMessageInfo" => &["id", "author", "date", "message", "_revision_number"],
        "SubmitRequirementResultInfo" => &["name", "status", "is_legacy"],
        "SubmitRequirementExpressionInfo" => &["expression", "fulfilled", "status"],
        "DiffPreferencesInfo" => &[
            "context",
            "tab_size",
            "line_length",
            "ignore_whitespace",
            "intraline_difference",
            "show_tabs",
        ],
        "GeneralPreferencesInfo" => &["diff_view"],
        _ => &[],
    }
}

fn string_for(hint: &str) -> String {
    match hint {
        "email" => "jane.doe@example.com".into(),
        "name" | "display_name" => "Jane Doe".into(),
        "username" => "jdoe".into(),
        "project" => "my/project".into(),
        "branch" => "main".into(),
        "full_branch" => "refs/heads/main".into(),
        "ref" => "refs/changes/45/12345/2".into(),
        "commit" | "current_revision" => SHA.into(),
        "change_id" => CHANGE_ID.into(),
        "id" => "my%2Fproject~12345".into(),
        "subject" => "Fix the frobnicator".into(),
        "message" => {
            format!("Fix the frobnicator\n\nLonger description.\n\nChange-Id: {CHANGE_ID}\n")
        }
        "url" => "https://gerrit.example.com/".into(),
        other => format!("{other}-value"),
    }
}

fn int_for(owner: &str, hint: &str) -> i64 {
    match (owner, hint) {
        (_, "_account_id") => 1_000_001,
        ("ChangeInfo", "_number") => 12345,
        (_, "_number" | "current_revision_number") => 2,
        (_, "insertions") => 12,
        (_, "deletions") => 3,
        (_, "tz") => 60,
        ("DiffPreferencesInfo", "context") => 10,
        ("DiffPreferencesInfo", "tab_size") => 8,
        ("DiffPreferencesInfo", "line_length") => 100,
        ("RelatedChangeAndCommitInfo", "_change_number") => 12346,
        ("ContentEntry", "skip") => 25,
        ("FileMeta", "lines") => 42,
        _ => 1,
    }
}

/// Keys used when a schema is a string-keyed map, chosen by property name.
fn map_keys(hint: &str) -> &'static [&'static str] {
    match hint {
        "attention_set" | "removed_from_attention_set" => &["1000001"],
        "labels" => &["Code-Review", "Verified"],
        "reviewers" | "pending_reviewers" => &["CC", "REVIEWER"],
        "revisions" => &[SHA],
        "values" => &["+1", "-1", " 0"],
        "fetch" => &["http"],
        "files" => &["src/Main.java"],
        _ => &["key"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schemas() -> Map<String, Value> {
        let v = json!({
            "Node": { "type": "object", "properties": {
                "parents": { "type": "array", "items": { "$ref": "#/components/schemas/Node" } },
                "kind": { "$ref": "#/components/schemas/Kind" },
                "when": { "type": "string", "format": "gerrit-timestamp" }
            }},
            "Kind": { "type": "string", "enum": ["A", "B"] }
        });
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn recursion_is_cut_off_and_enums_are_valid() {
        let s = schemas();
        let v = Generator::new(&s).generate("Node", Mode::Full).unwrap();
        assert_eq!(v["when"], TIMESTAMP);
        assert!(["A", "B"].contains(&v["kind"].as_str().unwrap()));
        // Node -> parents[Node] -> parents[] : third level is empty.
        assert_eq!(v["parents"][0]["parents"], json!([]));
    }

    #[test]
    fn minimal_omits_unlisted_fields() {
        let s = schemas();
        let v = Generator::new(&s).generate("Node", Mode::Minimal).unwrap();
        assert_eq!(v, json!({}));
    }
}
