//! Safe Rust facade for connecting a frontend to an already-running Maccer service.
//!
//! This module only starts the IPC client actor. It never constructs the embedded
//! provider backend, reads configuration, or starts a service.

use crate::{
    IconCache, PixelIcon,
    frontend_client::FrontendControl,
    service_client::{IpcCommand, IpcHandle},
};
use serde_json::Value;
use std::{
    path::Path,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
};

const EVENT_CAPACITY: usize = 256;
const MAX_SOCKET_PATH_BYTES: usize = 4096;

/// Owned native icon pixels. Rows are top-to-bottom straight-alpha BGRA8.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IconPixels {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// One independent IPC session to a separately managed Maccer service.
///
/// Keep main request IDs strictly increasing across search, home, refresh and
/// activation. `cancel` advances the main watermark too, so skip an ID after
/// cancellation. Details use a separate increasing sequence and do not consume
/// or advance main request IDs.
pub struct ServiceConnection {
    events: Receiver<Value>,
    event_sender: SyncSender<Value>,
    ipc: IpcHandle,
    icon_cache: Arc<IconCache>,
    frontend: Option<FrontendControl>,
}

impl ServiceConnection {
    /// Connect to an existing absolute UTF-8 Unix socket path.
    ///
    /// Connection errors are delivered asynchronously as `error` events after
    /// this method succeeds. This constructor validates the path and starts the
    /// actor; it does not synchronously establish the socket connection.
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let Some(path_text) = path.to_str() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "service socket path must be UTF-8",
            ));
        };
        if !path.is_absolute() || path_text.is_empty() || path_text.len() > MAX_SOCKET_PATH_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "service socket path must be absolute and at most 4096 UTF-8 bytes",
            ));
        }
        let (sender, events) = mpsc::sync_channel(EVENT_CAPACITY);
        let icon_cache = Arc::new(IconCache::default());
        let ipc = IpcHandle::start(path, sender.clone(), icon_cache.clone())?;
        Ok(Self {
            events,
            event_sender: sender,
            ipc,
            icon_cache,
            frontend: None,
        })
    }

    /// Connect with an independent, negotiated presentation-control poller.
    /// Search and presentation use separate IPC connections and event handling.
    pub fn connect_frontend(
        path: &Path,
        kind: launcher_protocol::FrontendKind,
    ) -> std::io::Result<Self> {
        let mut connection = Self::connect(path)?;
        let controller = FrontendControl::start(path, kind, connection.event_sender.clone());
        connection.frontend = Some(controller?);
        Ok(connection)
    }

    /// Queue a search. Queue-full and closed-client errors are returned without blocking.
    pub fn search(&self, request: u64, query: String) -> Result<(), String> {
        self.ipc.send(IpcCommand::Search { request, query })
    }

    /// Invoke an action authorized by the specified displayed search snapshot.
    /// Use this for actions whose descriptor carried no `confirmation` prompt.
    pub fn activate(
        &self,
        request: u64,
        search_request: u64,
        provider: String,
        id: String,
        action: String,
    ) -> Result<(), String> {
        self.activate_with_confirmation(request, search_request, provider, id, action, false)
    }

    /// Invoke an action after the user explicitly confirmed the descriptor's
    /// `confirmation` prompt. The service still decides whether consent is
    /// required, so callers must only reach this after showing that prompt.
    pub fn activate_confirmed(
        &self,
        request: u64,
        search_request: u64,
        provider: String,
        id: String,
        action: String,
    ) -> Result<(), String> {
        self.activate_with_confirmation(request, search_request, provider, id, action, true)
    }

    fn activate_with_confirmation(
        &self,
        request: u64,
        search_request: u64,
        provider: String,
        id: String,
        action: String,
        confirmed: bool,
    ) -> Result<(), String> {
        self.ipc.send(IpcCommand::Activate {
            request,
            search_request,
            provider,
            id,
            action,
            confirmed,
        })
    }

    /// Fetch provider-owned safe details for a result in a delivered snapshot.
    /// Detail request IDs are independent of the main search/activation sequence.
    pub fn details(
        &self,
        request: u64,
        search_request: u64,
        provider: String,
        id: String,
    ) -> Result<(), String> {
        self.ipc.send(IpcCommand::Details {
            request,
            search_request,
            provider,
            id,
        })
    }

    /// Requery the same active view after an expired asset, without retrying actions.
    pub fn refresh(&self, request: u64, search_request: u64) -> Result<(), String> {
        self.ipc.send(IpcCommand::Refresh {
            request,
            search_request,
        })
    }

    /// Navigate the service session to its searchable home view.
    pub fn home(&self, request: u64) -> Result<(), String> {
        self.ipc.send(IpcCommand::Home { request })
    }

    /// Cancel pending query work. This does not cancel an accepted mutation.
    /// The actor advances its request watermark, so skip an ID before the next request.
    pub fn cancel(&self) -> Result<(), String> {
        self.ipc.send(IpcCommand::Cancel)
    }

    /// Return the next already-delivered JSON event without waiting.
    pub fn try_recv(&self) -> Option<Value> {
        self.events.try_recv().ok()
    }

    /// Copy a previously delivered `image://maccer-icons/<token>` icon from the local cache.
    pub fn icon(&self, token: &str) -> Option<IconPixels> {
        let pixels: PixelIcon = self.icon_cache.pixels(token)?;
        Some(IconPixels {
            bgra: pixels.bgra,
            width: pixels.width,
            height: pixels.height,
        })
    }
}

impl Drop for ServiceConnection {
    fn drop(&mut self) {
        if let Some(frontend) = &self.frontend {
            frontend.stop();
        }
        self.ipc.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_returns_an_owned_copy_from_the_shared_actor_cache() {
        let path = std::env::temp_dir().join(format!("maccer-missing-{}.sock", std::process::id()));
        let (sender, events) = mpsc::sync_channel(EVENT_CAPACITY);
        let cache = Arc::new(IconCache::default());
        let token = cache
            .insert(
                "fixture-icon".into(),
                PixelIcon {
                    bgra: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                },
            )
            .unwrap();
        let ipc = IpcHandle::start(&path, sender.clone(), cache.clone()).unwrap();
        let connection = ServiceConnection {
            events,
            event_sender: sender,
            ipc,
            icon_cache: cache,
            frontend: None,
        };

        let mut pixels = connection.icon(&token).unwrap();
        assert_eq!(pixels.bgra, [1, 2, 3, 4]);
        pixels.bgra[0] = 9;
        assert_eq!(connection.icon(&token).unwrap().bgra, [1, 2, 3, 4]);
    }
}
