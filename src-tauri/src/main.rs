// no console window behind the tray app on Windows release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod key_tap;
mod schedule;

use schedule::{hms, Schedule, Showing};
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

// one reminder: what it says, how often, and how it looks. before there could
// be several, these fields sat at the top of config.json; any a file lacks
// take the defaults the single reminder had
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
struct Nudge {
    id: u32,
    enabled: bool,
    message: String,
    symbol: String,
    interval_secs: u32,
    play_sound: bool,
    auto_dismiss_secs: u32,
    break_ideas: bool,
    style: String,
    text_size: String,
    show_counts: bool,
    sound: String,
    snooze_mins: u32,
    layout: String,
}

impl Default for Nudge {
    fn default() -> Self {
        Self {
            id: 1,
            enabled: true,
            message: "Time to step away".into(),
            symbol: "figure".into(),
            interval_secs: 30 * 60,
            play_sound: true,
            auto_dismiss_secs: 0,
            break_ideas: true,
            style: "frosted".into(),
            text_size: "standard".into(),
            show_counts: true,
            sound: "chime".into(),
            snooze_mins: 5,
            layout: "classic".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Config {
    enabled_on_wake: bool,
    reset_on_wake: bool,
    launch_at_login: bool,
    count_total: u32,
    #[serde(default = "default_menu_bar_timer")]
    menu_bar_timer: String,
    #[serde(default)]
    nudges: Vec<Nudge>,
}

const STYLES: &[&str] = &["frosted", "dusk", "ocean", "forest", "midnight"];
const TEXT_SIZES: &[&str] = &["standard", "large", "xlarge"];
const SOUNDS: &[&str] = &["chime", "glass", "hero", "ping", "purr", "submarine"];
const MENU_BAR_TIMERS: &[&str] = &["never", "last5", "always"];
const LAYOUTS: &[&str] = &["classic", "card", "ring"];
const SYMBOLS: &[&str] = &["figure", "drop", "eye", "breath", "pill", "bell"];
const MAX_NUDGES: usize = 8;

// a nudge due this soon comes along with a reminder that is opening, rather
// than following it a minute later
const JOIN_WINDOW: Duration = Duration::from_secs(60);

fn default_menu_bar_timer() -> String {
    "never".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled_on_wake: true,
            reset_on_wake: true,
            launch_at_login: false,
            count_total: 0,
            menu_bar_timer: default_menu_bar_timer(),
            nudges: vec![Nudge::default()],
        }
    }
}

// what the Settings pages show: the global options, then each nudge with the
// seconds until it fires (0 while on screen, -1 with no timer)
#[derive(Serialize, Clone)]
struct PublicState {
    enabled_on_wake: bool,
    reset_on_wake: bool,
    launch_at_login: bool,
    menu_bar_timer: String,
    count_total: u32,
    count_session: u32,
    enabled: bool,
    showing: bool,
    nudges: Vec<NudgeState>,
}

#[derive(Serialize, Clone)]
struct NudgeState {
    #[serde(flatten)]
    nudge: Nudge,
    remaining_secs: i64,
}

// what a reminder window shows: the look of the nudge it opened for, and a
// line for each other nudge it answers
#[derive(Serialize, Clone)]
struct OverlayState {
    #[serde(flatten)]
    look: Nudge,
    also: Vec<AlsoNow>,
    count_total: u32,
    count_session: u32,
}

#[derive(Serialize, Clone)]
struct AlsoNow {
    message: String,
    symbol: String,
}

struct App {
    config: Mutex<Config>,
    // take after config, and never hold across a window, menu or tray call:
    // those run on the main thread, where the overlay focus handler takes it
    schedule: Mutex<Schedule>,
    count_session: Mutex<u32>,
    menu: Mutex<Option<Menu<Wry>>>,
    countdown_items: Mutex<Vec<MenuItem<Wry>>>,
    toggle_item: Mutex<Option<MenuItem<Wry>>>,
    tray: Mutex<Option<TrayIcon<Wry>>>,
    pause_menu: Mutex<Option<Submenu<Wry>>>,
    last_label: Mutex<String>,
}

// keys typed in the moment a reminder opens were meant for another app;
// letting them dismiss it would close the reminder before anyone sees it
#[cfg(target_os = "macos")]
const DISMISS_GRACE: Duration = Duration::from_millis(1500);

fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .map(|d| d.join("nudge"))
        .expect("cannot resolve config directory");
    let _ = fs::create_dir_all(&dir);
    dir.join("config.json")
}

// config.json from any version: before there could be several nudges, the one
// reminder's settings sat at the top level, and they become the first nudge
fn parse_config(text: &str) -> Option<Config> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut config: Config = serde_json::from_value(value.clone()).ok()?;
    if config.nudges.is_empty() {
        let mut first: Nudge = serde_json::from_value(value).ok()?;
        first.id = 1;
        config.nudges.push(first);
    }
    // settings and delete pick a nudge by id, so a repeated id (or a missing
    // one, which reads as 1) in a hand-edited file would change or delete
    // several at once; later repeats get fresh ids
    let mut seen = std::collections::HashSet::new();
    for index in 0..config.nudges.len() {
        if !seen.insert(config.nudges[index].id) {
            let id = new_id(&config.nudges);
            config.nudges[index].id = id;
            seen.insert(id);
        }
    }
    // a lone nudge has no switch of its own; the master switch covers it
    if let [only] = config.nudges.as_mut_slice() {
        only.enabled = true;
    }
    Some(config)
}

// one above the highest id, or the lowest free id when that would pass
// u32::MAX (only a hand-edited file gets that high)
fn new_id(nudges: &[Nudge]) -> u32 {
    let taken = |id: u32| nudges.iter().any(|n| n.id == id);
    let highest = nudges.iter().map(|n| n.id).max().unwrap_or(0);
    highest
        .checked_add(1)
        .unwrap_or_else(|| (1..=u32::MAX).find(|&id| !taken(id)).unwrap_or(0))
}

// the first nudge is also written at the top level, where older builds look,
// so going back to one keeps the main reminder (it drops the rest if it saves)
fn config_json(config: &Config) -> serde_json::Value {
    let mut value = serde_json::to_value(config).unwrap_or_default();
    let first = config.nudges.first().and_then(|n| serde_json::to_value(n).ok());
    if let (Some(serde_json::Value::Object(fields)), Some(top)) = (first, value.as_object_mut()) {
        for (key, field) in fields {
            if key != "id" {
                top.insert(key, field);
            }
        }
    }
    value
}

fn load_config() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|text| parse_config(&text))
        .unwrap_or_default()
}

fn save_config(app: &AppHandle) {
    let json = config_json(&app.state::<App>().config.lock().unwrap());
    if let Ok(text) = serde_json::to_string_pretty(&json) {
        if let Err(error) = replace_file(&config_path(), &text) {
            println!("saving settings failed: {error:?}");
        }
    }
}

// write beside the file, then rename over it, so a write that fails part way
// (a full disk, say) leaves the old settings whole rather than a cut-off file
fn replace_file(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let temp = path.with_extension("json.tmp");
    let result = fs::write(&temp, text).and_then(|_| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn nudges(app: &AppHandle) -> Vec<Nudge> {
    app.state::<App>().config.lock().unwrap().nudges.clone()
}

// each enabled nudge with its interval, for starting timers
fn enabled_timers(nudges: &[Nudge]) -> Vec<(u32, Duration)> {
    nudges
        .iter()
        .filter(|n| n.enabled)
        .map(|n| (n.id, Duration::from_secs(n.interval_secs as u64)))
        .collect()
}

fn snapshot(app: &AppHandle) -> PublicState {
    let state = app.state::<App>();
    let config = state.config.lock().unwrap().clone();
    let count_session = *state.count_session.lock().unwrap();
    let schedule = state.schedule.lock().unwrap();
    let now = Instant::now();
    PublicState {
        nudges: config
            .nudges
            .into_iter()
            .map(|nudge| NudgeState {
                remaining_secs: schedule.remaining(nudge.id, now),
                nudge,
            })
            .collect(),
        enabled_on_wake: config.enabled_on_wake,
        reset_on_wake: config.reset_on_wake,
        launch_at_login: config.launch_at_login,
        menu_bar_timer: config.menu_bar_timer,
        count_total: config.count_total,
        count_session,
        enabled: schedule.on,
        showing: schedule.showing.is_some(),
    }
}

fn overlay_state(app: &AppHandle) -> OverlayState {
    let state = app.state::<App>();
    let config = state.config.lock().unwrap().clone();
    let count_session = *state.count_session.lock().unwrap();
    let (look, due) = state
        .schedule
        .lock()
        .unwrap()
        .showing
        .as_ref()
        .map(|s| (s.look, s.due.clone()))
        .unwrap_or_default();
    let find = |id: u32| config.nudges.iter().find(|n| n.id == id);
    let also = due
        .iter()
        .filter(|&&id| id != look)
        .filter_map(|&id| find(id))
        .map(|n| AlsoNow {
            message: n.message.clone(),
            symbol: n.symbol.clone(),
        })
        .collect();
    OverlayState {
        // a nudge deleted while its reminder is up falls back to the first
        look: find(look).or(config.nudges.first()).cloned().unwrap_or_default(),
        also,
        count_total: config.count_total,
        count_session,
    }
}

fn broadcast(app: &AppHandle) {
    let _ = app.emit("nudge://state", snapshot(app));
}

fn is_on(app: &AppHandle) -> bool {
    app.state::<App>().schedule.lock().unwrap().on
}

// the master switch: on starts every enabled nudge's timer over, off stops
// them all and takes down any reminder
fn set_reminders(app: &AppHandle, on: bool) {
    if !on {
        close_windows(app);
    }
    let timers = enabled_timers(&nudges(app));
    {
        let state = app.state::<App>();
        let mut schedule = state.schedule.lock().unwrap();
        schedule.on = on;
        schedule.restart(&timers, Instant::now());
    }
    refresh_tray(app);
    broadcast(app);
}

fn toggle_reminders(app: &AppHandle) {
    set_reminders(app, !is_on(app));
}

// short countdown shown beside the tray icon, per the "menu bar timer"
// setting: to the soonest nudge, and hidden while a reminder is showing
fn menu_bar_title(mode: &str, schedule: &Schedule, now: Instant) -> Option<String> {
    if schedule.showing.is_some() {
        return None;
    }
    let secs = schedule.soonest()?.saturating_duration_since(now).as_secs();
    let show = match mode {
        "always" => true,
        "last5" => secs <= 5 * 60,
        _ => false,
    };
    show.then(|| hms(secs))
}

fn refresh_tray(app: &AppHandle) {
    let state = app.state::<App>();
    let (mode, messages) = {
        let config = state.config.lock().unwrap();
        let messages: Vec<(u32, String)> =
            config.nudges.iter().map(|n| (n.id, n.message.clone())).collect();
        (config.menu_bar_timer.clone(), messages)
    };
    let names: Vec<(u32, &str)> = messages.iter().map(|(id, m)| (*id, m.as_str())).collect();
    let now = Instant::now();
    let (enabled, lines, title) = {
        let schedule = state.schedule.lock().unwrap();
        let title = menu_bar_title(&mode, &schedule, now);
        (schedule.on, schedule.menu_lines(&names, now), title)
    };
    // the title and on/off state are part of the cache key so mode changes and
    // the end of a preview (same label, reminders now off) apply immediately
    let text = format!("{}|{}|{}", enabled, lines.join("\n"), title.clone().unwrap_or_default());

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
    let menu = state.menu.lock().unwrap().clone();
    let items = state.countdown_items.lock().unwrap().clone();
    let toggle = state.toggle_item.lock().unwrap().clone();
    let pause = state.pause_menu.lock().unwrap().clone();
    let tray = state.tray.lock().unwrap().clone();

    let items = match menu {
        Some(menu) => sync_countdown_items(app, &menu, items, lines.len()),
        None => items,
    };
    for (item, line) in items.iter().zip(&lines) {
        let _ = item.set_text(line);
    }
    *state.countdown_items.lock().unwrap() = items;
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
        let _ = tray.set_tooltip(lines.first());
        // an empty title, not None: on macOS None leaves the last countdown up
        let _ = tray.set_title(Some(title.as_deref().unwrap_or("")));
        let icon = if enabled {
            tauri::include_image!("icons/tray-on.png")
        } else {
            tauri::include_image!("icons/tray-off.png")
        };
        let _ = tray.set_icon(Some(icon));
    }
}

// one disabled countdown line per nudge at the top of the menu, added and
// removed as the count changes; there is always at least one
fn sync_countdown_items(
    app: &AppHandle,
    menu: &Menu<Wry>,
    mut items: Vec<MenuItem<Wry>>,
    count: usize,
) -> Vec<MenuItem<Wry>> {
    while items.len() < count {
        let id = format!("countdown-{}", items.len());
        let Ok(item) = MenuItem::with_id(app, id, "", false, None::<&str>) else {
            break;
        };
        if menu.insert(&item, items.len()).is_err() {
            break;
        }
        items.push(item);
    }
    while items.len() > count.max(1) {
        if let Some(item) = items.pop() {
            let _ = menu.remove(&item);
        }
    }
    items
}

// cover every screen with the reminder for `look`, answering the nudges in
// `due` (none for a preview); false when no window could open
fn show_overlay(app: &AppHandle, look: u32, due: Vec<u32>, counted: bool) -> bool {
    let state = app.state::<App>();
    let seq = {
        let mut schedule = state.schedule.lock().unwrap();
        if schedule.showing.is_some() {
            return false;
        }
        schedule.next_seq()
    };

    let monitors = match app.available_monitors() {
        Ok(monitors) if !monitors.is_empty() => monitors,
        _ => match app.primary_monitor() {
            Ok(Some(monitor)) => vec![monitor],
            _ => return false,
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

    // nothing on screen: leave the schedule as it was so callers can roll back
    if opened.is_empty() {
        return false;
    }

    // count only reminders that actually appeared
    if counted {
        state.config.lock().unwrap().count_total += 1;
        *state.count_session.lock().unwrap() += 1;
        save_config(app);
    }

    let primary = opened.first().cloned();
    state.schedule.lock().unwrap().open(Showing {
        look,
        due,
        seq,
        windows: opened,
        shown_at: Instant::now(),
    });

    #[cfg(target_os = "macos")]
    let _ = app.show();
    // take keyboard focus so "press any key" works and keystrokes stop going
    // to the app underneath; the build-time focus flag is ignored while the
    // app is inactive
    if let Some(window) = primary.and_then(|label| app.get_webview_window(&label)) {
        let _ = window.set_focus();
    }
    // the sound and break length are the look nudge's
    let (sound, break_secs) = {
        let config = state.config.lock().unwrap();
        let nudge = config.nudges.iter().find(|n| n.id == look).or(config.nudges.first());
        (
            nudge.and_then(|n| n.play_sound.then(|| n.sound.clone())),
            nudge.map_or(0, |n| n.auto_dismiss_secs),
        )
    };
    if let Some(sound) = sound {
        play_sound(&sound);
    }

    // auto-dismiss after a configured delay so the reminder can never be missed
    // forever, e.g. when the global key listener lacks permission
    if break_secs > 0 {
        let wait = Duration::from_secs(break_secs as u64);
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(wait);
            let dismiss_handle = handle.clone();
            let _ = handle.run_on_main_thread(move || {
                // ignore timers left over from an earlier reminder that was
                // dismissed by hand before this one opened
                let same_reminder = dismiss_handle
                    .state::<App>()
                    .schedule
                    .lock()
                    .unwrap()
                    .showing
                    .as_ref()
                    .is_some_and(|s| s.seq == seq);
                if same_reminder {
                    finish_overlay(&dismiss_handle, None);
                }
            });
        });
    }

    refresh_tray(app);
    broadcast(app);
    true
}

// take the reminder off every screen; the caller decides what its nudges do next
fn close_windows(app: &AppHandle) -> Option<Showing> {
    let shown = app.state::<App>().schedule.lock().unwrap().close()?;
    for label in &shown.windows {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.close();
        }
    }
    // only hide the whole app when no settings window is open that the user
    // may still be interacting with
    #[cfg(target_os = "macos")]
    if app.get_webview_window("settings").is_none() {
        let _ = app.hide();
    }
    Some(shown)
}

// dismiss the reminder and carry on: the nudges it answered start over, a
// full interval from now or, when snoozed, a few minutes
fn finish_overlay(app: &AppHandle, snooze_mins: Option<u32>) {
    let Some(shown) = close_windows(app) else {
        return;
    };
    let snooze = snooze_mins.map(|minutes| Duration::from_secs(minutes.clamp(1, 240) as u64 * 60));
    let timers: Vec<_> = enabled_timers(&nudges(app))
        .into_iter()
        .filter(|(id, _)| shown.due.contains(id))
        .map(|(id, interval)| (id, snooze.unwrap_or(interval)))
        .collect();
    app.state::<App>()
        .schedule
        .lock()
        .unwrap()
        .resume(&timers, Instant::now());
    refresh_tray(app);
    broadcast(app);
}

#[tauri::command]
fn close_overlay(app: AppHandle) {
    let handle = app.clone();
    // a double click or key repeat can queue this twice; the second one finds
    // nothing on screen and does nothing
    let _ = app.run_on_main_thread(move || finish_overlay(&handle, None));
}

fn on_wake(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || on_wake_main(&handle));
}

fn on_wake_main(app: &AppHandle) {
    let state = app.state::<App>();
    let (enabled_on_wake, reset_on_wake) = {
        let config = state.config.lock().unwrap();
        (config.enabled_on_wake, config.reset_on_wake)
    };

    *state.count_session.lock().unwrap() = 0;

    finish_overlay(app, None);
    let on = is_on(app);
    if (enabled_on_wake && !on) || (on && reset_on_wake) {
        set_reminders(app, true);
    }
    refresh_tray(app);
    broadcast(app);
}

// global key listener: dismiss the reminder on any key press, even when some
// other app is frontmost (the overlay webview never gets those key events).
// needs Input Monitoring permission; without it the reminder still closes on
// a click or a key typed into it
#[cfg(target_os = "macos")]
fn global_key_listener(app: AppHandle) {
    // modifiers never arrive as key presses; Tab moves focus between the
    // reminder's buttons (and is half of Cmd-Tab), so it doesn't dismiss
    let result = key_tap::listen(move |keycode| {
        if keycode == key_tap::TAB {
            return;
        }
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            let labels = {
                let state = handle.state::<App>();
                let schedule = state.schedule.lock().unwrap();
                match schedule.showing.as_ref() {
                    Some(shown) if shown.shown_at.elapsed() >= DISMISS_GRACE => shown.windows.clone(),
                    _ => return,
                }
            };
            // a focused overlay page gets the key itself (and keeps Enter/Space
            // for its buttons); this listener covers keys typed into other apps
            let overlay_focused = labels.iter().any(|label| {
                handle
                    .get_webview_window(label)
                    .and_then(|window| window.is_focused().ok())
                    .unwrap_or(false)
            });
            if !overlay_focused {
                finish_overlay(&handle, None);
            }
        });
    });
    if let Err(error) = result {
        println!("global key listener unavailable: {error}");
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
        let _ = app.run_on_main_thread(move || tick(&handle));
    }
}

fn tick(app: &AppHandle) {
    let now = Instant::now();
    let (joined, due) = {
        let state = app.state::<App>();
        let mut schedule = state.schedule.lock().unwrap();
        if schedule.showing.is_some() {
            let joined = schedule.due_now(now, Duration::ZERO, false);
            schedule.join(&joined);
            (joined, Vec::new())
        } else {
            (Vec::new(), schedule.due_now(now, JOIN_WINDOW, false))
        }
    };
    if !joined.is_empty() {
        // the reminder on screen gains a line for each, without a second
        // sound or count
        let _ = app.emit("nudge://overlay", overlay_state(app));
        refresh_tray(app);
        broadcast(app);
    } else if let Some(&look) = due.first() {
        if !show_overlay(app, look, due.clone(), true) {
            // no window could open; try again after the next interval
            let timers: Vec<_> = enabled_timers(&nudges(app))
                .into_iter()
                .filter(|(id, _)| due.contains(id))
                .collect();
            app.state::<App>().schedule.lock().unwrap().resume(&timers, now);
            refresh_tray(app);
            broadcast(app);
        }
    } else {
        refresh_tray(app);
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


// Take a Break Now: the next nudge due, early. with reminders off (or every
// nudge off) it shows the first nudge and starts nothing when dismissed
fn take_break_now(app: &AppHandle) {
    let state = app.state::<App>();
    let first = state.config.lock().unwrap().nudges.first().map_or(1, |n| n.id);
    let due = {
        let schedule = state.schedule.lock().unwrap();
        if schedule.showing.is_some() {
            return;
        }
        schedule.due_now(Instant::now(), JOIN_WINDOW, true)
    };
    let look = due.first().copied().unwrap_or(first);
    if show_overlay(app, look, due, true) {
        let timers = enabled_timers(&nudges(app));
        state.schedule.lock().unwrap().end_pause(&timers, Instant::now());
    }
}

// hide any reminder and hold every nudge back at least `minutes`
fn pause_reminders(app: &AppHandle, minutes: u32) {
    if !is_on(app) {
        return;
    }
    close_windows(app);
    let ids: Vec<u32> = enabled_timers(&nudges(app)).into_iter().map(|(id, _)| id).collect();
    let until = Instant::now() + Duration::from_secs(minutes.clamp(1, 240) as u64 * 60);
    app.state::<App>().schedule.lock().unwrap().pause(&ids, until);
    refresh_tray(app);
    broadcast(app);
}

#[tauri::command]
fn snooze(app: AppHandle, minutes: u32) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || finish_overlay(&handle, Some(minutes)));
}

// async: a sync command runs inside the IPC callback on the main thread, where
// building the overlay windows deadlocks on Windows
#[tauri::command]
async fn preview_reminder(app: AppHandle, id: u32) {
    let handle = app.clone();
    // a preview answers no nudge: timers keep running and dismissing it
    // restarts nothing
    let _ = app.run_on_main_thread(move || {
        show_overlay(&handle, id, Vec::new(), false);
    });
}

#[tauri::command]
fn get_overlay(app: AppHandle) -> OverlayState {
    overlay_state(&app)
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

// the options every nudge shares
#[tauri::command]
fn set_config(
    app: AppHandle,
    enabled_on_wake: Option<bool>,
    reset_on_wake: Option<bool>,
    launch_at_login: Option<bool>,
    menu_bar_timer: Option<String>,
) {
    let state = app.state::<App>();
    {
        let mut config = state.config.lock().unwrap();
        if let Some(value) = enabled_on_wake {
            config.enabled_on_wake = value;
        }
        if let Some(value) = reset_on_wake {
            config.reset_on_wake = value;
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
#[allow(clippy::too_many_arguments)]
fn set_nudge(
    app: AppHandle,
    id: u32,
    enabled: Option<bool>,
    message: Option<String>,
    symbol: Option<String>,
    interval_secs: Option<u32>,
    play_sound: Option<bool>,
    auto_dismiss_secs: Option<u32>,
    break_ideas: Option<bool>,
    style: Option<String>,
    text_size: Option<String>,
    show_counts: Option<bool>,
    sound: Option<String>,
    snooze_mins: Option<u32>,
    layout: Option<String>,
) {
    let state = app.state::<App>();
    // what the schedule has to hear, applied once the config lock is released
    let (start, stop) = {
        let mut config = state.config.lock().unwrap();
        let only = config.nudges.len() == 1;
        let Some(nudge) = config.nudges.iter_mut().find(|n| n.id == id) else {
            return;
        };
        let mut start = false;
        let mut stop = false;
        if let Some(secs) = interval_secs.filter(|&secs| secs > 0) {
            nudge.interval_secs = secs.max(5 * 60);
            // a new interval restarts the timer
            start = true;
        }
        // a lone nudge can't be turned off by itself; the master switch covers it
        if let Some(value) = enabled.filter(|&value| value || !only) {
            if value != nudge.enabled {
                nudge.enabled = value;
                start |= value;
                stop = !value;
            }
        }
        if let Some(value) = message {
            nudge.message = value;
        }
        if let Some(value) = symbol.filter(|v| SYMBOLS.contains(&v.as_str())) {
            nudge.symbol = value;
        }
        if let Some(value) = play_sound {
            nudge.play_sound = value;
        }
        if let Some(value) = auto_dismiss_secs {
            nudge.auto_dismiss_secs = value.min(600);
        }
        if let Some(value) = break_ideas {
            nudge.break_ideas = value;
        }
        if let Some(value) = show_counts {
            nudge.show_counts = value;
        }
        if let Some(value) = style.filter(|v| STYLES.contains(&v.as_str())) {
            nudge.style = value;
        }
        if let Some(value) = text_size.filter(|v| TEXT_SIZES.contains(&v.as_str())) {
            nudge.text_size = value;
        }
        if let Some(value) = sound.filter(|v| SOUNDS.contains(&v.as_str())) {
            nudge.sound = value;
        }
        if let Some(value) = snooze_mins {
            nudge.snooze_mins = value.min(30);
        }
        if let Some(value) = layout.filter(|v| LAYOUTS.contains(&v.as_str())) {
            nudge.layout = value;
        }
        let timer = (nudge.id, Duration::from_secs(nudge.interval_secs as u64));
        ((start && nudge.enabled).then_some(timer), stop)
    };
    {
        let mut schedule = state.schedule.lock().unwrap();
        if let Some(timer) = start {
            schedule.resume(&[timer], Instant::now());
        }
        if stop {
            schedule.next.remove(&id);
        }
    }
    save_config(&app);
    refresh_tray(&app);
    broadcast(&app);
}

// a new nudge with the usual defaults, its timer running; None at the limit
#[tauri::command]
fn add_nudge(app: AppHandle) -> Option<u32> {
    let state = app.state::<App>();
    let nudge = {
        let mut config = state.config.lock().unwrap();
        if config.nudges.len() >= MAX_NUDGES {
            return None;
        }
        let id = new_id(&config.nudges);
        let nudge = Nudge {
            id,
            message: "New nudge".into(),
            symbol: "bell".into(),
            ..Nudge::default()
        };
        config.nudges.push(nudge.clone());
        nudge
    };
    save_config(&app);
    let timer = (nudge.id, Duration::from_secs(nudge.interval_secs as u64));
    state.schedule.lock().unwrap().resume(&[timer], Instant::now());
    refresh_tray(&app);
    broadcast(&app);
    Some(nudge.id)
}

// there is always at least one nudge, so the last can't be deleted
#[tauri::command]
fn delete_nudge(app: AppHandle, id: u32) {
    let state = app.state::<App>();
    let survivor = {
        let mut config = state.config.lock().unwrap();
        if config.nudges.len() <= 1 || !config.nudges.iter().any(|n| n.id == id) {
            return;
        }
        config.nudges.retain(|n| n.id != id);
        // a lone nudge has no switch of its own, so it can't be left off
        match config.nudges.as_mut_slice() {
            [only] if !only.enabled => {
                only.enabled = true;
                Some((only.id, Duration::from_secs(only.interval_secs as u64)))
            }
            _ => None,
        }
    };
    save_config(&app);
    {
        let mut schedule = state.schedule.lock().unwrap();
        schedule.forget(id);
        if let Some(timer) = survivor {
            schedule.resume(&[timer], Instant::now());
        }
    }
    refresh_tray(&app);
    broadcast(&app);
}

#[tauri::command]
fn set_enabled(app: AppHandle, enabled: bool) {
    if enabled != is_on(&app) {
        set_reminders(&app, enabled);
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
            schedule: Mutex::new(Schedule::new()),
            count_session: Mutex::new(0),
            menu: Mutex::new(None),
            countdown_items: Mutex::new(Vec::new()),
            toggle_item: Mutex::new(None),
            tray: Mutex::new(None),
            pause_menu: Mutex::new(None),
            last_label: Mutex::new(String::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_config,
            set_nudge,
            add_nudge,
            delete_nudge,
            get_overlay,
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
                && window.app_handle().state::<App>().schedule.lock().unwrap().showing.is_some()
            {
                let handle = window.app_handle().clone();
                let focus_handle = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    let primary = focus_handle
                        .state::<App>()
                        .schedule
                        .lock()
                        .unwrap()
                        .showing
                        .as_ref()
                        .and_then(|shown| shown.windows.first().cloned())
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
                    "break-now" => take_break_now(app),
                    "pause-30" | "pause-60" | "pause-120" => {
                        let minutes = event.id().as_ref()[6..].parse().unwrap_or(60);
                        pause_reminders(app, minutes);
                    }
                    "settings" => open_settings(app.clone()),
                    #[cfg(target_os = "macos")]
                    "about" => show_about(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(&handle)?;

            *app.state::<App>().menu.lock().unwrap() = Some(menu);
            *app.state::<App>().countdown_items.lock().unwrap() = vec![countdown];
            *app.state::<App>().toggle_item.lock().unwrap() = Some(toggle);
            *app.state::<App>().tray.lock().unwrap() = Some(tray);
            *app.state::<App>().pause_menu.lock().unwrap() = Some(pause);
            refresh_tray(app.app_handle());
            set_reminders(app.app_handle(), true);

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
    use super::{
        config_json, hms, new_id, parse_config, replace_file, AlsoNow, Config, Nudge,
        NudgeState, OverlayState, PublicState,
    };
    use serde::Deserialize;

    #[test]
    fn config_from_0_1_0_loads_with_new_defaults() {
        // a config.json written by 0.1.0, before any customization options existed
        let old = r#"{"interval_secs":2700,"message":"stretch","enabled_on_wake":false,
            "reset_on_wake":true,"play_sound":false,"launch_at_login":true,"count_total":52}"#;
        let config = parse_config(old).expect("old config must still load");
        assert_eq!(config.count_total, 52);
        assert!(!config.enabled_on_wake);
        assert!(config.launch_at_login);
        assert_eq!(config.menu_bar_timer, "never");
        assert_eq!(config.nudges.len(), 1);
        let nudge = &config.nudges[0];
        assert_eq!(nudge.id, 1);
        assert!(nudge.enabled);
        assert_eq!(nudge.interval_secs, 2700);
        assert_eq!(nudge.message, "stretch");
        assert!(!nudge.play_sound);
        assert_eq!(nudge.auto_dismiss_secs, 0);
        assert!(nudge.break_ideas);
        assert_eq!(nudge.style, "frosted");
        assert_eq!(nudge.text_size, "standard");
        assert!(nudge.show_counts);
        assert_eq!(nudge.sound, "chime");
        assert_eq!(nudge.snooze_mins, 5);
        assert_eq!(nudge.layout, "classic");
        assert_eq!(nudge.symbol, "figure");
    }

    #[test]
    fn config_from_0_2_0_becomes_the_first_nudge() {
        let old = r#"{"interval_secs":5400,"message":"Time to step away","enabled_on_wake":true,
            "reset_on_wake":true,"play_sound":false,"launch_at_login":true,"count_total":63,
            "auto_dismiss_secs":0,"break_ideas":true,"style":"frosted","text_size":"standard",
            "show_counts":true,"sound":"chime","snooze_mins":5,"menu_bar_timer":"always",
            "layout":"card"}"#;
        let config = parse_config(old).expect("0.2.0 config must load");
        assert_eq!(config.count_total, 63);
        assert!(config.launch_at_login);
        assert_eq!(config.menu_bar_timer, "always");
        let expected = Nudge {
            interval_secs: 5400,
            play_sound: false,
            layout: "card".into(),
            ..Nudge::default()
        };
        assert_eq!(config.nudges, vec![expected]);
    }

    #[test]
    fn saved_config_loads_back_unchanged() {
        let mut config = Config::default();
        config.count_total = 9;
        config.menu_bar_timer = "last5".into();
        config.nudges.push(Nudge {
            id: 4,
            enabled: false,
            message: "Drink some water".into(),
            symbol: "drop".into(),
            interval_secs: 1200,
            style: "ocean".into(),
            ..Nudge::default()
        });
        let text = serde_json::to_string_pretty(&config_json(&config)).unwrap();
        assert_eq!(parse_config(&text), Some(config));
    }

    #[test]
    fn a_lone_nudge_is_always_on() {
        let text = r#"{"enabled_on_wake":true,"reset_on_wake":true,"launch_at_login":false,
            "count_total":0,"nudges":[{"id":3,"enabled":false,"message":"water"}]}"#;
        let config = parse_config(text).unwrap();
        assert_eq!(config.nudges.len(), 1);
        assert_eq!(config.nudges[0].id, 3);
        assert!(config.nudges[0].enabled);
    }

    #[test]
    fn repeated_or_missing_ids_become_unique() {
        let text = r#"{"enabled_on_wake":true,"reset_on_wake":true,"launch_at_login":false,
            "count_total":0,"nudges":[{"id":2,"message":"a"},{"id":2,"message":"b"},
            {"message":"c"},{"message":"d"}]}"#;
        let config = parse_config(text).unwrap();
        let loaded: Vec<(u32, &str)> =
            config.nudges.iter().map(|n| (n.id, n.message.as_str())).collect();
        assert_eq!(loaded, vec![(2, "a"), (3, "b"), (1, "c"), (4, "d")]);
    }

    #[test]
    fn ids_at_the_top_of_the_range_stay_unique() {
        let text = serde_json::json!({
            "enabled_on_wake": true, "reset_on_wake": true, "launch_at_login": false,
            "count_total": 0,
            "nudges": [
                {"id": u32::MAX, "message": "a"},
                {"id": u32::MAX, "message": "b"},
                {"id": 0, "message": "c"}
            ]
        })
        .to_string();
        let config = parse_config(&text).unwrap();
        let loaded: Vec<(u32, &str)> =
            config.nudges.iter().map(|n| (n.id, n.message.as_str())).collect();
        assert_eq!(loaded, vec![(u32::MAX, "a"), (1, "b"), (0, "c")]);
        // the next nudge added takes a free id rather than wrapping to 0
        assert_eq!(new_id(&config.nudges), 2);
    }

    #[test]
    fn a_failed_save_keeps_the_old_settings() {
        let dir = std::env::temp_dir().join(format!("nudge-save-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        replace_file(&path, "old").unwrap();
        replace_file(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!dir.join("config.json.tmp").exists());

        // a directory where the temp file goes makes the write fail
        std::fs::create_dir(dir.join("config.json.tmp")).unwrap();
        assert!(replace_file(&path, "lost").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        // a directory in the file's place makes the rename fail after the
        // write, and the temp file that was written is removed
        let blocked = dir.join("blocked.json");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("inside"), "").unwrap();
        assert!(replace_file(&blocked, "lost").is_err());
        assert!(!dir.join("blocked.json.tmp").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // the Config struct as 0.2.0 declared it, to check that going back to that
    // build still reads the file this one writes
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Config020 {
        interval_secs: u32,
        message: String,
        enabled_on_wake: bool,
        reset_on_wake: bool,
        play_sound: bool,
        launch_at_login: bool,
        count_total: u32,
        #[serde(default)]
        auto_dismiss_secs: u32,
        break_ideas: bool,
        style: String,
        text_size: String,
        show_counts: bool,
        sound: String,
        snooze_mins: u32,
        menu_bar_timer: String,
        layout: String,
    }

    #[test]
    fn older_builds_still_read_the_first_nudge() {
        let mut config = Config::default();
        config.count_total = 63;
        config.nudges[0] = Nudge {
            message: "stretch".into(),
            interval_secs: 2700,
            style: "dusk".into(),
            layout: "ring".into(),
            play_sound: false,
            ..Nudge::default()
        };
        config.nudges.push(Nudge {
            id: 2,
            message: "water".into(),
            ..Nudge::default()
        });
        let old: Config020 =
            serde_json::from_value(config_json(&config)).expect("0.2.0 must read the new file");
        assert_eq!(old.message, "stretch");
        assert_eq!(old.interval_secs, 2700);
        assert_eq!(old.style, "dusk");
        assert_eq!(old.layout, "ring");
        assert!(!old.play_sound);
        assert_eq!(old.count_total, 63);
    }

    #[test]
    fn public_state_lists_each_nudge_with_its_countdown() {
        let state = PublicState {
            enabled_on_wake: true,
            reset_on_wake: true,
            launch_at_login: false,
            menu_bar_timer: "never".into(),
            count_total: 5,
            count_session: 3,
            enabled: true,
            showing: false,
            nudges: vec![NudgeState {
                nudge: Nudge::default(),
                remaining_secs: 42,
            }],
        };
        let json = serde_json::to_value(state).unwrap();
        for key in [
            "enabled_on_wake", "reset_on_wake", "launch_at_login", "menu_bar_timer",
            "count_total", "count_session", "enabled", "showing",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        let nudge = &json["nudges"][0];
        for key in [
            "id", "enabled", "message", "symbol", "interval_secs", "play_sound",
            "auto_dismiss_secs", "style", "snooze_mins", "layout", "remaining_secs",
        ] {
            assert!(nudge.get(key).is_some(), "missing nudge {key}");
        }
        assert_eq!(nudge["remaining_secs"], 42);
    }

    #[test]
    fn overlay_state_is_flat_for_the_reminder_page() {
        let state = OverlayState {
            look: Nudge::default(),
            also: vec![AlsoNow {
                message: "Drink some water".into(),
                symbol: "drop".into(),
            }],
            count_total: 7,
            count_session: 2,
        };
        let json = serde_json::to_value(state).unwrap();
        for key in [
            "message", "symbol", "style", "layout", "text_size", "break_ideas", "snooze_mins",
            "auto_dismiss_secs", "show_counts", "count_total", "count_session",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        assert_eq!(json["also"][0]["symbol"], "drop");
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
