const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let intervalSecs = 1800;

function applyPresetHighlight() {
  document.querySelectorAll(".presets button").forEach((b) => {
    b.classList.toggle("active", Number(b.dataset.minutes) * 60 === intervalSecs);
  });
  if (![...document.querySelectorAll(".presets button")].some((b) => b.classList.contains("active"))) {
    $("custom-minutes").value = Math.round(intervalSecs / 60);
  }
}

function render(s) {
  intervalSecs = s.interval_secs;
  $("message").value = s.message;
  $("enabled-on-wake").checked = s.enabled_on_wake;
  $("reset-on-wake").checked = s.reset_on_wake;
  $("counter-line").textContent =
    `${s.count_session} reminders since last wake, ${s.count_total} reminders total`;
  applyPresetHighlight();
}

invoke("get_state").then(render);

listen("nudge://state", (event) => render(event.payload));

document.querySelectorAll(".presets button").forEach((b) => {
  b.addEventListener("click", () => {
    intervalSecs = Number(b.dataset.minutes) * 60;
    invoke("set_config", { intervalSecs: intervalSecs }).then(render);
  });
});

$("custom-minutes").addEventListener("change", () => {
  const minutes = Number($("custom-minutes").value);
  if (minutes > 0) {
    intervalSecs = minutes * 60;
    invoke("set_config", { intervalSecs: intervalSecs }).then(render);
  }
});

$("message").addEventListener("change", () => {
  invoke("set_config", { message: $("message").value }).then(render);
});

$("enabled-on-wake").addEventListener("change", () => {
  invoke("set_config", { enabledOnWake: $("enabled-on-wake").checked }).then(render);
});

$("reset-on-wake").addEventListener("change", () => {
  invoke("set_config", { resetOnWake: $("reset-on-wake").checked }).then(render);
});

$("reset-counters").addEventListener("click", () => {
  invoke("reset_counters").then(render);
});
