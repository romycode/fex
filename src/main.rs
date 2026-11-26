//! fex — explorador de archivos con GPUI.
//!
//! Reglas para llegar al primer frame rápido:
//! - La ventana se abre y pinta ANTES de leer el directorio.
//! - El listado usa solo `getdents` (d_type vía `file_type()`), sin `stat`.
//! - Tamaños y fechas se rellenan después, en segundo plano.
//! - El watcher se crea tras el primer listado.
//! - Nada de gvfs, temas de iconos ni miniaturas.
//!
//! Arquitectura: ver `ARCHITECTURE.md`. La aplicación vive en [`app`]; el resto de módulos
//! son dominio puro (sin GPUI) y se testean por separado.

mod app;
mod backend;
mod config;
mod fs;
mod i18n;
mod icons;
mod input;
mod keymap;
#[cfg(target_os = "macos")]
mod macos;
mod ops;
mod s3;
mod safe_fs;
mod sidebar;
mod theme;
mod undo;
mod watch;

#[cfg(all(test, feature = "ui-tests", target_os = "linux"))]
mod ui_tests;

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::OnceLock,
    time::Instant,
};

use gpui::{App, AppContext, Bounds, WindowBackgroundAppearance, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

use app::Explorer;
use i18n::Lang;
use icons::Assets;
use theme::Mode;

static T0: OnceLock<Instant> = OnceLock::new();

fn since_start_ms() -> f64 {
    T0.get().map(|t| t.elapsed()).unwrap_or_default().as_secs_f64() * 1e3
}

fn start_dir(arg: Option<OsString>) -> PathBuf {
    arg.map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `fex --bench DIR`: mide listado + stat sin GUI.
fn bench(dir: PathBuf) {
    for show_hidden in [false, true] {
        let t = Instant::now();
        let entries = fs::list_dir(&dir, show_hidden).expect("no se pudo leer el directorio");
        let list = t.elapsed();
        let t = Instant::now();
        let _ = fs::stat_all(&dir, entries.iter().map(|e| e.name.clone()).collect());
        let stat = t.elapsed();
        println!(
            "hidden={show_hidden:<5} entradas={:<7} listado={:>7.2} ms  stat={:>7.2} ms",
            entries.len(),
            list.as_secs_f64() * 1e3,
            stat.as_secs_f64() * 1e3
        );
    }
}

fn main() {
    T0.set(Instant::now()).ok();

    let mut exit_after_first_frame = std::env::var_os("FEX_EXIT_AFTER_FIRST_FRAME").is_some();
    let mut forced_mode = None;
    let mut forced_lang = None;
    let mut dir_arg = None;
    let mut bench_mode = false;
    for a in std::env::args_os().skip(1) {
        match a.to_str().unwrap_or("") {
            "--bench" => bench_mode = true,
            "--list-commands" => {
                // Plantilla del fichero `keymap`: todos los comandos con sus atajos por defecto.
                if let Ok(path) = config::file(app::bindings::FILE_NAME) {
                    println!("# {}", path.display());
                }
                print!("{}", app::bindings::template(Lang::detect()));
                return;
            }
            "--exit-after-first-frame" => exit_after_first_frame = true,
            "--light" => forced_mode = Some(Mode::Light),
            "--dark" => forced_mode = Some(Mode::Dark),
            value if value.starts_with("--lang=") => forced_lang = Lang::parse(&value[7..]),
            _ => dir_arg = Some(a),
        }
    }
    let dir = start_dir(dir_arg);
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);

    if bench_mode {
        bench(dir);
        return;
    }

    application().with_assets(Assets).run(move |cx: &mut App| {
        eprintln!("[fex] app lista: {:.1} ms", since_start_ms());
        let bounds = Bounds::centered(None, size(px(1000.), px(640.)), cx);
        let options = WindowOptions {
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };
        #[cfg(target_os = "macos")]
        let options = {
            let mut options = options;
            if let Some(titlebar) = &mut options.titlebar {
                // Let the frame reach the top edge while keeping the native controls.
                titlebar.appears_transparent = true;
            }
            options
        };
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("fex".into()),
                ..options
            },
            move |window, cx| {
                eprintln!("[fex] ventana creada: {:.1} ms", since_start_ms());
                let view = cx.new(|cx| {
                    let mut e = Explorer::new(dir, window, cx);
                    if let Some(m) = forced_mode {
                        e.mode = m;
                    }
                    if let Some(l) = forced_lang {
                        e.lang = l;
                    }
                    e
                });
                let closing_view = view.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    closing_view.upgrade().is_none_or(|view| !view.read(cx).is_busy())
                });
                let focus = view.read(cx).focus.clone();
                window.focus(&focus, cx);
                window.on_next_frame(move |_window, cx| {
                    eprintln!("[fex] primer frame: {:.1} ms", since_start_ms());
                    if exit_after_first_frame {
                        cx.quit();
                    }
                });
                view
            },
        )
        .expect("no se pudo abrir la ventana");
        cx.activate(true);
    });
}
