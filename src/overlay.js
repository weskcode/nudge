const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);

// small, concrete actions: a new one each time keeps the reminder from fading into the background
const BREAK_IDEAS = [
  "Stand up and stretch your arms overhead.",
  "Refill your water and take a few sips.",
  "Look at something far away for 20 seconds.",
  "Roll your shoulders back five times.",
  "Take three slow, deep breaths.",
  "Walk to another room and back.",
  "Unclench your jaw and relax your hands.",
  "Sit back, feet flat, shoulders down.",
  "Open a window or step outside for a minute.",
  "Do ten slow calf raises.",
  "Close your eyes and rest them for a moment.",
  "Jot down where you left off, then step away.",
];

const isMac = navigator.userAgent.includes("Mac");
const device = isMac ? "Mac" : "computer";
let snoozeMins = 5;

// keys and clicks already in flight when the reminder opened were meant for
// another app; ignore them so the reminder can't vanish unseen
const openedAt = performance.now();
const settling = () => performance.now() - openedAt < 1500;

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

function breakLabel(secs) {
  return secs % 60 === 0 ? plural(secs / 60, "minute") : `${secs} seconds`;
}

invoke("get_state").then((s) => {
  document.body.classList.add(`style-${s.style}`, `size-${s.text_size}`);
  if (!isMac) document.body.classList.add("no-blur");

  $("overlay-text").textContent = s.message;

  if (s.break_ideas) {
    $("overlay-idea").textContent = BREAK_IDEAS[s.count_total % BREAK_IDEAS.length];
    $("overlay-idea").hidden = false;
    $("overlay").setAttribute("aria-describedby", "overlay-idea");
  }

  if (s.snooze_mins > 0) {
    snoozeMins = s.snooze_mins;
    $("snooze").textContent = `Snooze ${s.snooze_mins} min`;
    $("snooze").hidden = false;
  }

  if (s.auto_dismiss_secs > 0) {
    document.body.classList.add("has-ring");
    document.body.style.setProperty("--break", `${s.auto_dismiss_secs}s`);
    document.querySelector(".ring").removeAttribute("hidden"); // SVG elements have no .hidden property
    $("overlay-hint").textContent = `Ends on its own in ${breakLabel(s.auto_dismiss_secs)}, or press any key`;
  }

  if (s.show_counts) {
    $("overlay-stats").textContent =
      `${s.count_session} since your ${device} woke  ·  ${s.count_total} total`;
    $("overlay-stats").hidden = false;
  }

  // give VoiceOver and the keyboard a control to start from; Enter and Space on
  // a button are already kept out of press-any-key
  $("done").focus({ focusVisible: false });
});

function dismiss() {
  invoke("close_overlay").catch(() => {});
}

$("overlay").addEventListener("click", (event) => {
  if (event.target.closest("#snooze")) return;
  // a keyboard press on the focused button (detail 0) in the first moments was
  // typed for another app, same as any other key
  if (settling() && (event.detail === 0 || !event.target.closest("#done"))) return;
  dismiss();
});

$("snooze").addEventListener("click", (event) => {
  event.stopPropagation();
  if (settling() && event.detail === 0) return;
  invoke("snooze", { minutes: snoozeMins }).catch(() => {});
});

// keys that move focus or start a shortcut never dismiss, so the buttons stay reachable
const NON_DISMISSING = new Set(["Tab", "Shift", "Control", "Alt", "Meta", "CapsLock", "Fn"]);

// press any key to dismiss; a focused button keeps Enter/Space for itself
document.addEventListener("keydown", (event) => {
  if (event.repeat || settling() || NON_DISMISSING.has(event.key)) return;
  const onButton = event.target && event.target.closest && event.target.closest("button");
  if (onButton && (event.key === "Enter" || event.key === " ")) return;
  dismiss();
});

document.addEventListener("contextmenu", (event) => event.preventDefault());
