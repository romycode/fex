//! Render con GPUI. Aquí no hay lógica de negocio: solo se lee el estado de [`Explorer`]
//! y se conectan clics a métodos o a comandos del registro.

mod list;
mod overlays;
mod sidebar;
mod status;
mod toolbar;

use std::{
    path::Path,
    time::{Duration, SystemTime},
};

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, IntoElement, MouseButton, MouseDownEvent, Render, Role,
    SharedString, Stateful, Window, div, prelude::*, px,
};

use super::{Explorer, Menu, Modal, commands};
use crate::{
    fs::{Entry, Kind},
    i18n::Lang,
    icons::Icon,
    theme::{Theme, alpha},
};

pub const ROW_H: f32 = 30.;
pub const FILE_TILE_W: f32 = 20.;
pub const SIDEBAR_W: f32 = 184.;
pub const COL_SIZE_W: f32 = 84.;
pub const COL_DATE_W: f32 = 118.;

struct Tooltip {
    label: &'static str,
    shortcut: SharedString,
    theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(t.border)
            .bg(t.popover)
            .shadow_md()
            .text_xs()
            .text_color(t.text)
            .child(self.label)
            .when(!self.shortcut.is_empty(), |d| {
                d.child(div().px_1().rounded_sm().bg(t.control).text_color(t.muted).child(self.shortcut.clone()))
            })
    }
}

/// Botón cuadrado con icono y tooltip.
pub fn icon_button(
    id: impl Into<SharedString>,
    icon: Icon,
    label: &'static str,
    shortcut: impl Into<SharedString>,
    active: bool,
    t: Theme,
) -> Stateful<Div> {
    let shortcut = shortcut.into();
    div()
        .id(id.into())
        .role(Role::Button)
        .aria_label(label)
        .size(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .when(active, |d| d.bg(t.selection))
        .hover(|d| d.bg(if active { t.selection } else { t.hover }))
        .child(icon.el(16.).text_color(if active { t.accent } else { t.muted }))
        .tooltip(move |_, cx| cx.new(|_| Tooltip { label, shortcut: shortcut.clone(), theme: t }).into())
}

impl Explorer {
    /// Botón de barra generado desde un comando: icono, etiqueta, atajo y estado salen del registro.
    pub(super) fn command_button(&self, id: &'static str, t: &Theme, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let command = commands::find(id);
        if !(command.when)(self) {
            return None;
        }
        let checked = command.is_checked(self);
        let button = icon_button(
            id,
            command.icon.unwrap_or(Icon::Unknown),
            (command.label)(self.lang.t()),
            command.shortcut(self.lang),
            checked,
            *t,
        )
        .when(command.checked.is_some(), |d| {
            d.aria_toggled(if checked { gpui::Toggled::True } else { gpui::Toggled::False })
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| commands::run(this, command, cx)));
        Some(button)
    }
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.mode.resolve(window.appearance());

        // Envoltorio para el clic derecho en zona vacía (las filas paran la propagación).
        let body = div()
            .id("body")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.menu = Some(Menu { pos: ev.position, on_item: false });
                    cx.notify();
                }),
            )
            .child(self.render_list(&t, cx));

        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(&t, cx))
            .child(
                div()
                    .mx_3()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .rounded_xl()
                    .border_1()
                    .border_color(t.border)
                    .bg(t.bg)
                    .overflow_hidden()
                    .child(self.render_column_header(&t, cx))
                    .child(body),
            )
            .child(self.render_footer(&t, cx));

        let menu = self.menu.as_ref().map(|m| self.render_menu(m, &t, cx));
        let overlay = match &self.modal {
            Modal::ConfirmDelete(paths) => Some(self.render_confirm(paths, &t, cx).into_any_element()),
            Modal::Errors(errors) => Some(self.render_errors(errors, &t, cx).into_any_element()),
            Modal::Palette(_) => Some(self.render_palette(&t, cx).into_any_element()),
            _ => None,
        };

        div()
            .key_context("Explorer")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .relative()
            .size_full()
            .flex()
            .bg(t.frame)
            .text_color(t.text)
            .text_sm()
            .child(self.render_sidebar(&t, cx))
            .child(main)
            .children(menu)
            .children(overlay)
    }
}

// --------------------------------------------------------------- helpers ----

/// Icono "tile": SVG sobre fondo tintado; carpetas con acento, ficheros con color por tipo.
pub fn file_tile(e: &Entry, t: &Theme) -> Div {
    let (icon, tint) = match e.kind {
        Kind::Dir => (Icon::Folder, Some(t.accent)),
        Kind::Symlink { to_dir: true } => (Icon::FolderLink, Some(t.accent)),
        Kind::Symlink { to_dir: false } => (Icon::Link, None),
        Kind::Other => (Icon::Unknown, None),
        Kind::File => {
            let ext = Path::new(&e.name).extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            t.file_style(&ext)
        }
    };
    let (bg, fg) = match tint {
        Some(c) => (alpha(c, 0.15), c),
        None => (t.tile_file, t.tile_file_fg),
    };
    div()
        .size(px(FILE_TILE_W))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(bg)
        .child(icon.el(13.).text_color(fg))
}

pub fn placeholder(text: String, color: gpui::Rgba) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(SharedString::from(text))
        .into_any_element()
}

pub fn human_size(b: u64) -> SharedString {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024. && i < U.len() - 1 {
        v /= 1024.;
        i += 1;
    }
    if i == 0 { format!("{b} B").into() } else { format!("{v:.1} {}", U[i]).into() }
}

pub fn age(lang: Lang, t: SystemTime) -> SharedString {
    lang.age(SystemTime::now().duration_since(t).unwrap_or(Duration::ZERO).as_secs()).into()
}

/// Cabecera de sección (barra lateral).
pub fn heading(label: &'static str, t: &Theme) -> Div {
    div()
        .h(px(30.))
        .px_2()
        .flex()
        .items_center()
        .justify_between()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.muted)
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn sizes_are_human() {
        assert_eq!(human_size(0).as_ref(), "0 B");
        assert_eq!(human_size(1023).as_ref(), "1023 B");
        assert_eq!(human_size(1536).as_ref(), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024).as_ref(), "5.0 GB");
    }
}
