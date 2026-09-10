const { invoke } = window.__TAURI__.core;

const text = document.getElementById("overlay-text");

invoke("get_state").then((s) => {
  text.textContent = `${s.message}\n\n${s.count_session} reminders since last wake\n${s.count_total} reminders total`;
});

document.getElementById("overlay").addEventListener("click", () => invoke("close_overlay"));
document.addEventListener("keydown", () => invoke("close_overlay"));
