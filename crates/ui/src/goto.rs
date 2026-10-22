//! Opening a change from a link to it in the Gerrit web UI: the Go to
//! dialog (`g`, prefilled from the clipboard) and the link parser that the
//! command line uses too.
//!
//! [`parse_link`] is pure. It understands the URLs PolyGerrit shows and
//! the older forms Gerrit still redirects:
//!
//! - `<base>/c/<project>/+/<change>[/<ps>|/<base>..<ps>[/<file>]]`
//! - `<base>/c/<change>[/...]` and `<base>/<change>`
//! - `<base>/#/c/<change>/...` (the old GWT UI)
//! - a plain change number.
//!
//! Anything after the change that is not a patch set (`/comment/<id>`,
//! `/edit`, ...) is ignored, as are the query and a `#<line>` fragment.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{ChangeNumber, PatchSetNumber};

use crate::actions::Entry;
use crate::nav::{ChangeView, DiffRoute, Route};

actions!(
    goto,
    [
        /// Esc: close the dialog.
        CloseGoto,
    ]
);

/// Key context of the dialog.
pub const GOTO_DIALOG: &str = "GotoDialog";

/// As deep as the field's own context, to win over its Esc.
const IN_FIELD: &str = "GotoDialog > Input";

pub fn entries() -> Vec<Entry> {
    vec![
        Entry {
            binding: KeyBinding::new("escape", CloseGoto, Some(GOTO_DIALOG)),
            keys: "Esc",
            description: "Close the dialog (Enter opens the change)",
        },
        Entry {
            binding: KeyBinding::new("escape", CloseGoto, Some(IN_FIELD)),
            keys: "",
            description: "",
        },
    ]
}

/// Registers the dialog's bindings. Call once at startup, after the
/// component library's own.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}

/// What a link points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeLink {
    pub change: ChangeNumber,
    /// The patch set compared with; `None` is the parent commit.
    pub base: Option<PatchSetNumber>,
    pub patch_set: Option<PatchSetNumber>,
    pub path: Option<String>,
}

impl ChangeLink {
    /// The page to open, and for a change page the patch sets and file to
    /// show on it.
    pub fn route(&self) -> (Route, Option<ChangeView>) {
        match (self.patch_set, &self.path) {
            (Some(target), Some(path)) => (
                Route::Diff(DiffRoute {
                    change: self.change,
                    base: self.base,
                    target,
                    path: path.clone(),
                    thread: None,
                }),
                None,
            ),
            (None, None) => (Route::Change(self.change), None),
            (target, path) => (
                Route::Change(self.change),
                Some(ChangeView {
                    base: self.base,
                    target,
                    file: path.clone(),
                }),
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("Paste a link to a change, or type its number.")]
    Empty,
    #[error("This link is for {0}, not for this server.")]
    OtherServer(String),
    #[error("This is not a link to a change.")]
    NotAChange,
}

/// Parses `text`, a link to a change on the server at `base_url` (as the
/// login stores it: with scheme, without a trailing slash) or a change
/// number.
pub fn parse_link(text: &str, base_url: &str) -> Result<ChangeLink, LinkError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(LinkError::Empty);
    }
    if let Some(change) = number(text) {
        return Ok(ChangeLink {
            change: ChangeNumber(change),
            base: None,
            patch_set: None,
            path: None,
        });
    }
    let (authority, rest) = split_url(text).ok_or(LinkError::NotAChange)?;
    let (base_authority, base_path) = split_url(base_url).ok_or(LinkError::NotAChange)?;
    if !authority.eq_ignore_ascii_case(base_authority) {
        return Err(LinkError::OtherServer(authority.to_owned()));
    }
    // The old UI routed in the fragment: `/#/c/123/`.
    let path = match rest.split_once('#') {
        Some((_, fragment)) if fragment.starts_with('/') => fragment,
        Some((path, _)) => path,
        None => rest,
    };
    let path = path.split('?').next().unwrap_or_default();
    let path = path.strip_prefix(base_path).unwrap_or(path);
    let path = path.trim_start_matches('/');
    let segments = match path.strip_prefix("c/") {
        Some(rest) => match rest.split_once("/+/") {
            Some((_project, rest)) => rest,
            None => rest,
        },
        None => path,
    };
    let mut segments = segments.split('/');
    let change = segments
        .next()
        .and_then(number)
        .ok_or(LinkError::NotAChange)?;
    let mut link = ChangeLink {
        change: ChangeNumber(change),
        base: None,
        patch_set: None,
        path: None,
    };
    let Some(range) = segments.next() else {
        return Ok(link);
    };
    match range.split_once("..") {
        // A negative base is a parent of a merge commit; show the change's
        // default instead.
        Some((base, target)) => {
            link.base = number(base).and_then(patch_set);
            link.patch_set = number(target).and_then(patch_set);
        }
        None => link.patch_set = number(range).and_then(patch_set),
    }
    if link.patch_set.is_none() {
        link.base = None;
        return Ok(link);
    }
    let file = segments.collect::<Vec<_>>().join("/");
    if !file.is_empty() {
        link.path = Some(percent_decode(&file));
    }
    Ok(link)
}

/// A positive decimal number.
fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|&n| n > 0)
}

fn patch_set(n: u64) -> Option<PatchSetNumber> {
    u32::try_from(n).ok().map(PatchSetNumber)
}

/// Splits an `http(s)://` URL into its host (and port, without user info)
/// and the rest, which is empty or starts with `/`, `?` or `#`.
fn split_url(url: &str) -> Option<(&str, &str)> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let after = if lower.starts_with("https://") {
        &url[8..]
    } else if lower.starts_with("http://") {
        &url[7..]
    } else {
        return None;
    };
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..end];
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    (!authority.is_empty()).then_some((authority, &after[end..]))
}

/// Decodes `%XX` escapes; anything else, including a broken escape, stays.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(&h), Some(&l)) if hex(h).is_some() && hex(l).is_some() => {
                let (h, l) = (hex(h).unwrap_or(0), hex(l).unwrap_or(0));
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the dialog tells the shell.
pub enum GotoEvent {
    Open(ChangeLink),
    Close,
}

/// The Go to dialog: one field for a link or a change number. Enter opens
/// the change; a link that does not parse leaves the dialog open and says
/// why.
pub struct GotoDialog {
    base_url: String,
    input: Entity<InputState>,
    error: Option<LinkError>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<GotoEvent> for GotoDialog {}

impl Focusable for GotoDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl GotoDialog {
    /// Opens the dialog with `text` in the field, selected so that typing
    /// replaces it, and the focus in it. `error` is shown until the text is
    /// edited (a link from the command line that did not parse).
    pub fn new(
        base_url: String,
        text: String,
        error: Option<LinkError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut s = InputState::new(window, cx)
                .placeholder("https://gerrit.example.org/c/project/+/12345");
            s.set_value(text, window, cx);
            s.select_all(window, cx);
            s
        });
        let subs =
            vec![
                cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.submit(cx),
                    _ => {}
                }),
            ];
        input.update(cx, |s, cx| s.focus(window, cx));
        Self {
            base_url,
            input,
            error,
            focus: cx.focus_handle(),
            _subs: subs,
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value();
        match parse_link(&text, &self.base_url) {
            Ok(link) => cx.emit(GotoEvent::Open(link)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn close(&mut self, _: &CloseGoto, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(GotoEvent::Close);
    }

    #[cfg(test)]
    pub(crate) fn submit_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input
            .update(cx, |s, cx| s.set_value(text.to_owned(), window, cx));
        self.submit(cx);
    }

    #[cfg(test)]
    pub(crate) fn text_for_test(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<LinkError> {
        self.error.clone()
    }
}

impl Render for GotoDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .id("goto-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            // Not stretched to the bottom: as tall as its content.
            .items_start()
            .justify_center()
            .pt(px(120.))
            .bg(crate::theme::modal_backdrop(cx))
            .child(
                v_flex()
                    .id("goto-dialog")
                    .key_context(GOTO_DIALOG)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::close))
                    .w(px(640.))
                    .max_w_full()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .child(div().text_base().font_semibold().child("Go to a change"))
                    .child(Input::new(&self.input))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "A link to a change on {}, or its number. Enter: open, Esc: close",
                                self.base_url
                            )),
                    )
                    .when_some(self.error.clone(), |d, e| {
                        d.child(div().text_color(theme.danger).child(e.to_string()))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use model::{ChangeNumber, PatchSetNumber};

    use super::{ChangeLink, LinkError, parse_link};
    use crate::nav::{ChangeView, DiffRoute, Route};

    const BASE: &str = "https://review.example.org";

    fn link(change: u64, base: Option<u32>, ps: Option<u32>, path: Option<&str>) -> ChangeLink {
        ChangeLink {
            change: ChangeNumber(change),
            base: base.map(PatchSetNumber),
            patch_set: ps.map(PatchSetNumber),
            path: path.map(str::to_owned),
        }
    }

    fn ok(text: &str) -> ChangeLink {
        parse_link(text, BASE).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn change_number() {
        assert_eq!(ok(" 12345\n"), link(12345, None, None, None));
    }

    #[test]
    fn polygerrit_change() {
        let l = link(123, None, None, None);
        assert_eq!(ok("https://review.example.org/c/proj/+/123"), l);
        assert_eq!(ok("https://review.example.org/c/proj/+/123/"), l);
        assert_eq!(ok("https://review.example.org/c/a/b/c/+/123"), l);
        assert_eq!(ok("https://review.example.org/c/proj/+/123?tab=checks"), l);
        assert_eq!(ok("https://review.example.org/c/proj/+/123#message-abc"), l);
    }

    #[test]
    fn patch_sets_and_files() {
        assert_eq!(
            ok("https://review.example.org/c/proj/+/123/4"),
            link(123, None, Some(4), None)
        );
        assert_eq!(
            ok("https://review.example.org/c/proj/+/123/2..4"),
            link(123, Some(2), Some(4), None)
        );
        assert_eq!(
            ok("https://review.example.org/c/proj/+/123/4/src/main.rs"),
            link(123, None, Some(4), Some("src/main.rs"))
        );
        assert_eq!(
            ok("https://review.example.org/c/proj/+/123/2..4/src/a%20b.rs#40"),
            link(123, Some(2), Some(4), Some("src/a b.rs"))
        );
        assert_eq!(
            ok("https://review.example.org/c/proj/+/123/4/COMMIT_MSG"),
            link(123, None, Some(4), Some("COMMIT_MSG"))
        );
    }

    #[test]
    fn merge_parent_base_is_dropped() {
        assert_eq!(
            ok("https://review.example.org/c/proj/+/123/-1..4/a.rs"),
            link(123, None, Some(4), Some("a.rs"))
        );
    }

    #[test]
    fn other_suffixes_open_the_change() {
        let l = link(123, None, None, None);
        assert_eq!(
            ok("https://review.example.org/c/proj/+/123/comment/abc/"),
            l
        );
        assert_eq!(ok("https://review.example.org/c/proj/+/123/edit/a.rs"), l);
    }

    #[test]
    fn old_forms() {
        let l = link(123, None, None, None);
        assert_eq!(ok("https://review.example.org/123"), l);
        assert_eq!(ok("https://review.example.org/c/123/"), l);
        assert_eq!(ok("https://review.example.org/#/c/123/"), l);
        assert_eq!(
            ok("https://review.example.org/#/c/123/3/a.rs"),
            link(123, None, Some(3), Some("a.rs"))
        );
    }

    #[test]
    fn host_matching_ignores_scheme_case_and_user() {
        let l = link(5, None, None, None);
        assert_eq!(ok("http://review.example.org/c/p/+/5"), l);
        assert_eq!(ok("HTTPS://Review.Example.org/c/p/+/5"), l);
        assert_eq!(ok("https://me@review.example.org/c/p/+/5"), l);
    }

    #[test]
    fn base_path_prefix() {
        let base = "https://example.org/r";
        let l = link(7, None, None, None);
        assert_eq!(
            parse_link("https://example.org/r/c/p/+/7", base),
            Ok(l.clone())
        );
        assert_eq!(parse_link("https://example.org/r/7", base), Ok(l));
    }

    #[test]
    fn errors() {
        assert_eq!(parse_link("  ", BASE), Err(LinkError::Empty));
        assert_eq!(
            parse_link("https://other.org/c/p/+/1", BASE),
            Err(LinkError::OtherServer("other.org".into()))
        );
        for text in [
            "https://review.example.org/",
            "https://review.example.org/q/status:open",
            "https://review.example.org/c/p/+/abc",
            "review.example.org/c/p/+/1",
            "0",
            "hello",
        ] {
            assert_eq!(parse_link(text, BASE), Err(LinkError::NotAChange), "{text}");
        }
    }

    #[test]
    fn routes() {
        assert_eq!(
            link(1, None, None, None).route(),
            (Route::Change(ChangeNumber(1)), None)
        );
        let (route, view) = link(1, Some(2), Some(3), None).route();
        assert_eq!(route, Route::Change(ChangeNumber(1)));
        assert_eq!(
            view,
            Some(ChangeView {
                base: Some(PatchSetNumber(2)),
                target: Some(PatchSetNumber(3)),
                file: None,
            })
        );
        let (route, view) = link(1, None, Some(3), Some("a.rs")).route();
        assert_eq!(view, None);
        assert_eq!(
            route,
            Route::Diff(DiffRoute {
                change: ChangeNumber(1),
                base: None,
                target: PatchSetNumber(3),
                path: "a.rs".into(),
                thread: None,
            })
        );
    }
}
