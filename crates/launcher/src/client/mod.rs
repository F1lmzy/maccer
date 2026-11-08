//! Unix-socket client for the standalone Maccer service.
use crate::protocol::{
    AssetData, AssetLimits, AssetMetadata, AssetRef, CloseViewParams, FrontendPollParams,
    FrontendPollResult, GetAssetParams, GetDetailsParams, HelloParams, HelloResult,
    InvokeActionParams, ItemDetails, OpenViewParams, OperationResult, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, ProviderDescriptor, QueryViewParams, Route, RpcError, RpcRequest, RpcResponse,
    ThemeState, ViewSnapshot, framing,
};
use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fmt,
    os::unix::net::UnixStream,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
/// Action invocation can run a bounded provider subprocess server-side (for
/// example a theme helper with a 30-second deadline), so it needs a wider read
/// window than ordinary calls. Every other method keeps [`SOCKET_TIMEOUT`].
const ACTION_TIMEOUT: Duration = Duration::from_secs(35);

/// A synchronous single-flight connection. The client never starts a service.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    hello: HelloResult,
}

impl Client {
    /// Connect to an existing service and negotiate protocol version 1.
    pub fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let mut stream = UnixStream::connect(path.as_ref()).with_context(|| {
            format!(
                "connecting to Maccer service at {}",
                path.as_ref().display()
            )
        })?;
        stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
        let id = next_id();
        let request = RpcRequest::new(
            id.clone(),
            "hello",
            serde_json::to_value(HelloParams {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
                client_name: "maccer-client".into(),
            })?,
        );
        send(&mut stream, &request)?;
        let response = receive(&mut stream, &id)?;
        let hello: HelloResult = result(response)?;
        if hello.major != PROTOCOL_MAJOR {
            bail!(
                "service protocol major {} is incompatible with client major {}",
                hello.major,
                PROTOCOL_MAJOR
            );
        }
        if hello.instance_id.is_empty() || hello.session_id.is_empty() {
            bail!("service returned an empty instance or session ID");
        }
        Ok(Self { stream, hello })
    }

    pub fn hello(&self) -> &HelloResult {
        &self.hello
    }

    /// Send one request and decode its result. Calls on a client are serialized.
    pub fn request<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T> {
        if method == "assets.get" {
            bail!("use get_asset for the binary assets.get response");
        }
        let id = next_id();
        let request = RpcRequest::new(id.clone(), method, serde_json::to_value(params)?);
        send(&mut self.stream, &request)?;
        let response = receive(&mut self.stream, &id)?;
        result(response)
    }

    pub fn providers(&mut self) -> Result<Vec<ProviderDescriptor>> {
        self.request("providers.list", serde_json::json!({}))
    }

    pub fn open_view(&mut self, route: Route, query: &str, limit: usize) -> Result<ViewSnapshot> {
        self.request(
            "views.open",
            OpenViewParams {
                route,
                query: query.into(),
                limit,
            },
        )
    }

    pub fn query_view(&mut self, view_id: &str, query: &str) -> Result<ViewSnapshot> {
        self.request(
            "views.query",
            QueryViewParams {
                view_id: view_id.into(),
                query: query.into(),
            },
        )
    }

    pub fn close_view(&mut self, view_id: &str) -> Result<()> {
        self.request(
            "views.close",
            CloseViewParams {
                view_id: view_id.into(),
            },
        )
    }

    pub fn invoke(&mut self, params: &InvokeActionParams) -> Result<OperationResult> {
        self.stream.set_read_timeout(Some(ACTION_TIMEOUT))?;
        let result = self.request("actions.invoke", params);
        self.stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        result
    }

    /// Poll the negotiated frontend presentation controller. This method does not
    /// issue searches or authorize actions; callers should use a dedicated connection.
    pub fn poll_frontend(&mut self, params: &FrontendPollParams) -> Result<FrontendPollResult> {
        if !self
            .hello
            .capabilities
            .iter()
            .any(|capability| capability == "frontendControl")
        {
            bail!("service did not negotiate frontendControl");
        }
        self.request("frontend.poll", params)
    }

    /// Read the current active theme state. Independent of views and never a mutation.
    pub fn theme_state(&mut self) -> Result<ThemeState> {
        if !self
            .hello
            .capabilities
            .iter()
            .any(|capability| capability == "themeState")
        {
            bail!("service did not negotiate themeState");
        }
        let state: ThemeState = self.request("state.theme", serde_json::json!({}))?;
        state.validate().map_err(anyhow::Error::msg)?;
        Ok(state)
    }

    /// Fetch safe, provider-projected details for a result in the current snapshot.
    pub fn get_details(&mut self, params: &GetDetailsParams) -> Result<ItemDetails> {
        if !self
            .hello
            .capabilities
            .iter()
            .any(|capability| capability == "itemDetails")
        {
            bail!("service did not negotiate itemDetails");
        }
        let details: ItemDetails = self.request("views.details", params)?;
        if details.view_id != params.view_id
            || details.revision != params.revision
            || details.provider_id != params.provider_id
            || details.item_id != params.item_id
        {
            bail!("service details identity does not match request");
        }
        Ok(details)
    }

    /// Fetches one negotiated, bounded BGRA8 asset in the representation the
    /// reference advertises. Any malformed response poisons the single-flight
    /// stream, so the connection is closed before returning an error.
    pub fn get_asset(&mut self, asset: &AssetRef) -> Result<AssetData> {
        let (representation, limits) = asset
            .representations
            .iter()
            .find_map(|name| {
                AssetLimits::for_representation(name).map(|limits| (name.clone(), limits))
            })
            .ok_or_else(|| {
                anyhow::anyhow!("asset does not advertise a supported representation")
            })?;
        if !self
            .hello
            .capabilities
            .iter()
            .any(|c| c == limits.capability())
        {
            bail!(
                "service did not negotiate the {} capability",
                limits.capability()
            );
        }
        let id = next_id();
        let request = RpcRequest::new(
            id.clone(),
            "assets.get",
            serde_json::to_value(GetAssetParams {
                asset_id: asset.id.clone(),
                revision: asset.revision.clone(),
                representation: representation.clone(),
            })?,
        );
        if let Err(error) = send(&mut self.stream, &request) {
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
            return Err(error);
        }
        let response = match receive(&mut self.stream, &id) {
            Ok(response) => response,
            Err(error) => {
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
                return Err(error);
            }
        };
        if let Some(error) = response.error {
            return Err(remote_error(error));
        }
        let result = (|| -> Result<AssetData> {
            let metadata: AssetMetadata =
                serde_json::from_value(response.result.expect("validated response has result"))?;
            validate_metadata(&metadata, asset, &representation, limits)?;
            let bytes = framing::read_bounded_frame(&mut self.stream, limits.max_bytes)?;
            if bytes.len() != metadata.byte_length {
                bail!("asset frame length does not match metadata");
            }
            Ok(AssetData { metadata, bytes })
        })();
        if result.is_err() {
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
        result
    }
}

fn validate_metadata(
    metadata: &AssetMetadata,
    asset: &AssetRef,
    representation: &str,
    limits: AssetLimits,
) -> Result<()> {
    if metadata.asset_id != asset.id
        || metadata.revision != asset.revision
        || metadata.representation != representation
        || metadata.alpha_mode != "straight"
        || metadata.row_order != "topToBottom"
        || metadata.width == 0
        || metadata.height == 0
        || metadata.width > limits.max_edge
        || metadata.height > limits.max_edge
    {
        bail!("invalid asset metadata identity or format");
    }
    let stride = metadata
        .width
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("invalid asset stride"))?;
    let length = (stride as usize)
        .checked_mul(metadata.height as usize)
        .ok_or_else(|| anyhow::anyhow!("invalid asset size"))?;
    if metadata.stride != stride || metadata.byte_length != length || length > limits.max_bytes {
        bail!("invalid asset metadata dimensions or length");
    }
    Ok(())
}

fn next_id() -> String {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed).to_string()
}
fn send(stream: &mut UnixStream, request: &RpcRequest) -> Result<()> {
    let bytes = serde_json::to_vec(request)?;
    framing::write_frame(stream, &bytes).context("writing Maccer service request")
}
fn receive(stream: &mut UnixStream, expected_id: &str) -> Result<RpcResponse> {
    let bytes = framing::read_frame(stream).context("reading Maccer service response")?;
    let response: RpcResponse =
        serde_json::from_slice(&bytes).context("decoding Maccer service response")?;
    response.validate().map_err(anyhow::Error::msg)?;
    if response.id != expected_id {
        bail!(
            "service response ID {:?} does not match request ID {:?}",
            response.id,
            expected_id
        );
    }
    Ok(response)
}
fn result<T: DeserializeOwned>(response: RpcResponse) -> Result<T> {
    if let Some(error) = response.error {
        return Err(remote_error(error));
    }
    serde_json::from_value(
        response
            .result
            .expect("validated JSON-RPC response has a result"),
    )
    .context("decoding Maccer service result")
}
/// A structured error returned by the service, preserved for callers that need stable codes.
#[derive(Debug)]
pub struct RemoteError(pub RpcError);
impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Maccer service error {} ({}): {}",
            self.0.code, self.0.data.code, self.0.message
        )
    }
}
impl std::error::Error for RemoteError {}

fn remote_error(error: RpcError) -> anyhow::Error {
    anyhow::Error::new(RemoteError(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR, framing};
    use serde_json::Value;
    use std::{io::Read, os::unix::net::UnixListener, thread};
    use tempfile::tempdir;

    fn fixture_hello(id: &str, major: u16) -> RpcResponse {
        RpcResponse::success(
            id,
            serde_json::json!({
                "major": major, "minor": PROTOCOL_MINOR,
                "instanceId": "instance-1", "sessionId": "session-1",
                "capabilities": ["views"]
            }),
        )
    }
    fn server(path: &Path, responses: Vec<RpcResponse>) -> thread::JoinHandle<Vec<Value>> {
        let listener = UnixListener::bind(path).unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut requests = Vec::new();
            for response in responses {
                let bytes = framing::read_frame(&mut stream).unwrap();
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                let id = request["id"].as_str().unwrap().to_owned();
                requests.push(request);
                let response = if response.id.starts_with("$request") {
                    RpcResponse { id, ..response }
                } else {
                    response
                };
                framing::write_frame(&mut stream, &serde_json::to_vec(&response).unwrap()).unwrap();
            }
            requests
        })
    }
    fn ok(result: Value) -> RpcResponse {
        RpcResponse::success("$request-result", result)
    }

    #[test]
    fn frontend_poll_requires_capability_and_sends_typed_request() {
        use crate::protocol::{FrontendKind, FrontendPollParams};
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello_bytes = framing::read_frame(&mut stream).unwrap();
            let hello: Value = serde_json::from_slice(&hello_bytes).unwrap();
            let hello_result = serde_json::json!({
                "major":1,"minor":5,"instanceId":"i","sessionId":"s",
                "capabilities":["frontendControl"]
            });
            framing::write_frame(
                &mut stream,
                &serde_json::to_vec(&RpcResponse::success(
                    hello["id"].as_str().unwrap(),
                    hello_result,
                ))
                .unwrap(),
            )
            .unwrap();
            let poll_bytes = framing::read_frame(&mut stream).unwrap();
            let poll: Value = serde_json::from_slice(&poll_bytes).unwrap();
            let result = serde_json::json!({"registered":true,"show":"serial-1"});
            framing::write_frame(
                &mut stream,
                &serde_json::to_vec(&RpcResponse::success(poll["id"].as_str().unwrap(), result))
                    .unwrap(),
            )
            .unwrap();
            poll
        });
        let mut client = Client::connect(&path).unwrap();
        let result = client
            .poll_frontend(&FrontendPollParams {
                kind: FrontendKind::Pill,
                ack: None,
            })
            .unwrap();
        assert_eq!(
            result,
            crate::protocol::FrontendPollResult {
                registered: true,
                show: Some("serial-1".into())
            }
        );
        let request = worker.join().unwrap();
        assert_eq!(request["method"], "frontend.poll");
        assert_eq!(request["params"], serde_json::json!({"kind":"pill"}));
    }

    #[test]
    fn frontend_poll_is_not_sent_without_negotiated_capability() {
        use crate::protocol::{FrontendKind, FrontendPollParams};
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let worker = server(&path, vec![fixture_hello("$request", PROTOCOL_MAJOR)]);
        let mut client = Client::connect(&path).unwrap();
        assert!(
            client
                .poll_frontend(&FrontendPollParams {
                    kind: FrontendKind::Gpui,
                    ack: None
                })
                .is_err()
        );
        assert_eq!(worker.join().unwrap().len(), 1, "only hello may be sent");
    }

    #[test]
    fn connects_negotiates_and_round_trips_provider_request() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let thread = server(
            &path,
            vec![
                fixture_hello("$request", PROTOCOL_MAJOR),
                ok(serde_json::json!([])),
            ],
        );
        let mut client = Client::connect(&path).unwrap();
        assert_eq!(client.hello().session_id, "session-1");
        let providers: Vec<ProviderDescriptor> = client.providers().unwrap();
        assert!(providers.is_empty());
        let requests = thread.join().unwrap();
        assert_eq!(requests[0]["method"], "hello");
        assert_eq!(requests[0]["params"]["clientName"], "maccer-client");
        assert_eq!(requests[1]["method"], "providers.list");
    }

    #[test]
    fn get_details_requires_capability_and_validates_correlated_identity() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let thread = server(&path, vec![fixture_hello("$request", PROTOCOL_MAJOR)]);
        let mut client = Client::connect(&path).unwrap();
        let params = GetDetailsParams {
            view_id: "view-1".into(),
            revision: "1".into(),
            provider_id: "apps".into(),
            item_id: "app-1".into(),
        };
        assert!(
            client
                .get_details(&params)
                .unwrap_err()
                .to_string()
                .contains("did not negotiate itemDetails")
        );
        assert_eq!(
            thread.join().unwrap().len(),
            1,
            "capability rejection must not send a method request"
        );

        let path = dir.path().join("details.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            framing::write_frame(&mut stream, &serde_json::to_vec(&RpcResponse::success(
                hello["id"].as_str().unwrap(), serde_json::json!({"major":1,"minor":2,"instanceId":"i","sessionId":"s","capabilities":["itemDetails"]})
            )).unwrap()).unwrap();
            let request: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            assert_eq!(request["method"], "views.details");
            let result = serde_json::json!({"viewId":"view-1","revision":"1","providerId":"apps","itemId":"app-1","content":[]});
            framing::write_frame(
                &mut stream,
                &serde_json::to_vec(&RpcResponse::success(
                    request["id"].as_str().unwrap(),
                    result,
                ))
                .unwrap(),
            )
            .unwrap();
        });
        let mut client = Client::connect(&path).unwrap();
        let details = client.get_details(&params).unwrap();
        assert_eq!(details.item_id, "app-1");
        thread.join().unwrap();
    }

    #[test]
    fn get_details_rejects_response_for_another_item() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            framing::write_frame(&mut stream, &serde_json::to_vec(&RpcResponse::success(
                hello["id"].as_str().unwrap(), serde_json::json!({"major":1,"minor":2,"instanceId":"i","sessionId":"s","capabilities":["itemDetails"]})
            )).unwrap()).unwrap();
            let request: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            let result = serde_json::json!({"viewId":"view-1","revision":"1","providerId":"apps","itemId":"different","content":[]});
            framing::write_frame(
                &mut stream,
                &serde_json::to_vec(&RpcResponse::success(
                    request["id"].as_str().unwrap(),
                    result,
                ))
                .unwrap(),
            )
            .unwrap();
        });
        let mut client = Client::connect(&path).unwrap();
        let params = GetDetailsParams {
            view_id: "view-1".into(),
            revision: "1".into(),
            provider_id: "apps".into(),
            item_id: "app-1".into(),
        };
        assert!(
            client
                .get_details(&params)
                .unwrap_err()
                .to_string()
                .contains("identity does not match")
        );
        thread.join().unwrap();
    }

    fn valid_theme_payload() -> Value {
        use crate::protocol::{
            THEME_BAR_KEYS, THEME_BORDER_KEYS, THEME_PALETTE_KEYS, THEME_TOKEN_KEYS,
        };
        let map = |keys: &[&str]| {
            Value::Object(
                keys.iter()
                    .map(|key| ((*key).to_owned(), serde_json::json!("#101112")))
                    .collect(),
            )
        };
        serde_json::json!({
            "revision": "r1",
            "available": true,
            "palette": {
                "schema": 1,
                "slug": "test",
                "name": "Test",
                "variant": "dark",
                "alpha_byte": 230,
                "palette": map(&THEME_PALETTE_KEYS),
                "tokens": map(&THEME_TOKEN_KEYS),
                "border": map(&THEME_BORDER_KEYS),
                "bar": map(&THEME_BAR_KEYS),
            },
            "error": Value::Null,
        })
    }

    #[test]
    fn theme_state_requires_capability_and_validates_the_payload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let thread = server(&path, vec![fixture_hello("$request", PROTOCOL_MAJOR)]);
        let mut client = Client::connect(&path).unwrap();
        assert!(
            client
                .theme_state()
                .unwrap_err()
                .to_string()
                .contains("did not negotiate themeState")
        );
        thread.join().unwrap();

        let path = dir.path().join("theme.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let good = valid_theme_payload();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            framing::write_frame(&mut stream, &serde_json::to_vec(&RpcResponse::success(
                hello["id"].as_str().unwrap(), serde_json::json!({"major":1,"minor":3,"instanceId":"i","sessionId":"s","capabilities":["themeState"]})
            )).unwrap()).unwrap();
            let request: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            assert_eq!(request["method"], "state.theme");
            assert_eq!(request["params"], serde_json::json!({}));
            framing::write_frame(
                &mut stream,
                &serde_json::to_vec(&RpcResponse::success(request["id"].as_str().unwrap(), good))
                    .unwrap(),
            )
            .unwrap();
        });
        let mut client = Client::connect(&path).unwrap();
        let state = client.theme_state().unwrap();
        assert!(state.available);
        assert_eq!(state.revision, "r1");
        assert_eq!(state.palette.unwrap().slug, "test");
        thread.join().unwrap();
    }

    #[test]
    fn asset_fetch_correlates_metadata_and_reads_bounded_binary_frame() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            framing::write_frame(&mut stream, &serde_json::to_vec(&RpcResponse::success(hello["id"].as_str().unwrap(), serde_json::json!({"major":1,"minor":1,"instanceId":"i","sessionId":"s","capabilities":["iconAssetsBgra8"]}))).unwrap()).unwrap();
            let request: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            assert_eq!(request["method"], "assets.get");
            assert_eq!(request["params"]["representation"], "bgra8");
            let metadata = AssetMetadata {
                asset_id: "opaque".into(),
                revision: "4".into(),
                representation: "bgra8".into(),
                width: 1,
                height: 1,
                stride: 4,
                alpha_mode: "straight".into(),
                row_order: "topToBottom".into(),
                byte_length: 4,
            };
            framing::write_frame(
                &mut stream,
                &serde_json::to_vec(&RpcResponse::success(
                    request["id"].as_str().unwrap(),
                    serde_json::to_value(metadata).unwrap(),
                ))
                .unwrap(),
            )
            .unwrap();
            framing::write_frame(&mut stream, &[10, 20, 30, 40]).unwrap();
        });
        let mut client = Client::connect(&path).unwrap();
        let asset = AssetRef {
            id: "opaque".into(),
            revision: "4".into(),
            representations: vec!["bgra8".into()],
        };
        let data = client.get_asset(&asset).unwrap();
        assert_eq!(data.bytes, [10, 20, 30, 40]);
        thread.join().unwrap();
    }

    #[test]
    fn malformed_asset_metadata_closes_desynchronized_connection_before_binary_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            framing::write_frame(&mut stream, &serde_json::to_vec(&RpcResponse::success(hello["id"].as_str().unwrap(), serde_json::json!({"major":1,"minor":1,"instanceId":"i","sessionId":"s","capabilities":["iconAssetsBgra8"]}))).unwrap()).unwrap();
            let request: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            let metadata = serde_json::json!({"assetId":"opaque","revision":"4","representation":"bgra8","width":65,"height":1,"stride":260,"alphaMode":"straight","rowOrder":"topToBottom","byteLength":260});
            framing::write_frame(
                &mut stream,
                &serde_json::to_vec(&RpcResponse::success(
                    request["id"].as_str().unwrap(),
                    metadata,
                ))
                .unwrap(),
            )
            .unwrap();
            let mut byte = [0];
            assert_eq!(
                stream.read(&mut byte).unwrap(),
                0,
                "client should close rather than read an untrusted binary frame"
            );
        });
        let mut client = Client::connect(&path).unwrap();
        let asset = AssetRef {
            id: "opaque".into(),
            revision: "4".into(),
            representations: vec!["bgra8".into()],
        };
        assert!(client.get_asset(&asset).is_err());
        thread.join().unwrap();
    }

    #[test]
    fn rejects_incompatible_service_protocol_major() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let thread = server(&path, vec![fixture_hello("$request", PROTOCOL_MAJOR + 1)]);
        let error = Client::connect(&path).unwrap_err().to_string();
        assert!(error.contains("incompatible"));
        thread.join().unwrap();
    }

    #[test]
    fn rejects_mismatched_response_id() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let thread = server(
            &path,
            vec![
                fixture_hello("$request", PROTOCOL_MAJOR),
                RpcResponse::success("wrong", serde_json::json!([])),
            ],
        );
        let mut client = Client::connect(&path).unwrap();
        let error = client.providers().unwrap_err().to_string();
        assert!(error.contains("does not match"));
        thread.join().unwrap();
    }

    #[test]
    fn rejects_invalid_json_rpc_response() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let thread = server(
            &path,
            vec![fixture_hello("$request", PROTOCOL_MAJOR), {
                let mut invalid = RpcResponse::success("$request-result", serde_json::json!([]));
                invalid.error = Some(RpcError::new(-1, "BROKEN", "broken"));
                invalid
            }],
        );
        let mut client = Client::connect(&path).unwrap();
        assert!(
            client
                .providers()
                .unwrap_err()
                .to_string()
                .contains("invalid JSON-RPC response")
        );
        thread.join().unwrap();
    }

    #[test]
    fn remote_errors_surface_stable_code_and_message() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let thread = server(
            &path,
            vec![
                fixture_hello("$request", PROTOCOL_MAJOR),
                RpcResponse::failure(
                    "$request-result",
                    RpcError::new(-32000, "NO_PROVIDER", "not found"),
                ),
            ],
        );
        let mut client = Client::connect(&path).unwrap();
        let error = client.providers().unwrap_err();
        assert!(
            error.to_string().contains("NO_PROVIDER") && error.to_string().contains("not found")
        );
        assert_eq!(
            error.downcast_ref::<RemoteError>().unwrap().0.data.code,
            "NO_PROVIDER"
        );
        thread.join().unwrap();
    }

    #[test]
    fn invoke_waits_longer_than_ordinary_calls_and_then_restores_the_short_window() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            framing::write_frame(&mut stream, &serde_json::to_vec(&RpcResponse::success(
                hello["id"].as_str().unwrap(), serde_json::json!({"major":1,"minor":4,"instanceId":"i","sessionId":"s","capabilities":["actionConfirmation"]})
            )).unwrap()).unwrap();
            let invoke: Value =
                serde_json::from_slice(&framing::read_frame(&mut stream).unwrap()).unwrap();
            assert_eq!(invoke["method"], "actions.invoke");
            assert_eq!(invoke["params"]["confirmed"], true);
            // Longer than the ordinary 5s read window but well inside the action window.
            thread::sleep(Duration::from_millis(5_400));
            framing::write_frame(
                &mut stream,
                &serde_json::to_vec(&RpcResponse::success(
                    invoke["id"].as_str().unwrap(),
                    serde_json::json!({"operationId":"op-1","status":"succeeded","content":[]}),
                ))
                .unwrap(),
            )
            .unwrap();
            // Consume the follow-up request and stall: the client must give up at 5s again.
            let _ = framing::read_frame(&mut stream);
            thread::sleep(Duration::from_millis(5_400));
        });
        let mut client = Client::connect(&path).unwrap();
        let params = InvokeActionParams {
            view_id: "view-1".into(),
            revision: "1".into(),
            action_ref: "ref-1".into(),
            operation_id: "op-1".into(),
            confirmed: true,
            input: Default::default(),
        };
        client.invoke(&params).unwrap();
        assert!(
            client.providers().is_err(),
            "ordinary calls must return to the five-second read window after invoke"
        );
        thread.join().unwrap();
    }

    #[test]
    fn only_requests_are_accepted_from_server_and_missing_socket_does_not_spawn_one() {
        let dir = tempdir().unwrap();
        let error = Client::connect(dir.path().join("missing.sock"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("connecting to Maccer service"));
    }
}
