//! Modelo de selección puro: cursor, ancla para rangos y marcas múltiples.
//!
//! Sin GPUI: se testea con vectores de nombres. Las marcas se guardan por nombre para
//! sobrevivir a refrescos y reordenaciones del listado.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
};

use crate::fs::Entry;

#[derive(Default)]
pub struct Selection {
    /// Fila con el foco de teclado.
    pub cursor: usize,
    /// Origen de la selección por rango (Shift).
    pub anchor: usize,
    /// Selección múltiple por nombre. Vacía = solo `cursor`.
    marked: HashSet<OsString>,
}

impl Selection {
    pub fn is_selected(&self, ix: usize, name: &OsStr) -> bool {
        if self.marked.is_empty() { ix == self.cursor } else { self.marked.contains(name) }
    }

    /// Número de elementos marcados (0 si solo cuenta el cursor).
    pub fn marked_len(&self) -> usize {
        self.marked.len()
    }

    #[cfg(test)]
    pub fn has_marks(&self) -> bool {
        !self.marked.is_empty()
    }

    /// Nombres afectados por una acción, en orden de listado.
    pub fn targets(&self, entries: &[Entry]) -> Vec<OsString> {
        if self.marked.is_empty() {
            entries.get(self.cursor).map(|e| vec![e.name.clone()]).unwrap_or_default()
        } else {
            entries.iter().filter(|e| self.marked.contains(&e.name)).map(|e| e.name.clone()).collect()
        }
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    /// Cursor en `ix`, sin marcas.
    pub fn select_only(&mut self, ix: usize) {
        self.marked.clear();
        self.cursor = ix;
        self.anchor = ix;
    }

    pub fn select_all(&mut self, entries: &[Entry]) {
        self.marked = entries.iter().map(|e| e.name.clone()).collect();
    }

    /// Ctrl/⌘ + clic: añade o quita `ix` sin perder el resto.
    pub fn toggle(&mut self, ix: usize, entries: &[Entry]) {
        let Some(name) = entries.get(ix).map(|e| e.name.clone()) else { return };
        if self.marked.is_empty()
            && let Some(current) = entries.get(self.cursor)
        {
            self.marked.insert(current.name.clone());
        }
        if !self.marked.remove(&name) {
            self.marked.insert(name);
        }
        self.cursor = ix;
        self.anchor = ix;
    }

    /// Shift + clic: rango desde el ancla hasta `ix`.
    pub fn extend_to(&mut self, ix: usize, entries: &[Entry]) {
        if entries.is_empty() {
            return;
        }
        self.cursor = ix.min(entries.len() - 1);
        self.mark_range(entries);
    }

    /// Mueve el cursor `delta` filas (saturando). `extend` = Shift.
    pub fn move_by(&mut self, delta: isize, extend: bool, entries: &[Entry]) {
        if entries.is_empty() {
            return;
        }
        let last = entries.len() as isize - 1;
        self.cursor = (self.cursor as isize).saturating_add(delta).clamp(0, last) as usize;
        if extend {
            self.mark_range(entries);
        } else {
            self.marked.clear();
            self.anchor = self.cursor;
        }
    }

    fn mark_range(&mut self, entries: &[Entry]) {
        let last = entries.len() - 1;
        let (lo, hi) = (self.anchor.min(self.cursor).min(last), self.anchor.max(self.cursor).min(last));
        self.marked = entries[lo..=hi].iter().map(|e| e.name.clone()).collect();
    }

    /// Recoloca la selección tras cambiar el listado.
    ///
    /// - `refresh` = mismo directorio (watcher, F5, tras una operación): conserva marcas y cursor
    ///   por nombre.
    /// - `select` no vacío: esos nombres quedan seleccionados y el primero recibe el cursor.
    /// - `previous` = nombre que tenía el cursor antes del cambio.
    pub fn restore(&mut self, entries: &[Entry], refresh: bool, select: &[OsString], previous: Option<OsString>) {
        let pos = |name: &OsStr| entries.iter().position(|e| e.name == name);
        if refresh {
            if select.is_empty() {
                if !self.marked.is_empty() {
                    let names: HashSet<&OsStr> = entries.iter().map(|e| e.name.as_os_str()).collect();
                    self.marked.retain(|n| names.contains(n.as_os_str()));
                }
            } else {
                self.marked = select.iter().filter(|n| pos(n).is_some()).cloned().collect();
                if self.marked.len() <= 1 {
                    self.marked.clear(); // uno solo = cursor
                }
            }
            let target = select.first().cloned().or(previous);
            self.cursor = target
                .and_then(|n| pos(&n))
                .unwrap_or_else(|| self.cursor.min(entries.len().saturating_sub(1)));
        } else {
            self.marked.clear();
            self.cursor = select.first().and_then(|n| pos(n)).unwrap_or(0);
        }
        self.anchor = self.cursor;
    }

    /// Tras reordenar: el cursor sigue al mismo nombre.
    pub fn follow(&mut self, name: Option<OsString>, entries: &[Entry]) {
        self.cursor = name.and_then(|name| entries.iter().position(|e| e.name == name)).unwrap_or(0);
        self.anchor = self.cursor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::Kind;

    fn entries(names: &[&str]) -> Vec<Entry> {
        names.iter().map(|n| Entry::remote(n, n, Kind::File, None, None)).collect()
    }

    fn names(v: Vec<OsString>) -> Vec<String> {
        v.into_iter().map(|n| n.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn cursor_alone_is_the_target() {
        let list = entries(&["a", "b", "c"]);
        let mut s = Selection::default();
        s.move_by(1, false, &list);
        assert_eq!(names(s.targets(&list)), ["b"]);
        s.move_by(isize::MAX, false, &list);
        assert_eq!(s.cursor, 2);
        s.move_by(isize::MIN, false, &list);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn shift_extends_from_anchor_in_listing_order() {
        let list = entries(&["a", "b", "c", "d"]);
        let mut s = Selection::default();
        s.select_only(2);
        s.move_by(-2, true, &list);
        assert_eq!(names(s.targets(&list)), ["a", "b", "c"]);
        s.extend_to(3, &list);
        assert_eq!(names(s.targets(&list)), ["c", "d"]);
    }

    #[test]
    fn toggle_seeds_with_cursor() {
        let list = entries(&["a", "b", "c"]);
        let mut s = Selection::default();
        s.toggle(2, &list);
        assert_eq!(names(s.targets(&list)), ["a", "c"]);
        s.toggle(0, &list);
        assert_eq!(names(s.targets(&list)), ["c"]);
    }

    #[test]
    fn refresh_keeps_marks_by_name_and_drops_missing() {
        let mut s = Selection::default();
        let before = entries(&["a", "b", "c"]);
        s.select_all(&before);
        let after = entries(&["c", "a", "z"]);
        s.restore(&after, true, &[], Some("a".into()));
        assert_eq!(names(s.targets(&after)), ["c", "a"]);
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn refresh_selects_created_names() {
        let mut s = Selection::default();
        let list = entries(&["a", "b", "c"]);
        s.restore(&list, true, &["c".into()], Some("a".into()));
        assert_eq!(s.cursor, 2);
        assert!(!s.has_marks());
        s.restore(&list, true, &["b".into(), "c".into(), "gone".into()], None);
        assert_eq!(names(s.targets(&list)), ["b", "c"]);
    }

    #[test]
    fn navigation_resets_and_focuses_previous_child() {
        let mut s = Selection::default();
        let list = entries(&["a", "b", "c"]);
        s.select_all(&list);
        s.restore(&list, false, &["b".into()], None);
        assert_eq!(s.cursor, 1);
        assert!(!s.has_marks());
    }

    #[test]
    fn empty_listing_is_safe() {
        let mut s = Selection::default();
        s.move_by(5, true, &[]);
        s.extend_to(3, &[]);
        s.restore(&[], true, &[], Some("x".into()));
        assert_eq!(s.cursor, 0);
        assert!(s.targets(&[]).is_empty());
    }
}
