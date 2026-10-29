//! Rendering of the change page blocks.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{
    Account, COMMIT_MSG_PATH, ChangeDetail, ChangeMessage, ChangeStatus, ChangeSummary, FileEntry,
    FileStatus, MERGE_LIST_PATH, RelatedChange,
};

use super::{
    ChangeEvent, ChangePage, Phase,
    state::{Block, size_text},
};
use crate::{
    actions::*,
    filters::{FileStyle, FilterAction},
    pages::list::Status,
    row::{browser_url, chips, has_merge_conflict},
    theme::chip_colors,
    time_fmt::{clock, relative},
};

/// The cards of the page. Consecutive blocks of one section are drawn as
/// one card.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Files,
    Comments,
    Log,
}

impl Section {
    fn of(b: Block) -> Option<Self> {
        match b {
            Block::Header => None,
            Block::PatchSets | Block::FilterBar | Block::FilesNotice | Block::File(_) => {
                Some(Self::Files)
            }
            Block::ThreadsHeader
            | Block::ThreadsNotice
            | Block::Thread(_)
            | Block::PatchSetComments => Some(Self::Comments),
            Block::LogHeader | Block::Message(_) => Some(Self::Log),
        }
    }
}

pub(super) use crate::card::title_bar as card_title_bar;

/// Width for 72 monospace columns, the card padding and border.
/// Width of the relation chain side panel: room for `#12345`, a subject of
/// ~55 characters and the status pills, while the left side keeps ~640px
/// at the narrowest window that still gets the panel.
const RELATED_PANEL_WIDTH: f32 = 540.;
/// Below this window width the chain stays in its box, wanted panel or not.
const RELATED_PANEL_MIN_WINDOW: f32 = 1180.;
const COMMIT_BASIS: f32 = 660.;

pub(super) fn pill(text: impl Into<SharedString>, fg: Hsla) -> Div {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .text_xs()
        .text_color(fg)
        .bg(fg.opacity(0.15))
        .child(text.into())
}

/// "Merge conflict" in solid red with its own text colour, so it stands out
/// from the tinted pills as it does in the web UI.
fn conflict_pill(cx: &App) -> Div {
    let t = cx.theme();
    pill("Merge conflict", t.danger)
        .bg(t.danger)
        .text_color(t.danger_foreground)
        .font_bold()
}

/// One div per line, URLs in them clickable. `id` must be unique on the page.
fn lines_of(id: impl Into<ElementId>, text: &str, empty_pad: bool, link: Hsla) -> Vec<AnyElement> {
    let id = id.into();
    text.trim_end()
        .lines()
        .enumerate()
        .map(|(i, l)| {
            let line = if l.is_empty() && empty_pad { " " } else { l };
            div()
                .child(crate::links::linked_line(
                    (id.clone(), format!("line-{i}")),
                    line,
                    link,
                ))
                .into_any_element()
        })
        .collect()
}

fn people(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(Account::display)
        .collect::<Vec<_>>()
        .join(", ")
}

fn status_label(s: &ChangeStatus) -> (&'static str, bool) {
    match s {
        ChangeStatus::New => ("NEW", false),
        ChangeStatus::Merged => ("MERGED", false),
        ChangeStatus::Abandoned => ("ABANDONED", false),
        ChangeStatus::Unknown(_) => ("", true),
    }
}

impl ChangePage {
    pub(super) fn render_block(
        &mut self,
        ix: usize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(block) = self.blocks.get(ix).copied() else {
            return div().into_any_element();
        };
        let inner = match block {
            Block::Header => self.header(cx),
            Block::PatchSets => self.patch_sets(cx),
            Block::FilterBar => self.filter_bar(cx),
            Block::FilesNotice => self.files_notice(cx),
            Block::File(i) => self.file(i, cx),
            Block::ThreadsHeader => self.threads_header(cx),
            Block::ThreadsNotice => self.threads_notice(cx),
            Block::Thread(i) => self.thread_row(i, cx),
            Block::PatchSetComments => self.patchset_comments(cx),
            Block::LogHeader => self.log_header(cx),
            Block::Message(i) => self.message(i, cx),
        };
        let Some(section) = Section::of(block) else {
            return div().w_full().px_4().py_1().child(inner).into_any_element();
        };
        let first = ix
            .checked_sub(1)
            .and_then(|p| self.blocks.get(p))
            .is_none_or(|b| Section::of(*b) != Some(section));
        let last = self
            .blocks
            .get(ix + 1)
            .is_none_or(|b| Section::of(*b) != Some(section));
        // The section is a card drawn row by row: the title bar on top
        // with the rounded corners, side borders all the way, the bottom
        // border and corners on the last row.
        let row = if first {
            inner
        } else {
            crate::card::row_separator(cx)
                .px_2()
                .py_1()
                .child(inner)
                .into_any_element()
        };
        crate::card::frame(row, first, last, cx).into_any_element()
    }

    fn status_line(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let status = self.status_text(cx);
        match self.hint.current() {
            Some((text, error)) => h_flex()
                .gap_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(if error {
                            theme.danger
                        } else {
                            theme.foreground
                        })
                        .child(text.to_owned()),
                )
                .child(status)
                .into_any_element(),
            None => status,
        }
    }

    fn status_text(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        match &self.status {
            Status::Refreshing | Status::LoadingMore => div()
                .text_xs()
                .text_color(muted)
                .child("refreshing\u{2026}")
                .into_any_element(),
            Status::Failed(msg) => h_flex()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(format!("error: {msg}")),
                )
                .child(
                    Button::new("retry")
                        .label("Retry")
                        .xsmall()
                        .on_click(cx.listener(|this, _, w, cx| this.refresh_now(&Refresh, w, cx))),
                )
                .into_any_element(),
            Status::Idle => match self.fetched_at {
                Some(t) => div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!(
                        "updated {}",
                        relative(Timestamp::now(), t, &TimeZone::system())
                    ))
                    .into_any_element(),
                None => div().into_any_element(),
            },
        }
    }

    fn header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        if self.detail.is_none() {
            let status = self.status_line(cx);
            let text = if self.loaded && matches!(self.status, Status::Failed(_)) {
                "Could not load the change."
            } else {
                "Loading\u{2026}"
            };
            return v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_lg().child(format!("Change {}", self.number)))
                        .children(self.server_badge.as_ref().map(|b| b.pill(cx))),
                )
                .child(div().text_color(muted).child(text))
                .child(status)
                .into_any_element();
        }
        let bar = (!self.panel_shown).then(|| self.header_bar(cx)).flatten();
        let info = self.change_info(cx);
        let commit = self.commit(cx);
        let related = (!self.panel_shown)
            .then(|| self.related_block(false, cx))
            .flatten();
        // Wrapping columns: each grows from a basis, so a narrow window
        // stacks them instead of squeezing the text.
        let column = |basis: f32, child: AnyElement| {
            div()
                .flex_grow(1.)
                .flex_basis(px(basis))
                .min_w(px(260.))
                .max_w_full()
                .child(child)
        };
        // The commit message needs ~72 monospace columns (plus padding) to
        // not wrap in the middle; it keeps that width while there is room
        // and shrinks or wraps only when the window is narrower.
        let commit_column = |child: AnyElement| {
            div()
                .flex_grow(1.)
                .flex_basis(px(COMMIT_BASIS))
                .min_w(px(260.))
                .max_w_full()
                .child(child)
        };
        v_flex()
            .gap_2()
            .children(bar)
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .flex_wrap()
                    .gap_x_4()
                    .gap_y_3()
                    .child(column(320., info))
                    .child(commit_column(commit))
                    .when_some(related, |d, r| d.child(column(320., r))),
            )
            .into_any_element()
    }

    /// The status / number / subject bar. With the relation chain as a side
    /// panel it is drawn above both the list and the panel, not in the list.
    pub(super) fn header_bar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let status = self.status_line(cx);
        let d = self.detail.as_ref()?;
        let c = &d.summary;
        let (status_text, unknown) = status_label(&c.status);
        let status_color = match c.status {
            ChangeStatus::Merged => theme.success,
            ChangeStatus::Abandoned => theme.muted_foreground,
            _ => theme.primary,
        };
        let voting = self.voting();
        let quick: Vec<_> = self
            .quick_buttons(cx)
            .into_iter()
            .enumerate()
            .map(|(i, q)| {
                let key = match i {
                    0 => "Shift-V".to_owned(),
                    1..=8 => format!("v {}", i + 1),
                    _ => String::new(),
                };
                let label = match (q.applied, key.is_empty()) {
                    (true, _) => format!("\u{2713} {}", q.text()),
                    (false, true) => q.text(),
                    (false, false) => format!("{}  ({key})", q.text()),
                };
                // Code-Review is the vote that is cast most, so it stands out.
                let strong = q.label == "Code-Review";
                let reject = q.value < 0;
                let disabled = q.applied || voting;
                let color = if reject {
                    cx.theme().danger
                } else {
                    cx.theme().primary
                };
                Button::new(("quick-vote", i))
                    .label(label)
                    .small()
                    .custom(crate::theme::vote_button(strong, reject, cx))
                    // Over the variant's faint idle background; a disabled
                    // button keeps its disabled look.
                    .when(strong && !disabled, |b| b.bg(color))
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, _, cx| this.quick_vote(i, cx)))
            })
            .collect();
        let submit_chain =
            (c.status == ChangeStatus::New && c.submittable == Some(true)).then(|| {
                Button::new("submit-chain")
                    .label("Submit chain")
                    .small()
                    .custom(crate::theme::vote_button(true, false, cx))
                    .bg(cx.theme().primary)
                    .on_click(cx.listener(|this, _, _, cx| this.submit_chain(cx)))
            });
        let bar = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .flex_wrap()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(if unknown {
                pill(c.status.to_string(), muted)
            } else {
                pill(status_text, status_color)
            })
            .when(has_merge_conflict(c), |d| d.child(conflict_pill(cx)))
            .child({
                let url = browser_url(&self.session.base_url, &c.project, c.number);
                div()
                    .id("change-number-link")
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(|d| d.underline())
                    .on_click(move |_, _, cx| cx.open_url(&url))
                    .child(format!("#{}", c.number))
            })
            .children(self.server_badge.as_ref().map(|b| b.pill(cx)))
            .children(
                chips(c)
                    .into_iter()
                    .filter(|chip| {
                        // The status pill says these, and the conflict
                        // has its own solid one.
                        !matches!(
                            chip,
                            crate::row::Chip::Merged
                                | crate::row::Chip::Abandoned
                                | crate::row::Chip::MergeConflict
                        )
                    })
                    .map(|chip| pill(chip.text(), chip_colors(chip, cx).0)),
            )
            .child(
                div()
                    .min_w_0()
                    .text_lg()
                    .font_semibold()
                    .child(c.subject.clone()),
            )
            .child(div().flex_1())
            .children(submit_chain)
            .children(quick)
            .child(status);
        Some(bar.into_any_element())
    }

    /// First column of the top: the Reply button, who and where, then the
    /// submit requirements and the votes.
    fn change_info(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let c = &d.summary;
        let now = Timestamp::now();
        let tz = TimeZone::system();
        let field = |name: &'static str, value: String| {
            h_flex()
                .gap_3()
                .text_sm()
                .items_baseline()
                .child(div().w(px(110.)).flex_none().text_color(muted).child(name))
                .child(div().min_w_0().child(value))
        };
        let list = |name: &'static str, list: &[Account]| {
            field(
                name,
                if list.is_empty() {
                    "none".to_owned()
                } else {
                    people(list)
                },
            )
        };
        let fields = v_flex()
            .gap_0p5()
            .child(field("Owner", c.owner.display()))
            .child(list("Reviewers", &d.reviewers))
            .child(list("CC", &d.cc))
            .child(field(
                "Repo | Branch",
                format!("{} | {}", c.project, c.branch),
            ))
            .when_some(c.topic.clone(), |d, t| d.child(field("Topic", t)))
            .when(!c.hashtags.is_empty(), |d| {
                d.child(field(
                    "Hashtags",
                    c.hashtags
                        .iter()
                        .map(|h| format!("#{h}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                ))
            })
            .child(field("Updated", relative(now, c.updated, &tz)))
            .child(field("Created", relative(now, c.created, &tz)));
        let has_requirements = !d.submit_requirements.is_empty();
        let requirements = has_requirements.then(|| self.requirements(cx));
        let votes = self.votes(cx);
        // Not for anonymous sessions, which cannot reply.
        let reply = self.session.can_comment().then(|| {
            h_flex().child(
                Button::new("reply")
                    .label("Reply  (A)")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| this.reply(&Reply, window, cx))),
            )
        });
        v_flex()
            .gap_2()
            .children(reply)
            .child(fields)
            .children(requirements)
            .children(votes)
            .into_any_element()
    }

    /// The commit message of the patch set being shown.
    fn target_message(&self, d: &ChangeDetail) -> Option<String> {
        let target = self.sel.target_number(d)?;
        d.revision(target)
            .and_then(|r| r.commit.as_ref()?.message.clone())
            .or_else(|| {
                (Some(target) == d.current_patch_set)
                    .then(|| d.commit_message.clone())
                    .flatten()
            })
    }

    fn commit(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let message = self.target_message(d);
        let sha = self
            .sel
            .target_ref(d)
            .map(|t| t.sha.short(12).to_owned())
            .unwrap_or_default();
        let open = self.commit_open;
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .id("commit-toggle")
                    .gap_2()
                    .cursor_pointer()
                    .on_click(
                        cx.listener(|this, _, w, cx| this.toggle_commit(&ToggleCommit, w, cx)),
                    )
                    .child(div().text_color(theme.muted_foreground).child(if open {
                        "\u{25BE}"
                    } else {
                        "\u{25B8}"
                    }))
                    .child(section_title("Commit message", cx))
                    .child(
                        div()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(sha),
                    ),
            )
            .when(open, |d| {
                d.child(
                    v_flex()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        // Set off from the columns beside it, which sit on
                        // the page background.
                        .bg(crate::theme::commit_card_background(cx))
                        .text_color(theme.foreground)
                        .font_family(theme.mono_font_family.clone())
                        .text_sm()
                        .children(lines_of(
                            "commit-message",
                            message.as_deref().unwrap_or("(not available)"),
                            true,
                            theme.link,
                        )),
                )
            })
            .into_any_element()
    }

    fn patch_sets(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let target = self.sel.target_number(d);
        let base = self.sel.base_number(d);
        let current = d.current_patch_set;
        let target_text = match target {
            Some(n) if Some(n) == current => format!("Patchset {n} (current)"),
            Some(n) => format!("Patchset {n}"),
            None => "unknown".into(),
        };
        let base_text = match base {
            Some(n) => format!("Patchset {n}"),
            None => "Base".into(),
        };
        let info = target
            .and_then(|n| d.revision(n))
            .map(|r| {
                let mut parts = Vec::new();
                if let Some(u) = &r.uploader {
                    parts.push(format!("uploaded by {}", u.display()));
                }
                if let Some(t) = r.created {
                    parts.push(relative(Timestamp::now(), t, &TimeZone::system()));
                }
                if let Some(desc) = r.description.as_deref().filter(|s| !s.is_empty()) {
                    parts.push(format!("\u{201C}{desc}\u{201D}"));
                }
                parts.join(", ")
            })
            .unwrap_or_default();
        let files = &self.files.list;
        let ins: u32 = files
            .iter()
            .filter(|f| !f.is_special())
            .map(|f| f.lines_inserted)
            .sum();
        let del: u32 = files
            .iter()
            .filter(|f| !f.is_special())
            .map(|f| f.lines_deleted)
            .sum();
        let count = files.iter().filter(|f| !f.is_special()).count();
        v_flex()
            .child(
                card_title_bar(cx)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .flex_wrap()
                            .child(section_title("Files", cx))
                            .child(div().text_sm().text_color(muted).child("Compare"))
                            .child(
                                Button::new("base-ps")
                                    .label(format!("{base_text}  (b)"))
                                    .small()
                                    .outline()
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.next_base(&NextBase, w, cx)
                                    })),
                            )
                            .child(div().text_sm().text_color(muted).child("with"))
                            .child(
                                Button::new("target-ps")
                                    .label(format!("{target_text}  (t)"))
                                    .small()
                                    .outline()
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.next_target(&NextTarget, w, cx)
                                    })),
                            )
                            .child(div().flex_1())
                            .when(self.files.loaded, |d| {
                                d.child(
                                    h_flex()
                                        .gap_2()
                                        .text_sm()
                                        .text_color(muted)
                                        .child(format!("{count} files"))
                                        .child(
                                            div()
                                                .text_color(theme.success)
                                                .child(format!("+{ins}")),
                                        )
                                        .child(
                                            div()
                                                .text_color(theme.danger)
                                                .child(format!("\u{2212}{del}")),
                                        ),
                                )
                            }),
                    )
                    .when(!info.is_empty(), |d| {
                        d.child(div().text_xs().text_color(muted).child(info))
                    }),
            )
            .into_any_element()
    }

    /// The file filter buttons, "N hidden" and the Manage filters button;
    /// the file table's header sits under them.
    fn filter_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let project = d.summary.project.clone();
        let rules: Vec<_> = {
            let filters = self.filters.read(cx);
            filters
                .applicable(&self.session.key, &project)
                .into_iter()
                .map(|r| (r.name.clone(), filters.is_active(r), r.action.clone()))
                .collect()
        };
        let buttons: Vec<_> = rules
            .into_iter()
            .enumerate()
            .map(|(i, (name, on, action))| {
                let label = match i {
                    0..=8 => format!("{name}  (f {})", i + 1),
                    _ => name.clone(),
                };
                let button = Button::new(("file-filter", i))
                    .label(label)
                    .small()
                    .when(on, |b| b.primary())
                    .when(!on, |b| b.outline())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.filters.update(cx, |f, cx| f.toggle(&name, cx))
                    }));
                // What the rule does, next to the button: a swatch of the
                // highlight, or the word.
                let effect = match &action {
                    FilterAction::Hide => div().text_xs().text_color(muted).child("hide"),
                    FilterAction::Dim => div().text_xs().text_color(muted).child("dim"),
                    FilterAction::Highlight { color, bold } => {
                        let swatch = div()
                            .size_2()
                            .rounded_full()
                            .bg(color.map_or(theme.foreground, Hsla::from));
                        h_flex()
                            .gap_0p5()
                            .items_center()
                            .text_xs()
                            .text_color(muted)
                            .child(swatch)
                            .when(*bold, |d| d.child("bold"))
                    }
                };
                h_flex()
                    .gap_1()
                    .items_center()
                    .opacity(if on { 1. } else { 0.8 })
                    .child(button)
                    .child(effect)
            })
            .collect();
        let hidden = self.files.list.len() - self.visible.len();
        v_flex()
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_3()
                    .items_center()
                    .flex_wrap()
                    .children(buttons)
                    .when(hidden > 0, |d| {
                        d.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(format!("{hidden} hidden")),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("manage-filters")
                            .label("Manage filters  (f m)")
                            .small()
                            .outline()
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ChangeEvent::ManageFilters {
                                    project: project.clone(),
                                })
                            })),
                    ),
            )
            .when(self.files.loaded && !self.visible.is_empty(), |d| {
                d.child(file_table_header(cx))
            })
            .into_any_element()
    }

    /// What the notice in place of the file rows says, and whether it is an
    /// error.
    pub(super) fn files_notice_text(&self) -> (String, bool) {
        if let Some(e) = &self.files.error {
            (format!("Could not load the files: {e}"), true)
        } else if self.files.loaded && !self.files.list.is_empty() {
            let n = self.files.list.len();
            (format!("All {n} files are hidden by filters"), false)
        } else if self.files.loaded {
            ("No files changed".to_owned(), false)
        } else {
            ("Loading files\u{2026}".to_owned(), false)
        }
    }

    fn files_notice(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (text, error) = self.files_notice_text();
        div()
            .py_1()
            .text_sm()
            .text_color(if error {
                theme.danger
            } else {
                theme.muted_foreground
            })
            .child(text)
            .into_any_element()
    }

    fn file(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(&fi) = self.visible.get(ix) else {
            return div().into_any_element();
        };
        let (Some(f), Some(style)) = (
            self.files.list.get(fi).cloned(),
            self.styles.get(fi).cloned(),
        ) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let selected = ix == self.cursor;
        let reviewed = self.files.reviewed.contains(&f.path);
        let max_change = self
            .visible
            .iter()
            .map(|&i| &self.files.list[i])
            .filter(|f| !f.is_special() && !f.binary)
            .map(|f| f.lines_inserted + f.lines_deleted)
            .max()
            .unwrap_or(0);
        let expanded = self.inline.contains_key(&f.path);
        let focused = self.focus_within_page_is_file_list();
        let (cursor_bg, cursor_bar) = if focused {
            (theme.list_active, theme.list_active_border)
        } else {
            (muted.opacity(0.10), muted)
        };
        let letter_color = match f.status {
            FileStatus::Added => theme.success,
            FileStatus::Deleted => theme.danger,
            FileStatus::Modified => muted,
            _ => theme.warning,
        };
        let letter_color = if style.dimmed {
            letter_color.opacity(DIMMED)
        } else {
            letter_color
        };
        let row = h_flex()
            .id(("file-row", ix))
            .gap_2()
            .px_1()
            .py_0p5()
            .items_center()
            .text_sm()
            .border_l_2()
            .border_color(if selected {
                cursor_bar
            } else {
                theme.transparent
            })
            .when(selected, |d| d.bg(cursor_bg))
            .hover(|d| d.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _, _, cx| this.open_file(ix, cx)))
            .child(
                div()
                    .id(("file-expand", ix))
                    .w(px(16.))
                    .flex_none()
                    .text_color(muted)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.cursor = ix;
                        this.toggle_inline_at(ix, cx);
                    }))
                    .child(if expanded { "\u{25BE}" } else { "\u{25B8}" }),
            )
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .text_color(letter_color)
                    .child(f.status.letter().to_owned()),
            )
            .child(file_path(&f, &style, cx))
            .child(super::thread_list::counts_cell(
                self.counts.get(&f.path).copied(),
                cx,
            ))
            .child(
                div()
                    .w(px(REVIEWED_W))
                    .flex_none()
                    .text_center()
                    .text_color(theme.success)
                    .child(if reviewed { "\u{2713}" } else { "" }),
            )
            .child(file_size(&f, max_change, cx));

        let inline = self.inline.get(&f.path).map(|i| {
            let body: AnyElement = match (&i.view, &i.error) {
                (Some(view), _) => div().w_full().child(view.clone()).into_any_element(),
                (None, Some(e)) => div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(format!("Could not load the diff: {e}"))
                    .into_any_element(),
                (None, None) => div()
                    .text_sm()
                    .text_color(muted)
                    .child(if i.phase == Phase::Queued {
                        "Waiting\u{2026}"
                    } else {
                        "Loading diff\u{2026}"
                    })
                    .into_any_element(),
            };
            div()
                .ml(px(20.))
                .mb_1()
                .border_1()
                .border_color(theme.border)
                .rounded_sm()
                .overflow_hidden()
                .child(body)
        });
        v_flex()
            .child(row)
            .children(inline.map(|i| i.mr_2()))
            .into_any_element()
    }

    fn focus_within_page_is_file_list(&self) -> bool {
        // The cursor is drawn in the active color unless focus is elsewhere
        // (an inline diff, the search box); a per-frame window query would
        // need the window here, so the page remembers it in render.
        self.has_focus
    }

    /// Whether there is a relation chain worth showing.
    pub(super) fn has_chain(&self) -> bool {
        self.related.as_ref().is_some_and(|r| r.changes.len() > 1)
    }

    /// The relation chain, `None` when there is nothing but this change.
    /// The box is capped in height and scrolls, so a long stack does not
    /// push the files down; the current change has an arrow and a tint. As
    /// the `panel` it fills the height of its column instead.
    pub(super) fn related_block(
        &mut self,
        panel: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let r = self.related.as_ref().filter(|r| r.changes.len() > 1)?;
        let own = self.detail.as_ref().map(|d| &d.summary);
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let rows = r.changes.iter().enumerate().map(|(i, c)| {
            let this_change = c.number == self.number;
            let number = c.number;
            h_flex()
                .id(("related", i))
                .gap_2()
                .px_1()
                .items_center()
                .text_sm()
                .when(this_change, |d| d.bg(theme.accent))
                .hover(|d| d.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if number != this.number {
                        cx.emit(super::ChangeEvent::OpenChange(number));
                    }
                }))
                .child(
                    div()
                        .w(px(12.))
                        .flex_none()
                        .text_color(theme.primary)
                        .child(if this_change { "\u{2192}" } else { "" }),
                )
                .child(
                    div()
                        .w(px(64.))
                        .flex_none()
                        .whitespace_nowrap()
                        .text_color(muted)
                        .child(format!("#{}", c.number)),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .when(this_change, |d| d.font_semibold())
                        .child(related_subject(c)),
                )
                .child(div().flex_1())
                .children(related_pills(
                    c,
                    // Until the chain's summaries arrive, this change's own
                    // row uses the detail the page already has.
                    r.summaries
                        .get(&c.number)
                        .or(own.filter(|s| s.number == c.number)),
                    cx,
                ))
        });
        let scroll = self.related_scroll.clone();
        let list = v_flex()
            .id("related-list")
            .overflow_y_scroll()
            .track_scroll(&self.related_scroll)
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .children(rows.collect::<Vec<_>>());
        let list = if panel {
            // The panel is as tall as the window, so a long chain scrolls in
            // it, with a scrollbar that shows only when it overflows.
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(list.size_full())
                .vertical_scrollbar(&self.related_scroll)
                .into_any_element()
        } else {
            list.max_h(px(220.))
                .on_scroll_wheel(move |ev, window, cx| {
                    let delta = ev.delta.pixel_delta(window.line_height()).y;
                    keep_wheel_while_scrollable(&scroll, delta, cx);
                })
                .into_any_element()
        };
        let toggle = div()
            .id("related-toggle")
            .text_xs()
            .text_color(theme.link)
            .cursor_pointer()
            .hover(|d| d.underline())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_related_panel(cx)))
            .child(if panel {
                "Collapse (Shift-L)"
            } else {
                "Show as panel (Shift-L)"
            });
        Some(
            v_flex()
                .gap_1()
                .when(panel, |d| d.size_full())
                .child(section_title("Relation chain", cx))
                .child(list)
                .child(toggle)
                .into_any_element(),
        )
    }

    fn log_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let n = self.detail.as_ref().map_or(0, |d| d.messages.len());
        card_title_bar(cx)
            .child(section_title(&format!("Change log ({n})"), cx))
            .into_any_element()
    }

    fn message(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(m) = self.detail.as_ref().and_then(|d| d.messages.get(ix)) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let auto = m.is_autogenerated();
        let who = m
            .author
            .as_ref()
            .map_or_else(|| "Gerrit".to_owned(), Account::display);
        // Folded, as in the Gerrit web UI: the message on one line. A click
        // on the entry folds it out; once out, the text itself does not
        // fold it back, so that its links stay clickable.
        let open = self.open_messages.contains(&m.id);
        let text = v_flex()
            .id(("change-message-text", ix))
            .flex_1()
            .min_w_0()
            .text_sm()
            .when(auto, |d| d.text_color(muted));
        let text = if open {
            text.children(message_lines(ix, m, theme.link))
        } else {
            text.cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_message(ix, cx)))
                .child(div().truncate().child(one_line(&m.message)))
        };
        h_flex()
            .gap_3()
            .items_start()
            .child(
                v_flex()
                    .id(("change-message-author", ix))
                    .w(px(150.))
                    .flex_none()
                    .items_start()
                    .gap_0p5()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_message(ix, cx)))
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .truncate()
                            .text_color(theme.foreground)
                            .child(who),
                    )
                    .when(auto, |d| d.child(pill("auto", muted))),
            )
            .child(text)
            .child(
                v_flex()
                    .id(("change-message-meta", ix))
                    .w(px(80.))
                    .flex_none()
                    .items_end()
                    .text_xs()
                    .text_color(muted)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_message(ix, cx)))
                    .when_some(m.revision, |d, n| d.child(format!("PS{n}")))
                    .child(clock(Timestamp::now(), m.date, &TimeZone::system())),
            )
            .into_any_element()
    }
}

/// The message folded to one line: its lines joined by spaces.
fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn message_lines(ix: usize, m: &ChangeMessage, link: Hsla) -> Vec<AnyElement> {
    lines_of(("change-message", ix), &m.message, true, link)
}

pub(super) fn section_title(text: &str, cx: &App) -> Div {
    div()
        .text_sm()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_owned())
}

/// Opacity of the status letter and the path of a dimmed file.
const DIMMED: f32 = 0.55;

fn file_path(f: &FileEntry, style: &FileStyle, cx: &App) -> Div {
    let theme = cx.theme();
    let name = match f.path.as_str() {
        COMMIT_MSG_PATH => "Commit message".to_owned(),
        MERGE_LIST_PATH => "Merge list".to_owned(),
        p => p.to_owned(),
    };
    let text = match (&f.old_path, f.status.clone()) {
        (Some(old), FileStatus::Renamed | FileStatus::Copied) => {
            format!("{old} \u{2192} {name}")
        }
        _ => name,
    };
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .text_color(if f.is_special() {
            theme.muted_foreground
        } else if style.dimmed {
            theme.muted_foreground.opacity(DIMMED)
        } else {
            style.color.map_or(theme.foreground, Hsla::from)
        })
        .when(style.bold, |d| d.font_weight(FontWeight::BOLD))
        .child(text)
}

const REVIEWED_W: f32 = 28.;
const SIZE_W: f32 = 170.;
const BAR_W: f32 = 56.;

fn file_table_header(cx: &App) -> Div {
    let theme = cx.theme();
    let cell = |text: &'static str| div().flex_none().child(text);
    h_flex()
        .px_3()
        .py_1()
        .gap_2()
        .items_center()
        .text_xs()
        .font_semibold()
        .text_color(theme.muted_foreground)
        // Same left offsets as a file row: border, arrow, status letter.
        .child(div().w(px(2. + 16. + 14. + 16.)).flex_none())
        .child(div().flex_1().child("File"))
        .child(
            cell("Comments")
                .w(px(super::thread_list::COUNTS_W))
                .text_right(),
        )
        .child(cell("").w(px(REVIEWED_W)))
        .child(cell("Size").w(px(SIZE_W)).text_right())
}

/// Lines added / removed and a bar of their proportions. The bar's length is
/// log-scaled against the largest file, so small changes stay visible next
/// to a huge one.
fn file_size(f: &FileEntry, max_change: u32, cx: &App) -> Div {
    let theme = cx.theme();
    let cell = h_flex()
        .flex_none()
        .w(px(SIZE_W))
        .gap_2()
        .justify_end()
        .items_center()
        .text_xs();
    if f.binary {
        return cell
            .text_color(theme.muted_foreground)
            .child(format!("binary, {}", size_text(f.size)));
    }
    let total = f.lines_inserted + f.lines_deleted;
    let bar = if f.is_special() || total == 0 || max_change == 0 {
        div().w(px(BAR_W)).flex_none()
    } else {
        let scale = (f64::from(total) + 1.).ln() / (f64::from(max_change) + 1.).ln();
        let len = (f64::from(BAR_W) * scale).clamp(4., f64::from(BAR_W)) as f32;
        let ins = len * f.lines_inserted as f32 / total as f32;
        h_flex()
            .w(px(BAR_W))
            .flex_none()
            .justify_end()
            .child(div().h(px(8.)).w(px(ins)).bg(theme.success))
            .child(div().h(px(8.)).w(px(len - ins)).bg(theme.danger))
    };
    cell.child(bar)
        .child(
            div()
                .w(px(34.))
                .text_right()
                .text_color(theme.success)
                .child(format!("+{}", f.lines_inserted)),
        )
        .child(
            div()
                .w(px(34.))
                .text_right()
                .text_color(theme.danger)
                .child(format!("\u{2212}{}", f.lines_deleted)),
        )
}

/// Called after the scroll box applied a wheel event of `delta` (vertical,
/// in pixels) to its offset. Stops the event from also scrolling the page
/// behind the box, unless the box did not overflow or already sat at the
/// end the wheel pushed against: then the page gets it, as in a browser.
fn keep_wheel_while_scrollable(scroll: &ScrollHandle, delta: Pixels, cx: &mut App) {
    // GPUI clamps the offset only when it paints the next frame, so what
    // the box applied is undone here to see where it stood before.
    let before = scroll.offset().y - delta;
    if wheel_stays_in_box(before, delta, scroll.max_offset().y) {
        cx.stop_propagation();
    }
}

/// `offset` is where the box stood before the wheel moved the content by
/// `delta` (at most 0, down to `-max`).
fn wheel_stays_in_box(offset: Pixels, delta: Pixels, max: Pixels) -> bool {
    let at_start = offset >= px(0.);
    let at_end = offset <= -max;
    max > px(0.) && delta != px(0.) && !(delta > px(0.) && at_start) && !(delta < px(0.) && at_end)
}

fn related_subject(c: &RelatedChange) -> String {
    c.commit
        .as_ref()
        .map_or_else(|| "(no subject)".to_owned(), |k| k.subject.clone())
}

/// Negative votes that hold a change back: `Rejected` for Code-Review at its
/// lowest, otherwise the label and vote (`Verified -1`).
pub(super) fn blocking_votes(summary: Option<&ChangeSummary>) -> Vec<String> {
    let Some(s) = summary else {
        return Vec::new();
    };
    s.labels
        .iter()
        .filter_map(|(name, l)| match crate::labels::mark(l) {
            crate::labels::Mark::Rejected if name == "Code-Review" => Some("Rejected".to_owned()),
            crate::labels::Mark::Rejected => Some(format!("{name} {}", l.min.unwrap_or(-1))),
            crate::labels::Mark::Disliked(v) => Some(format!("{name} {v}")),
            _ => None,
        })
        .collect()
}

fn related_pills(c: &RelatedChange, summary: Option<&ChangeSummary>, cx: &App) -> Vec<Div> {
    let t = cx.theme();
    let mut v = Vec::new();
    match &c.status {
        Some(ChangeStatus::Merged) => v.push(pill("Merged", t.success)),
        Some(ChangeStatus::Abandoned) => v.push(pill("Abandoned", t.muted_foreground)),
        Some(ChangeStatus::New) => {
            if summary.is_some_and(has_merge_conflict) {
                v.push(conflict_pill(cx));
            }
            for text in blocking_votes(summary) {
                v.push(pill(text, t.danger));
            }
            // The related-changes endpoint carries no `submittable`; the
            // change's own summary does.
            if c.submittable == Some(true) || summary.is_some_and(|s| s.submittable == Some(true)) {
                // Solid primary box with its own text colour, to stand out from the
                // tinted ones.
                v.push(
                    pill("Submittable", t.primary)
                        .bg(t.primary)
                        .text_color(t.primary_foreground)
                        .font_bold(),
                );
            }
        }
        _ => {}
    }
    if c.work_in_progress {
        v.push(pill("WIP", t.warning));
    }
    if c.is_outdated() {
        v.push(pill(
            format!(
                "outdated (PS{} of {})",
                c.patch_set,
                c.current_patch_set.map_or(0, |n| n.0)
            ),
            t.warning,
        ));
    }
    v
}

impl Render for ChangePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.has_focus = self.focus.is_focused(window);
        let shown = self.layout.read(cx).related_panel()
            && window.viewport_size().width >= px(RELATED_PANEL_MIN_WINDOW)
            && self.has_chain();
        if shown != self.panel_shown {
            // The header changes shape: its bar moves in or out of the list.
            self.panel_shown = shown;
            self.remeasure(Block::Header);
            self.reveal_current_related();
        }
        let page = div()
            .relative()
            .size_full()
            .flex_1()
            .min_w_0()
            .child(
                list(
                    self.list.clone(),
                    cx.processor(|this, ix, window, cx| this.render_block(ix, window, cx)),
                )
                .size_full(),
            )
            .vertical_scrollbar(&self.list);
        let panel = shown.then(|| self.related_block(true, cx)).flatten();
        let bar = shown.then(|| self.header_bar(cx)).flatten();
        let theme = cx.theme().clone();
        v_flex()
            .id("change-page")
            .key_context(CHANGE_PAGE)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::refresh_now))
            .on_action(cx.listener(Self::quick_vote_action))
            .on_action(cx.listener(Self::toggle_filter))
            .on_action(cx.listener(Self::manage_filters))
            .on_action(cx.listener(Self::open_in_browser))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::prev_file))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::toggle_inline_action))
            .on_action(cx.listener(Self::toggle_all_inline))
            .on_action(cx.listener(Self::next_target))
            .on_action(cx.listener(Self::prev_target))
            .on_action(cx.listener(Self::next_base))
            .on_action(cx.listener(Self::prev_base))
            .on_action(cx.listener(Self::toggle_commit))
            .on_action(cx.listener(Self::next_related))
            .on_action(cx.listener(Self::prev_related))
            .on_action(cx.listener(Self::toggle_related_panel_action))
            .on_action(cx.listener(Self::next_thread))
            .on_action(cx.listener(Self::prev_thread))
            .on_action(cx.listener(Self::open_thread_action))
            .on_action(cx.listener(Self::threads_unresolved))
            .on_action(cx.listener(Self::threads_drafts))
            .on_action(cx.listener(Self::threads_all))
            .on_action(cx.listener(Self::new_patchset_comment_action))
            .on_action(cx.listener(Self::toggle_reviewed))
            .on_action(cx.listener(Self::reply))
            .on_action(cx.listener(Self::open_actions))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::page_up))
            .size_full()
            .children(self.server_badge.as_ref().map(|b| b.accent_line(cx)))
            .children(bar.map(|b| div().w_full().px_4().pt_1().pb_2().child(b)))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(page)
                    .children(panel.map(|p| {
                        div()
                            .w(px(RELATED_PANEL_WIDTH))
                            .flex_none()
                            .h_full()
                            .pl_2()
                            .pr_4()
                            .pb_2()
                            .border_l_1()
                            .border_color(theme.border)
                            .child(p)
                    })),
            )
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::wheel_stays_in_box;
    use gpui_kit::px;

    #[test]
    fn the_wheel_stays_in_the_box_until_it_pushes_against_an_end() {
        let stays =
            |offset: f32, delta: f32, max: f32| wheel_stays_in_box(px(offset), px(delta), px(max));
        // Mid-way: both directions stay.
        assert!(stays(-20., -10., 100.));
        assert!(stays(-20., 10., 100.));
        // At the top, down stays and up goes on to the page.
        assert!(stays(0., -10., 100.));
        assert!(!stays(0., 10., 100.));
        // At the bottom, the other way round.
        assert!(stays(-100., 10., 100.));
        assert!(!stays(-100., -10., 100.));
        // A box that does not overflow never keeps the wheel.
        assert!(!stays(0., -10., 0.));
    }
}
