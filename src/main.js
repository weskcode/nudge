const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const isMac = navigator.userAgent.includes("Mac");
const isWindows = navigator.userAgent.includes("Windows");
const PRESETS = [15, 30, 45, 60, 90];
const device = isMac ? "Mac" : "computer";
const DIAL = 175.93; // circumference of the countdown dial (r = 28)
const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)");

// the same ideas the reminder shows, so the preview is honest about what appears
const BREAK_IDEAS = [
  "Stand up and stretch your arms overhead.",
  "Refill your water and take a few sips.",
  "Look at something far away for 20 seconds.",
  "Roll your shoulders back five times.",
  "Take three slow, deep breaths.",
  "Walk to another room and back.",
];

let state = null;
let deadline = null; // local clock time of the next reminder, for the live countdown
let customMode = false;
let ideaIndex = 0;

function formatClock(secs) {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const mm = h > 0 ? String(m).padStart(2, "0") : String(m);
  return `${h > 0 ? `${h}:` : ""}${mm}:${String(s).padStart(2, "0")}`;
}

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

function renderStatus() {
  if (!state) return;
  const minutes = Math.round(state.interval_secs / 60);
  const left = deadline ? Math.max(0, Math.round((deadline - Date.now()) / 1000)) : null;
  let title, detail, label, time, info, fraction;

  if (state.showing) {
    title = "Reminder Showing";
    detail = "Dismiss it to continue";
    label = "Reminder showing";
    time = "Now";
    info = "Dismiss it to start the next one.";
    fraction = 0;
  } else if (state.enabled && left !== null) {
    const at = new Date(deadline).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
    title = "Reminders On";
    detail = `Next in ${formatClock(left)}`;
    label = "Next reminder in";
    time = formatClock(left);
    info = `At ${at} · every ${plural(minutes, "minute")}`;
    fraction = Math.min(1, left / state.interval_secs);
  } else {
    title = "Reminders Off";
    detail = `Every ${plural(minutes, "minute")} when on`;
    label = "Reminders are off";
    time = "Off";
    info = `Turn them on to get a reminder every ${plural(minutes, "minute")}.`;
    fraction = 0;
  }

  if ($("status-title").textContent !== title) $("status-title").textContent = title;
  $("status-detail").textContent = detail;
  $("countdown-label").textContent = label;
  $("countdown-time").textContent = time;
  $("countdown-detail").textContent = info;
  $("dial-fill").style.strokeDashoffset = String(DIAL * (1 - fraction));

  // the menu bar miniature mirrors what the status item shows
  const mode = state.menu_bar_timer;
  const shown = mode === "always" || (mode === "last5" && left !== null && left <= 300);
  $("mb-time").textContent = left !== null && shown ? formatClock(left) : mode === "last5" ? "4:59" : "";
  $("mb-time").classList.toggle("gone", mode === "never" || !state.enabled);
  $("mb-clock").textContent = new Date().toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

function setRadio(name, value) {
  document.querySelectorAll(`input[name="${name}"]`).forEach((input) => {
    input.checked = input.value === value;
  });
}

// keep an unexpected saved value visible instead of silently showing the wrong option
function setSelect(select, value, labelFor) {
  const key = String(value);
  if (![...select.options].some((o) => o.value === key)) {
    select.add(new Option(labelFor(value), key));
  }
  select.value = key;
}

// replay the reminder's entrance so a change reads as "this is what you'll see"
function replayPreview() {
  const content = $("pv-content");
  content.classList.remove("rise");
  void content.offsetWidth;
  content.classList.add("rise");
}

function renderPreview(s, message = s.message) {
  const preview = $("live-preview");
  const before = preview.className;
  preview.className = `preview style-${s.style} size-${s.text_size}${s.auto_dismiss_secs > 0 ? " has-ring" : ""}`;
  $("pv-message").textContent = message || "Time to step away";
  $("pv-idea").hidden = !s.break_ideas;
  $("pv-idea").textContent = BREAK_IDEAS[ideaIndex];
  $("pv-snooze").hidden = s.snooze_mins === 0;
  $("pv-snooze").textContent = `Snooze ${s.snooze_mins} min`;
  $("pv-stats").hidden = !s.show_counts;
  $("pv-stats").textContent = `${s.count_session} since your ${device} woke  ·  ${s.count_total} total`;
  if (before && before !== preview.className) replayPreview();
}

function render(s) {
  state = s;
  deadline = s.enabled && !s.showing && s.remaining_secs >= 0 ? Date.now() + s.remaining_secs * 1000 : null;
  document.body.classList.toggle("is-off", !s.enabled);

  $("enabled").checked = s.enabled;

  const minutes = Math.round(s.interval_secs / 60);
  const preset = PRESETS.includes(minutes) && !customMode;
  setRadio("interval", preset ? String(minutes) : "custom");
  $("custom-row").classList.toggle("collapsed", preset);
  $("custom-row").inert = preset; // out of the tab order and VoiceOver while hidden
  if (document.activeElement !== $("custom-minutes")) $("custom-minutes").value = minutes;

  if (document.activeElement !== $("message")) $("message").value = s.message;
  $("break-ideas").checked = s.break_ideas;
  setRadio("style", s.style);
  setRadio("text-size", s.text_size);
  $("show-counts").checked = s.show_counts;

  setSelect($("auto-dismiss"), s.auto_dismiss_secs, (v) => `${v} seconds`);
  setSelect($("snooze"), s.snooze_mins, (v) => plural(v, "minute"));
  $("sound").value = s.play_sound ? s.sound : "none";
  if ($("sound").selectedIndex < 0) $("sound").value = "chime"; // a macOS-only sound elsewhere
  $("menu-bar-timer").value = s.menu_bar_timer;

  $("enabled-on-wake").checked = s.enabled_on_wake;
  $("reset-on-wake").checked = s.reset_on_wake;
  $("launch-at-login").checked = s.launch_at_login;

  $("count-total").textContent = `${plural(s.count_total, "reminder")} so far`;
  $("count-session").textContent = `${s.count_session} since your ${device} last woke`;

  renderPreview(s, document.activeElement === $("message") ? $("message").value.trim() : s.message);
  renderStatus();
}

function save(fields) {
  return invoke("set_config", fields).catch((error) => console.error("saving settings failed", error));
}

// ----- sidebar navigation -----

const tabs = [...document.querySelectorAll('.nav [role="tab"]')];

function showPage(name, focus = false) {
  const tab = tabs.find((t) => t.dataset.page === name && !t.hidden) || tabs[0];
  tabs.forEach((t) => {
    const selected = t === tab;
    t.setAttribute("aria-selected", String(selected));
    t.tabIndex = selected ? 0 : -1;
    $(`page-${t.dataset.page}`).hidden = !selected;
  });
  const page = $(`page-${tab.dataset.page}`);
  $("page-title").textContent = page.dataset.title;
  document.querySelector(".pages").scrollTop = 0;
  if (focus) tab.focus();
  try {
    localStorage.setItem("nudge.page", tab.dataset.page);
  } catch {}
}

tabs.forEach((tab) => tab.addEventListener("click", () => showPage(tab.dataset.page)));

// arrow keys move through the sidebar, like a native source list
document.querySelector(".nav").addEventListener("keydown", (e) => {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  e.preventDefault();
  const visible = tabs.filter((t) => !t.hidden);
  const at = visible.indexOf(document.activeElement);
  const next = visible[(at + (e.key === "ArrowDown" ? 1 : visible.length - 1)) % visible.length];
  showPage(next.dataset.page, true);
});

// a background window greys its sidebar selection, as AppKit does
window.addEventListener("blur", () => document.body.classList.add("inactive"));
window.addEventListener("focus", () => document.body.classList.remove("inactive"));

// ----- platform-specific options -----

if (isMac) {
  document.body.classList.add("glass"); // the native window material shows through (see open_settings)
} else {
  document.querySelectorAll("#sound option[data-mac]").forEach((o) => o.remove());
}
if (isWindows) {
  $("tab-menubar").hidden = true; // the Windows tray cannot show text
}
$("h-wake").textContent = `After your ${device} wakes`;

let saved = null;
try {
  saved = localStorage.getItem("nudge.page");
} catch {}
showPage(saved || "schedule");

invoke("get_state").then(render);
listen("nudge://state", (event) => render(event.payload));
setInterval(renderStatus, 1000);

// cycle the sample idea so it is clear a different one appears each time
setInterval(() => {
  if (!state || !state.break_ideas || $("page-reminder").hidden) return;
  const idea = $("pv-idea");
  const swap = () => {
    ideaIndex = (ideaIndex + 1) % BREAK_IDEAS.length;
    idea.textContent = BREAK_IDEAS[ideaIndex];
    idea.classList.remove("fading");
  };
  if (reduceMotion.matches) return swap();
  idea.classList.add("fading");
  setTimeout(swap, 300);
}, 4000);

window.__TAURI__.app
  .getVersion()
  .then((v) => ($("version").textContent = `Version ${v}`))
  .catch(() => {});

// ----- controls -----

$("enabled").addEventListener("change", (e) => invoke("set_enabled", { enabled: e.target.checked }));

document.querySelectorAll('input[name="interval"]').forEach((input) => {
  input.addEventListener("change", () => {
    if (input.value === "custom") {
      customMode = true;
      $("custom-row").classList.remove("collapsed");
      $("custom-row").inert = false;
      $("custom-minutes").focus();
      $("custom-minutes").select();
      return;
    }
    customMode = false;
    save({ intervalSecs: Number(input.value) * 60 });
  });
});

$("custom-minutes").addEventListener("change", () => {
  const minutes = Math.round(Number($("custom-minutes").value));
  if (minutes > 0) save({ intervalSecs: Math.min(480, Math.max(5, minutes)) * 60 });
});

$("message").addEventListener("input", () => state && renderPreview(state, $("message").value.trim()));
$("message").addEventListener("change", () => {
  const message = $("message").value.trim() || "Time to step away";
  save({ message });
});
$("message").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("message").blur();
});

$("break-ideas").addEventListener("change", (e) => save({ breakIdeas: e.target.checked }));
document.querySelectorAll('input[name="style"]').forEach((input) =>
  input.addEventListener("change", () => save({ style: input.value })),
);
document.querySelectorAll('input[name="text-size"]').forEach((input) =>
  input.addEventListener("change", () => save({ textSize: input.value })),
);
$("show-counts").addEventListener("change", (e) => save({ showCounts: e.target.checked }));
$("preview").addEventListener("click", () => invoke("preview_reminder"));

$("auto-dismiss").addEventListener("change", (e) => save({ autoDismissSecs: Number(e.target.value) }));
$("snooze").addEventListener("change", (e) => save({ snoozeMins: Number(e.target.value) }));
$("sound").addEventListener("change", (e) => {
  const sound = e.target.value;
  if (sound === "none") {
    save({ playSound: false });
  } else {
    save({ playSound: true, sound });
    invoke("preview_sound", { sound });
  }
});
$("menu-bar-timer").addEventListener("change", (e) => save({ menuBarTimer: e.target.value }));

$("enabled-on-wake").addEventListener("change", (e) => save({ enabledOnWake: e.target.checked }));
$("reset-on-wake").addEventListener("change", (e) => save({ resetOnWake: e.target.checked }));
$("launch-at-login").addEventListener("change", (e) => save({ launchAtLogin: e.target.checked }));

$("reset-counters").addEventListener("click", () => invoke("reset_counters"));

$("credits").addEventListener("click", (e) => {
  e.preventDefault();
  invoke("open_credits");
});

// a native window has no "Reload" or "Inspect" menu; keep the system menu for text fields
document.addEventListener("contextmenu", (e) => {
  if (!e.target.closest("input[type=text], input[type=number]")) e.preventDefault();
});
