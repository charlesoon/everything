//! The Tauri baseline uses muda/AppKit context menus on macOS. Keep the same
//! menu implementation, order and separators while dispatching into Rust.
use super::*;

pub(super) fn show(
    window: &Window,
    position: Point<Pixels>,
    items: Vec<(&'static str, UiAction, &'static str)>,
) -> anyhow::Result<async_channel::Receiver<Option<UiAction>>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .as_raw()
    else {
        anyhow::bail!("Expected an AppKit window");
    };
    let view = handle.ns_view.as_ptr() as usize;
    let (sender, receiver) = async_channel::bounded(1);
    let x = f32::from(position.x) as f64;
    let y = f32::from(position.y) as f64;
    // AppKit tracks menus in a nested run loop. Enter it after GPUI has released
    // its App/Entity borrow so native events can continue to be delivered.
    dispatch::Queue::main().exec_async(move || {
        use muda::{ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
        let menu = Menu::new();
        let mut actions = HashMap::new();
        let result = (|| -> muda::Result<()> {
            for (index, (label, action, _)) in items.into_iter().enumerate() {
                if matches!(
                    action,
                    UiAction::Reveal | UiAction::CopyFiles | UiAction::Trash
                ) {
                    menu.append(&PredefinedMenuItem::separator())?;
                }
                let id = format!("everything-native-context-{index}");
                menu.append(&MenuItem::with_id(id.clone(), label, true, None))?;
                actions.insert(id, action);
            }
            Ok(())
        })();
        let mut chosen = None;
        if result.is_ok() {
            while MenuEvent::receiver().try_recv().is_ok() {}
            unsafe {
                menu.show_context_menu_for_nsview(
                    view as *const _,
                    Some(muda::dpi::LogicalPosition::new(x, y).into()),
                );
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some(action) = actions.get(event.id.as_ref()) {
                    chosen = Some(*action);
                }
            }
        }
        let _ = sender.try_send(chosen);
    });
    Ok(receiver)
}

pub(super) fn position_buttons(window: &Window) {
    use objc2::{msg_send, runtime::AnyObject};
    use objc2_foundation::NSRect;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    unsafe {
        let view = handle.ns_view.as_ptr().cast::<AnyObject>();
        let native: *mut AnyObject = msg_send![view, window];
        let fullscreen: u64 = msg_send![native, styleMask];
        if fullscreen & (1 << 14) != 0 {
            return;
        }
        for index in 0..3u64 {
            let button: *mut AnyObject = msg_send![native, standardWindowButton:index];
            if button.is_null() {
                continue;
            }
            let mut frame: NSRect = msg_send![button, frame];
            frame.origin.x = 16. + 23. * index as f64; // Orca-style left margin and pitch
            let _: () = msg_send![button, setFrame:frame];
        }
    }
}
