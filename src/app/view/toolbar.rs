use std::path::Path;

use gpui::{ClickEvent, Context, Div, FontWeight, SharedString, div, prelude::*, px, relative};

use super::{COL_DATE_W, COL_SIZE_W, Explorer, FILE_TILE_W, icon_button};
use crate::{
    app::{LocationInput, Modal, commands},
    backend::Location,
    fs::SortColumn,
    icons::Icon,
    theme::Theme,
};

/// Botones a la derecha de la ruta. `None` = separador. Solo se pintan los disponibles.
const TOOLBAR: &[Option<&str>] = &[Some("file.new_folder"), None, Some("view.hidden"), Some("file.download")];

impl Explorer {
    /// Segmentos de la ruta actual, con "Inicio" para HOME y "…" si hay demasiados.
    pub fn crumbs(&self) -> Vec<(SharedString, Location)> {
        let mut out = Vec::new();
        match &self.source {
            Location::S3(location) => {
                let mut current = location.clone();
                loop {
                    out.push((current.label().into(), Location::S3(current.clone())));
                    let Some((parent, _)) = current.parent() else { break };
                    current = parent;
                }
                out.reverse();
            }
            Location::Local(path) => {
                let (label, base) = match &self.home {
                    Some(home) if path.starts_with(home) => (self.lang.t().home, home.as_path()),
                    _ => ("/", Path::new("/")),
                };
                let mut accumulated = base.to_path_buf();
                out.push((label.into(), Location::Local(accumulated.clone())));
                for segment in path.strip_prefix(base).unwrap_or(path).components() {
                    accumulated.push(segment);
                    out.push((
                        segment.as_os_str().to_string_lossy().into_owned().into(),
                        Location::Local(accumulated.clone()),
                    ));
                }
            }
        }
        if out.len() > 6 {
            let hidden = out.len() - 5;
            out[hidden].0 = "…".into();
            out.drain(1..hidden);
        }
        out
    }

    fn render_trail(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        if let Modal::Location(location) = &self.modal {
            let (input, placeholder) = match location {
                LocationInput::Local(input) => (input, None),
                LocationInput::DownloadDestination(input, _) => (input, Some(self.lang.t().download_destination)),
            };
            let (before, selected, after) = input.parts();
            return div()
                .flex_1()
                .min_w_0()
                .h(px(34.))
                .px_2()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .rounded_md()
                .border_1()
                .border_color(t.accent)
                .bg(t.path_bg)
                .when_some(placeholder, |d, label| d.child(div().flex_none().mr_2().text_color(t.muted).child(label)))
                .child(SharedString::from(before.to_owned()))
                .child(div().bg(t.text_selection).child(SharedString::from(selected.to_owned())))
                .child(div().w(px(1.)).h(px(18.)).bg(t.accent))
                .child(SharedString::from(after.to_owned()));
        }

        let crumbs = self.crumbs();
        let last = crumbs.len().saturating_sub(1);
        let mut trail = div()
            .flex_1()
            .min_w_0()
            .h(px(34.))
            .flex()
            .items_center()
            .gap_0p5()
            .px_2()
            .rounded_md()
            .border_1()
            .border_color(t.border)
            .bg(t.path_bg)
            .overflow_hidden();
        for (i, (label, path)) in crumbs.into_iter().enumerate() {
            if i > 0 {
                trail = trail.child(Icon::ChevronRight.el(11.).text_color(t.faint));
            }
            let is_last = i == last;
            trail = trail.child(
                div()
                    .id(SharedString::from(format!("crumb-{i}")))
                    .min_w_0()
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(if is_last { t.text } else { t.muted })
                    .when(is_last, |d| {
                        d.flex_shrink_0()
                            .max_w(relative(if last > 0 { 0.6 } else { 1. }))
                            .bg(t.selection)
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!is_last, |d| d.max_w(px(110.)))
                    .truncate()
                    .hover(|d| d.bg(t.hover).text_color(t.text))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.navigate_to(path.clone(), None, cx);
                    }))
                    .child(label),
            );
        }
        trail
    }

    pub(super) fn render_toolbar(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let s = self.lang.t();

        // Separadores solo entre grupos con algún botón visible.
        let mut buttons = Vec::new();
        let mut pending_separator = false;
        for item in TOOLBAR {
            match item {
                None => pending_separator = !buttons.is_empty(),
                Some(id) => {
                    if let Some(button) = self.command_button(*id, t, cx) {
                        if pending_separator {
                            buttons.push(div().w(px(1.)).h(px(16.)).mx_0p5().bg(t.border).into_any_element());
                            pending_separator = false;
                        }
                        buttons.push(button.into_any_element());
                    }
                }
            }
        }
        let theme = commands::find("view.theme");
        let (theme_icon, theme_label) = self.mode.label(s);
        let actions = div()
            .h(px(34.))
            .flex_none()
            .flex()
            .items_center()
            .gap_0p5()
            .px_0p5()
            .border_1()
            .border_color(t.border)
            .rounded_md()
            .children(buttons)
            .child(
                icon_button("theme", theme_icon, theme_label, self.shortcut(theme), false, *t)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| commands::run(this, theme, cx))),
            );

        div()
            .flex_none()
            .h(px(56.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .children(self.command_button("nav.up", t, cx))
            .child(self.render_trail(t, cx))
            .child(actions)
    }

    pub(super) fn render_column_header(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let s = self.lang.t();
        let header = |id, label: &'static str, column, cx: &mut Context<Self>| {
            let marker = if self.sort_column != column {
                ""
            } else if self.sort_descending {
                " ↓"
            } else {
                " ↑"
            };
            div()
                .id(id)
                .cursor_pointer()
                .text_color(if self.sort_column == column { t.text } else { t.muted })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.change_sort(column, cx)))
                .child(SharedString::from(format!("{label}{marker}")))
        };
        div()
            .flex_none()
            .h(px(34.))
            .mx_2()
            .px(px(9.))
            .flex()
            .items_center()
            .gap_3()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .border_b_1()
            .border_color(t.border)
            .text_color(t.muted)
            .child(div().w(px(FILE_TILE_W)).flex_none())
            .child(header("sort-name", s.col_name, SortColumn::Name, cx).flex_1())
            .child(header("sort-size", s.col_size, SortColumn::Size, cx).w(px(COL_SIZE_W)).flex_none().text_right())
            .child(
                header("sort-date", s.col_modified, SortColumn::Modified, cx)
                    .w(px(COL_DATE_W))
                    .flex_none()
                    .text_right(),
            )
    }
}
