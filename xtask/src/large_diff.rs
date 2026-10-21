//! A deterministic, roughly 10k line diff for virtualization work.
//!
//! Built by code rather than from the schema: it needs thousands of
//! consistent lines. The chunk mix repeats per 100-line block: common
//! lines, a modified region with intraline edits (including non-ASCII
//! text and an astral character, to exercise UTF-16 offsets), a pure
//! insertion, a skipped region, and a pure deletion.

use serde_json::{Value, json};

/// Lines of the new file the generator stops after (a block may overshoot).
const TARGET_LINES: u32 = 10_000;

pub fn large_diff() -> Value {
    let mut content = Vec::new();
    let (mut lines_a, mut lines_b) = (0_u32, 0_u32);
    let mut n = 0_u32;
    while lines_b < TARGET_LINES {
        // 30 common lines.
        let common: Vec<String> = (0..30).map(|i| code_line(n * 100 + i)).collect();
        lines_a += 30;
        lines_b += 30;
        content.push(json!({ "ab": common }));

        // A modified region: 3 old lines become 4 new ones.
        let a: Vec<String> = (0..3).map(|i| old_line(n * 100 + 30 + i)).collect();
        let b: Vec<String> = (0..4).map(|i| new_line(n * 100 + 30 + i)).collect();
        let (edit_a, edit_b) = edits(&a, &b);
        lines_a += 3;
        lines_b += 4;
        content.push(json!({ "a": a, "b": b, "edit_a": edit_a, "edit_b": edit_b }));

        // More common lines.
        let common: Vec<String> = (0..20).map(|i| code_line(n * 100 + 40 + i)).collect();
        lines_a += 20;
        lines_b += 20;
        content.push(json!({ "ab": common }));

        // A pure insertion (no intraline edits); every 5th block is "due to rebase".
        let b: Vec<String> = (0..5)
            .map(|i| format!("    // inserted {n}.{i} \u{2713}"))
            .collect();
        lines_b += 5;
        if n.is_multiple_of(5) {
            content.push(json!({ "b": b, "due_to_rebase": true }));
        } else {
            content.push(json!({ "b": b }));
        }

        // A skipped region (the server omits unchanged context).
        lines_a += 40;
        lines_b += 40;
        content.push(json!({ "skip": 40 }));

        // A pure deletion, then common lines to the end of the block.
        let a: Vec<String> = (0..2)
            .map(|i| format!("    removed_call_{n}_{i}();"))
            .collect();
        lines_a += 2;
        content.push(json!({ "a": a }));
        let common: Vec<String> = (0..3).map(|i| code_line(n * 100 + 60 + i)).collect();
        lines_a += 3;
        lines_b += 3;
        content.push(json!({ "ab": common }));
        n += 1;
    }
    json!({
        "meta_a": { "name": "src/generated.rs", "content_type": "text/x-rust", "lines": lines_a },
        "meta_b": { "name": "src/generated.rs", "content_type": "text/x-rust", "lines": lines_b },
        "change_type": "MODIFIED",
        "intraline_status": "OK",
        "diff_header": [
            "diff --git a/src/generated.rs b/src/generated.rs",
            "index 1111111..2222222 100644",
            "--- a/src/generated.rs",
            "+++ b/src/generated.rs"
        ],
        "content": content,
    })
}

fn code_line(i: u32) -> String {
    match i % 7 {
        0 => format!("fn function_{i}(input: &str) -> usize {{"),
        1 => format!("    let value_{i} = compute({i}, {});", i % 13),
        2 => format!("    // comment {i}: na\u{ef}ve caf\u{e9} handling"),
        3 => String::new(),
        4 => format!("    if value_{} > {} {{ return {i}; }}", i - 3, i % 17),
        5 => format!("    println!(\"line {i} \\t tabbed\");"),
        _ => "}".to_owned(),
    }
}

fn old_line(i: u32) -> String {
    format!("    let total_{i} = base_{i} + offset; // old \u{1F600}")
}

fn new_line(i: u32) -> String {
    format!("    let total_{i} = base_{i} * scale + offset; // new \u{1F600}")
}

/// Intraline edits for two sides whose lines pair up by index: the common
/// prefix and suffix of each pair stay unmarked, extra lines are marked
/// whole. Offsets follow Gerrit: UTF-16 units over the lines joined by
/// `\n`, each pair `[skip, mark]` counted from the previous mark's end.
fn edits(a: &[String], b: &[String]) -> (Vec<[u32; 2]>, Vec<[u32; 2]>) {
    let mut ranges_a = Vec::new();
    let mut ranges_b = Vec::new();
    let (mut start_a, mut start_b) = (0_u32, 0_u32);
    for i in 0..a.len().max(b.len()) {
        let ca: Vec<char> = a.get(i).map(|s| s.chars().collect()).unwrap_or_default();
        let cb: Vec<char> = b.get(i).map(|s| s.chars().collect()).unwrap_or_default();
        let (pa, pb) = if a.get(i).is_some() && b.get(i).is_some() {
            let max = ca.len().min(cb.len());
            let prefix = (0..max).take_while(|&k| ca[k] == cb[k]).count();
            let suffix = (0..max - prefix)
                .take_while(|&k| ca[ca.len() - 1 - k] == cb[cb.len() - 1 - k])
                .count();
            ((prefix, ca.len() - suffix), (prefix, cb.len() - suffix))
        } else {
            ((0, ca.len()), (0, cb.len()))
        };
        for (chars, (from, to), start, ranges) in [
            (&ca, pa, start_a, &mut ranges_a),
            (&cb, pb, start_b, &mut ranges_b),
        ] {
            if to > from {
                let units = |range: std::ops::Range<usize>| -> u32 {
                    chars[range].iter().map(|c| c.len_utf16() as u32).sum()
                };
                let s = start + units(0..from);
                ranges.push((s, s + units(from..to)));
            }
        }
        let width = |c: &[char]| c.iter().map(|c| c.len_utf16() as u32).sum::<u32>() + 1;
        start_a += width(&ca);
        start_b += width(&cb);
    }
    (to_pairs(&ranges_a), to_pairs(&ranges_b))
}

/// Absolute `(start, end)` ranges to Gerrit's `[skip, mark]` pairs.
fn to_pairs(ranges: &[(u32, u32)]) -> Vec<[u32; 2]> {
    let mut prev_end = 0;
    ranges
        .iter()
        .map(|&(s, e)| {
            let pair = [s - prev_end, e - s];
            prev_end = e;
            pair
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_and_consistent() {
        let d = large_diff();
        assert_eq!(d, large_diff());
        let mut total_a = 0;
        let mut total_b = 0;
        for c in d["content"].as_array().unwrap() {
            let len = |k: &str| c[k].as_array().map_or(0, |v| v.len() as u64);
            let skip = c["skip"].as_u64().unwrap_or(0);
            total_a += len("ab") + len("a") + skip;
            total_b += len("ab") + len("b") + skip;
        }
        assert_eq!(total_a, d["meta_a"]["lines"].as_u64().unwrap());
        assert_eq!(total_b, d["meta_b"]["lines"].as_u64().unwrap());
        assert!((10_000..10_100).contains(&total_b), "{total_b}");
    }

    #[test]
    fn edits_pair_prefix_and_suffix() {
        let a = vec!["let x = 1;".to_owned()];
        let b = vec!["let yy = 1;".to_owned()];
        let (ea, eb) = edits(&a, &b);
        assert_eq!(ea, vec![[4, 1]]);
        assert_eq!(eb, vec![[4, 2]]);
    }

    #[test]
    fn edits_count_newlines_and_extra_lines() {
        let a = vec!["ab".to_owned()];
        let b = vec!["aX".to_owned(), "new".to_owned()];
        let (ea, eb) = edits(&a, &b);
        assert_eq!(ea, vec![[1, 1]]);
        // "X" at offset 1, then the whole second line at offset 3.
        assert_eq!(eb, vec![[1, 1], [1, 3]]);
    }
}
