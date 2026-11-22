//! Atajos efectivos: los de cada comando más lo que el usuario cambie en el fichero `keymap`.
//!
//! Formato del fichero, una línea por comando:
//!
//! ```text
//! # comentario
//! file.trash = delete, mac:mod-backspace
//! view.hidden = mod-h
//! app.quit =                  # sin atajo
//! ```
//!
//! Lo escrito sustituye los atajos por defecto de ese comando, y tiene prioridad si choca
//! con el atajo por defecto de otro. Las líneas con errores se ignoran y se informan.

use std::collections::HashMap;

use gpui::Keystroke;

use super::{
    Explorer,
    commands::{COMMANDS, Command},
};
use crate::{
    i18n::Lang,
    keymap::{self, Chord, Platform},
};

pub const FILE_NAME: &str = "keymap";

#[derive(Default, Clone)]
pub struct Bindings {
    /// id de comando → atajos del usuario (posiblemente vacío: sin atajo).
    overrides: HashMap<&'static str, Vec<String>>,
}

impl Bindings {
    /// Interpreta el fichero. Devuelve lo válido y un mensaje por cada línea descartada.
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut bindings = Self::default();
        let mut errors = Vec::new();
        for (number, raw) in text.lines().enumerate() {
            let line = raw.split_once('#').map_or(raw, |(code, _)| code).trim();
            if line.is_empty() {
                continue;
            }
            let number = number + 1;
            let Some((id, keys)) = line.split_once('=') else {
                errors.push(format!("{number}: falta '=' en «{line}»"));
                continue;
            };
            let id = id.trim();
            let Some(command) = COMMANDS.iter().find(|c| c.id == id) else {
                errors.push(format!("{number}: comando desconocido «{id}»"));
                continue;
            };
            let keys: Vec<String> =
                keys.split(',').map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned).collect();
            let invalid: Vec<&String> = keys
                .iter()
                .filter(|spec| [Platform::Linux, Platform::Mac].iter().all(|p| Chord::parse(spec, *p).is_none()))
                .collect();
            if !invalid.is_empty() {
                errors.push(format!("{number}: atajo no válido {invalid:?}"));
                continue;
            }
            bindings.overrides.insert(command.id, keys);
        }
        (bindings, errors)
    }

    pub fn is_overridden(&self, command: &Command) -> bool {
        self.overrides.contains_key(command.id)
    }

    /// Atajos efectivos de un comando (de todas las plataformas; `Chord::parse` filtra).
    pub fn keys<'a>(&'a self, command: &'a Command) -> Vec<&'a str> {
        match self.overrides.get(command.id) {
            Some(keys) => keys.iter().map(String::as_str).collect(),
            None => command.keys.to_vec(),
        }
    }

    pub fn label_on(&self, command: &Command, lang: Lang, platform: Platform) -> String {
        keymap::shortcut_label_on(&self.keys(command), lang, platform)
    }

    pub fn label(&self, command: &Command, lang: Lang) -> String {
        self.label_on(command, lang, Platform::CURRENT)
    }

    fn matches(&self, command: &Command, keystroke: &Keystroke, platform: Platform) -> bool {
        keymap::matches_any_on(&self.keys(command), keystroke, platform)
    }

    /// Comando que corresponde a la pulsación: primero los que el usuario ha cambiado y luego
    /// el resto, en orden de [`COMMANDS`]. `available` filtra por `when`.
    pub fn resolve(
        &self,
        keystroke: &Keystroke,
        platform: Platform,
        available: impl Fn(&Command) -> bool,
    ) -> Option<&'static Command> {
        let (custom, default): (Vec<_>, Vec<_>) = COMMANDS.iter().partition(|c| self.is_overridden(c));
        custom
            .into_iter()
            .chain(default)
            .find(|command| self.matches(command, keystroke, platform) && available(command))
    }

    pub fn find(&self, explorer: &Explorer, keystroke: &Keystroke) -> Option<&'static Command> {
        self.resolve(keystroke, Platform::CURRENT, |command| (command.when)(explorer))
    }
}

/// Plantilla con todos los comandos y sus atajos por defecto, comentados.
/// Es lo que imprime `fex --list-commands` y con lo que se crea el fichero la primera vez.
pub fn template(lang: Lang) -> String {
    let mut out = String::from(
        "# Atajos de fex. Descomenta una línea y cámbiala para sustituir los atajos del comando.\n\
         # `mod` es Ctrl en Linux y ⌘ en macOS. Prefijos `linux:` y `mac:` para una sola plataforma.\n\
         # Teclas: a-z, 0-9, f1-f12, enter, escape, backspace, delete, up, down, left, right,\n\
         #         home, end, pageup, pagedown, space, tab. `comando =` sin nada lo deja sin atajo.\n\n",
    );
    let width = COMMANDS.iter().map(|c| c.id.len()).max().unwrap_or(0);
    for command in COMMANDS {
        let label = (command.label)(lang.t());
        let comment = if label.is_empty() { String::new() } else { format!("  # {label}") };
        out.push_str(&format!("# {:width$} = {}{comment}\n", command.id, command.keys.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Platform::{Linux, Mac};

    fn press(spec: &'static str) -> Keystroke {
        Keystroke::parse(spec).unwrap()
    }

    #[test]
    fn overrides_replace_defaults_and_report_bad_lines() {
        let (b, errors) = Bindings::parse(
            "# comentario\n\
             file.trash = mod-t  # en línea\n\
             app.quit =\n\
             nope.cmd = mod-x\n\
             view.refresh = hyper-r\n\
             sin igual\n",
        );
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].starts_with("4:") && errors[1].starts_with("5:") && errors[2].starts_with("6:"));
        let trash = super::super::commands::find("file.trash");
        assert_eq!(b.keys(trash), ["mod-t"]);
        assert_eq!(b.label_on(trash, Lang::En, Linux), "Ctrl+T");
        assert_eq!(b.label_on(trash, Lang::En, Mac), "⌘T");
        let quit = super::super::commands::find("app.quit");
        assert!(b.keys(quit).is_empty());
        assert_eq!(b.label_on(quit, Lang::En, Linux), "");
        // El resto conserva sus atajos.
        let refresh = super::super::commands::find("view.refresh");
        assert_eq!(b.keys(refresh), refresh.keys);
    }

    #[test]
    fn user_bindings_win_conflicts() {
        // `t` es el tema por defecto; el usuario lo asigna a la papelera.
        let (b, errors) = Bindings::parse("file.trash = t");
        assert!(errors.is_empty());
        let hit = b.resolve(&press("t"), Linux, |_| true).unwrap();
        assert_eq!(hit.id, "file.trash");
        // Si el suyo no está disponible, cae al comando por defecto.
        let hit = b.resolve(&press("t"), Linux, |c| c.id != "file.trash").unwrap();
        assert_eq!(hit.id, "view.theme");
        // Ya no responde a Supr.
        assert_ne!(b.resolve(&press("delete"), Linux, |_| true).map(|c| c.id), Some("file.trash"));
    }

    #[test]
    fn defaults_resolve_per_platform() {
        let b = Bindings::default();
        assert_eq!(b.resolve(&press("cmd-backspace"), Mac, |_| true).unwrap().id, "file.trash");
        assert_eq!(b.resolve(&press("ctrl-h"), Linux, |_| true).unwrap().id, "view.hidden");
        assert!(b.resolve(&press("cmd-h"), Mac, |_| true).is_none());
    }

    #[test]
    fn template_round_trips() {
        let template = template(Lang::En);
        let (b, errors) = Bindings::parse(&template);
        assert!(errors.is_empty() && b.overrides.is_empty(), "todo comentado: {errors:?}");
        let uncommented: String = template
            .lines()
            .filter(|l| l.starts_with("# ") && l.contains(" = "))
            .map(|l| format!("{}\n", &l[2..]))
            .collect();
        let (b, errors) = Bindings::parse(&uncommented);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(b.overrides.len(), COMMANDS.len());
        for command in COMMANDS {
            assert_eq!(b.keys(command), command.keys, "{}", command.id);
        }
    }
}
