use std::mem;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDIBits, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON,
    SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES, SIIGBF_ICONONLY,
};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, HICON, DI_NORMAL};

use super::com_guard::{ComGuard, to_wide};

const ICON_SIZE: i32 = 32;
const ICON_SIZE_FALLBACK: i32 = 16;

/// High-quality icon via IShellItemImageFactory (requires real file path).
pub fn load_icon_png(path: &str) -> Option<Vec<u8>> {
    let _com = ComGuard::init().ok()?;

    let wide = to_wide(path);
    let item: windows::Win32::UI::Shell::IShellItem = unsafe {
        SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?
    };

    let factory: IShellItemImageFactory = item.cast().ok()?;
    let hbitmap = unsafe {
        factory
            .GetImage(SIZE { cx: ICON_SIZE, cy: ICON_SIZE }, SIIGBF_ICONONLY)
            .ok()?
    };

    let rgba = hbitmap_to_rgba(hbitmap, ICON_SIZE);
    unsafe {
        let _ = DeleteObject(hbitmap);
    }

    encode_png(ICON_SIZE as u32, ICON_SIZE as u32, &rgba?)
}

/// Fallback icon via SHGetFileInfo (extension only, no real file needed).
pub fn load_icon_png_by_ext(ext: &str) -> Option<Vec<u8>> {
    let _com = ComGuard::init().ok()?;

    let (dummy_path, file_attrs) = if ext.eq_ignore_ascii_case("folder")
        || ext.eq_ignore_ascii_case("__folder__")
    {
        ("folder".to_string(), FILE_ATTRIBUTE_DIRECTORY)
    } else if ext.is_empty() || ext == "__default__" {
        ("file".to_string(), FILE_ATTRIBUTE_NORMAL)
    } else {
        let sanitized: String = ext
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
            .collect();
        (format!("x.{sanitized}"), FILE_ATTRIBUTE_NORMAL)
    };

    let wide = to_wide(&dummy_path);
    let hicon = get_shell_icon(&wide, file_attrs)?;
    let rgba = hicon_to_rgba(hicon, ICON_SIZE_FALLBACK);
    unsafe {
        let _ = DestroyIcon(hicon);
    }

    encode_png(
        ICON_SIZE_FALLBACK as u32,
        ICON_SIZE_FALLBACK as u32,
        &rgba?,
    )
}

fn get_shell_icon(
    wide_path: &[u16],
    file_attrs: FILE_FLAGS_AND_ATTRIBUTES,
) -> Option<HICON> {
    let mut shfi = SHFILEINFOW::default();
    let flags = SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES;

    let result = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide_path.as_ptr()),
            file_attrs,
            Some(&mut shfi),
            mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };

    if result == 0 || shfi.hIcon.is_invalid() {
        return None;
    }
    Some(shfi.hIcon)
}

/// Read pixels from HBITMAP returned by IShellItemImageFactory::GetImage.
fn hbitmap_to_rgba(
    hbitmap: HBITMAP,
    size: i32,
) -> Option<Vec<u8>> {
    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let byte_count = (size * size * 4) as usize;
        let mut buffer = vec![0u8; byte_count];

        let lines = GetDIBits(
            hdc,
            hbitmap,
            0,
            size as u32,
            Some(buffer.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(hdc);

        if lines == 0 {
            return None;
        }

        Some(bgra_to_rgba(&buffer))
    }
}

/// Draw HICON into a DIB section and read RGBA pixels.
fn hicon_to_rgba(
    hicon: HICON,
    size: i32,
) -> Option<Vec<u8>> {
    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbitmap = match CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0) {
            Ok(bmp) => bmp,
            Err(_) => {
                let _ = DeleteDC(hdc);
                return None;
            }
        };

        if bits_ptr.is_null() {
            let _ = DeleteObject(hbitmap);
            let _ = DeleteDC(hdc);
            return None;
        }

        let old_bmp = SelectObject(hdc, hbitmap);
        let _ = DrawIconEx(hdc, 0, 0, hicon, size, size, 0, None, DI_NORMAL);

        let byte_count = (size * size * 4) as usize;
        let src = std::slice::from_raw_parts(bits_ptr as *const u8, byte_count);
        let rgba = bgra_to_rgba(src);

        SelectObject(hdc, old_bmp);
        let _ = DeleteObject(hbitmap);
        let _ = DeleteDC(hdc);

        Some(rgba)
    }
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut has_any_alpha = false;
    let mut rgba = Vec::with_capacity(bgra.len());

    for chunk in bgra.chunks_exact(4) {
        rgba.push(chunk[2]); // R
        rgba.push(chunk[1]); // G
        rgba.push(chunk[0]); // B
        rgba.push(chunk[3]); // A
        if chunk[3] != 0 {
            has_any_alpha = true;
        }
    }

    if !has_any_alpha {
        for pixel in rgba.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }

    rgba
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(buf)
}
