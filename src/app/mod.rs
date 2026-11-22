//! Estado de la aplicación y su lógica. El render vive en [`view`].
//!
//! Mapa:
//! - [`commands`]: registro de acciones (teclado, menús, barra, paleta).
//! - [`selection`]: cursor y selección múltiple, sin GPUI.
//! - [`navigation`]: listar, refrescar, ordenar, ir a rutas.
//! - [`file_ops`]: copiar, pegar, papelera, renombrar, descargar…
//! - [`jobs`]: trabajos en segundo plano, avisos y pila de deshacer.
//! - [`view`]: pintado con GPUI.

pub mod commands;
mod file_ops;
mod jobs;
mod navigation;
pub mod selection;
mod view;

use std::{
    collections::HashSet,
    ffi::OsString,
    path::PathBuf,
    time::Duration,
};

use futures::{StreamExt, channel::mpsc};
use gpui::{AppContext, Context, FocusHandle, KeyDownEvent, Pixels, Point, Subscription, Task, UniformListScrollHandle, Window};

use crate::{
    backend::{Capabilities, Location},
    fs::{self, Entry, Place, SortColumn},
    i18n::Lang,
    input::{InputResult, LineInput},
    keymap::Platform,
    ops::Op,
    sidebar,
    theme::Mode,
    watch::DirWatcher,
};
pub use jobs::Jobs;
use selection::Selection;

/// Portapapeles interno. `text` es lo que escribimos en el del sistema: si al pegar
/// sigue ahí, usamos esto (conserva copiar/cortar y rutas no UTF-8).
pub struct Clip {
    pub paths: Vec<PathBuf>,
    pub set: HashSet<PathBuf>,
    pub op: Op,
    pub text: String,
}

pub enum LocationInput {
    Local(LineInput),
    S3Profile(LineInput),
    DownloadDestination(LineInput, DownloadRequest),
}

impl LocationInput {
    fn input(&mut self) -> &mut LineInput {
        match self {
            Self::Local(input) | Self::S3Profile(input) | Self::DownloadDestination(input, _) => input,
        }
    }
}

pub struct DownloadRequest {
    pub source: Location,
    pub keys: Vec<String>,
}

pub struct Rename {
    pub original: OsString,
    pub input: LineInput,
}

/// Filas visibles en la paleta.
pub const PALETTE_ROWS: usize = 12;

pub struct Palette {
    pub input: LineInput,
    pub selected: usize,
}

/// Lo que captura el teclado. Solo puede haber uno a la vez: el tipo impide estados imposibles
/// como renombrar con el diálogo de borrado abierto.
#[derive(Default)]
pub enum Modal {
    #[default]
    None,
    Rename(Rename),
    Location(LocationInput),
    Palette(Palette),
    ConfirmDelete(Vec<PathBuf>),
    Errors(Vec<String>),
}

/// Menú contextual. No es modal: el teclado sigue funcionando y cualquier comando lo cierra.
pub struct Menu {
    pub pos: Point<Pixels>,
    pub on_item: bool,
}

/// Barra lateral: lugares fijos, carpetas fijadas y discos.
pub struct Places {
    pub fixed: Vec<Place>,
    /// `None` hasta cargarlas del disco.
    pub pins: Option<Vec<PathBuf>>,
    pub disks: Vec<PathBuf>,
    pub pin_busy: bool,
}

pub struct Explorer {
    // ------------------------------------------------------------ ubicación
    pub source: Location,
    /// Último directorio local; se conserva al navegar por otro backend.
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub entries: Vec<Entry>,
    pub show_hidden: bool,
    pub sort_column: SortColumn,
    pub sort_descending: bool,
    pub loading: bool,
    pub error: Option<String>,
    /// Sube con cada carga: los resultados de cargas anteriores se descartan.
    generation: u64,
    pub status_count: Option<(usize, f64)>,

    // ------------------------------------------------------------ interacción
    pub selection: Selection,
    pub modal: Modal,
    pub menu: Option<Menu>,
    pub clip: Option<Clip>,
    /// Carpeta recién creada que debe entrar en modo renombrar al aparecer en el listado.
    pending_rename: Option<OsString>,

    // ------------------------------------------------------------ resto
    pub places: Places,
    pub jobs: Jobs,
    pub mode: Mode,
    pub lang: Lang,

    watcher: Option<DirWatcher>,
    watch_tx: mpsc::UnboundedSender<()>,

    pub focus: FocusHandle,
    pub scroll: UniformListScrollHandle,
    _list_task: Option<Task<()>>,
    _stat_task: Option<Task<()>>,
    download_prompt: Option<Task<()>>,
    _watch_task: Task<()>,
    _sidebar_task: Task<()>,
    _appearance: Subscription,
}

impl Explorer {
    pub fn new(cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Re-pinta cuando el sistema cambia entre claro y oscuro (portal color-scheme).
        let appearance = cx.observe_window_appearance(window, |_, _, cx| cx.notify());

        // Eventos del watcher nativo → refresco con debounce y como mucho ~2.5/s.
        let (watch_tx, mut watch_rx) = mpsc::unbounded::<()>();
        let watch_task = cx.spawn(async move |this, cx| {
            while watch_rx.next().await.is_some() {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                while watch_rx.try_recv().is_ok() {}
                let updated = this.update(cx, |this, cx| {
                    // Hasta que termine la navegación, el watcher sigue en la ruta anterior.
                    if this.caps().watch && !this.loading {
                        this.reload(Vec::new(), cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(250)).await;
            }
        });
        let sidebar_task = cx.spawn(async move |this, cx| {
            let pins = cx.background_spawn(async { sidebar::load_pins().unwrap_or_default() }).await;
            if this
                .update(cx, |this, cx| {
                    this.places.pins = Some(pins);
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            loop {
                let disks = cx.background_spawn(async { sidebar::mounted_disks() }).await;
                if this
                    .update(cx, |this, cx| {
                        if let Ok(disks) = disks
                            && this.places.disks != disks
                        {
                            this.places.disks = disks;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(3)).await;
            }
        });

        let mut this = Self {
            source: Location::Local(PathBuf::new()),
            cwd: PathBuf::new(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            entries: Vec::new(),
            show_hidden: false,
            sort_column: SortColumn::Name,
            sort_descending: false,
            loading: false,
            error: None,
            generation: 0,
            status_count: None,
            selection: Selection::default(),
            modal: Modal::None,
            menu: None,
            clip: None,
            pending_rename: None,
            places: Places { fixed: fs::places(), pins: None, disks: Vec::new(), pin_busy: false },
            jobs: Jobs::default(),
            mode: Mode::System,
            lang: Lang::detect(),
            watcher: None,
            watch_tx,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _list_task: None,
            _stat_task: None,
            download_prompt: None,
            _watch_task: watch_task,
            _sidebar_task: sidebar_task,
            _appearance: appearance,
        };
        this.navigate(cwd, None, cx);
        this
    }

    // -------------------------------------------------------- consultas ----

    pub fn caps(&self) -> Capabilities {
        self.source.capabilities()
    }

    pub fn cursor_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selection.cursor)
    }

    pub fn target_names(&self) -> Vec<OsString> {
        self.selection.targets(&self.entries)
    }

    /// Rutas locales afectadas por una acción. Vacío fuera del sistema de ficheros local.
    pub fn target_paths(&self) -> Vec<PathBuf> {
        if !self.source.is_local() {
            return Vec::new();
        }
        self.target_names().into_iter().map(|n| self.cwd.join(n)).collect()
    }

    pub fn download_keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().enumerate().filter_map(|(i, entry)| {
            if self.selection.is_selected(i, &entry.name) { self.source.download_key(entry) } else { None }
        })
    }

    pub fn is_pinned(&self) -> bool {
        self.places.pins.as_ref().is_some_and(|pins| pins.contains(&self.cwd))
    }

    pub fn is_busy(&self) -> bool {
        !self.jobs.busy.is_empty()
    }

    // ------------------------------------------------------- preferencias ----

    pub fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.reload(Vec::new(), cx);
    }

    pub fn cycle_lang(&mut self, cx: &mut Context<Self>) {
        self.lang = self.lang.next();
        cx.notify();
    }

    pub fn cycle_theme(&mut self, cx: &mut Context<Self>) {
        self.mode = self.mode.next();
        cx.notify();
    }

    pub fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        self.set_pinned(self.cwd.clone(), !self.is_pinned(), cx);
    }

    pub fn set_pinned(&mut self, path: PathBuf, pinned: bool, cx: &mut Context<Self>) {
        if self.places.pin_busy {
            return;
        }
        self.places.pin_busy = true;
        let task = cx.background_spawn(async move { sidebar::set_pinned(&path, pinned) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.places.pin_busy = false;
                match result {
                    Ok(pins) => this.places.pins = Some(pins),
                    Err(error) => {
                        let message = format!("{}: {error}", this.lang.t().pin_error);
                        this.set_notice(jobs::Notice::Error(message), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ------------------------------------------------------- selección ----

    pub fn move_cursor(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        self.selection.move_by(delta, extend, &self.entries);
        self.scroll.scroll_to_item(self.selection.cursor, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection.select_all(&self.entries);
        cx.notify();
    }

    pub fn escape(&mut self, cx: &mut Context<Self>) {
        self.selection.clear_marks();
        cx.notify();
    }

    pub fn click_row(&mut self, ix: usize, ev: &gpui::ClickEvent, cx: &mut Context<Self>) {
        let Some(name) = self.entries.get(ix).map(|e| e.name.clone()) else { return };
        if let Modal::Rename(r) = &self.modal {
            if r.original == name {
                return; // clic dentro del campo de texto
            }
            self.commit_rename(cx);
        }
        let m = ev.modifiers();
        if Platform::CURRENT.toggle_click(&m) {
            self.selection.toggle(ix, &self.entries);
        } else if m.shift {
            self.selection.extend_to(ix, &self.entries);
        } else {
            self.selection.select_only(ix);
            if ev.click_count() >= 2 {
                self.open_cursor(cx);
            }
        }
        cx.notify();
    }

    pub fn right_click_row(&mut self, ix: usize, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(name) = self.entries.get(ix).map(|e| e.name.clone()) else { return };
        if !self.selection.is_selected(ix, &name) {
            self.selection.select_only(ix);
        }
        self.menu = Some(Menu { pos, on_item: true });
        cx.notify();
    }

    // ------------------------------------------------------- paleta ----

    pub fn open_palette(&mut self, cx: &mut Context<Self>) {
        self.modal = Modal::Palette(Palette { input: LineInput::new(String::new(), 0), selected: 0 });
        cx.notify();
    }

    /// Comandos disponibles que casan con lo escrito, mejor puntuados primero.
    pub fn palette_matches(&self, query: &str) -> Vec<&'static commands::Command> {
        let s = self.lang.t();
        let mut matches: Vec<_> = commands::COMMANDS
            .iter()
            .filter(|c| c.palette && (c.when)(self))
            .filter_map(|c| commands::fuzzy_score((c.label)(s), query).map(|score| (score, c)))
            .collect();
        matches.sort_by(|a, b| b.0.cmp(&a.0));
        matches.into_iter().map(|(_, c)| c).collect()
    }

    fn palette_key(&mut self, key: &str, ch: Option<&str>, ctrl: bool, cx: &mut Context<Self>) {
        let Modal::Palette(palette) = &mut self.modal else { return };
        match key {
            "down" => palette.selected += 1,
            "up" => palette.selected = palette.selected.saturating_sub(1),
            _ => match palette.input.key(key, ch, ctrl) {
                InputResult::Commit => {
                    let query = palette.input.text.clone();
                    let selected = palette.selected;
                    self.modal = Modal::None;
                    if let Some(command) = self.palette_matches(&query).get(selected).copied() {
                        commands::run(self, command, cx);
                    }
                    return;
                }
                InputResult::Cancel => {
                    self.modal = Modal::None;
                    return;
                }
                InputResult::Continue => palette.selected = 0,
            },
        }
        let query = palette.input.text.clone();
        let count = self.palette_matches(&query).len().min(PALETTE_ROWS);
        if let Modal::Palette(palette) = &mut self.modal {
            palette.selected = palette.selected.min(count.saturating_sub(1));
        }
    }

    // ---------------------------------------------------------- teclado ----

    fn on_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        let key = k.key.as_str();
        let command = Platform::CURRENT.command(m);
        let ch = k.key_char.as_deref();

        // Lo modal captura el teclado antes que los comandos.
        match &mut self.modal {
            Modal::None => {}
            Modal::Errors(_) => {
                if key == "escape" || key == "enter" {
                    self.modal = Modal::None;
                }
                cx.notify();
                return;
            }
            Modal::ConfirmDelete(_) => {
                match key {
                    "enter" => self.confirm_delete(cx),
                    "escape" => self.modal = Modal::None,
                    _ => {}
                }
                cx.notify();
                return;
            }
            Modal::Palette(palette) => {
                if command && key == "v" {
                    paste_line(&mut palette.input, cx);
                } else if command && key == "a" {
                    palette.input.select_all();
                } else {
                    self.palette_key(key, ch, command || m.control, cx);
                }
                cx.notify();
                return;
            }
            Modal::Location(location) => {
                let input = location.input();
                match (command, key) {
                    (true, "v") => paste_line(input, cx),
                    (true, "a" | "l") => input.select_all(),
                    _ => match input.key(key, ch, command || m.control) {
                        InputResult::Commit => self.commit_location(cx),
                        InputResult::Cancel => self.modal = Modal::None,
                        InputResult::Continue => {}
                    },
                }
                cx.notify();
                return;
            }
            Modal::Rename(r) => {
                match (command, key) {
                    (true, "v") => paste_line(&mut r.input, cx),
                    (true, "a") => r.input.select_all(),
                    _ => match r.input.key(key, ch, m.control) {
                        InputResult::Commit => self.commit_rename(cx),
                        InputResult::Cancel => self.modal = Modal::None,
                        InputResult::Continue => {}
                    },
                }
                cx.notify();
                return;
            }
        }

        if key == "escape" && self.menu.take().is_some() {
            cx.notify();
            return;
        }
        commands::dispatch(self, k, cx);
    }
}

/// Pega la primera línea del portapapeles en un campo de texto.
fn paste_line(input: &mut LineInput, cx: &mut Context<Explorer>) {
    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
        input.insert(text.lines().next().unwrap_or(""));
    }
}
