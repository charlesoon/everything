//! Visual tokens (shadcn/ui zinc palette), not a GPUI theme.
use gpui::*;
use gpui_component::ActiveTheme;

pub(super) const SANS: &str = "Geist";
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
    pub ring: Hsla,
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
        // shadcn/ui "zinc" tokens: neutral surfaces, 1px zinc borders, and
        // foreground (not blue) as the accent, like shadcn's `primary`.
        if cx.theme().is_dark() {
            Self {
                bg: rgb(0x09090b).into(),     // background
                text: rgb(0xd4d4d8).into(),   // zinc-300, not #fafafa: less glare
                muted: rgb(0xa1a1aa).into(),  // muted-foreground
                input: rgb(0x1a1a1c).into(), // input/50 over bg, opaque: GPUI ring shadows fill under the box
                border: rgb(0x27272a).into(), // border
                input_border: rgb(0x27272a).into(),
                ring: rgb(0x71717a).into(),
                accent: rgb(0xe4e4e7).into(), // primary
                row_border: rgba(0xffffff08).into(),
                hover: rgb(0x18181b).into(),    // accent/50
                selected: rgb(0x27272a).into(), // accent
                button: rgb(0x0f0f12).into(),
                button_text: rgb(0xd4d4d8).into(),
                menu: rgb(0x18181b).into(), // popover
                menu_text: rgb(0xd4d4d8).into(),
                menu_hover: rgb(0x27272a).into(),
                toast: rgb(0x18181b).into(),
                toast_text: rgb(0xd4d4d8).into(),
            }
        } else {
            Self {
                bg: rgb(0xfafafa).into(),
                text: rgb(0x27272a).into(),
                muted: rgb(0x71717a).into(),
                input: rgb(0xefeff0).into(),
                border: rgb(0xe4e4e7).into(),
                input_border: rgb(0xe4e4e7).into(),
                ring: rgb(0xa1a1aa).into(),
                accent: rgb(0x27272a).into(),
                row_border: rgb(0xf0f0f2).into(),
                hover: rgb(0xf4f4f5).into(),
                selected: rgb(0xe4e4e7).into(),
                button: rgb(0xffffff).into(),
                button_text: rgb(0x27272a).into(),
                menu: rgb(0xffffff).into(),
                menu_text: rgb(0x27272a).into(),
                menu_hover: rgb(0xf4f4f5).into(),
                toast: rgb(0xffffff).into(),
                toast_text: rgb(0x27272a).into(),
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

#[cfg(test)]
mod tests {
    #[test]
    fn fitted_columns_never_overflow_the_viewport() {
        let prefs = super::default_columns(1200.);
        for tenth in 6400..14000 {
            let width = tenth as f32 / 10.;
            let total: f32 = super::fit_columns(prefs, width).iter().sum();
            assert!(total <= width + 0.01, "{width}: {total}");
        }
    }
}
