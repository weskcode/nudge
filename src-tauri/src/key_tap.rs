// a listen-only CoreGraphics event tap for key presses anywhere on the Mac.
// rdev did this before, but it looks up the keyboard layout for every key on
// its own thread, and macOS 27 kills an app that does that off the main
// thread. this reads only the key code, so it never touches the layout

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};

// virtual key code of Tab (kVK_Tab)
pub const TAB: i64 = 0x30;

type Ref = *mut c_void;
type Callback = extern "C" fn(proxy: Ref, kind: u32, event: Ref, info: *mut c_void) -> Ref;

const HID_EVENT_TAP: u32 = 0;
const HEAD_INSERT_EVENT_TAP: u32 = 0;
const LISTEN_ONLY: u32 = 1;
const KEY_DOWN: u32 = 10;
const KEYBOARD_EVENT_KEYCODE: u32 = 9;
// kCGEventFlagMaskControl, kCGEventFlagMaskAlternate, kCGEventFlagMaskCommand
const SHORTCUT_FLAGS: u64 = 0x0004_0000 | 0x0008_0000 | 0x0010_0000;
// sent in place of an event when macOS has switched the tap off
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: Callback,
        info: *mut c_void,
    ) -> Ref;
    fn CGEventTapEnable(tap: Ref, enable: bool);
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    fn CGEventGetFlags(event: Ref) -> u64;
}

// a key held with Control, Option or Command is a shortcut, VoiceOver's among them
fn is_shortcut(flags: u64) -> bool {
    flags & SHORTCUT_FLAGS != 0
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: *const c_void;
    fn CFMachPortCreateRunLoopSource(allocator: *const c_void, port: Ref, order: isize) -> Ref;
    fn CFRunLoopGetCurrent() -> Ref;
    fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: *const c_void);
    fn CFRunLoopRun();
}

struct Listener {
    tap: AtomicPtr<c_void>,
    on_key: Box<dyn Fn(i64, bool)>,
}

extern "C" fn callback(_proxy: Ref, kind: u32, event: Ref, info: *mut c_void) -> Ref {
    let listener = unsafe { &*(info as *const Listener) };
    match kind {
        KEY_DOWN => {
            let keycode = unsafe { CGEventGetIntegerValueField(event, KEYBOARD_EVENT_KEYCODE) };
            let shortcut = is_shortcut(unsafe { CGEventGetFlags(event) });
            // a panic must not unwind into CoreGraphics; the panic hook has
            // already printed it by the time it's caught here
            let _ = catch_unwind(AssertUnwindSafe(|| (listener.on_key)(keycode, shortcut)));
        }
        TAP_DISABLED_BY_TIMEOUT | TAP_DISABLED_BY_USER_INPUT => unsafe {
            CGEventTapEnable(listener.tap.load(Ordering::Relaxed), true)
        },
        _ => {}
    }
    event
}

// calls on_key with the key code of every key press and whether it was a
// shortcut, on this thread, for the life of the app. fails straight away
// without Input Monitoring permission
pub fn listen(on_key: impl Fn(i64, bool) + 'static) -> Result<(), &'static str> {
    // leaked: the tap hands this pointer to the callback for as long as the
    // tap exists, which is the life of the app
    let listener: &'static Listener = Box::leak(Box::new(Listener {
        tap: AtomicPtr::new(null_mut()),
        on_key: Box::new(on_key),
    }));
    unsafe {
        let tap = CGEventTapCreate(
            HID_EVENT_TAP,
            HEAD_INSERT_EVENT_TAP,
            LISTEN_ONLY,
            1 << KEY_DOWN,
            callback,
            listener as *const Listener as *mut c_void,
        );
        if tap.is_null() {
            return Err("cannot create an event tap (Input Monitoring is off)");
        }
        listener.tap.store(tap, Ordering::Relaxed);
        let source = CFMachPortCreateRunLoopSource(null(), tap, 0);
        if source.is_null() {
            return Err("cannot add the event tap to a run loop");
        }
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        CFRunLoopRun();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_shortcut;

    #[test]
    fn control_option_and_command_make_a_shortcut_but_shift_does_not() {
        assert!(!is_shortcut(0));
        assert!(!is_shortcut(0x0002_0000), "Shift alone types a capital, a normal key");
        assert!(is_shortcut(0x0004_0000));
        assert!(is_shortcut(0x0008_0000));
        assert!(is_shortcut(0x0010_0000));
        // VoiceOver's keys: Control and Option together
        assert!(is_shortcut(0x0004_0000 | 0x0008_0000));
    }
}
