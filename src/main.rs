//! fex — POC de explorador de archivos con GPUI.
//!
//! Reglas para llegar al primer frame rápido:
//! - La ventana se abre y pinta ANTES de leer el directorio.
//! - El listado usa solo `getdents` (d_type vía `file_type()`), sin `stat`.
//! - Tamaños y fechas se rellenan después, en segundo plano.
//! - El watcher se crea tras el primer listado.
//! - Nada de gvfs, temas de iconos ni miniaturas.

mod fs;
mod i18n;
mod icons;
mod input;
#[cfg(target_os = "macos")]
mod macos;
mod ops;
mod safe_fs;
mod sidebar;
mod theme;
mod undo;
mod watch;

#[cfg(all(test, feature = "ui-tests", target_os = "linux"))]
mod ui_tests;

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, ClipboardEntry, ClipboardItem, Context, Div, FocusHandle, FontFeatures,
    FontWeight, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Role, ScrollStrategy, SharedString, Stateful,
    Subscription, Task, Toggled, UniformListScrollHandle, Window, WindowBounds, WindowOptions, anchored, deferred, div,
    prelude::*, px, relative, size, uniform_list, white,
};
use gpui_platform::application;

use fs::{Entry, Kind, Place, SortColumn};
use i18n::Lang;
use icons::{Assets, Icon};
use input::{InputResult, LineInput};
use ops::{NameError, Op, OpError};
use theme::{Mode, Theme, alpha};
use undo::{MAX_UNDO, Undo, UndoKind};
use watch::DirWatcher;

static T0: OnceLock<Instant> = OnceLock::new();

fn since_start_ms() -> f64 {
    T0.get().map(|t| t.elapsed()).unwrap_or_default().as_secs_f64() * 1e3
}

const ROW_H: f32 = 30.;
const FILE_TILE_W: f32 = 20.;
const SIDEBAR_W: f32 = 184.;
const COL_SIZE_W: f32 = 84.;
const COL_DATE_W: f32 = 118.;
const NOTICE_TTL: Duration = Duration::from_secs(5);

fn shortcut(linux: &'static str, macos: &'static str) -> &'static str {
    if cfg!(target_os = "macos") { macos } else { linux }
}

// ----------------------------------------------------------------- estado ---

/// Portapapeles interno. `text` es lo que escribimos en el del sistema: si al pegar
/// sigue ahí, usamos esto (conserva copiar/cortar y rutas no UTF-8).
struct Clip {
    paths: Vec<PathBuf>,
    set: HashSet<PathBuf>,
    op: Op,
    text: String,
}

struct Rename {
    original: OsString,
    input: LineInput,
}

struct Menu {
    pos: Point<Pixels>,
    on_item: bool,
}

struct ToolbarTooltip {
    label: &'static str,
    shortcut: &'static str,
    theme: Theme,
}

impl Render for ToolbarTooltip {
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
                d.child(div().px_1().rounded_sm().bg(t.control).text_color(t.muted).child(self.shortcut))
            })
    }
}

/// Trabajo en background: una operación o un deshacer.
#[derive(Clone, Copy)]
enum Job {
    Op(Op),
    Undo(UndoKind),
}

enum Notice {
    Done(Op, usize),
    Undone(UndoKind),
    /// Informativo (gris): "Nada que deshacer".
    Info(&'static str),
    Error(String),
}

type Action = fn(&mut Explorer, &mut Context<Explorer>);

struct Explorer {
    cwd: PathBuf,
    home: Option<PathBuf>,
    places: Vec<Place>,
    pins: Option<Vec<PathBuf>>,
    disks: Vec<PathBuf>,
    pin_busy: bool,
    entries: Vec<Entry>,
    /// Fila con el foco de teclado.
    cursor: usize,
    /// Origen de la selección por rango (Shift).
    anchor: usize,
    /// Selección múltiple por nombre (sobrevive a los refrescos). Vacía = solo `cursor`.
    marked: HashSet<OsString>,
    show_hidden: bool,
    mode: Mode,
    lang: Lang,
    loading: bool,
    error: Option<String>,
    generation: u64,
    status_count: Option<(usize, f64)>,

    clip: Option<Clip>,
    renaming: Option<Rename>,
    location: Option<LineInput>,
    sort_column: SortColumn,
    sort_descending: bool,
    pending_rename: Option<OsString>,
    menu: Option<Menu>,
    confirm: Option<Vec<PathBuf>>,
    busy: Vec<(u64, Job, usize)>,
    undo: Vec<Undo>,
    next_job: u64,
    progress: Arc<AtomicU64>,
    operation_errors: Option<Vec<String>>,
    retry_paste: Option<ops::Retry>,
    notice: Option<Notice>,

    watcher: Option<DirWatcher>,
    watch_tx: mpsc::UnboundedSender<()>,

    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    _list_task: Option<Task<()>>,
    _stat_task: Option<Task<()>>,
    _notice_task: Option<Task<()>>,
    _watch_task: Task<()>,
    _sidebar_task: Task<()>,
    _appearance: Subscription,
}

impl Explorer {
    fn new(cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
                    if !this.loading {
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
                    this.pins = Some(pins);
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
                            && this.disks != disks
                        {
                            this.disks = disks;
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
            cwd: PathBuf::new(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            places: fs::places(),
            pins: None,
            disks: Vec::new(),
            pin_busy: false,
            entries: Vec::new(),
            cursor: 0,
            anchor: 0,
            marked: HashSet::new(),
            show_hidden: false,
            mode: Mode::System,
            lang: Lang::detect(),
            loading: false,
            error: None,
            generation: 0,
            status_count: None,
            clip: None,
            renaming: None,
            location: None,
            sort_column: SortColumn::Name,
            sort_descending: false,
            pending_rename: None,
            menu: None,
            confirm: None,
            busy: Vec::new(),
            undo: Vec::new(),
            next_job: 0,
            progress: Arc::new(AtomicU64::new(0)),
            operation_errors: None,
            retry_paste: None,
            notice: None,
            watcher: None,
            watch_tx,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _list_task: None,
            _stat_task: None,
            _notice_task: None,
            _watch_task: watch_task,
            _sidebar_task: sidebar_task,
            _appearance: appearance,
        };
        this.navigate(cwd, None, cx);
        this
    }

    // ------------------------------------------------------ navegación ----

    fn navigate(&mut self, dir: PathBuf, select: Option<OsString>, cx: &mut Context<Self>) {
        let refresh = dir == self.cwd;
        if !refresh {
            // Las filas y las acciones pendientes pertenecen al directorio anterior.
            self.entries.clear();
            self.cursor = 0;
            self.anchor = 0;
            self.marked.clear();
            self.status_count = None;
            self.renaming = None;
            self.pending_rename = None;
            self.confirm = None;
        }
        self.menu = None;
        self.load(dir, refresh, select.into_iter().collect(), cx);
    }

    /// Relee el directorio actual sin perder selección ni scroll.
    /// `select` no vacío → esos nombres quedan seleccionados (tras pegar, renombrar…).
    fn reload(&mut self, select: Vec<OsString>, cx: &mut Context<Self>) {
        self.load(self.cwd.clone(), true, select, cx);
    }

    fn load(&mut self, dir: PathBuf, refresh: bool, select: Vec<OsString>, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.cwd = dir.clone();
        if !refresh {
            self.loading = true;
            self.error = None;
        }
        self._stat_task = None;
        cx.notify();

        let show_hidden = self.show_hidden;
        let started = Instant::now();
        let listing = cx.background_spawn(async move { fs::list_dir(&dir, show_hidden) });

        self._list_task = Some(cx.spawn(async move |this, cx| {
            let result = listing.await;
            let took = started.elapsed();
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return; // hay una carga más reciente
                }
                this.loading = false;
                match result {
                    Ok(entries) => this.apply_listing(entries, refresh, select, took, generation, cx),
                    Err(e) if refresh && e.kind() == io::ErrorKind::NotFound => {
                        // Han borrado el directorio actual: subir al primer ancestro que exista.
                        let up = this.cwd.ancestors().skip(1).find(|p| p.is_dir()).map(Path::to_path_buf);
                        if let Some(up) = up {
                            this.navigate(up, None, cx);
                        }
                    }
                    Err(e) => {
                        this.entries.clear();
                        this.marked.clear();
                        this.error = Some(e.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn apply_listing(
        &mut self,
        entries: Vec<Entry>,
        refresh: bool,
        select: Vec<OsString>,
        took: Duration,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let prev_cursor = self.entries.get(self.cursor).map(|e| e.name.clone());
        self.entries = entries;
        fs::sort_entries(&mut self.entries, self.sort_column, self.sort_descending);
        self.error = None;
        let pos = |name: &OsStr, entries: &[Entry]| entries.iter().position(|e| e.name == name);

        if refresh {
            if select.is_empty() {
                if !self.marked.is_empty() {
                    let names: HashSet<&OsStr> = self.entries.iter().map(|e| e.name.as_os_str()).collect();
                    self.marked.retain(|n| names.contains(n.as_os_str()));
                }
            } else {
                self.marked = select.iter().filter(|n| pos(n, &self.entries).is_some()).cloned().collect();
                if self.marked.len() <= 1 {
                    self.marked.clear(); // uno solo = cursor
                }
            }
            let target = select.first().cloned().or(prev_cursor);
            self.cursor = target
                .and_then(|n| pos(&n, &self.entries))
                .unwrap_or_else(|| self.cursor.min(self.entries.len().saturating_sub(1)));
            if !select.is_empty() {
                self.scroll.scroll_to_item(self.cursor, ScrollStrategy::Nearest);
            }
        } else {
            self.marked.clear();
            self.cursor = select.first().and_then(|n| pos(n, &self.entries)).unwrap_or(0);
            self.scroll.scroll_to_item(self.cursor, ScrollStrategy::Center);
        }
        self.anchor = self.cursor;

        if let Some(name) = self.pending_rename.take()
            && let Some(ix) = pos(&name, &self.entries)
        {
            self.cursor = ix;
            let mut input = LineInput::new(name.to_string_lossy().into_owned(), 0);
            input.select_all();
            self.renaming = Some(Rename { original: name, input });
        }
        if let Some(r) = &self.renaming
            && pos(&r.original, &self.entries).is_none()
        {
            self.renaming = None;
        }

        self.status_count = Some((self.entries.len(), took.as_secs_f64() * 1e3));
        self.watch_cwd();
        self.start_stat_pass(generation, cx);
    }

    fn watch_cwd(&mut self) {
        if self.watcher.is_none() {
            self.watcher = DirWatcher::new(self.watch_tx.clone()).ok();
        }
        if let Some(w) = &mut self.watcher {
            w.watch(&self.cwd);
        }
    }

    fn start_stat_pass(&mut self, generation: u64, cx: &mut Context<Self>) {
        let dir = self.cwd.clone();
        let names: Vec<OsString> = self.entries.iter().map(|e| e.name.clone()).collect();
        let stats = cx.background_spawn(async move { fs::stat_all(&dir, names) });
        self._stat_task = Some(cx.spawn(async move |this, cx| {
            let stats = stats.await;
            this.update(cx, |this, cx| {
                if this.generation != generation || stats.len() != this.entries.len() {
                    return;
                }
                let stats: HashMap<_, _> =
                    stats.into_iter().map(|(name, size, modified)| (name, (size, modified))).collect();
                for entry in &mut this.entries {
                    if let Some(&(size, modified)) = stats.get(&entry.name) {
                        entry.size = size;
                        entry.modified = modified;
                    }
                }
                this.sort_listing();
                cx.notify();
            })
            .ok();
        }));
    }

    fn sort_listing(&mut self) {
        let selected = self.entries.get(self.cursor).map(|entry| entry.name.clone());
        fs::sort_entries(&mut self.entries, self.sort_column, self.sort_descending);
        self.cursor = selected.and_then(|name| self.entries.iter().position(|entry| entry.name == name)).unwrap_or(0);
        self.anchor = self.cursor;
    }

    fn change_sort(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        if self.sort_column == column {
            self.sort_descending = !self.sort_descending;
        } else {
            self.sort_column = column;
            self.sort_descending = false;
        }
        self.sort_listing();
        self.scroll.scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn edit_location(&mut self, cx: &mut Context<Self>) {
        let mut input = LineInput::new(self.cwd.to_string_lossy().into_owned(), 0);
        input.select_all();
        self.location = Some(input);
        self.renaming = None;
        self.menu = None;
        cx.notify();
    }

    fn commit_location(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.location.take() else { return };
        let path = if input.text == "~" {
            self.home.clone().unwrap_or_else(|| self.cwd.clone())
        } else if let Some(suffix) = input.text.strip_prefix("~/") {
            self.home.as_ref().unwrap_or(&self.cwd).join(suffix)
        } else {
            self.cwd.join(&input.text)
        };
        self.navigate(path, None, cx);
    }

    fn open_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(self.cursor) else { return };
        let path = self.cwd.join(&entry.name);
        if entry.kind.is_dir() {
            self.navigate(path, None, cx);
        } else {
            // En un hilo aparte para recoger el proceso (sin zombis) y no bloquear.
            std::thread::spawn(move || {
                let opener = if cfg!(target_os = "macos") { "/usr/bin/open" } else { "xdg-open" };
                let _ = std::process::Command::new(opener).arg(path).status();
            });
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else { return };
        let came_from = self.cwd.file_name().map(OsStr::to_os_string);
        self.navigate(parent, came_from, cx);
    }

    fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.reload(Vec::new(), cx);
    }

    fn cycle_lang(&mut self, cx: &mut Context<Self>) {
        self.lang = self.lang.next();
        cx.notify();
    }

    fn cycle_theme(&mut self, cx: &mut Context<Self>) {
        self.mode = self.mode.next();
        cx.notify();
    }

    fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        let Some(pins) = &self.pins else { return };
        let pinned = !pins.contains(&self.cwd);
        self.set_pinned(self.cwd.clone(), pinned, cx);
    }

    fn set_pinned(&mut self, path: PathBuf, pinned: bool, cx: &mut Context<Self>) {
        if self.pin_busy {
            return;
        }
        self.pin_busy = true;
        let task = cx.background_spawn(async move { sidebar::set_pinned(&path, pinned) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.pin_busy = false;
                match result {
                    Ok(pins) => this.pins = Some(pins),
                    Err(error) => this.set_notice(Notice::Error(format!("{}: {error}", this.lang.t().pin_error)), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ------------------------------------------------------- selección ----

    fn is_selected(&self, ix: usize, name: &OsStr) -> bool {
        if self.marked.is_empty() { ix == self.cursor } else { self.marked.contains(name) }
    }

    fn target_names(&self) -> Vec<OsString> {
        if self.marked.is_empty() {
            self.entries.get(self.cursor).map(|e| vec![e.name.clone()]).unwrap_or_default()
        } else {
            // En orden de listado, no de inserción en el HashSet.
            self.entries.iter().filter(|e| self.marked.contains(&e.name)).map(|e| e.name.clone()).collect()
        }
    }

    fn target_paths(&self) -> Vec<PathBuf> {
        self.target_names().into_iter().map(|n| self.cwd.join(n)).collect()
    }

    fn seed_marks(&mut self) {
        if self.marked.is_empty()
            && let Some(e) = self.entries.get(self.cursor)
        {
            self.marked.insert(e.name.clone());
        }
    }

    fn mark_range(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        self.marked = self.entries[lo..=hi.min(self.entries.len() - 1)].iter().map(|e| e.name.clone()).collect();
    }

    fn move_cursor(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as isize - 1;
        self.cursor = (self.cursor as isize).saturating_add(delta).clamp(0, last) as usize;
        if extend {
            self.mark_range(self.anchor, self.cursor);
        } else {
            self.marked.clear();
            self.anchor = self.cursor;
        }
        self.scroll.scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.marked = self.entries.iter().map(|e| e.name.clone()).collect();
        cx.notify();
    }

    fn click_row(&mut self, ix: usize, ev: &ClickEvent, cx: &mut Context<Self>) {
        let Some(name) = self.entries.get(ix).map(|e| e.name.clone()) else { return };
        if let Some(r) = &self.renaming {
            if r.original == name {
                return; // click dentro del campo de texto
            }
            self.commit_rename(cx);
        }
        let m = ev.modifiers();
        if if cfg!(target_os = "macos") { m.platform } else { m.control } {
            self.seed_marks();
            if !self.marked.remove(&name) {
                self.marked.insert(name);
            }
            self.cursor = ix;
            self.anchor = ix;
        } else if m.shift {
            self.cursor = ix;
            self.mark_range(self.anchor, ix);
        } else {
            self.marked.clear();
            self.cursor = ix;
            self.anchor = ix;
            if ev.click_count() >= 2 {
                self.open_cursor(cx);
            }
        }
        cx.notify();
    }

    fn right_click_row(&mut self, ix: usize, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(name) = self.entries.get(ix).map(|e| e.name.clone()) else { return };
        if !self.is_selected(ix, &name) {
            self.marked.clear();
            self.cursor = ix;
            self.anchor = ix;
        }
        self.menu = Some(Menu { pos, on_item: true });
        cx.notify();
    }

    // ------------------------------------------------------ operaciones ----

    fn set_notice(&mut self, notice: Notice, cx: &mut Context<Self>) {
        let is_error = matches!(notice, Notice::Error(_));
        self.notice = Some(notice);
        if is_error {
            self._notice_task = None;
            cx.notify();
            return;
        }
        self._notice_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_TTL).await;
            this.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn describe_errors(&self, errors: &[OpError]) -> String {
        let s = self.lang.t();
        let first = match &errors[0] {
            OpError::IntoItself(n) => format!("{n}: {}", s.into_itself),
            OpError::Exists(n) => self.lang.exists(n),
            OpError::Io(n, e) => format!("{n}: {e}"),
            OpError::Trash(n, e) => format!("{n}: {e}"),
        };
        if errors.len() > 1 { first + &self.lang.more_errors(errors.len() - 1) } else { first }
    }

    fn push_undo(&mut self, u: Undo) {
        self.undo.push(u);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }

    /// Lanza un trabajo en background; al terminar avisa, apila su deshacer (si lo tiene)
    /// y refresca seleccionando el resultado.
    fn run_job(
        &mut self,
        job: Job,
        n: usize,
        work: impl FnOnce() -> ops::Report + Send + 'static,
        make_undo: impl FnOnce(&ops::Report) -> Option<Undo> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if !self.busy.is_empty() {
            return;
        }
        let cwd = self.cwd.clone();
        let id = self.next_job;
        self.next_job += 1;
        self.progress.store(0, Ordering::Relaxed);
        self.busy.push((id, job, n));
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let active = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.busy.iter().any(|(job_id, ..)| *job_id == id)
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
        self.menu = None;
        cx.notify();

        let task = cx.background_spawn(async move {
            let mut report = work();
            let undo = match make_undo(&report).map(Undo::guard) {
                Some(Ok(undo)) => Some(undo),
                Some(Err(error)) => {
                    report.errors.push(OpError::Io("Undo".into(), error));
                    None
                }
                None => None,
            };
            (report, undo)
        });
        cx.spawn(async move |this, cx| {
            let (report, undo) = task.await;
            this.update(cx, |this, cx| {
                this.busy.retain(|(i, ..)| *i != id);
                if let Some(u) = undo {
                    this.push_undo(u);
                }
                this.retry_paste = report.retry.clone();
                if !report.errors.is_empty() {
                    let mut details = vec![match job {
                        Job::Op(op) => this.lang.done(op, report.done),
                        Job::Undo(_) => this.lang.t().undo.into(),
                    }];
                    details.extend(report.errors.iter().map(|error| this.describe_errors(std::slice::from_ref(error))));
                    this.operation_errors = Some(details);
                }
                let notice = match (report.errors.is_empty(), job) {
                    (false, _) => Notice::Error(this.describe_errors(&report.errors)),
                    (true, Job::Op(op)) => Notice::Done(op, report.done),
                    (true, Job::Undo(k)) => Notice::Undone(k),
                };
                this.set_notice(notice, cx);
                let selected = report
                    .created_paths
                    .iter()
                    .filter(|path| path.parent() == Some(this.cwd.as_path()))
                    .filter_map(|path| path.file_name().map(OsStr::to_os_string))
                    .collect::<Vec<_>>();
                if this.cwd == cwd || !selected.is_empty() {
                    this.reload(selected, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn undo_last(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if !self.busy.is_empty() {
            return;
        }
        let Some(u) = self.undo.pop() else {
            self.set_notice(Notice::Info(self.lang.t().nothing_to_undo), cx);
            return;
        };
        self.clip = None; // las rutas del portapapeles pueden dejar de ser válidas
        let kind = u.kind();
        let retry = u.clone();
        self.run_job(
            Job::Undo(kind),
            1,
            move || u.revert(),
            move |report| (report.done == 0 && !report.errors.is_empty()).then_some(retry),
            cx,
        );
    }

    fn copy_or_cut(&mut self, op: Op, cx: &mut Context<Self>) {
        let paths = self.target_paths();
        if paths.is_empty() {
            return;
        }
        // Rutas en texto plano: útil al pegar en una terminal o editor.
        let text = paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.clip = Some(Clip { set: paths.iter().cloned().collect(), paths, op, text });
        self.menu = None;
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if !self.busy.is_empty() {
            return;
        }
        let system = cx.read_from_clipboard();
        let paths = system.as_ref().and_then(|clipboard| {
            clipboard.entries.iter().find_map(|entry| match entry {
                ClipboardEntry::ExternalPaths(paths) if !paths.paths().is_empty() => Some(paths.paths().to_vec()),
                _ => None,
            })
        });
        let parsed = paths.map(|paths| (paths, Op::Copy)).or_else(|| {
            match (&self.clip, system.and_then(|clipboard| clipboard.text())) {
                (Some(c), Some(t)) if t == c.text => Some((c.paths.clone(), c.op)),
                (_, Some(t)) => ops::parse_clipboard(&t),
                (Some(c), None) => Some((c.paths.clone(), c.op)),
                (None, None) => None,
            }
        });
        let Some((paths, op)) = parsed else {
            self.menu = None;
            self.set_notice(Notice::Info(self.lang.t().nothing_to_paste), cx);
            return;
        };
        self.paste_paths(paths, self.cwd.clone(), op, cx);
    }

    fn paste_paths(&mut self, paths: Vec<PathBuf>, dest: PathBuf, op: Op, cx: &mut Context<Self>) {
        if !self.busy.is_empty() {
            return;
        }
        if op == Op::Move {
            self.clip = None;
        }
        let progress = self.progress.clone();
        let label = self.lang.t().copy_suffix;
        let n = paths.len();
        self.run_job(
            Job::Op(op),
            n,
            move || ops::paste_progress(&paths, &dest, op, label, Some(&progress)),
            move |r| {
                let pairs = r.pairs.clone();
                match op {
                    _ if pairs.is_empty() => None,
                    Op::Copy => Some(Undo::Copy { created: pairs.into_iter().map(|(_, to)| to).collect() }),
                    _ => Some(Undo::Move { pairs }),
                }
            },
            cx,
        );
    }

    fn trash_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.target_paths();
        if !paths.is_empty() {
            self.run_job(
                Job::Op(Op::Trash),
                paths.len(),
                move || ops::trash(&paths),
                move |r| (!r.trashed.is_empty()).then(|| Undo::Trash { paths: r.trashed.clone() }),
                cx,
            );
        }
    }

    fn ask_delete(&mut self, cx: &mut Context<Self>) {
        let paths = self.target_paths();
        if !paths.is_empty() {
            self.confirm = Some(paths);
            self.menu = None;
            cx.notify();
        }
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        if let Some(paths) = self.confirm.take() {
            self.run_job(Job::Op(Op::Delete), paths.len(), move || ops::delete(&paths), |_| None, cx);
        }
    }

    fn start_rename(&mut self, cx: &mut Context<Self>) {
        if !self.busy.is_empty() {
            return;
        }
        let Some(e) = self.entries.get(self.cursor) else { return };
        let stem = ops::stem_len(&e.name.to_string_lossy(), e.kind.is_dir());
        self.renaming = Some(Rename {
            original: e.name.clone(),
            input: LineInput::new(e.name.to_string_lossy().into_owned(), stem),
        });
        self.marked.clear();
        self.menu = None;
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(r) = self.renaming.take() else { return };
        let new = r.input.text.trim().to_string();
        // Un único rename(2): no merece la pena ir a background.
        match ops::rename(&self.cwd, &r.original, &new) {
            Ok(()) => {
                // Renombrar una carpeta recién creada: un único paso de deshacer.
                let fresh = self.undo.last().is_some_and(|undo| undo.is_new_folder(&self.cwd.join(&r.original)));
                if fresh {
                    self.undo.pop();
                    if let Ok(undo) = (Undo::NewFolder { path: self.cwd.join(&new) }).guard() {
                        self.push_undo(undo);
                    }
                } else if OsStr::new(&new) != r.original {
                    if let Ok(undo) =
                        (Undo::Rename { dir: self.cwd.clone(), from: r.original.clone(), to: new.clone().into() })
                            .guard()
                    {
                        self.push_undo(undo);
                    }
                }
                self.reload(vec![new.into()], cx)
            }
            Err(NameError::Invalid) => {
                self.renaming = Some(r); // dejar corregirlo
                self.set_notice(Notice::Error(self.lang.t().invalid_name.into()), cx);
            }
            Err(NameError::Exists(n)) => {
                self.renaming = Some(r);
                self.set_notice(Notice::Error(self.lang.exists(&n)), cx);
            }
            Err(NameError::Io(e)) => {
                self.set_notice(Notice::Error(format!("{}: {e}", r.original.to_string_lossy())), cx)
            }
        }
        cx.notify();
    }

    fn new_folder(&mut self, cx: &mut Context<Self>) {
        if !self.busy.is_empty() {
            return;
        }
        self.menu = None;
        self.renaming = None;
        match ops::mkdir(&self.cwd, self.lang.t().new_folder) {
            Ok(name) => {
                if let Ok(undo) = (Undo::NewFolder { path: self.cwd.join(&name) }).guard() {
                    self.push_undo(undo);
                }
                self.pending_rename = Some(name.clone());
                self.reload(vec![name], cx);
            }
            Err(NameError::Io(e)) => self.set_notice(Notice::Error(e.to_string()), cx),
            Err(_) => {}
        }
    }

    // ---------------------------------------------------------- teclado ----

    fn on_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = &k.modifiers;
        let key = k.key.as_str();
        let command = if cfg!(target_os = "macos") { m.platform } else { m.control && !m.platform };

        if self.operation_errors.is_some() {
            if key == "escape" || key == "enter" {
                self.operation_errors = None;
                cx.notify();
            }
            return;
        }

        if let Some(input) = &mut self.location {
            match (command, key) {
                (true, "v") => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        input.insert(text.lines().next().unwrap_or(""));
                    }
                }
                (true, "a") => input.select_all(),
                (true, "l") => input.select_all(),
                _ => match input.key(key, k.key_char.as_deref(), command || m.control) {
                    InputResult::Commit => self.commit_location(cx),
                    InputResult::Cancel => self.location = None,
                    InputResult::Continue => {}
                },
            }
            cx.notify();
            return;
        }

        if let Some(r) = &mut self.renaming {
            match (command, key) {
                (true, "v") => {
                    if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        r.input.insert(t.lines().next().unwrap_or(""));
                    }
                }
                (true, "a") => r.input.select_all(),
                _ => match r.input.key(key, k.key_char.as_deref(), m.control) {
                    InputResult::Commit => self.commit_rename(cx),
                    InputResult::Cancel => self.renaming = None,
                    InputResult::Continue => {}
                },
            }
            cx.notify();
            return;
        }

        if self.confirm.is_some() {
            match key {
                "enter" => self.confirm_delete(cx),
                "escape" => self.confirm = None,
                _ => {}
            }
            cx.notify();
            return;
        }

        if command {
            match (m.shift, key) {
                (false, "backspace") if cfg!(target_os = "macos") => self.trash_selection(cx),
                (true, "backspace") if cfg!(target_os = "macos") => self.ask_delete(cx),
                (false, "c") => self.copy_or_cut(Op::Copy, cx),
                (false, "d") => self.toggle_pin(cx),
                (false, "l") => self.edit_location(cx),
                (false, "x") => self.copy_or_cut(Op::Move, cx),
                (false, "v") => self.paste(cx),
                (false, "a") => self.select_all(cx),
                (false, "h") if cfg!(target_os = "linux") => self.toggle_hidden(cx),
                (true, "." | ">") if cfg!(target_os = "macos") => self.toggle_hidden(cx),
                (false, "r") => self.reload(Vec::new(), cx),
                (true, "n") => self.new_folder(cx),
                (false, "z") => self.undo_last(cx),
                (false, "q") if self.busy.is_empty() => cx.quit(),
                _ => {}
            }
            return;
        }
        if m.platform || m.control {
            return;
        }

        match (m.alt, m.shift, key) {
            (true, _, "up") | (false, false, "backspace" | "left" | "h") => self.go_up(cx),
            (true, ..) => {}
            (_, true, "down") => self.move_cursor(1, true, cx),
            (_, true, "up") => self.move_cursor(-1, true, cx),
            (_, true, "delete") => self.ask_delete(cx),
            (_, _, "down" | "j") => self.move_cursor(1, false, cx),
            (_, _, "up" | "k") => self.move_cursor(-1, false, cx),
            (_, _, "pagedown") => self.move_cursor(25, m.shift, cx),
            (_, _, "pageup") => self.move_cursor(-25, m.shift, cx),
            (_, _, "home" | "g") => self.move_cursor(isize::MIN, m.shift, cx),
            (_, _, "end") => self.move_cursor(isize::MAX, m.shift, cx),
            (_, _, "enter" | "right" | "l") => self.open_cursor(cx),
            (_, _, "f2") => self.start_rename(cx),
            (_, _, "f5") => self.reload(Vec::new(), cx),
            (_, _, "delete") => self.trash_selection(cx),
            (_, _, ".") => self.toggle_hidden(cx),
            (_, _, "t") => self.cycle_theme(cx),
            (_, _, "i") => self.cycle_lang(cx),
            (_, _, "q") if self.busy.is_empty() => cx.quit(),
            (_, _, "escape") => {
                if self.menu.take().is_none() {
                    self.marked.clear();
                }
                cx.notify();
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------- render ----

    fn render_sidebar(&self, t: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let s = self.lang.t();
        let pins = self.pins.as_deref().unwrap_or_default();
        let current = self
            .places
            .iter()
            .map(|place| ("place", &place.path))
            .chain(self.disks.iter().map(|path| ("disk", path)))
            .chain(pins.iter().map(|path| ("pin", path)))
            .filter(|(_, path)| self.cwd.starts_with(path) && (path.as_path() != Path::new("/") || &self.cwd == *path))
            .max_by_key(|(_, path)| path.as_os_str().len());

        let row = |section, i, path: &PathBuf, label: SharedString, icon: Icon, cx: &mut Context<Self>| {
            let active = current == Some((section, path));
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
        let heading = |label| {
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
        };
        let empty = |label| div().px_2().py_1().text_xs().text_color(t.faint).child(label);
        let places = self
            .places
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
                div().id(SharedString::from(format!("pin-row-{i}"))).flex().items_center().gap_0p5().child(link).child(
                    Self::toolbar_button("remove", Icon::Close, s.unpin_folder, "", false, *t).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.set_pinned(target.clone(), false, cx);
                        },
                    )),
                )
            })
            .collect::<Vec<_>>();
        let disks = self
            .disks
            .iter()
            .enumerate()
            .map(|(i, path)| row("disk", i, path, path_label(path), Icon::System, cx))
            .collect::<Vec<_>>();
        let current_pinned = pins.contains(&self.cwd);
        let pin_button = Self::toolbar_button(
            "pin-current",
            Icon::Pin,
            if current_pinned { s.unpin_folder } else { s.pin_folder },
            shortcut("Ctrl+D", "⌘D"),
            current_pinned,
            *t,
        )
        .aria_toggled(if current_pinned { Toggled::True } else { Toggled::False })
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_pin(cx)));

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
            .child(div().flex_none().child(heading(s.places)).children(places))
            .child(
                div()
                    .flex_none()
                    .child(heading(s.pins).child(pin_button))
                    .when(pins.is_empty(), |d| d.child(empty(s.no_pins)))
                    .children(pinned),
            )
            .child(
                div()
                    .flex_none()
                    .child(heading(s.disks))
                    .when(disks.is_empty(), |d| d.child(empty(s.no_disks)))
                    .children(disks),
            )
    }

    /// Segmentos de la ruta actual, con "Inicio" para HOME y "…" si hay demasiados.
    fn crumbs(&self) -> Vec<(SharedString, PathBuf)> {
        let mut out = Vec::new();
        let (base_label, base, rest) = match &self.home {
            Some(h) if self.cwd.starts_with(h) => {
                (self.lang.t().home, h.clone(), self.cwd.strip_prefix(h).unwrap().to_path_buf())
            }
            _ => ("/", PathBuf::from("/"), self.cwd.strip_prefix("/").unwrap_or(&self.cwd).to_path_buf()),
        };
        out.push((SharedString::from(base_label), base.clone()));
        let mut acc = base;
        for c in rest.components() {
            acc.push(c);
            out.push((c.as_os_str().to_string_lossy().into_owned().into(), acc.clone()));
        }
        if out.len() > 6 {
            let tail = out.split_off(out.len() - 4);
            out.truncate(1);
            out.push(("…".into(), tail[0].1.parent().map(Path::to_path_buf).unwrap_or_default()));
            out.extend(tail);
        }
        out
    }

    fn toolbar_button(
        id: &'static str,
        icon: Icon,
        label: &'static str,
        shortcut: &'static str,
        active: bool,
        t: Theme,
    ) -> Stateful<Div> {
        div()
            .id(id)
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
            .tooltip(move |_, cx| cx.new(|_| ToolbarTooltip { label, shortcut, theme: t }).into())
    }

    fn render_toolbar(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let crumbs = self.crumbs();
        let last = crumbs.len() - 1;
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
                        this.navigate(path.clone(), None, cx);
                    }))
                    .child(label),
            );
        }

        if let Some(input) = &self.location {
            let (before, selected, after) = input.parts();
            trail = div()
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
                .child(SharedString::from(before.to_owned()))
                .child(div().bg(t.text_selection).child(SharedString::from(selected.to_owned())))
                .child(div().w(px(1.)).h(px(18.)).bg(t.accent))
                .child(SharedString::from(after.to_owned()));
        }
        let s = self.lang.t();
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
            .child(
                Self::toolbar_button(
                    "new-folder",
                    Icon::FolderPlus,
                    s.new_folder,
                    shortcut("Ctrl+Shift+N", "⇧⌘N"),
                    false,
                    *t,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.new_folder(cx))),
            )
            .child(div().w(px(1.)).h(px(16.)).mx_0p5().bg(t.border))
            .child(
                Self::toolbar_button("hidden", Icon::Eye, s.hidden, shortcut("Ctrl+H", "⇧⌘."), self.show_hidden, *t)
                    .aria_toggled(if self.show_hidden { Toggled::True } else { Toggled::False })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_hidden(cx))),
            )
            .child(
                Self::toolbar_button("theme", theme_icon, theme_label, "T", false, *t)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cycle_theme(cx))),
            );

        div()
            .flex_none()
            .h(px(56.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Self::toolbar_button("up", Icon::ArrowUp, s.go_up, shortcut("Alt+↑", "⌥↑"), false, *t)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.go_up(cx))),
            )
            .child(trail)
            .child(actions)
    }

    fn render_column_header(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
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

    fn render_name(&self, e: &Entry, is_cut: bool, t: &Theme) -> AnyElement {
        if let Some(r) = self.renaming.as_ref().filter(|r| r.original == e.name) {
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
            .child(SharedString::from(e.name.to_string_lossy().into_owned()))
            .into_any_element()
    }

    fn render_rows(&mut self, range: Range<usize>, window: &Window, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let t = self.mode.resolve(window.appearance());
        let lang = self.lang;
        static TABULAR_NUMBERS: OnceLock<FontFeatures> = OnceLock::new();
        let tabular_numbers = TABULAR_NUMBERS.get_or_init(|| FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
        let list_focused = window.is_window_active()
            && self.focus.is_focused(window)
            && self.confirm.is_none()
            && self.renaming.is_none();
        let cut = self.clip.as_ref().filter(|c| c.op == Op::Move).map(|c| &c.set);
        range
            .filter_map(|ix| self.entries.get(ix).map(|e| (ix, e)))
            .map(|(ix, e)| {
                let selected = self.is_selected(ix, &e.name);
                let focused = list_focused && ix == self.cursor;
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

    fn render_menu(&self, menu: &Menu, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let s = self.lang.t();
        let (del, shift_del) = match self.lang {
            Lang::En => ("Del", "Shift+Del"),
            Lang::Es => ("Supr", "Mayús+Supr"),
            Lang::Ca => ("Supr", "Maj+Supr"),
        };
        let del = shortcut(del, "⌘⌫");
        let shift_del = shortcut(shift_del, "⇧⌘⌫");
        let single = self.target_names().len() == 1;

        // None = separador.
        let mut items: Vec<Option<(Icon, &'static str, &'static str, bool, Action)>> = Vec::new();
        if menu.on_item {
            if single {
                items.push(Some((Icon::Open, s.open, "⏎", false, |this, cx| this.open_cursor(cx))));
                items.push(None);
            }
            items.push(Some((Icon::Cut, s.cut, shortcut("Ctrl+X", "⌘X"), false, |this, cx| {
                this.copy_or_cut(Op::Move, cx)
            })));
            items.push(Some((Icon::Copy, s.copy, shortcut("Ctrl+C", "⌘C"), false, |this, cx| {
                this.copy_or_cut(Op::Copy, cx)
            })));
            items.push(Some((Icon::Paste, s.paste, shortcut("Ctrl+V", "⌘V"), false, |this, cx| this.paste(cx))));
            if single {
                items.push(None);
                items.push(Some((Icon::Pencil, s.rename, "F2", false, |this, cx| this.start_rename(cx))));
            }
            items.push(None);
            items.push(Some((Icon::Trash, s.trash, del, false, |this, cx| this.trash_selection(cx))));
            items.push(Some((Icon::DeleteForever, s.delete, shift_del, true, |this, cx| this.ask_delete(cx))));
        } else {
            if !self.undo.is_empty() {
                items
                    .push(Some((Icon::Undo, s.undo, shortcut("Ctrl+Z", "⌘Z"), false, |this, cx| this.undo_last(cx))));
                items.push(None);
            }
            items.push(Some((Icon::Paste, s.paste, shortcut("Ctrl+V", "⌘V"), false, |this, cx| this.paste(cx))));
            items.push(Some((
                Icon::FolderPlus,
                s.new_folder,
                shortcut("Ctrl+Shift+N", "⇧⌘N"),
                false,
                |this, cx| this.new_folder(cx),
            )));
            items.push(None);
            items.push(Some((Icon::Refresh, s.refresh, "F5", false, |this, cx| this.reload(Vec::new(), cx))));
            items.push(Some((Icon::SelectAll, s.select_all, shortcut("Ctrl+A", "⌘A"), false, |this, cx| {
                this.select_all(cx)
            })));
        }

        let rows = items.into_iter().enumerate().map(|(i, item)| match item {
            None => div().my_1().mx_1().h(px(1.)).bg(t.border).into_any_element(),
            Some((icon, label, shortcut, danger, action)) => {
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
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.menu = None;
                        action(this, cx);
                        cx.notify();
                    }))
                    .child(icon.el(15.).text_color(if danger { t.error } else { t.muted }))
                    .child(div().flex_1().whitespace_nowrap().child(label))
                    .child(div().pl_6().whitespace_nowrap().text_xs().text_color(t.faint).child(shortcut))
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

    fn render_confirm(&self, paths: &[PathBuf], t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let s = self.lang.t();
        let preview = paths
            .iter()
            .take(3)
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(", ")
            + if paths.len() > 3 { ", …" } else { "" };

        div()
            .id("scrim")
            .absolute()
            .inset_0()
            .occlude()
            .bg(t.scrim)
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.confirm = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("confirm")
                    .w(px(400.))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_xl()
                    .bg(t.popover)
                    .border_1()
                    .border_color(t.border)
                    .shadow_lg()
                    .on_click(|_, _, cx| cx.stop_propagation())
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
                            .child(
                                div()
                                    .id("cancel")
                                    .h(px(32.))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(t.control)
                                    .hover(|d| d.bg(t.hover))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.confirm = None;
                                        cx.notify();
                                    }))
                                    .child(s.cancel),
                            )
                            .child(
                                div()
                                    .id("do-delete")
                                    .h(px(32.))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(t.danger_bg)
                                    .text_color(white())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .hover(|d| d.opacity(0.9))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.confirm_delete(cx);
                                        cx.notify();
                                    }))
                                    .child(s.delete),
                            ),
                    ),
            )
    }

    fn render_status(&self, t: &Theme) -> Div {
        let s = self.lang.t();
        let base = div().flex().items_center().gap_1p5().min_w_0().overflow_hidden();
        if let Some(&(_, job, n)) = self.busy.last() {
            let mut text = match job {
                Job::Op(op) => self.lang.busy(op, n),
                Job::Undo(_) => s.undoing.to_string(),
            };
            let copied = self.progress.load(Ordering::Relaxed);
            if copied > 0 {
                text.push_str(&format!(" · {}", human_size(copied)));
            }
            return base
                .text_color(t.accent)
                .child(Icon::Refresh.el(12.).text_color(t.accent))
                .child(SharedString::from(text));
        }
        match &self.notice {
            Some(Notice::Done(op, n)) => base
                .text_color(t.muted)
                .child(Icon::Check.el(12.).text_color(t.image))
                .child(SharedString::from(self.lang.done(*op, *n))),
            Some(Notice::Undone(k)) => base
                .text_color(t.muted)
                .child(Icon::Undo.el(12.).text_color(t.accent))
                .child(SharedString::from(self.lang.undone(*k))),
            Some(Notice::Info(msg)) => base.text_color(t.muted).child(*msg),
            Some(Notice::Error(msg)) => base
                .text_color(t.error)
                .child(Icon::Alert.el(12.).text_color(t.error))
                .child(div().truncate().child(SharedString::from(msg.clone()))),
            None => base.child(SharedString::from(match (self.loading, self.status_count) {
                (true, _) => s.loading.to_string(),
                (false, Some((n, ms))) if self.marked.len() > 1 => {
                    format!("{n} {} · {} {} · {ms:.1} ms", s.items, self.marked.len(), s.selected)
                }
                (false, Some((n, ms))) => format!("{n} {} · {ms:.1} ms", s.items),
                (false, None) => String::new(),
            })),
        }
    }
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.mode.resolve(window.appearance());
        let s = self.lang.t();

        let list: AnyElement = if let Some(err) = &self.error {
            placeholder(format!("{}: {err}", s.cannot_open), t.error).into_any_element()
        } else if self.loading && self.entries.is_empty() {
            placeholder(s.loading.into(), t.faint).into_any_element()
        } else if self.entries.is_empty() {
            placeholder(s.empty.into(), t.faint).into_any_element()
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
        };

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
            .child(list);

        let footer = div()
            .flex_none()
            .h(px(34.))
            .px_4()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .text_xs()
            .text_color(t.faint)
            .child(self.render_status(&t))
            .child(
                div().min_w_0().flex().items_center().gap_3().child(div().min_w_0().truncate().child(s.hints)).child(
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
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cycle_lang(cx)))
                        .child(Icon::Globe.el(11.).text_color(t.muted))
                        .child(self.lang.code()),
                ),
            );

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
            .child(footer);

        let menu = self.menu.as_ref().map(|m| self.render_menu(m, &t, cx));
        let confirm = self.confirm.as_deref().map(|p| self.render_confirm(p, &t, cx));
        let errors =
            self.operation_errors.as_ref().map(|errors| {
                div().absolute().inset_0().flex().items_center().justify_center().bg(t.scrim).child(
                    div()
                        .w(px(560.))
                        .max_w(relative(0.9))
                        .max_h(relative(0.8))
                        .flex()
                        .flex_col()
                        .p_4()
                        .gap_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(t.border)
                        .bg(t.popover)
                        .child(
                            div().id("operation-errors").min_h_0().overflow_y_scroll().children(errors.iter().map(
                                |error| div().py_1().text_color(t.error).child(SharedString::from(error.clone())),
                            )),
                        )
                        .when(self.retry_paste.is_some(), |d| {
                            d.child(
                                div()
                                    .id("retry-paste")
                                    .cursor_pointer()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(t.selection)
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.operation_errors = None;
                                        if let Some(retry) = this.retry_paste.take() {
                                            this.paste_paths(retry.sources, retry.destination, retry.operation, cx);
                                        }
                                    }))
                                    .child(s.retry),
                            )
                        })
                        .child(
                            div()
                                .id("dismiss-errors")
                                .cursor_pointer()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(t.control)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.operation_errors = None;
                                    cx.notify();
                                }))
                                .child("OK"),
                        ),
                )
            });

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
            .children(confirm)
            .children(errors)
    }
}

// --------------------------------------------------------------- helpers ----

/// Icono "tile": SVG sobre fondo tintado; carpetas con acento, ficheros con color por tipo.
fn file_tile(e: &Entry, t: &Theme) -> Div {
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

fn placeholder(text: String, color: gpui::Rgba) -> Div {
    div().flex_1().flex().items_center().justify_center().text_color(color).child(SharedString::from(text))
}

fn human_size(b: u64) -> SharedString {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024. && i < U.len() - 1 {
        v /= 1024.;
        i += 1;
    }
    if i == 0 { format!("{b} B").into() } else { format!("{v:.1} {}", U[i]).into() }
}

fn age(lang: Lang, t: SystemTime) -> SharedString {
    lang.age(SystemTime::now().duration_since(t).unwrap_or(Duration::ZERO).as_secs()).into()
}

fn start_dir(arg: Option<OsString>) -> PathBuf {
    arg.map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `fex --bench DIR`: mide listado + stat sin GUI.
fn bench(dir: PathBuf) {
    for show_hidden in [false, true] {
        let t = Instant::now();
        let entries = fs::list_dir(&dir, show_hidden).expect("no se pudo leer el directorio");
        let list = t.elapsed();
        let t = Instant::now();
        let _ = fs::stat_all(&dir, entries.iter().map(|e| e.name.clone()).collect());
        let stat = t.elapsed();
        println!(
            "hidden={show_hidden:<5} entradas={:<7} listado={:>7.2} ms  stat={:>7.2} ms",
            entries.len(),
            list.as_secs_f64() * 1e3,
            stat.as_secs_f64() * 1e3
        );
    }
}

fn main() {
    T0.set(Instant::now()).ok();

    let mut exit_after_first_frame = std::env::var_os("FEX_EXIT_AFTER_FIRST_FRAME").is_some();
    let mut forced_mode = None;
    let mut forced_lang = None;
    let mut dir_arg = None;
    let mut bench_mode = false;
    for a in std::env::args_os().skip(1) {
        match a.to_str().unwrap_or("") {
            "--bench" => bench_mode = true,
            "--exit-after-first-frame" => exit_after_first_frame = true,
            "--light" => forced_mode = Some(Mode::Light),
            "--dark" => forced_mode = Some(Mode::Dark),
            value if value.starts_with("--lang=") => forced_lang = Lang::parse(&value[7..]),
            _ => dir_arg = Some(a),
        }
    }
    let dir = start_dir(dir_arg);
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);

    if bench_mode {
        bench(dir);
        return;
    }

    application().with_assets(Assets).run(move |cx: &mut App| {
        eprintln!("[fex] app lista: {:.1} ms", since_start_ms());
        let bounds = Bounds::centered(None, size(px(1000.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("fex".into()),
                ..Default::default()
            },
            move |window, cx| {
                eprintln!("[fex] ventana creada: {:.1} ms", since_start_ms());
                let view = cx.new(|cx| {
                    let mut e = Explorer::new(dir, window, cx);
                    if let Some(m) = forced_mode {
                        e.mode = m;
                    }
                    if let Some(l) = forced_lang {
                        e.lang = l;
                    }
                    e
                });
                let closing_view = view.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    closing_view.upgrade().is_none_or(|view| view.read(cx).busy.is_empty())
                });
                let focus = view.read(cx).focus.clone();
                window.focus(&focus, cx);
                window.on_next_frame(move |_window, cx| {
                    eprintln!("[fex] primer frame: {:.1} ms", since_start_ms());
                    if exit_after_first_frame {
                        cx.quit();
                    }
                });
                view
            },
        )
        .expect("no se pudo abrir la ventana");
        cx.activate(true);
    });
}
