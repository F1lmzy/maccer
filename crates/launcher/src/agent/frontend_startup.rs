//! Native application startup admission. Acquire before creating windows and
//! retain through process shutdown. Normal IPC clients remain unrestricted.
use crate::macos::FrontendInstance;
use crate::protocol::{FrontendKind, FrontendPollParams};
use anyhow::{Context, Result, bail};
use std::{
    ffi::{CStr, c_char},
    path::Path,
};

/// Blocking startup-only preflight, not a UI-thread search operation. The
/// lifetime lock excludes both native frontend kinds, including across daemon
/// restarts and control-lease expiry. The short probe connection releases its
/// presentation lease on disconnect; the normal control worker renews its own.
pub fn claim(socket: &Path, kind: &str) -> Result<FrontendInstance> {
    let kind = match kind {
        "gpui" => FrontendKind::Gpui,
        "pill" => FrontendKind::Pill,
        _ => bail!("frontend must be gpui or pill"),
    };
    let instance = FrontendInstance::claim(socket)?.context(
        "a frontend for this service is already running; quit it before switching frontends",
    )?;
    let mut client = crate::client::Client::connect(socket)
        .context("connecting to the existing backend for frontend admission")?;
    if client
        .hello()
        .capabilities
        .iter()
        .any(|capability| capability == "frontendControl")
        && !client
            .poll_frontend(&FrontendPollParams { kind, ack: None })?
            .registered
    {
        bail!(
            "this is not the selected frontend, or its presentation lease is occupied; use the backend shortcut"
        );
    }
    Ok(instance)
}

pub struct MaccerFrontendClaim {
    _instance: FrontendInstance,
}

/// Acquire startup admission and return an opaque lifetime guard, or null.
/// `error`, when non-null, receives an owned string freed with
/// `maccer_agent_string_free`. No window, provider or backend is created.
///
/// # Safety
/// Inputs must be valid NUL-terminated strings. `error` must be null or writable
/// for one pointer. Retain the returned guard until the native app exits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_claim_frontend(
    socket: *const c_char,
    kind: *const c_char,
    error: *mut *mut c_char,
) -> *mut MaccerFrontendClaim {
    if !error.is_null() {
        unsafe {
            *error = std::ptr::null_mut();
        }
    }
    let result = std::panic::catch_unwind(|| -> Result<FrontendInstance> {
        if socket.is_null() || kind.is_null() {
            bail!("frontend socket and kind are required");
        }
        let socket = unsafe { CStr::from_ptr(socket) }.to_str()?;
        let kind = unsafe { CStr::from_ptr(kind) }.to_str()?;
        claim(Path::new(socket), kind)
    })
    .unwrap_or_else(|_| Err(anyhow::anyhow!("frontend admission panicked")));
    match result {
        Ok(instance) => Box::into_raw(Box::new(MaccerFrontendClaim {
            _instance: instance,
        })),
        Err(message) => {
            if !error.is_null() {
                unsafe {
                    *error = crate::agent::c_string(format!("{message:#}"));
                }
            }
            std::ptr::null_mut()
        }
    }
}

/// Release a startup claim. Null is a no-op.
///
/// # Safety
/// A non-null pointer must be a live claim returned by
/// `maccer_agent_claim_frontend`, and may be freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_frontend_claim_free(claim: *mut MaccerFrontendClaim) {
    if !claim.is_null() {
        unsafe {
            drop(Box::from_raw(claim));
        }
    }
}
