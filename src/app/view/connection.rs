//! Diálogo para añadir o editar una conexión S3.

use gpui::{ClickEvent, Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, px, relative, white};

use super::{
    Explorer,
    overlays::{button, dialog, filled_button, scrim},
};
use crate::{
    app::{
        Modal,
        connection_form::{ConnectionForm, Field, Row},
    },
    i18n::Strings,
    input::LineInput,
    theme::Theme,
};

const LABEL_W: f32 = 150.;

fn label(field: Field, s: &Strings) -> (&'static str, Option<&'static str>) {
    match field {
        Field::Name => (s.s3_name, None),
        Field::Profile => (s.aws_profile, None),
        Field::AccessKey => (s.s3_access_key, None),
        Field::SecretKey => (s.s3_secret_key, None),
        Field::SessionToken => (s.s3_session_token, Some(s.s3_optional)),
        Field::Endpoint => (s.s3_endpoint, Some(s.s3_default_hint)),
        Field::Region => (s.s3_region, Some(s.s3_optional)),
        Field::CaFile => (s.s3_ca_file, Some(s.s3_optional)),
    }
}

/// Campo de texto de una línea. Los secretos se muestran como puntos.
fn text_box(input: &LineInput, focused: bool, secret: bool, placeholder: Option<&'static str>, t: &Theme) -> Div {
    let mask = |text: &str| -> SharedString {
        if secret { "•".repeat(text.chars().count()).into() } else { text.to_owned().into() }
    };
    let (before, selected, after) = input.parts();
    let caret = || div().flex_none().w(px(1.5)).h(px(16.)).bg(t.accent);
    div()
        .flex_1()
        .min_w_0()
        .h(px(30.))
        .px_2()
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .rounded_md()
        .border_1()
        .border_color(if focused { t.accent } else { t.border })
        .bg(t.path_bg)
        .when(input.text.is_empty() && !focused, |d| {
            d.when_some(placeholder, |d, p| d.child(div().text_color(t.faint).child(p)))
        })
        .when(input.text.is_empty() && focused, |d| {
            d.child(caret()).when_some(placeholder, |d, p| d.child(div().ml_1().text_color(t.faint).child(p)))
        })
        .when(!input.text.is_empty(), |d| {
            d.child(mask(before))
                .when(focused && !selected.is_empty(), |d| d.child(div().bg(t.text_selection).child(mask(selected))))
                .when(!focused, |d| d.child(mask(selected)))
                .when(focused, |d| d.child(caret()))
                .child(mask(after))
        })
}

/// Interruptor de dos estados con su texto.
fn switch(on: bool, focused: bool, text: &'static str, t: &Theme) -> Div {
    let knob = div().size(px(14.)).rounded_full().bg(white());
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_1()
        .rounded_md()
        .border_1()
        .border_color(if focused { t.accent } else { gpui::transparent_black().into() })
        .child(
            div()
                .w(px(30.))
                .h(px(18.))
                .p(px(2.))
                .flex()
                .items_center()
                .rounded_full()
                .bg(if on { t.accent } else { t.control })
                .when(on, |d| d.justify_end())
                .child(knob),
        )
        .child(text)
}

impl Explorer {
    pub(super) fn render_connection(&self, form: &ConnectionForm, t: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let s = self.lang.t();
        let focused = form.focused();
        let rows = form.rows().into_iter().map(|row| {
            let is_focused = row == focused;
            let (title, control): (&'static str, gpui::AnyElement) = match row {
                Row::Text(field) => {
                    let (title, placeholder) = label(field, s);
                    let input = form.input(field);
                    let secret = matches!(field, Field::SecretKey | Field::SessionToken);
                    (title, text_box(input, is_focused, secret, placeholder, t).into_any_element())
                }
                Row::UseKeys => (
                    s.s3_credentials,
                    switch(form.use_keys, is_focused, if form.use_keys { s.s3_keys } else { s.s3_system }, t)
                        .into_any_element(),
                ),
                Row::PathStyle => (s.s3_path_style, switch(form.path_style, is_focused, "", t).into_any_element()),
            };
            div()
                .id(SharedString::from(format!("s3-form-{row:?}")))
                .flex()
                .items_center()
                .gap_3()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Modal::Connection(form) = &mut this.modal {
                        match row {
                            Row::Text(_) => form.focus_row(row),
                            Row::UseKeys | Row::PathStyle => form.toggle(row),
                        }
                        cx.notify();
                    }
                }))
                .child(div().w(px(LABEL_W)).flex_none().text_color(t.muted).child(title))
                .child(control)
        });

        let title = if form.original.is_some() { s.s3_edit } else { s.s3_add };
        scrim("s3-scrim", t, cx).child(
            dialog("s3-form", t)
                .w(px(560.))
                .max_w(relative(0.95))
                .p_5()
                .gap_2p5()
                .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child(title))
                .children(rows)
                .when_some(form.error.clone(), |d, error| d.child(div().pt_1().text_color(t.error).child(error)))
                .child(
                    div()
                        .pt_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .when(form.original.is_some(), |d| {
                            d.child(
                                button("s3-delete", s.s3_delete, t)
                                    .text_color(t.error)
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.delete_connection(cx))),
                            )
                        })
                        .child(div().flex_1())
                        .child(button("s3-cancel", s.cancel, t).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.modal = Modal::None;
                            cx.notify();
                        })))
                        .child(
                            filled_button("s3-save", s.s3_save, t.accent)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_connection(cx))),
                        ),
                ),
        )
    }
}
