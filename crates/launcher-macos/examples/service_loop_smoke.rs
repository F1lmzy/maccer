//! Read-only smoke test for the shared service main run loop.
//!
//! Verifies that work submitted to the libdispatch main queue from a worker
//! thread executes while `launcher_macos::run_service_loop` pumps the main run
//! loop, and that the loop exits once the stop flag is set. It performs no
//! application launch, clipboard, reveal, hotkey or personal-state changes.
//!
//! cargo run -p launcher-macos --example service_loop_smoke

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    // Fail loudly instead of hanging if the main queue is never drained.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(10));
        eprintln!("service loop smoke timed out");
        std::process::exit(2);
    });

    let callback_ran = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let ran = callback_ran.clone();
    let stop_from_worker = stop.clone();
    std::thread::spawn(move || {
        // Let the main loop enter its first pass before queuing.
        std::thread::sleep(Duration::from_millis(200));
        dispatch2::DispatchQueue::main().exec_async(move || {
            ran.store(true, Ordering::Release);
            stop_from_worker.store(true, Ordering::Release);
        });
    });

    launcher_macos::run_service_loop(&stop)?;
    assert!(
        callback_ran.load(Ordering::Acquire),
        "main queue work never ran on the service main thread"
    );
    println!("service loop smoke passed: main queue work executed and the loop exited");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("service loop smoke skipped: requires macOS");
}
