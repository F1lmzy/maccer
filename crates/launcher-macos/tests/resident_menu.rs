//! Cocoa objects require the real main thread, so this test has no Rust harness.
#[cfg(target_os = "macos")]
#[path = "../src/resident.rs"]
pub mod resident;

fn main() {
    #[cfg(target_os = "macos")]
    {
        gpui::Application::new().run(|cx| {
            resident::native_menu_smoke();
            resident::native_alert_smoke(cx);
        });
    }
}
