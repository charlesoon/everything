use super::*;
use design::{scaled_px, Palette, COLUMN_MIN, MONO, ROW_HEIGHT, SANS};
use gpui_component::{
    scroll::{Scrollbar, ScrollbarShow},
    tooltip::Tooltip,
};

impl EverythingView {
    pub(super) fn copyable_path(
        &self,
        id: &'static str,
        path: String,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let p = Palette::get(cx);
        let label = display_path(&path, &self.backend.home_dir);
        let tooltip = format!("{path}\nClick to copy path");
        div()
            .id(id)
            .min_w_0()
            .overflow_hidden()
            .cursor_pointer()
            .hover(|path| path.text_color(p.accent))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                let time = Instant::now();
                this.copied = Some((id, time));
                cx.spawn(async move |view, cx| {
                    Timer::after(Duration::from_millis(1500)).await;
                    let _ = view.update(cx, |this, cx| {
                        if this.copied.is_some_and(|(_, t)| t == time) {
                            this.copied = None;
                            cx.notify();
                        }
                    });
                })
                .detach();
                cx.notify();
            }))
            .flex()
            .items_center()
            .child(middle_text::middle_path(label))
            .when(self.copied.is_some_and(|(copied, _)| copied == id), |d| {
                d.child(
                    // Pill badge, like Orca's "파일 경로 복사됨".
                    div()
                        .flex_shrink_0()
                        .ml(scaled_px(8.))
                        .px(scaled_px(8.))
                        .py(scaled_px(2.))
                        .rounded_full()
                        .bg(p.selected)
                        .text_color(p.text)
                        .font_family(SANS)
                        .font_weight(FontWeight::MEDIUM)
                        .child("Copied"),
                )
            })
    }

    fn button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        action: UiAction,
        disabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let p = Palette::get(cx);
        div()
            .id(id)
            .h(scaled_px(22.))
            .px(scaled_px(8.))
            .flex()
            .items_center()
            .gap(scaled_px(3.))
            .font_family(SANS)
            .text_size(scaled_px(11.))
            .line_height(scaled_px(14.))
            .border_1()
            .border_color(p.border)
            .rounded(scaled_px(5.))
            .bg(p.button)
            .text_color(p.button_text)
            .when(disabled, |b| b.opacity(0.55))
            .when(!disabled, |b| {
                b.cursor_pointer().hover(|b| b.bg(p.hover)).on_click(
                    cx.listener(move |this, _, window, cx| this.action(action, window, cx)),
                )
            })
            .child(label.into())
    }

    fn row(&self, index: usize, widths: [f32; 4], cx: &mut Context<Self>) -> Stateful<Div> {
        let entry = &self.model.rows[index];
        let selected = self.model.selected.contains(&index);
        let p = Palette::get(cx);
        let editing = !self.rename_in_tree && self.rename_path.as_deref() == Some(&entry.path);
        let style = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        let name = div().truncate().when(editing, |d| d.opacity(0.25)).child(
            StyledText::new(entry.name.clone()).with_highlights(
                highlight(&entry.name, &self.model.query)
                    .into_iter()
                    .map(|range| (range, style)),
            ),
        );
        let rename_width = (self
            .rename_input
            .read(cx)
            .value()
            .encode_utf16()
            .count()
            .max(1) as f32
            * 8.5
            + 12.)
            .max(120.)
            .min((widths[0] + widths[1]) / self.prefs.zoom - 38.);
        let icon = svg()
            .path(if entry.is_dir {
                super::file_icons::folder(false)
            } else {
                super::file_icons::file(&entry.name)
            })
            .size(scaled_px(14.))
            .flex_shrink_0()
            .text_color(p.muted);
        div()
            .id(("result", index))
            .relative()
            .flex()
            .items_center()
            .h(scaled_px(ROW_HEIGHT))
            .w(px(widths.iter().sum()))
            .border_b_1()
            .border_color(p.row_border)
            .text_size(scaled_px(12.))
            .line_height(scaled_px(15.))
            .text_color(p.text)
            .when(selected, |r| r.bg(p.selected))
            .hover(|r| r.bg(if selected { p.selected } else { p.hover }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if this.modal.is_some()
                        || (!this.rename_in_tree
                            && this.rename_path.as_deref() == Some(&this.model.rows[index].path))
                    {
                        return;
                    }
                    this.context_menu = None;
                    this.target_tree = false;
                    this.rename_click_candidate = (cfg!(target_os = "macos")
                        && event.click_count == 1
                        && !event.modifiers.alt
                        && !event.modifiers.platform
                        && !event.modifiers.control
                        && !event.modifiers.shift
                        && this.rename_path.is_none()
                        && this.model.selected.len() == 1
                        && this.model.selected.contains(&index))
                    .then(|| this.model.rows[index].path.clone());
                    if this.rename_path.is_some() {
                        this.commit_rename(window, cx);
                    }
                    let toggle = event.modifiers.platform || event.modifiers.control;
                    if !this.model.selected.contains(&index)
                        || toggle
                        || event.modifiers.shift
                        || this.model.selected.len() == 1
                    {
                        this.model.select(index, toggle, event.modifiers.shift);
                    }
                    this.reveal_result_in_tree();
                    this.table_focus.focus(window);
                    this.drag_start = Some((event.position, index));
                    cx.notify();
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() == 2 {
                    this.cancel_pending_rename();
                    this.model.select(index, false, false);
                    this.action(UiAction::Open, window, cx);
                } else if !event.modifiers().platform
                    && !event.modifiers().control
                    && !event.modifiers().shift
                    && this.rename_path.is_none()
                {
                    let rename = this.rename_click_candidate.take();
                    this.model.select(index, false, false);
                    if let Some(path) = rename {
                        this.schedule_click_rename(path, window, cx);
                    }
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if this.modal.is_some() {
                        return;
                    }
                    if !this.model.selected.contains(&index) {
                        this.model.select(index, false, false);
                    }
                    this.target_tree = false;
                    this.reveal_result_in_tree();
                    this.table_focus.focus(window);
                    this.open_context_menu(event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(scaled_px(6.))
                    .w(px(widths[0]))
                    .h_full()
                    .flex_shrink_0()
                    .pl(scaled_px(16.))
                    .pr(scaled_px(8.))
                    .overflow_hidden()
                    .child(icon)
                    .child(div().flex_1().min_w_0().child(name)),
            )
            .child(
                div()
                    .w(px(widths[1]))
                    .flex_shrink_0()
                    .pl(scaled_px(8.))
                    .pr(scaled_px(4.))
                    .font_family(MONO)
                    .text_size(scaled_px(11.))
                    .text_color(p.muted)
                    .overflow_hidden()
                    .flex() // lets the path shrink below its full width
                    .child(middle_text::middle_path(display_path(&entry.dir, &self.backend.home_dir))),
            )
            .child(
                div()
                    .w(px(widths[2]))
                    .flex_shrink_0()
                    .text_right()
                    .font_family(MONO)
                    .text_size(scaled_px(11.))
                    .text_color(p.muted)
                    .truncate()
                    .child(format_size(entry)),
            )
            .child(
                div()
                    .w(px(widths[3]))
                    .flex_shrink_0()
                    .pl(scaled_px(8.))
                    .pr(scaled_px(16.))
                    .font_family(MONO)
                    .text_size(scaled_px(11.))
                    .text_color(p.muted)
                    .truncate()
                    .child(format_modified(entry.mtime)),
            )
            .when(editing, |r| {
                r.child(
                    div()
                        .absolute()
                        .left(scaled_px(29.))
                        .top_0()
                        .w(scaled_px(rename_width))
                        .h(scaled_px(26.))
                        .child(
                            Input::new(&self.rename_input)
                                .appearance(false)
                                .w_full()
                                .min_h(scaled_px(26.))
                                .max_h(scaled_px(26.))
                                .px(scaled_px(4.))
                                .py_0()
                                .font_family(SANS)
                                .text_size(scaled_px(12.))
                                .line_height(scaled_px(18.))
                                .bg(p.input)
                                .border(scaled_px(1.5))
                                .border_color(p.accent)
                                .rounded(scaled_px(4.))
                                .shadow(vec![BoxShadow {
                                    color: p.accent.opacity(0.13),
                                    offset: point(px(0.), px(0.)),
                                    blur_radius: px(0.),
                                    spread_radius: px(3. * self.prefs.zoom),
                                }]),
                        ),
                )
            })
            .when(selected, |r| {
                r.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(scaled_px(3.))
                        .bg(p.accent),
                )
            })
    }

    fn context_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        // macOS and Windows use their original native context menus.
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            return div().into_any_element();
        }
        let Some(position) = self.context_menu else {
            return div().into_any_element();
        };
        let p = Palette::get(cx);
        div()
            .absolute()
            .left(position.x)
            .top(position.y)
            .w(scaled_px(222.))
            .rounded(scaled_px(8.))
            .border_1()
            .border_color(p.border)
            .bg(p.menu)
            .text_color(p.menu_text)
            .overflow_hidden()
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.context_menu = None;
                cx.notify();
            }))
            .children(self.context_actions().into_iter().enumerate().map(
                |(i, (label, action, _))| {
                    div()
                        .id(("context", i))
                        .px(scaled_px(10.))
                        .py(scaled_px(8.))
                        .text_size(scaled_px(12.))
                        .cursor_pointer()
                        .when(i == self.menu_selection, |r| r.bg(p.menu_hover))
                        .hover(|r| r.bg(p.menu_hover))
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            this.menu_selection = i;
                            cx.notify();
                        }))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.action(action, window, cx)),
                        )
                        .child(label)
                },
            ))
            .into_any_element()
    }

    fn dialog_button(
        &self,
        id: &'static str,
        label: &'static str,
        action: UiAction,
        index: usize,
        disabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let p = Palette::get(cx);
        div()
            .id(id)
            .px(scaled_px(12.))
            .py(scaled_px(6.))
            .border_1()
            .border_color(p.border)
            .rounded(scaled_px(6.))
            .bg(p.button)
            .text_color(p.button_text)
            .text_size(scaled_px(11.))
            .line_height(scaled_px(14.))
            .when(self.modal_selection == index, |b| {
                b.shadow(vec![BoxShadow {
                    color: p.muted.opacity(0.4),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(3. * self.prefs.zoom),
                }])
            })
            .when(disabled, |b| b.opacity(0.45))
            .when(!disabled, |b| {
                b.cursor_pointer().on_click(
                    cx.listener(move |this, _, window, cx| this.action(action, window, cx)),
                )
            })
            .child(label)
    }

    /// Actionable rows of the settings popover, in display (and arrow-key) order.
    pub(super) fn settings_items(&self) -> Vec<UiAction> {
        let mut items = Vec::new();
        if cfg!(target_os = "macos") {
            items.extend([UiAction::FdaSettings, UiAction::Autostart]);
        }
        items.extend([UiAction::Add, UiAction::Ignore, UiAction::Reset]);
        items
    }

    /// Gear popover, laid out like Orca's status-bar usage panel.
    fn settings_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = Palette::get(cx);
        let indexing = self.status.state == "Indexing";
        let autostart = self.onboarding.as_ref();
        let autostart_on = autostart.is_some_and(|s| !s.opted_out && s.registration == "enabled");
        let autostart_unsupported = autostart.is_none_or(|s| s.registration == "unsupported");
        let items = self.settings_items();
        let tile = |icon: &'static str| {
            div()
                .size(scaled_px(20.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(scaled_px(6.))
                .border_1()
                .border_color(p.border)
                .bg(p.hover)
                .child(svg().path(icon).size(scaled_px(12.)).text_color(p.text))
        };
        let chevron = || {
            svg()
                .path("everything/chevron.svg") // Lucide ChevronRight, as in Orca
                .size(scaled_px(14.))
                .flex_shrink_0()
                .text_color(p.muted)
        };
        let meta = |text: SharedString| {
            div()
                .flex_shrink_0()
                .ml(scaled_px(8.))
                .mr(scaled_px(4.))
                .text_size(scaled_px(11.))
                .text_color(p.muted)
                .whitespace_nowrap()
                .child(text)
        };
        let row = |action: UiAction, disabled: bool, cx: &mut Context<Self>| {
            let index = items
                .iter()
                .position(|a| std::mem::discriminant(a) == std::mem::discriminant(&action))
                .unwrap_or(0);
            div()
                .id(("settings-item", index))
                .flex()
                .items_center()
                .gap(scaled_px(10.))
                .px(scaled_px(14.))
                .py(scaled_px(9.))
                .when(disabled, |r| r.opacity(0.45))
                .when(!disabled, |r| {
                    r.cursor_pointer()
                        .when(self.modal_selection == index, |r| r.bg(p.hover))
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            if this.modal_selection != index {
                                this.modal_selection = index;
                                cx.notify();
                            }
                        }))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.action(action, window, cx)),
                        )
                })
        };
        let name = |text: &'static str| {
            div()
                .flex_1()
                .min_w_0()
                .text_size(scaled_px(13.))
                .font_weight(FontWeight::MEDIUM)
                .truncate()
                .child(text)
        };
        let separator = || div().border_t_1().border_color(p.border);
        let (dot, state) = match self.status.state.as_str() {
            "Indexing" => (0xfebc2e, "Indexing"),
            "Error" => (0xff5f57, "Error"),
            _ => (0x28c840, "Index ready"),
        };
        let switch = div()
            .ml(scaled_px(8.))
            .w(scaled_px(28.))
            .h(scaled_px(16.))
            .p(scaled_px(2.))
            .flex()
            .flex_shrink_0()
            .rounded_full()
            .bg(if autostart_on { p.accent } else { p.border })
            .when(autostart_on, |s| s.justify_end())
            .child(
                div()
                    .size(scaled_px(12.))
                    .rounded_full()
                    .bg(p.menu)
                    .shadow_sm(),
            );
        let popover = div()
            .id("settings-popover")
            .absolute()
            .right(scaled_px(8.))
            .bottom(scaled_px(40.))
            .w(scaled_px(300.))
            .max_w(relative(0.95))
            .max_h(relative(0.7))
            .rounded(scaled_px(10.))
            .border_1()
            .border_color(p.border)
            .bg(p.menu)
            .text_color(p.menu_text)
            .text_size(scaled_px(12.))
            .shadow_lg()
            .overflow_hidden()
            .overflow_y_scroll()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(scaled_px(14.))
                    .pt(scaled_px(12.))
                    .pb(scaled_px(8.))
                    .child(
                        div()
                            .text_size(scaled_px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(scaled_px(6.))
                            .text_size(scaled_px(11.))
                            .text_color(p.muted)
                            .child(div().size(scaled_px(6.)).rounded_full().bg(rgb(dot)))
                            .child(state),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(scaled_px(10.))
                    .px(scaled_px(14.))
                    .py(scaled_px(9.))
                    .child(tile("everything/database.svg"))
                    .child(name("Index"))
                    .child(meta(
                        format!("{} items", thousands(self.status.entries_count)).into(),
                    )),
            )
            .when(cfg!(target_os = "macos"), |pop| {
                pop.child(
                    row(UiAction::FdaSettings, false, cx)
                        .child(tile("everything/shield-check.svg"))
                        .child(name("Full Disk Access"))
                        .child(meta(
                            if self.fda_ok {
                                "Granted"
                            } else {
                                "Not granted"
                            }
                            .into(),
                        ))
                        .child(chevron()),
                )
                .child(
                    row(
                        UiAction::Autostart,
                        self.action_busy || autostart_unsupported,
                        cx,
                    )
                    .child(tile("everything/settings.svg"))
                    .child(name("Launch at login"))
                    .child(switch),
                )
            })
            .child(separator())
            .child(
                row(UiAction::Add, false, cx)
                    .child(tile("everything/folder-plus.svg"))
                    .child(name("Indexed folders"))
                    .child(meta(".pathindexing".into()).font_family(MONO))
                    .child(chevron()),
            )
            .child(
                row(UiAction::Ignore, false, cx)
                    .child(tile("everything/folder-minus.svg"))
                    .child(name("Excluded paths"))
                    .child(meta(".pathignore".into()).font_family(MONO))
                    .child(chevron()),
            )
            .child(separator())
            .child(
                row(UiAction::Reset, indexing, cx)
                    .child(
                        div()
                            .flex_1()
                            .text_size(scaled_px(13.))
                            .text_color(rgb(if cx.theme().is_dark() {
                                0xf87171
                            } else {
                                0xdc2626
                            }))
                            .child("Reset index"),
                    )
                    .child(meta("Rebuild from scratch".into()))
                    .child(chevron()),
            );
        // Transparent backdrop: a click anywhere outside closes the popover
        // (and swallows the click, so the gear toggles instead of reopening).
        div()
            .id("settings-backdrop")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.action(UiAction::Cancel, window, cx);
                }),
            )
            .child(popover)
            .into_any_element()
    }

    fn modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(modal) = &self.modal else {
            return div().into_any_element();
        };
        let p = Palette::get(cx);
        let onboarding = matches!(modal, Modal::Onboarding);
        if matches!(modal, Modal::Settings) {
            return self.settings_popover(cx);
        }
        let body = if let Modal::Trash(paths) = modal {
            let title = if paths.len() == 1 {
                "Move selected item to Trash?".into()
            } else {
                format!("Move {} items to Trash?", paths.len())
            };
            div()
                .p(scaled_px(24.))
                .max_w(scaled_px(410.))
                .rounded(scaled_px(8.))
                .bg(p.menu)
                .border_1()
                .border_color(p.border)
                .shadow_xl()
                .child(
                    div()
                        .mb(scaled_px(24.))
                        .text_size(scaled_px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(scaled_px(8.))
                        .child(self.dialog_button(
                            "dialog-cancel",
                            "Cancel",
                            UiAction::Cancel,
                            0,
                            false,
                            cx,
                        ))
                        .child(self.dialog_button(
                            "dialog-confirm",
                            "Move to Trash",
                            UiAction::Confirm,
                            1,
                            self.action_busy,
                            cx,
                        )),
                )
                .into_any_element()
        } else {
            let intro = "백그라운드 인덱서가 로그인할 때 자동으로 실행되어 파일 인덱스를 항상 최신으로 유지합니다. 보호된 폴더(데스크탑·문서·다운로드 등)까지 검색하려면 전체 디스크 접근 권한이 필요합니다.";
            let emphasis = intro.find("전체 디스크 접근").unwrap();
            div().id("onboarding-body").w(scaled_px(482.)).max_w_full().max_h_full().overflow_y_scroll()
                .px(scaled_px(20.)).pt(scaled_px(18.)).pb(scaled_px(16.)).rounded(scaled_px(8.)).border_1().border_color(p.border)
                .bg(p.menu).text_size(scaled_px(12.5)).line_height(relative(1.65)).shadow_xl()
                .child(div().mb(scaled_px(8.)).text_size(scaled_px(15.)).font_weight(FontWeight::BOLD).child("Everything 시작하기"))
                .child(div().mb(scaled_px(10.)).child(StyledText::new(intro).with_highlights([(emphasis..emphasis + "전체 디스크 접근".len(), HighlightStyle {font_weight: Some(FontWeight::BOLD), ..Default::default()})])))
                .child(div().flex().items_center().gap(scaled_px(8.)).px(scaled_px(10.)).py(scaled_px(8.)).mb(scaled_px(10.)).rounded(scaled_px(8.)).bg(rgba(if cx.theme().is_dark(){0xe8a02038}else{0xe8a0202e}))
                    .child(div().flex_1().font_weight(if self.fda_ok{FontWeight::SEMIBOLD}else{FontWeight::MEDIUM}).when(self.fda_ok,|d|d.text_color(rgb(0x1f9d57))).child(if self.fda_ok{"✓ 전체 디스크 접근 허용됨"}else{"전체 디스크 접근이 아직 허용되지 않았습니다"}))
                    .when(!self.fda_ok,|d|d
                        .child(self.dialog_button("onboarding-settings","설정 열기",UiAction::FdaSettings,0,false,cx).text_size(scaled_px(11.5)).font_weight(FontWeight::MEDIUM).px(scaled_px(10.)).py(scaled_px(3.)).rounded(scaled_px(5.)).border_0().bg(rgb(0xe8a020)).text_color(white()))
                        .child(self.dialog_button("onboarding-restart","앱 재시작",UiAction::Restart,1,false,cx).text_size(scaled_px(11.5)).font_weight(FontWeight::MEDIUM).px(scaled_px(10.)).py(scaled_px(3.)).rounded(scaled_px(5.)).border_0().bg(rgb(0xe8a020)).text_color(white()))))
                .child(div().mb(scaled_px(10.)).text_size(scaled_px(11.5)).opacity(0.75).child("시스템 설정 > 개인정보 보호 및 보안 > 전체 디스크 접근에서 Everything을 켜 주세요. 목록에 없으면 + 버튼으로 응용 프로그램에서 추가할 수 있습니다. 허용하면 이 창이 자동으로 감지합니다 — 잠시 후에도 감지되지 않으면 (시스템이 재실행을 요구하는 경우) '앱 재시작'을 눌러 주세요."))
                .child(div().flex().justify_end().gap(scaled_px(8.)).mt(scaled_px(4.))
                    .child(self.dialog_button("onboarding-start","시작하기",UiAction::OnboardGranted,3,self.action_busy||!self.fda_ok,cx).text_size(scaled_px(12.)).font_weight(FontWeight::SEMIBOLD).px(scaled_px(14.)).py(scaled_px(5.)).border_color(transparent_black()).bg(p.accent).text_color(p.bg))
                    .child(self.dialog_button("onboarding-skip","나중에 (제한된 인덱스로 시작)",UiAction::OnboardSkip,2,self.action_busy,cx).text_size(scaled_px(12.)).font_weight(FontWeight::SEMIBOLD).px(scaled_px(14.)).py(scaled_px(5.)).bg(transparent_black()).text_color(p.text)))
                .into_any_element()
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p(scaled_px(24.))
            .bg(rgba(if onboarding { 0x00000073 } else { 0x00000040 }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(body)
            .into_any_element()
    }

    fn titlebar(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let p = Palette::get(cx);
        let tree_width = self.file_tree_width(window);
        div()
            .id("titlebar")
            .relative()
            .flex()
            .items_center()
            .h(px(design::TITLEBAR_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .when(!cfg!(target_os = "windows"), |title| {
                title.on_mouse_down(MouseButton::Left, |event, window, _| {
                    if event.click_count == 2 {
                        window.titlebar_double_click();
                    } else {
                        window.start_window_move();
                    }
                })
            })
            .when(cfg!(target_os = "windows"), |title| {
                // Windows start_window_move() is a no-op in GPUI 0.2.2.
                // Native caption hit testing handles move, snap, double-click
                // and drag-to-restore. Keep all three 46px buttons outside it.
                title.child(
                    div()
                        .id("titlebar-drag")
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .right(px(46. * 3.))
                        .window_control_area(WindowControlArea::Drag),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_center()
                    // Left-aligned; on macOS start past the traffic lights
                    // (16 + 2 * 23 + 14 = 76pt, see mac_menu::position_buttons).
                    .pl(px(if cfg!(target_os = "macos") { 88. } else { 12. }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                svg()
                                    .path("everything/search.svg")
                                    .size(px(14.))
                                    .text_color(p.muted),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .line_height(px(12.))
                                    .text_color(p.muted)
                                    .child("Everything"),
                            ),
                    ),
            )
            .when(self.prefs.file_tree_visible, |title| {
                title.child(
                    div()
                        .id("file-tree-titlebar")
                        .relative()
                        .w(px(tree_width))
                        .h_full()
                        .flex_shrink_0()
                        .border_l_1()
                        .border_color(p.border)
                        .child(self.file_tree_toggle(cx))
                        .child(self.file_tree_resize_handle(tree_width, cx)),
                )
            })
            .when(!self.prefs.file_tree_visible, |title| {
                title.child(self.file_tree_toggle(cx))
            })
            .when(self.zoom_button_visible(), |title| {
                title.child(self.zoom_button(window, cx))
            })
            .when(cfg!(target_os = "windows"), |title| {
                title.child(
                    div()
                        .absolute()
                        .right_0()
                        .top_0()
                        .h_full()
                        .flex()
                        .child(
                            window_control("minimize", "everything/minimize.svg", false, p.text)
                                .on_click(|_, window, _| window.minimize_window()),
                        )
                        .child(
                            window_control(
                                "maximize",
                                if window.is_maximized() {
                                    "everything/restore.svg"
                                } else {
                                    "everything/maximize.svg"
                                },
                                false,
                                p.text,
                            )
                            .on_click(|_, window, _| toggle_maximized(window)),
                        )
                        .child(
                            window_control("close", "everything/close.svg", true, p.text)
                                .on_click(|_, window, _| window.remove_window()),
                        ),
                )
            })
    }

    fn statusbar(&self, cx: &Context<Self>) -> Div {
        let p = Palette::get(cx);
        let selected = self.model.paths();
        let state = self.status_display.state_text(&self.status, Instant::now());
        let color = match self.status.state.as_str() {
            "Error" => 0xff5f57,
            "Indexing" => 0xfebc2e,
            _ => 0x28c840,
        };
        let dot = div().size(scaled_px(6.)).rounded_full().bg(rgb(color));
        let dot = if self.status.state == "Indexing"
            || (self.status.state == "Ready" && self.status.background_active)
        {
            dot.with_animation(
                "index-pulse",
                Animation::new(Duration::from_millis(1400))
                    .repeat()
                    .with_easing(bounce(ease_in_out)),
                |dot, phase| dot.opacity(1. - phase * 0.75),
            )
            .into_any_element()
        } else {
            dot.into_any_element()
        };
        let selection_summary = (selected.len() > 1).then(|| {
            let total = self
                .model
                .selected
                .iter()
                .filter_map(|i| self.model.rows.get(*i))
                .filter_map(|e| e.size)
                .sum::<i64>();
            format!("{} selected, Size: {}", selected.len(), format_bytes(total))
        });
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(scaled_px(33.))
            .px(scaled_px(16.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(p.border)
            .font_family(MONO)
            .text_size(scaled_px(11.))
            .line_height(scaled_px(14.))
            .text_color(p.muted)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(scaled_px(16.))
                    .min_w_0()
                    .overflow_hidden()
                    .when(selected.len() == 1, |d| {
                        d.child(self.copyable_path("status-file-path", selected[0].clone(), cx))
                    })
                    .when_some(selection_summary, |d, summary| {
                        d.child(div().truncate().child(summary))
                    })
                    .when(selected.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(scaled_px(6.))
                                .flex_shrink_0()
                                .child(dot)
                                .child(state),
                        )
                        .when_some(self.status_display.finished_text(), |d, text| d.child(text))
                        .when(!self.displayed_query.is_empty(), |d| {
                            d.child(div().whitespace_nowrap().child(format!(
                                "\"{}\" · {} results",
                                self.displayed_query, self.model.total,
                            )))
                        })
                    })
                    .when(self.model.mode.starts_with("spotlight"), |d| {
                        d.child(div().text_color(rgb(0xf59e0b)).child(
                            if self.model.mode == "spotlight_timeout" {
                                "Spotlight fallback (partial)"
                            } else {
                                "Spotlight fallback"
                            },
                        ))
                    })
                    .when_some(self.status.message.clone(), |d, message| {
                        let color = if status::is_catchup(&self.status) {
                            p.muted
                        } else if self.status.state == "Error" {
                            rgb(if cx.theme().is_dark() {
                                0xff9d9d
                            } else {
                                0xd93025
                            })
                            .into()
                        } else {
                            rgb(if cx.theme().is_dark() {
                                0xe0c670
                            } else {
                                0xb45309
                            })
                            .into()
                        };
                        d.child(div().text_color(color).child(message))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(scaled_px(8.))
                    .flex_shrink_0()
                    .child(
                        // Ghost icon button: Add/Ignore/Reset live in the settings dialog.
                        self.button("settings", "", UiAction::Settings, false, cx)
                            .px(scaled_px(5.))
                            .border_color(transparent_black())
                            .bg(transparent_black())
                            .tooltip(|window, cx| Tooltip::new("Settings").build(window, cx))
                            .child(
                                svg()
                                    .path("everything/settings.svg")
                                    .size(scaled_px(14.))
                                    .text_color(p.muted),
                            ),
                    ),
            )
    }
}

impl Render for EverythingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let zoom = self.prefs.zoom;
        window.set_rem_size(px(design::BASE_REM_SIZE * zoom));
        #[cfg(target_os = "macos")]
        if self.table_width == 0. {
            mac_menu::position_buttons(window);
        }
        self.sync_file_tree(cx);
        let tree_width = self.file_tree_width(window);
        let window_width = f32::from(window.viewport_size().width);
        let width = window_width - tree_width;
        if self.prefs.columns == [0.; 4] {
            self.prefs.columns = design::default_columns(window_width / zoom);
        }
        if width != self.table_width {
            // Refit on every width change: a sub-pixel (or <=2px) stale table
            // left the content wider than the viewport mid-resize, flashing the
            // horizontal scrollbar. fit_columns keeps the user's proportions and
            // never writes back to preferences.
            self.table_columns =
                design::fit_columns(self.prefs.columns, width / zoom).map(|value| value * zoom);
            self.table_width = width;
        }
        let widths = self.table_columns;
        let total_width = widths.iter().sum::<f32>();
        let p = Palette::get(cx);
        let row_count = self.model.rows.len();
        let header_height = (if cfg!(target_os = "windows") {
            25.
        } else {
            33.
        }) * zoom;
        let header = div()
            .flex()
            .h(px(header_height))
            .w(px(total_width))
            .flex_shrink_0()
            .border_b_1()
            .border_color(p.border)
            .children(
                ["Name", "Path", "Size", "Modified"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        let active = self.sort_by == ["name", "dir", "size", "mtime"][index];
                        div()
                            .relative()
                            .flex()
                            .items_center()
                            .w(px(widths[index]))
                            .h_full()
                            .flex_shrink_0()
                            .child(
                                div()
                                    .id(("sort", index))
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .gap(scaled_px(4.))
                                    .px(scaled_px(8.))
                                    .when(index == 0, |d| d.pl(scaled_px(16.)))
                                    .when(index == 2, |d| d.justify_end())
                                    .font_family(SANS)
                                    .text_size(scaled_px(11.))
                                    .line_height(scaled_px(11.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.muted)
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.sort(index, window, cx)
                                    }))
                                    .child(label)
                                    .when(active, |d| {
                                        d.child(
                                            svg()
                                                .path(if self.sort_dir == "asc" {
                                                    "everything/sort-asc.svg"
                                                } else {
                                                    "everything/sort-desc.svg"
                                                })
                                                .size(scaled_px(10.))
                                                .text_color(p.accent),
                                        )
                                    }),
                            )
                            .when(index < 2, |d| {
                                d.child(
                                    div()
                                        .id(("resize", index))
                                        .absolute()
                                        .right(scaled_px(-4.))
                                        .top_0()
                                        .h_full()
                                        .w(scaled_px(8.))
                                        .cursor_col_resize()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this, event: &MouseDownEvent, _, cx| {
                                                    this.resizing = Some((
                                                        index,
                                                        event.position,
                                                        this.table_columns,
                                                    ));
                                                    cx.stop_propagation();
                                                },
                                            ),
                                        ),
                                )
                            })
                    }),
            );
        let results = div()
            .id("results-area")
            .when(cfg!(target_os = "windows"), |area| {
                area.track_focus(&self.table_focus)
            })
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .child(
                div()
                    .id("horizontal-results")
                    .size_full()
                    .overflow_x_scroll()
                    .track_scroll(&self.horizontal_scroll)
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(div().relative().flex_1().min_h_0().w(px(total_width)).when(
                        row_count > 0,
                        |area| {
                            area.child(
                                uniform_list(
                                    "search-results",
                                    row_count,
                                    cx.processor(
                                        move |this, range: std::ops::Range<usize>, window, cx| {
                                            this.visible_rows = range.clone();
                                            if this.model.loaded_count > 0
                                                && range.end + 30 >= this.model.rows.len()
                                            {
                                                this.search_page(
                                                    this.model.loaded_count,
                                                    true,
                                                    true,
                                                    window,
                                                    cx,
                                                );
                                            }
                                            range
                                                .map(|index| this.row(index, widths, cx))
                                                .collect::<Vec<_>>()
                                        },
                                    ),
                                )
                                .track_scroll(self.list_scroll.clone())
                                .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                                    if this.cancel_pending_rename() {
                                        cx.notify();
                                    }
                                }))
                                .size_full(),
                            )
                        },
                    )),
            )
            .when(row_count > 0, |area| {
                area.child(div().absolute().inset_0().top(px(header_height)).child(
                    Scrollbar::vertical(&self.list_scroll).scrollbar_show(ScrollbarShow::Always),
                ))
            })
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::horizontal(&self.horizontal_scroll)
                        .scrollbar_show(ScrollbarShow::Always),
                ),
            );
        self.write_test_state(window, cx);
        div()
            .id("everything")
            .key_context("Everything")
            // A focus-tracked element prevents the default mouse-down action.
            // On Windows only the results track table focus, so caption clicks
            // can reach DefWindowProc's native move/maximize handling.
            .when(!cfg!(target_os = "windows"), |root| {
                root.track_focus(&self.table_focus)
            })
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(p.bg)
            .text_color(p.text)
            .font_family(SANS)
            .line_height(relative(1.2))
            .capture_any_mouse_down(cx.listener(|this, _, _, cx| {
                if this.cancel_pending_rename() {
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                if this.cancel_pending_rename() {
                    cx.notify();
                }
            }))
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| {
                    this.focus_search(true, window, cx)
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if this.resize_file_tree(event, window, cx) {
                    return;
                }
                if let Some((index, start, widths)) = this.resizing {
                    let delta = f32::from(event.position.x - start.x).round();
                    if index == 0 {
                        this.table_columns[0] = (widths[0] + delta)
                            .clamp(COLUMN_MIN[0] * this.prefs.zoom,
                                widths[0] + widths[1] - COLUMN_MIN[1] * this.prefs.zoom);
                        this.table_columns[1] = widths[0] + widths[1] - this.table_columns[0];
                    } else {
                        this.table_columns[index] = (widths[index] + delta)
                            .max(COLUMN_MIN[index] * this.prefs.zoom);
                    }
                    cx.notify();
                } else if event.dragging() && !this.tree_drag_move(event.position, window, cx) {
                    if let Some((start, index)) = this.drag_start {
                        if f32::from(event.position.x - start.x).abs()
                            + f32::from(event.position.y - start.y).abs()
                            > 5.
                        {
                            this.drag_start = None;
                            this.cancel_pending_rename();
                            if !this.model.selected.contains(&index) {
                                this.model.select(index, false, false);
                            }
                            this.native_drag(window, cx);
                        }
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.finish_file_tree_resize(cx);
                    if this.resizing.take().is_some() {
                        this.prefs.columns = this.table_columns.map(|width| width / this.prefs.zoom);
                        this.save_preferences(cx);
                    }
                    this.drag_start = None;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.finish_file_tree_resize(cx);
                    if this.resizing.take().is_some() {
                        this.prefs.columns = this.table_columns.map(|width| width / this.prefs.zoom);
                        this.save_preferences(cx);
                    }
                    this.drag_start = None;
                }),
            )
            .child(self.titlebar(window, cx))
            .when(
                !self.fda_ok
                    && !self.prefs.fda_dismissed
                    && !matches!(self.modal, Some(Modal::Onboarding)),
                |app| app.child(self.notice_banner(if self.status_display.fda_settings_opened {
                    "전체 디스크 접근을 허용하면 자동으로 반영됩니다 (인덱서가 1분 내 감지)."
                } else {
                    "전체 디스크 접근을 허용하면 보호된 폴더까지 검색할 수 있습니다."
                }, cx).child(div().flex().gap(scaled_px(6.)).flex_shrink_0()
                    .child(self.notice_button("fda-settings", "설정 열기", UiAction::FdaSettings, false, false, cx))
                    .child(self.notice_button("fda-dismiss", "닫기", UiAction::DismissFda, true, false, cx)))),
            )
            .when(self.status_display.pathignore_changed, |app| app.child(
                self.notice_banner("설정 파일(.pathignore)이 변경되었습니다. 재시작 후 적용됩니다.", cx)
                    .child(div().flex().gap(scaled_px(6.)).flex_shrink_0()
                        .child(self.notice_button("pathignore-restart", "재시작", UiAction::Restart, false, false, cx))
                        .child(self.notice_button("pathignore-dismiss", "닫기", UiAction::DismissIgnore, true, false, cx)))))
            .when(self.status_display.pathindexing_changed, |app| app.child(
                self.notice_banner("인덱싱 경로(.pathindexing)가 변경되어 반영되었습니다.", cx)
                    .child(div().flex().gap(scaled_px(6.)).flex_shrink_0()
                        .child(self.notice_button("pathindexing-dismiss", "닫기", UiAction::DismissIndexing, true, false, cx)))))
            .when_some(self.health.clone(), |app, health| {
                app.child(self.health_banner(&health, cx))
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .h(scaled_px(44.))
                                    .flex_shrink_0()
                                    .px(scaled_px(if cfg!(target_os = "windows") { 8. } else { 16. }))
                                    .child(
                                        Input::new(&self.input)
                                            .appearance(false)
                                            .min_h(scaled_px(if cfg!(target_os = "windows") {
                                                30.
                                            } else {
                                                34.
                                            }))
                                            .max_h(scaled_px(if cfg!(target_os = "windows") {
                                                30.
                                            } else {
                                                34.
                                            }))
                                            .w_full()
                                            .px(scaled_px(10.))
                                            .py_0()
                                            .rounded(scaled_px(6.))
                                            .font_family(SANS)
                                            .text_size(scaled_px(if cfg!(target_os = "windows") {
                                                13.
                                            } else {
                                                12.
                                            }))
                                            .line_height(scaled_px(18.))
                                            .bg(p.input)
                                            .border_1()
                                            // Orca's filter field: focus-within:border-ring ring-1 ring-ring.
                                            .border_color(if self.input.focus_handle(cx).is_focused(window) {
                                                p.ring
                                            } else {
                                                p.input_border
                                            })
                                            .when(self.input.focus_handle(cx).is_focused(window), |i| {
                                                i.shadow(vec![BoxShadow {
                                                    color: p.ring,
                                                    offset: point(px(0.), px(0.)),
                                                    blur_radius: px(0.),
                                                    spread_radius: px(1. * self.prefs.zoom),
                                                }])
                                            }),
                                    ),
                            )
                            .child(results),
                    )
                    .when(self.prefs.file_tree_visible, |body| {
                        body.child(self.file_tree_panel(tree_width, cx))
                    }),
            )
            .child(self.statusbar(cx))
            .when_some(
                self.toast.as_ref().map(|(text, _)| text.clone()),
                |app, text| {
                    app.child(
                        div()
                            .absolute()
                            .right(scaled_px(14.))
                            .bottom(scaled_px(18.))
                            .px(scaled_px(12.))
                            .py(scaled_px(9.))
                            .rounded(scaled_px(8.))
                            .border_1()
                            .border_color(p.border)
                            .shadow_lg()
                            .bg(p.toast)
                            .text_color(p.toast_text)
                            .text_size(scaled_px(12.))
                            .child(text),
                    )
                },
            )
            .child(self.context_menu(cx))
            .child(self.modal(cx))
            .when(self.zoom_popover.visible, |app| {
                app.child(self.zoom_popover(window, cx))
            })
    }
}

fn toggle_maximized(window: &Window) {
    // GPUI 0.2.2's Windows zoom() always maximizes; it cannot restore a window.
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::{
            Foundation::HWND,
            UI::WindowsAndMessaging::{ShowWindowAsync, SW_MAXIMIZE, SW_RESTORE},
        };
        if let Ok(handle) = HasWindowHandle::window_handle(window) {
            if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                let command = if window.is_maximized() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                unsafe {
                    let _ = ShowWindowAsync(HWND(handle.hwnd.get() as *mut _), command);
                }
                return;
            }
        }
    }
    window.zoom_window();
}

fn window_control(id: &'static str, path: &'static str, close: bool, color: Hsla) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(46.))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .hover(move |d| {
            d.bg(if close {
                rgba(0xe81123ff)
            } else {
                rgba(0x88888820)
            })
        })
        .child(svg().path(path).size(px(12.)).text_color(color))
}

impl EverythingView {
    fn notice_banner(&self, text: &'static str, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap(scaled_px(8.))
            .px(scaled_px(12.))
            .py(scaled_px(7.))
            .flex_shrink_0()
            .min_w_0()
            // shadcn Alert: neutral surface with a hairline, not a solid amber bar.
            .bg(Palette::get(cx).hover)
            .border_b_1()
            .border_color(Palette::get(cx).border)
            .text_size(scaled_px(11.5))
            .line_height(relative(1.4))
            .text_color(Palette::get(cx).text)
            .child(div().flex_1().min_w_0().child(text))
    }

    fn notice_button(
        &self,
        id: &'static str,
        text: &'static str,
        action: UiAction,
        dismiss: bool,
        disabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        // shadcn outline button; dismiss is the ghost variant.
        let p = Palette::get(cx);
        self.button(id, text, action, disabled, cx)
            .px(scaled_px(10.))
            .font_weight(FontWeight::MEDIUM)
            .when(dismiss, |button| {
                button
                    .border_color(transparent_black())
                    .bg(transparent_black())
                    .text_color(p.muted)
            })
    }

    fn health_banner(&self, health: &str, cx: &Context<Self>) -> Div {
        let text = match health {
            "not_installed" => {
                "인덱스 서비스가 설치되어 있지 않습니다. 등록하려면 관리자 권한이 필요합니다."
            }
            "stopped" | "starting" => "인덱스 서비스를 시작하는 중입니다...",
            "unresponsive" => "인덱스 서비스가 응답하지 않습니다. 재등록이 필요할 수 있습니다.",
            "version_mismatch" => {
                "인덱스 서비스가 이전 버전으로 실행 중입니다. 재시작(재등록)이 필요합니다."
            }
            "not_registered" => "백그라운드 인덱서가 로그인 항목으로 등록되어 있지 않습니다.",
            "approval_required" => {
                "로그인 항목에서 Everything이 꺼져 있어 백그라운드 인덱서가 실행되지 않습니다."
            }
            "disabled" => "자동 실행이 꺼져 있습니다. 앱을 사용하는 동안에만 인덱스가 갱신됩니다.",
            _ => "인덱스 서비스 상태를 확인하는 중입니다...",
        };
        self.notice_banner(text, cx).child(
            div()
                .flex()
                .gap(scaled_px(6.))
                .flex_shrink_0()
                .when(health == "not_registered", |d| {
                    d.child(self.notice_button(
                        "daemon-register",
                        "등록",
                        UiAction::Register,
                        false,
                        false,
                        cx,
                    ))
                })
                .when(health == "approval_required", |d| {
                    d.child(self.notice_button(
                        "login-items",
                        "로그인 항목 열기",
                        UiAction::LoginSettings,
                        false,
                        false,
                        cx,
                    ))
                })
                .when(health == "disabled", |d| {
                    d.child(self.notice_button(
                        "autostart-enable",
                        "다시 켜기",
                        UiAction::Autostart,
                        false,
                        self.action_busy,
                        cx,
                    ))
                })
                .when(
                    cfg!(target_os = "windows")
                        && ["not_installed", "unresponsive", "version_mismatch"].contains(&health),
                    |d| {
                        d.child(self.notice_button(
                            "service-register",
                            "서비스 등록",
                            UiAction::Register,
                            false,
                            false,
                            cx,
                        ))
                    },
                ),
        )
    }

    pub(super) fn write_test_state(&self, window: &Window, cx: &App) {
        if let Some(path) = std::env::var_os("EVERYTHING_UI_STATE") {
            let zoom_button = self.zoom_button_bounds(window);
            let zoom_popover = self.zoom_popover_bounds(window);
            let zoom_state = serde_json::json!({
                "visible":self.zoom_popover.visible,"pinned":self.zoom_popover.pinned,
                "buttonVisible":self.zoom_button_visible(),
                "hovered":self.zoom_popover.hovered,"keyboardSelection":self.zoom_popover.keyboard_selection,
                "buttonBounds":[f32::from(zoom_button.origin.x),f32::from(zoom_button.origin.y),f32::from(zoom_button.size.width),f32::from(zoom_button.size.height)],
                "bounds":[f32::from(zoom_popover.origin.x),f32::from(zoom_popover.origin.y),f32::from(zoom_popover.size.width),f32::from(zoom_popover.size.height)],
            });
            let state = serde_json::json!({
                "query":self.model.query,"rows":self.model.rows.iter().take(35).map(|e|serde_json::json!({"name":e.name,"path":e.path,"dir":e.dir,"size":e.size,"isDir":e.is_dir})).collect::<Vec<_>>(),
                "loaded":self.model.rows.len(),"total":self.model.total,"totalKnown":self.model.total_known,"busy":self.model.busy,"paging":self.model.paging,
                "selected":self.model.paths(),"primary":self.model.primary,"sortBy":self.sort_by,"sortDir":self.sort_dir,
                "renamePath":self.rename_path,"renameValue":self.rename_input.read(cx).value(),
                "renamePending":self.rename_click_task.is_some(),"renameDelayMs":Self::rename_delay().as_millis() as u64,
                "modal":self.modal.as_ref().map(|m|match m{Modal::Trash(_)=>"trash",Modal::Onboarding=>"onboarding",Modal::Settings=>"settings"}),
                "modalSelection":self.modal_selection,"contextMenu":self.context_menu.is_some(),"menuSelection":self.menu_selection,
                "fdaOk":self.fda_ok,"fdaDismissed":self.prefs.fda_dismissed,"fdaSettingsOpened":self.status_display.fda_settings_opened,"pathignoreBanner":self.status_display.pathignore_changed,"pathindexingBanner":self.status_display.pathindexing_changed,"tableFocused":self.table_focus.is_focused(window),
                "horizontalOffset":f32::from(self.horizontal_scroll.offset().x),"visibleRange":[self.visible_rows.start,self.visible_rows.end],
                "fileTree":self.file_tree_test_state(window),
                "toast":self.toast.as_ref().map(|(s,_)|s),"theme":if cx.theme().is_dark(){"dark"}else{"light"},"columns":self.table_columns,"error":self.model.error,"actionBusy":self.action_busy,"completedAction":self.completed_action.map(|action|format!("{action:?}")),
                "width":f32::from(window.viewport_size().width),"height":f32::from(window.viewport_size().height),
                "zoom":self.prefs.zoom,"rowHeight":self.row_height(),
                "resultsTop":f32::from(self.list_scroll.0.borrow().base_handle.bounds().origin.y),
                "zoomPopover":zoom_state,
            });
            let _ = std::fs::write(path, state.to_string());
        }
    }
}

pub(super) fn display_path(path: &str, home: &std::path::Path) -> String {
    let home = home.to_string_lossy();
    if path == home.as_ref() {
        "~".into()
    } else if path.starts_with(&format!("{home}/")) {
        format!("~{}", &path[home.len()..])
    } else {
        path.into()
    }
}
fn format_size(entry: &EntryDto) -> String {
    if entry.is_dir {
        String::new()
    } else {
        entry.size.map(format_bytes).unwrap_or_default()
    }
}
fn format_bytes(size: i64) -> String {
    let size = size.max(0) as f64;
    if size < 1024. {
        format!("{size:.0} B")
    } else if size < 1048576. {
        format!("{:.1} KB", size / 1024.)
    } else if size < 1073741824. {
        format!("{:.1} MB", size / 1048576.)
    } else {
        format!("{:.1} GB", size / 1073741824.)
    }
}
fn format_modified(timestamp: Option<i64>) -> String {
    timestamp
        .filter(|t| *t != 0)
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
pub(super) fn is_package(entry: &EntryDto) -> bool {
    #[cfg(target_os = "macos")]
    {
        entry.is_dir && crate::has_package_extension(&entry.path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = entry;
        false
    }
}
fn highlight(name: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let leaf = query
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(query)
        .replace(['*', '?'], "");
    let lower = name.to_lowercase();
    let mut marks = vec![false; name.len()];
    for term in leaf.split_whitespace() {
        let term = term.to_lowercase();
        for (start, _) in lower.match_indices(&term) {
            let end = start + term.len();
            if name.get(start..end).is_some() {
                marks[start..end].fill(true);
            }
        }
    }
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < marks.len() {
        if !marks[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < marks.len() && marks[i] {
            i += 1;
        }
        ranges.push(start..i);
    }
    ranges
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
