use std::sync::atomic::Ordering;

use gpui::{ClickEvent, Context, Div, FontWeight, SharedString, div, prelude::*, px};

use super::{Explorer, human_size};
use crate::{
    app::{commands, jobs::Job, jobs::Notice},
    icons::Icon,
    theme::Theme,
};

impl Explorer {
    fn render_status(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let s = self.lang.t();
        let base = div().flex().items_center().gap_1p5().min_w_0().overflow_hidden();
        if let Some(&(_, job, n)) = self.jobs.busy.last() {
            let cancelling = self.jobs.progress.is_cancelled();
            let mut text = match job {
                _ if cancelling => s.cancelling.to_string(),
                Job::Op(op) => self.lang.busy(op, n),
                Job::Undo(_) => s.undoing.to_string(),
                Job::Download => format!("{} · {n} {}", s.downloading, s.items),
                Job::NewFolder => format!("{}…", s.new_folder),
                Job::Rename => format!("{}…", s.rename),
            };
            let copied = self.jobs.progress.bytes.load(Ordering::Relaxed);
            if copied > 0 {
                text.push_str(&format!(" · {}", human_size(copied)));
            }
            let cancel = commands::find("job.cancel");
            return base
                .text_color(t.accent)
                .child(Icon::Refresh.el(12.).text_color(t.accent))
                .child(div().truncate().child(SharedString::from(text)))
                .when((cancel.when)(self), |d| {
                    d.child(
                        div()
                            .id("cancel-job")
                            .flex_none()
                            .px_1p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(t.muted)
                            .bg(t.control)
                            .hover(|d| d.bg(t.hover).text_color(t.text))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| commands::run(this, cancel, cx)))
                            .child(s.cancel),
                    )
                });
        }
        match &self.jobs.notice {
            Some(Notice::Done(op, n)) => base
                .text_color(t.muted)
                .child(Icon::Check.el(12.).text_color(t.image))
                .child(SharedString::from(self.lang.done(*op, *n))),
            Some(Notice::Undone(k)) => base
                .text_color(t.muted)
                .child(Icon::Undo.el(12.).text_color(t.accent))
                .child(SharedString::from(self.lang.undone(*k))),
            Some(Notice::Downloaded(n)) => base
                .text_color(t.muted)
                .child(Icon::Check.el(12.).text_color(t.image))
                .child(format!("{n} {}", s.downloaded)),
            Some(Notice::Info(msg)) => base.text_color(t.muted).child(*msg),
            Some(Notice::Error(msg)) => base
                .text_color(t.error)
                .child(Icon::Alert.el(12.).text_color(t.error))
                .child(div().truncate().child(SharedString::from(msg.clone()))),
            None => base.child(SharedString::from(match (self.loading, self.status_count) {
                (true, _) => s.loading.to_string(),
                (false, Some((n, ms))) if self.selection.marked_len() > 1 => {
                    format!("{n} {} · {} {} · {ms:.1} ms", s.items, self.selection.marked_len(), s.selected)
                }
                (false, Some((n, ms))) => format!("{n} {} · {ms:.1} ms", s.items),
                (false, None) => String::new(),
            })),
        }
    }

    pub(super) fn render_footer(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let s = self.lang.t();
        let lang = commands::find("view.lang");
        div()
            .flex_none()
            .h(px(34.))
            .px_4()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .text_xs()
            .text_color(t.faint)
            .child(self.render_status(t, cx))
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when(self.caps().trash, |d| d.child(div().min_w_0().truncate().child(s.hints)))
                    .child(
                        div()
                            .id("lang")
                            .flex_none()
                            .px_1p5()
                            .flex()
                            .items_center()
                            .gap_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.muted)
                            .bg(t.control)
                            .hover(|d| d.bg(t.hover))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| commands::run(this, lang, cx)))
                            .child(Icon::Globe.el(11.).text_color(t.muted))
                            .child(self.lang.code()),
                    ),
            )
    }
}
