//! Formulario para añadir o editar una conexión S3. Lógica pura (sin GPUI): campos, foco,
//! teclado y validación. El render vive en `view/connection.rs`.

use crate::{
    i18n::Strings,
    input::{InputResult, LineInput},
    s3::connection::{Connection, Credentials, expand_home},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Name,
    Profile,
    AccessKey,
    SecretKey,
    SessionToken,
    Endpoint,
    Region,
    CaFile,
}

/// Filas que reciben el foco, en orden de tabulación.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Text(Field),
    /// Credenciales del sistema ↔ claves propias.
    UseKeys,
    PathStyle,
}

pub enum Outcome {
    Continue,
    Save,
    Cancel,
}

pub struct ConnectionForm {
    /// Nombre de la conexión que se edita (`None` = nueva).
    pub original: Option<String>,
    pub use_keys: bool,
    pub path_style: bool,
    inputs: Vec<(Field, LineInput)>,
    pub focus: usize,
    pub error: Option<String>,
}

const FIELDS: [Field; 8] = [
    Field::Name,
    Field::Profile,
    Field::AccessKey,
    Field::SecretKey,
    Field::SessionToken,
    Field::Endpoint,
    Field::Region,
    Field::CaFile,
];

impl ConnectionForm {
    pub fn new(existing: Option<&Connection>, suggested_name: String) -> Self {
        let value = |field: Field| -> String {
            let Some(c) = existing else {
                return if field == Field::Name { suggested_name.clone() } else { String::new() };
            };
            match (field, &c.credentials) {
                (Field::Name, _) => c.name.clone(),
                (Field::Profile, Credentials::System { profile }) => profile.clone().unwrap_or_default(),
                (Field::AccessKey, Credentials::Keys { access_key, .. }) => access_key.clone(),
                (Field::SecretKey, Credentials::Keys { secret_key, .. }) => secret_key.clone(),
                (Field::SessionToken, Credentials::Keys { session_token, .. }) => {
                    session_token.clone().unwrap_or_default()
                }
                (Field::Endpoint, _) => c.endpoint.clone().unwrap_or_default(),
                (Field::Region, _) => c.region.clone().unwrap_or_default(),
                (Field::CaFile, _) => c.ca_file.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                _ => String::new(),
            }
        };
        let mut inputs: Vec<_> = FIELDS.iter().map(|&f| (f, LineInput::new(value(f), 0))).collect();
        inputs[0].1.select_all();
        Self {
            original: existing.map(|c| c.name.clone()),
            use_keys: existing.is_some_and(|c| matches!(c.credentials, Credentials::Keys { .. })),
            path_style: existing.is_some_and(|c| c.path_style),
            inputs,
            focus: 0,
            error: None,
        }
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut rows = vec![Row::Text(Field::Name), Row::UseKeys];
        if self.use_keys {
            rows.extend([Field::AccessKey, Field::SecretKey, Field::SessionToken].map(Row::Text));
        } else {
            rows.push(Row::Text(Field::Profile));
        }
        rows.extend([Row::Text(Field::Endpoint), Row::Text(Field::Region), Row::PathStyle, Row::Text(Field::CaFile)]);
        rows
    }

    pub fn focused(&self) -> Row {
        let rows = self.rows();
        rows[self.focus.min(rows.len() - 1)]
    }

    pub fn input(&self, field: Field) -> &LineInput {
        &self.inputs.iter().find(|(f, _)| *f == field).expect("todos los campos existen").1
    }

    fn input_mut(&mut self, field: Field) -> &mut LineInput {
        &mut self.inputs.iter_mut().find(|(f, _)| *f == field).expect("todos los campos existen").1
    }

    fn value(&self, field: Field) -> Option<String> {
        Some(self.input(field).text.trim().to_owned()).filter(|v| !v.is_empty())
    }

    pub fn move_focus(&mut self, delta: isize) {
        let count = self.rows().len() as isize;
        self.focus = (self.focus as isize + delta).rem_euclid(count) as usize;
        if let Row::Text(field) = self.focused() {
            self.input_mut(field).select_all();
        }
    }

    pub fn focus_row(&mut self, row: Row) {
        if let Some(index) = self.rows().iter().position(|r| *r == row) {
            self.focus = index;
        }
    }

    pub fn toggle(&mut self, row: Row) {
        match row {
            Row::UseKeys => self.use_keys = !self.use_keys,
            Row::PathStyle => self.path_style = !self.path_style,
            Row::Text(_) => {}
        }
        self.focus_row(row);
    }

    /// Pegar texto en el campo con el foco (una línea).
    pub fn insert(&mut self, text: &str) {
        if let Row::Text(field) = self.focused() {
            self.input_mut(field).insert(text.lines().next().unwrap_or(""));
        }
    }

    pub fn select_all(&mut self) {
        if let Row::Text(field) = self.focused() {
            self.input_mut(field).select_all();
        }
    }

    /// Tab/⇧Tab y flechas cambian de fila; Espacio alterna; Enter guarda; Esc cancela.
    pub fn key(&mut self, key: &str, shift: bool, ch: Option<&str>, ctrl: bool) -> Outcome {
        let row = self.focused();
        match key {
            "tab" => self.move_focus(if shift { -1 } else { 1 }),
            "down" => self.move_focus(1),
            "up" => self.move_focus(-1),
            "enter" => return Outcome::Save,
            "escape" => return Outcome::Cancel,
            "space" if matches!(row, Row::UseKeys | Row::PathStyle) => self.toggle(row),
            _ => {
                if let Row::Text(field) = row
                    && let InputResult::Cancel = self.input_mut(field).key(key, ch, ctrl)
                {
                    return Outcome::Cancel;
                }
            }
        }
        self.error = None;
        Outcome::Continue
    }

    /// Valida y construye la conexión. `others` = nombres de las demás conexiones.
    pub fn build(&self, others: &[&str], s: &Strings) -> Result<Connection, String> {
        let name = self.value(Field::Name).ok_or_else(|| s.s3_name_required.to_owned())?;
        if name.contains(['[', ']']) {
            return Err(s.s3_name_required.to_owned());
        }
        if others.contains(&name.as_str()) {
            return Err(format!("{}: {name}", s.s3_name_taken));
        }
        let credentials = if self.use_keys {
            match (self.value(Field::AccessKey), self.value(Field::SecretKey)) {
                (Some(access_key), Some(secret_key)) => {
                    Credentials::Keys { access_key, secret_key, session_token: self.value(Field::SessionToken) }
                }
                _ => return Err(s.s3_keys_required.to_owned()),
            }
        } else {
            Credentials::System { profile: self.value(Field::Profile) }
        };
        let endpoint = self.value(Field::Endpoint);
        if endpoint.as_ref().is_some_and(|e| !(e.starts_with("http://") || e.starts_with("https://"))) {
            return Err(s.s3_endpoint_invalid.to_owned());
        }
        Ok(Connection {
            name,
            credentials,
            endpoint,
            region: self.value(Field::Region),
            path_style: self.path_style,
            ca_file: self.value(Field::CaFile).map(|p| expand_home(&p)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;

    fn type_text(form: &mut ConnectionForm, text: &str) {
        form.select_all();
        form.insert(text);
    }

    #[test]
    fn builds_a_custom_endpoint_with_keys() {
        let s = Lang::En.t();
        let mut form = ConnectionForm::new(None, "S3".into());
        type_text(&mut form, "Emulador");
        form.key("tab", false, None, false);
        assert_eq!(form.focused(), Row::UseKeys);
        form.key("space", false, Some(" "), false);
        assert!(form.use_keys);
        form.key("tab", false, None, false);
        assert_eq!(form.focused(), Row::Text(Field::AccessKey));
        assert_eq!(form.build(&[], s).unwrap_err(), s.s3_keys_required);
        type_text(&mut form, "AKIA");
        form.key("tab", false, None, false);
        type_text(&mut form, "secreto");
        form.focus_row(Row::Text(Field::Endpoint));
        type_text(&mut form, "127.0.0.1:9297");
        assert_eq!(form.build(&[], s).unwrap_err(), s.s3_endpoint_invalid);
        type_text(&mut form, "https://127.0.0.1:9297");
        form.toggle(Row::PathStyle);
        let c = form.build(&["AWS"], s).unwrap();
        assert_eq!(c.name, "Emulador");
        assert_eq!(c.credentials, Credentials::Keys { access_key: "AKIA".into(), secret_key: "secreto".into(), session_token: None });
        assert_eq!(c.endpoint.as_deref(), Some("https://127.0.0.1:9297"));
        assert!(c.path_style);
        assert_eq!(form.build(&["Emulador"], s).unwrap_err(), format!("{}: Emulador", s.s3_name_taken));
    }

    #[test]
    fn editing_keeps_values_and_tab_wraps() {
        let existing = Connection {
            name: "prod".into(),
            credentials: Credentials::System { profile: Some("prod".into()) },
            endpoint: None,
            region: Some("eu-west-1".into()),
            path_style: false,
            ca_file: None,
        };
        let form = ConnectionForm::new(Some(&existing), String::new());
        assert_eq!(form.build(&[], Lang::Es.t()).unwrap(), existing);
        let mut form = form;
        let rows = form.rows().len();
        form.key("tab", true, None, false);
        assert_eq!(form.focus, rows - 1, "⇧Tab desde la primera fila va a la última");
        assert!(matches!(form.key("escape", false, None, false), Outcome::Cancel));
        assert!(matches!(form.key("enter", false, None, false), Outcome::Save));
    }
}
