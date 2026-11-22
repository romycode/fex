use std::path::{Path, PathBuf};

use gpui::{ClickEvent, Context, Div, FontWeight, Role, SharedString, Stateful, div, prelude::*, px};

use super::{Explorer, SIDEBAR_W, heading, icon_button};
use crate::{
    app::commands,
    icons::Icon,
    theme::Theme,
};

impl Explorer {
    pub(super) fn render_sidebar(&self, t: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let s = self.lang.t();
        let places = &self.places;
        let pins = places.pins.as_deref().unwrap_or_default();
        // La entrada activa es la más específica que contiene el directorio actual.
        let current = places
            .fixed
            .iter()
            .map(|place| ("place", &place.path))
            .chain(places.disks.iter().map(|path| ("disk", path)))
            .chain(pins.iter().map(|path| ("pin", path)))
            .filter(|(_, path)| self.cwd.starts_with(path) && (path.as_path() != Path::new("/") || &self.cwd == *path))
            .max_by_key(|(_, path)| path.as_os_str().len());

        let row = |section, i, path: &PathBuf, label: SharedString, icon: Icon, cx: &mut Context<Self>| {
            let active = self.source.is_local() && current == Some((section, path));
            let target = path.clone();
            div()
                .id(SharedString::from(format!("{section}-{i}")))
                .role(Role::Button)
                .aria_label(label.clone())
                .aria_description(path.display().to_string())
                .h(px(34.))
                .flex_1()
                .min_w_0()
                .px_2()
                .flex()
                .items_center()
                .gap_2p5()
                .rounded_lg()
                .cursor_pointer()
                .text_color(if active { t.text } else { t.muted })
                .when(active, |d| d.bg(t.selection).font_weight(FontWeight::MEDIUM))
                .when(!active, |d| d.hover(|d| d.bg(t.hover)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.navigate(target.clone(), None, cx);
                }))
                .child(icon.el(15.).text_color(if active { t.accent } else { t.faint }))
                .child(div().min_w_0().truncate().child(label))
        };
        let path_label = |path: &Path| -> SharedString {
            path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned().into()
        };
        let empty = |label| div().px_2().py_1().text_xs().text_color(t.faint).child(label);

        let fixed = places
            .fixed
            .iter()
            .enumerate()
            .map(|(i, place)| row("place", i, &place.path, place.id.label(s).into(), place.icon, cx))
            .collect::<Vec<_>>();
        let pinned = pins
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let link = row("pin", i, path, path_label(path), Icon::Folder, cx);
                let target = path.clone();
                let remove = icon_button(SharedString::from(format!("unpin-{i}")), Icon::Close, s.unpin_folder, "", false, *t)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_pinned(target.clone(), false, cx)));
                div().id(SharedString::from(format!("pin-row-{i}"))).flex().items_center().gap_0p5().child(link).child(remove)
            })
            .collect::<Vec<_>>();
        let disks = places
            .disks
            .iter()
            .enumerate()
            .map(|(i, path)| row("disk", i, path, path_label(path), Icon::System, cx))
            .collect::<Vec<_>>();

        let pin = commands::find("places.pin");
        let pin_button = (pin.when)(self).then(|| {
            let pinned = self.is_pinned();
            icon_button(
                "pin-current",
                Icon::Pin,
                if pinned { s.unpin_folder } else { s.pin_folder },
                pin.shortcut(self.lang),
                pinned,
                *t,
            )
            .aria_toggled(if pinned { gpui::Toggled::True } else { gpui::Toggled::False })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| commands::run(this, pin, cx)))
        });

        let s3 = commands::find("nav.s3");
        div()
            .id("sidebar")
            .w(px(SIDEBAR_W))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .px_3()
            .py_3()
            .overflow_y_scroll()
            .child(div().flex_none().child(heading(s.places, t)).children(fixed))
            .child(
                div()
                    .id("s3")
                    .role(Role::Button)
                    .aria_label((s3.label)(s))
                    .h(px(34.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .rounded_lg()
                    .cursor_pointer()
                    .when(!self.source.is_local(), |d| d.bg(t.selection))
                    .hover(|d| d.bg(t.hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| commands::run(this, s3, cx)))
                    .child(Icon::Globe.el(15.).text_color(t.accent))
                    .child("Amazon S3"),
            )
            .child(
                div()
                    .flex_none()
                    .child(heading(s.pins, t).children(pin_button))
                    .when(pins.is_empty(), |d| d.child(empty(s.no_pins)))
                    .children(pinned),
            )
            .child(
                div()
                    .flex_none()
                    .child(heading(s.disks, t))
                    .when(disks.is_empty(), |d| d.child(empty(s.no_disks)))
                    .children(disks),
            )
    }
}
