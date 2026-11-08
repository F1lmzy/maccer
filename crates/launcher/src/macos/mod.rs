//! macOS integration for maccer. Native services live here; core and UI stay portable.

mod application_icon;
mod application_watcher;
mod applications;
pub mod clipboard;
pub mod fd_index;
mod file_metadata;
pub mod file_preview;
pub mod file_tools;
pub mod file_transfer;
mod frontend_instance;
mod hotkey;
#[cfg(target_os = "macos")]
mod image_thumbnail;
mod platform;
pub mod process;
pub mod resident;
mod service_runtime;
pub mod spotlight;
mod thumbnail_cache;
pub mod web_preview;

pub use application_icon::{ApplicationIconPixels, application_icon, application_icon_pixels};
pub use application_watcher::ApplicationWatcher;
pub use applications::{Application, application_search_paths, discover_applications, discover_in};
pub use frontend_instance::FrontendInstance;
pub use hotkey::{GlobalHotkey, is_activation_event, validate_hotkey};
pub use platform::{NativePlatform, Platform, configure_accessory_app, mouse_display_bounds};
pub use service_runtime::{run_service_loop, run_service_loop_with};
pub use web_preview::{WebHost, WebPreview, WebPreviewEvent};

/// Bounded straight-alpha BGRA pixels for one local image, used by preview
/// assets. The caller owns the size class; this only decodes and caches nothing.
#[cfg(target_os = "macos")]
pub(crate) fn preview_pixels(
    path: &std::path::Path,
    max_edge: u32,
) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    image_thumbnail::thumbnail_pixels_sized(
        path,
        max_edge,
        &crate::core::CancellationToken::default(),
    )
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn preview_pixels(
    _path: &std::path::Path,
    _max_edge: u32,
) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    anyhow::bail!("preview assets require macOS")
}
