use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum UiAction {
    Open,
    OpenWith,
    Reveal,
    CopyPaths,
    CopyFiles,
    Trash,
    TrashImmediately,
    DeletePermanently,
    Rename,
    QuickLook,
    Package,
    Add,
    Ignore,
    Reset,
    Settings,
    FdaSettings,
    LoginSettings,
    Register,
    Autostart,
    Restart,
    DismissFda,
    DismissIgnore,
    DismissIndexing,
    Cancel,
    Confirm,
    OnboardGranted,
    OnboardSkip,
}

enum Outcome {
    Done,
    Message(String),
    Trash(crate::file_actions::TrashResult),
    Refresh,
}

impl EverythingView {
    pub(super) fn action(&mut self, action: UiAction, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_pending_rename();
        telemetry(serde_json::json!({"event":"action","action":format!("{action:?}")}));
        self.context_menu = None;
        match action {
            UiAction::DismissIgnore | UiAction::DismissIndexing => {
                if matches!(action, UiAction::DismissIgnore) {
                    self.status_display.pathignore_changed = false;
                } else {
                    self.status_display.pathindexing_changed = false;
                }
                cx.notify();
                return;
            }
            UiAction::DismissFda => {
                self.prefs.fda_dismissed = true;
                self.save_preferences(cx);
                cx.notify();
                return;
            }
            UiAction::Settings => {
                self.modal = Some(Modal::Settings);
                self.modal_selection = 0;
                self.table_focus.focus(window);
                cx.notify();
                return;
            }
            UiAction::Add | UiAction::Ignore | UiAction::Reset | UiAction::FdaSettings
                if matches!(self.modal, Some(Modal::Settings)) =>
            {
                self.modal = None;
                cx.notify();
            }
            UiAction::Cancel => {
                if !matches!(self.modal, Some(Modal::Onboarding)) {
                    self.modal = None;
                }
                self.rename_path = None;
                self.table_focus.focus(window);
                cx.notify();
                return;
            }
            _ => {}
        }
        if matches!(action, UiAction::FdaSettings) {
            self.status_display.fda_settings_opened = true;
            cx.notify();
        }
        if self.action_busy {
            return;
        }
        if crate::env_truthy("EVERYTHING_UI_TEST") {
            if matches!(action, UiAction::OnboardGranted | UiAction::OnboardSkip) {
                self.fixture_onboarding_decided = true;
                self.modal = None;
                cx.notify();
                return;
            }
            if matches!(
                action,
                UiAction::Register
                    | UiAction::Autostart
                    | UiAction::Restart
                    | UiAction::FdaSettings
                    | UiAction::LoginSettings
                    | UiAction::Reset
            ) {
                self.notify_message("System settings are disabled in isolated UI tests", cx);
                return;
            }
        }
        if matches!(action, UiAction::Rename) {
            self.start_rename(window, cx);
            return;
        }
        if matches!(action, UiAction::Trash) {
            let paths = self.model.paths();
            if !paths.is_empty() {
                self.modal = Some(Modal::Trash(paths));
                self.modal_selection = 0;
                self.table_focus.focus(window);
                cx.notify();
            }
            return;
        }
        if matches!(action, UiAction::Reset) {
            if self.status.state == "Indexing" {
                self.notify_message(
                    "Indexing in progress. Please try again after it completes.",
                    cx,
                );
                return;
            }
            self.model.generation += 1;
            self.search_task = None;
            self.model.rows.clear();
            self.model.clear_selection();
            self.model.loaded_count = 0;
            self.model.total = 0;
            self.displayed_query.clear();
            let mut status = self.status.clone();
            status.state = "Indexing".into();
            status.scanned = 0;
            status.entries_count = 0;
            status.message = None;
            self.update_status(status, cx);
        }
        let mut paths = self.model.paths();
        let confirm = self.modal.clone();
        if matches!(action, UiAction::Confirm) {
            match &confirm {
                Some(Modal::Trash(targets)) => paths = targets.clone(),
                _ => return,
            }
            self.modal = None;
        }
        let path = paths.first().cloned().unwrap_or_default();
        if matches!(
            action,
            UiAction::Open
                | UiAction::OpenWith
                | UiAction::Reveal
                | UiAction::CopyPaths
                | UiAction::CopyFiles
                | UiAction::QuickLook
                | UiAction::Package
                | UiAction::TrashImmediately
                | UiAction::DeletePermanently
        ) && paths.is_empty()
        {
            return;
        }
        let enabled = !self
            .onboarding
            .as_ref()
            .is_some_and(|s| !s.opted_out && s.registration == "enabled");
        let backend = self.backend.clone();
        let sink = self.sink.clone();
        self.action_busy = true;
        cx.spawn_in(window, async move |view, cx| {
            let result: crate::AppResult<Outcome> = async {
                match action {
                    UiAction::Open => {
                        crate::open(paths).await?;
                        Ok(Outcome::Done)
                    }
                    UiAction::OpenWith => {
                        crate::open_with(path).await?;
                        Ok(Outcome::Done)
                    }
                    UiAction::Reveal => {
                        crate::reveal_in_finder(paths).await?;
                        Ok(Outcome::Done)
                    }
                    UiAction::QuickLook => {
                        crate::quick_look(path).await?;
                        Ok(Outcome::Done)
                    }
                    UiAction::Package => {
                        crate::show_package_contents(path).await?;
                        Ok(Outcome::Done)
                    }
                    UiAction::CopyPaths => {
                        let count = paths.len();
                        crate::runtime::spawn_blocking(move || crate::copy_paths(paths))
                            .await
                            .map_err(|e| e.to_string())??;
                        Ok(Outcome::Message(format!("Copied {count} path(s)")))
                    }
                    UiAction::CopyFiles => {
                        let count = paths.len();
                        crate::runtime::spawn_blocking(move || crate::copy_files(paths))
                            .await
                            .map_err(|e| e.to_string())??;
                        Ok(Outcome::Message(format!("Copied {count} item(s)")))
                    }
                    UiAction::DeletePermanently => crate::runtime::spawn_blocking(move || {
                        crate::file_actions::delete_permanently(&paths)
                    })
                    .await
                    .map(Outcome::Trash)
                    .map_err(|error| error.to_string()),
                    UiAction::Add => {
                        crate::runtime::spawn_blocking(move || crate::open_pathindexing(&backend))
                            .await
                            .map_err(|e| e.to_string())??;
                        Ok(Outcome::Done)
                    }
                    UiAction::Ignore => {
                        crate::runtime::spawn_blocking(move || crate::open_pathignore(&backend))
                            .await
                            .map_err(|e| e.to_string())??;
                        Ok(Outcome::Done)
                    }
                    UiAction::FdaSettings => {
                        crate::open_privacy_settings();
                        Ok(Outcome::Refresh)
                    }
                    UiAction::LoginSettings => {
                        crate::open_login_items_settings();
                        Ok(Outcome::Refresh)
                    }
                    UiAction::Register => {
                        crate::register_service_elevated()?;
                        crate::register_daemon(&backend).await?;
                        Ok(Outcome::Refresh)
                    }
                    UiAction::Autostart => {
                        crate::set_daemon_autostart(enabled, &backend).await?;
                        Ok(Outcome::Refresh)
                    }
                    UiAction::Restart => {
                        crate::restart_app(sink);
                        Ok(Outcome::Refresh)
                    }
                    UiAction::OnboardGranted | UiAction::OnboardSkip => {
                        crate::record_onboarding_decision(
                            if matches!(action, UiAction::OnboardGranted) {
                                "granted"
                            } else {
                                "skipped"
                            }
                            .into(),
                            &backend,
                        )
                        .await?;
                        Ok(Outcome::Refresh)
                    }
                    UiAction::Reset => {
                        crate::reset_index(sink, &backend).await?;
                        Ok(Outcome::Done)
                    }
                    UiAction::TrashImmediately | UiAction::Confirm => {
                        crate::move_to_trash(paths, sink, &backend)
                            .await
                            .map(Outcome::Trash)
                    }
                    _ => Ok(Outcome::Refresh),
                }
            }
            .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.action_busy = false;
                this.completed_action = Some(action);
                match result {
                    Ok(Outcome::Done) => {}
                    Ok(Outcome::Message(message)) => this.notify_message(message, cx),
                    Ok(Outcome::Trash(result)) => {
                        this.model.remove_paths(&result.removed_paths);
                        this.refresh_file_tree(cx);
                        let message = if result.failures.is_empty() {
                            if matches!(action, UiAction::DeletePermanently) {
                                format!(
                                    "Deleted {} item(s) permanently.",
                                    result.removed_paths.len()
                                )
                            } else {
                                format!("Moved {} item(s) to Trash.", result.removed_paths.len())
                            }
                        } else {
                            let first = &result.failures[0];
                            format!(
                                "{} removed, {} failed: {}: {}",
                                result.removed_paths.len(),
                                result.failures.len(),
                                first.path,
                                first.error,
                            )
                        };
                        this.notify_message(message, cx);
                    }
                    Ok(Outcome::Refresh) => {
                        if matches!(action, UiAction::OnboardGranted | UiAction::OnboardSkip) {
                            this.modal = None;
                        }
                        this.refresh_onboarding(window, cx);
                    }
                    Err(error) => this.notify_message(error, cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn cancel_pending_rename(&mut self) -> bool {
        self.rename_click_candidate = None;
        self.rename_click_task.take().is_some()
    }

    pub(super) fn rename_delay() -> Duration {
        #[cfg(target_os = "macos")]
        let interval = (objc2_app_kit::NSEvent::doubleClickInterval() * 1000.).ceil() as u64;
        #[cfg(not(target_os = "macos"))]
        let interval = 500;
        Duration::from_millis(interval + 50)
    }

    pub(super) fn schedule_click_rename(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rename_click_task = Some(cx.spawn_in(window, async move |view, cx| {
            Timer::after(Self::rename_delay()).await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.rename_click_task = None;
                cx.notify();
                if window.is_window_active()
                    && this.table_focus.is_focused(window)
                    && this.rename_path.is_none()
                    && this.modal.is_none()
                    && this.context_menu.is_none()
                    && this.model.paths() == [path]
                {
                    this.start_rename(window, cx);
                }
            });
        }));
    }

    pub(super) fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_pending_rename();
        if self.model.selected.len() != 1 {
            return;
        }
        let Some(index) = self.model.primary else {
            return;
        };
        let entry = self.model.rows[index].clone();
        self.rename_path = Some(entry.path.clone());
        if !self.visible_rows.contains(&index) {
            self.list_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
        let stem_end = if entry.is_dir {
            entry.name.len()
        } else {
            entry
                .name
                .rfind('.')
                .filter(|dot| *dot > 0)
                .unwrap_or(entry.name.len())
        };
        let selection_end = entry.name[..stem_end].encode_utf16().count();
        self.rename_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.replace_and_mark_text_in_range(
                Some(0..0),
                &entry.name,
                Some(0..selection_end),
                window,
                cx,
            );
            input.unmark_text(window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    pub(super) fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.action_busy {
            return;
        }
        let Some(path) = self.rename_path.clone() else {
            return;
        };
        let name = self.rename_input.read(cx).value().to_string();
        if let Err(error) = crate::validate_new_name(&name) {
            self.notify_message(format!("Failed to rename: {error}"), cx);
            self.rename_input
                .update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let backend = self.backend.clone();
        let sink = self.sink.clone();
        self.action_busy = true;
        cx.spawn_in(window, async move |view, cx| {
            let result = crate::rename(path.clone(), name, sink, &backend).await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.action_busy = false;
                match result {
                    Ok(entry) => {
                        if let Some(row) = this.model.rows.iter_mut().find(|e| e.path == path) {
                            *row = entry;
                        }
                        this.rename_path = None;
                        if !this.input.focus_handle(cx).is_focused(window) {
                            this.table_focus.focus(window);
                        }
                        this.toast = None;
                        this.refresh_file_tree(cx);
                    }
                    Err(error) => {
                        this.notify_message(format!("Failed to rename: {error}"), cx);
                        this.rename_input
                            .update(cx, |input, cx| input.focus(window, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn refresh_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if crate::env_truthy("EVERYTHING_UI_TEST") {
            self.fda_ok = !crate::env_truthy("EVERYTHING_UI_TEST_ONBOARDING");
            self.onboarding = Some(OnboardingStateDto {
                recorded: self.fda_ok || self.fixture_onboarding_decided,
                fda_ok: self.fda_ok,
                registration: "unsupported".into(),
                ..Default::default()
            });
            if !self.fda_ok && !self.fixture_onboarding_decided {
                self.modal = Some(Modal::Onboarding);
                self.modal_selection = 2;
                self.table_focus.focus(window);
            }
            return;
        }
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |view, cx| {
            let result = crate::get_onboarding_state(&backend).await;
            let _ = view.update_in(cx, |this, window, cx| {
                match result {
                    Ok(state) => {
                        this.fda_ok = state.fda_ok;
                        if !state.recorded {
                            this.modal = Some(Modal::Onboarding);
                            this.modal_selection = 2;
                            this.table_focus.focus(window);
                        }
                        this.onboarding = Some(state);
                        if !this.fda_ok {
                            this.poll_fda(window, cx);
                        }
                    }
                    Err(error) => this.notify_message(error, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn poll_fda(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.fda_polling {
            return;
        }
        self.fda_polling = true;
        cx.spawn_in(window, async move |view, cx| loop {
            Timer::after(Duration::from_secs(3)).await;
            let ok = crate::runtime::spawn_blocking(crate::check_full_disk_access)
                .await
                .unwrap_or(false);
            if view
                .update_in(cx, |this, _, cx| {
                    if this.fda_ok != ok {
                        this.fda_ok = ok;
                        cx.notify();
                    }
                    if ok {
                        this.fda_polling = false;
                    }
                })
                .is_err()
                || ok
            {
                break;
            }
        })
        .detach();
    }

    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(action) = zoom::Zoom::shortcut(&event.keystroke.key, event.keystroke.modifiers)
        {
            self.change_zoom(action, window, cx);
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if self.zoom_popover_key_down(event, window, cx) {
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if self.cancel_pending_rename() {
            cx.notify();
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let meta = modifiers.platform || modifiers.control;
        let input_focused = self.input.focus_handle(cx).is_focused(window);
        let handled = if let Some(modal) = self.modal.clone() {
            let onboarding = matches!(modal, Modal::Onboarding);
            let settings = matches!(modal, Modal::Settings);
            let count = if onboarding {
                if self.fda_ok {
                    4
                } else {
                    3
                }
            } else if settings {
                self.settings_items().len()
            } else {
                2
            };
            match key {
                "escape" => self.action(UiAction::Cancel, window, cx),
                "tab" | "left" | "right" | "up" | "down" => {
                    let backwards = key == "left" || key == "up" || modifiers.shift;
                    self.modal_selection =
                        (self.modal_selection + if backwards { count - 1 } else { 1 }) % count;
                    cx.notify();
                }
                "enter" | "space" => {
                    let action = if onboarding {
                        [
                            UiAction::FdaSettings,
                            UiAction::Restart,
                            UiAction::OnboardSkip,
                            UiAction::OnboardGranted,
                        ][self.modal_selection]
                    } else if settings {
                        self.settings_items()[self.modal_selection]
                    } else if self.modal_selection == 0 {
                        UiAction::Cancel
                    } else {
                        UiAction::Confirm
                    };
                    self.action(action, window, cx);
                }
                _ => {}
            }
            true
        } else if self.context_menu.is_some() {
            let items = self.context_actions();
            match key {
                "up" | "down" => {
                    self.menu_selection = (self.menu_selection
                        + if key == "up" { items.len() - 1 } else { 1 })
                        % items.len();
                    cx.notify();
                    true
                }
                "enter" | "space" => {
                    if let Some((_, action, _)) = items.get(self.menu_selection) {
                        self.action(*action, window, cx);
                    }
                    true
                }
                "escape" => {
                    self.context_menu = None;
                    cx.notify();
                    true
                }
                _ => false,
            }
        } else if meta && key == "f" {
            self.focus_search(true, window, cx);
            true
        } else if self.rename_path.is_some() {
            match key {
                "escape" => {
                    self.action(UiAction::Cancel, window, cx);
                    true
                }
                "enter" => {
                    self.commit_rename(window, cx);
                    true
                }
                _ => false,
            }
        } else if self.file_tree_focused(window) {
            self.file_tree_key_down(key, window, cx)
        } else {
            match key {
                "down" | "up" | "pagedown" | "pageup" => {
                    if !self.model.rows.is_empty() {
                        let viewport = self.list_scroll.0.borrow().base_handle.bounds().size.height;
                        let page =
                            (f32::from(viewport) / self.row_height()).floor().max(1.) as isize;
                        let delta = match key {
                            "up" => -1,
                            "pageup" => -page,
                            "pagedown" => page,
                            _ => 1,
                        };
                        let next = if let Some(i) = self.model.primary {
                            (i as isize + delta).clamp(0, self.model.rows.len() as isize - 1)
                                as usize
                        } else {
                            delta.clamp(0, self.model.rows.len() as isize - 1) as usize
                        };
                        self.model.select(next, false, modifiers.shift);
                        self.table_focus.focus(window);
                        if !self.visible_rows.contains(&next) {
                            self.list_scroll.scroll_to_item(
                                next,
                                if delta > 0 {
                                    ScrollStrategy::Bottom
                                } else {
                                    ScrollStrategy::Top
                                },
                            );
                        }
                        if self.model.loaded_count > 0 && next + 30 >= self.model.rows.len() {
                            self.search_page(self.model.loaded_count, true, true, window, cx);
                        }
                        cx.notify();
                    }
                    true
                }
                "escape" => {
                    self.context_menu = None;
                    self.model.clear_selection();
                    cx.notify();
                    true
                }
                "o" if meta => {
                    self.action(UiAction::Open, window, cx);
                    true
                }
                "enter" if meta => {
                    self.action(UiAction::Reveal, window, cx);
                    true
                }
                "enter" if !input_focused => {
                    self.action(UiAction::Open, window, cx);
                    true
                }
                "f2" if !cfg!(target_os = "macos") => {
                    self.action(UiAction::Rename, window, cx);
                    true
                }
                "space" if !input_focused => {
                    if cfg!(target_os = "macos") {
                        self.action(UiAction::QuickLook, window, cx);
                    }
                    true
                }
                "a" if meta && !input_focused => {
                    self.model.selected = (0..self.model.rows.len()).collect();
                    cx.notify();
                    true
                }
                "c" if meta && !input_focused => {
                    self.action(
                        if cfg!(target_os = "macos") && !modifiers.alt {
                            UiAction::CopyFiles
                        } else {
                            UiAction::CopyPaths
                        },
                        window,
                        cx,
                    );
                    true
                }
                "delete" | "backspace"
                    if cfg!(target_os = "macos") && modifiers.platform && !input_focused =>
                {
                    self.action(UiAction::TrashImmediately, window, cx);
                    true
                }
                "delete" if !input_focused => {
                    let action = if cfg!(target_os = "windows") {
                        if modifiers.shift {
                            UiAction::DeletePermanently
                        } else {
                            UiAction::TrashImmediately
                        }
                    } else {
                        UiAction::Trash
                    };
                    self.action(action, window, cx);
                    true
                }
                "backspace" if meta && !input_focused => {
                    self.action(UiAction::Trash, window, cx);
                    true
                }
                _ => false,
            }
        };
        if handled {
            cx.stop_propagation();
            window.prevent_default();
        }
    }

    pub(super) fn sort(&mut self, column: usize, window: &mut Window, cx: &mut Context<Self>) {
        let key = ["name", "dir", "size", "mtime"][column];
        if self.sort_by == key {
            self.sort_dir = if self.sort_dir == "asc" {
                "desc"
            } else {
                "asc"
            }
            .into();
        } else {
            self.sort_by = key.into();
            self.sort_dir = "asc".into();
        }
        self.model.generation += 1;
        self.model.paging = false;
        self.input_started = Instant::now();
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.schedule_search(true, false, window, cx);
    }

    pub(super) fn context_actions(&self) -> Vec<(&'static str, UiAction, &'static str)> {
        let single = self.model.selected.len() == 1;
        let package = single
            && self
                .model
                .primary
                .and_then(|i| self.model.rows.get(i))
                .is_some_and(view::is_package);
        let reveal = if cfg!(target_os = "macos") {
            "Reveal in Finder"
        } else {
            "Reveal in Explorer"
        };
        let mut items = vec![("Open", UiAction::Open, "")];
        if package {
            items.push(("Show Package Contents", UiAction::Package, ""));
        }
        if cfg!(target_os = "macos") {
            items.push(("Quick Look", UiAction::QuickLook, ""));
        }
        items.extend([
            ("Open With...", UiAction::OpenWith, ""),
            (reveal, UiAction::Reveal, ""),
        ]);
        if cfg!(target_os = "macos") {
            items.push(("Copy", UiAction::CopyFiles, ""));
        }
        items.push(("Copy Path", UiAction::CopyPaths, ""));
        items.push(("Move to Trash", UiAction::Trash, ""));
        if single {
            items.push(("Rename", UiAction::Rename, ""));
        }
        items
    }

    pub(super) fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu_selection = 0;
        #[cfg(target_os = "windows")]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(handle) = HasWindowHandle::window_handle(window) {
                if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                    let b = window.bounds();
                    let scale = window.scale_factor();
                    let x = (f32::from(b.origin.x + position.x) * scale) as i32;
                    let y = (f32::from(b.origin.y + position.y) * scale) as i32;
                    if let Err(error) =
                        crate::win::context_menu::show(handle.hwnd.get(), &self.model.paths(), x, y)
                    {
                        self.notify_message(error, cx);
                    }
                    return;
                }
            }
        }
        self.context_menu = Some(position);
        self.write_test_state(window, cx);
        #[cfg(target_os = "macos")]
        match mac_menu::show(window, position, self.context_actions()) {
            Ok(receiver) => {
                cx.spawn_in(window, async move |view, cx| {
                    let action = receiver.recv().await.ok().flatten();
                    let _ = view.update_in(cx, |this, window, cx| {
                        this.context_menu = None;
                        if let Some(action) = action {
                            this.action(action, window, cx);
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            Err(error) => {
                self.context_menu = None;
                self.notify_message(error.to_string(), cx);
            }
        }
        cx.notify();
    }

    pub(super) fn native_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self
            .model
            .paths()
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return;
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Err(error) = drag::start_drag(
            window,
            drag::DragItem::Files(paths),
            drag::Image::Raw(include_bytes!("../../icons/32x32.png").to_vec()),
            |_, _| {},
            drag::Options::default(),
        ) {
            self.notify_message(error.to_string(), cx);
        }
    }
}
