//! Acciones sobre ficheros locales. La lógica de verdad vive en [`crate::ops`] y
//! [`crate::undo`]; aquí solo se recoge la selección, se lanza el trabajo y se decide su deshacer.

use std::{ffi::OsStr, path::PathBuf};

use gpui::{ClipboardEntry, ClipboardItem, Context};

use super::{Clip, Explorer, Modal, Rename, jobs::Job, jobs::Notice};
use crate::{
    input::LineInput,
    ops::{self, NameError, Op},
    undo::Undo,
};

impl Explorer {
    pub fn copy_or_cut(&mut self, op: Op, cx: &mut Context<Self>) {
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

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if !self.can_start_job() {
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

    pub fn paste_paths(&mut self, paths: Vec<PathBuf>, dest: PathBuf, op: Op, cx: &mut Context<Self>) {
        if !self.can_start_job() {
            return;
        }
        if op == Op::Move {
            self.clip = None;
        }
        let progress = self.jobs.progress.clone();
        let label = self.lang.t().copy_suffix;
        let n = paths.len();
        self.run_job(
            Job::Op(op),
            n,
            async move { ops::paste_progress(&paths, &dest, op, label, Some(&progress)) },
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

    pub fn trash_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.target_paths();
        if !paths.is_empty() {
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
        let paths = self.target_paths();
        if !paths.is_empty() {
            self.modal = Modal::ConfirmDelete(paths);
            self.menu = None;
            cx.notify();
        }
    }

    pub fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.modal, Modal::ConfirmDelete(_)) {
            return;
        }
        if let Modal::ConfirmDelete(paths) = std::mem::take(&mut self.modal) {
            self.run_job(Job::Op(Op::Delete), paths.len(), async move { ops::delete(&paths) }, |_| None, cx);
        }
    }

    pub fn start_rename(&mut self, cx: &mut Context<Self>) {
        if !self.can_start_job() {
            return;
        }
        let Some(e) = self.cursor_entry() else { return };
        let stem = ops::stem_len(&e.name.to_string_lossy(), e.kind.is_dir());
        self.modal = Modal::Rename(Rename {
            original: e.name.clone(),
            input: LineInput::new(e.name.to_string_lossy().into_owned(), stem),
        });
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

    pub fn new_folder(&mut self, cx: &mut Context<Self>) {
        if !self.can_start_job() {
            return;
        }
        self.menu = None;
        if matches!(self.modal, Modal::Rename(_)) {
            self.modal = Modal::None;
        }
        match ops::mkdir(&self.cwd, self.lang.t().new_folder) {
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
