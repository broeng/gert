//! `cargo xtask gen-fixtures`: writes deterministic JSON fixtures.

use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::{
    large_diff::large_diff,
    schema::{CHANGE_ID, Generator, Mode, SHA},
};

/// Component schemas gerrit-api's tests consume directly.
const SCHEMAS: &[&str] = &[
    "ChangeInfo",
    "AccountInfo",
    "LabelInfo",
    "ApprovalInfo",
    "RevisionInfo",
    "CommitInfo",
    "GitPerson",
    "AttentionSetInfo",
    "CommonFileInfo",
    "FileMeta",
    "ContentEntry",
    "DiffInfo",
    "RelatedChangeAndCommitInfo",
    "RelatedChangesInfo",
    "ChangeMessageInfo",
    "SubmitRequirementResultInfo",
    "SubmitRequirementExpressionInfo",
    "DiffPreferencesInfo",
    "GeneralPreferencesInfo",
    "CommentInfo",
    "RobotCommentInfo",
    "Range",
    "ContextLineInfo",
    "ReviewResult",
];

/// What every Gerrit HTTP response body starts with.
const XSSI: &str = ")]}'\n";

pub fn generate(spec_path: &Path, out: &Path) -> Result<()> {
    let spec: Value = serde_json::from_slice(
        &fs::read(spec_path).with_context(|| format!("reading {}", spec_path.display()))?,
    )
    .context("parsing spec")?;
    let schemas = spec["components"]["schemas"]
        .as_object()
        .context("spec has no components.schemas")?;
    let g = Generator::new(schemas);

    fs::create_dir_all(out)?;
    // Drop stale fixtures so the directory always mirrors the generator.
    for entry in fs::read_dir(out)? {
        let p = entry?.path();
        if p.extension().is_some_and(|e| e == "json") {
            fs::remove_file(p)?;
        }
    }

    let mut files: Vec<(String, String)> = Vec::new();
    let mut put = |name: &str, v: &Value, xssi: bool| -> Result<()> {
        let mut text = String::new();
        if xssi {
            text.push_str(XSSI);
        }
        // Big generated fixtures are compact to keep the repository small.
        if name.starts_with("large_") {
            text.push_str(&serde_json::to_string(v)?);
        } else {
            text.push_str(&serde_json::to_string_pretty(v)?);
        }
        text.push('\n');
        files.push((name.to_owned(), text));
        Ok(())
    };

    for name in SCHEMAS {
        for (mode, suffix) in [(Mode::Full, "full"), (Mode::Minimal, "minimal")] {
            let v = g
                .generate(name, mode)
                .with_context(|| format!("schema {name} not found in spec"))?;
            let v = fix_edit_pairs(v);
            put(&format!("{name}.{suffix}.json"), &v, false)?;
        }
    }

    // Edge cases: values the spec does not (yet) list.
    let mut unknown = minimal_change(&g)?;
    set(&mut unknown, "status", json!("ARCHIVED"));
    set(&mut unknown, "submit_type", json!("TELEPORT"));
    set(
        &mut unknown,
        "revisions",
        json!({ SHA: { "kind": "WEIRD_KIND", "_number": 1 } }),
    );
    set(&mut unknown, "current_revision", json!(SHA));
    put("ChangeInfo.unknown_enum.json", &unknown, false)?;

    // Server version: a JSON string.
    put(
        "server_version.response.json",
        &json!("3.15.0-SNAPSHOT"),
        true,
    )?;

    let me = g
        .generate("AccountInfo", Mode::Full)
        .context("AccountInfo missing")?;
    put("account_self.response.json", &me, true)?;

    // Query responses.
    let mut c1 = minimal_change(&g)?;
    set(&mut c1, "_number", json!(1001));
    set(&mut c1, "status", json!("NEW"));
    set(&mut c1, "subject", json!("Add the frobnicator"));
    set(&mut c1, "insertions", json!(120));
    set(&mut c1, "deletions", json!(7));
    set(&mut c1, "total_comment_count", json!(4));
    set(&mut c1, "unresolved_comment_count", json!(2));
    set(&mut c1, "submittable", json!(false));
    set(&mut c1, "work_in_progress", json!(true));
    set(&mut c1, "topic", json!("frob"));
    set(&mut c1, "hashtags", json!(["wip", "frob"]));
    set(
        &mut c1,
        "attention_set",
        json!({ "1000002": { "account": { "_account_id": 1000002 },
                              "last_update": crate::schema::TIMESTAMP, "reason": "Reviewer added" } }),
    );
    set(
        &mut c1,
        "labels",
        json!({
            "Code-Review": { "recommended": { "_account_id": 1000002, "name": "Rev One" } },
            "Verified": { "rejected": { "_account_id": 1000003 }, "value": -1 }
        }),
    );

    let mut c2 = minimal_change(&g)?;
    set(&mut c2, "_number", json!(1002));
    set(&mut c2, "status", json!("MERGED"));
    set(&mut c2, "subject", json!("Fix the thing"));
    set(&mut c2, "updated", json!("2013-02-02 10:00:00"));
    set(&mut c2, "submittable", json!(true));
    set(
        &mut c2,
        "labels",
        json!({
            "Code-Review": { "approved": { "_account_id": 1000002 } },
            "Verified": { "approved": { "_account_id": 1000003 } }
        }),
    );

    let mut c3 = minimal_change(&g)?;
    set(&mut c3, "_number", json!(1003));
    set(&mut c3, "status", json!("ABANDONED"));
    set(&mut c3, "subject", json!("Abandoned experiment"));
    set(&mut c3, "is_private", json!(true));
    set(&mut c3, "labels", json!({}));
    set(&mut c3, "_more_changes", json!(true));

    put(
        "query_changes.response.json",
        &json!([c1, c2, c3.clone()]),
        true,
    )?;
    put("query_changes_empty.response.json", &json!([]), true)?;
    let mut unknown_row = unknown.clone();
    set(&mut unknown_row, "_number", json!(1004));
    put(
        "query_changes_unknown_enum.response.json",
        &json!([unknown_row]),
        true,
    )?;

    // Change detail.
    let mut detail = minimal_change(&g)?;
    set(&mut detail, "current_revision", json!(SHA));
    set(&mut detail, "current_revision_number", json!(2));
    set(
        &mut detail,
        "revisions",
        json!({ SHA: {
            "_number": 2,
            "created": crate::schema::TIMESTAMP,
            "ref": "refs/changes/45/12345/2",
            "commit": { "commit": SHA, "subject": "Fix the frobnicator",
                "message": format!("Fix the frobnicator\n\nLonger description.\n\nChange-Id: {CHANGE_ID}\n") }
        }}),
    );
    set(
        &mut detail,
        "reviewers",
        json!({
            "REVIEWER": [{ "_account_id": 1000002, "name": "Rev One" }],
            "CC": [{ "_account_id": 1000003, "email": "cc@example.com" }]
        }),
    );
    put("change_detail.response.json", &detail, true)?;
    let full = g
        .generate("ChangeInfo", Mode::Full)
        .context("ChangeInfo missing")?;
    put("change_detail_full.response.json", &full, true)?;

    // File list: a map keyed by path. Hand-shaped from CommonFileInfo.
    let file = |g: &Generator, fields: Value| -> Result<Value> {
        let mut f = g
            .generate("CommonFileInfo", Mode::Minimal)
            .context("CommonFileInfo missing")?;
        if let Value::Object(extra) = fields {
            for (k, v) in extra {
                set(&mut f, &k, v);
            }
        }
        Ok(f)
    };
    let files_map = json!({
        "src/Main.java": file(&g, json!({ "lines_inserted": 12, "lines_deleted": 3,
            "size_delta": 210, "size": 4096 }))?,
        "/COMMIT_MSG": file(&g, json!({ "lines_inserted": 7, "size_delta": 180, "size": 180 }))?,
        "src/New.java": file(&g, json!({ "status": "A", "lines_inserted": 30, "size_delta": 700,
            "size": 700 }))?,
        "src/renamed/Name.java": file(&g, json!({ "status": "R", "old_path": "src/Old name.java",
            "lines_inserted": 1, "lines_deleted": 1, "size_delta": 2, "size": 900 }))?,
        "src/Gone.java": file(&g, json!({ "status": "D", "lines_inserted": 0, "lines_deleted": 25,
            "size_delta": -600, "size": 0 }))?,
        "assets/logo.png": file(&g, json!({ "status": "A", "binary": true, "lines_inserted": 0,
            "size_delta": 5120, "size": 5120 }))?,
        "src/Weird.java": file(&g, json!({ "status": "Q" }))?,
    });
    put("files.response.json", &files_map, true)?;
    put(
        "files_reviewed.response.json",
        &json!(["/COMMIT_MSG", "src/Main.java"]),
        true,
    )?;
    put("files_empty.response.json", &json!({}), true)?;

    // Diffs.
    let mut diff = g
        .generate("DiffInfo", Mode::Minimal)
        .context("DiffInfo missing")?;
    set(&mut diff, "change_type", json!("MODIFIED"));
    set(&mut diff, "intraline_status", json!("OK"));
    set(
        &mut diff,
        "meta_a",
        json!({ "name": "src/Main.java", "content_type": "text/x-java", "lines": 7 }),
    );
    set(
        &mut diff,
        "meta_b",
        json!({ "name": "src/Main.java", "content_type": "text/x-java", "lines": 8 }),
    );
    set(
        &mut diff,
        "diff_header",
        json!([
            "diff --git a/src/Main.java b/src/Main.java",
            "index 1111111..2222222 100644"
        ]),
    );
    set(
        &mut diff,
        "content",
        json!([
            { "ab": ["class Main {", "  void run() {"] },
            { "a": ["    int x = 1;"], "b": ["    int total = 1;", "    log(total);"],
              "edit_a": [[8, 1]], "edit_b": [[8, 5], [14, 5]] },
            { "ab": ["  }", "}"] },
            { "skip": 2 }
        ]),
    );
    put("diff.response.json", &diff, true)?;
    let mut diff_unknown = diff.clone();
    set(&mut diff_unknown, "change_type", json!("TELEPORTED"));
    set(&mut diff_unknown, "intraline_status", json!("MAYBE"));
    put("diff_unknown_enum.response.json", &diff_unknown, true)?;
    put("large_diff.response.json", &large_diff(), true)?;

    // Related changes.
    let mut related = g
        .generate("RelatedChangesInfo", Mode::Minimal)
        .context("RelatedChangesInfo missing")?;
    let mut r1 = g
        .generate("RelatedChangeAndCommitInfo", Mode::Minimal)
        .context("RelatedChangeAndCommitInfo missing")?;
    set(&mut r1, "status", json!("NEW"));
    set(&mut r1, "_change_number", json!(12346));
    set(&mut r1, "_revision_number", json!(1));
    set(&mut r1, "_current_revision_number", json!(3));
    let mut r2 = r1.clone();
    set(&mut r2, "status", json!("MERGED"));
    set(&mut r2, "_change_number", json!(12340));
    set(&mut r2, "_revision_number", json!(2));
    set(&mut r2, "_current_revision_number", json!(2));
    set(&mut r2, "submittable", json!(true));
    set(&mut related, "changes", json!([r1, r2]));
    put("related.response.json", &related, true)?;
    put("related_empty.response.json", &json!({}), true)?;

    // Preferences.
    let mut prefs = g
        .generate("DiffPreferencesInfo", Mode::Full)
        .context("DiffPreferencesInfo missing")?;
    set(&mut prefs, "ignore_whitespace", json!("IGNORE_TRAILING"));
    put("diff_preferences.response.json", &prefs, true)?;
    set(&mut prefs, "ignore_whitespace", json!("IGNORE_SOMETHING"));
    set(&mut prefs, "context", json!(-1));
    put("diff_preferences_unknown_enum.response.json", &prefs, true)?;
    let mut general = g
        .generate("GeneralPreferencesInfo", Mode::Minimal)
        .context("GeneralPreferencesInfo missing")?;
    set(&mut general, "diff_view", json!("UNIFIED_DIFF"));
    put("general_preferences.response.json", &general, true)?;

    // Unknown enum values in the new schemas.
    let mut sr = g
        .generate("SubmitRequirementResultInfo", Mode::Full)
        .context("SubmitRequirementResultInfo missing")?;
    set(&mut sr, "status", json!("PAUSED"));
    put("SubmitRequirementResultInfo.unknown_enum.json", &sr, false)?;
    let mut file_unknown = file(&g, json!({ "status": "Q" }))?;
    set(&mut file_unknown, "future_field", json!([1]));
    put("CommonFileInfo.unknown_enum.json", &file_unknown, false)?;

    comment_fixtures(&g, &mut put)?;

    files.sort();
    for (name, text) in files {
        fs::write(out.join(&name), text).with_context(|| format!("writing {name}"))?;
    }
    println!("fixtures written to {}", out.display());
    Ok(())
}

/// The schema cannot say that an intraline edit is a `[skip, mark]` pair,
/// so generated arrays hold one number; make them real pairs.
fn fix_edit_pairs(mut v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(o) => {
                for (k, child) in o.iter_mut() {
                    if k == "edit_a" || k == "edit_b" {
                        *child = json!([[1, 2]]);
                    } else {
                        walk(child);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

fn minimal_change(g: &Generator) -> Result<Value> {
    g.generate("ChangeInfo", Mode::Minimal)
        .context("ChangeInfo missing")
}

fn set(v: &mut Value, key: &str, value: Value) {
    if let Some(obj) = v.as_object_mut() {
        let obj: &mut Map<String, Value> = obj;
        obj.insert(key.to_owned(), value);
    }
}

/// Comments, drafts, robot comments and review results.
fn comment_fixtures(
    g: &Generator,
    put: &mut impl FnMut(&str, &Value, bool) -> Result<()>,
) -> Result<()> {
    let author = |id: u64, name: &str| json!({ "_account_id": id, "name": name });
    let alice = author(1000001, "Alice");
    let bob = author(1000002, "Bob");

    // A side the spec does not list.
    let mut unknown = g
        .generate("CommentInfo", Mode::Minimal)
        .context("CommentInfo missing")?;
    set(&mut unknown, "side", json!("SIDEWAYS"));
    set(&mut unknown, "patch_set", json!(1));
    put("CommentInfo.unknown_enum.json", &unknown, false)?;

    // A realistic `GET /changes/{n}/comments`: keyed by path, no `path` in
    // the comments themselves. Covers a commit message comment, a file
    // comment, a line thread with replies that ends resolved, a range
    // thread that stays unresolved, a PARENT side comment, a comment on an
    // older patch set, a patch set level comment and a reply to a comment
    // that is not in the list.
    let comments = json!({
        "/COMMIT_MSG": [
            { "patch_set": 1, "id": "cm01", "line": 7, "author": bob,
              "updated": "2024-03-01 09:00:00.000000000", "message": "Mention the issue.",
              "unresolved": true, "commit_id": SHA }
        ],
        "/PATCHSET_LEVEL": [
            { "patch_set": 2, "id": "pl01", "author": bob,
              "updated": "2024-03-03 09:00:00.000000000", "message": "Nice cleanup overall.",
              "unresolved": false, "change_message_id": "msg0001" }
        ],
        "src/Main.java": [
            { "patch_set": 1, "id": "ln01", "line": 12, "author": bob,
              "updated": "2024-03-01 10:00:00.000000000", "message": "Why a new thread here?",
              "unresolved": true, "commit_id": SHA },
            { "patch_set": 1, "id": "ln02", "line": 12, "in_reply_to": "ln01", "author": alice,
              "updated": "2024-03-01 11:00:00.000000000", "message": "Needed for the retry.",
              "unresolved": true },
            { "patch_set": 1, "id": "ln03", "line": 12, "in_reply_to": "ln02", "author": bob,
              "updated": "2024-03-01 12:00:00.000000000", "message": "Ok, makes sense. Done?",
              "unresolved": false },
            { "patch_set": 2, "id": "rg01", "line": 21, "author": bob,
              "range": { "start_line": 20, "start_character": 4, "end_line": 21,
                         "end_character": 18 },
              "updated": "2024-03-02 10:00:00.000000000", "message": "This loop can overflow.",
              "unresolved": true },
            { "patch_set": 2, "id": "fl01", "author": alice,
              "updated": "2024-03-02 11:00:00.000000000", "message": "Whole file needs a rewrite.",
              "unresolved": false },
            { "patch_set": 2, "id": "pa01", "line": 3, "side": "PARENT", "author": alice,
              "updated": "2024-03-02 12:00:00.000000000", "message": "This was wrong before too.",
              "unresolved": false },
            { "patch_set": 2, "id": "or01", "line": 30, "in_reply_to": "deleted-comment",
              "author": bob, "updated": "2024-03-02 13:00:00.000000000",
              "message": "Reply to something that is gone.", "unresolved": true }
        ]
    });
    put("comments.response.json", &comments, true)?;
    put("comments_empty.response.json", &json!({}), true)?;

    // `GET /changes/{n}/drafts`: the user's own, replying to ln03 and one new.
    let drafts = json!({
        "src/Main.java": [
            { "patch_set": 1, "id": "dr01", "line": 12, "in_reply_to": "ln03",
              "updated": "2024-03-04 10:00:00.000000000", "message": "Yes, done.",
              "unresolved": false, "commit_id": SHA },
            { "patch_set": 2, "id": "dr02", "line": 40,
              "updated": "2024-03-04 11:00:00.000000000", "message": "Check this too.",
              "unresolved": true }
        ]
    });
    put("drafts.response.json", &drafts, true)?;

    let robot = json!({
        "src/Main.java": [
            { "patch_set": 2, "id": "rb01", "line": 5, "robot_id": "lint",
              "robot_run_id": "run-42", "url": "https://ci.example.com/run-42",
              "properties": { "severity": "warning" },
              "updated": "2024-03-02 09:00:00.000000000", "message": "Unused import.",
              "fix_suggestions": [ { "fix_id": "fix1", "description": "Remove it",
                "replacements": [ { "path": "src/Main.java",
                  "range": { "start_line": 5, "start_character": 0, "end_line": 6,
                             "end_character": 0 }, "replacement": "" } ] } ] }
        ]
    });
    put("robotcomments.response.json", &robot, true)?;

    // Review results: a plain success, and a partial failure.
    put(
        "review_result.response.json",
        &json!({ "labels": { "Code-Review": 1, "Verified": 0 }, "ready": true }),
        true,
    )?;
    put(
        "review_result_error.response.json",
        &json!({ "error": "Not Found: unknown-reviewer", "labels": {} }),
        true,
    )?;
    Ok(())
}
