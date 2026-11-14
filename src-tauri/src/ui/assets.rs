use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

pub(super) struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let source = match path {
            "everything/search.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 215 205"><path d="M85 0c46.9 0 85 38.1 85 85 0 20.1-7.2 38.5-19.2 52.7l40.7 40.8c5.8 5.8 5.8 15.2 0 21-5.8 5.8-15.2 5.8-21 0l-40.7-40.7c-13.6 9.7-30.4 15.2-44.8 15.2-46.9 0-85-42.1-85-89 0-46.9 38.1-85 85-85z m0 32c-29.3 0-53 23.7-53 53 0 29.3 23.7 53 53 53 29.3 0 53-23.7 53-53 0-29.3-23.7-53-53-53z" fill-rule="evenodd"/></svg>"##
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
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="3.5" width="15" height="13" rx="2"/><path d="M12.5 3.5v13M6.5 7.5L9 10l-2.5 2.5"/></svg>"##
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
            "everything/file.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect x="3" y="1" width="10" height="14" rx="1.5" fill="#b4bec9"/><rect x="5" y="5" width="6" height="1.2" fill="#8d98a5"/><rect x="5" y="8" width="6" height="1.2" fill="#8d98a5"/></svg>"##
            }
            "everything/folder.svg" => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect x="1" y="4" width="14" height="10" rx="2" fill="#d9b34f"/><rect x="1" y="2" width="6" height="4" rx="1" fill="#e6ca7a"/></svg>"##
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
            Cow::Borrowed(include_bytes!("../../assets/fonts/inter-400.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/inter-500.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/inter-600.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/jetbrains-mono-400.ttf")),
        ])
        .expect("embedded baseline UI fonts");
}
