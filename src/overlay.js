const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

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
let snoozeMins = 5;

// keys and clicks already in flight when the reminder opened were meant for
// another app; ignore them so the reminder can't vanish unseen
const openedAt = performance.now();
const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)");
const settling = () => performance.now() - openedAt < 1500;

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

function breakLabel(secs) {
  return secs % 60 === 0 ? plural(secs / 60, "minute") : `${secs} seconds`;
}

// a long message at a large text size can be taller than a small screen;
// shrink the message until the whole reminder, buttons included, fits
function fitReminder() {
  const title = $("overlay-text");
  const reminder = document.querySelector(".reminder");
  title.style.fontSize = "";
  const margin = 16;
  // the layout box, which ignores the opening animation's transform
  const fits = () =>
    reminder.offsetTop >= margin && reminder.offsetTop + reminder.offsetHeight <= innerHeight - margin;
  let size = parseFloat(getComputedStyle(title).fontSize);
  while (!fits() && size > 20) {
    size *= 0.9;
    title.style.fontSize = `${size}px`;
  }
}
addEventListener("resize", fitReminder);

// other nudges that came due with this one, each with its symbol
function renderAlso(also) {
  const line = $("overlay-also");
  const items = also.map((nudge) => {
    const item = document.createElement("span");
    const icon = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", `#s-${nudge.symbol}`);
    icon.setAttribute("aria-hidden", "true");
    icon.append(use);
    item.append(icon, nudge.message);
    return item;
  });
  const label = document.createElement("span");
  label.className = "lbl";
  label.textContent = "Also now";
  line.replaceChildren(...(items.length ? [label, ...items] : []));
  fitReminder();
}

// listen first, so a nudge that joins while the page asks isn't missed; the
// look (style, symbol, break length, snooze) is the nudge the reminder opened for
listen("nudge://overlay", (event) => renderAlso(event.payload.also))
  .catch(() => {})
  .then(() => invoke("get_overlay"))
  .then((s) => {
    document.body.classList.add(`style-${s.style}`, `size-${s.text_size}`, `layout-${s.layout}`);
    if (!isMac) document.body.classList.add("no-blur");

    $("glyph-symbol").setAttribute("href", `#s-${s.symbol}`);
    $("overlay-text").textContent = s.message;

    if (s.break_ideas) {
      $("overlay-idea").textContent = BREAK_IDEAS[s.count_total % BREAK_IDEAS.length];
      $("overlay-idea").hidden = false;
    }
    $("overlay").setAttribute(
      "aria-describedby",
      s.break_ideas ? "overlay-idea overlay-also overlay-hint" : "overlay-also overlay-hint",
    );
    renderAlso(s.also);

    if (s.snooze_mins > 0) {
      snoozeMins = s.snooze_mins;
      $("snooze").textContent = `Snooze ${s.snooze_mins} min`;
      $("snooze").hidden = false;
    }

    if (s.auto_dismiss_secs > 0) {
      document.body.classList.add("has-ring");
      document.body.style.setProperty("--break", `${s.auto_dismiss_secs}s`);
      $("overlay-hint").textContent = `Ends on its own in ${breakLabel(s.auto_dismiss_secs)}, or press any key`;
    }

    // SVG elements have no .hidden property; the ring layout always shows the ring
    if (s.auto_dismiss_secs > 0 || s.layout === "ring") document.querySelector(".ring").removeAttribute("hidden");
    fitReminder();

    // give VoiceOver and the keyboard a control to start from; Enter and Space on
    // a button are already kept out of press-any-key
    $("done").focus({ focusVisible: false });
  });

// fade out before the window closes; the first request wins
function leave(command, args) {
  if (document.body.classList.contains("leaving")) return;
  // on failure bring the reminder back so it can be dismissed again
  const send = () => invoke(command, args).catch(() => document.body.classList.remove("leaving"));
  if (reduceMotion.matches) return send();
  document.body.classList.add("leaving");
  setTimeout(send, 160);
}

function dismiss() {
  leave("close_overlay");
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
  leave("snooze", { minutes: snoozeMins });
});

// keys that move focus or start a shortcut never dismiss, so the buttons stay reachable
const NON_DISMISSING = new Set(["Tab", "Shift", "Control", "Alt", "Meta", "CapsLock", "Fn"]);

// Tab moves between the reminder's buttons only, with no empty stop between them
document.addEventListener("keydown", (event) => {
  if (event.key !== "Tab") return;
  const buttons = [...document.querySelectorAll(".overlay-actions button")].filter((b) => !b.hidden);
  const at = buttons.indexOf(document.activeElement);
  const step = event.shiftKey ? buttons.length - 1 : 1;
  event.preventDefault();
  buttons[(at + step) % buttons.length].focus();
});

// press any key to dismiss; a focused button keeps Enter/Space for itself, and a
// key held with Control, Option or Command is a shortcut (VoiceOver's among them)
document.addEventListener("keydown", (event) => {
  if (event.repeat || settling() || NON_DISMISSING.has(event.key)) return;
  if (event.ctrlKey || event.altKey || event.metaKey) return;
  const onButton = event.target && event.target.closest && event.target.closest("button");
  if (onButton && (event.key === "Enter" || event.key === " ")) return;
  dismiss();
});

document.addEventListener("contextmenu", (event) => event.preventDefault());
