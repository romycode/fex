//! Capas por encima de la vista: menú contextual, confirmación, informe de errores y paleta.


use gpui::{
    ClickEvent, Context, Div, FontWeight, MouseDownEvent, SharedString, Stateful, anchored, deferred, div, prelude::*,
    px, relative, white,
};

use super::Explorer;
use crate::{
    app::{Menu, Modal, commands},
    backend::Item,
    icons::Icon,
    theme::{Theme, alpha},
};

/// Fondo oscurecido a pantalla completa. Un clic fuera del contenido cierra el modal.
pub(super) fn scrim(id: &'static str, t: &Theme, cx: &Context<Explorer>) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .occlude()
        .bg(t.scrim)
        .flex()
        .items_center()
        .justify_center()
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.modal = Modal::None;
            cx.notify();
        }))
}

pub(super) fn dialog(id: &'static str, t: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .rounded_xl()
        .bg(t.popover)
        .border_1()
        .border_color(t.border)
        .shadow_lg()
        .on_click(|_, _, cx| cx.stop_propagation())
}

/// Botón secundario (fondo neutro).
pub(super) fn button(id: &'static str, label: &'static str, t: &Theme) -> Stateful<Div> {
    let hover = t.hover;
    button_base(id, label).bg(t.control).hover(move |d| d.bg(hover))
}

/// Botón principal o destructivo: fondo de color, texto blanco.
/// GPUI solo admite un `hover` por elemento, por eso no se construye sobre [`button`].
pub(super) fn filled_button(id: &'static str, label: &'static str, color: gpui::Rgba) -> Stateful<Div> {
    button_base(id, label).bg(color).text_color(white()).font_weight(FontWeight::SEMIBOLD).hover(|d| d.opacity(0.9))
}

fn button_base(id: &'static str, label: &'static str) -> Stateful<Div> {
    div().id(id).h(px(32.)).px_3().flex().items_center().rounded_md().cursor_pointer().child(label)
}

impl Explorer {
    pub(super) fn render_menu(&self, menu: &Menu, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let s = self.lang.t();
        let spec = if menu.on_item { commands::ITEM_MENU } else { commands::BACKGROUND_MENU };
        let rows = commands::resolve_menu(spec, self).into_iter().enumerate().map(|(i, item)| match item {
            None => div().my_1().mx_1().h(px(1.)).bg(t.border).into_any_element(),
            Some(command) => {
                let danger = command.danger;
                let fg = if danger { t.error } else { t.text };
                div()
                    .id(SharedString::from(format!("menu-{i}")))
                    .h(px(30.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(fg)
                    .hover(|d| d.bg(if danger { alpha(t.error, 0.12) } else { t.hover }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| commands::run(this, command, cx)))
                    .children(command.icon.map(|icon| icon.el(15.).text_color(if danger { t.error } else { t.muted })))
                    .child(div().flex_1().whitespace_nowrap().child((command.label)(s)))
                    .child(
                        div()
                            .pl_6()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(t.faint)
                            .child(SharedString::from(self.shortcut(command))),
                    )
                    .into_any_element()
            }
        });

        deferred(
            anchored().position(menu.pos).snap_to_window_with_margin(px(8.)).child(
                div()
                    .id("context-menu")
                    .occlude()
                    .min_w(px(250.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .bg(t.popover)
                    .border_1()
                    .border_color(t.border)
                    .shadow_lg()
                    .text_sm()
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(rows),
            ),
        )
        .with_priority(1)
    }

    pub(super) fn render_confirm(&self, paths: &[Item], t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let s = self.lang.t();
        let preview = paths
            .iter()
            .take(3)
            .map(Item::name)
            .collect::<Vec<_>>()
            .join(", ")
            + if paths.len() > 3 { ", …" } else { "" };

        scrim("scrim", t, cx).child(
            dialog("confirm", t)
                .w(px(400.))
                .p_5()
                .gap_2()
                .child(
                    div().flex().items_center().gap_2().child(Icon::Alert.el(18.).text_color(t.error)).child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(self.lang.confirm_delete(paths.len()))),
                    ),
                )
                .child(div().text_color(t.muted).truncate().child(SharedString::from(preview)))
                .child(div().text_color(t.muted).child(s.confirm_body))
                .child(
                    div()
                        .pt_3()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(button("cancel", s.cancel, t).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.modal = Modal::None;
                            cx.notify();
                        })))
                        .child(
                            filled_button("do-delete", s.delete, t.danger_bg)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.confirm_delete(cx);
                                    cx.notify();
                                })),
                        ),
                ),
        )
    }

    pub(super) fn render_errors(&self, errors: &[String], t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let s = self.lang.t();
        scrim("errors-scrim", t, cx).child(
            dialog("errors", t)
                .w(px(560.))
                .max_w(relative(0.9))
                .max_h(relative(0.8))
                .p_4()
                .gap_3()
                .child(
                    div().id("operation-errors").min_h_0().overflow_y_scroll().children(
                        errors
                            .iter()
                            .map(|error| div().py_1().text_color(t.error).child(SharedString::from(error.clone()))),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .when(self.jobs.retry_paste.is_some(), |d| {
                            d.child(
                                button_base("retry-paste", s.retry)
                                    .bg(t.selection)
                                    .hover(|d| d.opacity(0.9))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.retry_failed_paste(cx))),
                            )
                        })
                        .child(button("dismiss-errors", "OK", t).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.modal = Modal::None;
                            cx.notify();
                        }))),
                ),
        )
    }

    pub(super) fn render_palette(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let s = self.lang.t();
        let Modal::Palette(palette) = &self.modal else { unreachable!("solo se pinta con la paleta abierta") };
        let (before, selected_text, after) = palette.input.parts();
        let matches = self.palette_matches(&palette.input.text);
        let selected = palette.selected;

        let field = div()
            .h(px(40.))
            .px_3()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .border_b_1()
            .border_color(t.border)
            .when(palette.input.text.is_empty(), |d| {
                d.child(div().w(px(1.5)).h(px(18.)).bg(t.accent)).child(div().ml_1().text_color(t.faint).child(s.palette_placeholder))
            })
            .when(!palette.input.text.is_empty(), |d| {
                d.child(SharedString::from(before.to_owned()))
                    .child(div().bg(t.text_selection).child(SharedString::from(selected_text.to_owned())))
                    .child(div().w(px(1.5)).h(px(18.)).bg(t.accent))
                    .child(SharedString::from(after.to_owned()))
            });

        let rows = matches.iter().take(crate::app::PALETTE_ROWS).enumerate().map(|(i, command)| {
            let command = *command;
            let active = i == selected;
            div()
                .id(SharedString::from(format!("palette-{}", command.id)))
                .h(px(32.))
                .px_3()
                .flex()
                .items_center()
                .gap_2p5()
                .rounded_md()
                .cursor_pointer()
                .when(active, |d| d.bg(t.selection))
                .when(!active, |d| d.hover(|d| d.bg(t.hover)))
                .text_color(if command.danger { t.error } else { t.text })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.modal = Modal::None;
                    commands::run(this, command, cx);
                }))
                .child(
                    div()
                        .size(px(15.))
                        .flex_none()
                        .children(command.icon.map(|icon| icon.el(15.).text_color(if active { t.accent } else { t.muted }))),
                )
                .child(div().flex_1().truncate().child((command.label)(s)))
                .child(
                    div()
                        .text_xs()
                        .text_color(t.faint)
                        .whitespace_nowrap()
                        .child(SharedString::from(self.shortcut(command))),
                )
        });

        // Arriba y centrada, como en los editores.
        scrim("palette-scrim", t, cx).items_start().pt(px(72.)).child(
            dialog("palette", t)
                .w(px(520.))
                .max_w(relative(0.9))
                .overflow_hidden()
                .child(field)
                .child(
                    div()
                        .p_1()
                        .flex()
                        .flex_col()
                        .when(matches.is_empty(), |d| {
                            d.child(div().px_3().py_2().text_color(t.faint).child(s.no_commands))
                        })
                        .children(rows),
                ),
        )
    }
}
