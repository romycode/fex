//! fexp — POC de explorador de archivos con GPUI.
//!
//! Reglas para llegar al primer frame rápido:
//! - La ventana se abre y pinta ANTES de leer el directorio.
//! - El listado usa solo `getdents` (d_type vía `file_type()`), sin `stat`.
//! - Tamaños y fechas se rellenan después, en segundo plano.
//! - El watcher (inotify) se crea tras el primer listado.
//! - Nada de gvfs, temas de iconos ni miniaturas.

mod fs;
mod i18n;
mod icons;
mod input;
mod ops;
mod theme;
mod undo;
mod watch;

use std::{
    collections::HashSet,
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant, SystemTime},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, Div, FocusHandle, FontWeight,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, ScrollStrategy, SharedString,
    Stateful, Subscription, Task, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
    anchored, deferred, div, prelude::*, px, size, uniform_list, white,
};
use gpui_platform::application;

use fs::{Entry, Kind, Place};
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
const SIDEBAR_W: f32 = 200.;
const COL_SIZE_W: f32 = 84.;
const COL_DATE_W: f32 = 118.;
const NOTICE_TTL: Duration = Duration::from_secs(5);

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
    original: String,
    input: LineInput,
}

struct Menu {
    pos: Point<Pixels>,
    on_item: bool,
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
    entries: Vec<Entry>,
    /// Fila con el foco de teclado.
    cursor: usize,
    /// Origen de la selección por rango (Shift).
    anchor: usize,
    /// Selección múltiple por nombre (sobrevive a los refrescos). Vacía = solo `cursor`.
    marked: HashSet<String>,
    show_hidden: bool,
    mode: Mode,
    lang: Lang,
    loading: bool,
    error: Option<String>,
    generation: u64,
    status_count: Option<(usize, f64)>,

    clip: Option<Clip>,
    renaming: Option<Rename>,
    pending_rename: Option<String>,
    menu: Option<Menu>,
    confirm: Option<Vec<PathBuf>>,
    busy: Vec<(u64, Job, usize)>,
    undo: Vec<Undo>,
    next_job: u64,
    notice: Option<Notice>,

    watcher: Option<DirWatcher>,
    watch_tx: mpsc::UnboundedSender<()>,

    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    _list_task: Option<Task<()>>,
    _stat_task: Option<Task<()>>,
    _notice_task: Option<Task<()>>,
    _watch_task: Task<()>,
    _appearance: Subscription,
}

impl Explorer {
    fn new(cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Re-pinta cuando el sistema cambia entre claro y oscuro (portal color-scheme).
        let appearance = cx.observe_window_appearance(window, |_, _, cx| cx.notify());

        // Eventos de inotify → refresco con debounce (150 ms) y como mucho ~2.5/s.
        let (watch_tx, mut watch_rx) = mpsc::unbounded::<()>();
        let watch_task = cx.spawn(async move |this, cx| {
            while watch_rx.next().await.is_some() {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                while watch_rx.try_recv().is_ok() {}
                if this.update(cx, |this, cx| this.reload(Vec::new(), cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(250)).await;
            }
        });

        let mut this = Self {
            cwd: PathBuf::new(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            places: fs::places(),
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
            pending_rename: None,
            menu: None,
            confirm: None,
            busy: Vec::new(),
            undo: Vec::new(),
            next_job: 0,
            notice: None,
            watcher: None,
            watch_tx,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _list_task: None,
            _stat_task: None,
            _notice_task: None,
            _watch_task: watch_task,
            _appearance: appearance,
        };
        this.navigate(cwd, None, cx);
        this
    }

    // ------------------------------------------------------ navegación ----

    fn navigate(&mut self, dir: PathBuf, select: Option<String>, cx: &mut Context<Self>) {
        let refresh = dir == self.cwd;
        if !refresh {
            self.renaming = None;
        }
        self.menu = None;
        self.load(dir, refresh, select.into_iter().collect(), cx);
    }

    /// Relee el directorio actual sin perder selección ni scroll.
    /// `select` no vacío → esos nombres quedan seleccionados (tras pegar, renombrar…).
    fn reload(&mut self, select: Vec<String>, cx: &mut Context<Self>) {
        self.load(self.cwd.clone(), true, select, cx);
    }

    fn load(&mut self, dir: PathBuf, refresh: bool, select: Vec<String>, cx: &mut Context<Self>) {
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
        select: Vec<String>,
        took: Duration,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let prev_cursor = self.entries.get(self.cursor).map(|e| e.name.clone());
        self.entries = entries;
        self.error = None;
        let pos = |name: &str, entries: &[Entry]| entries.iter().position(|e| e.name == name);

        if refresh {
            if select.is_empty() {
                if !self.marked.is_empty() {
                    let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
                    self.marked.retain(|n| names.contains(n.as_str()));
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
            let mut input = LineInput::new(name.clone(), 0);
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
        let names: Vec<String> = self.entries.iter().map(|e| e.name.clone()).collect();
        let stats = cx.background_spawn(async move { fs::stat_all(&dir, names) });
        self._stat_task = Some(cx.spawn(async move |this, cx| {
            let stats = stats.await;
            this.update(cx, |this, cx| {
                if this.generation != generation || stats.len() != this.entries.len() {
                    return;
                }
                for (e, (size, modified)) in this.entries.iter_mut().zip(stats) {
                    e.size = size;
                    e.modified = modified;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(self.cursor) else { return };
        let path = self.cwd.join(&entry.name);
        if entry.kind.is_dir() {
            self.navigate(path, None, cx);
        } else {
            // En un hilo aparte para recoger el proceso (sin zombis) y no bloquear.
            std::thread::spawn(move || {
                let _ = std::process::Command::new("xdg-open").arg(path).status();
            });
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else { return };
        let came_from = self.cwd.file_name().map(|n| n.to_string_lossy().into_owned());
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

    // ------------------------------------------------------- selección ----

    fn is_selected(&self, ix: usize, name: &str) -> bool {
        if self.marked.is_empty() { ix == self.cursor } else { self.marked.contains(name) }
    }

    fn target_names(&self) -> Vec<String> {
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
        if m.control {
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
        self.notice = Some(notice);
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
        make_undo: impl FnOnce(&ops::Report) -> Option<Undo> + 'static,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_job;
        self.next_job += 1;
        self.busy.push((id, job, n));
        self.menu = None;
        cx.notify();

        let task = cx.background_spawn(async move { work() });
        cx.spawn(async move |this, cx| {
            let report = task.await;
            this.update(cx, |this, cx| {
                this.busy.retain(|(i, ..)| *i != id);
                if let Some(u) = make_undo(&report) {
                    this.push_undo(u);
                }
                let notice = match (report.errors.is_empty(), job) {
                    (false, _) => Notice::Error(this.describe_errors(&report.errors)),
                    (true, Job::Op(op)) => Notice::Done(op, report.done),
                    (true, Job::Undo(k)) => Notice::Undone(k),
                };
                this.set_notice(notice, cx);
                this.reload(report.created, cx);
            })
            .ok();
        })
        .detach();
    }

    fn undo_last(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        let Some(u) = self.undo.pop() else {
            self.set_notice(Notice::Info(self.lang.t().nothing_to_undo), cx);
            return;
        };
        self.clip = None; // las rutas del portapapeles pueden dejar de ser válidas
        let kind = u.kind();
        self.run_job(Job::Undo(kind), 1, move || u.revert(), |_| None, cx);
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
        let system = cx.read_from_clipboard().and_then(|c| c.text());
        let parsed = match (&self.clip, system) {
            (Some(c), Some(t)) if t == c.text => Some((c.paths.clone(), c.op)),
            (_, Some(t)) => ops::parse_clipboard(&t),
            (Some(c), None) => Some((c.paths.clone(), c.op)),
            (None, None) => None,
        };
        let Some((paths, op)) = parsed else {
            self.menu = None;
            self.set_notice(Notice::Info(self.lang.t().nothing_to_paste), cx);
            return;
        };
        if op == Op::Move {
            self.clip = None; // cortar se consume al pegar
        }
        let dest = self.cwd.clone();
        let label = self.lang.t().copy_suffix;
        let n = paths.len();
        self.run_job(
            Job::Op(op),
            n,
            move || ops::paste(&paths, &dest, op, label),
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
            let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            self.run_job(
                Job::Op(Op::Trash),
                paths.len(),
                move || ops::trash(&paths),
                move |r| (!r.trashed.is_empty()).then(|| Undo::Trash { paths: r.trashed.clone(), since }),
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
        let Some(e) = self.entries.get(self.cursor) else { return };
        let stem = ops::stem_len(&e.name, e.kind.is_dir());
        self.renaming = Some(Rename { original: e.name.clone(), input: LineInput::new(e.name.clone(), stem) });
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
                let fresh = matches!(self.undo.last(), Some(Undo::NewFolder { path }) if *path == self.cwd.join(&r.original));
                if fresh {
                    self.undo.pop();
                    self.push_undo(Undo::NewFolder { path: self.cwd.join(&new) });
                } else if new != r.original {
                    self.push_undo(Undo::Rename { dir: self.cwd.clone(), from: r.original.clone(), to: new.clone() });
                }
                self.reload(vec![new], cx)
            }
            Err(NameError::Invalid) => {
                self.renaming = Some(r); // dejar corregirlo
                self.set_notice(Notice::Error(self.lang.t().invalid_name.into()), cx);
            }
            Err(NameError::Exists(n)) => {
                self.renaming = Some(r);
                self.set_notice(Notice::Error(self.lang.exists(&n)), cx);
            }
            Err(NameError::Io(e)) => self.set_notice(Notice::Error(format!("{}: {e}", r.original)), cx),
        }
        cx.notify();
    }

    fn new_folder(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        self.renaming = None;
        match ops::mkdir(&self.cwd, self.lang.t().new_folder) {
            Ok(name) => {
                self.push_undo(Undo::NewFolder { path: self.cwd.join(&name) });
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

        if let Some(r) = &mut self.renaming {
            match (m.control, key) {
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

        if m.platform {
            return;
        }
        if m.control {
            match (m.shift, key) {
                (false, "c") => self.copy_or_cut(Op::Copy, cx),
                (false, "x") => self.copy_or_cut(Op::Move, cx),
                (false, "v") => self.paste(cx),
                (false, "a") => self.select_all(cx),
                (false, "h") => self.toggle_hidden(cx),
                (false, "r") => self.reload(Vec::new(), cx),
                (true, "n") => self.new_folder(cx),
                (false, "z") => self.undo_last(cx),
                (false, "q") => cx.quit(),
                _ => {}
            }
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
            (_, _, "q") => cx.quit(),
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

    fn render_sidebar(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let s = self.lang.t();
        let items = self.places.iter().enumerate().map(|(i, place)| {
            let active = self.cwd == place.path;
            let path = place.path.clone();
            div()
                .id(SharedString::from(format!("place-{i}")))
                .h(px(30.))
                .px_2()
                .flex()
                .items_center()
                .gap_2p5()
                .rounded_md()
                .cursor_pointer()
                .text_color(if active { t.text } else { t.muted })
                .when(active, |d| d.bg(t.selection).font_weight(FontWeight::MEDIUM))
                .when(!active, |d| d.hover(|d| d.bg(t.hover)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.navigate(path.clone(), None, cx);
                }))
                .child(place.icon.el(16.).text_color(if active { t.accent } else { t.faint }))
                .child(place.id.label(s))
        });

        div()
            .w(px(SIDEBAR_W))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap_0p5()
            .px_2()
            .pt_3()
            .bg(t.sidebar)
            .border_r_1()
            .border_color(t.border)
            .child(
                div()
                    .px_2()
                    .pb_1p5()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.faint)
                    .child(s.places),
            )
            .children(items)
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

    fn button(&self, id: &'static str, icon: Icon, label: Option<&'static str>, active: bool, t: &Theme) -> Stateful<Div> {
        let fg = if active { t.accent } else { t.muted };
        div()
            .id(id)
            .h(px(28.))
            .when(label.is_some(), |d| d.px_2p5())
            .when(label.is_none(), |d| d.w(px(28.)).justify_center())
            .flex()
            .items_center()
            .gap_1p5()
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .text_color(fg)
            .bg(if active { t.selection } else { t.control })
            .hover(|d| d.bg(t.hover))
            .child(icon.el(15.).text_color(fg))
            .children(label)
    }

    fn render_toolbar(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let crumbs = self.crumbs();
        let last = crumbs.len() - 1;
        let mut trail = div().flex_1().min_w_0().flex().items_center().gap_0p5().overflow_hidden();
        for (i, (label, path)) in crumbs.into_iter().enumerate() {
            if i > 0 {
                trail = trail.child(Icon::ChevronRight.el(13.).text_color(t.faint));
            }
            let is_last = i == last;
            trail = trail.child(
                div()
                    .id(SharedString::from(format!("crumb-{i}")))
                    .flex_none()
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .whitespace_nowrap()
                    .text_color(if is_last { t.text } else { t.muted })
                    .when(is_last, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .hover(|d| d.bg(t.hover).text_color(t.text))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.navigate(path.clone(), None, cx);
                    }))
                    .child(label),
            );
        }

        let (theme_icon, theme_label) = self.mode.label(self.lang.t());
        div()
            .flex_none()
            .h(px(48.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(t.border)
            .child(
                self.button("up", Icon::ArrowUp, None, false, t)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.go_up(cx))),
            )
            .child(trail)
            .child(
                self.button("new-folder", Icon::FolderPlus, None, false, t)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.new_folder(cx))),
            )
            .child(
                self.button("hidden", Icon::Eye, Some(self.lang.t().hidden), self.show_hidden, t)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_hidden(cx))),
            )
            .child(
                self.button("theme", theme_icon, Some(theme_label), false, t)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cycle_theme(cx))),
            )
    }

    fn render_column_header(&self, t: &Theme) -> Div {
        let s = self.lang.t();
        div()
            .flex_none()
            .h(px(30.))
            .mx_2()
            .px_2()
            .flex()
            .items_center()
            .gap_3()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.faint)
            .child(div().w(px(26.)).flex_none())
            .child(div().flex_1().child(s.col_name))
            .child(div().w(px(COL_SIZE_W)).flex_none().text_right().child(s.col_size))
            .child(div().w(px(COL_DATE_W)).flex_none().text_right().child(s.col_modified))
    }

    fn render_name(&self, e: &Entry, t: &Theme) -> AnyElement {
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
            .when(e.kind.is_dir(), |d| d.font_weight(FontWeight::MEDIUM))
            .child(SharedString::from(e.name.clone()))
            .into_any_element()
    }

    fn render_rows(&mut self, range: Range<usize>, window: &Window, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let t = self.mode.resolve(window.appearance());
        let lang = self.lang;
        let multi = !self.marked.is_empty();
        let cut = self.clip.as_ref().filter(|c| c.op == Op::Move).map(|c| &c.set);
        range
            .filter_map(|ix| self.entries.get(ix).map(|e| (ix, e)))
            .map(|(ix, e)| {
                let selected = self.is_selected(ix, &e.name);
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
                    .border_color(if multi && ix == self.cursor { alpha(t.accent, 0.5) } else { alpha(t.accent, 0.) })
                    .when(selected, |d| d.bg(t.selection))
                    .when(!selected, |d| d.hover(|d| d.bg(t.hover)))
                    .when(is_cut, |d| d.opacity(0.45))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _w, cx| this.click_row(ix, ev, cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                            this.right_click_row(ix, ev.position, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(file_tile(e, &t))
                    .child(self.render_name(e, &t))
                    .child(
                        div()
                            .w(px(COL_SIZE_W))
                            .flex_none()
                            .text_right()
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
        let single = self.target_names().len() == 1;

        // None = separador.
        let mut items: Vec<Option<(Icon, &'static str, &'static str, bool, Action)>> = Vec::new();
        if menu.on_item {
            if single {
                items.push(Some((Icon::Open, s.open, "⏎", false, |this, cx| this.open_cursor(cx))));
                items.push(None);
            }
            items.push(Some((Icon::Cut, s.cut, "Ctrl+X", false, |this, cx| this.copy_or_cut(Op::Move, cx))));
            items.push(Some((Icon::Copy, s.copy, "Ctrl+C", false, |this, cx| this.copy_or_cut(Op::Copy, cx))));
            items.push(Some((Icon::Paste, s.paste, "Ctrl+V", false, |this, cx| this.paste(cx))));
            if single {
                items.push(None);
                items.push(Some((Icon::Pencil, s.rename, "F2", false, |this, cx| this.start_rename(cx))));
            }
            items.push(None);
            items.push(Some((Icon::Trash, s.trash, del, false, |this, cx| this.trash_selection(cx))));
            items.push(Some((Icon::DeleteForever, s.delete, shift_del, true, |this, cx| this.ask_delete(cx))));
        } else {
            if !self.undo.is_empty() {
                items.push(Some((Icon::Undo, s.undo, "Ctrl+Z", false, |this, cx| this.undo_last(cx))));
                items.push(None);
            }
            items.push(Some((Icon::Paste, s.paste, "Ctrl+V", false, |this, cx| this.paste(cx))));
            items.push(Some((Icon::FolderPlus, s.new_folder, "Ctrl+Shift+N", false, |this, cx| this.new_folder(cx))));
            items.push(None);
            items.push(Some((Icon::Refresh, s.refresh, "F5", false, |this, cx| this.reload(Vec::new(), cx))));
            items.push(Some((Icon::SelectAll, s.select_all, "Ctrl+A", false, |this, cx| this.select_all(cx))));
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
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::Alert.el(18.).text_color(t.error))
                            .child(
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
            let text = match job {
                Job::Op(op) => self.lang.busy(op, n),
                Job::Undo(_) => s.undoing.to_string(),
            };
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
            .h(px(28.))
            .px_4()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .border_t_1()
            .border_color(t.border)
            .text_xs()
            .text_color(t.faint)
            .child(self.render_status(&t))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(s.hints)
                    .child(
                        div()
                            .id("lang")
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
            .child(self.render_column_header(&t))
            .child(body)
            .child(footer);

        let menu = self.menu.as_ref().map(|m| self.render_menu(m, &t, cx));
        let confirm = self.confirm.as_deref().map(|p| self.render_confirm(p, &t, cx));

        div()
            .key_context("Explorer")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .relative()
            .size_full()
            .flex()
            .bg(t.bg)
            .text_color(t.text)
            .text_sm()
            .child(self.render_sidebar(&t, cx))
            .child(main)
            .children(menu)
            .children(confirm)
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
            let ext = Path::new(&e.name)
                .extension()
                .map(|x| x.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            t.file_style(&ext)
        }
    };
    let (bg, fg) = match tint {
        Some(c) => (alpha(c, 0.15), c),
        None => (t.tile_file, t.tile_file_fg),
    };
    div()
        .size(px(26.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(bg)
        .child(icon.el(15.).text_color(fg))
}

fn placeholder(text: String, color: gpui::Rgba) -> Div {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(SharedString::from(text))
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

fn start_dir(arg: Option<String>) -> PathBuf {
    arg.map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `fexp --bench DIR`: mide listado + stat sin GUI.
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

    let mut exit_after_first_frame = std::env::var_os("FEXP_EXIT_AFTER_FIRST_FRAME").is_some();
    let mut forced_mode = None;
    let mut forced_lang = None;
    let mut dir_arg = None;
    let mut bench_mode = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--bench" => bench_mode = true,
            "--exit-after-first-frame" => exit_after_first_frame = true,
            "--light" => forced_mode = Some(Mode::Light),
            "--dark" => forced_mode = Some(Mode::Dark),
            _ if a.starts_with("--lang=") => forced_lang = Lang::parse(&a[7..]),
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
        eprintln!("[fexp] app lista: {:.1} ms", since_start_ms());
        let bounds = Bounds::centered(None, size(px(1000.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("fexp".into()),
                ..Default::default()
            },
            move |window, cx| {
                eprintln!("[fexp] ventana creada: {:.1} ms", since_start_ms());
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
                let focus = view.read(cx).focus.clone();
                window.focus(&focus, cx);
                window.on_next_frame(move |_window, cx| {
                    eprintln!("[fexp] primer frame: {:.1} ms", since_start_ms());
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
