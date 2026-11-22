//! Navegación: listar en background, refrescar, ordenar, ir a rutas y abrir entradas.

use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui::{AppContext, Context, PathPromptOptions, ScrollStrategy};

use super::{DownloadRequest, Explorer, LocationInput, Modal, Rename};
use crate::{
    backend::Location,
    fs::{self, Entry, SortColumn},
    input::LineInput,
    watch::DirWatcher,
};

impl Explorer {
    pub fn navigate(&mut self, dir: PathBuf, select: Option<OsString>, cx: &mut Context<Self>) {
        self.navigate_to(Location::Local(dir), select, cx);
    }

    /// `select` = nombre que debe quedar con el cursor (p. ej. la carpeta de la que se sube).
    pub fn navigate_to(&mut self, location: Location, select: Option<OsString>, cx: &mut Context<Self>) {
        let refresh = location == self.source;
        if !refresh {
            // Las filas y las acciones pendientes pertenecen al directorio anterior.
            self.entries.clear();
            self.selection = Default::default();
            self.status_count = None;
            self.pending_rename = None;
        }
        // Un informe de errores sobrevive a la navegación. Renombrar sobrevive a un refresco.
        match self.modal {
            Modal::Errors(_) => {}
            Modal::Rename(_) if refresh => {}
            _ => self.modal = Modal::None,
        }
        self.menu = None;
        self.load(location, refresh, select.into_iter().collect(), cx);
    }

    /// Vuelve a listar la ubicación actual. `select` no vacío → esos nombres quedan seleccionados.
    pub fn reload(&mut self, select: Vec<OsString>, cx: &mut Context<Self>) {
        self.load(self.source.clone(), true, select, cx);
    }

    fn load(&mut self, location: Location, refresh: bool, select: Vec<OsString>, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        if let Location::Local(dir) = &location {
            self.cwd = dir.clone();
        }
        if !location.capabilities().watch {
            self.watcher = None;
        }
        self.source = location.clone();
        if !refresh {
            self.loading = true;
            self.error = None;
        }
        self._stat_task = None;
        cx.notify();

        let show_hidden = self.show_hidden;
        let started = Instant::now();
        let listing = cx.background_spawn(async move { location.list(show_hidden).await });

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
                    Err(e) if this.source.is_local() && refresh && e.kind() == io::ErrorKind::NotFound => {
                        // Han borrado el directorio actual: subir al primer ancestro que exista.
                        let up = this.cwd.ancestors().skip(1).find(|p| p.is_dir()).map(Path::to_path_buf);
                        if let Some(up) = up {
                            this.navigate(up, None, cx);
                        }
                    }
                    Err(e) => {
                        this.entries.clear();
                        this.selection.clear_marks();
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
        let previous = self.cursor_entry().map(|e| e.name.clone());
        self.entries = entries;
        fs::sort_entries(&mut self.entries, self.sort_column, self.sort_descending);
        self.error = None;
        self.selection.restore(&self.entries, refresh, &select, previous);
        let cursor = self.selection.cursor;
        if !refresh {
            self.scroll.scroll_to_item(cursor, ScrollStrategy::Center);
        } else if !select.is_empty() {
            self.scroll.scroll_to_item(cursor, ScrollStrategy::Nearest);
        }

        let pos = |name: &OsString, entries: &[Entry]| entries.iter().position(|e| &e.name == name);
        if let Some(name) = self.pending_rename.take()
            && let Some(ix) = pos(&name, &self.entries)
        {
            self.selection.select_only(ix);
            let mut input = LineInput::new(name.to_string_lossy().into_owned(), 0);
            input.select_all();
            self.modal = Modal::Rename(Rename { original: name, input });
        }
        if let Modal::Rename(r) = &self.modal
            && pos(&r.original, &self.entries).is_none()
        {
            self.modal = Modal::None;
        }

        self.status_count = Some((self.entries.len(), took.as_secs_f64() * 1e3));
        let caps = self.caps();
        if caps.watch {
            self.watch_cwd();
        }
        if caps.stat_pass {
            self.start_stat_pass(generation, cx);
        }
    }

    fn watch_cwd(&mut self) {
        if self.watcher.is_none() {
            self.watcher = DirWatcher::new(self.watch_tx.clone()).ok();
        }
        if let Some(w) = &mut self.watcher {
            w.watch(&self.cwd);
        }
    }

    /// Segunda pasada: tamaño y fecha, sin bloquear el primer pintado del listado.
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
        let selected = self.cursor_entry().map(|entry| entry.name.clone());
        fs::sort_entries(&mut self.entries, self.sort_column, self.sort_descending);
        self.selection.follow(selected, &self.entries);
    }

    pub fn change_sort(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        if self.sort_column == column {
            self.sort_descending = !self.sort_descending;
        } else {
            self.sort_column = column;
            self.sort_descending = false;
        }
        self.sort_listing();
        self.scroll.scroll_to_item(self.selection.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    // ------------------------------------------------------- ir a una ruta ----

    pub fn edit_location(&mut self, cx: &mut Context<Self>) {
        let mut input = LineInput::new(self.cwd.to_string_lossy().into_owned(), 0);
        input.select_all();
        self.modal = Modal::Location(LocationInput::Local(input));
        self.menu = None;
        cx.notify();
    }

    pub fn commit_location(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.modal, Modal::Location(_)) {
            return;
        }
        let Modal::Location(location) = std::mem::take(&mut self.modal) else { return };
        match location {
            LocationInput::Local(input) => self.navigate(self.resolve_local_path(&input.text), None, cx),
            LocationInput::DownloadDestination(input, request) => {
                self.start_download(request, self.resolve_local_path(&input.text), cx);
            }
        }
    }

    /// Admite rutas absolutas, relativas al directorio actual, `~` y `~/…`.
    pub fn resolve_local_path(&self, input: &str) -> PathBuf {
        if input == "~" {
            self.home.clone().unwrap_or_else(|| self.cwd.clone())
        } else if let Some(suffix) = input.strip_prefix("~/") {
            self.home.as_ref().unwrap_or(&self.cwd).join(suffix)
        } else {
            self.cwd.join(input)
        }
    }

    pub fn open_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry() else { return };
        if let Some(location) = self.source.child(entry) {
            self.navigate_to(location, None, cx);
        } else if self.caps().open_files {
            let path = self.cwd.join(&entry.name);
            // En un hilo aparte para recoger el proceso (sin zombis) y no bloquear.
            std::thread::spawn(move || {
                let _ = std::process::Command::new(crate::keymap::Platform::CURRENT.opener()).arg(path).status();
            });
        }
    }

    pub fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some((parent, child)) = self.source.parent() {
            self.navigate_to(parent, Some(child), cx);
        } else if !self.source.is_local() {
            self.navigate(self.cwd.clone(), None, cx);
        }
    }

    // ------------------------------------------------------------------ S3 ----

    pub fn download_selection(&mut self, cx: &mut Context<Self>) {
        if !self.can_start_job() || !self.caps().download {
            return;
        }
        let items = self.target_items();
        if items.is_empty() {
            return;
        }
        let request = DownloadRequest { items };
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(self.lang.t().download_destination.into()),
        });
        self.menu = None;
        self.download_prompt = Some(cx.spawn(async move |this, cx| {
            let response = receiver.await;
            this.update(cx, |this, cx| {
                this.download_prompt = None;
                match response {
                    Ok(Ok(None)) => {}
                    Ok(Ok(Some(mut paths))) if !paths.is_empty() => {
                        this.start_download(request, paths.pop().unwrap(), cx);
                    }
                    // Sin diálogo nativo (portal ausente): pedir la ruta en la barra.
                    _ => {
                        let mut input = LineInput::new(this.cwd.to_string_lossy().into_owned(), 0);
                        input.select_all();
                        this.modal = Modal::Location(LocationInput::DownloadDestination(input, request));
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Descargar = pegar en un directorio local elegido (copia, con deshacer a la papelera).
    fn start_download(&mut self, request: DownloadRequest, directory: PathBuf, cx: &mut Context<Self>) {
        let progress = self.jobs.progress.clone();
        let label = self.lang.t().copy_suffix;
        let items = request.items;
        let n = items.len();
        let work = crate::backend::paste(items, Location::Local(directory), crate::ops::Op::Copy, label, progress);
        self.run_job(super::jobs::Job::Download, n, work, |report| crate::backend::paste_undo(report, crate::ops::Op::Copy), cx);
    }
}
