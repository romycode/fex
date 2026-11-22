use super::*;
use gpui::{HeadlessAppContext, Keystroke, PlatformInput};
use std::os::unix::ffi::OsStringExt;

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
        explorer.cursor = explorer.entries.iter().position(|entry| entry.name == raw).unwrap();
        assert_eq!(explorer.target_paths(), [first.join(&raw)]);
        explorer.change_sort(SortColumn::Size, cx);
        assert_eq!(explorer.target_paths(), [first.join(&raw)]);
        assert!(explorer.entries[0].kind.is_dir());
    }).unwrap();
    app.update_window(handle.into(), |_, window, cx| key(window, cx, "ctrl-l")).unwrap();
    handle.update(&mut app, |explorer, _, _| {
        let Some(LocationInput::Local(input)) = explorer.location.as_mut() else {
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
            explorer.operation_errors = Some(vec!["Example failure".into()]);
            cx.notify();
        }).unwrap();
        app.run_until_parked();
        app.update_window(handle.into(), |_, window, cx| { let _ = window.draw(cx); }).unwrap();
        let screenshot = app.capture_screenshot(handle.into()).unwrap();
        assert!(screenshot.width() >= 1000);
        assert!(screenshot.height() >= 640);
        app.update_window(handle.into(), |_, window, cx| key(window, cx, "escape")).unwrap();
        handle.update(&mut app, |explorer, _, _| assert!(explorer.operation_errors.is_none())).unwrap();
    }
    let destination = second.join("missing");
    handle.update(&mut app, |explorer, _, cx| {
        explorer.paste_paths(vec![first.join("small")], destination.clone(), Op::Copy, cx);
        assert_eq!(explorer.busy.len(), 1);
        explorer.new_folder(cx);
        assert_eq!(explorer.busy.len(), 1);
    }).unwrap();
    app.run_until_parked();
    handle.update(&mut app, |explorer, _, _| {
        assert!(explorer.operation_errors.is_some());
        assert!(explorer.retry_paste.is_some());
    }).unwrap();
    std::fs::create_dir(&destination).unwrap();
    handle.update(&mut app, |explorer, _, cx| {
        explorer.operation_errors = None;
        let retry = explorer.retry_paste.take().unwrap();
        explorer.paste_paths(retry.sources, retry.destination, retry.operation, cx);
    }).unwrap();
    app.run_until_parked();
    assert_eq!(std::fs::read_to_string(destination.join("small")).unwrap(), "x");
    handle.update(&mut app, |explorer, _, cx| {
        assert!(explorer.busy.is_empty());
        assert!(explorer.operation_errors.is_none());
        explorer.undo_last(cx);
    }).unwrap();
    app.run_until_parked();
    assert!(!destination.join("small").exists());
    assert_eq!(std::fs::read_to_string(first.join("small")).unwrap(), "x");

}
