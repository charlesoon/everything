//! Opt-in native macOS UI driver. Only included with `ui-automation` and only
//! listens when EVERYTHING_UI_AUTOMATION_SOCKET is explicitly set. Events go
//! through NSApplication/NSEvent and the real rendered window. Text uses the
//! NSTextInputClient committed-text entry point, independent of the current
//! keyboard/IME layout. It never changes the application's UI model.
//!
//! Local socket permissions are 0600; there is no network listener. This also
//! lets CI test its own window without global Accessibility control.

#![allow(deprecated, unexpected_cfgs)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    sync::mpsc,
    time::Duration,
};

use cocoa::{
    appkit::{NSApp, NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSView, NSWindow},
    base::{id, nil},
    foundation::{NSNotFound, NSPoint, NSRange, NSRect, NSString},
};
use objc::{class, msg_send, sel, sel_impl};
use serde_json::{json, Value};

pub(crate) fn start() {
    let Some(path) = std::env::var_os("EVERYTHING_UI_AUTOMATION_SOCKET").map(PathBuf::from) else {
        return;
    };
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("UI driver socket: {e}");
            return;
        }
    };
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            let Ok(mut stream) = connection else {
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut line = String::new();
            if BufReader::new(&mut stream).read_line(&mut line).is_err() {
                continue;
            }
            let request = match serde_json::from_str::<Value>(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Native menu tracking runs a nested loop inside a main-queue
            // block. AppKit's thread-safe event queue remains available there.
            if request["op"].as_str() == Some("menu_key") {
                let key = request["key"].as_str().unwrap_or("");
                let result = unsafe { post_menu_key(key) };
                let response = result
                    .map(|_| json!({"ok":true}))
                    .unwrap_or_else(|error| json!({"error":error}));
                let _ = writeln!(stream, "{response}");
                continue;
            }
            let (send, receive) = mpsc::sync_channel(1);
            dispatch::Queue::main().exec_async(move || {
                let result = unsafe { execute(request) };
                let _ = send.send(result.unwrap_or_else(|e| json!({"error":e})));
            });
            let response = receive
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|e| json!({"error":e.to_string()}));
            let _ = writeln!(stream, "{response}");
        }
        let _ = fs::remove_file(path);
    });
}

unsafe fn execute(request: Value) -> Result<Value, String> {
    let app = NSApp();
    let mut window: id = msg_send![app, keyWindow];
    if window == nil {
        window = msg_send![app, mainWindow];
    }
    if window == nil {
        let windows: id = msg_send![app, windows];
        window = msg_send![windows, firstObject];
    }
    if window == nil {
        return Err("No native window".into());
    }
    let view = window.contentView();
    let bounds = view.bounds();
    let op = request["op"].as_str().unwrap_or("");
    if op != "capture" && op != "window" && op != "clipboard" && op != "hide" {
        app.activateIgnoringOtherApps_(true);
        window.makeKeyAndOrderFront_(nil);
    }
    match op {
        "window" => {
            let frame = NSWindow::frame(window);
            let number = window.windowNumber();
            Ok(
                json!({"windowId":number,"x":frame.origin.x,"y":frame.origin.y,"width":bounds.size.width,"height":bounds.size.height}),
            )
        }
        "appearance" => {
            let name = match request["theme"].as_str() {
                Some("dark") => "NSAppearanceNameDarkAqua",
                Some("light") => "NSAppearanceNameAqua",
                _ => return Err("Expected light or dark".into()),
            };
            let name = NSString::alloc(nil).init_str(name);
            let appearance: id = msg_send![class!(NSAppearance),appearanceNamed:name];
            let _: () = msg_send![app,setAppearance:appearance];
            let _: () = msg_send![window,setAppearance:appearance];
            let _: () = msg_send![name, release];
            Ok(json!({"ok":true}))
        }
        "activate" => Ok(json!({"ok":true})),
        "hide" => {
            let _: () = msg_send![app,hide:nil];
            Ok(json!({"ok":true}))
        }
        "resize" => {
            let width = request["width"].as_f64().ok_or("width missing")?;
            let height = request["height"].as_f64().ok_or("height missing")?;
            if !(400.0..=3000.0).contains(&width) || !(300.0..=2000.0).contains(&height) {
                return Err("Invalid dimensions".into());
            }
            window.setContentSize_(cocoa::foundation::NSSize::new(width, height));
            Ok(json!({"ok":true}))
        }
        "key" => {
            let combination = request["key"].as_str().ok_or("key missing")?;
            let parts = combination.split('-').collect::<Vec<_>>();
            let key = *parts.last().unwrap();
            let (code, characters) = match key {
                "enter" => (36, "\r"),
                "escape" => (53, "\u{1b}"),
                "backspace" => (51, "\u{7f}"),
                "delete" => (117, "\u{f728}"),
                "down" => (125, "\u{f701}"),
                "up" => (126, "\u{f700}"),
                "left" => (123, "\u{f702}"),
                "right" => (124, "\u{f703}"),
                "pagedown" => (121, "\u{f72d}"),
                "pageup" => (116, "\u{f72c}"),
                "f2" => (120, "\u{f705}"),
                "space" => (49, " "),
                "tab" => (48, "\t"),
                "a" => (0, "a"),
                "c" => (8, "c"),
                "f" => (3, "f"),
                "o" => (31, "o"),
                "v" => (9, "v"),
                "q" => (12, "q"),
                "z" => (6, "z"),
                _ => return Err(format!("Unsupported key: {key}")),
            };
            send_key(app, window, code, characters, flags(&parts));
            Ok(json!({"ok":true}))
        }
        "text" => {
            let text = request["text"].as_str().ok_or("text missing")?;
            if text.len() > 10000 {
                return Err("Text is too long".into());
            }
            let responder: id = msg_send![window, firstResponder];
            let supports: bool =
                msg_send![responder, respondsToSelector:sel!(insertText:replacementRange:)];
            if !supports {
                return Err("The first responder does not accept committed text".into());
            }
            let _: () = msg_send![responder, unmarkText];
            for ch in text.chars() {
                let string = NSString::alloc(nil).init_str(&ch.to_string());
                let _: () = msg_send![responder, insertText:string replacementRange:NSRange::new(NSNotFound as u64, 0)];
                let _: () = msg_send![string, release];
            }
            Ok(json!({"ok":true}))
        }
        "click" | "move" | "down" | "up" => {
            let x = request["x"].as_f64().ok_or("x missing")?;
            let y = request["y"].as_f64().ok_or("y missing")?;
            let location = NSPoint::new(x, bounds.size.height - y);
            let location: NSPoint = msg_send![view,convertPoint:location toView:nil];
            let right = request["button"].as_str() == Some("right");
            let count = request["count"].as_i64().unwrap_or(1);
            let modifiers = request["modifiers"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            let types = if op == "click" {
                if right {
                    vec![NSEventType::NSRightMouseDown, NSEventType::NSRightMouseUp]
                } else {
                    vec![NSEventType::NSLeftMouseDown, NSEventType::NSLeftMouseUp]
                }
            } else if op == "move" {
                vec![if request["drag"].as_bool() == Some(true) {
                    NSEventType::NSLeftMouseDragged
                } else {
                    NSEventType::NSMouseMoved
                }]
            } else if op == "down" {
                vec![NSEventType::NSLeftMouseDown]
            } else {
                vec![NSEventType::NSLeftMouseUp]
            };
            for kind in types {
                let event=NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure_(nil,kind,location,flags(&modifiers),0.,window.windowNumber(),nil,0,count,1.);
                let event = if right {
                    let cg: *mut std::ffi::c_void = msg_send![event, CGEvent];
                    CGEventSetIntegerValueField(cg, 3, 1); // kCGMouseEventButtonNumber
                    let native: id = msg_send![class!(NSEvent),eventWithCGEvent:cg];
                    native
                } else {
                    event
                };
                app.sendEvent_(event);
            }
            Ok(json!({"ok":true}))
        }
        "clipboard" => {
            let board: id = msg_send![class!(NSPasteboard), generalPasteboard];
            let kind = NSString::alloc(nil).init_str("public.utf8-plain-text");
            let text: id = msg_send![board,stringForType:kind];
            let string = if text == nil {
                None
            } else {
                Some(
                    std::ffi::CStr::from_ptr(text.UTF8String())
                        .to_string_lossy()
                        .into_owned(),
                )
            };
            let types: id = msg_send![board, types];
            let count: usize = msg_send![types, count];
            let mut all = Vec::new();
            for i in 0..count {
                let kind: id = msg_send![types,objectAtIndex:i];
                all.push(
                    std::ffi::CStr::from_ptr(kind.UTF8String())
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            Ok(json!({"text":string,"types":all}))
        }
        "capture" => {
            let path = PathBuf::from(request["path"].as_str().ok_or("path missing")?);
            capture(window, &path)?;
            Ok(json!({"ok":true,"path":path}))
        }
        _ => Err(format!("Unsupported operation: {op}")),
    }
}

unsafe fn post_menu_key(key: &str) -> Result<(), String> {
    let (code, chars) = match key {
        "down" => (125, "\u{f701}"),
        "up" => (126, "\u{f700}"),
        "enter" => (36, "\r"),
        "escape" => (53, "\u{1b}"),
        _ => return Err("Unsupported menu key".into()),
    };
    let pool: id = msg_send![class!(NSAutoreleasePool), new];
    let app = NSApp();
    let string = NSString::alloc(nil).init_str(chars);
    for kind in [NSEventType::NSKeyDown, NSEventType::NSKeyUp] {
        let event=NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode_(nil,kind,NSPoint::new(0.,0.),NSEventModifierFlags::empty(),0.,0,nil,string,string,false,code);
        app.postEvent_atStart_(event, false);
    }
    let _: () = msg_send![string, release];
    let _: () = msg_send![pool, drain];
    Ok(())
}

fn flags(parts: &[&str]) -> NSEventModifierFlags {
    let mut flags = NSEventModifierFlags::empty();
    for part in parts {
        match *part {
            "cmd" => flags |= NSEventModifierFlags::NSCommandKeyMask,
            "ctrl" => flags |= NSEventModifierFlags::NSControlKeyMask,
            "shift" => flags |= NSEventModifierFlags::NSShiftKeyMask,
            "alt" => flags |= NSEventModifierFlags::NSAlternateKeyMask,
            _ => {}
        }
    }
    flags
}

unsafe fn send_key(
    app: id,
    window: id,
    code: u16,
    characters: &str,
    modifiers: NSEventModifierFlags,
) {
    let string = NSString::alloc(nil).init_str(characters);
    for kind in [NSEventType::NSKeyDown, NSEventType::NSKeyUp] {
        let event=NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode_(nil,kind,NSPoint::new(0.,0.),modifiers,0.,window.windowNumber(),nil,string,string,false,code);
        app.sendEvent_(event);
    }
    let _: () = msg_send![string, release];
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCreateImage(
        bounds: NSRect,
        options: u32,
        window: u32,
        image_options: u32,
    ) -> *const std::ffi::c_void;
    fn CGImageRelease(image: *const std::ffi::c_void);
    fn CGEventSetIntegerValueField(event: *mut std::ffi::c_void, field: u32, value: i64);
}

unsafe fn capture(window: id, path: &std::path::Path) -> Result<(), String> {
    let bounds = NSRect::new(
        NSPoint::new(f64::INFINITY, f64::INFINITY),
        cocoa::foundation::NSSize::new(0., 0.),
    );
    let image = CGWindowListCreateImage(bounds, 1 << 3, window.windowNumber() as u32, 1);
    if image.is_null() {
        return Err("Own-window screenshot unavailable".into());
    }
    let rep: id = msg_send![class!(NSBitmapImageRep), alloc];
    let rep: id = msg_send![rep,initWithCGImage:image];
    CGImageRelease(image);
    let properties: id = msg_send![class!(NSDictionary), dictionary];
    let data: id = msg_send![rep,representationUsingType:4usize properties:properties];
    if data == nil {
        let _: () = msg_send![rep, release];
        return Err("PNG encoding failed".into());
    }
    let bytes: *const u8 = msg_send![data, bytes];
    let length: usize = msg_send![data, length];
    let result =
        fs::write(path, std::slice::from_raw_parts(bytes, length)).map_err(|e| e.to_string());
    let _: () = msg_send![rep, release];
    result
}
