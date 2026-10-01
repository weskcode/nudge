const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const isMac = navigator.userAgent.includes("Mac");
const isWindows = navigator.userAgent.includes("Windows");
const PRESETS = [15, 30, 45, 60, 90];
const MAX_NUDGES = 8; // matches the limit in main.rs
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
let deadlines = new Map(); // nudge id to the local clock time it fires, for the live countdowns
let customMode = false;
let ideaIndex = 0;
let selectedId = null; // the nudge the Schedule, Reminder and Break pages edit
let pickerSignature = "";

// the nudge being edited; the first one if the saved choice was deleted
function current() {
  return state.nudges.find((n) => n.id === selectedId) || state.nudges[0];
}

function secsUntil(at) {
  return at === undefined ? null : Math.max(0, Math.round((at - Date.now()) / 1000));
}

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
  const nudge = current();
  const minutes = Math.round(nudge.interval_secs / 60);
  const deadline = deadlines.get(nudge.id);
  const left = secsUntil(deadline);
  // the sidebar and the menu bar count down to whichever nudge is next
  const next = secsUntil(deadlines.size ? Math.min(...deadlines.values()) : undefined);
  let title, detail, label, time, info, fraction;

  if (state.showing) {
    title = "Reminder Showing";
    detail = "Dismiss it to continue";
  } else if (state.enabled && next !== null) {
    title = "Reminders On";
    detail = `Next in ${formatClock(next)}`;
  } else if (state.enabled) {
    title = "Reminders On";
    detail = "Every nudge is off";
  } else {
    title = "Reminders Off";
    detail =
      state.nudges.length > 1 ? `${state.nudges.length} nudges when on` : `Every ${plural(minutes, "minute")} when on`;
  }

  // the countdown card is for the nudge being edited
  // with reminders off (Take a Break Now, Preview) no nudge is counting down
  if (state.showing && (nudge.remaining_secs === 0 || !state.enabled)) {
    label = "Reminder showing";
    time = "Now";
    info = "Dismiss it to start the next one.";
    fraction = 0;
  } else if (!state.enabled) {
    label = "Reminders are off";
    time = "Off";
    info = `Turn them on to get a reminder every ${plural(minutes, "minute")}.`;
    fraction = 0;
  } else if (!nudge.enabled) {
    label = "This nudge is off";
    time = "Off";
    info = `Turn it on to get it every ${plural(minutes, "minute")}.`;
    fraction = 0;
  } else if (left !== null) {
    const at = new Date(deadline).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
    label = "Next reminder in";
    time = formatClock(left);
    info = `At ${at} · every ${plural(minutes, "minute")}`;
    fraction = Math.min(1, left / nudge.interval_secs);
  } else {
    label = "Next reminder in";
    time = "–";
    info = `Every ${plural(minutes, "minute")}`;
    fraction = 0;
  }

  if ($("status-title").textContent !== title) $("status-title").textContent = title;
  $("status-detail").textContent = detail;
  $("countdown-label").textContent = label;
  $("countdown-time").textContent = time;
  $("countdown-detail").textContent = info;
  // jump, don't sweep, on the first render or when a new cycle starts
  const dial = $("dial-fill");
  const offset = DIAL * (1 - fraction);
  const jump = !(Math.abs(offset - parseFloat(dial.style.strokeDashoffset)) <= DIAL * 0.02);
  dial.classList.toggle("no-anim", jump);
  dial.style.strokeDashoffset = String(offset);
  if (jump) {
    void dial.getBoundingClientRect();
    dial.classList.remove("no-anim");
  }

  // the menu bar miniature mirrors what the status item shows
  const mode = state.menu_bar_timer;
  const shown = mode === "always" || (mode === "last5" && next !== null && next <= 300);
  $("mb-time").textContent = next !== null && shown ? formatClock(next) : mode === "last5" ? "4:59" : "";
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

function renderPreview(n, message = n.message) {
  const preview = $("live-preview");
  const before = preview.className;
  preview.className = `preview style-${n.style} size-${n.text_size} layout-${n.layout}${n.auto_dismiss_secs > 0 ? " has-ring" : ""}`;
  $("pv-symbol").setAttribute("href", `#s-${n.symbol}`);
  $("pv-message").textContent = message || "Time to step away";
  $("pv-idea").hidden = !n.break_ideas;
  $("pv-idea").textContent = BREAK_IDEAS[ideaIndex];
  $("pv-snooze").hidden = n.snooze_mins === 0;
  $("pv-snooze").textContent = `Snooze ${n.snooze_mins} min`;
  if (before && before !== preview.className) replayPreview();
}

// a nudge's tile wears its symbol in the colour of its reminder style
function paintTile(tile, use, n) {
  tile.className = `tile n-${n.style}`;
  use.setAttribute("href", `#s-${n.symbol}`);
}

function clip(text, max) {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

// the pop-up lists the nudges, then New and Delete; rebuilt only when a name or
// the count changes, so an open menu isn't pulled out from under the pointer
function renderPicker(s, n) {
  const select = $("nudge-select");
  const key = JSON.stringify([n.id, s.nudges.map((x) => [x.id, x.message])]);
  if (key !== pickerSignature) {
    pickerSignature = key;
    const add = new Option("New Nudge…", "new");
    add.disabled = s.nudges.length >= MAX_NUDGES;
    const remove = new Option(`Delete “${clip(n.message, 24)}”`, "delete");
    remove.disabled = s.nudges.length <= 1;
    select.replaceChildren(
      ...s.nudges.map((x) => new Option(x.message || "Untitled", String(x.id))),
      document.createElement("hr"),
      add,
      remove,
    );
  }
  select.value = String(n.id);
  fitToChoice(select);
  paintTile($("picker-tile"), $("picker-symbol"), n);
}

// a pop-up is as wide as its longest item; size it to the chosen one instead
const measure = document.createElement("canvas").getContext("2d");
function fitToChoice(select) {
  const style = getComputedStyle(select);
  measure.font = `${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
  const text = select.selectedOptions[0] ? select.selectedOptions[0].text : "";
  const padding = parseFloat(style.paddingLeft) + parseFloat(style.paddingRight);
  select.style.width = `${Math.ceil(measure.measureText(text).width + padding) + 2}px`;
}

function selectNudge(id) {
  selectedId = id;
  customMode = false;
  try {
    localStorage.setItem("nudge.selected", String(id));
  } catch {}
  if (state) render(state);
}

function render(s) {
  state = s;
  // 0 while a nudge is on screen, -1 with no timer
  deadlines = new Map(
    s.nudges
      .filter((x) => s.enabled && x.remaining_secs >= 0 && !(s.showing && x.remaining_secs === 0))
      .map((x) => [x.id, Date.now() + x.remaining_secs * 1000]),
  );
  const n = current();
  document.body.classList.toggle("is-off", !s.enabled);
  document.body.classList.toggle("nudge-off", !n.enabled);

  $("enabled").checked = s.enabled;
  renderPicker(s, n);
  // one nudge is covered by the switch in the sidebar
  $("nudge-on-row").hidden = s.nudges.length < 2;
  $("nudge-enabled").checked = n.enabled;

  const minutes = Math.round(n.interval_secs / 60);
  const preset = PRESETS.includes(minutes) && !customMode;
  setRadio("interval", preset ? String(minutes) : "custom");
  $("custom-row").classList.toggle("collapsed", preset);
  $("custom-row").inert = preset; // out of the tab order and VoiceOver while hidden
  if (document.activeElement !== $("custom-minutes")) $("custom-minutes").value = minutes;

  if (document.activeElement !== $("message")) $("message").value = n.message;
  setRadio("symbol", n.symbol);
  paintTile($("symbol-tile"), $("symbol-tile-symbol"), n);
  $("break-ideas").checked = n.break_ideas;
  setRadio("style", n.style);
  setRadio("text-size", n.text_size);
  setRadio("layout", n.layout);

  setSelect($("auto-dismiss"), n.auto_dismiss_secs, (v) => `${v} seconds`);
  setSelect($("snooze"), n.snooze_mins, (v) => plural(v, "minute"));
  $("sound").value = n.play_sound ? n.sound : "none";
  if ($("sound").selectedIndex < 0) $("sound").value = "chime"; // a macOS-only sound elsewhere
  $("menu-bar-timer").value = s.menu_bar_timer;

  $("enabled-on-wake").checked = s.enabled_on_wake;
  $("reset-on-wake").checked = s.reset_on_wake;
  $("launch-at-login").checked = s.launch_at_login;

  renderPreview(n, document.activeElement === $("message") ? $("message").value.trim() : n.message);
  renderStatus();
}

function save(fields) {
  return invoke("set_config", fields).catch((error) => console.error("saving settings failed", error));
}

// settings that belong to the nudge being edited
function saveNudge(fields) {
  return invoke("set_nudge", { id: current().id, ...fields }).catch((error) =>
    console.error("saving the nudge failed", error),
  );
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
  $("picker").hidden = !("nudge" in page.dataset);
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
document.body.classList.toggle("inactive", !document.hasFocus());

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
  selectedId = Number(localStorage.getItem("nudge.selected")) || null;
} catch {}
showPage(saved || "schedule");

invoke("get_state").then(render);
listen("nudge://state", (event) => render(event.payload));
setInterval(renderStatus, 1000);

// cycle the sample idea so it is clear a different one appears each time
setInterval(() => {
  if (!state || !current().break_ideas || $("page-reminder").hidden) return;
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

// a letter or arrow key changes a closed pop-up without opening it; that may
// pick a nudge, but only a choice made in the open menu adds or deletes one
let lastPickerKey = { key: "", at: -Infinity };
$("nudge-select").addEventListener("keydown", (e) => {
  lastPickerKey = { key: e.key, at: performance.now() };
});

$("nudge-select").addEventListener("change", async (e) => {
  const choice = e.target.value;
  // New and Delete are commands; the pop-up keeps showing the nudge being edited
  e.target.value = String(current().id);
  const typed = performance.now() - lastPickerKey.at < 500 && lastPickerKey.key !== "Enter" && lastPickerKey.key !== " ";
  if (typed && (choice === "new" || choice === "delete")) return;
  if (choice === "new") {
    const id = await invoke("add_nudge");
    if (id == null) return;
    state = await invoke("get_state");
    selectNudge(id);
    // a new nudge starts with its name
    showPage("reminder");
    $("message").focus();
    $("message").select();
  } else if (choice === "delete") {
    await invoke("delete_nudge", { id: current().id });
  } else {
    selectNudge(Number(choice));
  }
});

$("nudge-enabled").addEventListener("change", (e) => saveNudge({ enabled: e.target.checked }));

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
    saveNudge({ intervalSecs: Number(input.value) * 60 });
  });
});

$("custom-minutes").addEventListener("change", () => {
  const minutes = Math.round(Number($("custom-minutes").value));
  if (minutes > 0) saveNudge({ intervalSecs: Math.min(480, Math.max(5, minutes)) * 60 });
});

$("message").addEventListener("input", () => state && renderPreview(current(), $("message").value.trim()));
$("message").addEventListener("change", () => {
  const message = $("message").value.trim() || "Time to step away";
  saveNudge({ message });
});
$("message").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("message").blur();
});

document.querySelectorAll('input[name="symbol"]').forEach((input) =>
  input.addEventListener("change", () => saveNudge({ symbol: input.value })),
);
$("break-ideas").addEventListener("change", (e) => saveNudge({ breakIdeas: e.target.checked }));
document.querySelectorAll('input[name="style"]').forEach((input) =>
  input.addEventListener("change", () => saveNudge({ style: input.value })),
);
document.querySelectorAll('input[name="text-size"]').forEach((input) =>
  input.addEventListener("change", () => saveNudge({ textSize: input.value })),
);
document.querySelectorAll('input[name="layout"]').forEach((input) =>
  input.addEventListener("change", () => saveNudge({ layout: input.value })),
);
$("preview").addEventListener("click", () => invoke("preview_reminder", { id: current().id }));

$("auto-dismiss").addEventListener("change", (e) => saveNudge({ autoDismissSecs: Number(e.target.value) }));
$("snooze").addEventListener("change", (e) => saveNudge({ snoozeMins: Number(e.target.value) }));
$("sound").addEventListener("change", (e) => {
  const sound = e.target.value;
  if (sound === "none") {
    saveNudge({ playSound: false });
  } else {
    saveNudge({ playSound: true, sound });
    invoke("preview_sound", { sound });
  }
});
$("menu-bar-timer").addEventListener("change", (e) => save({ menuBarTimer: e.target.value }));

$("enabled-on-wake").addEventListener("change", (e) => save({ enabledOnWake: e.target.checked }));
$("reset-on-wake").addEventListener("change", (e) => save({ resetOnWake: e.target.checked }));
$("launch-at-login").addEventListener("change", (e) => save({ launchAtLogin: e.target.checked }));


$("credits").addEventListener("click", (e) => {
  e.preventDefault();
  invoke("open_credits");
});

// a native window has no "Reload" or "Inspect" menu; keep the system menu for text fields
document.addEventListener("contextmenu", (e) => {
  if (!e.target.closest("input[type=text], input[type=number]")) e.preventDefault();
});
