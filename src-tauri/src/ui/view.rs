use super::*;
use design::{Palette, COLUMN_MIN, MONO, ROW_HEIGHT, SANS};
use gpui_component::scroll::Scrollbar;

impl EverythingView {
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
            .h(px(22.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(3.))
            .font_family(SANS)
            .text_size(px(11.))
            .line_height(px(14.))
            .border_1()
            .border_color(p.border)
            .rounded(px(5.))
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
        let editing = self.rename_path.as_deref() == Some(&entry.path);
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
            .min(widths[0] + widths[1] - 38.);
        let icon = if let Some(icon) = self.icons.get(&icon_key(entry)).cloned() {
            img(icon).size(px(16.)).flex_shrink_0().into_any_element()
        } else {
            svg()
                .path(if entry.is_dir {
                    "everything/folder.svg"
                } else {
                    "everything/file.svg"
                })
                .size(px(16.))
                .flex_shrink_0()
                .into_any_element()
        };
        div()
            .id(("result", index))
            .relative()
            .flex()
            .items_center()
            .h(px(ROW_HEIGHT))
            .w(px(widths.iter().sum()))
            .border_b_1()
            .border_color(p.row_border)
            .text_size(px(12.))
            .line_height(px(15.))
            .text_color(p.text)
            .when(selected, |r| r.bg(p.selected))
            .hover(|r| r.bg(if selected { p.selected } else { p.hover }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if this.modal.is_some()
                        || this.rename_path.as_deref() == Some(&this.model.rows[index].path)
                    {
                        return;
                    }
                    this.context_menu = None;
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
                    this.table_focus.focus(window);
                    this.open_context_menu(event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .w(px(widths[0]))
                    .h_full()
                    .flex_shrink_0()
                    .pl(px(16.))
                    .pr(px(8.))
                    .overflow_hidden()
                    .child(icon)
                    .child(div().flex_1().min_w_0().child(name)),
            )
            .child(
                div()
                    .w(px(widths[1]))
                    .flex_shrink_0()
                    .pl(px(8.))
                    .pr(px(4.))
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(p.muted)
                    .truncate()
                    .child(display_path(&entry.dir, &self.backend.home_dir)),
            )
            .child(
                div()
                    .w(px(widths[2]))
                    .flex_shrink_0()
                    .text_right()
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(p.muted)
                    .truncate()
                    .child(format_size(entry)),
            )
            .child(
                div()
                    .w(px(widths[3]))
                    .flex_shrink_0()
                    .pl(px(8.))
                    .pr(px(16.))
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(p.muted)
                    .truncate()
                    .child(format_modified(entry.mtime)),
            )
            .when(editing, |r| {
                r.child(
                    div()
                        .absolute()
                        .left(px(29.))
                        .top_0()
                        .w(px(rename_width))
                        .h(px(26.))
                        .child(
                            Input::new(&self.rename_input)
                                .appearance(false)
                                .w_full()
                                .min_h(px(26.))
                                .max_h(px(26.))
                                .px(px(4.))
                                .py_0()
                                .font_family(SANS)
                                .text_size(px(12.))
                                .line_height(px(18.))
                                .bg(p.input)
                                .border(px(1.5))
                                .border_color(p.accent)
                                .rounded(px(4.))
                                .shadow(vec![BoxShadow {
                                    color: p.accent.opacity(0.13),
                                    offset: point(px(0.), px(0.)),
                                    blur_radius: px(0.),
                                    spread_radius: px(3.),
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
                        .w(px(3.))
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
            .w(px(222.))
            .rounded(px(10.))
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
                        .px(px(10.))
                        .py(px(8.))
                        .text_size(px(12.))
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
            .px(px(12.))
            .py(px(6.))
            .border_1()
            .border_color(p.border)
            .rounded(px(6.))
            .bg(p.button)
            .text_color(p.button_text)
            .text_size(px(11.))
            .line_height(px(14.))
            .when(self.modal_selection == index, |b| {
                b.shadow(vec![BoxShadow {
                    color: p.accent.opacity(0.5),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(3.),
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

    fn modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(modal) = &self.modal else {
            return div().into_any_element();
        };
        let p = Palette::get(cx);
        let onboarding = matches!(modal, Modal::Onboarding);
        let body = if let Modal::Trash(paths) = modal {
            let title = if paths.len() == 1 {
                "Move selected item to Trash?".into()
            } else {
                format!("Move {} items to Trash?", paths.len())
            };
            div()
                .p(px(24.))
                .max_w(px(410.))
                .rounded(px(12.))
                .bg(p.menu)
                .border_1()
                .border_color(p.border)
                .shadow_xl()
                .child(
                    div()
                        .mb(px(24.))
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
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
            div().id("onboarding-body").w(px(482.)).max_w_full().max_h_full().overflow_y_scroll()
                .px(px(20.)).pt(px(18.)).pb(px(16.)).rounded(px(12.)).border_1().border_color(if cx.theme().is_dark(){rgb(0x3a3f46).into()}else{p.border})
                .bg(if cx.theme().is_dark(){rgb(0x26292e).into()}else{p.bg}).text_size(px(12.5)).line_height(relative(1.65)).shadow_xl()
                .child(div().mb(px(8.)).text_size(px(15.)).font_weight(FontWeight::BOLD).child("Everything 시작하기"))
                .child(div().mb(px(10.)).child(StyledText::new(intro).with_highlights([(emphasis..emphasis + "전체 디스크 접근".len(), HighlightStyle {font_weight: Some(FontWeight::BOLD), ..Default::default()})])))
                .child(div().flex().items_center().gap(px(8.)).px(px(10.)).py(px(8.)).mb(px(10.)).rounded(px(8.)).bg(rgba(if cx.theme().is_dark(){0xe8a02038}else{0xe8a0202e}))
                    .child(div().flex_1().font_weight(if self.fda_ok{FontWeight::SEMIBOLD}else{FontWeight::MEDIUM}).when(self.fda_ok,|d|d.text_color(rgb(0x1f9d57))).child(if self.fda_ok{"✓ 전체 디스크 접근 허용됨"}else{"전체 디스크 접근이 아직 허용되지 않았습니다"}))
                    .when(!self.fda_ok,|d|d
                        .child(self.dialog_button("onboarding-settings","설정 열기",UiAction::FdaSettings,0,false,cx).text_size(px(11.5)).font_weight(FontWeight::MEDIUM).px(px(10.)).py(px(3.)).rounded(px(5.)).border_0().bg(rgb(0xe8a020)).text_color(white()))
                        .child(self.dialog_button("onboarding-restart","앱 재시작",UiAction::Restart,1,false,cx).text_size(px(11.5)).font_weight(FontWeight::MEDIUM).px(px(10.)).py(px(3.)).rounded(px(5.)).border_0().bg(rgb(0xe8a020)).text_color(white()))))
                .child(div().mb(px(10.)).text_size(px(11.5)).opacity(0.75).child("시스템 설정 > 개인정보 보호 및 보안 > 전체 디스크 접근에서 Everything을 켜 주세요. 목록에 없으면 + 버튼으로 응용 프로그램에서 추가할 수 있습니다. 허용하면 이 창이 자동으로 감지합니다 — 잠시 후에도 감지되지 않으면 (시스템이 재실행을 요구하는 경우) '앱 재시작'을 눌러 주세요."))
                .child(div().flex().justify_end().gap(px(8.)).mt(px(4.))
                    .child(self.dialog_button("onboarding-start","시작하기",UiAction::OnboardGranted,3,self.action_busy||!self.fda_ok,cx).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).px(px(14.)).py(px(5.)).border_color(transparent_black()).bg(rgb(0x2f6ede)).text_color(white()))
                    .child(self.dialog_button("onboarding-skip","나중에 (제한된 인덱스로 시작)",UiAction::OnboardSkip,2,self.action_busy,cx).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).px(px(14.)).py(px(5.)).bg(transparent_black()).text_color(p.text)))
                .into_any_element()
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.))
            .bg(rgba(if onboarding { 0x00000073 } else { 0x00000040 }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(body)
            .into_any_element()
    }

    fn titlebar(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let p = Palette::get(cx);
        div()
            .id("titlebar")
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .h(px(30.))
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
                    .items_center()
                    .gap(px(8.))
                    .child(
                        svg()
                            .path("everything/search.svg")
                            .size(px(12.))
                            .text_color(p.accent),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Everything"),
                    ),
            )
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
        let dot = div().size(px(6.)).rounded_full().bg(rgb(color));
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
        let selection = if selected.len() == 1 {
            display_path(&selected[0], &self.backend.home_dir)
        } else {
            let total = self
                .model
                .selected
                .iter()
                .filter_map(|i| self.model.rows.get(*i))
                .filter_map(|e| e.size)
                .sum::<i64>();
            format!("{} selected, Size: {}", selected.len(), format_bytes(total))
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(33.))
            .px(px(16.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(p.border)
            .font_family(MONO)
            .text_size(px(11.))
            .line_height(px(14.))
            .text_color(p.muted)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .min_w_0()
                    .overflow_hidden()
                    .when(!selected.is_empty(), |d| {
                        d.child(div().truncate().child(selection))
                    })
                    .when(selected.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
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
                    .gap(px(8.))
                    .flex_shrink_0()
                    .child(self.button("add", "Add", UiAction::Add, false, cx))
                    .child(self.button("ignore", "Ignore", UiAction::Ignore, false, cx))
                    .child(
                        self.button(
                            "reset",
                            "Reset",
                            UiAction::Reset,
                            self.status.state == "Indexing",
                            cx,
                        )
                        .text_size(px(10.))
                        .font_weight(FontWeight::MEDIUM)
                        .rounded(px(4.))
                        .px(px(10.))
                        .gap(px(6.))
                        .children([svg()
                            .path("everything/reset.svg")
                            .size(px(11.))
                            .text_color(p.accent)
                            .into_any_element()])
                        .flex_row_reverse(),
                    ),
            )
    }
}

impl Render for EverythingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "macos")]
        if self.table_width == 0. {
            mac_menu::position_buttons(window);
        }
        let width = f32::from(window.viewport_size().width);
        if (width - self.table_width).abs() > 1. {
            self.prefs.columns = if self.table_width == 0. && self.prefs.columns == [0.; 4] {
                design::default_columns(width)
            } else if self.table_width == 0.
                && (self.prefs.columns.iter().sum::<f32>() - width).abs() <= 2.
            {
                self.prefs.columns
            } else {
                design::fit_columns(self.prefs.columns, width)
            };
            self.table_width = width;
        }
        let widths = self.prefs.columns;
        let total_width = widths.iter().sum::<f32>();
        let p = Palette::get(cx);
        let row_count = self.model.rows.len();
        let header = div()
            .flex()
            .h(px(if cfg!(target_os = "windows") {
                25.
            } else {
                33.
            }))
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
                                    .gap(px(4.))
                                    .px(px(8.))
                                    .when(index == 0, |d| d.pl(px(16.)))
                                    .when(index == 2, |d| d.justify_end())
                                    .font_family(SANS)
                                    .text_size(px(11.))
                                    .line_height(px(11.))
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
                                                .size(px(10.))
                                                .text_color(p.accent),
                                        )
                                    }),
                            )
                            .when(index < 2, |d| {
                                d.child(
                                    div()
                                        .id(("resize", index))
                                        .absolute()
                                        .right(px(-4.))
                                        .top_0()
                                        .h_full()
                                        .w(px(8.))
                                        .cursor_col_resize()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this, event: &MouseDownEvent, _, cx| {
                                                    this.resizing = Some((
                                                        index,
                                                        event.position,
                                                        this.prefs.columns,
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
                                            let changed = this.visible_rows != range;
                                            this.visible_rows = range.clone();
                                            if changed {
                                                this.queue_icons(window, cx);
                                            }
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
                area.child(Scrollbar::vertical(&self.list_scroll))
            })
            .child(Scrollbar::horizontal(&self.horizontal_scroll));
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
                if let Some((index, start, widths)) = this.resizing {
                    let delta = f32::from(event.position.x - start.x).round();
                    if index == 0 {
                        this.prefs.columns[0] = (widths[0] + delta)
                            .clamp(COLUMN_MIN[0], widths[0] + widths[1] - COLUMN_MIN[1]);
                        this.prefs.columns[1] = widths[0] + widths[1] - this.prefs.columns[0];
                    } else {
                        this.prefs.columns[index] = (widths[index] + delta).max(COLUMN_MIN[index]);
                    }
                    cx.notify();
                } else if event.dragging() {
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
                    if this.resizing.take().is_some() {
                        this.save_preferences(cx);
                    }
                    this.drag_start = None;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resizing.take().is_some() {
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
                }, cx).child(div().flex().gap(px(6.)).flex_shrink_0()
                    .child(self.notice_button("fda-settings", "설정 열기", UiAction::FdaSettings, false, false, cx))
                    .child(self.notice_button("fda-dismiss", "닫기", UiAction::DismissFda, true, false, cx)))),
            )
            .when(self.status_display.pathignore_changed, |app| app.child(
                self.notice_banner("설정 파일(.pathignore)이 변경되었습니다. 재시작 후 적용됩니다.", cx)
                    .child(div().flex().gap(px(6.)).flex_shrink_0()
                        .child(self.notice_button("pathignore-restart", "재시작", UiAction::Restart, false, false, cx))
                        .child(self.notice_button("pathignore-dismiss", "닫기", UiAction::DismissIgnore, true, false, cx)))))
            .when(self.status_display.pathindexing_changed, |app| app.child(
                self.notice_banner("인덱싱 경로(.pathindexing)가 변경되어 반영되었습니다.", cx)
                    .child(div().flex().gap(px(6.)).flex_shrink_0()
                        .child(self.notice_button("pathindexing-dismiss", "닫기", UiAction::DismissIndexing, true, false, cx)))))
            .when_some(self.health.clone(), |app, health| {
                app.child(self.health_banner(&health, cx))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(44.))
                    .flex_shrink_0()
                    .px(px(if cfg!(target_os = "windows") { 8. } else { 16. }))
                    .child(
                        Input::new(&self.input)
                            .appearance(false)
                            .min_h(px(if cfg!(target_os = "windows") {
                                30.
                            } else {
                                34.
                            }))
                            .max_h(px(if cfg!(target_os = "windows") {
                                30.
                            } else {
                                34.
                            }))
                            .w_full()
                            .px(px(10.))
                            .py_0()
                            .rounded(px(8.))
                            .font_family(MONO)
                            .text_size(px(if cfg!(target_os = "windows") {
                                13.
                            } else {
                                12.
                            }))
                            .line_height(px(18.))
                            .bg(p.input)
                            .border_1()
                            .border_color(if self.input.focus_handle(cx).is_focused(window) {
                                p.accent
                            } else {
                                p.input_border
                            })
                            .when(self.input.focus_handle(cx).is_focused(window), |i| {
                                i.shadow(vec![BoxShadow {
                                    color: rgba(0x7da9ff40).into(),
                                    offset: point(px(0.), px(0.)),
                                    blur_radius: px(0.),
                                    spread_radius: px(2.),
                                }])
                            }),
                    ),
            )
            .child(results)
            .child(self.statusbar(cx))
            .when_some(
                self.toast.as_ref().map(|(text, _)| text.clone()),
                |app, text| {
                    app.child(
                        div()
                            .absolute()
                            .right(px(14.))
                            .bottom(px(18.))
                            .px(px(12.))
                            .py(px(9.))
                            .rounded(px(10.))
                            .shadow_lg()
                            .bg(p.toast)
                            .text_color(p.toast_text)
                            .text_size(px(12.))
                            .child(text),
                    )
                },
            )
            .child(self.context_menu(cx))
            .child(self.modal(cx))
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
            .gap(px(8.))
            .px(px(12.))
            .py(px(7.))
            .flex_shrink_0()
            .min_w_0()
            .bg(if cx.theme().is_dark() {
                rgb(0x7a4800)
            } else {
                rgb(0xe8a020)
            })
            .text_size(px(11.5))
            .line_height(relative(1.4))
            .text_color(white())
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
        self.button(id, text, action, disabled, cx)
            .h(px(20.))
            .px(px(10.))
            .border_0()
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .bg(if dismiss {
                transparent_black()
            } else {
                white()
            })
            .text_color(if dismiss {
                rgba(0xffffffd9)
            } else {
                rgba(0xa05000ff)
            })
            .hover(move |button| {
                button
                    .bg(if dismiss {
                        transparent_black()
                    } else {
                        white()
                    })
                    .text_color(if dismiss {
                        white()
                    } else {
                        rgba(0xa05000ff).into()
                    })
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
                .gap(px(6.))
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
            let state = serde_json::json!({
                "query":self.model.query,"rows":self.model.rows.iter().take(35).map(|e|serde_json::json!({"name":e.name,"path":e.path,"dir":e.dir,"size":e.size,"isDir":e.is_dir})).collect::<Vec<_>>(),
                "loaded":self.model.rows.len(),"total":self.model.total,"totalKnown":self.model.total_known,"busy":self.model.busy,"paging":self.model.paging,
                "selected":self.model.paths(),"primary":self.model.primary,"sortBy":self.sort_by,"sortDir":self.sort_dir,
                "renamePath":self.rename_path,"renameValue":self.rename_input.read(cx).value(),
                "renamePending":self.rename_click_task.is_some(),"renameDelayMs":Self::rename_delay().as_millis() as u64,
                "modal":self.modal.as_ref().map(|m|match m{Modal::Trash(_)=>"trash",Modal::Onboarding=>"onboarding"}),
                "modalSelection":self.modal_selection,"contextMenu":self.context_menu.is_some(),"menuSelection":self.menu_selection,
                "fdaOk":self.fda_ok,"fdaDismissed":self.prefs.fda_dismissed,"fdaSettingsOpened":self.status_display.fda_settings_opened,"pathignoreBanner":self.status_display.pathignore_changed,"pathindexingBanner":self.status_display.pathindexing_changed,"tableFocused":self.table_focus.is_focused(window),
                "horizontalOffset":f32::from(self.horizontal_scroll.offset().x),"visibleRange":[self.visible_rows.start,self.visible_rows.end],
                "toast":self.toast.as_ref().map(|(s,_)|s),"theme":if cx.theme().is_dark(){"dark"}else{"light"},"columns":self.prefs.columns,"error":self.model.error,"actionBusy":self.action_busy,"completedAction":self.completed_action.map(|action|format!("{action:?}")),
                "width":f32::from(window.viewport_size().width),"height":f32::from(window.viewport_size().height),
            });
            let _ = std::fs::write(path, state.to_string());
        }
    }
}

fn display_path(path: &str, home: &std::path::Path) -> String {
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
