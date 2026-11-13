//! Visual tokens copied from the preserved Tauri App.svelte, not a GPUI theme.
use gpui::*;
use gpui_component::ActiveTheme;

pub(super) const SANS: &str = "Inter";
pub(super) const MONO: &str = "JetBrains Mono";
pub(super) const ROW_HEIGHT: f32 = 27.; // CSS height:26px + 1px bottom border.
pub(super) const COLUMN_MIN: [f32; 4] = [180., 240., 62., 150.];

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub bg: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub input: Hsla,
    pub border: Hsla,
    pub input_border: Hsla,
    pub accent: Hsla,
    pub row_border: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub button: Hsla,
    pub button_text: Hsla,
    pub menu: Hsla,
    pub menu_text: Hsla,
    pub menu_hover: Hsla,
    pub toast: Hsla,
    pub toast_text: Hsla,
}
impl Palette {
    pub fn get(cx: &App) -> Self {
        if cx.theme().is_dark() {
            Self {
                bg: rgb(0x1e1e21).into(),
                text: rgb(0xb8bdc5).into(),
                muted: rgb(0x94a3b8).into(),
                input: rgb(0x161618).into(),
                border: rgb(0x2a2a2d).into(),
                input_border: rgba(0xffffff0f).into(),
                accent: rgb(0x4da8ff).into(),
                row_border: rgba(0xffffff05).into(),
                hover: rgb(0x282828).into(),
                selected: rgba(0x4da8ff1a).into(),
                button: rgba(0xffffff0d).into(),
                button_text: rgb(0x94a3b8).into(),
                menu: rgb(0x252528).into(),
                menu_text: rgb(0xe2e8f0).into(),
                menu_hover: rgba(0xffffff0a).into(),
                toast: rgba(0x0a0a10ed).into(),
                toast_text: rgb(0xf2f2f6).into(),
            }
        } else {
            Self {
                bg: rgb(0xf5f5f7).into(),
                text: rgb(0x1c1c1e).into(),
                muted: rgb(0x6e6e7a).into(),
                input: rgb(0xffffff).into(),
                border: rgb(0xd1d1d6).into(),
                input_border: rgba(0x0000001a).into(),
                accent: rgb(0x3a8bf5).into(),
                row_border: rgba(0x0000000a).into(),
                hover: rgba(0x0000000a).into(),
                selected: rgba(0x3a8bf51a).into(),
                button: rgb(0xffffff).into(),
                button_text: rgb(0x3c3c43).into(),
                menu: rgb(0xffffff).into(),
                menu_text: rgb(0x1c1c1e).into(),
                menu_hover: rgba(0x0000000d).into(),
                toast: rgba(0xfffffff2).into(),
                toast_text: rgb(0x1c1c1e).into(),
            }
        }
    }
}

pub(super) fn default_columns(width: f32) -> [f32; 4] {
    let rest = width - 150.;
    [
        (rest * 0.32 / 0.85).round().max(180.),
        (rest * 0.48 / 0.85).round().max(240.),
        (rest * 0.05 / 0.85).round().max(62.),
        150.,
    ]
}

pub(super) fn fit_columns(columns: [f32; 4], width: f32) -> [f32; 4] {
    let flexible = (width - columns[3]).max(482.);
    let scale = flexible / (columns[0] + columns[1] + columns[2]).max(1.);
    let mut name = (columns[0] * scale).round().max(180.);
    let mut size = (columns[2] * scale).round().max(62.);
    let mut path = flexible - name - size;
    if path < 240. {
        let reduction = (240. - path).min(name - 180.);
        name -= reduction;
        size -= (240. - path - reduction).max(0.).min(size - 62.);
        path = flexible - name - size;
    }
    [name, path.max(240.), size, columns[3]]
}
