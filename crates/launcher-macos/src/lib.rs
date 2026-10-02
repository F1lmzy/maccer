//! macOS integration for maccer. Native services live here; core and UI stay portable.

mod applications;
mod hotkey;
mod platform;
pub mod process;
pub mod spotlight;

pub use applications::{Application, discover_applications, discover_in};
pub use hotkey::{GlobalHotkey, is_activation_event, validate_hotkey};
pub use platform::{NativePlatform, Platform, configure_accessory_app, mouse_display_bounds};
