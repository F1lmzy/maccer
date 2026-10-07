//! macOS integration for maccer. Native services live here; core and UI stay portable.

mod application_icon;
mod applications;
pub mod fd_index;
mod file_metadata;
pub mod file_preview;
pub mod file_tools;
pub mod file_transfer;
mod hotkey;
#[cfg(target_os = "macos")]
mod image_thumbnail;
mod platform;
pub mod process;
pub mod spotlight;
mod thumbnail_cache;

pub use application_icon::application_icon;
pub use applications::{Application, discover_applications, discover_in};
pub use hotkey::{GlobalHotkey, is_activation_event, validate_hotkey};
pub use platform::{NativePlatform, Platform, configure_accessory_app, mouse_display_bounds};
