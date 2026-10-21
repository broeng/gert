//! Rendering of the change page blocks.

use gpui_kit::component::button::Button;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{
    Account, COMMIT_MSG_PATH, ChangeDetail, ChangeMessage, ChangeStatus, FileEntry, FileStatus,
    MERGE_LIST_PATH, RelatedChange, SubmitRequirement, SubmitRequirementStatus,
};

use super::{
    ChangePage, Phase,
    state::{Block, size_text},
};
use crate::{actions::*, pages::list::Status, row::chips, theme::chip_colors, time_fmt::relative};

const LABEL_ORDER: [&str; 2] = ["Code-Review", "Verified"];

fn pill(text: impl Into<SharedString>, fg: Hsla) -> Div {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .text_xs()
        .text_color(fg)
        .bg(fg.opacity(0.15))
        .child(text.into())
}

fn lines_of(text: &str, empty_pad: bool) -> Vec<Div> {
    text.trim_end()
        .lines()
        .map(|l| {
            div().child(if l.is_empty() && empty_pad {
                " ".to_owned()
            } else {
                l.to_owned()
            })
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

/// Votes are shown signed: `+2`, `-1`; `0` is not a vote.
fn vote_text(v: i32) -> String {
    if v > 0 {
        format!("+{v}")
    } else {
        v.to_string()
    }
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
            Block::Requirements => self.requirements(cx),
            Block::Votes => self.votes(cx),
            Block::People => self.people_block(cx),
            Block::Commit => self.commit(cx),
            Block::PatchSets => self.patch_sets(cx),
            Block::FilesNotice => self.files_notice(cx),
            Block::File(i) => self.file(i, cx),
            Block::Related => self.related_block(cx),
            Block::LogHeader => self.log_header(cx),
            Block::Message(i) => self.message(i, cx),
        };
        div().w_full().px_4().py_1().child(inner).into_any_element()
    }

    fn status_line(&self, cx: &mut Context<Self>) -> AnyElement {
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
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let status = self.status_line(cx);
        let Some(d) = &self.detail else {
            let text = if self.loaded && matches!(self.status, Status::Failed(_)) {
                "Could not load the change."
            } else {
                "Loading\u{2026}"
            };
            return v_flex()
                .gap_1()
                .child(div().text_lg().child(format!("Change {}", self.number)))
                .child(div().text_color(muted).child(text))
                .child(status)
                .into_any_element();
        };
        let c = &d.summary;
        let now = Timestamp::now();
        let tz = TimeZone::system();
        let (status_text, unknown) = status_label(&c.status);
        let status_color = match c.status {
            ChangeStatus::Merged => theme.success,
            ChangeStatus::Abandoned => theme.muted_foreground,
            _ => theme.primary,
        };
        let field = |name: &'static str, value: String| {
            h_flex()
                .gap_1()
                .child(div().text_color(muted).child(name))
                .child(div().child(value))
        };
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(div().text_color(muted).child(format!("#{}", c.number)))
                    .child(if unknown {
                        pill(c.status.to_string(), muted)
                    } else {
                        pill(status_text, status_color)
                    })
                    .children(
                        chips(c)
                            .into_iter()
                            .filter(|chip| {
                                !matches!(
                                    chip,
                                    crate::row::Chip::Merged | crate::row::Chip::Abandoned
                                )
                            })
                            .map(|chip| pill(chip.text(), chip_colors(chip, cx).0)),
                    )
                    .child(div().text_lg().font_semibold().child(c.subject.clone()))
                    .child(div().flex_1())
                    .child(status),
            )
            .child(
                h_flex()
                    .gap_x_4()
                    .gap_y_0p5()
                    .flex_wrap()
                    .text_sm()
                    .child(field("Owner", c.owner.display()))
                    .child(field("Project", format!("{} ({})", c.project, c.branch)))
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
                    .child(field("Created", relative(now, c.created, &tz))),
            )
            .into_any_element()
    }

    fn requirements(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let rows = d.submit_requirements.iter().map(|r| {
            let (glyph, color) = requirement_mark(r, cx);
            let note = requirement_note(r);
            h_flex()
                .gap_2()
                .items_baseline()
                .text_sm()
                .child(div().w(px(14.)).flex_none().text_color(color).child(glyph))
                .child(div().child(r.name.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(note),
                )
        });
        v_flex()
            .gap_0p5()
            .child(section_title("Submit requirements", cx))
            .children(rows.collect::<Vec<_>>())
            .into_any_element()
    }

    fn votes(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let mut names: Vec<&String> = d.votes.keys().collect();
        names.sort_by_key(|n| {
            (
                LABEL_ORDER
                    .iter()
                    .position(|l| l == n)
                    .unwrap_or(LABEL_ORDER.len()),
                (*n).clone(),
            )
        });
        let rows = names.into_iter().map(|name| {
            let votes = &d.votes[name];
            let voters: Vec<AnyElement> = votes
                .iter()
                .filter(|v| v.value != 0)
                .map(|v| {
                    let color = if v.value > 0 {
                        theme.success
                    } else {
                        theme.danger
                    };
                    h_flex()
                        .gap_1()
                        .child(div().text_color(color).child(vote_text(v.value)))
                        .child(div().child(v.account.display()))
                        .into_any_element()
                })
                .collect();
            h_flex()
                .gap_3()
                .text_sm()
                .items_baseline()
                .child(
                    div()
                        .w(px(110.))
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(name.clone()),
                )
                .child(if voters.is_empty() {
                    div()
                        .text_color(theme.muted_foreground)
                        .child("no votes")
                        .into_any_element()
                } else {
                    h_flex()
                        .gap_3()
                        .flex_wrap()
                        .children(voters)
                        .into_any_element()
                })
        });
        v_flex()
            .gap_0p5()
            .child(section_title("Votes", cx))
            .children(rows.collect::<Vec<_>>())
            .into_any_element()
    }

    fn people_block(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.detail else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let row = |name: &'static str, list: &[Account]| {
            h_flex()
                .gap_3()
                .text_sm()
                .items_baseline()
                .child(div().w(px(110.)).flex_none().text_color(muted).child(name))
                .child(div().min_w_0().child(if list.is_empty() {
                    "none".to_owned()
                } else {
                    people(list)
                }))
        };
        v_flex()
            .gap_0p5()
            .child(row("Reviewers", &d.reviewers))
            .child(row("CC", &d.cc))
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
                        .font_family(theme.mono_font_family.clone())
                        .text_sm()
                        .children(lines_of(
                            message.as_deref().unwrap_or("(not available)"),
                            true,
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
            .gap_1()
            .pt_2()
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
                            .on_click(
                                cx.listener(|this, _, w, cx| this.next_base(&NextBase, w, cx)),
                            ),
                    )
                    .child(div().text_sm().text_color(muted).child("with"))
                    .child(
                        Button::new("target-ps")
                            .label(format!("{target_text}  (t)"))
                            .small()
                            .outline()
                            .on_click(
                                cx.listener(|this, _, w, cx| this.next_target(&NextTarget, w, cx)),
                            ),
                    )
                    .child(div().flex_1())
                    .when(self.files.loaded, |d| {
                        d.child(
                            h_flex()
                                .gap_2()
                                .text_sm()
                                .text_color(muted)
                                .child(format!("{count} files"))
                                .child(div().text_color(theme.success).child(format!("+{ins}")))
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
            })
            .into_any_element()
    }

    fn files_notice(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (text, color) = if let Some(e) = &self.files.error {
            (format!("Could not load the files: {e}"), theme.danger)
        } else if self.files.loaded {
            ("No files changed".to_owned(), theme.muted_foreground)
        } else {
            ("Loading files\u{2026}".to_owned(), theme.muted_foreground)
        };
        div()
            .py_1()
            .text_sm()
            .text_color(color)
            .child(text)
            .into_any_element()
    }

    fn file(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(f) = self.files.list.get(ix).cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let selected = ix == self.cursor;
        let reviewed = self.files.reviewed.contains(&f.path);
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
            .child(file_path(&f, cx))
            .child(div().flex_1())
            .when(reviewed, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child("\u{2713} reviewed"),
                )
            })
            .child(file_size(&f, cx));

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
        v_flex().child(row).children(inline).into_any_element()
    }

    fn focus_within_page_is_file_list(&self) -> bool {
        // The cursor is drawn in the active color unless focus is elsewhere
        // (an inline diff, the search box); a per-frame window query would
        // need the window here, so the page remembers it in render.
        self.has_focus
    }

    fn related_block(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(r) = &self.related else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let rows = r.changes.iter().enumerate().map(|(i, c)| {
            let this_change = c.number == self.number;
            let selected = self.related_cursor == Some(i);
            let number = c.number;
            h_flex()
                .id(("related", i))
                .gap_2()
                .px_1()
                .items_center()
                .text_sm()
                .border_l_2()
                .border_color(if selected {
                    theme.list_active_border
                } else {
                    theme.transparent
                })
                .when(selected, |d| d.bg(theme.list_active))
                .hover(|d| d.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if number != this.number {
                        cx.emit(super::ChangeEvent::OpenChange(number));
                    }
                }))
                .child(
                    div()
                        .w(px(72.))
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
                .children(related_pills(c, this_change, cx))
        });
        v_flex()
            .gap_0p5()
            .pt_2()
            .child(section_title("Related changes", cx))
            .children(rows.collect::<Vec<_>>())
            .into_any_element()
    }

    fn log_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let n = self.detail.as_ref().map_or(0, |d| d.messages.len());
        div()
            .pt_2()
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
        v_flex()
            .gap_0p5()
            .py_1()
            .border_t_1()
            .border_color(theme.border.opacity(0.6))
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .text_xs()
                    .text_color(muted)
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child(who),
                    )
                    .when_some(m.revision, |d, n| d.child(format!("PS{n}")))
                    .child(relative(Timestamp::now(), m.date, &TimeZone::system()))
                    .when(auto, |d| d.child(pill("auto", muted))),
            )
            .child(
                v_flex()
                    .text_sm()
                    .when(auto, |d| d.text_color(muted))
                    .children(message_lines(m)),
            )
            .into_any_element()
    }
}

fn message_lines(m: &ChangeMessage) -> Vec<Div> {
    lines_of(&m.message, true)
}

fn section_title(text: &str, cx: &App) -> Div {
    div()
        .text_sm()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_owned())
}

fn requirement_mark(r: &SubmitRequirement, cx: &App) -> (&'static str, Hsla) {
    let t = cx.theme();
    match &r.status {
        SubmitRequirementStatus::Satisfied => ("\u{2713}", t.success),
        SubmitRequirementStatus::Overridden | SubmitRequirementStatus::Forced => {
            ("\u{2713}", t.warning)
        }
        SubmitRequirementStatus::NotApplicable => ("\u{2013}", t.muted_foreground),
        SubmitRequirementStatus::Unsatisfied => ("\u{2717}", t.danger),
        SubmitRequirementStatus::Error | SubmitRequirementStatus::Timeout => ("!", t.danger),
        SubmitRequirementStatus::Unknown(_) => ("?", t.muted_foreground),
    }
}

/// The small print after a requirement: its state, and for a blocking one
/// what is missing.
fn requirement_note(r: &SubmitRequirement) -> String {
    let state = match &r.status {
        SubmitRequirementStatus::Satisfied => "satisfied".to_owned(),
        SubmitRequirementStatus::Unsatisfied => "unsatisfied".to_owned(),
        SubmitRequirementStatus::Overridden => "overridden".to_owned(),
        SubmitRequirementStatus::NotApplicable => "not applicable".to_owned(),
        SubmitRequirementStatus::Error => "error".to_owned(),
        SubmitRequirementStatus::Forced => "forced".to_owned(),
        SubmitRequirementStatus::Timeout => "timed out".to_owned(),
        SubmitRequirementStatus::Unknown(s) => s.to_lowercase(),
    };
    let missing = match (&r.status, &r.submittability) {
        (SubmitRequirementStatus::Unsatisfied, Some(e)) if !e.failing_atoms.is_empty() => {
            format!(" \u{2013} needs {}", e.failing_atoms.join(", "))
        }
        _ => String::new(),
    };
    format!("{state}{missing}")
}

fn file_path(f: &FileEntry, cx: &App) -> Div {
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
        .min_w_0()
        .truncate()
        .text_color(if f.is_special() {
            theme.muted_foreground
        } else {
            theme.foreground
        })
        .child(text)
}

fn file_size(f: &FileEntry, cx: &App) -> Div {
    let theme = cx.theme();
    if f.binary {
        return div()
            .flex_none()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(format!("binary, {}", size_text(f.size)));
    }
    h_flex()
        .flex_none()
        .w(px(90.))
        .gap_2()
        .justify_end()
        .text_xs()
        .child(
            div()
                .text_color(theme.success)
                .child(format!("+{}", f.lines_inserted)),
        )
        .child(
            div()
                .text_color(theme.danger)
                .child(format!("\u{2212}{}", f.lines_deleted)),
        )
}

fn related_subject(c: &RelatedChange) -> String {
    c.commit
        .as_ref()
        .map_or_else(|| "(no subject)".to_owned(), |k| k.subject.clone())
}

fn related_pills(c: &RelatedChange, this_change: bool, cx: &App) -> Vec<Div> {
    let t = cx.theme();
    let mut v = Vec::new();
    if this_change {
        v.push(pill("this change", t.primary));
    }
    match &c.status {
        Some(ChangeStatus::Merged) => v.push(pill("Merged", t.success)),
        Some(ChangeStatus::Abandoned) => v.push(pill("Abandoned", t.muted_foreground)),
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
        v_flex()
            .id("change-page")
            .key_context(CHANGE_PAGE)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::refresh_now))
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
            .on_action(cx.listener(Self::open_related))
            .on_action(cx.listener(Self::focus_files))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::page_up))
            .relative()
            .size_full()
            .child(
                list(
                    self.list.clone(),
                    cx.processor(|this, ix, window, cx| this.render_block(ix, window, cx)),
                )
                .size_full(),
            )
            .vertical_scrollbar(&self.list)
    }
}
