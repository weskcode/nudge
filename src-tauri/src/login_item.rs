// open at login. the default build asks the autostart plugin: a LaunchAgent on
// macOS, a Run key on Windows, an autostart entry on Linux. the Mac App Store
// build cannot write a LaunchAgent, because its sandbox only allows files inside
// the app's container, and launchd would never read one written there. it asks
// SMAppService instead, which needs macOS 13

use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Wry};

#[cfg(all(not(feature = "launch-agent"), not(target_os = "macos")))]
compile_error!("open at login needs the launch-agent feature outside macOS");

// the error set_enabled returns when the system waits for the user to allow
// the login item in System Settings
pub const NEEDS_APPROVAL: &str = "approve Nudge in System Settings > General > Login Items";

// what Settings shows for a failed change
pub fn problem_text(error: &str) -> String {
    if error == NEEDS_APPROVAL {
        "Open at login needs your approval in System Settings > General > Login Items.".into()
    } else {
        "Open at login couldn’t be changed.".into()
    }
}

#[cfg(feature = "launch-agent")]
pub fn plugin() -> TauriPlugin<Wry> {
    tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None)
}

#[cfg(not(feature = "launch-agent"))]
pub fn plugin() -> TauriPlugin<Wry> {
    tauri::plugin::Builder::new("login-item").build()
}

#[cfg(feature = "launch-agent")]
pub fn is_enabled(app: &AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|error| format!("{error:?}"))
}

#[cfg(feature = "launch-agent")]
pub fn set_enabled(app: &AppHandle, on: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let autostart = app.autolaunch();
    let result = if on { autostart.enable() } else { autostart.disable() };
    result.map_err(|error| format!("{error:?}"))
}

#[cfg(all(not(feature = "launch-agent"), target_os = "macos"))]
pub fn is_enabled(_: &AppHandle) -> Result<bool, String> {
    Ok(service::status()? == service::ENABLED)
}

#[cfg(all(not(feature = "launch-agent"), target_os = "macos"))]
pub fn set_enabled(_: &AppHandle, on: bool) -> Result<(), String> {
    service::set(on)
}

#[cfg(all(not(feature = "launch-agent"), target_os = "macos"))]
mod service {
    use super::NEEDS_APPROVAL;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSError;

    // SMAppService lives in ServiceManagement; linking it loads the class
    #[link(name = "ServiceManagement", kind = "framework")]
    extern "C" {}

    // SMAppServiceStatus: 0 not registered, 1 enabled, 2 needs the user's
    // approval in System Settings, 3 not found
    pub const ENABLED: isize = 1;
    const REQUIRES_APPROVAL: isize = 2;

    // looked up by name: no objc2 crate for ServiceManagement is in the lock file
    fn class() -> Result<&'static AnyClass, String> {
        AnyClass::get(c"SMAppService").ok_or_else(|| "SMAppService is missing".to_string())
    }

    fn main_app() -> Result<Retained<AnyObject>, String> {
        let class = class()?;
        // SAFETY: +mainAppService takes no arguments and returns an SMAppService
        let service: Option<Retained<AnyObject>> = unsafe { msg_send![class, mainAppService] };
        service.ok_or_else(|| "no login item service for this app".to_string())
    }

    fn status_of(service: &AnyObject) -> isize {
        // SAFETY: -status takes no arguments and returns an NSInteger
        unsafe { msg_send![service, status] }
    }

    pub fn status() -> Result<isize, String> {
        Ok(status_of(&*main_app()?))
    }

    // after the user switched the item off in System Settings, registering
    // again does not turn it on; they have to allow it there
    fn ask_for_approval() -> Result<(), String> {
        let class = class()?;
        // SAFETY: +openSystemSettingsLoginItems takes no arguments and returns nothing
        unsafe {
            let _: () = msg_send![class, openSystemSettingsLoginItems];
        }
        Err(NEEDS_APPROVAL.into())
    }

    pub fn set(on: bool) -> Result<(), String> {
        let service = main_app()?;
        let status = status_of(&service);
        if on {
            match status {
                ENABLED => return Ok(()),
                REQUIRES_APPROVAL => return ask_for_approval(),
                _ => {}
            }
            // SAFETY: the method takes only an NSError out-parameter and returns BOOL
            let result: Result<(), Retained<NSError>> =
                unsafe { msg_send![&*service, registerAndReturnError: _] };
            result.map_err(|error| error.localizedDescription().to_string())?;
            // registering can succeed while the item still waits for approval
            if status_of(&service) == REQUIRES_APPROVAL {
                return ask_for_approval();
            }
        } else if status == ENABLED || status == REQUIRES_APPROVAL {
            // SAFETY: the method takes only an NSError out-parameter and returns BOOL
            let result: Result<(), Retained<NSError>> =
                unsafe { msg_send![&*service, unregisterAndReturnError: _] };
            result.map_err(|error| error.localizedDescription().to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{problem_text, NEEDS_APPROVAL};

    #[test]
    fn a_login_item_waiting_for_approval_says_where_to_give_it() {
        assert!(problem_text(NEEDS_APPROVAL).contains("System Settings"));
    }

    #[test]
    fn any_other_failure_gets_the_plain_text() {
        assert_eq!(problem_text("Operation not permitted"), "Open at login couldn’t be changed.");
    }
}
