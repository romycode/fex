//! Registro de comandos: la única forma de exponer una acción al usuario.
//!
//! Cada comando se declara una vez con su id, etiqueta, icono, atajos, condición de
//! disponibilidad y acción. De aquí salen el teclado, el menú contextual, los botones de
//! la barra de herramientas y la paleta de comandos (`Ctrl+K`). Añadir una funcionalidad
//! nueva = añadir una entrada a [`COMMANDS`] (y, si procede, a un menú).
//!
//! Los atajos se escriben como en [`crate::keymap`]. Si dos comandos comparten atajo, gana
//! el primero de la lista cuyo `when` sea cierto: así `mod-d` fija carpetas en local y
//! descarga en S3.

use gpui::Context;

use super::Explorer;
use crate::{
    fs::SortColumn,
    i18n::Strings,
    icons::Icon,
    ops::Op,
};

type Run = fn(&mut Explorer, &mut Context<Explorer>);

pub struct Command {
    /// Identificador estable (`área.acción`). Lo usan menús, tests y, en el futuro, el keymap del usuario.
    pub id: &'static str,
    pub label: fn(&Strings) -> &'static str,
    pub icon: Option<Icon>,
    pub keys: &'static [&'static str],
    /// Se pinta en rojo: acciones destructivas.
    pub danger: bool,
    /// Aparece en la paleta. Los movimientos de cursor no.
    pub palette: bool,
    /// Disponible ahora. Si es falso no se ejecuta, no sale en menús ni en la paleta.
    pub when: fn(&Explorer) -> bool,
    /// Estado activado (botones conmutables).
    pub checked: Option<fn(&Explorer) -> bool>,
    pub run: Run,
}

impl Command {
    pub fn is_checked(&self, explorer: &Explorer) -> bool {
        self.checked.is_some_and(|checked| checked(explorer))
    }
}

const fn cmd(id: &'static str, label: fn(&Strings) -> &'static str, run: Run) -> Command {
    Command {
        id,
        label,
        icon: None,
        keys: &[],
        danger: false,
        palette: true,
        when: always,
        checked: None,
        run,
    }
}

impl Command {
    const fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    const fn keys(mut self, keys: &'static [&'static str]) -> Self {
        self.keys = keys;
        self
    }
    const fn when(mut self, when: fn(&Explorer) -> bool) -> Self {
        self.when = when;
        self
    }
    const fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
    const fn hidden(mut self) -> Self {
        self.palette = false;
        self
    }
    const fn checked(mut self, checked: fn(&Explorer) -> bool) -> Self {
        self.checked = Some(checked);
        self
    }
}

fn always(_: &Explorer) -> bool {
    true
}
fn no_label(_: &Strings) -> &'static str {
    ""
}
fn has_entries(e: &Explorer) -> bool {
    !e.entries.is_empty()
}
fn writable(e: &Explorer) -> bool {
    e.caps().write && e.can_start_job()
}
fn writable_with_targets(e: &Explorer) -> bool {
    writable(e) && has_items(e)
}
/// Hay algo seleccionado sobre lo que actuar (en la lista de buckets, no).
fn has_items(e: &Explorer) -> bool {
    !e.target_items().is_empty()
}

/// Todos los comandos. El orden importa para resolver atajos compartidos.
pub static COMMANDS: &[Command] = &[
    // ------------------------------------------------------------- navegación
    cmd("nav.open", |s| s.open, |e, cx| e.open_cursor(cx))
        .icon(Icon::Open)
        .keys(&["enter", "right", "l"])
        .when(|e| e.cursor_entry().is_some_and(|entry| entry.kind.is_dir() || e.caps().open_files)),
    cmd("nav.up", |s| s.go_up, |e, cx| e.go_up(cx)).icon(Icon::ArrowUp).keys(&[
        "alt-up",
        "backspace",
        "left",
        "h",
    ]),
    cmd("nav.location", |s| s.go_to, |e, cx| e.edit_location(cx)).icon(Icon::Folder).keys(&["mod-l"]).when(|e| e.source.is_local()),
    cmd("nav.s3", |s| s.s3_browse, |e, cx| e.browse_s3(cx)).icon(Icon::Globe),
    cmd("s3.add", |s| s.s3_add, |e, cx| e.add_connection(cx)).icon(Icon::Plus),
    cmd("s3.edit", |s| s.s3_edit, |e, cx| e.edit_current_connection(cx))
        .icon(Icon::Pencil)
        .when(|e| !e.source.is_local()),
    cmd("view.refresh", |s| s.refresh, |e, cx| e.reload(Vec::new(), cx)).icon(Icon::Refresh).keys(&["f5", "mod-r"]),
    cmd("view.hidden", |s| s.hidden, |e, cx| e.toggle_hidden(cx))
        .icon(Icon::Eye)
        .keys(&["linux:mod-h", "mac:mod-shift-.", "mac:mod-shift->", "."])
        .when(|e| e.caps().hidden_files)
        .checked(|e| e.show_hidden),
    cmd("view.sort_name", |s| s.sort_name, |e, cx| e.change_sort(SortColumn::Name, cx)),
    cmd("view.sort_size", |s| s.sort_size, |e, cx| e.change_sort(SortColumn::Size, cx)),
    cmd("view.sort_date", |s| s.sort_date, |e, cx| e.change_sort(SortColumn::Modified, cx)),
    cmd("view.theme", |s| s.cycle_theme, |e, cx| e.cycle_theme(cx)).icon(Icon::Contrast).keys(&["t"]),
    cmd("view.lang", |s| s.cycle_lang, |e, cx| e.cycle_lang(cx)).icon(Icon::Globe).keys(&["i"]),
    cmd("app.palette", |s| s.palette, |e, cx| e.open_palette(cx)).keys(&["mod-k", "mod-shift-p"]).hidden(),
    cmd("app.edit_keymap", |s| s.edit_keymap, |e, cx| e.edit_keymap(cx)).icon(Icon::Pencil),
    cmd("app.reload_keymap", |s| s.reload_keymap, |e, cx| e.reload_keymap(cx)).icon(Icon::Refresh),
    cmd("app.quit", |s| s.quit, |_, cx| cx.quit()).keys(&["mod-q", "q"]).when(|e| e.jobs.busy.is_empty()),
    // -------------------------------------------------------------- selección
    cmd("cursor.down", no_label, |e, cx| e.move_cursor(1, false, cx)).keys(&["down", "j"]).hidden(),
    cmd("cursor.up", no_label, |e, cx| e.move_cursor(-1, false, cx)).keys(&["up", "k"]).hidden(),
    cmd("cursor.page_down", no_label, |e, cx| e.move_cursor(25, false, cx)).keys(&["pagedown"]).hidden(),
    cmd("cursor.page_up", no_label, |e, cx| e.move_cursor(-25, false, cx)).keys(&["pageup"]).hidden(),
    cmd("cursor.first", no_label, |e, cx| e.move_cursor(isize::MIN, false, cx)).keys(&["home", "g"]).hidden(),
    cmd("cursor.last", no_label, |e, cx| e.move_cursor(isize::MAX, false, cx)).keys(&["end"]).hidden(),
    cmd("select.down", no_label, |e, cx| e.move_cursor(1, true, cx)).keys(&["shift-down"]).hidden(),
    cmd("select.up", no_label, |e, cx| e.move_cursor(-1, true, cx)).keys(&["shift-up"]).hidden(),
    cmd("select.page_down", no_label, |e, cx| e.move_cursor(25, true, cx)).keys(&["shift-pagedown"]).hidden(),
    cmd("select.page_up", no_label, |e, cx| e.move_cursor(-25, true, cx)).keys(&["shift-pageup"]).hidden(),
    cmd("select.first", no_label, |e, cx| e.move_cursor(isize::MIN, true, cx)).keys(&["shift-home"]).hidden(),
    cmd("select.last", no_label, |e, cx| e.move_cursor(isize::MAX, true, cx)).keys(&["shift-end"]).hidden(),
    cmd("select.all", |s| s.select_all, |e, cx| e.select_all(cx)).icon(Icon::SelectAll).keys(&["mod-a"]).when(has_entries),
    cmd("select.clear", no_label, |e, cx| e.escape(cx)).keys(&["escape"]).hidden(),
    // ----------------------------------------------------------- operaciones
    cmd("file.cut", |s| s.cut, |e, cx| e.copy_or_cut(Op::Move, cx))
        .icon(Icon::Cut)
        .keys(&["mod-x"])
        .when(|e| e.caps().write && has_items(e)),
    cmd("file.copy", |s| s.copy, |e, cx| e.copy_or_cut(Op::Copy, cx))
        .icon(Icon::Copy)
        .keys(&["mod-c"])
        .when(has_items),
    cmd("file.paste", |s| s.paste, |e, cx| e.paste(cx)).icon(Icon::Paste).keys(&["mod-v"]).when(writable),
    cmd("file.rename", |s| s.rename, |e, cx| e.start_rename(cx))
        .icon(Icon::Pencil)
        .keys(&["f2"])
        .when(writable_with_targets),
    cmd("file.new_folder", |s| s.new_folder, |e, cx| e.new_folder(cx))
        .icon(Icon::FolderPlus)
        .keys(&["mod-shift-n"])
        .when(writable),
    cmd("file.trash", |s| s.trash, |e, cx| e.trash_selection(cx))
        .icon(Icon::Trash)
        .keys(&["mac:mod-backspace", "delete"])
        .when(|e| e.caps().trash && writable_with_targets(e)),
    cmd("file.delete", |s| s.delete, |e, cx| e.ask_delete(cx))
        .icon(Icon::DeleteForever)
        // Sin papelera (S3), Supr y ⌘⌫ piden borrar permanentemente: `file.trash` va antes.
        .keys(&["mac:mod-shift-backspace", "shift-delete", "delete", "mac:mod-backspace"])
        .when(writable_with_targets)
        .danger(),
    cmd("file.undo", |s| s.undo, |e, cx| e.undo_last(cx))
        .icon(Icon::Undo)
        .keys(&["mod-z"])
        .when(|e| e.can_start_job() && !e.jobs.undo.is_empty()),
    cmd("file.download", |s| s.download, |e, cx| e.download_selection(cx))
        .icon(Icon::Download)
        .keys(&["mod-d"])
        .when(|e| e.caps().download && e.can_start_job() && has_items(e)),
    cmd("job.cancel", |s| s.cancel_job, |e, cx| e.cancel_job(cx))
        .icon(Icon::Close)
        .when(|e| e.is_busy() && !e.jobs.progress.is_cancelled()),
    cmd("places.pin", |s| s.pin_folder, |e, cx| e.toggle_pin(cx))
        .icon(Icon::Pin)
        .keys(&["mod-d"])
        .when(|e| e.caps().pin && e.places.pins.is_some())
        .checked(|e| e.is_pinned()),
];

pub fn find(id: &str) -> &'static Command {
    COMMANDS.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("comando desconocido: {id}"))
}

/// Entrada de menú: un comando o un separador. `Single` solo aparece con un único objetivo.
#[derive(Clone, Copy)]
pub enum MenuItem {
    Cmd(&'static str),
    Single(&'static str),
    Separator,
}

use MenuItem::{Cmd, Separator, Single};

/// Menú al hacer clic derecho sobre una fila.
pub static ITEM_MENU: &[MenuItem] = &[
    Single("nav.open"),
    Separator,
    Cmd("file.download"),
    Cmd("file.cut"),
    Cmd("file.copy"),
    Cmd("file.paste"),
    Separator,
    Single("file.rename"),
    Separator,
    Cmd("file.trash"),
    Cmd("file.delete"),
];

/// Menú al hacer clic derecho en una zona vacía.
pub static BACKGROUND_MENU: &[MenuItem] = &[
    Cmd("file.undo"),
    Separator,
    Cmd("file.paste"),
    Cmd("file.new_folder"),
    Separator,
    Cmd("view.refresh"),
    Cmd("select.all"),
];

/// Comandos visibles de un menú, sin separadores al principio, al final ni repetidos.
pub fn resolve_menu(items: &[MenuItem], explorer: &Explorer) -> Vec<Option<&'static Command>> {
    let single = explorer.selection.targets(&explorer.entries).len() == 1;
    let mut out: Vec<Option<&'static Command>> = Vec::new();
    for item in items {
        match *item {
            Separator => {
                if out.last().is_some_and(Option::is_some) {
                    out.push(None);
                }
            }
            Cmd(id) | Single(id) => {
                let command = find(id);
                if (command.when)(explorer) && (single || !matches!(item, Single(_))) {
                    out.push(Some(command));
                }
            }
        }
    }
    if out.last().is_some_and(Option::is_none) {
        out.pop();
    }
    out
}

/// Ejecuta el primer comando disponible cuyo atajo case con la pulsación.
pub fn dispatch(explorer: &mut Explorer, keystroke: &gpui::Keystroke, cx: &mut Context<Explorer>) -> bool {
    let Some(command) = explorer.bindings.find(explorer, keystroke) else {
        return false;
    };
    run(explorer, command, cx);
    true
}

pub fn run(explorer: &mut Explorer, command: &Command, cx: &mut Context<Explorer>) {
    if !(command.when)(explorer) {
        return;
    }
    explorer.menu = None;
    (command.run)(explorer, cx);
    cx.notify();
}

/// Puntuación de coincidencia para la paleta: subcadena > iniciales > subsecuencia.
pub fn fuzzy_score(label: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let label = label.to_lowercase();
    let query = query.to_lowercase();
    if let Some(at) = label.find(&query) {
        return Some(1000 - at.min(999) as u32);
    }
    let mut chars = label.chars();
    query.chars().filter(|c| !c.is_whitespace()).all(|q| chars.any(|c| c == q)).then_some(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{i18n::Lang, keymap};
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_menus_reference_known_commands() {
        let mut seen = HashSet::new();
        for command in COMMANDS {
            assert!(seen.insert(command.id), "id repetido: {}", command.id);
            for spec in command.keys {
                let valid = [keymap::Platform::Linux, keymap::Platform::Mac]
                    .into_iter()
                    .any(|platform| keymap::Chord::parse(spec, platform).is_some());
                assert!(valid, "atajo inválido en {}: {spec}", command.id);
            }
        }
        for item in ITEM_MENU.iter().chain(BACKGROUND_MENU) {
            if let Cmd(id) | Single(id) = item {
                assert!(seen.contains(id), "el menú usa un comando desconocido: {id}");
            }
        }
    }

    /// En cada plataforma, un atajo solo se comparte a propósito (con `when` excluyentes) y
    /// ningún comando con atajo se queda sin él en una de las dos.
    #[test]
    fn shortcuts_are_consistent_on_linux_and_macos() {
        use keymap::{Chord, Platform};
        // Atajos compartidos a propósito: local vs. S3.
        // Papelera vs. borrado permanente: el segundo solo responde donde no hay papelera.
        const SHARED: &[&str] = &["mod-d", "mod-l", "delete", "mod-backspace"];
        for platform in [Platform::Linux, Platform::Mac] {
            let mut owners: std::collections::HashMap<Chord, Vec<&str>> = Default::default();
            for command in COMMANDS {
                let chords: Vec<_> = command.keys.iter().filter_map(|spec| Chord::parse(spec, platform)).collect();
                assert!(
                    command.keys.is_empty() || !chords.is_empty(),
                    "{} no tiene atajo en {platform:?}",
                    command.id
                );
                for chord in chords {
                    owners.entry(chord).or_default().push(command.id);
                }
            }
            for (chord, ids) in owners {
                let spec = command_spec(&chord);
                assert!(ids.len() == 1 || SHARED.contains(&spec.as_str()), "{spec} repetido en {platform:?}: {ids:?}");
            }
        }
    }

    fn command_spec(chord: &keymap::Chord) -> String {
        let mut spec = String::new();
        for (on, name) in [(chord.command, "mod-"), (chord.ctrl, "ctrl-"), (chord.alt, "alt-"), (chord.shift, "shift-")] {
            if on {
                spec.push_str(name);
            }
        }
        spec + chord.key
    }

    #[test]
    fn palette_commands_have_labels_in_every_language() {
        for lang in [Lang::Es, Lang::Ca, Lang::En] {
            for command in COMMANDS.iter().filter(|c| c.palette) {
                assert!(!(command.label)(lang.t()).is_empty(), "{} sin etiqueta", command.id);
            }
        }
    }

    #[test]
    fn fuzzy_prefers_substrings() {
        assert!(fuzzy_score("Nueva carpeta", "carp").unwrap() > fuzzy_score("Nueva carpeta", "nc").unwrap());
        assert_eq!(fuzzy_score("Copiar", "xyz"), None);
        assert_eq!(fuzzy_score("Copiar", ""), Some(0));
        assert!(fuzzy_score("Mover a la papelera", "mvpap").is_some());
    }
}
