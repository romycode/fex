//! i18n mínimo: tablas estáticas, sin dependencias ni carga de ficheros (coste de arranque cero).
//! Para muchos idiomas o plurales complejos, migrar a Fluent (`fluent-bundle`).

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Es,
    Ca,
    En,
}

impl Lang {
    /// Orden de precedencia POSIX: LC_ALL > LC_MESSAGES > LANG; luego LANGUAGE (lista ":").
    pub fn detect() -> Self {
        ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .flat_map(|v| v.split(':').map(str::to_owned).collect::<Vec<_>>())
            .find_map(|v| Self::parse(&v))
            .unwrap_or(Lang::En)
    }

    pub fn parse(s: &str) -> Option<Self> {
        let code = s.split(['_', '.', '@', '-']).next()?.to_ascii_lowercase();
        match code.as_str() {
            "es" => Some(Lang::Es),
            "ca" => Some(Lang::Ca),
            "en" => Some(Lang::En),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Lang::Es => Lang::Ca,
            Lang::Ca => Lang::En,
            Lang::En => Lang::Es,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::Es => "ES",
            Lang::Ca => "CA",
            Lang::En => "EN",
        }
    }

    pub fn t(self) -> &'static Strings {
        match self {
            Lang::Es => &ES,
            Lang::Ca => &CA,
            Lang::En => &EN,
        }
    }

    /// Fecha relativa con plurales correctos por idioma.
    pub fn age(self, secs: u64) -> String {
        let (n, unit) = match secs {
            0..60 => return self.t().now.into(),
            60..3600 => (secs / 60, Unit::Min),
            3600..86400 => (secs / 3600, Unit::Hour),
            86400..172_800 => return self.t().yesterday.into(),
            172_800..2_592_000 => (secs / 86400, Unit::Day),
            2_592_000..31_536_000 => (secs / 2_592_000, Unit::Month),
            _ => (secs / 31_536_000, Unit::Year),
        };
        let one = n == 1;
        match self {
            Lang::Es => {
                let u = match unit {
                    Unit::Min => "min",
                    Unit::Hour => "h",
                    Unit::Day => if one { "día" } else { "días" },
                    Unit::Month => if one { "mes" } else { "meses" },
                    Unit::Year => if one { "año" } else { "años" },
                };
                format!("Hace {n} {u}")
            }
            Lang::Ca => {
                let u = match unit {
                    Unit::Min => "min",
                    Unit::Hour => "h",
                    Unit::Day => if one { "dia" } else { "dies" },
                    Unit::Month => if one { "mes" } else { "mesos" },
                    Unit::Year => if one { "any" } else { "anys" },
                };
                format!("Fa {n} {u}")
            }
            Lang::En => {
                let u = match unit {
                    Unit::Min => "min",
                    Unit::Hour => "h",
                    Unit::Day => if one { "day" } else { "days" },
                    Unit::Month => if one { "month" } else { "months" },
                    Unit::Year => if one { "year" } else { "years" },
                };
                format!("{n} {u} ago")
            }
        }
    }
}

impl Lang {
    /// "Copiando 3 elementos…"
    pub fn busy(self, op: crate::ops::Op, n: usize) -> String {
        use crate::ops::Op::*;
        let items = self.items(n);
        match self {
            Lang::Es => format!("{} {n} {items}…", match op { Copy => "Copiando", Move => "Moviendo", Trash => "Enviando a la papelera", Delete => "Eliminando" }),
            Lang::Ca => format!("{} {n} {items}…", match op { Copy => "S'estan copiant", Move => "S'estan movent", Trash => "S'estan enviant a la paperera", Delete => "S'estan eliminant" }),
            Lang::En => format!("{} {n} {items}…", match op { Copy => "Copying", Move => "Moving", Trash => "Moving to Trash", Delete => "Deleting" }),
        }
    }

    /// "3 elementos copiados"
    pub fn done(self, op: crate::ops::Op, n: usize) -> String {
        use crate::ops::Op::*;
        let one = n == 1;
        let items = self.items(n);
        match self {
            Lang::Es => {
                let v = match op { Copy => "copiado", Move => "movido", Trash => "enviado a la papelera", Delete => "eliminado" };
                let v = if one { v.to_string() } else { v.replacen("do", "dos", 1) };
                format!("{n} {items} {v}")
            }
            Lang::Ca => {
                let v = match (op, one) {
                    (Copy, true) => "copiat", (Copy, false) => "copiats",
                    (Move, true) => "mogut", (Move, false) => "moguts",
                    (Trash, true) => "enviat a la paperera", (Trash, false) => "enviats a la paperera",
                    (Delete, true) => "eliminat", (Delete, false) => "eliminats",
                };
                format!("{n} {items} {v}")
            }
            Lang::En => {
                let v = match op { Copy => "Copied", Move => "Moved", Trash => "Moved to Trash:", Delete => "Deleted" };
                format!("{v} {n} {items}")
            }
        }
    }

    pub fn items(self, n: usize) -> &'static str {
        match (self, n == 1) {
            (Lang::Es, true) => "elemento",
            (Lang::Es, false) => "elementos",
            (Lang::Ca, true) => "element",
            (Lang::Ca, false) => "elements",
            (Lang::En, true) => "item",
            (Lang::En, false) => "items",
        }
    }

    pub fn confirm_delete(self, n: usize) -> String {
        let items = self.items(n);
        match self {
            Lang::Es => format!("¿Eliminar permanentemente {n} {items}?"),
            Lang::Ca => format!("Vols eliminar permanentment {n} {items}?"),
            Lang::En => format!("Permanently delete {n} {items}?"),
        }
    }

    pub fn exists(self, name: &str) -> String {
        match self {
            Lang::Es => format!("Ya existe «{name}»"),
            Lang::Ca => format!("Ja existeix «{name}»"),
            Lang::En => format!("“{name}” already exists"),
        }
    }

    pub fn undo_label(self, k: crate::undo::UndoKind) -> &'static str {
        use crate::undo::UndoKind::*;
        match (self, k) {
            (Lang::Es, Copy) => "copiar",
            (Lang::Es, Move) => "mover",
            (Lang::Es, Rename) => "cambiar nombre",
            (Lang::Es, NewFolder) => "nueva carpeta",
            (Lang::Es, Trash) => "mover a la papelera",
            (Lang::Ca, Copy) => "copiar",
            (Lang::Ca, Move) => "moure",
            (Lang::Ca, Rename) => "canviar el nom",
            (Lang::Ca, NewFolder) => "carpeta nova",
            (Lang::Ca, Trash) => "moure a la paperera",
            (Lang::En, Copy) => "copy",
            (Lang::En, Move) => "move",
            (Lang::En, Rename) => "rename",
            (Lang::En, NewFolder) => "new folder",
            (Lang::En, Trash) => "move to Trash",
        }
    }

    /// "Deshecho: mover a la papelera"
    pub fn undone(self, k: crate::undo::UndoKind) -> String {
        let l = self.undo_label(k);
        match self {
            Lang::Es => format!("Deshecho: {l}"),
            Lang::Ca => format!("Desfet: {l}"),
            Lang::En => format!("Undid {l}"),
        }
    }

    pub fn more_errors(self, n: usize) -> String {
        match self {
            Lang::Es => format!(" (y {n} errores más)"),
            Lang::Ca => format!(" (i {n} errors més)"),
            Lang::En => format!(" (and {n} more errors)"),
        }
    }
}

enum Unit {
    Min,
    Hour,
    Day,
    Month,
    Year,
}

pub struct Strings {
    pub places: &'static str,
    pub home: &'static str,
    pub desktop: &'static str,
    pub documents: &'static str,
    pub downloads: &'static str,
    pub pictures: &'static str,
    pub music: &'static str,
    pub videos: &'static str,
    pub system: &'static str,
    pub hidden: &'static str,
    pub theme_auto: &'static str,
    pub theme_light: &'static str,
    pub theme_dark: &'static str,
    pub col_name: &'static str,
    pub col_size: &'static str,
    pub col_modified: &'static str,
    pub loading: &'static str,
    pub empty: &'static str,
    pub cannot_open: &'static str,
    pub items: &'static str,
    pub hints: &'static str,
    pub now: &'static str,
    pub yesterday: &'static str,
    pub open: &'static str,
    pub cut: &'static str,
    pub copy: &'static str,
    pub paste: &'static str,
    pub rename: &'static str,
    pub trash: &'static str,
    pub delete: &'static str,
    pub new_folder: &'static str,
    pub refresh: &'static str,
    pub select_all: &'static str,
    pub copy_suffix: &'static str,
    pub cancel: &'static str,
    pub confirm_body: &'static str,
    pub nothing_to_paste: &'static str,
    pub invalid_name: &'static str,
    pub into_itself: &'static str,
    pub selected: &'static str,
    pub undo: &'static str,
    pub nothing_to_undo: &'static str,
    pub undoing: &'static str,
}

static ES: Strings = Strings {
    places: "LUGARES",
    home: "Inicio",
    desktop: "Escritorio",
    documents: "Documentos",
    downloads: "Descargas",
    pictures: "Imágenes",
    music: "Música",
    videos: "Vídeos",
    system: "Sistema",
    hidden: "Ocultos",
    theme_auto: "Auto",
    theme_light: "Claro",
    theme_dark: "Oscuro",
    col_name: "Nombre",
    col_size: "Tamaño",
    col_modified: "Modificado",
    loading: "Cargando…",
    empty: "Carpeta vacía",
    cannot_open: "No se puede abrir",
    items: "elementos",
    hints: "⏎ abrir · ⌫ subir · F2 renombrar · Supr papelera · clic derecho: más",
    now: "Ahora",
    yesterday: "Ayer",
    open: "Abrir",
    cut: "Cortar",
    copy: "Copiar",
    paste: "Pegar",
    rename: "Cambiar nombre",
    trash: "Mover a la papelera",
    delete: "Eliminar permanentemente",
    new_folder: "Nueva carpeta",
    refresh: "Actualizar",
    select_all: "Seleccionar todo",
    copy_suffix: "copia",
    cancel: "Cancelar",
    confirm_body: "Esta acción no se puede deshacer.",
    nothing_to_paste: "No hay nada que pegar",
    invalid_name: "Nombre no válido",
    into_itself: "No se puede copiar una carpeta dentro de sí misma",
    selected: "seleccionados",
    undo: "Deshacer",
    nothing_to_undo: "Nada que deshacer",
    undoing: "Deshaciendo…",
};

static CA: Strings = Strings {
    places: "LLOCS",
    home: "Inici",
    desktop: "Escriptori",
    documents: "Documents",
    downloads: "Baixades",
    pictures: "Imatges",
    music: "Música",
    videos: "Vídeos",
    system: "Sistema",
    hidden: "Ocults",
    theme_auto: "Auto",
    theme_light: "Clar",
    theme_dark: "Fosc",
    col_name: "Nom",
    col_size: "Mida",
    col_modified: "Modificat",
    loading: "S'està carregant…",
    empty: "Carpeta buida",
    cannot_open: "No es pot obrir",
    items: "elements",
    hints: "⏎ obrir · ⌫ pujar · F2 reanomena · Supr paperera · clic dret: més",
    now: "Ara",
    yesterday: "Ahir",
    open: "Obre",
    cut: "Retalla",
    copy: "Copia",
    paste: "Enganxa",
    rename: "Canvia el nom",
    trash: "Mou a la paperera",
    delete: "Elimina permanentment",
    new_folder: "Carpeta nova",
    refresh: "Actualitza",
    select_all: "Selecciona-ho tot",
    copy_suffix: "còpia",
    cancel: "Cancel·la",
    confirm_body: "Aquesta acció no es pot desfer.",
    nothing_to_paste: "No hi ha res per enganxar",
    invalid_name: "Nom no vàlid",
    into_itself: "No es pot copiar una carpeta dins d'ella mateixa",
    selected: "seleccionats",
    undo: "Desfés",
    nothing_to_undo: "No hi ha res per desfer",
    undoing: "S'està desfent…",
};

static EN: Strings = Strings {
    places: "PLACES",
    home: "Home",
    desktop: "Desktop",
    documents: "Documents",
    downloads: "Downloads",
    pictures: "Pictures",
    music: "Music",
    videos: "Videos",
    system: "System",
    hidden: "Hidden",
    theme_auto: "Auto",
    theme_light: "Light",
    theme_dark: "Dark",
    col_name: "Name",
    col_size: "Size",
    col_modified: "Modified",
    loading: "Loading…",
    empty: "Empty folder",
    cannot_open: "Can't open",
    items: "items",
    hints: "⏎ open · ⌫ up · F2 rename · Del trash · right-click: more",
    now: "Just now",
    yesterday: "Yesterday",
    open: "Open",
    cut: "Cut",
    copy: "Copy",
    paste: "Paste",
    rename: "Rename",
    trash: "Move to Trash",
    delete: "Delete Permanently",
    new_folder: "New Folder",
    refresh: "Refresh",
    select_all: "Select All",
    copy_suffix: "copy",
    cancel: "Cancel",
    confirm_body: "This can't be undone.",
    nothing_to_paste: "Nothing to paste",
    invalid_name: "Invalid name",
    into_itself: "Can't copy a folder into itself",
    selected: "selected",
    undo: "Undo",
    nothing_to_undo: "Nothing to undo",
    undoing: "Undoing…",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_locales() {
        assert!(Lang::parse("ca_ES.UTF-8") == Some(Lang::Ca));
        assert!(Lang::parse("es_ES@euro") == Some(Lang::Es));
        assert!(Lang::parse("en-US") == Some(Lang::En));
        assert!(Lang::parse("C.UTF-8").is_none());
    }

    #[test]
    fn op_messages() {
        use crate::ops::Op;
        assert_eq!(Lang::Es.done(Op::Copy, 3), "3 elementos copiados");
        assert_eq!(Lang::Es.done(Op::Trash, 2), "2 elementos enviados a la papelera");
        assert_eq!(Lang::Es.done(Op::Move, 1), "1 elemento movido");
        assert_eq!(Lang::Ca.done(Op::Move, 2), "2 elements moguts");
        assert_eq!(Lang::En.done(Op::Delete, 1), "Deleted 1 item");
        assert_eq!(Lang::Ca.busy(Op::Copy, 4), "S'estan copiant 4 elements…");
    }

    #[test]
    fn plurals() {
        assert_eq!(Lang::Es.age(31_536_000), "Hace 1 año");
        assert_eq!(Lang::Ca.age(3 * 2_592_000), "Fa 3 mesos");
        assert_eq!(Lang::En.age(5 * 86400), "5 days ago");
        assert_eq!(Lang::Ca.age(100_000), "Ahir");
    }
}
