const { invoke } = window.__TAURI__.core;

const text = document.getElementById("overlay-text");
const stats = document.getElementById("overlay-stats");
const overlay = document.getElementById("overlay");

invoke("get_state").then((s) => {
  text.textContent = s.message;
  stats.textContent = `${s.count_session} since last wake  ·  ${s.count_total} total`;
});

function dismiss() {
  invoke("close_overlay").catch(() => {});
}

overlay.addEventListener("click", (e) => {
  if (e.target && e.target.id === "snooze") {
    return;
  }
  dismiss();
});

document.getElementById("snooze").addEventListener("click", (event) => {
  event.stopPropagation();
  invoke("snooze", { minutes: 5 }).catch(() => {});
});

// press any key to dismiss; let the snooze button keep Enter/Space clicks
document.addEventListener("keydown", (event) => {
  const onSnooze = event.target && event.target.id === "snooze";
  if (onSnooze && (event.key === "Enter" || event.key === " ")) {
    return;
  }
  dismiss();
});
