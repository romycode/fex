//! Editor de una línea para renombrar. Mínimo a propósito: selección simple, sin IME.
//! (Para IME y selección con ratón habría que implementar `EntityInputHandler`, como el
//! ejemplo `input.rs` de GPUI.)

use std::ops::Range;

pub struct LineInput {
    pub text: String,
    /// Índice en bytes, siempre en frontera de carácter.
    pub cursor: usize,
    /// Texto seleccionado: lo que se escriba lo reemplaza (como Nautilus al renombrar).
    pub sel: Option<Range<usize>>,
}

pub enum InputResult {
    Continue,
    Commit,
    Cancel,
}

impl LineInput {
    /// Empieza con `0..sel_end` seleccionado (el nombre sin extensión).
    pub fn new(text: String, sel_end: usize) -> Self {
        let sel_end = sel_end.min(text.len());
        Self { sel: (sel_end > 0).then_some(0..sel_end), cursor: sel_end, text }
    }

    /// (antes, seleccionado, después). Sin selección, el cursor va entre `antes` y `después`.
    pub fn parts(&self) -> (&str, &str, &str) {
        match &self.sel {
            Some(r) => (&self.text[..r.start], &self.text[r.clone()], &self.text[r.end..]),
            None => (&self.text[..self.cursor], "", &self.text[self.cursor..]),
        }
    }

    pub fn select_all(&mut self) {
        self.sel = (!self.text.is_empty()).then(|| 0..self.text.len());
        self.cursor = self.text.len();
    }

    fn prev(&self) -> usize {
        self.text[..self.cursor].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..].chars().next().map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    fn prev_word(&self) -> usize {
        let s = &self.text[..self.cursor];
        let trimmed = s.trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(0, |(i, c)| i + c.len_utf8())
    }

    /// Borra la selección si la hay. Devuelve true si borró algo.
    fn delete_sel(&mut self) -> bool {
        match self.sel.take() {
            Some(r) => {
                self.cursor = r.start;
                self.text.replace_range(r, "");
                true
            }
            None => false,
        }
    }

    pub fn insert(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        self.delete_sel();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    /// `key` es el nombre de tecla de GPUI; `ch` el texto producido (si lo hay).
    pub fn key(&mut self, key: &str, ch: Option<&str>, ctrl: bool) -> InputResult {
        match (ctrl, key) {
            (_, "enter") => return InputResult::Commit,
            (_, "escape") => return InputResult::Cancel,
            (_, "backspace" | "delete") if self.delete_sel() => {}
            (true, "backspace") => {
                let p = self.prev_word();
                self.text.replace_range(p..self.cursor, "");
                self.cursor = p;
            }
            (false, "backspace") => {
                let p = self.prev();
                self.text.replace_range(p..self.cursor, "");
                self.cursor = p;
            }
            (_, "delete") => {
                let n = self.next();
                self.text.replace_range(self.cursor..n, "");
            }
            (_, "left") if self.sel.is_some() => self.cursor = self.sel.take().unwrap().start,
            (_, "right") if self.sel.is_some() => self.cursor = self.sel.take().unwrap().end,
            (true, "left") => self.cursor = self.prev_word(),
            (false, "left") => self.cursor = self.prev(),
            (_, "right") => self.cursor = self.next(),
            (_, "home") => {
                self.sel = None;
                self.cursor = 0
            }
            (_, "end") => {
                self.sel = None;
                self.cursor = self.text.len()
            }
            (false, _) => {
                if let Some(ch) = ch {
                    self.insert(ch);
                }
            }
            _ => {}
        }
        InputResult::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_replaces_stem_keeps_extension() {
        let mut i = LineInput::new("foto vieja.jpg".into(), "foto vieja".len());
        assert_eq!(i.parts(), ("", "foto vieja", ".jpg"));
        i.key("n", Some("n"), false);
        i.key("ñ", Some("ñ"), false);
        assert_eq!(i.text, "nñ.jpg");
        assert_eq!(i.parts(), ("nñ", "", ".jpg"));
    }

    #[test]
    fn editing_utf8() {
        let mut i = LineInput::new("niño.txt".into(), "niño".len());
        i.key("right", None, false); // quita la selección, cursor tras "niño"
        i.key("backspace", None, false);
        assert_eq!(i.text, "niñ.txt");
        i.key("x", Some("ç"), false);
        assert_eq!(i.parts().0, "niñç");
        i.key("left", None, false);
        i.key("delete", None, false);
        assert_eq!(i.text, "niñ.txt");
        i.key("end", None, false);
        i.key("backspace", None, true);
        assert_eq!(i.text, "niñ.");
        i.select_all();
        i.key("backspace", None, false);
        assert_eq!(i.text, "");
    }
}
