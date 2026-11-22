use std::{
    ops::Range,
    sync::{Arc, OnceLock},
};

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontFeatures, FontWeight, MouseButton, MouseDownEvent, SharedString, Stateful,
    Window, div, prelude::*, px, uniform_list,
};

use super::{COL_DATE_W, COL_SIZE_W, Explorer, ROW_H, age, file_tile, human_size, placeholder};
use crate::{
    app::Modal,
    fs::{Entry, Kind},
    ops::Op,
    theme::{Theme, alpha},
};

impl Explorer {
    pub(super) fn render_list(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let s = self.lang.t();
        if let Some(err) = &self.error {
            placeholder(format!("{}: {err}", s.cannot_open), t.error)
        } else if self.loading && self.entries.is_empty() {
            placeholder(s.loading.into(), t.faint)
        } else if self.entries.is_empty() {
            placeholder(s.empty.into(), t.faint)
        } else {
            uniform_list(
                "entries",
                self.entries.len(),
                cx.processor(|this, range, window, cx| this.render_rows(range, window, cx)),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .px_2()
            .pb_2()
            .into_any_element()
        }
    }

    fn render_name(&self, e: &Entry, is_cut: bool, t: &Theme) -> AnyElement {
        if let Modal::Rename(r) = &self.modal
            && r.original == e.name
        {
            let (pre, sel, post) = r.input.parts();
            let caret = div().flex_none().w(px(1.5)).h(px(16.)).bg(t.accent);
            return div()
                .flex_1()
                .min_w_0()
                .h(px(24.))
                .px_1p5()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .rounded_sm()
                .bg(t.bg)
                .border_1()
                .border_color(t.accent)
                .child(SharedString::from(pre.to_owned()))
                .when(!sel.is_empty(), |d| {
                    d.child(div().bg(t.text_selection).child(SharedString::from(sel.to_owned())))
                })
                .child(caret)
                .child(SharedString::from(post.to_owned()))
                .into_any_element();
        }
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(if e.kind == Kind::Other { t.muted } else { t.text })
            .when(is_cut, |d| d.opacity(0.55))
            .when(e.kind.is_dir(), |d| d.font_weight(FontWeight::MEDIUM))
            .child(SharedString::from(self.source.entry_label(e)))
            .into_any_element()
    }

    fn render_rows(&mut self, range: Range<usize>, window: &Window, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let t = self.mode.resolve(window.appearance());
        let lang = self.lang;
        static TABULAR_NUMBERS: OnceLock<FontFeatures> = OnceLock::new();
        let tabular_numbers = TABULAR_NUMBERS.get_or_init(|| FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
        let list_focused =
            window.is_window_active() && self.focus.is_focused(window) && matches!(self.modal, Modal::None);
        let cut = self.clip.as_ref().filter(|c| self.source.is_local() && c.op == Op::Move).map(|c| &c.set);
        range
            .filter_map(|ix| self.entries.get(ix).map(|e| (ix, e)))
            .map(|(ix, e)| {
                let selected = self.selection.is_selected(ix, &e.name);
                let focused = list_focused && ix == self.selection.cursor;
                let is_cut = cut.is_some_and(|set| set.contains(&self.cwd.join(&e.name)));
                div()
                    .id(ix)
                    .w_full()
                    .h(px(ROW_H))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(if focused { t.accent } else { alpha(t.accent, 0.) })
                    .when(selected, |d| d.bg(t.selection))
                    .when(!selected, |d| d.hover(|d| d.bg(t.hover)))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _w, cx| this.click_row(ix, ev, cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                            this.right_click_row(ix, ev.position, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(file_tile(e, &t).when(is_cut, |d| d.opacity(0.45)))
                    .child(self.render_name(e, is_cut, &t))
                    .child(
                        div()
                            .w(px(COL_SIZE_W))
                            .flex_none()
                            .text_right()
                            .text_xs()
                            .font_features(tabular_numbers.clone())
                            .text_color(t.muted)
                            .child(match (e.kind.is_dir(), e.size) {
                                (true, _) => SharedString::from("—"),
                                (false, Some(s)) => human_size(s),
                                (false, None) => SharedString::default(),
                            }),
                    )
                    .child(
                        div()
                            .w(px(COL_DATE_W))
                            .flex_none()
                            .text_right()
                            .text_xs()
                            .text_color(t.muted)
                            .child(e.modified.map(|m| age(lang, m)).unwrap_or_default()),
                    )
            })
            .collect()
    }
}
