//! Atajos de teclado como datos.
//!
//! Formato: `"[linux:|mac:]mod-ctrl-alt-shift-<tecla>"`.
//! - `mod` es Ctrl en Linux y ⌘ en macOS: casi todos los atajos deberían usarlo.
//! - `ctrl` es Control literal (en macOS, ⌃).
//! - El prefijo `linux:`/`mac:` limita el atajo a una plataforma.
//! - `<tecla>` es el nombre de GPUI: `a`, `.`, `enter`, `escape`, `up`, `f2`, `pagedown`…
//!
//! La misma cadena sirve para casar la pulsación y para pintar la etiqueta del atajo
//! (menús, tooltips, paleta), así nunca se desincronizan.

use gpui::Keystroke;

use crate::i18n::Lang;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chord<'a> {
    pub command: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: &'a str,
}

const MAC: bool = cfg!(target_os = "macos");

impl<'a> Chord<'a> {
    /// `None` si el atajo es de otra plataforma o está mal escrito.
    pub fn parse(spec: &'a str) -> Option<Self> {
        let spec = match spec.split_once(':') {
            Some(("linux", rest)) if !MAC => rest,
            Some(("mac", rest)) if MAC => rest,
            Some(("linux" | "mac", _)) => return None,
            _ => spec,
        };
        let mut chord = Chord { command: false, ctrl: false, alt: false, shift: false, key: "" };
        let mut rest = spec;
        // La tecla puede ser "-", así que se consume prefijo a prefijo.
        loop {
            let Some((head, tail)) = rest.split_once('-') else { break };
            if tail.is_empty() {
                break;
            }
            match head {
                "mod" => chord.command = true,
                "ctrl" => chord.ctrl = true,
                "alt" => chord.alt = true,
                "shift" => chord.shift = true,
                _ => return None,
            }
            rest = tail;
        }
        chord.key = rest;
        (!rest.is_empty()).then_some(chord)
    }

    /// Casa con los modificadores exactos: `shift-down` no dispara `down`.
    pub fn matches(&self, keystroke: &Keystroke) -> bool {
        let m = &keystroke.modifiers;
        let modifiers = if MAC {
            m.platform == self.command && m.control == self.ctrl
        } else {
            // En Linux `mod` y `ctrl` son Control. Super nunca forma parte de un atajo.
            !m.platform && m.control == (self.command || self.ctrl)
        };
        modifiers
            && m.alt == self.alt
            && m.shift == self.shift
            && keystroke.key.eq_ignore_ascii_case(self.key)
    }

    /// Etiqueta para mostrar: `Ctrl+Mayús+N` en Linux, `⇧⌘N` en macOS.
    pub fn label(&self, lang: Lang) -> String {
        let key = key_label(self.key, lang);
        if MAC {
            let mut out = String::new();
            for (on, symbol) in [(self.ctrl, "⌃"), (self.alt, "⌥"), (self.shift, "⇧"), (self.command, "⌘")] {
                if on {
                    out.push_str(symbol);
                }
            }
            out + &key
        } else {
            let shift = match lang {
                Lang::En => "Shift",
                Lang::Es => "Mayús",
                Lang::Ca => "Maj",
            };
            let mut parts = Vec::new();
            for (on, name) in [(self.command || self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, shift)] {
                if on {
                    parts.push(name.to_owned());
                }
            }
            parts.push(key);
            parts.join("+")
        }
    }
}

fn key_label(key: &str, lang: Lang) -> String {
    let named = match key {
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "enter" => "⏎",
        "escape" => "Esc",
        "space" => "Space",
        "tab" => "Tab",
        "backspace" if MAC => "⌫",
        "backspace" => "Backspace",
        "delete" if MAC => "⌦",
        "delete" => match lang {
            Lang::En => "Del",
            Lang::Es | Lang::Ca => "Supr",
        },
        "pageup" => "PgUp",
        "pagedown" => "PgDn",
        "home" => "Home",
        "end" => "End",
        _ => return key.to_uppercase(),
    };
    named.to_owned()
}

/// Primer atajo válido en esta plataforma, ya formateado. Vacío si no hay.
pub fn shortcut_label(specs: &[&str], lang: Lang) -> String {
    specs.iter().find_map(|spec| Chord::parse(spec)).map(|chord| chord.label(lang)).unwrap_or_default()
}

pub fn matches_any(specs: &[&str], keystroke: &Keystroke) -> bool {
    specs.iter().filter_map(|spec| Chord::parse(spec)).any(|chord| chord.matches(keystroke))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(spec: &'static str) -> Keystroke {
        Keystroke::parse(spec).unwrap()
    }

    #[test]
    fn parses_modifiers_and_dash_key() {
        let c = Chord::parse("mod-shift-n").unwrap();
        assert!(c.command && c.shift && !c.alt && !c.ctrl);
        assert_eq!(c.key, "n");
        assert_eq!(Chord::parse("ctrl--").unwrap().key, "-");
        assert_eq!(Chord::parse("-").unwrap().key, "-");
        assert!(Chord::parse("hyper-x").is_none());
        assert!(Chord::parse("").is_none());
    }

    #[test]
    fn platform_prefix_filters() {
        let other = if MAC { "linux:delete" } else { "mac:delete" };
        let this = if MAC { "mac:delete" } else { "linux:delete" };
        assert!(Chord::parse(other).is_none());
        assert!(Chord::parse(this).is_some());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_matching_is_exact() {
        let copy = Chord::parse("mod-c").unwrap();
        assert!(copy.matches(&press("ctrl-c")));
        assert!(!copy.matches(&press("ctrl-shift-c")));
        assert!(!copy.matches(&press("c")));
        assert!(!copy.matches(&press("super-c")));
        let down = Chord::parse("down").unwrap();
        assert!(down.matches(&press("down")));
        assert!(!down.matches(&press("shift-down")));
        assert!(Chord::parse("shift-down").unwrap().matches(&press("shift-down")));
        assert!(Chord::parse("alt-up").unwrap().matches(&press("alt-up")));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_labels() {
        assert_eq!(shortcut_label(&["mod-shift-n"], Lang::En), "Ctrl+Shift+N");
        assert_eq!(shortcut_label(&["mod-shift-n"], Lang::Es), "Ctrl+Mayús+N");
        assert_eq!(shortcut_label(&["mac:mod-backspace", "linux:delete"], Lang::Es), "Supr");
        assert_eq!(shortcut_label(&["alt-up"], Lang::En), "Alt+↑");
        assert_eq!(shortcut_label(&[], Lang::En), "");
    }
}
