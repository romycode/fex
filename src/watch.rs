//! Vigila el directorio actual con el backend nativo de `notify` y avisa por un canal.
//! Se crea perezosamente tras el primer listado: no cuesta nada en el arranque.

use std::path::{Path, PathBuf};

use futures::channel::mpsc::UnboundedSender;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub struct DirWatcher {
    inner: RecommendedWatcher,
    current: Option<PathBuf>,
}

impl DirWatcher {
    pub fn new(tx: UnboundedSender<()>) -> notify::Result<Self> {
        let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            // Los accesos (abrir/leer) no cambian el listado.
            if let Ok(ev) = res
                && !matches!(ev.kind, EventKind::Access(_))
            {
                let _ = tx.unbounded_send(());
            }
        })?;
        Ok(Self { inner, current: None })
    }

    /// Cambia el directorio vigilado (no recursivo: solo interesa el listado visible).
    pub fn watch(&mut self, dir: &Path) {
        if self.current.as_deref() == Some(dir) {
            return;
        }
        if let Some(old) = self.current.take() {
            let _ = self.inner.unwatch(&old);
        }
        if self.inner.watch(dir, RecursiveMode::NonRecursive).is_ok() {
            self.current = Some(dir.to_path_buf());
        }
    }
}
