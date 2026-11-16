//! Single-line text with a middle ellipsis: `~/a/very/lo…/file.txt`.
//!
//! GPUI's `truncate()` caches its first measurement, so text shrunk later by
//! flex is clipped without an ellipsis. This element reports its full width
//! as the intrinsic size (it may shrink to 0) and decides the cut at paint
//! time, when the real bounds are known.

use gpui::*;

pub(super) struct MiddleText {
    text: SharedString,
    /// Byte index where the protected tail (the last path component) starts.
    split: usize,
}

/// Path text whose last component (after the final `/` or `\`) stays visible.
pub(super) fn middle_path(text: impl Into<SharedString>) -> MiddleText {
    let text = text.into();
    let split = text.rfind(['/', '\\']).map_or(0, |i| i + 1);
    MiddleText { text, split }
}

impl IntoElement for MiddleText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

pub(super) struct Shaped {
    style: TextStyle,
    font_size: Pixels,
    line_height: Pixels,
}

impl Element for MiddleText {
    type RequestLayoutState = Shaped;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Shaped) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = style.line_height_in_pixels(window.rem_size());
        let width = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &[style.to_run(self.text.len())], None)
            .width;
        let mut layout = Style::default();
        layout.size.width = width.ceil().into();
        layout.size.height = line_height.into();
        layout.min_size.width = px(0.).into();
        layout.flex_shrink = 1.;
        let id = window.request_layout(layout, [], cx);
        (id, Shaped { style, font_size, line_height })
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Shaped,
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        s: &mut Shaped,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let shape = |text: SharedString, window: &mut Window| {
            window
                .text_system()
                .shape_line(text.clone(), s.font_size, &[s.style.to_run(text.len())], None)
        };
        let full = shape(self.text.clone(), window);
        let text: SharedString = if full.width <= bounds.size.width + px(0.5) {
            self.text.clone()
        } else {
            let ellipsis = shape("…".into(), window).width;
            fit(
                &self.text,
                self.split,
                full.width,
                full.x_for_index(self.split),
                ellipsis,
                bounds.size.width,
                |x| full.index_for_x(x),
            )
            .into()
        };
        let line = shape(text, window);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            let _ = line.paint(bounds.origin, s.line_height, window, cx);
        });
    }
}

/// Picks the visible string for `max_w`. `index_for_x` maps an x offset in the
/// full line to the byte index of the glyph there (`None` past the end).
fn fit(
    text: &str,
    split: usize,
    total_w: Pixels,
    name_x: Pixels,
    ellipsis: Pixels,
    max_w: Pixels,
    index_for_x: impl Fn(Pixels) -> Option<usize>,
) -> String {
    let name_w = total_w - name_x;
    let room = max_w - ellipsis - name_w;
    if room >= px(0.) {
        // Prefix … name
        let cut = index_for_x(room).unwrap_or(split).min(split);
        format!("{}…{}", &text[..cut], &text[split..])
    } else {
        // The name alone overflows: show its start, end-truncated.
        let cut = index_for_x(name_x + (max_w - ellipsis).max(px(0.)))
            .unwrap_or(text.len())
            .max(split);
        format!("{}…", &text[split..cut])
    }
}

#[cfg(test)]
mod tests {
    use super::fit;
    use gpui::{px, Pixels};

    #[test]
    fn keeps_the_name_and_cuts_the_middle() {
        // Monospace model: every char is 10px wide, "…" too.
        let text = "~/a/bcd/file.txt";
        let split = text.rfind('/').unwrap() + 1;
        let at = |x: Pixels| {
            let i = (f32::from(x) / 10.) as usize;
            (i < text.len()).then_some(i)
        };
        let w = |n: usize| px(n as f32 * 10.);
        let fitted = |max| fit(text, split, w(text.len()), w(split), w(1), w(max), at);
        assert_eq!(fitted(13), "~/a/…file.txt");
        assert_eq!(fitted(10), "~…file.txt");
        assert_eq!(fitted(9), "…file.txt");
        assert_eq!(fitted(5), "file…");
    }
}
