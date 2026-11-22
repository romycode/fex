//! Conexiones S3: cargarlas, explorarlas, añadirlas, editarlas y borrarlas.

use gpui::{AppContext, Context};

use super::{Explorer, Modal, connection_form::ConnectionForm, jobs::Notice};
use crate::{
    backend::Location,
    s3::{self, connection},
};

impl Explorer {
    /// Lee el fichero de conexiones en background. Sin fichero: la conexión del sistema.
    pub(super) fn load_s3_connections(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async { connection::load() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((connections, errors)) => {
                        this.set_connections(connections);
                        if let Some(first) = errors.first() {
                            let message = format!("{} {first}", this.lang.t().s3_connections_error);
                            this.set_notice(Notice::Error(message), cx);
                        }
                    }
                    Err(error) => {
                        let message = format!("{} {error}", this.lang.t().s3_connections_error);
                        this.set_notice(Notice::Error(message), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_connections(&mut self, connections: Vec<connection::Connection>) {
        s3::set_connections(connections.clone());
        self.s3_connections = connections;
    }

    pub fn open_connection(&mut self, name: String, cx: &mut Context<Self>) {
        self.navigate_to(Location::S3(s3::Location::Buckets { connection: name }), None, cx);
    }

    /// "Explorar Amazon S3": la primera conexión, o el formulario si no hay ninguna.
    pub fn browse_s3(&mut self, cx: &mut Context<Self>) {
        match self.s3_connections.first() {
            Some(first) => self.open_connection(first.name.clone(), cx),
            None => self.add_connection(cx),
        }
    }

    pub fn add_connection(&mut self, cx: &mut Context<Self>) {
        let taken = |name: &str| self.s3_connections.iter().any(|c| c.name == name);
        let name = crate::ops::unique_name_by("S3", true, None, taken);
        self.modal = Modal::Connection(ConnectionForm::new(None, name));
        self.menu = None;
        cx.notify();
    }

    pub fn edit_connection(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(existing) = self.s3_connections.iter().find(|c| c.name == name) else { return };
        self.modal = Modal::Connection(ConnectionForm::new(Some(existing), String::new()));
        self.menu = None;
        cx.notify();
    }

    pub fn edit_current_connection(&mut self, cx: &mut Context<Self>) {
        if let Location::S3(location) = &self.source {
            let name = location.connection().to_owned();
            self.edit_connection(&name, cx);
        }
    }

    pub fn save_connection(&mut self, cx: &mut Context<Self>) {
        let Modal::Connection(form) = &mut self.modal else { return };
        let others: Vec<&str> = self
            .s3_connections
            .iter()
            .map(|c| c.name.as_str())
            .filter(|name| Some(*name) != form.original.as_deref())
            .collect();
        let connection = match form.build(&others, self.lang.t()) {
            Ok(connection) => connection,
            Err(error) => {
                form.error = Some(error);
                cx.notify();
                return;
            }
        };
        let original = form.original.clone();
        let mut connections = self.s3_connections.clone();
        match original.as_ref().and_then(|name| connections.iter().position(|c| &c.name == name)) {
            Some(index) => connections[index] = connection.clone(),
            None => connections.push(connection.clone()),
        }
        self.modal = Modal::None;
        self.persist_connections(connections, cx);
        // Si se estaba explorando esta conexión, seguir en ella (quizá con otro nombre o claves).
        let browsing = matches!(&self.source, Location::S3(l) if Some(l.connection()) == original.as_deref());
        if browsing || original.is_none() {
            self.open_connection(connection.name, cx);
        }
    }

    pub fn delete_connection(&mut self, cx: &mut Context<Self>) {
        let Modal::Connection(form) = &self.modal else { return };
        let Some(name) = form.original.clone() else {
            self.modal = Modal::None;
            cx.notify();
            return;
        };
        let connections = self.s3_connections.iter().filter(|c| c.name != name).cloned().collect();
        self.modal = Modal::None;
        self.persist_connections(connections, cx);
        if matches!(&self.source, Location::S3(l) if l.connection() == name) {
            self.navigate(self.cwd.clone(), None, cx);
        }
        cx.notify();
    }

    /// Aplica en memoria al momento y guarda en disco en background.
    fn persist_connections(&mut self, connections: Vec<connection::Connection>, cx: &mut Context<Self>) {
        self.set_connections(connections.clone());
        let task = cx.background_spawn(async move { connection::save(&connections) });
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    let message = format!("{} {error}", this.lang.t().s3_connections_error);
                    this.set_notice(Notice::Error(message), cx);
                })
                .ok();
            }
        })
        .detach();
    }
}
