use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

pub(super) struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let source = match path {
            "everything/search.svg" => {
                // Lucide "search"
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.34-4.34"/></svg>"##
            }
            // Lucide (ISC): fixed-size titlebar zoom controls.
            "everything/zoom-in.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/><path d="M8 11h6"/><path d="M11 8v6"/></svg>"##
            }
            "everything/zoom-out.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/><path d="M8 11h6"/></svg>"##
            }
            "everything/plus.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/><path d="M12 5v14"/></svg>"##
            }
            "everything/minus.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/></svg>"##
            }
            "everything/sort-asc.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10" fill="none" stroke="black" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 6.5 L5 4 L7.5 6.5"/></svg>"##
            }
            "everything/sort-desc.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10" fill="none" stroke="black" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 3.5 L5 6 L7.5 3.5"/></svg>"##
            }
            "everything/minimize.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 12"><path d="M1 6h10" fill="none" stroke="black"/></svg>"##
            }
            "everything/maximize.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 12"><rect x="2" y="2" width="8" height="8" fill="none" stroke="black"/></svg>"##
            }
            "everything/restore.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 12" fill="none" stroke="black"><rect x="2.5" y="3.5" width="6" height="6"/><path d="M4.5 3.5V1.5h6v6h-2"/></svg>"##
            }
            "everything/close.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 12"><path d="M2 2l8 8M10 2l-8 8" fill="none" stroke="black"/></svg>"##
            }
            "everything/panel-right.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round"><rect x="3.5" y="3.5" width="13" height="13" rx="1.5"/><path d="M12 3.5v13"/></svg>"##
            }
            "everything/chevron-right.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 12" fill="none" stroke="black" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M4.5 3L7.5 6l-3 3"/></svg>"##
            }
            "everything/chevron-down.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 12" fill="none" stroke="black" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M3 4.5l3 3 3-3"/></svg>"##
            }
            "everything/arrow-up.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M8 13V3M4 7l4-4 4 4"/></svg>"##
            }
            "everything/reset.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M23 4v6h-6"/><path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/></svg>"##
            }
            path if path.starts_with(super::file_icons::PREFIX) => {
                match super::file_icons::svg(path) {
                    Some(svg) => svg,
                    None => return Ok(None),
                }
            }
            // Lucide icons (ISC): settings popover.
            "everything/chevron.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg>"##
            }
            "everything/settings.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"/><circle cx="12" cy="12" r="3"/></svg>"##
            }
            "everything/database.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ><ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5V19A9 3 0 0 0 21 19V5"/><path d="M3 12A9 3 0 0 0 21 12"/></svg>"##
            }
            "everything/shield-check.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ><path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/></svg>"##
            }
            "everything/folder-plus.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ><path d="M12 10v6"/><path d="M9 13h6"/><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></svg>"##
            }
            "everything/folder-minus.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ><path d="M9 13h6"/><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></svg>"##
            }
            _ => return gpui_component_assets::Assets.load(path),
        };
        Ok(Some(Cow::Borrowed(source.as_bytes())))
    }
    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        gpui_component_assets::Assets.list(path)
    }
}

pub(super) fn load_fonts(cx: &mut gpui::App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!("../../assets/fonts/geist-400.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/geist-500.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/geist-600.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/jetbrains-mono-400.ttf")),
        ])
        .expect("embedded baseline UI fonts");
}
