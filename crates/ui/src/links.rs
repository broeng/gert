//! Clickable URLs in plain text (commit messages, change log, comments).
//!
//! [`find_urls`] is a pure scanner; [`linked_line`] renders one line of text
//! with its URLs underlined, coloured and opening in the system browser on a
//! plain click.

use std::ops::Range;

use gpui_kit::*;

/// Byte ranges of the `http://` / `https://` URLs in `text`.
///
/// A URL runs up to whitespace. Trailing punctuation and closing brackets
/// that do not balance an opening one inside the URL are not part of it, so
/// `(see https://x.org/a)` and `https://x.org/a.` link `https://x.org/a`,
/// while `https://en.wikipedia.org/wiki/Foo_(bar)` keeps its `)`.
pub fn find_urls(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(start) = next_scheme(text, from) {
        let rest = &text[start..];
        let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let end = start + trim_url(&rest[..len]);
        let scheme = if text[start..].starts_with("https://") {
            8
        } else {
            7
        };
        if end > start + scheme {
            out.push(start..end);
        }
        from = start + len.max(1);
    }
    out
}

fn next_scheme(text: &str, from: usize) -> Option<usize> {
    let http = text[from..].find("http://");
    let https = text[from..].find("https://");
    let ix = match (http, https) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b)?,
    };
    Some(from + ix)
}

/// Length of `url` after dropping trailing punctuation and unbalanced
/// closing brackets.
fn trim_url(url: &str) -> usize {
    let mut end = url.len();
    while let Some(c) = url[..end].chars().next_back() {
        let drop = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '»' | '”' | '’' | '›' => true,
            ')' | ']' | '>' | '}' => {
                let open = match c {
                    ')' => '(',
                    ']' => '[',
                    '>' => '<',
                    _ => '{',
                };
                let body = &url[..end];
                body.matches(c).count() > body.matches(open).count()
            }
            _ => false,
        };
        if !drop {
            break;
        }
        end -= c.len_utf8();
    }
    end
}

/// One line of text. Without URLs it is the plain string; with them the URLs
/// are underlined in `color` and a click on one opens it in the browser.
/// `id` must be unique among the elements of the page.
pub fn linked_line(id: impl Into<ElementId>, text: &str, color: Hsla) -> AnyElement {
    let ranges = find_urls(text);
    if ranges.is_empty() {
        return text.to_owned().into_any_element();
    }
    let urls: Vec<String> = ranges.iter().map(|r| text[r.clone()].to_owned()).collect();
    let style = HighlightStyle {
        color: Some(color),
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: false,
        }),
        ..Default::default()
    };
    let highlights = ranges.iter().map(|r| (r.clone(), style));
    InteractiveText::new(
        id,
        StyledText::new(text.to_owned()).with_highlights(highlights),
    )
    .on_click(ranges, move |ix, _, cx| {
        if let Some(url) = urls.get(ix) {
            cx.open_url(url);
        }
    })
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::find_urls;

    fn found(text: &str) -> Vec<&str> {
        find_urls(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn plain_url() {
        assert_eq!(found("https://example.org/a"), ["https://example.org/a"]);
        assert_eq!(found("see http://x.org/a b"), ["http://x.org/a"]);
    }

    #[test]
    fn url_at_start_and_end() {
        assert_eq!(found("https://a.org text"), ["https://a.org"]);
        assert_eq!(found("text https://a.org"), ["https://a.org"]);
    }

    #[test]
    fn trailing_punctuation_is_dropped() {
        assert_eq!(found("see https://a.org/x."), ["https://a.org/x"]);
        assert_eq!(found("https://a.org/x?!,;:'\""), ["https://a.org/x"]);
        assert_eq!(found("https://a.org/x?q=1"), ["https://a.org/x?q=1"]);
    }

    #[test]
    fn unbalanced_brackets_are_dropped() {
        assert_eq!(found("(see https://a.org/x)"), ["https://a.org/x"]);
        assert_eq!(found("(see https://a.org/x)."), ["https://a.org/x"]);
        assert_eq!(found("<https://a.org/x>"), ["https://a.org/x"]);
        assert_eq!(found("[https://a.org/x]"), ["https://a.org/x"]);
    }

    #[test]
    fn balanced_parens_are_kept() {
        assert_eq!(
            found("https://en.wikipedia.org/wiki/Foo_(bar)"),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
        assert_eq!(
            found("(https://en.wikipedia.org/wiki/Foo_(bar))."),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
    }

    #[test]
    fn several_urls_per_line() {
        let t = "a https://a.org, b http://b.org/x; c https://c.org";
        assert_eq!(
            found(t),
            ["https://a.org", "http://b.org/x", "https://c.org"]
        );
    }

    #[test]
    fn non_ascii_around_the_url() {
        let t = "så «https://a.org/æ» ok";
        let r = find_urls(t);
        assert_eq!(r.len(), 1);
        assert_eq!(&t[r[0].clone()], "https://a.org/æ");
        assert_eq!(found("æøå https://a.org/ü."), ["https://a.org/ü"]);
    }

    #[test]
    fn not_links() {
        assert!(found("example.org/a").is_empty());
        assert!(found("www.example.org").is_empty());
        assert!(found("http://").is_empty());
        assert!(found("see https:// there").is_empty());
        assert!(found("http://.").is_empty());
        assert!(found("ftp://a.org").is_empty());
    }
}
