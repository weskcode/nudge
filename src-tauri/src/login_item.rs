// open at login. the default build asks the autostart plugin: a LaunchAgent on
// macOS, a Run key on Windows, an autostart entry on Linux. the Mac App Store
// build cannot write a LaunchAgent, because its sandbox only allows files inside
// the app's container, and launchd would never read one written there. it asks
// SMAppService instead, which needs macOS 13

use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Wry};

#[cfg(all(not(feature = "launch-agent"), not(target_os = "macos")))]
compile_error!("open at login needs the launch-agent feature outside macOS");

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

    // the class is looked up by name so the app still starts on a macOS that
    // lacks it, and open at login then reports a failure
    fn main_app() -> Result<Retained<AnyObject>, String> {
        let class = AnyClass::get(c"SMAppService").ok_or("SMAppService needs macOS 13")?;
        // SAFETY: +mainAppService takes no arguments and returns an SMAppService
        let service: Option<Retained<AnyObject>> = unsafe { msg_send![class, mainAppService] };
        service.ok_or_else(|| "no login item service for this app".to_string())
    }

    pub fn status() -> Result<isize, String> {
        let service = main_app()?;
        // SAFETY: -status takes no arguments and returns an NSInteger
        Ok(unsafe { msg_send![&*service, status] })
    }

    pub fn set(on: bool) -> Result<(), String> {
        let service = main_app()?;
        // SAFETY: both methods take only an NSError out-parameter and return BOOL
        let result: Result<(), Retained<NSError>> = unsafe {
            if on {
                msg_send![&*service, registerAndReturnError: _]
            } else {
                msg_send![&*service, unregisterAndReturnError: _]
            }
        };
        result.map_err(|error| error.localizedDescription().to_string())?;
        // registering succeeds while the user still has to approve the item in
        // System Settings; until then it does not open at login
        if on && status()? == REQUIRES_APPROVAL {
            return Err("approve Nudge in System Settings > General > Login Items".into());
        }
        Ok(())
    }
}
