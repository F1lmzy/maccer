//! Resident app controls. Login state is owned by ServiceManagement, not TOML.
use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentEvent {
    OpenLauncher,
    ToggleLaunchAtLogin,
    Quit,
    ToggleLauncher,
    HotkeyFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
    Unavailable,
}

impl LoginStatus {
    fn toggle_enables(self) -> Result<bool> {
        match self {
            Self::NotRegistered | Self::NotFound => Ok(true),
            Self::Enabled | Self::RequiresApproval => Ok(false),
            Self::Unavailable => {
                bail!("Launch at Login requires macOS 13 or newer and a signed maccer.app bundle.")
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[path = "resident/native.rs"]
mod native;

#[cfg(target_os = "macos")]
pub use native::{ResidentMenu, show_error_alert};

#[cfg(not(target_os = "macos"))]
pub struct ResidentMenu;
#[cfg(not(target_os = "macos"))]
impl ResidentMenu {
    pub fn new(_: async_channel::Sender<ResidentEvent>) -> Result<Self> {
        bail!("menu bar controls require macOS")
    }
    pub fn new_frontend(_: async_channel::Sender<ResidentEvent>) -> Result<Self> {
        bail!("menu bar controls require macOS")
    }
    pub fn toggle_launch_at_login(&self) -> Result<()> {
        LoginStatus::Unavailable.toggle_enables().map(|_| ())
    }
}
#[cfg(not(target_os = "macos"))]
pub fn show_error_alert(_: &str, _: &str) {}

#[cfg(all(test, target_os = "macos"))]
pub fn native_menu_smoke() {
    native::menu_smoke();
    native::frontend_menu_smoke();
}

#[cfg(all(test, target_os = "macos"))]
pub fn native_alert_smoke(cx: &mut gpui::App) {
    native::alert_smoke(cx);
}

#[cfg(test)]
mod tests {
    #[test]
    fn login_toggle_uses_system_state_not_a_local_preference() {
        use super::LoginStatus;
        assert!(LoginStatus::NotRegistered.toggle_enables().unwrap());
        assert!(LoginStatus::NotFound.toggle_enables().unwrap());
        assert!(!LoginStatus::Enabled.toggle_enables().unwrap());
        // A registered item awaiting consent must still be removable.
        assert!(!LoginStatus::RequiresApproval.toggle_enables().unwrap());
        assert!(LoginStatus::Unavailable.toggle_enables().is_err());
    }
}
