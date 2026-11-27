# Mac App Store submission

The values to enter in App Store Connect for the Mac App Store build. The build
itself comes from `sh scripts/build-mas.sh` (see the README). Each claim here was
checked against the code; if the app changes, check this page again.

## App record

| Field | Value |
|---|---|
| Platform | macOS |
| Bundle ID | `com.weskcode.nudge` (`in.nudge.app` belongs to another team) |
| SKU | `NUDGE-MAC` |
| Primary language | English (U.S.) |
| Name | `Nudge: Break Reminders`, if `Nudge` alone is taken |
| Primary category | Productivity |
| Secondary category | Health & Fitness |
| Price | Your choice. The source is free under the MIT license. |
| Minimum macOS | 13.0, set by `src-tauri/tauri.mas.conf.json` |

## Listing

**Subtitle** (29 of 30 characters)

```
Move, hydrate, rest your eyes
```

**Promotional text** (147 of 170 characters)

```
Set a schedule and Nudge covers every screen with a gentle reminder to stand up, drink water or rest your eyes. Click or press any key to carry on.
```

**Keywords** (98 of 100 characters). They repeat no word from the name or
subtitle, and they leave out Apple feature names such as Screen Time.

```
timer,pomodoro,stretch,posture,menu bar,pause,20-20-20,water,desk,focus,fullscreen,eye strain,tray
```

**Description**

```
Nudge sits in your menu bar and, on a schedule you choose, covers every screen with a firm but gentle reminder to take a break.

Stand up. Drink some water. Rest your eyes. Click, press Done, or press any key while the reminder is in front, and carry on.

Make each reminder yours
• Up to 8 separate nudges, each with its own message, interval and symbol
• Five looks: Frosted, Dusk, Ocean, Forest and Midnight
• Three layouts: Classic, Card and Ring, a ring that counts down your break
• Three animations: Calm, Bloom and Whisper. With Reduce Motion on, the reminder just appears
• A break idea that changes each time, so the reminder does not fade into the background
• Snooze, pause for 30 minutes, 1 hour or 2 hours, or take a break right now
• Chime or macOS system sounds, or none
• Open at login, if you want it

Private by design
Nudge has no account, no analytics and no tracking, and it sends nothing anywhere. Your settings stay on your Mac, readable only by you. Settings > General shows the privacy policy.

Nudge is open source under the MIT license and needs macOS 13 or later.
```

## Links and legal

| Field | Value |
|---|---|
| Support URL | https://github.com/weskcode/nudge/issues |
| Privacy policy URL | https://github.com/weskcode/nudge/blob/main/PRIVACY.md, which works once this branch is on `main` |
| Copyright | `2026 Wesley Keetch`, or the name you publish under |

## App Privacy

Choose **Data Not Collected**. Nudge's code opens no network connections, and
`scripts/check-no-network.sh` checks that for every build. The local diagnostics
never leave the Mac unless the user copies the report themselves.

## Age rating

Every content question is "None" or "No". Nudge shows only the user's own reminder
text, has no web browsing inside the app, and has no user-generated content shared
with others. That gives 4+.

## Export compliance

`ITSAppUsesNonExemptEncryption` is `false` in `src-tauri/Info.plist`, so App Store
Connect does not ask.

## Notes for App Review

```
Nudge is a menu bar app with no Dock icon. After launch, click the bell in the menu bar.

To see a reminder right away, choose Take a Break Now from the menu. The reminder covers every screen. Click Done, click anywhere, or press any key to close it. Settings opens from the same menu.

No account or sign-in is needed, and nothing is purchased in the app.

Input Monitoring is optional. If allowed, it lets any key close a reminder while another app is in front. The key listener runs only while a reminder is showing and reads only the key code. Without the permission, the reminder still closes with a click, the Done button, or a key typed while it has focus.

Open at login uses SMAppService.

The app holds the com.apple.security.network.client entitlement because WebKit, which draws the Settings window and the reminders, does not render them in the sandbox without it. Nudge itself makes no network connections and loads only its own bundled files.
```
