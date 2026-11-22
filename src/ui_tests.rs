use std::{ffi::OsString, os::unix::ffi::OsStringExt, sync::Arc};

use gpui::{App, AppContext, HeadlessAppContext, KeyDownEvent, Keystroke, PlatformInput, Window, px, size};

use crate::{
    app::{Explorer, LocationInput, Modal},
    fs::SortColumn,
    icons::Assets,
    ops::Op,
    theme::Mode,
};

fn key(window: &mut Window, cx: &mut App, value: &'static str) {
    window.dispatch_event(
        PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke::parse(value).unwrap(),
            is_held: false,
            prefer_character_input: false,
        }),
        cx,
    );
}

/// Los trabajos corren en un hilo del sistema: `run_until_parked` no basta para esperarlos.
fn wait_for_jobs(app: &mut HeadlessAppContext, handle: gpui::WindowHandle<Explorer>) {
    for _ in 0..500 {
        app.run_until_parked();
        if !handle.update(app, |explorer, _, _| explorer.is_busy()).unwrap() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("el trabajo no terminó a tiempo");
}

#[test]
fn navigation_sort_and_location_use_real_ui_entry_points() {
    let folder = tempfile::tempdir().unwrap();
    let first = folder.path().join("first");
    let second = folder.path().join("second");
    std::fs::create_dir_all(first.join("folder")).unwrap();
    std::fs::create_dir(&second).unwrap();
    std::fs::write(first.join("small"), "x").unwrap();
    std::fs::write(first.join("large"), "longer file").unwrap();
    let raw = OsString::from_vec(vec![b'r', b'a', b'w', b'-', 0xff]);
    std::fs::write(first.join(&raw), "raw name").unwrap();
    let mut app = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(Assets),
        gpui_platform::current_headless_renderer,
    );
    let start = first.clone();
    let handle = app.open_window(size(px(1000.), px(640.)), move |window, cx| {
        let view = cx.new(|cx| Explorer::new(start, window, cx));
        window.focus(&view.read(cx).focus.clone(), cx);
        view
    }).unwrap();
    app.run_until_parked();
    app.update_window(handle.into(), |_, window, cx| { let _ = window.draw(cx); }).unwrap();
    handle.update(&mut app, |explorer, _, cx| {
        explorer.selection.cursor = explorer.entries.iter().position(|entry| entry.name == raw).unwrap();
        assert_eq!(explorer.target_paths(), [first.join(&raw)]);
        explorer.change_sort(SortColumn::Size, cx);
        assert_eq!(explorer.target_paths(), [first.join(&raw)]);
        assert!(explorer.entries[0].kind.is_dir());
    }).unwrap();
    app.update_window(handle.into(), |_, window, cx| key(window, cx, "ctrl-l")).unwrap();
    handle.update(&mut app, |explorer, _, _| {
        let Modal::Location(LocationInput::Local(input)) = &mut explorer.modal else {
            panic!("Ctrl+L should open the local location input");
        };
        input.insert(second.to_str().unwrap());
    }).unwrap();
    app.update_window(handle.into(), |_, window, cx| {
        let _ = window.draw(cx);
        key(window, cx, "enter");
    }).unwrap();
    handle.update(&mut app, |explorer, _, _| {
        assert_eq!(explorer.cwd, second);
        assert!(explorer.entries.is_empty());
        assert!(explorer.target_paths().is_empty());
    }).unwrap();
    app.run_until_parked();
    handle.update(&mut app, |explorer, _, cx| {
        explorer.navigate(first.clone(), None, cx);
    }).unwrap();
    app.run_until_parked();
    for mode in [Mode::Light, Mode::Dark] {
        handle.update(&mut app, |explorer, _, cx| {
            explorer.mode = mode;
            explorer.modal = Modal::Errors(vec!["Example failure".into()]);
            cx.notify();
        }).unwrap();
        app.run_until_parked();
        app.update_window(handle.into(), |_, window, cx| { let _ = window.draw(cx); }).unwrap();
        let screenshot = app.capture_screenshot(handle.into()).unwrap();
        assert!(screenshot.width() >= 1000);
        assert!(screenshot.height() >= 640);
        app.update_window(handle.into(), |_, window, cx| key(window, cx, "escape")).unwrap();
        handle.update(&mut app, |explorer, _, _| assert!(matches!(explorer.modal, Modal::None))).unwrap();
    }
    let destination = second.join("missing");
    handle.update(&mut app, |explorer, _, cx| {
        explorer.paste_paths(vec![first.join("small")], destination.clone(), Op::Copy, cx);
        assert_eq!(explorer.jobs.busy.len(), 1);
        explorer.new_folder(cx);
        assert_eq!(explorer.jobs.busy.len(), 1);
    }).unwrap();
    wait_for_jobs(&mut app, handle);
    handle.update(&mut app, |explorer, _, _| {
        assert!(matches!(explorer.modal, Modal::Errors(_)));
        assert!(explorer.jobs.retry_paste.is_some());
    }).unwrap();
    std::fs::create_dir(&destination).unwrap();
    handle.update(&mut app, |explorer, _, cx| {
        explorer.modal = Modal::None;
        let retry = explorer.jobs.retry_paste.take().unwrap();
        explorer.paste_paths(retry.sources, retry.destination, retry.operation, cx);
    }).unwrap();
    wait_for_jobs(&mut app, handle);
    assert_eq!(std::fs::read_to_string(destination.join("small")).unwrap(), "x");
    handle.update(&mut app, |explorer, _, cx| {
        assert!(explorer.jobs.busy.is_empty());
        assert!(matches!(explorer.modal, Modal::None));
        explorer.undo_last(cx);
    }).unwrap();
    wait_for_jobs(&mut app, handle);
    assert!(!destination.join("small").exists());
    assert_eq!(std::fs::read_to_string(first.join("small")).unwrap(), "x");

}

fn open(app: &mut HeadlessAppContext, dir: std::path::PathBuf) -> gpui::WindowHandle<Explorer> {
    let handle = app
        .open_window(size(px(1000.), px(640.)), move |window, cx| {
            let view = cx.new(|cx| Explorer::new(dir, window, cx));
            window.focus(&view.read(cx).focus.clone(), cx);
            view
        })
        .unwrap();
    app.run_until_parked();
    app.update_window(handle.into(), |_, window, cx| {
        let _ = window.draw(cx);
    })
    .unwrap();
    handle
}

/// Guarda una captura si `FEX_SCREENSHOT_DIR` está definido (revisión visual manual).
fn screenshot(app: &mut HeadlessAppContext, handle: gpui::WindowHandle<Explorer>, name: &str) {
    app.run_until_parked();
    app.update_window(handle.into(), |_, window, cx| {
        let _ = window.draw(cx);
    })
    .unwrap();
    let image = app.capture_screenshot(handle.into()).unwrap();
    if let Some(dir) = std::env::var_os("FEX_SCREENSHOT_DIR") {
        image.save(std::path::Path::new(&dir).join(format!("{name}.png"))).unwrap();
    }
}

#[test]
fn commands_drive_keyboard_palette_and_menus() {
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path().to_path_buf();
    for name in ["alpha.txt", "beta.rs", "gamma.png"] {
        std::fs::write(root.join(name), name).unwrap();
    }
    std::fs::create_dir(root.join("docs")).unwrap();
    let mut app = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(Assets),
        gpui_platform::current_headless_renderer,
    );
    let handle = open(&mut app, root.clone());
    handle.update(&mut app, |e, _, cx| {
        e.mode = Mode::Dark;
        cx.notify();
    }).unwrap();
    screenshot(&mut app, handle, "list");

    // Teclado → registro: el cursor baja y Shift amplía la selección.
    app.update_window(handle.into(), |_, window, cx| {
        key(window, cx, "down");
        key(window, cx, "shift-down");
    })
    .unwrap();
    handle.update(&mut app, |e, _, _| {
        assert_eq!(e.selection.cursor, 2);
        assert_eq!(e.target_names().len(), 2);
    }).unwrap();

    // Menú contextual: solo comandos disponibles, sin separadores colgando.
    handle.update(&mut app, |e, _, cx| {
        e.menu = Some(crate::app::Menu { pos: gpui::point(px(400.), px(200.)), on_item: true });
        let items = crate::app::commands::resolve_menu(crate::app::commands::ITEM_MENU, e);
        assert!(items.first().unwrap().is_some() && items.last().unwrap().is_some());
        let ids: Vec<_> = items.iter().flatten().map(|c| c.id).collect();
        assert!(ids.contains(&"file.copy") && !ids.contains(&"file.rename") && !ids.contains(&"file.download"));
        cx.notify();
    }).unwrap();
    screenshot(&mut app, handle, "menu");
    app.update_window(handle.into(), |_, window, cx| key(window, cx, "escape")).unwrap();
    handle.update(&mut app, |e, _, _| {
        assert!(e.menu.is_none());
        assert_eq!(e.target_names().len(), 2, "Esc con menú abierto solo cierra el menú");
    }).unwrap();

    // Paleta: Ctrl+K, filtrar, Enter ejecuta.
    app.update_window(handle.into(), |_, window, cx| key(window, cx, "ctrl-k")).unwrap();
    handle.update(&mut app, |e, _, _| assert!(matches!(e.modal, Modal::Palette(_)))).unwrap();
    for ch in ["n", "e", "w", " ", "f"] {
        app.update_window(handle.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: Keystroke { key_char: Some(ch.into()), ..Keystroke::parse(if ch == " " { "space" } else { ch }).unwrap() },
                    is_held: false,
                    prefer_character_input: false,
                }),
                cx,
            )
        })
        .unwrap();
    }
    handle.update(&mut app, |e, _, cx| {
        e.lang = crate::i18n::Lang::En;
        let Modal::Palette(p) = &e.modal else { panic!("la paleta debería seguir abierta") };
        let query = p.input.text.clone();
        assert_eq!(query, "new f");
        assert_eq!(e.palette_matches(&query)[0].id, "file.new_folder");
        cx.notify();
    }).unwrap();
    screenshot(&mut app, handle, "palette");
    app.update_window(handle.into(), |_, window, cx| key(window, cx, "enter")).unwrap();
    app.run_until_parked();
    handle.update(&mut app, |e, _, _| {
        assert!(matches!(e.modal, Modal::Rename(_)), "la carpeta nueva entra en modo renombrar");
    }).unwrap();
    assert!(root.join("New Folder").is_dir());
    screenshot(&mut app, handle, "rename");
    app.update_window(handle.into(), |_, window, cx| key(window, cx, "escape")).unwrap();

    // Deshacer desde el teclado.
    app.update_window(handle.into(), |_, window, cx| key(window, cx, "ctrl-z")).unwrap();
    wait_for_jobs(&mut app, handle);
    assert!(!root.join("New Folder").exists());

    // Un trabajo bloquea los comandos que escriben y ofrece cancelar.
    handle.update(&mut app, |e, _, cx| {
        let sources = vec![root.join("alpha.txt")];
        e.jobs.progress.cancel();
        e.paste_paths(sources, root.join("docs"), Op::Copy, cx);
        assert!(e.is_busy());
        assert!(!(crate::app::commands::find("file.paste").when)(e));
        e.cancel_job(cx);
    }).unwrap();
    wait_for_jobs(&mut app, handle);
    handle.update(&mut app, |e, _, _| assert!(!e.jobs.progress.is_cancelled() || !e.is_busy())).unwrap();
}
