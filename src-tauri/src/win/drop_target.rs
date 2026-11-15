//! Opts the window out of GPUI's OLE drop target.
//!
//! GPUI 0.2.2's `IDropTarget::DragEnter` dereferences the CF_HDROP `HGLOBAL`
//! as if it pointed to an `HDROP`. That only happens to work for movable
//! handles; a fixed one — what the shell data object of our own file drag
//! carries — crashes in `DragQueryFileW`. The app never accepts dropped files,
//! so the window drops out of OLE drag-and-drop as a target entirely.

use windows::Win32::{Foundation::HWND, System::Ole::RevokeDragDrop};

pub fn revoke(hwnd: isize) {
    if let Err(error) = unsafe { RevokeDragDrop(HWND(hwnd as *mut _)) } {
        eprintln!("RevokeDragDrop: {error}");
    }
}
