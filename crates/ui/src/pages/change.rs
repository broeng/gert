//! Placeholder change page: change metadata only (M1). Diffs and comments
//! come in M2/M3.

use std::time::Duration;

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{Account, ChangeDetail, ChangeNumber};
use sync::PollPolicy;

use crate::{
    actions::*,
    labels::{Mark, mark},
    pages::list::Status,
    row::{browser_url, chips},
    session::{Session, is_unauthorized},
    theme::{chip_colors, mark_color},
    time_fmt::relative,
    tracker::Tracker,
};

pub enum ChangeEvent {
    Unauthorized,
}

pub struct ChangePage {
    session: Session,
    number: ChangeNumber,
    detail: Option<ChangeDetail>,
    fetched_at: Option<Timestamp>,
    loaded: bool,
    status: Status,
    track: Tracker,
    focus: FocusHandle,
    window_active: bool,
    policy: PollPolicy,
    ticks: u32,
    task: Option<Task<()>>,
    _ticker: Task<()>,
}

impl EventEmitter<ChangeEvent> for ChangePage {}

impl Focusable for ChangePage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ChangePage {
    pub fn new(
        session: Session,
        number: ChangeNumber,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            session,
            number,
            detail: None,
            fetched_at: None,
            loaded: false,
            status: Status::Idle,
            track: Tracker::default(),
            focus: cx.focus_handle(),
            window_active,
            policy: PollPolicy::default(),
            ticks: 0,
            task: None,
            _ticker: Task::ready(()),
        };
        let syncer = this.session.syncer().clone();
        this.task = Some(cx.spawn(async move |this, cx| {
            let cached = cx
                .background_spawn(async move { syncer.cached_change(number) })
                .await;
            this.update(cx, |this, cx| {
                this.loaded = true;
                match cached {
                    Ok(Some(c)) => {
                        this.detail = Some(c.detail);
                        this.fetched_at = Some(c.fetched_at);
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("reading cached change {number}: {e}"),
                }
                this.tick(cx);
                cx.notify();
            })
            .ok();
        }));
        this._ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        });
        this
    }

    #[cfg(test)]
    pub fn detail(&self) -> Option<&ChangeDetail> {
        self.detail.as_ref()
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(10) {
            cx.notify();
        }
        if !self.session.can_sync() || !self.loaded || self.status.busy() {
            return;
        }
        let now = Timestamp::now();
        if self
            .policy
            .is_due(now, &self.track.state(self.window_active, true))
        {
            self.refresh(cx);
        }
    }

    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.window_active != active {
            self.window_active = active;
            self.tick(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let number = self.number;
        self.status = Status::Refreshing;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { syncer.refresh_change(number).await })
                .await;
            this.update(cx, |this, cx| {
                let now = Timestamp::now();
                match result {
                    Ok(detail) => {
                        this.track.success(now);
                        this.detail = Some(detail);
                        this.fetched_at = Some(now);
                        this.status = Status::Idle;
                    }
                    Err(e) if is_unauthorized(&e) => {
                        this.status = Status::Failed("unauthorized".into());
                        cx.emit(ChangeEvent::Unauthorized);
                    }
                    Err(e) => {
                        log::warn!("refreshing change {}: {e}", this.number);
                        this.track.failure(now);
                        this.status = Status::Failed(e.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn refresh_now(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.track.invalidate();
        self.tick(cx);
    }

    fn open_in_browser(&mut self, _: &OpenInBrowser, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = &self.detail {
            cx.open_url(&browser_url(
                &self.session.base_url,
                &d.summary.project,
                self.number,
            ));
        }
    }
}

fn people(accounts: &[Account]) -> String {
    if accounts.is_empty() {
        return "none".into();
    }
    accounts
        .iter()
        .map(Account::display)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Render for ChangePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let now = Timestamp::now();
        let tz = TimeZone::system();
        let muted = theme.muted_foreground;

        let status: AnyElement = match &self.status {
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
                    .child(format!("updated {}", relative(now, t, &tz)))
                    .into_any_element(),
                None => div().into_any_element(),
            },
        };

        let root = v_flex()
            .id("change-page")
            .key_context(CHANGE_PAGE)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::refresh_now))
            .on_action(cx.listener(Self::open_in_browser))
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3();

        let Some(d) = &self.detail else {
            let text = if self.loaded && matches!(self.status, Status::Failed(_)) {
                "Could not load the change."
            } else {
                "Loading\u{2026}"
            };
            return root
                .child(div().text_lg().child(format!("Change {}", self.number)))
                .child(div().text_color(muted).child(text))
                .child(status);
        };
        let c = &d.summary;
        let field = |name: &'static str, value: AnyElement| {
            h_flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .w(px(110.))
                        .flex_none()
                        .text_sm()
                        .text_color(muted)
                        .child(name),
                )
                .child(div().flex_1().min_w_0().text_sm().child(value))
        };
        let text = |s: String| div().child(s).into_any_element();

        let labels = h_flex()
            .flex_wrap()
            .gap_3()
            .children(c.labels.iter().map(|(name, l)| {
                let m = mark(l);
                h_flex().gap_1().child(name.clone()).child(
                    div()
                        .text_color(mark_color(&m, cx))
                        .child(if m == Mark::None {
                            "\u{2013}".to_owned()
                        } else {
                            m.text()
                        }),
                )
            }))
            .into_any_element();

        let sha = d.current_revision.as_ref().map(|s| s.short(12).to_owned());
        let patch_set = match (d.current_patch_set, sha) {
            (Some(n), Some(sha)) => format!("{n}  ({sha})"),
            (Some(n), None) => n.to_string(),
            (None, Some(sha)) => sha,
            (None, None) => "unknown".into(),
        };

        root.child(
            h_flex()
                .gap_2()
                .child(div().text_color(muted).child(format!("#{}", c.number)))
                .children(chips(c).into_iter().map(|chip| {
                    let (fg, bg) = chip_colors(chip, cx);
                    div()
                        .px_1()
                        .rounded_sm()
                        .text_xs()
                        .text_color(fg)
                        .bg(bg)
                        .child(chip.text())
                }))
                .child(div().text_lg().font_semibold().child(c.subject.clone()))
                .child(div().flex_1())
                .child(status),
        )
        .child(field("Status", text(c.status.to_string())))
        .child(field("Owner", text(c.owner.display())))
        .child(field(
            "Project",
            text(format!("{}  ({})", c.project, c.branch)),
        ))
        .child(field(
            "Topic",
            text(c.topic.clone().unwrap_or_else(|| "none".into())),
        ))
        .child(field(
            "Hashtags",
            text(if c.hashtags.is_empty() {
                "none".into()
            } else {
                c.hashtags.join(", ")
            }),
        ))
        .child(field("Updated", text(relative(now, c.updated, &tz))))
        .child(field("Created", text(relative(now, c.created, &tz))))
        .child(field("Patch set", text(patch_set)))
        .child(field(
            "Size",
            text(format!("+{} \u{2212}{}", c.insertions, c.deletions)),
        ))
        .child(field("Labels", labels))
        .child(field("Reviewers", text(people(&d.reviewers))))
        .child(field("CC", text(people(&d.cc))))
        .child(
            v_flex()
                .gap_1()
                .child(div().text_sm().text_color(muted).child("Commit message"))
                .child(
                    v_flex()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .font_family("monospace")
                        .text_sm()
                        .children(
                            d.commit_message
                                .as_deref()
                                .unwrap_or("(not available)")
                                .lines()
                                .map(|l| {
                                    div().child(if l.is_empty() {
                                        " ".to_owned()
                                    } else {
                                        l.to_owned()
                                    })
                                })
                                .collect::<Vec<_>>(),
                        ),
                ),
        )
        .child(
            h_flex().gap_2().child(
                Button::new("browser")
                    .label("Open in browser (Shift-O)")
                    .small()
                    .on_click(
                        cx.listener(|this, _, w, cx| this.open_in_browser(&OpenInBrowser, w, cx)),
                    ),
            ),
        )
    }
}
