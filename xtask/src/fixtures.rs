//! `cargo xtask gen-fixtures`: writes deterministic JSON fixtures.

use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::schema::{CHANGE_ID, Generator, Mode, SHA};

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
        text.push_str(&serde_json::to_string_pretty(v)?);
        text.push('\n');
        files.push((name.to_owned(), text));
        Ok(())
    };

    for name in SCHEMAS {
        for (mode, suffix) in [(Mode::Full, "full"), (Mode::Minimal, "minimal")] {
            let v = g
                .generate(name, mode)
                .with_context(|| format!("schema {name} not found in spec"))?;
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

    files.sort();
    for (name, text) in files {
        fs::write(out.join(&name), text).with_context(|| format!("writing {name}"))?;
    }
    println!("fixtures written to {}", out.display());
    Ok(())
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
