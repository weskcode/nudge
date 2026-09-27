// no console window behind the tray app on Windows release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command as ProcCommand;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, Wry};

const CREDITS_URL: &str = "https://github.com/brettferdosi/remindful";

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    interval_secs: u32,
    message: String,
    enabled_on_wake: bool,
    reset_on_wake: bool,
    play_sound: bool,
    launch_at_login: bool,
    count_total: u32,
    #[serde(default)]
    auto_dismiss_secs: u32,
    // customization added after 0.1.0; defaults keep older config files valid
    #[serde(default = "default_true")]
    break_ideas: bool,
    #[serde(default = "default_style")]
    style: String,
    #[serde(default = "default_text_size")]
    text_size: String,
    #[serde(default = "default_true")]
    show_counts: bool,
    #[serde(default = "default_sound")]
    sound: String,
    #[serde(default = "default_snooze")]
    snooze_mins: u32,
    #[serde(default = "default_menu_bar_timer")]
    menu_bar_timer: String,
}

const STYLES: &[&str] = &["frosted", "dusk", "ocean", "forest", "midnight"];
const TEXT_SIZES: &[&str] = &["standard", "large", "xlarge"];
const SOUNDS: &[&str] = &["chime", "glass", "hero", "ping", "purr", "submarine"];
const MENU_BAR_TIMERS: &[&str] = &["never", "last5", "always"];

fn default_true() -> bool {
    true
}
fn default_style() -> String {
    "frosted".into()
}
fn default_text_size() -> String {
    "standard".into()
}
fn default_sound() -> String {
    "chime".into()
}
fn default_snooze() -> u32 {
    5
}
fn default_menu_bar_timer() -> String {
    "never".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_secs: 30 * 60,
            message: "Time to step away".into(),
            enabled_on_wake: true,
            reset_on_wake: true,
            play_sound: true,
            launch_at_login: false,
            count_total: 0,
            auto_dismiss_secs: 0,
            break_ideas: true,
            style: default_style(),
            text_size: default_text_size(),
            show_counts: true,
            sound: default_sound(),
            snooze_mins: default_snooze(),
            menu_bar_timer: default_menu_bar_timer(),
        }
    }
}

#[derive(Serialize, Clone)]
struct PublicState {
    #[serde(flatten)]
    config: Config,
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
    pause_menu: Mutex<Option<Submenu<Wry>>>,
    last_label: Mutex<String>,
    // set while a preview (or a break taken with reminders off) is showing: the
    // schedule to put back on dismissal instead of starting a new interval
    restore_fire: Mutex<Option<Option<Instant>>>,
    shown_at: Mutex<Instant>,
}

// keys typed in the moment a reminder opens were meant for another app;
// letting them dismiss it would close the reminder before anyone sees it
const DISMISS_GRACE: Duration = Duration::from_millis(1500);

fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .map(|d| d.join("nudge"))
        .expect("cannot resolve config directory");
    let _ = fs::create_dir_all(&dir);
    dir.join("config.json")
}

fn load_config() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_config(app: &AppHandle) {
    let state = app.state::<App>();
    let config = state.config.lock().unwrap();
    if let Ok(json) = serde_json::to_string_pretty(&*config) {
        if let Err(error) = fs::write(config_path(), json) {
            println!("saving settings failed: {error:?}");
        }
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
        config,
        count_session: session,
        enabled: is_reminders_enabled(app),
        showing,
        remaining_secs: remaining,
    }
}

fn broadcast(app: &AppHandle) {
    let _ = app.emit("nudge://state", snapshot(app));
}

fn is_reminders_enabled(app: &AppHandle) -> bool {
    let state = app.state::<App>();
    if let Some(restore) = *state.restore_fire.lock().unwrap() {
        return restore.is_some();
    }
    state.next_fire.lock().unwrap().is_some() || *state.showing.lock().unwrap()
}

fn enable_reminders(app: &AppHandle) {
    let state = app.state::<App>();
    *state.restore_fire.lock().unwrap() = None;
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
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
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

// short countdown shown beside the tray icon, per the "menu bar timer" setting
fn menu_bar_title(app: &AppHandle) -> Option<String> {
    let state = app.state::<App>();
    let mode = state.config.lock().unwrap().menu_bar_timer.clone();
    let next = (*state.next_fire.lock().unwrap())?;
    let secs = next.saturating_duration_since(Instant::now()).as_secs();
    let show = match mode.as_str() {
        "always" => true,
        "last5" => secs <= 5 * 60,
        _ => false,
    };
    show.then(|| hms(secs))
}

fn refresh_tray(app: &AppHandle) {
    let state = app.state::<App>();
    let enabled = is_reminders_enabled(app);
    let title = menu_bar_title(app);
    // the title and on/off state are part of the cache key so mode changes and
    // the end of a preview (same label, reminders now off) apply immediately
    let text = format!("{}|{}|{}", enabled, current_label(app), title.clone().unwrap_or_default());

    {
        let mut last = state.last_label.lock().unwrap();
        if *last == text {
            return;
        }
        *last = text.clone();
    }

    // clone the handles and drop the guards first: each setter below blocks
    // until the main thread runs it, and the main thread may be waiting on
    // these same locks inside another refresh_tray call
    let label = current_label(app);
    let countdown = state.countdown_item.lock().unwrap().clone();
    let toggle = state.toggle_item.lock().unwrap().clone();
    let pause = state.pause_menu.lock().unwrap().clone();
    let tray = state.tray.lock().unwrap().clone();

    if let Some(item) = countdown {
        let _ = item.set_text(label.clone());
    }
    if let Some(item) = toggle {
        let _ = item.set_text(if enabled {
            "Turn Reminders Off"
        } else {
            "Turn Reminders On"
        });
    }
    if let Some(menu) = pause {
        // pausing only makes sense while reminders are on
        let _ = menu.set_enabled(enabled);
    }
    if let Some(tray) = tray {
        let _ = tray.set_tooltip(Some(label.clone()));
        let _ = tray.set_title(title.as_deref());
        let icon = if enabled {
            tauri::include_image!("icons/tray-on.png")
        } else {
            tauri::include_image!("icons/tray-off.png")
        };
        let _ = tray.set_icon(Some(icon));
    }
}

fn show_overlay(app: &AppHandle, counted: bool) {
    if *app.state::<App>().showing.lock().unwrap() {
        return;
    }
    let state = app.state::<App>();
    let mut seq = state.overlay_seq.lock().unwrap();
    *seq += 1;
    let seq = *seq;

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

        if cfg!(target_os = "macos") {
            // real behind-window blur (NSVisualEffectView); the page adds only a light tint
            use tauri::window::{Effect, EffectState, EffectsBuilder};
            builder = builder
                .theme(Some(tauri::Theme::Dark))
                .effects(
                    EffectsBuilder::new()
                        .effect(Effect::HudWindow)
                        .state(EffectState::Active)
                        .build(),
                );
        } else {
            builder = builder.fullscreen(true);
        }

        match builder.build() {
            Ok(window) => {
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
            Err(error) => println!("overlay {label} failed to create: {error:?}"),
        }
    }

    // nothing on screen: leave the app out of the "showing" state so callers
    // can roll back
    if opened.is_empty() {
        return;
    }

    // count only reminders that actually appeared
    if counted {
        state.config.lock().unwrap().count_total += 1;
        *state.count_session.lock().unwrap() += 1;
        save_config(app);
    }

    *state.overlay_windows.lock().unwrap() = opened;
    *state.showing.lock().unwrap() = true;
    *state.shown_at.lock().unwrap() = Instant::now();

    #[cfg(target_os = "macos")]
    let _ = app.show();
    // take keyboard focus so "press any key" works and keystrokes stop going
    // to the app underneath; the build-time focus flag is ignored while the
    // app is inactive
    let primary = state.overlay_windows.lock().unwrap().first().cloned();
    if let Some(window) = primary.and_then(|label| app.get_webview_window(&label)) {
        let _ = window.set_focus();
    }
    let sound = {
        let config = state.config.lock().unwrap();
        config.play_sound.then(|| config.sound.clone())
    };
    if let Some(sound) = sound {
        play_sound(&sound);
    }

    // auto-dismiss after a configured delay so the reminder can never be missed
    // forever, e.g. when the global key listener lacks permission
    if let Some(wait) = {
        let secs = state.config.lock().unwrap().auto_dismiss_secs;
        (secs > 0).then(|| Duration::from_secs(secs as u64))
    } {
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(wait);
            let dismiss_handle = handle.clone();
            let _ = handle.run_on_main_thread(move || {
                let reminder_state = dismiss_handle.state::<App>();
                // ignore timers left over from an earlier reminder that was
                // dismissed by hand before this one opened
                let same_reminder = *reminder_state.overlay_seq.lock().unwrap() == seq;
                if same_reminder && *reminder_state.showing.lock().unwrap() {
                    finish_overlay(&dismiss_handle);
                }
            });
        });
    }

    refresh_tray(app);
    broadcast(app);
}

// dismiss the reminder and carry on: restore the schedule a preview interrupted,
// otherwise start a fresh interval
fn finish_overlay(app: &AppHandle) {
    let restore = app.state::<App>().restore_fire.lock().unwrap().take();
    hide_overlay(app);
    match restore {
        Some(fire) => {
            *app.state::<App>().next_fire.lock().unwrap() = fire;
            refresh_tray(app);
            broadcast(app);
        }
        None => enable_reminders(app),
    }
}

fn hide_overlay(app: &AppHandle) {
    let state = app.state::<App>();
    *state.restore_fire.lock().unwrap() = None;
    if *state.showing.lock().unwrap() {
        for label in state.overlay_windows.lock().unwrap().drain(..) {
            if let Some(window) = app.get_webview_window(&label) {
                let _ = window.close();
            }
        }
        *state.showing.lock().unwrap() = false;
        // only hide the whole app when no settings window is open that the user
        // may still be interacting with
        #[cfg(target_os = "macos")]
        if app.get_webview_window("settings").is_none() {
            let _ = app.hide();
        }
    }
}

#[tauri::command]
fn close_overlay(app: AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        // a double click or key repeat can queue this twice; the second one
        // would find nothing to restore and turn reminders on
        if *handle.state::<App>().showing.lock().unwrap() {
            finish_overlay(&handle);
        }
    });
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
        finish_overlay(app);
    }
    if config.enabled_on_wake && state.next_fire.lock().unwrap().is_none() {
        enable_reminders(app);
    } else if is_reminders_enabled(app) && config.reset_on_wake {
        enable_reminders(app);
    }
    refresh_tray(app);
    broadcast(app);
}

// global key listener: dismiss the reminder on any key press, even when some
// other app is frontmost (the overlay webview never gets those key events).
// needs Accessibility/Input Monitoring permission; see listen result comment
fn global_key_listener(app: AppHandle) {
    use rdev::{EventType, Key};
    let result = rdev::listen(move |event| {
        let EventType::KeyPress(key) = event.event_type else {
            return;
        };
        // Tab and modifiers move focus or start shortcuts; they don't dismiss
        if matches!(
            key,
            Key::Tab
                | Key::ShiftLeft
                | Key::ShiftRight
                | Key::ControlLeft
                | Key::ControlRight
                | Key::Alt
                | Key::AltGr
                | Key::MetaLeft
                | Key::MetaRight
                | Key::CapsLock
                | Key::Function
        ) {
            return;
        }
        let handle = app.clone();
        let main_handle = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            let state = main_handle.state::<App>();
            if !*state.showing.lock().unwrap()
                || state.shown_at.lock().unwrap().elapsed() < DISMISS_GRACE
            {
                return;
            }
            // a focused overlay page gets the key itself (and keeps Enter/Space
            // for its buttons); this listener covers keys typed into other apps
            let labels = state.overlay_windows.lock().unwrap().clone();
            let overlay_focused = labels.iter().any(|label| {
                main_handle
                    .get_webview_window(label)
                    .and_then(|window| window.is_focused().ok())
                    .unwrap_or(false)
            });
            if !overlay_focused {
                finish_overlay(&main_handle);
            }
        });
    });
    if let Err(error) = result {
        println!("global key listener unavailable: {error:?}");
    }
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

        // on the main thread, so a menu click (pause, turn off) can't land
        // between the due check and the reminder, and a tray refresh started
        // here can't finish after one the click triggered
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            let state = handle.state::<App>();
            let due = matches!(*state.next_fire.lock().unwrap(), Some(fire) if fire <= Instant::now());
            if due {
                *state.next_fire.lock().unwrap() = None;
                show_overlay(&handle, true);
                if !*state.showing.lock().unwrap() {
                    // no window could open; try again after the next interval
                    enable_reminders(&handle);
                }
            } else {
                refresh_tray(&handle);
            }
        });
    }
}

const CHIME: &[u8] = include_bytes!("assets/chime.wav");

fn play_sound(name: &str) {
    // macOS ships the other sounds; everywhere else they fall back to the chime
    #[cfg(target_os = "macos")]
    if name != "chime" && SOUNDS.contains(&name) {
        let mut chars = name.chars();
        let file = match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => return,
        };
        let path = format!("/System/Library/Sounds/{file}.aiff");
        if std::path::Path::new(&path).exists() {
            let _ = ProcCommand::new("afplay").arg(path).spawn();
            return;
        }
    }
    let _ = name;
    let path = std::env::temp_dir().join("nudge-chime.wav");
    if let Ok(mut file) = std::fs::File::create(&path) {
        use std::io::Write;
        let _ = file.write_all(CHIME);
        let path_string = path.display().to_string();
        let result = if cfg!(target_os = "macos") {
            ProcCommand::new("afplay").arg(&path_string).spawn()
        } else if cfg!(target_os = "windows") {
            quiet(ProcCommand::new("powershell"))
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "(New-Object Media.SoundPlayer '{}').PlaySync()",
                        path_string.replace('\'', "''")
                    ),
                ])
                .spawn()
        } else {
            ProcCommand::new("paplay").arg(&path_string).spawn().or_else(|_| {
                ProcCommand::new("aplay").arg(&path_string).spawn()
            })
        };
        let _ = result;
    }
}

// hide any reminder and schedule the next one `minutes` from now
fn remind_in(app: &AppHandle, minutes: u32) {
    let mins = minutes.clamp(1, 240);
    hide_overlay(app);
    let state = app.state::<App>();
    *state.next_fire.lock().unwrap() = Some(Instant::now() + Duration::from_secs(mins as u64 * 60));
    refresh_tray(app);
    broadcast(app);
}

#[tauri::command]
fn snooze(app: AppHandle, minutes: u32) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if !*handle.state::<App>().showing.lock().unwrap() {
            return;
        }
        let previewing = handle.state::<App>().restore_fire.lock().unwrap().is_some();
        if previewing {
            finish_overlay(&handle);
        } else {
            remind_in(&handle, minutes);
        }
    });
}

// async: a sync command runs inside the IPC callback on the main thread, where
// building the overlay windows deadlocks on Windows
#[tauri::command]
async fn preview_reminder(app: AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let state = handle.state::<App>();
        if *state.showing.lock().unwrap() {
            return;
        }
        let current = *state.next_fire.lock().unwrap();
        show_overlay(&handle, false);
        if *state.showing.lock().unwrap() {
            *state.restore_fire.lock().unwrap() = Some(current);
            refresh_tray(&handle);
            broadcast(&handle);
        }
    });
}

#[tauri::command]
fn preview_sound(sound: String) {
    if SOUNDS.contains(&sound.as_str()) {
        play_sound(&sound);
    }
}

// the release build is a GUI-subsystem process on Windows; without this flag
// every helper it starts flashes a console window and can steal focus
#[allow(unused_mut)]
fn quiet(mut command: ProcCommand) -> ProcCommand {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

fn open_url(url: &str) {
    let result = if cfg!(target_os = "macos") {
        ProcCommand::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        quiet(ProcCommand::new("cmd")).args(["/c", "start", "", url]).spawn()
    } else {
        ProcCommand::new("xdg-open").arg(url).spawn()
    };
    let _ = result;
}

// fixed destination only: the page never passes a URL to open
#[tauri::command]
fn open_credits() {
    open_url(CREDITS_URL);
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
    play_sound: Option<bool>,
    launch_at_login: Option<bool>,
    auto_dismiss_secs: Option<u32>,
    break_ideas: Option<bool>,
    style: Option<String>,
    text_size: Option<String>,
    show_counts: Option<bool>,
    sound: Option<String>,
    snooze_mins: Option<u32>,
    menu_bar_timer: Option<String>,
) {
    let state = app.state::<App>();
    if interval_secs.unwrap_or(0) > 0 {
        let interval = interval_secs.unwrap().max(5 * 60);
        let mut config = state.config.lock().unwrap();
        config.interval_secs = interval;
        if is_reminders_enabled(&app) {
            let fire = Some(Instant::now() + Duration::from_secs(interval as u64));
            let mut restore = state.restore_fire.lock().unwrap();
            if restore.is_some() {
                *restore = Some(fire);
            } else {
                *state.next_fire.lock().unwrap() = fire;
            }
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
    if let Some(value) = play_sound {
        state.config.lock().unwrap().play_sound = value;
    }
    if let Some(value) = auto_dismiss_secs {
        state.config.lock().unwrap().auto_dismiss_secs = value.min(600);
    }
    {
        let mut config = state.config.lock().unwrap();
        if let Some(value) = break_ideas {
            config.break_ideas = value;
        }
        if let Some(value) = show_counts {
            config.show_counts = value;
        }
        if let Some(value) = style.filter(|v| STYLES.contains(&v.as_str())) {
            config.style = value;
        }
        if let Some(value) = text_size.filter(|v| TEXT_SIZES.contains(&v.as_str())) {
            config.text_size = value;
        }
        if let Some(value) = sound.filter(|v| SOUNDS.contains(&v.as_str())) {
            config.sound = value;
        }
        if let Some(value) = snooze_mins {
            config.snooze_mins = value.min(30);
        }
        if let Some(value) = menu_bar_timer.filter(|v| MENU_BAR_TIMERS.contains(&v.as_str())) {
            config.menu_bar_timer = value;
        }
    }
    if let Some(enable) = launch_at_login {
        use tauri_plugin_autostart::ManagerExt;
        let autostart = app.autolaunch();
        let result = if enable {
            autostart.enable()
        } else {
            autostart.disable()
        };
        match result {
            Ok(()) => state.config.lock().unwrap().launch_at_login = enable,
            Err(error) => println!("launch at login change failed: {error:?}"),
        }
    }
    save_config(&app);
    refresh_tray(&app);
    broadcast(&app);
}

#[tauri::command]
fn set_enabled(app: AppHandle, enabled: bool) {
    if enabled != is_reminders_enabled(&app) {
        toggle_reminders(&app);
    }
}

#[tauri::command]
fn reset_counters(app: AppHandle) {
    let state = app.state::<App>();
    state.config.lock().unwrap().count_total = 0;
    *state.count_session.lock().unwrap() = 0;
    save_config(&app);
    broadcast(&app);
}

const ABOUT_CREDITS: &str =
    "Inspired by remindful by Brett Gutstein\nhttps://github.com/brettferdosi/remindful";

// the standard About panel, in front: like Settings, it would otherwise open
// behind the frontmost app. name, version and icon come from the bundle
#[cfg(target_os = "macos")]
fn show_about(app: &AppHandle) {
    use cocoa::base::{id, nil, YES};
    use cocoa::foundation::NSString;
    use objc::{class, msg_send, sel, sel_impl};
    let _ = app.show();
    unsafe {
        let ns_app: id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![ns_app, activateIgnoringOtherApps: YES];
        // credits in the small secondary system style the panel uses elsewhere;
        // a bare attributed string falls back to 12pt Helvetica in black
        let size: f64 = msg_send![class!(NSFont), smallSystemFontSize];
        let font: id = msg_send![class!(NSFont), systemFontOfSize: size];
        let color: id = msg_send![class!(NSColor), secondaryLabelColor];
        let paragraph: id = msg_send![class!(NSMutableParagraphStyle), new];
        // NSTextAlignmentCenter is 1 on Apple silicon and 2 on Intel
        let center: i64 = if cfg!(target_arch = "x86_64") { 2 } else { 1 };
        let _: () = msg_send![paragraph, setAlignment: center];
        let attr_keys = [
            NSString::alloc(nil).init_str("NSFont"),
            NSString::alloc(nil).init_str("NSColor"),
            NSString::alloc(nil).init_str("NSParagraphStyle"),
        ];
        let attr_values = [font, color, paragraph];
        let attrs: id = msg_send![class!(NSDictionary), dictionaryWithObjects: attr_values.as_ptr() forKeys: attr_keys.as_ptr() count: 3usize];
        let text = NSString::alloc(nil).init_str(ABOUT_CREDITS);
        let credits: id = msg_send![class!(NSAttributedString), alloc];
        let credits: id = msg_send![credits, initWithString: text attributes: attrs];
        // an empty build version drops the repeated "(0.1.0)" after the version
        let option_keys = [
            NSString::alloc(nil).init_str("Credits"),
            NSString::alloc(nil).init_str("Version"),
        ];
        let option_values = [credits, NSString::alloc(nil).init_str("")];
        let options: id = msg_send![class!(NSDictionary), dictionaryWithObjects: option_values.as_ptr() forKeys: option_keys.as_ptr() count: 2usize];
        let _: () = msg_send![ns_app, orderFrontStandardAboutPanelWithOptions: options];
    }
}

#[tauri::command]
fn open_settings(app: AppHandle) {
    // the app is hidden after each reminder and a menu bar app is never the
    // active one; unhide and focus, or Settings opens behind the frontmost app
    #[cfg(target_os = "macos")]
    let _ = app.show();
    let window = app.get_webview_window("settings").or_else(|| {
        let builder = WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("index.html".into()))
            .title("Nudge")
            .inner_size(780.0, 580.0)
            .min_inner_size(700.0, 460.0)
            .center()
            .resizable(true)
            .maximizable(false)
            .minimizable(false);
        // one surface from the title bar down, with the desktop tinting through
        // the window material like the System Settings sidebar; it turns opaque
        // when the window is inactive or Reduce Transparency is on. the page
        // draws under the title bar, shows the section title and supplies the
        // drag strips (index.html)
        #[cfg(target_os = "macos")]
        let builder = {
            use tauri::window::{Effect, EffectState, EffectsBuilder};
            builder
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .hidden_title(true)
                // inside the floating sidebar, level with the section title
                .traffic_light_position(tauri::LogicalPosition::new(22.0, 24.0))
                .transparent(true)
                .effects(
                    EffectsBuilder::new()
                        .effect(Effect::Sidebar)
                        .state(EffectState::FollowsWindowActiveState)
                        .build(),
                )
        };
        builder.build().ok()
    });
    if let Some(window) = window {
        let _ = window.set_focus();
    }
}


fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
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
            pause_menu: Mutex::new(None),
            last_label: Mutex::new(String::new()),
            restore_fire: Mutex::new(None),
            shown_at: Mutex::new(Instant::now()),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_config,
            close_overlay,
            snooze,
            set_enabled,
            preview_reminder,
            preview_sound,
            reset_counters,
            open_settings,
            open_credits
        ])
        .on_window_event(|window, event| {
            // keep the first overlay window key-focused while a reminder is showing, so
            // that "any key to dismiss" keeps working without fighting the other overlays
            if matches!(event, tauri::WindowEvent::Focused(false))
                && window.label().starts_with("overlay-")
                && *window.app_handle().state::<App>().showing.lock().unwrap()
            {
                let handle = window.app_handle().clone();
                let focus_handle = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    let primary = focus_handle
                        .state::<App>()
                        .overlay_windows
                        .lock()
                        .unwrap()
                        .first()
                        .cloned()
                        .unwrap_or_default();
                    if let Some(w) = focus_handle.get_webview_window(&primary) {
                        let _ = w.set_focus();
                    }
                });
            }
        })
        .setup(|app| {
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let mut config = load_config();
            // the login item can be removed outside the app (System Settings),
            // so trust the OS over the saved flag
            {
                use tauri_plugin_autostart::ManagerExt;
                if let Ok(enabled) = handle.autolaunch().is_enabled() {
                    config.launch_at_login = enabled;
                }
            }
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
                "Turn Reminders Off",
                true,
                None::<&str>,
            )?;
            let break_now =
                MenuItem::with_id(&handle, "break-now", "Take a Break Now", true, None::<&str>)?;
            let pause = Submenu::with_items(
                &handle,
                "Pause Reminders",
                true,
                &[
                    &MenuItem::with_id(&handle, "pause-30", "For 30 Minutes", true, None::<&str>)?,
                    &MenuItem::with_id(&handle, "pause-60", "For 1 Hour", true, None::<&str>)?,
                    &MenuItem::with_id(&handle, "pause-120", "For 2 Hours", true, None::<&str>)?,
                ],
            )?;
            let settings = MenuItem::with_id(&handle, "settings", "Settings…", true, None::<&str>)?;
            // macOS: a plain item, so the app can be brought forward before the
            // panel opens (see show_about)
            #[cfg(target_os = "macos")]
            let about = MenuItem::with_id(&handle, "about", "About Nudge", true, None::<&str>)?;
            #[cfg(not(target_os = "macos"))]
            let about = PredefinedMenuItem::about(
                &handle,
                Some("About Nudge"),
                Some(tauri::menu::AboutMetadata {
                    name: Some("Nudge".into()),
                    version: Some(env!("CARGO_PKG_VERSION").into()),
                    credits: Some(ABOUT_CREDITS.into()),
                    icon: Some(tauri::include_image!("icons/128x128@2x.png")),
                    ..Default::default()
                }),
            )?;
            let quit = MenuItem::with_id(&handle, "quit", "Quit Nudge", true, None::<&str>)?;

            let menu = Menu::with_items(
                &handle,
                &[
                    &countdown,
                    &PredefinedMenuItem::separator(&handle)?,
                    &break_now,
                    &pause,
                    &toggle,
                    &PredefinedMenuItem::separator(&handle)?,
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
                .show_menu_on_left_click(true)
                .tooltip("Nudge")
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "toggle" => toggle_reminders(app),
                    "break-now" => {
                        let state = app.state::<App>();
                        if !*state.showing.lock().unwrap() {
                            let was_enabled = is_reminders_enabled(app);
                            let previous = state.next_fire.lock().unwrap().take();
                            show_overlay(app, true);
                            if !*state.showing.lock().unwrap() {
                                *state.next_fire.lock().unwrap() = previous;
                            } else if !was_enabled {
                                *state.restore_fire.lock().unwrap() = Some(None);
                                refresh_tray(app);
                                broadcast(app);
                            }
                        }
                    }
                    "pause-30" | "pause-60" | "pause-120" if is_reminders_enabled(app) => {
                        let minutes = event.id().as_ref()[6..].parse().unwrap_or(60);
                        remind_in(app, minutes);
                    }
                    "settings" => open_settings(app.clone()),
                    #[cfg(target_os = "macos")]
                    "about" => show_about(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(&handle)?;

            *app.state::<App>().countdown_item.lock().unwrap() = Some(countdown);
            *app.state::<App>().toggle_item.lock().unwrap() = Some(toggle);
            *app.state::<App>().tray.lock().unwrap() = Some(tray);
            *app.state::<App>().pause_menu.lock().unwrap() = Some(pause);
            refresh_tray(app.app_handle());
            enable_reminders(app.app_handle());

            {
                let handle = handle.clone();
                std::thread::spawn(move || tick_loop(handle));
            }

            #[cfg(target_os = "macos")]
            {
                let key_handle = handle.clone();
                std::thread::spawn(move || global_key_listener(key_handle));
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

#[cfg(test)]
mod tests {
    use super::{hms, Config, PublicState};

    #[test]
    fn config_from_0_1_0_loads_with_new_defaults() {
        // a config.json written by 0.1.0, before any customization options existed
        let old = r#"{"interval_secs":2700,"message":"stretch","enabled_on_wake":false,
            "reset_on_wake":true,"play_sound":false,"launch_at_login":true,"count_total":52}"#;
        let config: Config = serde_json::from_str(old).expect("old config must still load");
        assert_eq!(config.interval_secs, 2700);
        assert_eq!(config.message, "stretch");
        assert_eq!(config.count_total, 52);
        assert!(!config.play_sound);
        assert_eq!(config.auto_dismiss_secs, 0);
        assert!(config.break_ideas);
        assert_eq!(config.style, "frosted");
        assert_eq!(config.text_size, "standard");
        assert!(config.show_counts);
        assert_eq!(config.sound, "chime");
        assert_eq!(config.snooze_mins, 5);
        assert_eq!(config.menu_bar_timer, "never");
    }

    #[test]
    fn public_state_keeps_flat_field_names_for_the_pages() {
        let state = PublicState {
            config: Config::default(),
            count_session: 3,
            enabled: true,
            showing: false,
            remaining_secs: 42,
        };
        let json = serde_json::to_value(state).unwrap();
        for key in [
            "interval_secs", "message", "play_sound", "auto_dismiss_secs", "style",
            "snooze_mins", "menu_bar_timer", "count_session", "enabled", "remaining_secs",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        assert_eq!(json["remaining_secs"], 42);
    }

    #[test]
    fn countdown_hides_hours_under_an_hour() {
        assert_eq!(hms(0), "0:00");
        assert_eq!(hms(59), "0:59");
        assert_eq!(hms(30 * 60), "30:00");
        assert_eq!(hms(3599), "59:59");
    }

    #[test]
    fn countdown_shows_hours_from_an_hour() {
        assert_eq!(hms(3600), "1:00:00");
        assert_eq!(hms(90 * 60 + 5), "1:30:05");
    }
}
