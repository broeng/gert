//! The words of a notification: title and body of one event.

use model::{Event, EventKind};

/// The longest comment snippet shown, in characters.
const SNIPPET_CHARS: usize = 140;

/// The headline of `event`. `mention` is set when the rule that fired was a
/// mention rule: the same comment then reads "Mentioned by Alice".
pub fn title(event: &Event, mention: bool) -> String {
    let who = event
        .actor
        .as_ref()
        .map_or_else(|| "someone".to_owned(), |a| a.display());
    let n = event.change;
    match &event.kind {
        EventKind::Vote {
            label, value: 0, ..
        } => format!("Vote removed: {label} by {who}"),
        EventKind::Vote { label, value, .. } => format!("{label} {value:+} from {who}"),
        EventKind::Comment { .. } if mention => format!("Mentioned by {who}"),
        EventKind::Comment { .. } => format!("New comment from {who}"),
        EventKind::AttentionAdded => format!("Your turn on #{n}"),
        EventKind::ReviewerAdded { cc: false } => format!("Added as reviewer on #{n}"),
        EventKind::ReviewerAdded { cc: true } => format!("Added as CC on #{n}"),
        EventKind::NewPatchSet { number } => format!("New patch set {number} on #{n}"),
        EventKind::Merged => format!("Merged #{n}"),
        EventKind::Abandoned => format!("Abandoned #{n}"),
        EventKind::ReadyToSubmit => format!("Ready to submit #{n}"),
        EventKind::MergeConflict => format!("Merge conflict on #{n}"),
    }
}

/// `project: subject (#12345)`, and for a comment the start of its text on
/// a second line.
pub fn body(event: &Event) -> String {
    let mut body = format!("{}: {} (#{})", event.project, event.subject, event.change);
    if let EventKind::Comment { snippet, .. } = &event.kind {
        let snippet = shorten(snippet);
        if !snippet.is_empty() {
            body.push('\n');
            body.push_str(&snippet);
        }
    }
    body
}

/// The text on one line: whitespace runs (newlines included) become one
/// space, and anything beyond [`SNIPPET_CHARS`] is cut off with an
/// ellipsis.
pub fn shorten(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= SNIPPET_CHARS {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(SNIPPET_CHARS).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('\u{2026}');
    cut
}
