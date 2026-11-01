//! Native regression: dispatch a process-local Carbon hotkey event through the
//! backend event loop. No global shortcut is registered and no keyboard input
//! is injected into the desktop.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    };

    // Installing the dependency's handler does not register any system hotkey.
    let _manager = GlobalHotKeyManager::new()?;
    let stop = AtomicBool::new(false);
    let mut posted_at = None;
    let mut received = false;
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(30));
        eprintln!("service hotkey regression timed out");
        std::process::exit(2);
    });
    launcher_macos::run_service_loop_with(&stop, || {
        if posted_at.is_none() {
            post_hotkey_event().expect("posting a process-local Carbon hotkey event");
            posted_at = Some(Instant::now());
        }
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.id == TEST_ID && event.state == HotKeyState::Pressed {
                received = true;
                stop.store(true, Ordering::Release);
            }
        }
        if posted_at.unwrap().elapsed() > Duration::from_secs(2) {
            stop.store(true, Ordering::Release);
        }
    })?;
    anyhow::ensure!(
        received,
        "service loop did not dispatch the Carbon hotkey event"
    );
    println!("service hotkey regression passed: Carbon event dispatched without desktop input");
    Ok(())
}

#[cfg(target_os = "macos")]
const TEST_ID: u32 = 0x4d43_4352;

#[cfg(target_os = "macos")]
fn post_hotkey_event() -> anyhow::Result<()> {
    use std::{ffi::c_void, mem::size_of, ptr};

    #[repr(C)]
    struct EventHotKeyId {
        signature: u32,
        id: u32,
    }
    // SDK: HIToolbox/CarbonEventsCore.h (CreateEvent, SetEventParameter,
    // GetMainEventQueue, PostEventToQueue, ReleaseEvent). Posting retains the
    // event and only the receiving process's main queue is affected.
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        #[link_name = "CreateEvent"]
        fn create_event(
            allocator: *const c_void,
            class: u32,
            kind: u32,
            when: f64,
            attributes: u32,
            event: *mut *mut c_void,
        ) -> i32;
        #[link_name = "SetEventParameter"]
        fn set_event_parameter(
            event: *mut c_void,
            name: u32,
            kind: u32,
            size: u32,
            data: *const c_void,
        ) -> i32;
        #[link_name = "GetMainEventQueue"]
        fn get_main_event_queue() -> *mut c_void;
        #[link_name = "PostEventToQueue"]
        fn post_event_to_queue(queue: *mut c_void, event: *mut c_void, priority: i16) -> i32;
        #[link_name = "ReleaseEvent"]
        fn release_event(event: *mut c_void);
    }
    let id = EventHotKeyId {
        // The signature used by global-hotkey 0.8's macOS registration.
        signature: u32::from_be_bytes(*b"htrs"),
        id: TEST_ID,
    };
    let mut event = ptr::null_mut();
    // SAFETY: Valid out-pointer and SDK constants; CreateEvent initializes a
    // retained event which is released below even if setting/posting fails.
    let status = unsafe {
        create_event(
            ptr::null(),
            u32::from_be_bytes(*b"keyb"),
            5,
            0.0,
            0,
            &mut event,
        )
    };
    anyhow::ensure!(status == 0, "CreateEvent failed: {status}");
    // SAFETY: event is valid after successful CreateEvent; id is a correctly
    // sized EventHotKeyID. The main queue exists on this process's main thread.
    let status = unsafe {
        let status = set_event_parameter(
            event,
            u32::from_be_bytes(*b"----"),
            u32::from_be_bytes(*b"hkid"),
            size_of::<EventHotKeyId>() as u32,
            ptr::from_ref(&id).cast(),
        );
        let status = if status == 0 {
            post_event_to_queue(get_main_event_queue(), event, 1)
        } else {
            status
        };
        release_event(event);
        status
    };
    anyhow::ensure!(status == 0, "queueing Carbon event failed: {status}");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("service hotkey regression skipped: requires macOS");
}
