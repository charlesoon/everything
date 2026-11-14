//! Browser-style UI zoom. Preferences and column widths use 100% design units;
//! native window dimensions and input coordinates stay in display pixels.
use super::*;
use gpui_component::tooltip::Tooltip;

const LEVELS: &[f32] = &[0.5, 0.67, 0.75, 0.8, 0.9, 1., 1.1, 1.25, 1.5, 1.75, 2.];
const POPOVER_WIDTH: f32 = 260.;
const POPOVER_HEIGHT: f32 = 52.;
const AUTO_HIDE_DELAY: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct Popover {
    pub(super) visible: bool,
    pub(super) pinned: bool,
    pub(super) hovered: bool,
    pub(super) keyboard_selection: Option<usize>,
    hide_task: Option<Task<()>>,
}

#[derive(Clone, Copy)]
pub(super) enum Zoom {
    In,
    Out,
    Reset,
}

impl Zoom {
    pub(super) fn shortcut(key: &str, modifiers: Modifiers) -> Option<Self> {
        if !(modifiers.platform || modifiers.control) || modifiers.alt {
            return None;
        }
        match key {
            "+" | "=" | "add" => Some(Self::In),
            "-" | "subtract" => Some(Self::Out),
            "0" => Some(Self::Reset),
            _ => None,
        }
    }

    fn next(self, current: f32) -> f32 {
        let current = crate::ui_prefs::normalize_zoom(current);
        match self {
            Self::In => LEVELS
                .iter()
                .copied()
                .find(|level| *level > current + 0.001)
                .unwrap_or(2.),
            Self::Out => LEVELS
                .iter()
                .rev()
                .copied()
                .find(|level| *level < current - 0.001)
                .unwrap_or(0.5),
            Self::Reset => 1.,
        }
    }
}

impl EverythingView {
    pub(super) fn zoom_button_visible(&self) -> bool {
        self.prefs.zoom != 1. || self.zoom_popover.visible
    }

    pub(super) fn row_height(&self) -> f32 {
        design::ROW_HEIGHT * self.prefs.zoom
    }

    pub(super) fn change_zoom(
        &mut self,
        action: Zoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.prefs.zoom;
        let next = action.next(previous);
        if next != previous {
            self.prefs.zoom = next;
            self.apply_theme(window, cx);
            window.set_rem_size(px(design::BASE_REM_SIZE * next));
            self.table_width = 0.;
            self.resizing = None;
            self.drag_start = None;
            let ratio = next / previous;
            let scroll = self.list_scroll.0.borrow();
            let offset = scroll.base_handle.offset();
            scroll
                .base_handle
                .set_offset(point(offset.x * ratio, offset.y * ratio));
            drop(scroll);
            let offset = self.horizontal_scroll.offset();
            self.horizontal_scroll
                .set_offset(point(offset.x * ratio, offset.y));
            self.file_tree.rescale_scroll(ratio);
            self.save_preferences(cx);
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
        self.zoom_popover.visible = true;
        self.schedule_zoom_popover_hide(cx);
        cx.notify();
    }

    pub(super) fn close_zoom_popover(&mut self, cx: &mut Context<Self>) {
        if self.zoom_popover.visible {
            self.zoom_popover = Popover::default();
            cx.notify();
        }
    }

    fn schedule_zoom_popover_hide(&mut self, cx: &mut Context<Self>) {
        self.zoom_popover.hide_task = None;
        if !self.zoom_popover.visible || self.zoom_popover.pinned || self.zoom_popover.hovered {
            return;
        }
        self.zoom_popover.hide_task = Some(cx.spawn(async move |view, cx| {
            Timer::after(AUTO_HIDE_DELAY).await;
            let _ = view.update(cx, |this, cx| {
                this.zoom_popover.visible = false;
                this.zoom_popover.keyboard_selection = None;
                cx.notify();
            });
        }));
    }

    pub(super) fn zoom_popover_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.zoom_popover.visible {
            return false;
        }
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            self.close_zoom_popover(cx);
            return true;
        }
        let modifiers = event.keystroke.modifiers;
        if self.zoom_popover.pinned && !(modifiers.platform || modifiers.control || modifiers.alt) {
            match key {
                "tab" | "left" | "right" => {
                    let backwards = key == "left" || (key == "tab" && modifiers.shift);
                    self.zoom_popover.keyboard_selection =
                        Some(match self.zoom_popover.keyboard_selection {
                            Some(index) => (index + if backwards { 2 } else { 1 }) % 3,
                            None => {
                                if backwards {
                                    2
                                } else {
                                    0
                                }
                            }
                        });
                    cx.notify();
                    return true;
                }
                "enter" | "space" => {
                    if let Some(index) = self.zoom_popover.keyboard_selection {
                        self.change_zoom([Zoom::Out, Zoom::In, Zoom::Reset][index], window, cx);
                        return true;
                    }
                }
                _ => {}
            }
        }
        // Normal typing/navigation continues in the existing input or table.
        self.close_zoom_popover(cx);
        false
    }

    pub(super) fn zoom_button_bounds(&self, window: &Window) -> Bounds<Pixels> {
        let width = 26.;
        let right = if cfg!(target_os = "windows") {
            179.
        } else {
            48.
        };
        Bounds::new(
            point(window.viewport_size().width - px(right + width), px(8.)),
            size(px(width), px(22.)),
        )
    }

    pub(super) fn zoom_popover_bounds(&self, window: &Window) -> Bounds<Pixels> {
        let width = f32::from(window.viewport_size().width);
        let right = (if cfg!(target_os = "windows") {
            179_f32
        } else {
            48_f32
        })
        .min(width - POPOVER_WIDTH - 8.);
        Bounds::new(
            point(
                px(width - right - POPOVER_WIDTH),
                px(design::TITLEBAR_HEIGHT + 4.),
            ),
            size(px(POPOVER_WIDTH), px(POPOVER_HEIGHT)),
        )
    }

    pub(super) fn zoom_button(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let p = design::Palette::get(cx);
        let bounds = self.zoom_button_bounds(window);
        let percent = (self.prefs.zoom * 100.).round() as u16;
        div()
            .id("toggle-zoom")
            .absolute()
            .left(bounds.origin.x)
            .top(bounds.origin.y)
            .w(bounds.size.width)
            .h(bounds.size.height)
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .text_color(p.text)
            .when(self.zoom_popover.visible, |button| button.bg(p.selected))
            .hover(|button| button.bg(p.hover))
            .cursor_pointer()
            .tooltip(move |window, cx| Tooltip::new(format!("Zoom ({percent}%)")).build(window, cx))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                cx.stop_propagation();
                window.prevent_default();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                if this.zoom_popover.visible {
                    this.close_zoom_popover(cx);
                } else {
                    this.zoom_popover.visible = true;
                    this.zoom_popover.pinned = true;
                    cx.notify();
                }
            }))
            .child(
                svg()
                    .path(if self.prefs.zoom < 1. {
                        "everything/zoom-out.svg"
                    } else {
                        "everything/zoom-in.svg"
                    })
                    .size(px(16.))
                    .flex_shrink_0()
                    .text_color(p.text),
            )
    }

    pub(super) fn zoom_popover(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let p = design::Palette::get(cx);
        let bounds = self.zoom_popover_bounds(window);
        let control = |index: usize, action: Zoom, enabled: bool, cx: &mut Context<Self>| {
            let selected = self.zoom_popover.keyboard_selection == Some(index);
            div()
                .id(("zoom-control", index))
                .w(px(if index == 2 { 64. } else { 28. }))
                .h(px(28.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(if index == 2 { 14. } else { 5. }))
                .border_1()
                .border_color(if selected {
                    p.ring
                } else if index == 2 {
                    p.border
                } else {
                    transparent_black()
                })
                .when(selected, |button| button.bg(p.menu_hover))
                .when(!enabled, |button| button.opacity(0.35))
                .when(enabled, |button| {
                    button
                        .cursor_pointer()
                        .hover(|button| button.bg(p.menu_hover))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.change_zoom(action, window, cx);
                        }))
                })
        };
        div()
            .id("zoom-popover")
            .absolute()
            .left(bounds.origin.x)
            .top(bounds.origin.y)
            .w(bounds.size.width)
            .h(bounds.size.height)
            .px(px(12.))
            .py(px(10.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(10.))
            .border_1()
            .border_color(p.border)
            .bg(p.menu)
            .text_color(p.menu_text)
            .font_family(design::SANS)
            .text_size(px(12.))
            .line_height(px(16.))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                cx.stop_propagation();
                window.prevent_default();
            })
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                // Let the titlebar button handle toggling instead of reopening
                // a popover which the outside-click handler just closed.
                if !this.zoom_button_bounds(window).contains(&event.position) {
                    this.close_zoom_popover(cx);
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.zoom_popover.hovered = *hovered;
                this.schedule_zoom_popover_hide(cx);
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .font_family(design::MONO)
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(format!("{}%", (self.prefs.zoom * 100.).round() as u16)),
            )
            .child(
                control(
                    0,
                    Zoom::Out,
                    self.prefs.zoom > crate::ui_prefs::MIN_ZOOM,
                    cx,
                )
                .tooltip(|window, cx| Tooltip::new("Zoom out").build(window, cx))
                .child(
                    svg()
                        .path("everything/minus.svg")
                        .size(px(16.))
                        .text_color(p.text),
                ),
            )
            .child(
                control(1, Zoom::In, self.prefs.zoom < crate::ui_prefs::MAX_ZOOM, cx)
                    .tooltip(|window, cx| Tooltip::new("Zoom in").build(window, cx))
                    .child(
                        svg()
                            .path("everything/plus.svg")
                            .size(px(16.))
                            .text_color(p.text),
                    ),
            )
            .child(control(2, Zoom::Reset, self.prefs.zoom != 1., cx).child("Reset"))
    }
}

#[cfg(test)]
mod tests {
    use super::Zoom;
    use gpui::Modifiers;

    #[test]
    fn repeated_zoom_stops_at_limits_and_resets() {
        let mut scale = 1.;
        for _ in 0..30 {
            scale = Zoom::In.next(scale);
        }
        assert_eq!(scale, crate::ui_prefs::MAX_ZOOM);
        for _ in 0..30 {
            scale = Zoom::Out.next(scale);
        }
        assert_eq!(scale, crate::ui_prefs::MIN_ZOOM);
        assert_eq!(Zoom::Reset.next(scale), 1.);
        assert_eq!(Zoom::Out.next(Zoom::In.next(1.)), 1.);
    }

    #[test]
    fn command_and_control_zoom_preserve_plain_text_keys() {
        for modifiers in [
            Modifiers {
                platform: true,
                ..Default::default()
            },
            Modifiers {
                control: true,
                ..Default::default()
            },
        ] {
            for key in ["+", "=", "-", "0"] {
                assert!(Zoom::shortcut(key, modifiers).is_some());
                assert!(Zoom::shortcut(key, Modifiers::default()).is_none());
            }
            assert!(Zoom::shortcut("f", modifiers).is_none());
            assert!(Zoom::shortcut(
                "+",
                Modifiers {
                    alt: true,
                    ..modifiers
                }
            )
            .is_none());
        }
    }
}
