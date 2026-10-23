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

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{ChangeNumber, PatchSetNumber};

use crate::actions::Entry;
use crate::nav::{ChangeView, DiffRoute, Route};
use crate::session::ServerKey;

actions!(
    goto,
    [
        /// Esc: close the dialog.
        CloseGoto,
        /// Ctrl-Enter: add the server of a link that matches none.
        GotoAddServer,
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
        Entry {
            binding: KeyBinding::new("ctrl-enter", GotoAddServer, Some(GOTO_DIALOG)),
            keys: "Ctrl-Enter",
            description: "Add the server of a link that is for none of yours",
        },
        Entry {
            binding: KeyBinding::new("ctrl-enter", GotoAddServer, Some(IN_FIELD)),
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
    /// No configured server has this address. Holds the address a server
    /// for it would have (scheme, host and path prefix), which the Add
    /// server offer fills into the login page.
    #[error("None of your servers is at {0}.")]
    UnknownServer(String),
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
        return Err(LinkError::UnknownServer(
            server_origin(text).unwrap_or_else(|| authority.to_owned()),
        ));
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

/// A configured server, as far as links are concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkServer {
    pub key: ServerKey,
    pub base_url: String,
}

/// Parses `text` against all of `servers`, in config order, and returns the
/// one it is for with what it points at. A plain change number is for
/// `active`.
///
/// A server matches a link when the host (and port) are the same and the
/// link's path starts with the server's path prefix, on a segment boundary.
/// The longest prefix wins, so `https://h/r` beats `https://h` for a link
/// under `/r/`. Servers at the same address (the same site, different
/// users) tie: `active` is preferred, else the first in the list.
pub fn resolve_link(
    text: &str,
    servers: &[LinkServer],
    active: Option<&ServerKey>,
) -> Result<(ServerKey, ChangeLink), LinkError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(LinkError::Empty);
    }
    if number(trimmed).is_some() {
        let key = active
            .or(servers.first().map(|s| &s.key))
            .ok_or(LinkError::NotAChange)?;
        // A number needs no address to be read against.
        return parse_link(trimmed, "").map(|l| (key.clone(), l));
    }
    let mut candidates: Vec<(usize, &LinkServer)> = servers
        .iter()
        .filter_map(|s| prefix_depth(trimmed, &s.base_url).map(|d| (d, s)))
        .collect();
    // Stable: ties keep the config order.
    candidates.sort_by_key(|(depth, s)| (std::cmp::Reverse(*depth), Some(&s.key) != active));
    if candidates.is_empty() {
        return Err(match server_origin(trimmed) {
            Some(origin) => LinkError::UnknownServer(origin),
            None => LinkError::NotAChange,
        });
    }
    let mut error = LinkError::NotAChange;
    for (_, server) in candidates {
        match parse_link(trimmed, &server.base_url) {
            Ok(link) => return Ok((server.key.clone(), link)),
            Err(e) => error = e,
        }
    }
    Err(error)
}

/// How many bytes of path prefix `base_url` has in common with the link
/// `text` if the link is on that server (same host, path under the prefix);
/// `None` otherwise.
fn prefix_depth(text: &str, base_url: &str) -> Option<usize> {
    let (authority, rest) = split_url(text)?;
    let (base_authority, base_path) = split_url(base_url)?;
    if !authority.eq_ignore_ascii_case(base_authority) {
        return None;
    }
    let base_path = base_path.trim_end_matches('/');
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    let under = path == base_path
        || path
            .strip_prefix(base_path)
            .is_some_and(|r| r.starts_with('/'));
    // An old-style `/#/c/1` link has the prefix in front of the `#` too.
    under.then_some(base_path.len())
}

/// Path segments that start Gerrit's own routes: what comes before the
/// first of them is the prefix the server is mounted at.
const ROUTES: [&str; 9] = [
    "c",
    "q",
    "dashboard",
    "settings",
    "admin",
    "x",
    "plugins",
    "a",
    "id",
];

/// The address a Gerrit server would be added at for the link `text`:
/// scheme, host and the path prefix in front of Gerrit's own routes
/// (`https://h/r/c/p/+/1` is `https://h/r`). `None` if `text` is no URL.
pub fn server_origin(text: &str) -> Option<String> {
    let text = text.trim();
    let (authority, rest) = split_url(text)?;
    let scheme = &text[..text.find("://")?];
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    let mut prefix = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        if ROUTES.contains(&segment) || number(segment).is_some() {
            break;
        }
        prefix.push('/');
        prefix.push_str(segment);
    }
    Some(format!(
        "{}://{authority}{prefix}",
        scheme.to_ascii_lowercase()
    ))
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
    /// Open the change on that server.
    Open(ServerKey, ChangeLink),
    /// The link is for a server that is not configured: open the login page
    /// for a new server at `url`. `text` is what was typed, to be opened
    /// once the server is added.
    AddServer {
        url: String,
        text: String,
    },
    Close,
}

/// The Go to dialog: one field for a link or a change number. Enter opens
/// the change; a link that does not parse leaves the dialog open and says
/// why.
pub struct GotoDialog {
    servers: Vec<LinkServer>,
    active: Option<ServerKey>,
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
        servers: Vec<LinkServer>,
        active: Option<ServerKey>,
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
            servers,
            active,
            input,
            error,
            focus: cx.focus_handle(),
            _subs: subs,
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value();
        match resolve_link(&text, &self.servers, self.active.as_ref()) {
            Ok((key, link)) => cx.emit(GotoEvent::Open(key, link)),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    /// The address to offer to add, if the last attempt was a link to a
    /// server that is not configured.
    fn offer(&self) -> Option<&str> {
        match &self.error {
            Some(LinkError::UnknownServer(url)) => Some(url),
            _ => None,
        }
    }

    fn add_server(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self.offer().map(str::to_owned) else {
            return;
        };
        let text = self.input.read(cx).value().trim().to_owned();
        cx.emit(GotoEvent::AddServer { url, text });
    }

    fn on_add_server(&mut self, _: &GotoAddServer, _: &mut Window, cx: &mut Context<Self>) {
        self.add_server(cx);
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
    pub(crate) fn offer_for_test(&self) -> Option<String> {
        self.offer().map(str::to_owned)
    }

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<LinkError> {
        self.error.clone()
    }
}

impl GotoDialog {
    fn hint(&self) -> String {
        match self.servers.as_slice() {
            [one] => format!(
                "A link to a change on {}, or its number. Enter: open, Esc: close",
                one.base_url
            ),
            _ => "A link to a change on one of your servers, or a number for this one. \
                  Enter: open, Esc: close"
                .to_owned(),
        }
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
                    .on_action(cx.listener(Self::on_add_server))
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
                            .child(self.hint()),
                    )
                    .when_some(self.error.clone(), |d, e| {
                        d.child(div().text_color(theme.danger).child(e.to_string()))
                    })
                    .when_some(self.offer().map(str::to_owned), |d, url| {
                        d.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("goto-add-server")
                                        .primary()
                                        .label("Add server")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.add_server(cx)),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("Log in at {url} (Ctrl-Enter)")),
                                ),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use model::{ChangeNumber, PatchSetNumber};

    use super::{ChangeLink, LinkError, LinkServer, parse_link, resolve_link, server_origin};
    use crate::nav::{ChangeView, DiffRoute, Route};
    use crate::session::ServerKey;

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
            Err(LinkError::UnknownServer("https://other.org".into()))
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

    fn servers(list: &[(&str, &str)]) -> Vec<LinkServer> {
        list.iter()
            .map(|(k, u)| LinkServer {
                key: ServerKey::new(*k),
                base_url: (*u).to_owned(),
            })
            .collect()
    }

    fn resolved(text: &str, list: &[LinkServer], active: &str) -> Result<String, LinkError> {
        let active = ServerKey::new(active);
        resolve_link(text, list, Some(&active)).map(|(k, _)| k.to_string())
    }

    #[test]
    fn a_link_goes_to_the_server_with_its_host() {
        let list = servers(&[
            ("a", "https://a.example.org"),
            ("b", "https://b.example.org"),
        ]);
        assert_eq!(
            resolved("https://b.example.org/c/p/+/1", &list, "a"),
            Ok("b".into())
        );
        assert_eq!(
            resolved("https://A.example.org/c/p/+/1", &list, "b"),
            Ok("a".into())
        );
        // The change comes out as well.
        let (key, l) = resolve_link("https://b.example.org/c/p/+/9/2", &list, None).unwrap();
        assert_eq!(key, ServerKey::new("b"));
        assert_eq!(l, link(9, None, Some(2), None));
    }

    #[test]
    fn a_number_is_for_the_active_server() {
        let list = servers(&[
            ("a", "https://a.example.org"),
            ("b", "https://b.example.org"),
        ]);
        assert_eq!(resolved(" 12 ", &list, "b"), Ok("b".into()));
        assert_eq!(
            resolve_link("12", &list, None).map(|(k, _)| k.to_string()),
            Ok("a".into())
        );
        assert_eq!(resolve_link("12", &[], None), Err(LinkError::NotAChange));
    }

    #[test]
    fn the_same_host_with_different_paths_is_told_apart() {
        let list = servers(&[
            ("root", "https://h.example.org"),
            ("r", "https://h.example.org/r"),
            ("s", "https://h.example.org/s/"),
        ]);
        assert_eq!(
            resolved("https://h.example.org/r/c/p/+/1", &list, "root"),
            Ok("r".into())
        );
        assert_eq!(
            resolved("https://h.example.org/s/7", &list, "root"),
            Ok("s".into())
        );
        assert_eq!(
            resolved("https://h.example.org/c/p/+/1", &list, "r"),
            Ok("root".into())
        );
        // `/rr` is not under `/r`; only the root server is left, which
        // cannot make sense of it.
        assert_eq!(
            resolved("https://h.example.org/rr/c/p/+/1", &list, "r"),
            Err(LinkError::NotAChange)
        );
        // The old UI keeps the route in the fragment.
        assert_eq!(
            resolved("https://h.example.org/r/#/c/4/", &list, "root"),
            Ok("r".into())
        );
    }

    #[test]
    fn servers_at_one_address_prefer_the_active_one_else_the_first() {
        let list = servers(&[
            ("alice", "https://h.example.org"),
            ("bob", "https://h.example.org"),
        ]);
        let url = "https://h.example.org/c/p/+/1";
        assert_eq!(resolved(url, &list, "bob"), Ok("bob".into()));
        assert_eq!(resolved(url, &list, "alice"), Ok("alice".into()));
        assert_eq!(resolved(url, &list, "elsewhere"), Ok("alice".into()));
        assert_eq!(
            resolve_link(url, &list, None).map(|(k, _)| k.to_string()),
            Ok("alice".into())
        );
    }

    #[test]
    fn a_link_to_no_server_says_where_a_server_would_be() {
        let list = servers(&[("a", "https://a.example.org")]);
        let unknown = |text| resolved(text, &list, "a");
        assert_eq!(
            unknown("https://new.example.org/c/p/+/1"),
            Err(LinkError::UnknownServer("https://new.example.org".into()))
        );
        assert_eq!(
            unknown("HTTP://me@new.example.org:8080/gerrit/c/p/+/1/2/a.rs?x=1"),
            Err(LinkError::UnknownServer(
                "http://new.example.org:8080/gerrit".into()
            ))
        );
        assert_eq!(unknown("hello"), Err(LinkError::NotAChange));
        assert_eq!(unknown(" "), Err(LinkError::Empty));
        // A known host with something that is no change stays that.
        assert_eq!(
            unknown("https://a.example.org/q/status:open"),
            Err(LinkError::NotAChange)
        );
    }

    #[test]
    fn the_origin_stops_at_gerrits_own_routes() {
        let o = |t| server_origin(t);
        assert_eq!(o("https://h.org/c/p/+/1"), Some("https://h.org".into()));
        assert_eq!(
            o("https://h.org/gerrit/r/c/p/+/1"),
            Some("https://h.org/gerrit/r".into())
        );
        assert_eq!(
            o("https://h.org/gerrit/#/c/1/"),
            Some("https://h.org/gerrit".into())
        );
        assert_eq!(
            o("https://h.org/gerrit/123"),
            Some("https://h.org/gerrit".into())
        );
        assert_eq!(o("https://h.org"), Some("https://h.org".into()));
        assert_eq!(o("h.org/c/1"), None);
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
