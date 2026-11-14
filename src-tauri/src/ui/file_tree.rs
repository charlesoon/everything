mod model;

use super::*;
use design::scaled_px;
use gpui_component::{
    scroll::{Scrollbar, ScrollbarShow},
    tooltip::Tooltip,
};
use model::{FileTree, RowKind};

pub(super) struct State {
    model: FileTree,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    visible_rows: std::ops::Range<usize>,
    result_selection: Option<PathBuf>,
    resizing: Option<(Point<Pixels>, f32)>,
}

impl State {
    pub fn new(cx: &mut Context<EverythingView>) -> Self {
        Self {
            model: FileTree::default(),
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            visible_rows: 0..0,
            result_selection: None,
            resizing: None,
        }
    }

    pub(super) fn rescale_scroll(&mut self, ratio: f32) {
        let scroll = self.scroll.0.borrow();
        let offset = scroll.base_handle.offset();
        scroll
            .base_handle
            .set_offset(point(offset.x * ratio, offset.y * ratio));
        self.resizing = None;
    }

    fn scroll_anchor(&self, row_height: f32) -> Option<(PathBuf, Pixels)> {
        if self.model.reveal_pending {
            return None;
        }
        let offset = self.scroll.0.borrow().base_handle.offset().y;
        let first = (-f32::from(offset) / row_height).floor().max(0.) as usize;
        self.model
            .rows
            .iter()
            .enumerate()
            .skip(first)
            .find_map(|(index, row)| {
                row.entry()
                    .map(|entry| (entry.path.clone(), offset + px(index as f32 * row_height)))
            })
    }

    fn restore_scroll_anchor(&self, anchor: Option<(PathBuf, Pixels)>, row_height: f32) {
        let Some((path, top)) = anchor else {
            return;
        };
        if let Some(index) = self
            .model
            .rows
            .iter()
            .position(|row| row.entry().is_some_and(|entry| entry.path == path))
        {
            let scroll = self.scroll.0.borrow();
            scroll.base_handle.set_offset(point(
                scroll.base_handle.offset().x,
                top - px(index as f32 * row_height),
            ));
        }
    }
}

impl EverythingView {
    pub(super) fn reveal_result_in_tree(&mut self) {
        self.file_tree.result_selection = None;
    }

    pub(super) fn file_tree_width(&self, window: &Window) -> f32 {
        if self.prefs.file_tree_visible {
            let width = f32::from(window.viewport_size().width);
            let zoom = self.prefs.zoom;
            let available = (width - 220. * zoom).max(width * 0.4);
            (self.prefs.file_tree_width * zoom)
                .clamp((180. * zoom).min(available), (640. * zoom).min(available))
        } else {
            0.
        }
    }

    pub(super) fn toggle_file_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_pending_rename();
        self.prefs.file_tree_visible = !self.prefs.file_tree_visible;
        if self.prefs.file_tree_visible {
            self.file_tree.result_selection = None;
            self.file_tree.model.refresh();
            self.file_tree.model.reveal_pending = true;
        } else if self.file_tree.focus.is_focused(window) {
            self.table_focus.focus(window);
        }
        self.save_preferences(cx);
        cx.notify();
    }

    pub(super) fn refresh_file_tree(&mut self, cx: &mut Context<Self>) {
        if self.prefs.file_tree_visible {
            self.file_tree.model.refresh();
            cx.notify();
        }
    }

    pub(super) fn sync_file_tree(&mut self, cx: &mut Context<Self>) {
        if !self.prefs.file_tree_visible {
            return;
        }
        let entry = self
            .model
            .primary
            .and_then(|index| self.model.rows.get(index));
        let selection = entry.map(|entry| PathBuf::from(&entry.path));
        if selection != self.file_tree.result_selection {
            self.file_tree.result_selection = selection.clone();
            if let (Some(path), Some(entry)) = (selection, entry) {
                self.file_tree.model.follow(path, entry.is_dir);
            }
        }
        for request in self.file_tree.model.requests() {
            let path = request.path.clone();
            cx.spawn(async move |view, cx| {
                let result = crate::runtime::spawn_blocking(move || model::read_directory(&path))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                let _ = view.update(cx, |this, cx| {
                    let row_height = this.row_height();
                    let anchor = this.file_tree.scroll_anchor(row_height);
                    if this.file_tree.model.apply(request, result) {
                        this.file_tree.restore_scroll_anchor(anchor, row_height);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        if self.file_tree.model.reveal_pending {
            if let Some(index) = self.file_tree.model.selected_index() {
                self.file_tree
                    .scroll
                    .scroll_to_item(index, ScrollStrategy::Center);
                self.file_tree.model.reveal_pending = false;
            }
        }
    }

    pub(super) fn file_tree_toggle(&self, cx: &Context<Self>) -> Stateful<Div> {
        let p = design::Palette::get(cx);
        let visible = self.prefs.file_tree_visible;
        div()
            .id("toggle-file-tree")
            .absolute()
            .right(px(if cfg!(target_os = "windows") {
                147.
            } else {
                16. // same right inset as the status-bar settings button
            }))
            .top(px(8.)) // (38px titlebar - 22px button) / 2
            .w(px(26.))
            .h(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .text_color(p.muted)
            .hover(|button| button.bg(p.hover))
            .cursor_pointer()
            .tooltip(move |window, cx| {
                Tooltip::new(if visible {
                    "Hide file tree"
                } else {
                    "Show file tree"
                })
                .build(window, cx)
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, window, cx| this.toggle_file_tree(window, cx)))
            .child(
                svg()
                    .path("everything/panel-right.svg")
                    .size(px(16.))
                    .text_color(p.muted),
            )
    }

    pub(super) fn file_tree_resize_handle(&self, width: f32, cx: &Context<Self>) -> Stateful<Div> {
        div()
            .id("resize-file-tree")
            .absolute()
            .left(px(-3.))
            .top_0()
            .bottom_0()
            .w(px(6.))
            .cursor_col_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.file_tree.resizing = Some((event.position, width));
                    cx.stop_propagation();
                }),
            )
    }

    pub(super) fn file_tree_panel(&self, width: f32, cx: &mut Context<Self>) -> Stateful<Div> {
        let p = design::Palette::get(cx);
        let root = self.file_tree.model.root.as_ref();
        let can_go_up = root.and_then(|root| root.parent()).is_some();
        let row_count = self.file_tree.model.rows.len();
        div()
            .id("file-tree")
            .track_focus(&self.file_tree.focus)
            .relative()
            .flex()
            .flex_col()
            .w(px(width))
            .h_full()
            .min_h_0()
            .flex_shrink_0()
            .border_l_1()
            .border_color(p.border)
            .bg(p.bg)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(scaled_px(4.))
                    .h(scaled_px(if cfg!(target_os = "windows") {
                        25.
                    } else {
                        33.
                    }))
                    .px(scaled_px(10.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        div()
                            .flex_1()
                            .text_size(scaled_px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.muted)
                            .child("Files"),
                    )
                    .child(
                        tree_button(
                            "file-tree-up",
                            "everything/arrow-up.svg",
                            "Parent folder",
                            !can_go_up,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.file_tree.model.go_up();
                            cx.notify();
                        })),
                    )
                    .child(
                        tree_button(
                            "file-tree-refresh",
                            "everything/reset.svg",
                            "Refresh folder",
                            root.is_none(),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_file_tree(cx))),
                    ),
            )
            .when_some(root, |panel, root| {
                panel.child(
                    self.copyable_path("file-tree-path", root.to_string_lossy().into_owned(), cx)
                        .h(scaled_px(26.))
                        .px(scaled_px(12.))
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .font_family(design::MONO)
                        .text_size(scaled_px(10.))
                        .text_color(p.muted),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .when(row_count > 0, |area| {
                        area.child(
                            uniform_list(
                                "file-tree-rows",
                                row_count,
                                cx.processor(
                                    move |this, range: std::ops::Range<usize>, window, cx| {
                                        let changed = this.file_tree.visible_rows != range;
                                        this.file_tree.visible_rows = range.clone();
                                        if changed
                                            && std::env::var_os("EVERYTHING_UI_STATE").is_some()
                                        {
                                            cx.on_next_frame(window, |_, _, cx| cx.notify());
                                        }
                                        range
                                            .map(|index| this.file_tree_row(index, width, cx))
                                            .collect::<Vec<_>>()
                                    },
                                ),
                            )
                            .track_scroll(self.file_tree.scroll.clone())
                            .size_full(),
                        )
                        .child(
                            div().absolute().inset_0().child(
                                Scrollbar::vertical(&self.file_tree.scroll)
                                    .scrollbar_show(ScrollbarShow::Always),
                            ),
                        )
                    })
                    .when(row_count == 0, |area| {
                        area.flex()
                            .items_center()
                            .justify_center()
                            .px(scaled_px(20.))
                            .child(
                                div()
                                    .text_center()
                                    .text_size(scaled_px(12.))
                                    .line_height(scaled_px(19.))
                                    .text_color(p.muted)
                                    .child("Select a search result to view its folder."),
                            )
                    }),
            )
            .when_some(self.file_tree.model.selection_error(), |panel, error| {
                panel.child(
                    div()
                        .id("file-tree-selection-error")
                        .px(scaled_px(12.))
                        .py(scaled_px(8.))
                        .flex_shrink_0()
                        .border_t_1()
                        .border_color(p.border)
                        .text_size(scaled_px(11.))
                        .text_color(p.muted)
                        .tooltip({
                            let error = error.clone();
                            move |window, cx| Tooltip::new(error.clone()).build(window, cx)
                        })
                        .child(div().truncate().child(error)),
                )
            })
            .child(self.file_tree_resize_handle(width, cx))
    }

    fn file_tree_row(&self, index: usize, width: f32, cx: &mut Context<Self>) -> Stateful<Div> {
        let row = &self.file_tree.model.rows[index];
        let p = design::Palette::get(cx);
        // Leave room for the name even after following a deeply nested result.
        let indent = (row.depth as f32 * 14.).min((width / self.prefs.zoom - 146.).max(0.));
        let container = div()
            .id(("file-tree-row", index))
            .relative()
            .flex()
            .items_center()
            .gap(scaled_px(5.))
            .h(scaled_px(design::ROW_HEIGHT))
            .w_full()
            .pl(scaled_px(8. + indent))
            .pr(scaled_px(20.))
            .text_size(scaled_px(12.))
            .line_height(scaled_px(15.))
            .text_color(p.text)
            .overflow_hidden();
        let entry = match &row.kind {
            RowKind::Entry(entry) => entry,
            kind => {
                let message = match kind {
                    RowKind::Loading => "Loading…".into(),
                    RowKind::Empty => "Empty folder".into(),
                    RowKind::Error(error) => format!("Cannot read folder: {error}"),
                    _ => unreachable!(),
                };
                return container
                    .text_color(p.muted)
                    .pl(scaled_px(28. + indent))
                    .tooltip({
                        let message = message.clone();
                        move |window, cx| Tooltip::new(message.clone()).build(window, cx)
                    })
                    .child(div().truncate().child(message));
            }
        };
        let selected = self.file_tree.model.selected.as_ref() == Some(&entry.path);
        let expanded = self.file_tree.model.is_expanded(&entry.path);
        let path = entry.path.clone();
        let full_path = path.to_string_lossy().into_owned();
        let is_dir = entry.is_dir;
        container
            .when(selected, |row| row.bg(p.selected).text_color(p.accent))
            .hover(|row| row.bg(if selected { p.selected } else { p.hover }))
            .cursor_pointer()
            .tooltip(move |window, cx| Tooltip::new(full_path.clone()).build(window, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    this.file_tree.focus.focus(window);
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.file_tree.model.selected = Some(path.clone());
                this.file_tree.model.reveal_pending = false;
                if is_dir && event.click_count() == 1 {
                    this.file_tree.model.toggle(&path);
                } else if !is_dir && event.click_count() == 2 {
                    this.open_tree_file(window, cx);
                }
                cx.notify();
            }))
            .child(
                div()
                    .size(scaled_px(12.))
                    .flex_shrink_0()
                    .when(is_dir, |slot| {
                        slot.child(
                            svg()
                                .path(if expanded {
                                    "everything/chevron-down.svg"
                                } else {
                                    "everything/chevron-right.svg"
                                })
                                .size(scaled_px(12.))
                                .text_color(p.muted),
                        )
                    }),
            )
            .child(
                svg()
                    .path(if is_dir {
                        super::file_icons::folder(expanded)
                    } else {
                        super::file_icons::file(&entry.name)
                    })
                    .size(scaled_px(14.))
                    .flex_shrink_0()
                    .text_color(p.muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(row.depth == 0, |name| name.font_weight(FontWeight::MEDIUM))
                    .child(entry.name.clone()),
            )
    }

    pub(super) fn resize_file_tree(
        &mut self,
        event: &MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((start, width)) = self.file_tree.resizing else {
            return false;
        };
        let maximum =
            (f32::from(window.viewport_size().width) / self.prefs.zoom - 220.).clamp(180., 640.);
        self.prefs.file_tree_width = ((width + f32::from(start.x - event.position.x))
            / self.prefs.zoom)
            .clamp(180., maximum);
        cx.notify();
        true
    }

    pub(super) fn finish_file_tree_resize(&mut self, cx: &mut Context<Self>) {
        if self.file_tree.resizing.take().is_some() {
            self.save_preferences(cx);
        }
    }

    pub(super) fn file_tree_focused(&self, window: &Window) -> bool {
        self.prefs.file_tree_visible && self.file_tree.focus.is_focused(window)
    }

    pub(super) fn file_tree_key_down(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let entries = self
            .file_tree
            .model
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.entry().map(|_| index))
            .collect::<Vec<_>>();
        let current = entries.iter().position(|index| {
            self.file_tree.model.rows[*index]
                .entry()
                .is_some_and(|entry| self.file_tree.model.selected.as_ref() == Some(&entry.path))
        });
        match key {
            "up" | "down" | "home" | "end" | "pageup" | "pagedown" if !entries.is_empty() => {
                let page = self.file_tree.visible_rows.len().max(1);
                let next = match key {
                    "home" => 0,
                    "end" => entries.len() - 1,
                    "up" => current.unwrap_or(0).saturating_sub(1),
                    "pageup" => current.unwrap_or(0).saturating_sub(page),
                    "pagedown" => (current.unwrap_or(0) + page).min(entries.len() - 1),
                    _ => current.map_or(0, |index| (index + 1).min(entries.len() - 1)),
                };
                self.file_tree.model.selected = self.file_tree.model.rows[entries[next]]
                    .entry()
                    .map(|entry| entry.path.clone());
                self.file_tree
                    .scroll
                    .scroll_to_item(entries[next], ScrollStrategy::Center);
            }
            "left" | "right" | "enter" | "space" => {
                if let Some(current) = current {
                    let entry = self.file_tree.model.rows[entries[current]]
                        .entry()
                        .unwrap()
                        .clone();
                    let expanded = self.file_tree.model.is_expanded(&entry.path);
                    if entry.is_dir
                        && ((key == "left" && expanded)
                            || (key == "right" && !expanded)
                            || key == "enter"
                            || key == "space")
                    {
                        self.file_tree.model.toggle(&entry.path);
                    } else if key == "left" {
                        if let Some(parent) = entry
                            .path
                            .parent()
                            .filter(|parent| {
                                entries.iter().any(|index| {
                                    self.file_tree.model.rows[*index]
                                        .entry()
                                        .is_some_and(|entry| entry.path == *parent)
                                })
                            })
                            .map(std::path::Path::to_path_buf)
                        {
                            self.file_tree.model.selected = Some(parent);
                        }
                    } else if key == "right" && expanded {
                        if let Some(child) = entries
                            .get(current + 1)
                            .and_then(|index| self.file_tree.model.rows[*index].entry())
                            .filter(|child| child.path.parent() == Some(entry.path.as_path()))
                        {
                            self.file_tree.model.selected = Some(child.path.clone());
                        }
                    } else if key == "enter" {
                        self.open_tree_file(window, cx);
                    }
                    if let Some(index) = self.file_tree.model.selected_index() {
                        self.file_tree
                            .scroll
                            .scroll_to_item(index, ScrollStrategy::Center);
                    }
                }
            }
            "escape" | "tab" => self.table_focus.focus(window),
            _ => return false,
        }
        cx.notify();
        true
    }

    fn open_tree_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.file_tree.model.selected.clone() else {
            return;
        };
        if self.action_busy {
            return;
        }
        self.action_busy = true;
        cx.spawn_in(window, async move |view, cx| {
            let result = crate::open(vec![path.to_string_lossy().into_owned()]).await;
            let _ = view.update(cx, |this, cx| {
                this.action_busy = false;
                this.completed_action = Some(UiAction::Open);
                if let Err(error) = result {
                    this.notify_message(error, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn file_tree_test_state(&self, window: &Window) -> serde_json::Value {
        let scroll = self.file_tree.scroll.0.borrow();
        let bounds = scroll.base_handle.bounds();
        serde_json::json!({
            "visible": self.prefs.file_tree_visible,
            "width": self.file_tree_width(window),
            "root": self.file_tree.model.root,
            "selected": self.file_tree.model.selected,
            "focused": self.file_tree.focus.is_focused(window),
            "visibleRange": [self.file_tree.visible_rows.start, self.file_tree.visible_rows.end],
            "listBounds": [f32::from(bounds.origin.x), f32::from(bounds.origin.y), f32::from(bounds.size.width), f32::from(bounds.size.height)],
            "scrollOffset": f32::from(scroll.base_handle.offset().y),
            "rowCount": self.file_tree.model.rows.len(),
            "selectedIndex": self.file_tree.model.selected_index(),
            "selectionError": self.file_tree.model.selection_error(),
            "loading": self.file_tree.model.is_loading(),
            "rows": self.file_tree.model.rows.iter().enumerate().filter(|(index, _)| self.file_tree.visible_rows.contains(index)).map(|(index, row)| {
                match &row.kind {
                    RowKind::Entry(entry) => serde_json::json!({"index":index,"depth":row.depth,"path":entry.path,"name":entry.name,"isDir":entry.is_dir,"expanded":self.file_tree.model.is_expanded(&entry.path)}),
                    kind => serde_json::json!({"index":index,"depth":row.depth,"message":match kind {RowKind::Loading=>"Loading…".into(),RowKind::Empty=>"Empty folder".into(),RowKind::Error(error)=>error.clone(),_=>unreachable!()}}),
                }
            }).collect::<Vec<_>>(),
        })
    }
}

fn tree_button(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    disabled: bool,
    cx: &Context<EverythingView>,
) -> Stateful<Div> {
    let p = design::Palette::get(cx);
    div()
        .id(id)
        .size(scaled_px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(scaled_px(4.))
        .text_color(p.muted)
        .when(disabled, |button| button.opacity(0.35))
        .when(!disabled, |button| {
            button.cursor_pointer().hover(|button| button.bg(p.hover))
        })
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .child(svg().path(icon).size(scaled_px(12.)).text_color(p.muted))
}
