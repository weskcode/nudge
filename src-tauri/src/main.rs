use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command as ProcCommand;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, Wry};

const REPO_URL: &str = "https://github.com/brettferdosi/remindful";

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    interval_secs: u32,
    message: String,
    enabled_on_wake: bool,
    reset_on_wake: bool,
    count_total: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_secs: 30 * 60,
            message: "time to step away".into(),
            enabled_on_wake: true,
            reset_on_wake: true,
            count_total: 0,
        }
    }
}

#[derive(Serialize, Clone)]
struct PublicState {
    interval_secs: u32,
    message: String,
    enabled_on_wake: bool,
    reset_on_wake: bool,
    count_total: u32,
    count_session: u32,
    enabled: bool,
    showing: bool,
    remaining_secs: i64,
}

struct App {
    config: Mutex<Config>,
    next_fire: Mutex<Option<Instant>>,
    showing: Mutex<bool>,
    count_session: Mutex<u32>,
    overlay_seq: Mutex<u32>,
    overlay_windows: Mutex<Vec<String>>,
    countdown_item: Mutex<Option<MenuItem<Wry>>>,
    toggle_item: Mutex<Option<MenuItem<Wry>>>,
    tray: Mutex<Option<TrayIcon<Wry>>>,
    last_label: Mutex<String>,
}

fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .map(|d| d.join("nudge"))
        .expect("cannot resolve config directory");
    let _ = fs::create_dir_all(&dir);
    dir.join("config.json")
}

fn load_config(_app: &AppHandle) -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_config(app: &AppHandle) {
    let state = app.state::<App>();
    let config = state.config.lock().unwrap();
    if let Ok(json) = serde_json::to_string_pretty(&*config) {
        let _ = fs::write(config_path(), json);
    }
}

fn snapshot(app: &AppHandle) -> PublicState {
    let state = app.state::<App>();
    let config = state.config.lock().unwrap().clone();
    let next = *state.next_fire.lock().unwrap();
    let showing = *state.showing.lock().unwrap();
    let session = *state.count_session.lock().unwrap();
    let remaining = if showing {
        0
    } else if next.is_none() {
        -1
    } else {
        next.unwrap()
            .saturating_duration_since(Instant::now())
            .as_secs() as i64
    };
    PublicState {
        interval_secs: config.interval_secs,
        message: config.message.clone(),
        enabled_on_wake: config.enabled_on_wake,
        reset_on_wake: config.reset_on_wake,
        count_total: config.count_total,
        count_session: session,
        enabled: next.is_some() || showing,
        showing,
        remaining_secs: remaining,
    }
}

fn broadcast(app: &AppHandle) {
    let _ = app.emit("nudge://state", snapshot(app));
}

fn is_reminders_enabled(app: &AppHandle) -> bool {
    let state = app.state::<App>();
    state.next_fire.lock().unwrap().is_some() || *state.showing.lock().unwrap()
}

fn enable_reminders(app: &AppHandle) {
    let state = app.state::<App>();
    let interval = state.config.lock().unwrap().interval_secs;
    *state.next_fire.lock().unwrap() = Some(Instant::now() + Duration::from_secs(interval as u64));
    refresh_tray(app);
    broadcast(app);
}

fn disable_reminders(app: &AppHandle) {
    let state = app.state::<App>();
    *state.next_fire.lock().unwrap() = None;
    if *state.showing.lock().unwrap() {
        hide_overlay(app);
        return;
    }
    refresh_tray(app);
    broadcast(app);
}

fn toggle_reminders(app: &AppHandle) {
    if is_reminders_enabled(app) {
        disable_reminders(app);
    } else {
        enable_reminders(app);
    }
}

fn hms(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn current_label(app: &AppHandle) -> String {
    let state = app.state::<App>();
    let next = *state.next_fire.lock().unwrap();
    let showing = *state.showing.lock().unwrap();
    if showing {
        "Reminder showing".to_string()
    } else if let Some(fire) = next {
        format!(
            "Reminder in {}",
            hms(fire.saturating_duration_since(Instant::now()).as_secs())
        )
    } else {
        "Reminders off".to_string()
    }
}

fn refresh_tray(app: &AppHandle) {
    let state = app.state::<App>();
    let enabled = is_reminders_enabled(app);
    let text = current_label(app);

    {
        let mut last = state.last_label.lock().unwrap();
        if *last == text {
            return;
        }
        *last = text.clone();
    }

    {
        let countdown_guard = state.countdown_item.lock().unwrap();
        if let Some(item) = countdown_guard.as_ref() {
            let _ = item.set_text(text.clone());
        }
        let toggle_guard = state.toggle_item.lock().unwrap();
        if let Some(item) = toggle_guard.as_ref() {
            let _ = item.set_text(if enabled {
                "Turn reminders off"
            } else {
                "Turn reminders on"
            });
        }
        let tray_guard = state.tray.lock().unwrap();
        if let Some(tray) = tray_guard.as_ref() {
            let _ = tray.set_tooltip(Some(text.clone()));
            let icon = if enabled {
                tauri::include_image!("icons/tray-on.png")
            } else {
                tauri::include_image!("icons/tray-off.png")
            };
            let _ = tray.set_icon(Some(icon));
        }
    }
}

fn show_overlay(app: &AppHandle) {
    if is_reminders_enabled(app) && *app.state::<App>().showing.lock().unwrap() {
        return;
    }
    let state = app.state::<App>();
    {
        let mut config = state.config.lock().unwrap();
        config.count_total += 1;
    }
    *state.count_session.lock().unwrap() += 1;
    save_config(app);

    let mut seq = state.overlay_seq.lock().unwrap();
    *seq += 1;
    let seq = *seq;
    drop(seq);

    let monitors = match app.available_monitors() {
        Ok(monitors) if !monitors.is_empty() => monitors,
        _ => match app.primary_monitor() {
            Ok(Some(monitor)) => vec![monitor],
            _ => return,
        },
    };

    let mut opened = Vec::new();
    for (index, monitor) in monitors.iter().enumerate() {
        let label = format!("overlay-{}-{}", seq, index);
        let position = monitor.position();
        let size = monitor.size();
        let scale = monitor.scale_factor();

        let mut builder = WebviewWindowBuilder::new(
            app,
            &label,
            WebviewUrl::App("overlay.html".into()),
        )
        .title("nudge")
        .position(position.x as f64 / scale, position.y as f64 / scale)
        .inner_size(size.width as f64 / scale, size.height as f64 / scale)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(cfg!(target_os = "macos"));

        if !cfg!(target_os = "macos") {
            builder = builder.fullscreen(true);
        }

        if let Ok(window) = builder.build() {
            #[cfg(target_os = "macos")]
            {
                unsafe {
                    use objc::{msg_send, sel, sel_impl};
                    use cocoa::base::id;
                    let ns_window: id = window.ns_window().unwrap() as id;
                    const NS_MAIN_MENU_WINDOW_LEVEL: i64 = 24;
                    let _: () = msg_send![ns_window, setLevel: NS_MAIN_MENU_WINDOW_LEVEL];
                    let behavior: u64 = msg_send![ns_window, collectionBehavior];
                    let combined = behavior | 1 << 0 | 1 << 8;
                    let _: () = msg_send![ns_window, setCollectionBehavior: combined];
                }
            }
            opened.push(label);
        }
    }

    *state.overlay_windows.lock().unwrap() = opened;
    *state.showing.lock().unwrap() = true;

    if cfg!(target_os = "macos") {
        let _ = app.show();
    }
    refresh_tray(app);
    broadcast(app);
}

fn hide_overlay(app: &AppHandle) {
    let state = app.state::<App>();
    if *state.showing.lock().unwrap() {
        for label in state.overlay_windows.lock().unwrap().drain(..) {
            if let Some(window) = app.get_webview_window(&label) {
                let _ = window.close();
            }
        }
        *state.showing.lock().unwrap() = false;
        if cfg!(target_os = "macos") {
            let _ = app.hide();
        }
    }
    enable_reminders(app);
}

fn on_wake(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || on_wake_main(&handle));
}

fn on_wake_main(app: &AppHandle) {
    let state = app.state::<App>();
    let was_showing = *state.showing.lock().unwrap();
    let config = state.config.lock().unwrap().clone();

    *state.count_session.lock().unwrap() = 0;

    if was_showing {
        hide_overlay(app);
    }
    if config.enabled_on_wake && state.next_fire.lock().unwrap().is_none() {
        enable_reminders(app);
    } else if is_reminders_enabled(app) && config.reset_on_wake {
        enable_reminders(app);
    }
    refresh_tray(app);
    broadcast(app);
}

fn tick_loop(app: AppHandle) {
    let mut last_checked = Instant::now();
    let mut last_wall = SystemTime::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));

        let wall_delta = SystemTime::now()
            .duration_since(last_wall)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let steady_delta = last_checked.elapsed().as_secs();
        last_checked = Instant::now();
        last_wall = SystemTime::now();
        if wall_delta > steady_delta + 3 {
            on_wake(&app);
        }

        let state = app.state::<App>();
        let next = *state.next_fire.lock().unwrap();
        if next.is_some() && next.unwrap() <= Instant::now() {
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || show_overlay(&handle));
            continue;
        }
        refresh_tray(&app);
    }
}

fn open_url(url: &str) {
    let result = if cfg!(target_os = "macos") {
        ProcCommand::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        ProcCommand::new("cmd").args(["/c", "start", "", url]).spawn()
    } else {
        ProcCommand::new("xdg-open").arg(url).spawn()
    };
    let _ = result;
}

#[tauri::command]
fn get_state(app: AppHandle) -> PublicState {
    snapshot(&app)
}

#[tauri::command]
fn set_config(
    app: AppHandle,
    interval_secs: Option<u32>,
    message: Option<String>,
    enabled_on_wake: Option<bool>,
    reset_on_wake: Option<bool>,
) {
    let state = app.state::<App>();
    if interval_secs.unwrap_or(0) > 0 {
        let interval = interval_secs.unwrap();
        let mut config = state.config.lock().unwrap();
        config.interval_secs = interval;
        if is_reminders_enabled(&app) {
            *state.next_fire.lock().unwrap() =
                Some(Instant::now() + Duration::from_secs(interval as u64));
        }
    }
    if let Some(message) = message {
        state.config.lock().unwrap().message = message;
    }
    if let Some(value) = enabled_on_wake {
        state.config.lock().unwrap().enabled_on_wake = value;
    }
    if let Some(value) = reset_on_wake {
        state.config.lock().unwrap().reset_on_wake = value;
    }
    save_config(&app);
    refresh_tray(&app);
    broadcast(&app);
}

#[tauri::command]
fn close_overlay(app: AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || hide_overlay(&handle));
}

#[tauri::command]
fn reset_counters(app: AppHandle) {
    let state = app.state::<App>();
    state.config.lock().unwrap().count_total = 0;
    *state.count_session.lock().unwrap() = 0;
    save_config(&app);
    broadcast(&app);
}

#[tauri::command]
fn open_settings(app: AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.set_focus();
    } else {
        let _ = WebviewWindowBuilder::new(
            &app,
            "settings",
            WebviewUrl::App("index.html".into()),
        )
        .title("Nudge Settings")
        .inner_size(440.0, 500.0)
        .center()
        .resizable(false)
        .build();
    }
}


fn main() {
    tauri::Builder::default()
        .manage(App {
            config: Mutex::new(Config::default()),
            next_fire: Mutex::new(None),
            showing: Mutex::new(false),
            count_session: Mutex::new(0),
            overlay_seq: Mutex::new(0),
            overlay_windows: Mutex::new(Vec::new()),
            countdown_item: Mutex::new(None),
            toggle_item: Mutex::new(None),
            tray: Mutex::new(None),
            last_label: Mutex::new(String::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_config,
            close_overlay,
            reset_counters,
            open_settings
        ])
        .setup(|app| {
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let config = load_config(&handle);
            *app.state::<App>().config.lock().unwrap() = config;

            let countdown = MenuItem::with_id(
                &handle,
                "countdown",
                "Reminder in 30:00",
                false,
                None::<&str>,
            )?;
            let toggle = MenuItem::with_id(
                &handle,
                "toggle",
                "Turn reminders off",
                true,
                None::<&str>,
            )?;
            let settings = MenuItem::with_id(&handle, "settings", "Settings", true, None::<&str>)?;
            let about = MenuItem::with_id(
                &handle,
                "about",
                "About nudge (inspired by remindful)",
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(&handle, "quit", "Quit", true, None::<&str>)?;

            let menu = Menu::with_items(
                &handle,
                &[
                    &countdown,
                    &PredefinedMenuItem::separator(&handle)?,
                    &toggle,
                    &settings,
                    &about,
                    &PredefinedMenuItem::separator(&handle)?,
                    &quit,
                ],
            )?;

            let tray = TrayIconBuilder::with_id("main")
                .icon(tauri::include_image!("icons/tray-on.png"))
                .icon_as_template(cfg!(target_os = "macos"))
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("nudge")
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "toggle" => toggle_reminders(app),
                    "settings" => open_settings(app.clone()),
                    "about" => open_url(REPO_URL),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_reminders(tray.app_handle());
                    }
                })
                .build(&handle)?;

            *app.state::<App>().countdown_item.lock().unwrap() = Some(countdown);
            *app.state::<App>().toggle_item.lock().unwrap() = Some(toggle);
            *app.state::<App>().tray.lock().unwrap() = Some(tray);
            refresh_tray(app.app_handle());
            enable_reminders(app.app_handle());

            {
                let handle = handle.clone();
                std::thread::spawn(move || tick_loop(handle));
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building nudge")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
