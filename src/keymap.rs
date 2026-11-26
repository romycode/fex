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
//!
//! La plataforma es un parámetro y no un `cfg!`: así los tests de macOS también corren en
//! Linux y al revés.

use gpui::{Keystroke, Modifiers};

use crate::i18n::Lang;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Linux,
    Mac,
}

impl Platform {
    pub const CURRENT: Self = if cfg!(target_os = "macos") { Self::Mac } else { Self::Linux };

    /// ¿Está pulsado el modificador de comandos (Ctrl en Linux, ⌘ en macOS)?
    /// En Linux, Super nunca cuenta como comando.
    pub fn command(self, m: &Modifiers) -> bool {
        match self {
            Self::Mac => m.platform,
            Self::Linux => m.control && !m.platform,
        }
    }

    /// Modificador para añadir o quitar de la selección con clic (Ctrl en Linux, ⌘ en macOS).
    pub fn toggle_click(self, m: &Modifiers) -> bool {
        match self {
            Self::Mac => m.platform,
            Self::Linux => m.control,
        }
    }

    /// Programa que abre ficheros con la aplicación predeterminada.
    pub fn opener(self) -> &'static str {
        match self {
            Self::Mac => "/usr/bin/open",
            Self::Linux => "xdg-open",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Chord<'a> {
    pub command: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: &'a str,
}

impl<'a> Chord<'a> {
    /// `None` si el atajo es de otra plataforma o está mal escrito.
    pub fn parse(spec: &'a str, platform: Platform) -> Option<Self> {
        let spec = match (spec.split_once(':'), platform) {
            (Some(("linux", rest)), Platform::Linux) | (Some(("mac", rest)), Platform::Mac) => rest,
            (Some(("linux" | "mac", _)), _) => return None,
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
    pub fn matches(&self, keystroke: &Keystroke, platform: Platform) -> bool {
        let m = &keystroke.modifiers;
        let modifiers = match platform {
            Platform::Mac => m.platform == self.command && m.control == self.ctrl,
            // En Linux `mod` y `ctrl` son Control. Super nunca forma parte de un atajo.
            Platform::Linux => !m.platform && m.control == (self.command || self.ctrl),
        };
        modifiers && m.alt == self.alt && m.shift == self.shift && keystroke.key.eq_ignore_ascii_case(self.key)
    }

    /// Etiqueta para mostrar: `Ctrl+Mayús+N` en Linux, `⇧⌘N` en macOS.
    pub fn label(&self, lang: Lang, platform: Platform) -> String {
        let key = key_label(self.key, lang, platform);
        match platform {
            Platform::Mac => {
                let mut out = String::new();
                for (on, symbol) in [(self.ctrl, "⌃"), (self.alt, "⌥"), (self.shift, "⇧"), (self.command, "⌘")] {
                    if on {
                        out.push_str(symbol);
                    }
                }
                out + &key
            }
            Platform::Linux => {
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
}

fn key_label(key: &str, lang: Lang, platform: Platform) -> String {
    let mac = platform == Platform::Mac;
    let named = match key {
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "enter" => "⏎",
        "escape" if mac => "⎋",
        "escape" => "Esc",
        "space" => "Space",
        "tab" => "Tab",
        "backspace" if mac => "⌫",
        "backspace" => "Backspace",
        "delete" if mac => "⌦",
        "delete" => match lang {
            Lang::En => "Del",
            Lang::Es | Lang::Ca => "Supr",
        },
        "pageup" if mac => "⇞",
        "pageup" => "PgUp",
        "pagedown" if mac => "⇟",
        "pagedown" => "PgDn",
        "home" if mac => "↖",
        "home" => "Home",
        "end" if mac => "↘",
        "end" => "End",
        _ => return key.to_uppercase(),
    };
    named.to_owned()
}

/// Primer atajo válido en la plataforma, ya formateado. Vacío si no hay.
pub fn shortcut_label_on(specs: &[&str], lang: Lang, platform: Platform) -> String {
    specs
        .iter()
        .find_map(|spec| Chord::parse(spec, platform))
        .map(|chord| chord.label(lang, platform))
        .unwrap_or_default()
}

pub fn matches_any_on(specs: &[&str], keystroke: &Keystroke, platform: Platform) -> bool {
    specs.iter().filter_map(|spec| Chord::parse(spec, platform)).any(|chord| chord.matches(keystroke, platform))
}


#[cfg(test)]
mod tests {
    use super::{Platform::*, *};

    fn press(spec: &'static str) -> Keystroke {
        Keystroke::parse(spec).unwrap()
    }

    #[test]
    fn parses_modifiers_and_dash_key() {
        let c = Chord::parse("mod-shift-n", Linux).unwrap();
        assert!(c.command && c.shift && !c.alt && !c.ctrl);
        assert_eq!(c.key, "n");
        assert_eq!(Chord::parse("ctrl--", Linux).unwrap().key, "-");
        assert_eq!(Chord::parse("-", Mac).unwrap().key, "-");
        assert!(Chord::parse("hyper-x", Linux).is_none());
        assert!(Chord::parse("", Linux).is_none());
    }

    #[test]
    fn platform_prefix_filters() {
        assert!(Chord::parse("linux:delete", Linux).is_some());
        assert!(Chord::parse("linux:delete", Mac).is_none());
        assert!(Chord::parse("mac:delete", Mac).is_some());
        assert!(Chord::parse("mac:delete", Linux).is_none());
    }

    #[test]
    fn linux_matching_is_exact() {
        let on = |spec, key| matches_any_on(&[spec], &press(key), Linux);
        assert!(on("mod-c", "ctrl-c"));
        assert!(!on("mod-c", "ctrl-shift-c"));
        assert!(!on("mod-c", "c"));
        assert!(!on("mod-c", "super-c"));
        assert!(!on("mod-c", "cmd-c"));
        assert!(on("down", "down"));
        assert!(!on("down", "shift-down"));
        assert!(on("shift-down", "shift-down"));
        assert!(on("alt-up", "alt-up"));
        assert!(!on("mac:mod-backspace", "ctrl-backspace"));
    }

    #[test]
    fn mac_matching_uses_command_not_control() {
        let on = |spec, key| matches_any_on(&[spec], &press(key), Mac);
        assert!(on("mod-c", "cmd-c"));
        assert!(!on("mod-c", "ctrl-c"), "en macOS Ctrl+C no es copiar");
        assert!(on("ctrl-c", "ctrl-c"));
        assert!(!on("mod-c", "cmd-ctrl-c"));
        assert!(on("mac:mod-backspace", "cmd-backspace"));
        assert!(on("mac:mod-shift-backspace", "cmd-shift-backspace"));
        assert!(!on("linux:mod-h", "cmd-h"), "⌘H es ocultar la app en macOS");
        assert!(on("mod-shift-.", "cmd-shift-."));
        assert!(on("alt-up", "alt-up"));
    }

    #[test]
    fn labels_per_platform() {
        let label = |specs: &[&str], lang, platform| shortcut_label_on(specs, lang, platform);
        assert_eq!(label(&["mod-shift-n"], Lang::En, Linux), "Ctrl+Shift+N");
        assert_eq!(label(&["mod-shift-n"], Lang::Es, Linux), "Ctrl+Mayús+N");
        assert_eq!(label(&["mod-shift-n"], Lang::Es, Mac), "⇧⌘N");
        assert_eq!(label(&["mac:mod-backspace", "delete"], Lang::Es, Linux), "Supr");
        assert_eq!(label(&["mac:mod-backspace", "delete"], Lang::Es, Mac), "⌘⌫");
        assert_eq!(label(&["alt-up"], Lang::En, Linux), "Alt+↑");
        assert_eq!(label(&["alt-up"], Lang::En, Mac), "⌥↑");
        assert_eq!(label(&["linux:mod-h", "mac:mod-shift-."], Lang::En, Mac), "⇧⌘.");
        assert_eq!(label(&[], Lang::En, Mac), "");
    }

    #[test]
    fn command_modifier_per_platform() {
        let ctrl = press("ctrl-a").modifiers;
        let cmd = press("cmd-a").modifiers;
        assert!(Linux.command(&ctrl) && !Linux.command(&cmd));
        assert!(Mac.command(&cmd) && !Mac.command(&ctrl));
        assert!(Linux.toggle_click(&ctrl) && Mac.toggle_click(&cmd) && !Mac.toggle_click(&ctrl));
    }
}
