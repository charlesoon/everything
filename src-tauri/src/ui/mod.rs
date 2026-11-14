//! GPUI desktop UI. Search and file operations are direct Rust calls; all I/O
//! runs outside the foreground executor.

mod actions;
mod assets;
mod design;
mod file_icons;
mod file_tree;
#[cfg(target_os = "macos")]
mod mac_menu;
mod middle_text;
mod status;
mod view;
mod zoom;

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui::{prelude::*, *};
use gpui_component::{
    input::{Input, InputEvent, InputState},
    ActiveTheme, Root, Theme,
};

use crate::{
    runtime::EventSink, ui_model::SearchModel, ui_prefs::Preferences, AppState, EntryDto,
    IndexStatusDto, OnboardingStateDto,
};
use actions::UiAction;

actions!(everything, [Quit, FocusSearch, OpenSettings]);

pub(super) fn run() {
    let started = Instant::now();
    let (state, sink, receiver) = match crate::setup_native() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Everything could not start: {error}");
            return;
        }
    };
    let prefs = Preferences::load();
    crate::logging::subscribe(sink.clone());
    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            assets::load_fonts(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("ctrl-q", Quit, None),
                KeyBinding::new("cmd-,", OpenSettings, None),
            ]);
            cx.set_menus(vec![Menu {
                name: "Everything".into(),
                items: vec![
                    MenuItem::action("Settings…", OpenSettings),
                    MenuItem::action("Search", FocusSearch),
                    MenuItem::separator(),
                    MenuItem::action("Quit Everything", Quit),
                ],
            }]);
            let fallback = Bounds::centered(None, size(px(600.), px(380.)), cx);
            let bounds = prefs
                .window
                .map(|[x, y, w, h]| Bounds::new(point(px(x), px(y)), size(px(w), px(h))))
                .filter(|b| cx.displays().iter().any(|d| d.bounds().intersects(b)))
                .unwrap_or(fallback);
            let result = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Everything".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(16.), px(12.))),
                    }),
                    app_id: Some("com.everything.app".into()),
                    window_min_size: Some(size(px(400.), px(300.))),
                    ..Default::default()
                },
                move |window, cx| {
                    let view = cx.new(|cx| {
                        EverythingView::new(state, sink, receiver, prefs, started, window, cx)
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                },
            );
            if let Err(error) = result {
                eprintln!("Could not open Everything: {error}");
                cx.quit();
            }
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}

#[derive(Clone)]
enum Modal {
    Trash(Vec<String>),
    Onboarding,
    Settings,
}

struct EverythingView {
    backend: AppState,
    sink: EventSink,
    input: Entity<InputState>,
    rename_input: Entity<InputState>,
    table_focus: FocusHandle,
    model: SearchModel,
    prefs: Preferences,
    sort_by: String,
    sort_dir: String,
    list_scroll: UniformListScrollHandle,
    list_layout_count: usize,
    horizontal_scroll: ScrollHandle,
    file_tree: file_tree::State,
    zoom_popover: zoom::Popover,
    status: IndexStatusDto,
    status_display: status::StatusDisplay,
    status_task: Option<Task<()>>,
    onboarding: Option<OnboardingStateDto>,
    health: Option<String>,
    fda_ok: bool,
    fda_polling: bool,
    fixture_onboarding_decided: bool,
    modal: Option<Modal>,
    context_menu: Option<Point<Pixels>>,
    menu_selection: usize,
    modal_selection: usize,
    rename_path: Option<String>,
    /// The inline rename editor lives in the file tree row, not the results row.
    rename_in_tree: bool,
    /// File actions (copy, trash, rename, context menu) target the file tree
    /// selection instead of the results selection.
    target_tree: bool,
    rename_click_candidate: Option<String>,
    rename_click_task: Option<Task<()>>,
    action_busy: bool,
    completed_action: Option<UiAction>,
    toast: Option<(String, Instant)>,
    /// Inline "Copied" badge next to a copyable path: (element id, when).
    copied: Option<(&'static str, Instant)>,
    search_task: Option<Task<()>>,
    preference_task: Option<Task<()>>,
    input_started: Instant,
    search_started: Instant,
    startup_started: Instant,
    first_results_logged: bool,
    last_search_ms: f64,
    last_search_fired: Option<Instant>,
    displayed_query: String,
    table_width: f32,
    table_columns: [f32; 4],
    resizing: Option<(usize, Point<Pixels>, [f32; 4])>,
    drag_start: Option<(Point<Pixels>, usize)>,
    visible_rows: std::ops::Range<usize>,
    _subscriptions: Vec<Subscription>,
    _hotkey_manager: Option<global_hotkey::GlobalHotKeyManager>,
}

impl EverythingView {
    fn new(
        backend: AppState,
        sink: EventSink,
        receiver: async_channel::Receiver<()>,
        prefs: Preferences,
        started: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search file/folder names"));
        let rename_input = cx.new(|cx| InputState::new(window, cx));
        let status = crate::get_index_status(&backend);
        let mut this = Self {
            backend,
            sink,
            input,
            rename_input,
            table_focus: cx.focus_handle(),
            model: SearchModel::default(),
            prefs,
            sort_by: "name".into(),
            sort_dir: "asc".into(),
            list_scroll: UniformListScrollHandle::new(),
            list_layout_count: 0,
            horizontal_scroll: ScrollHandle::new(),
            file_tree: file_tree::State::new(cx),
            zoom_popover: zoom::Popover::default(),
            status_display: status::StatusDisplay::new(&status),
            status_task: None,
            status,
            onboarding: None,
            health: None,
            fda_ok: true,
            modal: None,
            context_menu: None,
            menu_selection: 0,
            modal_selection: 0,
            rename_path: None,
            rename_in_tree: false,
            target_tree: false,
            rename_click_candidate: None,
            rename_click_task: None,
            action_busy: false,
            completed_action: None,
            toast: None,
            copied: None,
            fda_polling: false,
            fixture_onboarding_decided: false,
            search_task: None,
            preference_task: None,
            input_started: started,
            search_started: started,
            startup_started: started,
            first_results_logged: false,
            last_search_ms: 0.,
            last_search_fired: None,
            displayed_query: String::new(),
            table_width: 0.,
            table_columns: [0.; 4],
            resizing: None,
            drag_start: None,
            visible_rows: 0..0,
            _subscriptions: Vec::new(),
            _hotkey_manager: None,
        };
        this.apply_theme(window, cx);
        this._subscriptions.push(cx.subscribe_in(
            &this.input,
            window,
            |this, input, event, window, cx| match event {
                InputEvent::Change => {
                    this.cancel_pending_rename();
                    this.input_started = Instant::now();
                    this.status_display.clear_finished();
                    this.rename_path = None;
                    this.context_menu = None;
                    let query = input.read(cx).value().to_string();
                    this.model.new_query(query);
                    this.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    this.schedule_search(false, true, window, cx);
                }
                InputEvent::Focus => {
                    this.cancel_pending_rename();
                    this.target_tree = false;
                    this.model.clear_selection();
                    cx.notify();
                }
                _ => {}
            },
        ));
        this._subscriptions.push(cx.subscribe_in(
            &this.rename_input,
            window,
            |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.commit_rename(window, cx);
                }
            },
        ));
        this._subscriptions
            .push(cx.observe_window_bounds(window, |this, window, cx| {
                #[cfg(target_os = "macos")]
                mac_menu::position_buttons(window);
                if let WindowBounds::Windowed(b) = window.window_bounds() {
                    let content = window.viewport_size();
                    this.prefs.window = Some([
                        b.origin.x.into(),
                        b.origin.y.into(),
                        content.width.into(),
                        content.height.into(),
                    ]);
                    this.save_preferences(cx);
                }
            }));
        this._subscriptions
            .push(cx.observe_window_appearance(window, |this, window, cx| {
                this.apply_theme(window, cx);
                cx.notify();
            }));
        this._subscriptions.push(cx.on_app_quit(|this, _| {
            this.preference_task = None;
            let prefs = this.prefs.clone();
            async move {
                let _ = crate::runtime::spawn_blocking(move || prefs.save()).await;
            }
        }));
        this._subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.cancel_pending_rename();
                    this.close_zoom_popover(cx);
                }
                if window.is_window_active() {
                    this.update_status(crate::get_index_status(&this.backend), cx);
                    this.refresh_file_tree(cx);
                    cx.notify();
                }
            }));
        let event_sink = this.sink.clone();
        cx.spawn_in(window, async move |view, cx| {
            while receiver.recv().await.is_ok() {
                let events = event_sink.drain();
                if view
                    .update_in(cx, |this, window, cx| {
                        this.update_status(crate::get_index_status(&this.backend), cx);
                        this.status_display.receive(&events);
                        if let Some(health) = events.get("service_health") {
                            this.health = if health["ok"].as_bool() == Some(true) {
                                None
                            } else {
                                health["state"].as_str().map(str::to_string)
                            };
                        }
                        if events.contains_key("index_updated")
                            || events.contains_key("pathindexing_changed")
                            || events
                                .get("index_state")
                                .is_some_and(|event| event["state"] == "Ready")
                        {
                            this.schedule_search(true, false, window, cx);
                            this.refresh_file_tree(cx);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // Run before gpui-component's key bindings (notably Tab and Copy).
        let weak = cx.entity().downgrade();
        this._subscriptions
            .push(cx.intercept_keystrokes(move |event, window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.key_down(
                        &KeyDownEvent {
                            keystroke: event.keystroke.clone(),
                            is_held: false,
                        },
                        window,
                        cx,
                    );
                });
            }));
        this.refresh_onboarding(window, cx);
        this.update_status(this.status.clone(), cx);
        this.register_hotkey(window, cx);
        this.focus_search(false, window, cx);
        this.schedule_search(false, false, window, cx);
        #[cfg(all(target_os = "macos", feature = "ui-automation"))]
        crate::ui_automation::start();
        crate::mark_frontend_ready(&this.backend);
        cx.on_next_frame(window,move |_,window,cx| {
            // Two callbacks bracket a submitted frame, just as rAF→rAF does in
            // the Tauri reference harness. Logging is opt-in and after paint.
            window.on_next_frame(move |_,_| telemetry(serde_json::json!({"event":"firstFrame","elapsedMs":started.elapsed().as_secs_f64()*1000.})));
            cx.notify();
        });
        this
    }

    fn update_status(&mut self, next: IndexStatusDto, cx: &mut Context<Self>) {
        self.status_display
            .update(&self.status, &next, Instant::now());
        self.status = next;
        self.refresh_activity_timer(cx);
    }

    fn search_activity(&self) -> Option<String> {
        if self.model.busy || self.model.paging {
            Some(format!(
                "{} · {}s",
                if self.model.rows.is_empty() {
                    "Loading files"
                } else if self.model.paging {
                    "Loading more"
                } else {
                    "Searching"
                },
                self.search_started.elapsed().as_secs()
            ))
        } else {
            None
        }
    }

    fn empty_results_text(&self) -> String {
        if let Some(error) = &self.model.error {
            format!("Search failed: {error}")
        } else if let Some(activity) = self.search_activity() {
            activity
        } else if self.status.state == "Indexing" {
            "Indexing files…".into()
        } else if self.model.query.is_empty() {
            "No indexed files".into()
        } else {
            "No matching files".into()
        }
    }

    fn refresh_activity_timer(&mut self, cx: &mut Context<Self>) {
        if self.status.state != "Indexing" && !self.model.busy && !self.model.paging {
            self.status_task = None;
        } else if self.status_task.is_none() {
            self.status_task = Some(cx.spawn(async move |view, cx| loop {
                Timer::after(Duration::from_secs(1)).await;
                if !view
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.status.state == "Indexing" || this.model.busy || this.model.paging
                    })
                    .unwrap_or(false)
                {
                    break;
                }
            }));
        }
    }

    fn schedule_search(
        &mut self,
        preserve: bool,
        debounce: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if preserve && !debounce {
            self.model.generation += 1;
            self.model.paging = false;
        }
        self.search_task = None;
        if !debounce {
            self.search_page(0, preserve, true, window, cx);
            return;
        }
        // The preserved WebView uses a 200ms leading + trailing search.
        if self
            .last_search_fired
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(200))
        {
            self.last_search_fired = Some(Instant::now());
            self.search_page(0, preserve, false, window, cx);
        }
        let generation = self.model.generation;
        self.search_task = Some(cx.spawn_in(window, async move |view, cx| {
            Timer::after(Duration::from_millis(200)).await;
            let _ = view.update_in(cx, |this, window, cx| {
                if generation == this.model.generation {
                    this.last_search_fired = Some(Instant::now());
                    this.model.generation += 1;
                    this.model.paging = false;
                    // Preserve selection made while the leading results painted.
                    this.search_page(0, true, true, window, cx);
                }
            });
        }));
        cx.notify();
    }

    fn search_page(
        &mut self,
        offset: usize,
        preserve: bool,
        include_total: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if offset > 0 && (self.model.paging || self.model.eof || self.model.busy) {
            return;
        }
        if offset > 0 {
            self.model.paging = true;
        } else {
            self.model.busy = true;
        }
        self.search_started = Instant::now();
        self.refresh_activity_timer(cx);
        let state = self.backend.clone();
        let sink = self.sink.clone();
        let query = self.model.query.clone();
        let sort = self.sort_by.clone();
        let direction = self.sort_dir.clone();
        let generation = self.model.generation;
        let limit = if query.chars().count() == 1 { 100 } else { 500 };
        let requested = if preserve && offset == 0 {
            self.model.loaded_count.max(limit)
        } else {
            limit
        };
        let input_started = self.input_started;
        let removed = self.model.removed.iter().cloned().collect::<Vec<_>>();
        cx.spawn_in(window,async move |view,cx| {
            let started=Instant::now();
            // Refresh every loaded page so an index update does not truncate a
            // deep list or discard its selection/viewport. The engine caps an
            // individual request (1000, or 100 for a single character).
            let result:crate::AppResult<crate::SearchResultDto>=async {
                let mut combined:Option<crate::SearchResultDto>=None;
                let mut fetched=0;
                loop {
                    let chunk=crate::effective_search_limit(&query,Some((requested-fetched) as u32),500) as usize;
                    let page=crate::search(sink.clone(),query.clone(),Some(chunk as u32),Some((offset+fetched) as u32),Some(sort.clone()),Some(direction.clone()),Some(include_total && combined.is_none()),&state).await?;
                    let count=page.entries.len();
                    if let Some(result)=&mut combined {result.entries.extend(page.entries);} else {combined=Some(page);}
                    fetched+=count;
                    if count<chunk||fetched>=requested {break;}
                }
                Ok(combined.expect("at least one search page"))
            }.await;
            let pending=if !removed.is_empty() { crate::get_pending_trash_paths(removed,&state).await.ok() } else {None};
            let elapsed=started.elapsed().as_secs_f64()*1000.;
            let _=view.update_in(cx,move |this,window,cx| {
                if generation!=this.model.generation {return;}
                match result {
                    Ok(mut result)=>{
                        if !result.total_known {
                            if this.displayed_query == query && this.model.total_known {
                                result.total_count = this.model.total + result.entries.iter().filter(|entry|this.model.is_removed(&entry.path)).count() as u32;
                                result.total_known = true;
                            } else { result.total_count = (offset + result.entries.len()) as u32; }
                        }
                        if this.model.apply(generation,offset,result,preserve,requested) {
                            if let Some(paths)=pending {this.model.removed=paths.into_iter().collect();}
                            this.last_search_ms=elapsed;
                            if offset == 0 { this.displayed_query = query.clone(); }
                            if offset==0 {
                                let rows=this.model.rows.len();
                                let first=this.model.rows.first().map(|r|r.name.clone());
                                let first_results = rows > 0 && !this.first_results_logged;
                                this.first_results_logged |= first_results;
                                let startup_started = this.startup_started;
                                cx.on_next_frame(window,move |_,window,cx| {
                                    window.on_next_frame(move |_,cx| {
                                        telemetry(serde_json::json!({"event":"searchFrame","query":query,"backendMs":elapsed,"elapsedMs":input_started.elapsed().as_secs_f64()*1000.,"rows":rows,"first":first,"includeTotal":include_total}));
                                        if first_results {
                                            let elapsed = startup_started.elapsed().as_millis();
                                            cx.background_executor().spawn(async move {
                                                crate::diag::log(format!("phase=gui.first_results event=end elapsed_ms={elapsed} rows={rows}"));
                                            }).detach();
                                        }
                                    });
                                    cx.notify();
                                });
                            }
                        }
                    },
                    Err(error)=>{
                        this.notify_message(format!("{}: {error}", if offset == 0 {"Search failed"} else {"Failed to load more"}),cx);
                        this.model.busy=false;this.model.paging=false;this.model.error=Some(error);
                        if offset==0 {this.model.rows.clear();this.model.loaded_count=0;this.model.clear_selection();}
                    }
                }
                this.refresh_activity_timer(cx);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn focus_search(&mut self, select: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_pending_rename();
        self.context_menu = None;
        self.model.clear_selection();
        self.input.update(cx, |input, cx| input.focus(window, cx));
        if select {
            window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
        }
        cx.notify();
    }

    fn save_preferences(&mut self, cx: &mut Context<Self>) {
        let prefs = self.prefs.clone();
        self.preference_task = Some(cx.spawn(async move |_, _| {
            Timer::after(Duration::from_millis(250)).await;
            let _ = crate::runtime::spawn_blocking(move || prefs.save()).await;
        }));
    }

    fn apply_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        // Opt-in fixture launches can render both palettes without changing
        // Windows' actual theme or the user's saved preferences.
        let fixture_theme =
            if cfg!(feature = "ui-automation") && crate::env_truthy("EVERYTHING_UI_TEST") {
                match std::env::var("EVERYTHING_UI_TEST_THEME").as_deref() {
                    Ok("light") => Some(gpui_component::ThemeMode::Light),
                    Ok("dark") => Some(gpui_component::ThemeMode::Dark),
                    _ => None,
                }
            } else {
                None
            };
        if let Some(mode) = fixture_theme {
            Theme::change(mode, Some(window), cx);
        } else {
            Theme::sync_system_appearance(Some(window), cx);
        }
        let p = design::Palette::get(cx);
        let theme = Theme::global_mut(cx);
        theme.font_family = design::SANS.into();
        theme.font_size = px(design::BASE_REM_SIZE * self.prefs.zoom);
        theme.background = p.bg;
        theme.foreground = p.text;
        theme.muted_foreground = p.muted.opacity(0.85);
        theme.caret = p.text;
        theme.primary = p.accent;
        theme.selection = p.accent.opacity(0.3);
        theme.border = p.border;
        theme.input = p.input_border;
        theme.ring = p.ring;
        theme.popover = p.menu;
        theme.popover_foreground = p.menu_text;
        theme.radius = px(6. * self.prefs.zoom);
        theme.radius_lg = px(8. * self.prefs.zoom);
    }

    fn notify_message(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let time = Instant::now();
        self.toast = Some((message.into(), time));
        cx.spawn(async move |view, cx| {
            Timer::after(Duration::from_secs(3)).await;
            let _ = view.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|(_, t)| *t == time) {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn register_hotkey(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            use global_hotkey::{
                hotkey::{Code, HotKey, Modifiers},
                GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
            };
            if crate::env_truthy("EVERYTHING_UI_TEST") {
                return;
            }
            let Ok(manager) = GlobalHotKeyManager::new() else {
                return;
            };
            let hotkey = HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Space);
            if let Err(error) = manager.register(hotkey) {
                eprintln!("Global shortcut: {error}");
                return;
            }
            let (send, receive) = async_channel::bounded(1);
            GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
                if event.id == hotkey.id() && event.state == HotKeyState::Pressed {
                    let _ = send.try_send(());
                }
            }));
            self._hotkey_manager = Some(manager);
            cx.spawn_in(window, async move |view, cx| {
                while receive.recv().await.is_ok() {
                    if view
                        .update_in(cx, |this, window, cx| {
                            cx.activate(true);
                            window.activate_window();
                            this.focus_search(true, window, cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
    }
}

fn telemetry(value: serde_json::Value) {
    if let Some(path) = std::env::var_os("EVERYTHING_UI_METRICS") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{value}");
        }
    }
}
