//! Acciones sobre elementos de cualquier backend. La lógica de verdad vive en
//! [`crate::backend`], [`crate::ops`], [`crate::s3`] y [`crate::undo`]; aquí solo se recoge
//! la selección, se lanza el trabajo y se decide su deshacer.

use std::{ffi::OsStr, path::PathBuf};

use gpui::{ClipboardEntry, ClipboardItem, Context};

use super::{Clip, Explorer, Modal, Rename, jobs::Job, jobs::Notice};
use crate::{
    backend::{self, Item, Location},
    input::LineInput,
    ops::{self, NameError, Op},
    s3,
    undo::{Undo, UndoKind},
};

impl Explorer {
    pub fn copy_or_cut(&mut self, op: Op, cx: &mut Context<Self>) {
        let items = self.target_items();
        if items.is_empty() {
            return;
        }
        // Rutas o URIs s3:// en texto plano: útil al pegar en una terminal o editor.
        let text = items.iter().map(Item::to_clipboard).collect::<Vec<_>>().join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.clip = Some(Clip { set: items.iter().cloned().collect(), items, op, text });
        self.menu = None;
        cx.notify();
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if !self.can_start_job() {
            return;
        }
        let system = cx.read_from_clipboard();
        let external = system.as_ref().and_then(|clipboard| {
            clipboard.entries.iter().find_map(|entry| match entry {
                ClipboardEntry::ExternalPaths(paths) if !paths.paths().is_empty() => Some(paths.paths().to_vec()),
                _ => None,
            })
        });
        let local = |(paths, op): (Vec<PathBuf>, Op)| (paths.into_iter().map(Item::Local).collect::<Vec<_>>(), op);
        let parsed = external.map(|paths| local((paths, Op::Copy))).or_else(|| {
            match (&self.clip, system.and_then(|clipboard| clipboard.text())) {
                (Some(c), Some(t)) if t == c.text => Some((c.items.clone(), c.op)),
                (_, Some(t)) => ops::parse_clipboard(&t).map(local),
                (Some(c), None) => Some((c.items.clone(), c.op)),
                (None, None) => None,
            }
        });
        let Some((items, op)) = parsed else {
            self.menu = None;
            self.set_notice(Notice::Info(self.lang.t().nothing_to_paste), cx);
            return;
        };
        self.paste_items(items, self.source.clone(), op, cx);
    }

    /// Pegar rutas locales en un directorio local (reintentos y tests).
    pub fn paste_paths(&mut self, paths: Vec<PathBuf>, dest: PathBuf, op: Op, cx: &mut Context<Self>) {
        self.paste_items(paths.into_iter().map(Item::Local).collect(), Location::Local(dest), op, cx);
    }

    pub fn paste_items(&mut self, items: Vec<Item>, dest: Location, op: Op, cx: &mut Context<Self>) {
        if !self.can_start_job() || !dest.capabilities().write {
            return;
        }
        if op == Op::Move {
            self.clip = None;
        }
        let progress = self.jobs.progress.clone();
        let label = self.lang.t().copy_suffix;
        let n = items.len();
        self.run_job(
            Job::Op(op),
            n,
            backend::paste(items, dest, op, label, progress),
            move |report| backend::paste_undo(report, op),
            cx,
        );
    }

    pub fn trash_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.target_paths();
        if !paths.is_empty() && self.caps().trash {
            self.run_job(
                Job::Op(Op::Trash),
                paths.len(),
                async move { ops::trash(&paths) },
                move |r| (!r.trashed.is_empty()).then(|| Undo::Trash { paths: r.trashed.clone() }),
                cx,
            );
        }
    }

    pub fn ask_delete(&mut self, cx: &mut Context<Self>) {
        let items = self.target_items();
        if !items.is_empty() {
            self.modal = Modal::ConfirmDelete(items);
            self.menu = None;
            cx.notify();
        }
    }

    pub fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.modal, Modal::ConfirmDelete(_)) {
            return;
        }
        if let Modal::ConfirmDelete(items) = std::mem::take(&mut self.modal) {
            let progress = self.jobs.progress.clone();
            self.run_job(Job::Op(Op::Delete), items.len(), backend::delete(items, progress), |_| None, cx);
        }
    }

    pub fn start_rename(&mut self, cx: &mut Context<Self>) {
        if !self.can_start_job() {
            return;
        }
        let Some(e) = self.cursor_entry() else { return };
        let name = self.source.entry_name(e);
        let stem = ops::stem_len(&name, e.kind.is_dir());
        self.modal = Modal::Rename(Rename { original: e.name.clone(), input: LineInput::new(name, stem) });
        self.selection.clear_marks();
        self.menu = None;
        cx.notify();
    }

    pub fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.modal, Modal::Rename(_)) {
            return;
        }
        let Modal::Rename(r) = std::mem::take(&mut self.modal) else { return };
        let new = r.input.text.trim().to_string();
        if !self.source.is_local() {
            self.commit_remote_rename(r, new, cx);
            return;
        }
        // Un único rename(2): no merece la pena ir a background.
        match ops::rename(&self.cwd, &r.original, &new) {
            Ok(()) => {
                // Renombrar una carpeta recién creada: un único paso de deshacer.
                let fresh = self.jobs.undo.last().is_some_and(|undo| undo.is_new_folder(&self.cwd.join(&r.original)));
                if fresh {
                    self.jobs.undo.pop();
                    if let Ok(undo) = (Undo::NewFolder { path: self.cwd.join(&new) }).guard() {
                        self.jobs.push_undo(undo);
                    }
                } else if OsStr::new(&new) != r.original
                    && let Ok(undo) =
                        (Undo::Rename { dir: self.cwd.clone(), from: r.original.clone(), to: new.clone().into() })
                            .guard()
                {
                    self.jobs.push_undo(undo);
                }
                self.reload(vec![new.into()], cx)
            }
            Err(NameError::Invalid) => {
                self.modal = Modal::Rename(r); // dejar corregirlo
                self.set_notice(Notice::Error(self.lang.t().invalid_name.into()), cx);
            }
            Err(NameError::Exists(n)) => {
                self.modal = Modal::Rename(r);
                self.set_notice(Notice::Error(self.lang.exists(&n)), cx);
            }
            Err(NameError::Io(e)) => {
                self.set_notice(Notice::Error(format!("{}: {e}", r.original.to_string_lossy())), cx)
            }
        }
        cx.notify();
    }

    /// Renombrar fuera de local: copia + borrado en background.
    fn commit_remote_rename(&mut self, r: Rename, new: String, cx: &mut Context<Self>) {
        let entry = self.entries.iter().find(|e| e.name == r.original);
        let Some(item) = entry.and_then(|e| self.source.item(e)) else { return };
        if entry.is_some_and(|e| self.source.entry_name(e) == new) {
            return;
        }
        if !ops::validate_name(&new) {
            self.modal = Modal::Rename(r);
            self.set_notice(Notice::Error(self.lang.t().invalid_name.into()), cx);
            return;
        }
        // Renombrar una carpeta recién creada: un único paso de deshacer (crearla).
        let fresh = match (self.jobs.undo.last(), &item) {
            (Some(Undo::S3 { kind: UndoKind::NewFolder, change: s3::Change::Created(created) }), Item::S3(object))
                if created.len() == 1 && created[0].object == *object =>
            {
                self.jobs.undo.pop()
            }
            _ => None,
        };
        self.run_job(
            Job::Rename,
            1,
            backend::rename(item, new),
            move |report| match (report.s3_moved.is_empty(), fresh) {
                (true, fresh) => fresh,
                (false, Some(_)) => Some(Undo::S3 {
                    kind: UndoKind::NewFolder,
                    change: s3::Change::Created(report.s3_moved.iter().map(|(_, now)| now.clone()).collect()),
                }),
                (false, None) => {
                    Some(Undo::S3 { kind: UndoKind::Rename, change: s3::Change::Moved(report.s3_moved.clone()) })
                }
            },
            cx,
        );
    }

    pub fn new_folder(&mut self, cx: &mut Context<Self>) {
        if !self.can_start_job() || !self.caps().write {
            return;
        }
        self.menu = None;
        if matches!(self.modal, Modal::Rename(_)) {
            self.modal = Modal::None;
        }
        let base = self.lang.t().new_folder;
        if !self.source.is_local() {
            self.run_job(
                Job::NewFolder,
                1,
                backend::mkdir(self.source.clone(), base),
                |report| {
                    (!report.s3_created.is_empty()).then(|| Undo::S3 {
                        kind: UndoKind::NewFolder,
                        change: s3::Change::Created(report.s3_created.clone()),
                    })
                },
                cx,
            );
            return;
        }
        // Local: un único mkdir(2), sin esperar a un hilo.
        match ops::mkdir(&self.cwd, base) {
            Ok(name) => {
                if let Ok(undo) = (Undo::NewFolder { path: self.cwd.join(&name) }).guard() {
                    self.jobs.push_undo(undo);
                }
                self.pending_rename = Some(name.clone());
                self.reload(vec![name], cx);
            }
            Err(NameError::Io(e)) => self.set_notice(Notice::Error(e.to_string()), cx),
            Err(_) => {}
        }
    }
}
