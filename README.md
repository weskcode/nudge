# nudge

`nudge` is a free, open source, cross-platform status bar app that displays firm
but gentle periodic reminders. It runs on macOS, Windows and Linux, sits quietly
in your system tray, and periodically covers every screen with a friendly fullscreen reminder until
you dismiss it with a click or any key.

It is useful for the Pomodoro Technique and for remembering to get up from your
desk from time to time.

## Credits

`nudge` is inspired by and modeled on
[remindful](https://github.com/brettferdosi/remindful) by
[Brett Gutstein](https://brett.gutste.in), a macOS app that did it first.
Thank you for the idea and the design. `nudge` was written from scratch in Rust
and trades AppleScript-era Objective-C for a lean cross-platform core, but the
product behavior, the tray toggle, the countdown menu and the full-screen
reminder pattern all come from `remindful`.

## Features

- System tray icon: left click toggles reminders on and off, right click opens
  the menu with a live countdown, settings, and quit
- Full screen translucent reminder over every connected display that stays
  visible over fullscreen apps and workspaces
- Click anywhere or press any key to dismiss
- Interval presets of 15/30/45/60/90 minutes plus a custom value, applied live
- Custom reminder message
- Optional behaviors after your computer wakes from sleep: re-enable reminders,
  reset the timer
- Reminder counters (since last wake, lifetime) with a reset button
- Settings persist between runs

## Building from source

Requires Node, npm and the Rust toolchain.

```
npm install
npm run tauri dev     # run in development
npm run build         # produce an installer/binary in src-tauri/target
npm run install:app   # build a release bundle and install/relaunch /Applications/Nudge.app (macOS)
```

## Platform notes

- macOS: the reminder panel behaves like a native HUD, joining all workspaces
  and floating above fullscreen apps
- Windows and Linux: the reminder is a fullscreen always-on-top window
- Wake-from-sleep detection works on macOS and Linux

## License

MIT. The upstream notice for `remindful`, whose design inspired this app, is
preserved in `LICENSE`.
