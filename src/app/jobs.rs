//! Trabajos en segundo plano, avisos de estado y pila de deshacer.
//!
//! Cada trabajo corre en su propio hilo del sistema: las operaciones de ficheros hacen I/O
//! bloqueante (una copia de 50 GB puede durar minutos) y no deben ocupar los hilos del
//! executor de GPUI, que también listan directorios.

use std::{ffi::OsStr, future::Future, time::Duration};

use futures::channel::oneshot;
use gpui::{Context, Task};

use super::{Explorer, Modal};
use crate::{
    ops::{self, Op, OpError},
    undo::{MAX_UNDO, Undo, UndoKind},
};

const NOTICE_TTL: Duration = Duration::from_secs(5);

/// Trabajo en background: una operación, un deshacer o una descarga.
#[derive(Clone, Copy)]
pub enum Job {
    Op(Op),
    Undo(UndoKind),
    Download,
}

pub enum Notice {
    Done(Op, usize),
    Undone(UndoKind),
    Downloaded(usize),
    /// Informativo (gris): "Nada que deshacer".
    Info(&'static str),
    Error(String),
}

#[derive(Default)]
pub struct Jobs {
    /// Trabajos en curso: (id, tipo, nº de elementos).
    pub busy: Vec<(u64, Job, usize)>,
    pub undo: Vec<Undo>,
    next_id: u64,
    /// Bytes procesados y cancelación del trabajo actual.
    pub progress: ops::Progress,
    pub retry_paste: Option<ops::Retry>,
    pub notice: Option<Notice>,
    notice_task: Option<Task<()>>,
}

impl Jobs {
    pub fn push_undo(&mut self, u: Undo) {
        self.undo.push(u);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }
}

impl Explorer {
    pub fn set_notice(&mut self, notice: Notice, cx: &mut Context<Self>) {
        let is_error = matches!(notice, Notice::Error(_));
        self.jobs.notice = Some(notice);
        if is_error {
            self.jobs.notice_task = None;
            cx.notify();
            return;
        }
        self.jobs.notice_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_TTL).await;
            this.update(cx, |this, cx| {
                this.jobs.notice = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub fn describe_errors(&self, errors: &[OpError]) -> String {
        let s = self.lang.t();
        let first = match &errors[0] {
            OpError::IntoItself(n) => format!("{n}: {}", s.into_itself),
            OpError::Exists(n) => self.lang.exists(n),
            OpError::Io(n, e) => format!("{n}: {e}"),
            OpError::Trash(n, e) => format!("{n}: {e}"),
        };
        if errors.len() > 1 { first + &self.lang.more_errors(errors.len() - 1) } else { first }
    }

    /// Un trabajo a la vez, y ninguno mientras se elige el destino de una descarga.
    pub fn can_start_job(&self) -> bool {
        self.jobs.busy.is_empty()
            && self.download_prompt.is_none()
            && !matches!(self.modal, Modal::Location(super::LocationInput::DownloadDestination(..)))
    }

    /// Lanza un trabajo en un hilo propio; al terminar avisa, apila su deshacer (si lo tiene)
    /// y refresca seleccionando el resultado.
    pub fn run_job(
        &mut self,
        job: Job,
        n: usize,
        work: impl Future<Output = ops::Report> + Send + 'static,
        make_undo: impl FnOnce(&ops::Report) -> Option<Undo> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if !self.can_start_job() {
            return;
        }
        let cwd = self.cwd.clone();
        let id = self.jobs.next_id;
        self.jobs.next_id += 1;
        self.jobs.progress.reset();
        self.jobs.busy.push((id, job, n));
        self.menu = None;
        cx.notify();

        // Repinta el progreso mientras dure.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let active = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.jobs.busy.iter().any(|(job_id, ..)| *job_id == id)
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();

        let (tx, rx) = oneshot::channel();
        let spawned = std::thread::Builder::new().name("fex-job".into()).spawn(move || {
            let mut report = futures::executor::block_on(work);
            let undo = match make_undo(&report).map(Undo::guard) {
                Some(Ok(undo)) => Some(undo),
                Some(Err(error)) => {
                    report.errors.push(OpError::Io("Undo".into(), error));
                    None
                }
                None => None,
            };
            let _ = tx.send((report, undo));
        });
        if let Err(error) = spawned {
            self.jobs.busy.retain(|(i, ..)| *i != id);
            self.set_notice(Notice::Error(error.to_string()), cx);
            return;
        }

        cx.spawn(async move |this, cx| {
            let Ok((report, undo)) = rx.await else { return };
            this.update(cx, |this, cx| this.finish_job(id, job, cwd, report, undo, cx)).ok();
        })
        .detach();
    }

    fn finish_job(
        &mut self,
        id: u64,
        job: Job,
        cwd: std::path::PathBuf,
        report: ops::Report,
        undo: Option<Undo>,
        cx: &mut Context<Self>,
    ) {
        self.jobs.busy.retain(|(i, ..)| *i != id);
        if let Some(u) = undo {
            self.jobs.push_undo(u);
        }
        self.jobs.retry_paste = report.retry.clone();
        if !report.errors.is_empty() {
            let mut details = vec![match job {
                Job::Op(op) => self.lang.done(op, report.done),
                Job::Undo(_) => self.lang.t().undo.into(),
                Job::Download => format!("{} {}", report.done, self.lang.t().downloaded),
            }];
            details.extend(report.errors.iter().map(|error| self.describe_errors(std::slice::from_ref(error))));
            self.modal = Modal::Errors(details);
        }
        let notice = match (report.errors.is_empty(), job) {
            (false, _) => Notice::Error(self.describe_errors(&report.errors)),
            (true, Job::Op(op)) => Notice::Done(op, report.done),
            (true, Job::Undo(k)) => Notice::Undone(k),
            (true, Job::Download) => Notice::Downloaded(report.done),
        };
        self.set_notice(notice, cx);
        let selected = report
            .created_paths
            .iter()
            .filter(|path| path.parent() == Some(self.cwd.as_path()))
            .filter_map(|path| path.file_name().map(OsStr::to_os_string))
            .collect::<Vec<_>>();
        if self.source.is_local() && (!selected.is_empty() || (!matches!(job, Job::Download) && self.cwd == cwd)) {
            self.reload(selected, cx);
        }
    }

    pub fn undo_last(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if !self.can_start_job() {
            return;
        }
        let Some(u) = self.jobs.undo.pop() else {
            self.set_notice(Notice::Info(self.lang.t().nothing_to_undo), cx);
            return;
        };
        self.clip = None; // las rutas del portapapeles pueden dejar de ser válidas
        let kind = u.kind();
        let retry = u.clone();
        self.run_job(
            Job::Undo(kind),
            1,
            async move { u.revert() },
            move |report| (report.done == 0 && !report.errors.is_empty()).then_some(retry),
            cx,
        );
    }

    /// Pide al trabajo actual que pare. Las copias a medias se descartan (nunca se publican)
    /// y lo que no se llegó a copiar queda para "Reintentar pendientes".
    pub fn cancel_job(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            self.jobs.progress.cancel();
            cx.notify();
        }
    }

    pub fn retry_failed_paste(&mut self, cx: &mut Context<Self>) {
        self.modal = Modal::None;
        if let Some(retry) = self.jobs.retry_paste.take() {
            self.paste_paths(retry.sources, retry.destination, retry.operation, cx);
        }
    }
}
