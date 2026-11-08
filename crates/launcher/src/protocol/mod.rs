//! Frontend-neutral DTOs for Maccer's local service. See `docs/provider-service-spec.md`.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 10;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_ICON_EDGE: u32 = 64;
pub const MAX_ASSET_BYTES: usize = MAX_ICON_EDGE as usize * MAX_ICON_EDGE as usize * 4;
/// Largest edge of a preview asset. The focused filmstrip tile is about 155
/// points wide, so 384 covers it on a retina display while keeping each frame
/// well inside `MAX_FRAME_BYTES`.
pub const MAX_PREVIEW_EDGE: u32 = 384;
/// Frame budget for one preview asset. A square 384x384 BGRA8 is 589,824 bytes.
pub const MAX_PREVIEW_BYTES: usize = 640 * 1024;
/// Negotiated asset representations. An asset advertises exactly one of these.
pub const REPRESENTATION_ICON: &str = "bgra8";
pub const REPRESENTATION_PREVIEW: &str = "previewBgra8";
pub const MAX_QUERY_BYTES: usize = 8_192;
pub const MAX_RESULTS: usize = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: Value,
}
impl RpcRequest {
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: String,
    #[serde(
        default,
        deserialize_with = "present_result",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}
impl RpcResponse {
    pub fn success(id: impl Into<String>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }
    pub fn failure(id: impl Into<String>, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            result: None,
            error: Some(error),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.jsonrpc != "2.0"
            || self.id.is_empty()
            || self.result.is_some() == self.error.is_some()
        {
            Err("invalid JSON-RPC response".into())
        } else {
            Ok(())
        }
    }
}
// JSON-RPC permits a present null result. Ordinary Option decoding would
// confuse it with an absent result and reject successful unit-returning calls.
fn present_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    pub data: ErrorData,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorData {
    pub code: String,
    pub retryable: bool,
}
impl RpcError {
    pub fn new(code: i32, stable_code: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: ErrorData {
                code: stable_code.into(),
                retryable: false,
            },
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelloParams {
    pub major: u16,
    #[serde(default)]
    pub minor: u16,
    pub client_name: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FrontendKind {
    Gpui,
    Pill,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrontendPollParams {
    pub kind: FrontendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendPollResult {
    pub registered: bool,
    pub show: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub major: u16,
    pub minor: u16,
    pub instance_id: String,
    pub session_id: String,
    pub capabilities: Vec<String>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Route {
    #[default]
    Home,
    Provider {
        #[serde(rename = "providerId")]
        provider_id: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenViewParams {
    #[serde(default)]
    pub route: Route,
    #[serde(default)]
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_limit() -> usize {
    20
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryViewParams {
    pub view_id: String,
    pub query: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloseViewParams {
    pub view_id: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvokeActionParams {
    pub view_id: String,
    pub revision: String,
    pub action_ref: String,
    pub operation_id: String,
    /// Consent to a prompted action. Omit false for older services whose
    /// request decoder rejects unknown fields; omission also fails closed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub confirmed: bool,
    /// Values collected for an action that published an `input` field, keyed by
    /// that field's id. Omitted entirely when the action needs none, so an
    /// older strict service is unaffected.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub input: std::collections::BTreeMap<String, String>,
}
fn is_false(value: &bool) -> bool {
    !*value
}
/// Identifies one result from a specific, still-current view snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetDetailsParams {
    pub view_id: String,
    pub revision: String,
    pub provider_id: String,
    pub item_id: String,
}
/// Safe provider-projected content for a single authorized result.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemDetails {
    pub view_id: String,
    pub revision: String,
    pub provider_id: String,
    pub item_id: String,
    pub content: Vec<ContentBlock>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetAssetParams {
    pub asset_id: String,
    pub revision: String,
    pub representation: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssetMetadata {
    pub asset_id: String,
    pub revision: String,
    pub representation: String,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub alpha_mode: String,
    pub row_order: String,
    pub byte_length: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetData {
    pub metadata: AssetMetadata,
    pub bytes: Vec<u8>,
}

/// Color-role keys mac-themer publishes in `palette` (6 named + 16 ANSI).
pub const THEME_PALETTE_KEYS: [&str; 22] = [
    "background",
    "foreground",
    "cursor",
    "accent",
    "selection_background",
    "selection_foreground",
    "color0",
    "color1",
    "color2",
    "color3",
    "color4",
    "color5",
    "color6",
    "color7",
    "color8",
    "color9",
    "color10",
    "color11",
    "color12",
    "color13",
    "color14",
    "color15",
];
/// Semantic role keys mac-themer publishes in `tokens`.
pub const THEME_TOKEN_KEYS: [&str; 19] = [
    "surface",
    "surface_container_low",
    "surface_container",
    "surface_container_high",
    "surface_container_highest",
    "on_surface",
    "on_surface_variant",
    "outline",
    "outline_variant",
    "primary",
    "on_primary",
    "primary_container",
    "on_primary_container",
    "secondary",
    "info",
    "success",
    "warning",
    "error",
    "muted",
];
pub const THEME_BORDER_KEYS: [&str; 2] = ["active", "inactive"];
pub const THEME_BAR_KEYS: [&str; 10] = [
    "background",
    "background_solid",
    "item_bg",
    "accent",
    "label",
    "icon",
    "muted",
    "red",
    "green",
    "yellow",
];
/// Largest accepted `current.json`. mac-themer's resolved palette is ~2 KiB today.
pub const MAX_THEME_STATE_BYTES: usize = 65_536;
/// Longest accepted repository URL. mac-themer accepts at most this many bytes
/// and refuses anything else before it runs `git`.
pub const MAX_REPO_URL_BYTES: usize = 512;
/// Longest accepted theme name in a pack summary. A display name, not a sentence.
pub const MAX_THEME_NAME_BYTES: usize = 256;
/// Longest accepted wallpaper identity, in bytes. mac-themer publishes a
/// filename and refuses anything longer.
pub const MAX_WALLPAPER_ID_BYTES: usize = 128;
/// Longest derived wallpaper label. A filename is not a sentence.
pub const MAX_WALLPAPER_LABEL_BYTES: usize = 64;

/// A true `#rrggbb` color. Rejects alpha and bare hex, matching mac-themer's own consumer.
pub fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit)
}

/// A published wallpaper identity: one of a theme pack's filenames, never a path.
///
/// The service forwards it untouched, so it must survive the round trip as an
/// argument: no separator, no NUL, no control character, and bounded.
pub fn is_wallpaper_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_WALLPAPER_ID_BYTES
        && value != "."
        && value != ".."
        && !value.starts_with('.')
        && !value.bytes().any(|byte| matches!(byte, b'/' | b'\\' | 0))
        && !value.chars().any(char::is_control)
}

/// A published theme slug: `[a-z0-9][a-z0-9_-]{0,127}`.
///
/// The producer derives it from a pack directory name and accepts it back as an
/// argument, so it is narrower than the filesystem on purpose.
pub fn is_theme_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
    })
}

/// One wallpaper a theme pack ships: an opaque, path-free identity plus the
/// label a frontend draws.
///
/// mac-themer names wallpapers with the file that holds them (`1-totoro.webp`)
/// and sorts them in cycle order, so the label is derived here once rather than
/// in every frontend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeWallpaper {
    pub id: String,
    pub label: String,
}
impl ThemeWallpaper {
    /// Build a reference from a published identity, or `None` when the identity
    /// is not safe to hand to a frontend or back to the producer.
    pub fn new(id: &str) -> Option<Self> {
        if !is_wallpaper_id(id) {
            return None;
        }
        Some(Self {
            id: id.to_owned(),
            label: wallpaper_label(id),
        })
    }
}

/// `1-the-backwater.jpg` -> `The Backwater`, `oma_cityscape.webp` -> `Oma Cityscape`.
fn wallpaper_label(id: &str) -> String {
    // Drop a real extension: a dot inside the name is not one.
    let stem = match id.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => stem,
        _ => id,
    };
    // Drop the producer's ordering prefix (`0-`, `12_`) so labels read without
    // the count leaking into the name.
    let stem = match stem.find(['-', '_']) {
        Some(index)
            if index > 0
                && index + 1 < stem.len()
                && stem[..index].bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            &stem[index + 1..]
        }
        _ => stem,
    };
    let mut label = String::new();
    for word in stem
        .split(|character: char| character == '-' || character == '_' || character.is_whitespace())
    {
        if word.is_empty() {
            continue;
        }
        if !label.is_empty() {
            label.push(' ');
        }
        let mut characters = word.chars();
        if let Some(first) = characters.next() {
            label.extend(first.to_uppercase());
            label.push_str(characters.as_str());
        }
    }
    // A name made only of separators still needs something to draw.
    if label.is_empty() {
        label.push_str(id);
    }
    truncate_label(label)
}

fn truncate_label(mut label: String) -> String {
    if label.len() <= MAX_WALLPAPER_LABEL_BYTES {
        return label;
    }
    let mut end = MAX_WALLPAPER_LABEL_BYTES;
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    label.truncate(end);
    label.trim_end().to_owned()
}

/// Read-only projection of the mac-themer schema-1 resolved palette.
///
/// Inner color maps intentionally keep mac-themer's snake_case role keys and
/// carry only the required keys; the service never forwards paths or extra
/// metadata. Unknown top-level fields are ignored on decode, but `validate()`
/// requires exactly the documented color keys so a partial or padded palette is
/// never rendered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemePalette {
    pub schema: u16,
    pub slug: String,
    pub name: String,
    pub variant: String,
    pub alpha_byte: u8,
    pub palette: BTreeMap<String, String>,
    pub tokens: BTreeMap<String, String>,
    pub border: BTreeMap<String, String>,
    pub bar: BTreeMap<String, String>,
}
impl ThemePalette {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != 1 {
            return Err("unsupported theme palette schema".into());
        }
        if self.slug.is_empty()
            || self.slug.len() > 256
            || self.slug.contains('\0')
            || self.name.is_empty()
            || self.name.len() > 256
            || self.name.contains('\0')
        {
            return Err("theme identity is invalid".into());
        }
        if self.variant != "dark" && self.variant != "light" {
            return Err("theme variant is invalid".into());
        }
        validate_color_map("palette", &self.palette, &THEME_PALETTE_KEYS)?;
        validate_color_map("tokens", &self.tokens, &THEME_TOKEN_KEYS)?;
        validate_color_map("border", &self.border, &THEME_BORDER_KEYS)?;
        validate_color_map("bar", &self.bar, &THEME_BAR_KEYS)?;
        Ok(())
    }
}
fn validate_color_map(
    group: &str,
    map: &BTreeMap<String, String>,
    keys: &[&str],
) -> Result<(), String> {
    if map.len() != keys.len() {
        return Err(format!(
            "theme {group} must contain exactly {} colors",
            keys.len()
        ));
    }
    for key in keys {
        let Some(value) = map.get(*key) else {
            return Err(format!("theme {group} is missing {key}"));
        };
        if !is_hex_color(value) {
            return Err(format!("theme {group}.{key} must be #rrggbb"));
        }
    }
    Ok(())
}

/// Bounded, public error explaining why the active palette is stale or missing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeStateError {
    pub code: String,
    pub message: String,
}
impl ThemeStateError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Current active theme state. `available` and `palette` always agree: a good
/// palette may still carry a stale `error` when a newer file is invalid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeState {
    pub revision: String,
    pub available: bool,
    pub palette: Option<ThemePalette>,
    pub error: Option<ThemeStateError>,
}
impl ThemeState {
    pub fn validate(&self) -> Result<(), String> {
        if self.revision.is_empty() || self.revision.len() > 64 || self.revision.contains('\0') {
            return Err("theme revision is invalid".into());
        }
        if self.available != self.palette.is_some() {
            return Err("theme availability does not match its palette".into());
        }
        if let Some(palette) = &self.palette {
            palette.validate()?;
        }
        if let Some(error) = &self.error
            && (error.code.is_empty()
                || error.code.len() > 64
                || error.code.contains('\0')
                || error.message.len() > 1024
                || error.message.contains('\0'))
        {
            return Err("theme error is invalid".into());
        }
        Ok(())
    }
}

/// Install one theme pack from a Git repository the caller names.
///
/// The URL is the only third-party string this protocol carries into a
/// process argument, so it is a whole repository URL, never a path, a branch or
/// a command: the producer validates it again and refuses every transport but
/// https. `all` installs every theme a collection repository states; `force`
/// replaces a pack the producer did not install itself.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThemeInstallParams {
    pub url: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub all: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub force: bool,
}

/// Remove one installed pack by the exact slug a catalog entry published.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThemeUninstallParams {
    pub slug: String,
}

/// One pack an install produced, as the producer reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackSummary {
    pub slug: String,
    pub name: String,
    /// How many wallpapers came with the pack. Not a list: the catalog publishes
    /// the identities, and this reply only says the pack has them.
    pub wallpapers: usize,
    #[serde(default)]
    pub replaced: bool,
}
impl ThemePackSummary {
    pub fn validate(&self) -> Result<(), String> {
        if !is_theme_slug(&self.slug) {
            return Err("installed pack slug is invalid".into());
        }
        if self.name.is_empty()
            || self.name.len() > MAX_THEME_NAME_BYTES
            || self.name.contains('\0')
        {
            return Err("installed pack name is invalid".into());
        }
        Ok(())
    }
}

/// Reply to `themes.install`: the packs that are now on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeInstallResult {
    pub schema_version: u16,
    pub status: String,
    pub packs: Vec<ThemePackSummary>,
}

/// Reply to `themes.uninstall`: the pack that is no longer on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeUninstallResult {
    pub schema_version: u16,
    pub status: String,
    pub slug: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub explicit_scope: bool,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewSnapshot {
    pub view_id: String,
    pub revision: String,
    pub route: Route,
    pub query: String,
    pub sections: Vec<Section>,
    /// Commands that belong to the whole view rather than to one row. A scope
    /// shows them in its action panel, never in its results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope_actions: Vec<ScopeAction>,
    pub complete: bool,
    pub truncated: bool,
    pub errors: Vec<ProviderError>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub id: String,
    pub title: String,
    pub layout_hint: Option<String>,
    pub items: Vec<ResultItem>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderError {
    pub provider_id: String,
    pub code: String,
    pub message: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultItem {
    pub id: String,
    pub provider_id: String,
    pub revision: String,
    pub kind: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub icon: Option<Icon>,
    pub content: Vec<ContentBlock>,
    pub data: Option<DomainData>,
    pub actions: Vec<ActionDescriptor>,
    pub details_available: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Icon {
    Glyph {
        name: String,
        #[serde(rename = "fallbackText")]
        fallback_text: String,
    },
    Asset {
        asset: AssetRef,
        #[serde(rename = "fallbackText")]
        fallback_text: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetRef {
    pub id: String,
    pub revision: String,
    pub representations: Vec<String>,
}

/// Bounds for one asset representation. Both the service that produces pixels
/// and the client that accepts them read these, so a size class cannot drift.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssetLimits {
    pub max_edge: u32,
    pub max_bytes: usize,
}
impl AssetLimits {
    pub const ICON: Self = Self {
        max_edge: MAX_ICON_EDGE,
        max_bytes: MAX_ASSET_BYTES,
    };
    pub const PREVIEW: Self = Self {
        max_edge: MAX_PREVIEW_EDGE,
        max_bytes: MAX_PREVIEW_BYTES,
    };
    /// Limits for a representation name, or `None` when it is not supported.
    pub fn for_representation(representation: &str) -> Option<Self> {
        match representation {
            REPRESENTATION_ICON => Some(Self::ICON),
            REPRESENTATION_PREVIEW => Some(Self::PREVIEW),
            _ => None,
        }
    }
    /// The negotiated capability that authorizes this representation.
    pub fn capability(self) -> &'static str {
        if self == Self::PREVIEW {
            "previewAssetsBgra8"
        } else {
            "iconAssetsBgra8"
        }
    }
}
#[derive(Clone, Debug)]
pub enum ContentBlock {
    Text {
        text: String,
        truncated: bool,
        fallback_text: String,
    },
    Image {
        asset: AssetRef,
        alt: String,
        fallback_text: String,
    },
    ColorPalette {
        colors: Vec<ColorSwatch>,
        fallback_text: String,
    },
    Value {
        value: String,
        label: Option<String>,
        fallback_text: String,
    },
    Metadata {
        entries: Vec<MetadataEntry>,
        fallback_text: String,
    },
    Unknown {
        kind: String,
        fallback_text: String,
    },
}
impl Serialize for ContentBlock {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Self::Unknown {
            kind,
            fallback_text,
        } = self
        {
            let mut map = serde_json::Map::new();
            map.insert("type".into(), Value::String(kind.clone()));
            map.insert("fallbackText".into(), Value::String(fallback_text.clone()));
            return Value::Object(map).serialize(serializer);
        }
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "camelCase")]
        enum Known<'a> {
            Text {
                text: &'a str,
                truncated: bool,
                #[serde(rename = "fallbackText")]
                fallback_text: &'a str,
            },
            Image {
                asset: &'a AssetRef,
                alt: &'a str,
                #[serde(rename = "fallbackText")]
                fallback_text: &'a str,
            },
            ColorPalette {
                colors: &'a [ColorSwatch],
                #[serde(rename = "fallbackText")]
                fallback_text: &'a str,
            },
            Value {
                value: &'a str,
                label: &'a Option<String>,
                #[serde(rename = "fallbackText")]
                fallback_text: &'a str,
            },
            Metadata {
                entries: &'a [MetadataEntry],
                #[serde(rename = "fallbackText")]
                fallback_text: &'a str,
            },
        }
        let known = match self {
            Self::Text {
                text,
                truncated,
                fallback_text,
            } => Known::Text {
                text,
                truncated: *truncated,
                fallback_text,
            },
            Self::Image {
                asset,
                alt,
                fallback_text,
            } => Known::Image {
                asset,
                alt,
                fallback_text,
            },
            Self::ColorPalette {
                colors,
                fallback_text,
            } => Known::ColorPalette {
                colors,
                fallback_text,
            },
            Self::Value {
                value,
                label,
                fallback_text,
            } => Known::Value {
                value,
                label,
                fallback_text,
            },
            Self::Metadata {
                entries,
                fallback_text,
            } => Known::Metadata {
                entries,
                fallback_text,
            },
            Self::Unknown { .. } => unreachable!(),
        };
        known.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("content block requires a string type"))?;
        let fallback = value
            .get("fallbackText")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                serde::de::Error::custom("content block requires a string fallbackText")
            })?
            .to_owned();
        match kind {
            "text" => serde_json::from_value::<KnownContentBlock>(value)
                .map(Into::into)
                .map_err(serde::de::Error::custom),
            "image" => serde_json::from_value::<KnownContentBlock>(value)
                .map(Into::into)
                .map_err(serde::de::Error::custom),
            "colorPalette" => serde_json::from_value::<KnownContentBlock>(value)
                .map(Into::into)
                .map_err(serde::de::Error::custom),
            "value" => serde_json::from_value::<KnownContentBlock>(value)
                .map(Into::into)
                .map_err(serde::de::Error::custom),
            "metadata" => serde_json::from_value::<KnownContentBlock>(value)
                .map(Into::into)
                .map_err(serde::de::Error::custom),
            _ => Ok(Self::Unknown {
                kind: kind.to_owned(),
                fallback_text: fallback,
            }),
        }
    }
}
impl ContentBlock {
    pub fn fallback_text(&self) -> &str {
        match self {
            Self::Text { fallback_text, .. }
            | Self::Image { fallback_text, .. }
            | Self::ColorPalette { fallback_text, .. }
            | Self::Value { fallback_text, .. }
            | Self::Metadata { fallback_text, .. }
            | Self::Unknown { fallback_text, .. } => fallback_text,
        }
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum KnownContentBlock {
    Text {
        text: String,
        truncated: bool,
        #[serde(rename = "fallbackText")]
        fallback_text: String,
    },
    Image {
        asset: AssetRef,
        alt: String,
        #[serde(rename = "fallbackText")]
        fallback_text: String,
    },
    ColorPalette {
        colors: Vec<ColorSwatch>,
        #[serde(rename = "fallbackText")]
        fallback_text: String,
    },
    Value {
        value: String,
        label: Option<String>,
        #[serde(rename = "fallbackText")]
        fallback_text: String,
    },
    Metadata {
        entries: Vec<MetadataEntry>,
        #[serde(rename = "fallbackText")]
        fallback_text: String,
    },
}
impl From<KnownContentBlock> for ContentBlock {
    fn from(value: KnownContentBlock) -> Self {
        match value {
            KnownContentBlock::Text {
                text,
                truncated,
                fallback_text,
            } => Self::Text {
                text,
                truncated,
                fallback_text,
            },
            KnownContentBlock::Image {
                asset,
                alt,
                fallback_text,
            } => Self::Image {
                asset,
                alt,
                fallback_text,
            },
            KnownContentBlock::ColorPalette {
                colors,
                fallback_text,
            } => Self::ColorPalette {
                colors,
                fallback_text,
            },
            KnownContentBlock::Value {
                value,
                label,
                fallback_text,
            } => Self::Value {
                value,
                label,
                fallback_text,
            },
            KnownContentBlock::Metadata {
                entries,
                fallback_text,
            } => Self::Metadata {
                entries,
                fallback_text,
            },
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorSwatch {
    pub role: String,
    pub value: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEntry {
    pub label: String,
    pub value: String,
}
/// Colours a frontend needs to draw a theme as a palette card: the theme's own
/// surface, its text, its accent and the six accents a strip shows as dots.
/// Every value is a `#rrggbb` string from a palette that already passed
/// validation. `applied` marks the theme the desktop is wearing, so a strip can
/// say which one is live without asking a second endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeCard {
    pub background: String,
    pub foreground: String,
    pub accent: String,
    pub swatches: Vec<String>,
    #[serde(default)]
    pub applied: bool,
}
#[derive(Clone, Debug)]
pub enum DomainData {
    Application {
        schema_version: u16,
        application_id: String,
    },
    ThemeSummary {
        schema_version: u16,
        slug: String,
        variant: String,
        wallpapers: Vec<ThemeWallpaper>,
        /// Absent when the producer could not resolve the palette.
        card: Option<ThemeCard>,
    },
    WallpaperSummary {
        schema_version: u16,
        theme_slug: String,
        wallpaper_id: String,
    },
    Calculation {
        schema_version: u16,
        expression: String,
        result: String,
    },
    /// A browser destination a frontend may render itself. The URL is already
    /// provider-validated; frontends must still reject non-HTTP schemes.
    WebLink { schema_version: u16, url: String },
    Unknown {
        kind: String,
        schema_version: Option<u16>,
    },
}
impl Serialize for DomainData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            Self::Application {
                schema_version,
                application_id,
            } => {
                serde_json::json!({"type":"application","schemaVersion":schema_version,"applicationId":application_id})
            }
            Self::ThemeSummary {
                schema_version,
                slug,
                variant,
                wallpapers,
                card,
            } => {
                let mut value = serde_json::json!({"type":"themeSummary","schemaVersion":schema_version,"slug":slug,"variant":variant,"wallpapers":wallpapers});
                if let Some(card) = card {
                    value["card"] = serde_json::to_value(card).unwrap_or_default();
                }
                value
            }
            Self::WallpaperSummary {
                schema_version,
                theme_slug,
                wallpaper_id,
            } => {
                serde_json::json!({"type":"wallpaperSummary","schemaVersion":schema_version,"themeSlug":theme_slug,"wallpaperId":wallpaper_id})
            }
            Self::Calculation {
                schema_version,
                expression,
                result,
            } => {
                serde_json::json!({"type":"calculation","schemaVersion":schema_version,"expression":expression,"result":result})
            }
            Self::WebLink {
                schema_version,
                url,
            } => {
                serde_json::json!({"type":"webLink","schemaVersion":schema_version,"url":url})
            }
            Self::Unknown {
                kind,
                schema_version,
            } => match schema_version {
                Some(version) => serde_json::json!({"type":kind,"schemaVersion":version}),
                None => serde_json::json!({"type":kind}),
            },
        };
        value.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for DomainData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("domain data requires a string type"))?;
        let known = matches!(
            kind,
            "application" | "themeSummary" | "wallpaperSummary" | "calculation" | "webLink"
        );
        if known
            && value
                .get("schemaVersion")
                .and_then(Value::as_u64)
                .is_some_and(|version| version > 1)
        {
            let version = value.get("schemaVersion").and_then(Value::as_u64).unwrap();
            let version = u16::try_from(version).map_err(serde::de::Error::custom)?;
            return Ok(Self::Unknown {
                kind: kind.to_owned(),
                schema_version: Some(version),
            });
        }
        match kind {
            "application" | "themeSummary" | "wallpaperSummary" | "calculation" | "webLink" => {
                serde_json::from_value::<KnownDomainData>(value)
                    .map(Into::into)
                    .map_err(serde::de::Error::custom)
            }
            _ => Ok(Self::Unknown {
                kind: kind.to_owned(),
                schema_version: value
                    .get("schemaVersion")
                    .and_then(Value::as_u64)
                    .and_then(|v| u16::try_from(v).ok()),
            }),
        }
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum KnownDomainData {
    Application {
        schema_version: u16,
        application_id: String,
    },
    ThemeSummary {
        schema_version: u16,
        slug: String,
        variant: String,
        // Absent in payloads written before the service published wallpapers.
        #[serde(default)]
        wallpapers: Vec<ThemeWallpaper>,
        // Absent in payloads written before the service published palette cards.
        #[serde(default)]
        card: Option<ThemeCard>,
    },
    WallpaperSummary {
        schema_version: u16,
        theme_slug: String,
        wallpaper_id: String,
    },
    Calculation {
        schema_version: u16,
        expression: String,
        result: String,
    },
    WebLink {
        schema_version: u16,
        url: String,
    },
}
impl From<KnownDomainData> for DomainData {
    fn from(v: KnownDomainData) -> Self {
        match v {
            KnownDomainData::Application {
                schema_version,
                application_id,
            } => Self::Application {
                schema_version,
                application_id,
            },
            KnownDomainData::ThemeSummary {
                schema_version,
                slug,
                variant,
                wallpapers,
                card,
            } => Self::ThemeSummary {
                schema_version,
                slug,
                variant,
                wallpapers,
                card,
            },
            KnownDomainData::WallpaperSummary {
                schema_version,
                theme_slug,
                wallpaper_id,
            } => Self::WallpaperSummary {
                schema_version,
                theme_slug,
                wallpaper_id,
            },
            KnownDomainData::Calculation {
                schema_version,
                expression,
                result,
            } => Self::Calculation {
                schema_version,
                expression,
                result,
            },
            KnownDomainData::WebLink {
                schema_version,
                url,
            } => Self::WebLink {
                schema_version,
                url,
            },
        }
    }
}
/// The kinds of field a provider can ask the frontend to collect before it
/// invokes an action. The frontend knows how to draw these two and nothing
/// else; every word, field id and choice comes from the provider.
pub const ACTION_INPUT_TEXT: &str = "text";
pub const ACTION_INPUT_SELECT: &str = "select";
/// Longest accepted input value, in bytes. A repository URL or a pack slug.
pub const MAX_ACTION_INPUT_VALUE_BYTES: usize = 512;
/// Most fields one action may ask for. A dialog, not a form.
pub const MAX_ACTION_INPUT_FIELDS: usize = 4;
/// Most choices one select field may publish.
pub const MAX_ACTION_INPUT_OPTIONS: usize = 256;

/// One choice in a `select` input: the value the provider receives back and the
/// label the frontend draws. Both are provider-authored text, never a path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionInputOption {
    pub value: String,
    pub label: String,
}

/// A provider-published field the frontend must collect before it can invoke an
/// action.
///
/// This exists so a provider can own an interaction the action table cannot
/// express — a repository URL, or one of a list of pack slugs — without the
/// frontend knowing what the action is for. The frontend renders the dialog,
/// then sends the collected value back as `input` on `actions.invoke`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionInput {
    /// The key this value is returned under in `invokeActionParams.input`.
    pub id: String,
    /// `text` or `select`. An unknown kind is not rendered.
    pub kind: String,
    /// Dialog title, field label, placeholder and submit button text: all
    /// provider-authored, all bounded.
    pub title: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    pub submit_label: String,
    /// Choices for `select`, empty for `text`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ActionInputOption>,
}
impl ActionInput {
    pub fn validate(&self) -> Result<(), String> {
        if !is_input_id(&self.id) {
            return Err("action input id is invalid".into());
        }
        if self.kind != ACTION_INPUT_TEXT && self.kind != ACTION_INPUT_SELECT {
            return Err("action input kind is unknown".into());
        }
        for text in [&self.title, &self.label, &self.submit_label] {
            if text.is_empty() || text.len() > 256 || text.contains('\0') {
                return Err("action input text is invalid".into());
            }
        }
        if let Some(placeholder) = &self.placeholder
            && (placeholder.len() > 256 || placeholder.contains('\0'))
        {
            return Err("action input placeholder is invalid".into());
        }
        if self.kind == ACTION_INPUT_TEXT && !self.options.is_empty() {
            return Err("a text input carries no options".into());
        }
        if self.kind == ACTION_INPUT_SELECT && self.options.is_empty() {
            return Err("a select input needs at least one option".into());
        }
        if self.options.len() > MAX_ACTION_INPUT_OPTIONS {
            return Err("action input has too many options".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for option in &self.options {
            if option.value.is_empty()
                || option.value.len() > MAX_ACTION_INPUT_VALUE_BYTES
                || option.value.contains('\0')
            {
                return Err("action input option value is invalid".into());
            }
            if option.label.is_empty() || option.label.len() > 256 || option.label.contains('\0') {
                return Err("action input option label is invalid".into());
            }
            if !seen.insert(option.value.as_str()) {
                return Err("action input has a duplicate option".into());
            }
        }
        Ok(())
    }
}

/// An input field id: lowercase word characters and `-`, so it is a JSON key
/// and nothing else.
fn is_input_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
        })
}

/// Validate the values a frontend collected for one action.
pub fn validate_action_input(
    input: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    if input.len() > MAX_ACTION_INPUT_FIELDS {
        return Err("too many action inputs".into());
    }
    for (key, value) in input {
        if !is_input_id(key) {
            return Err("action input key is invalid".into());
        }
        if value.is_empty() || value.len() > MAX_ACTION_INPUT_VALUE_BYTES || value.contains('\0') {
            return Err("action input value is invalid".into());
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDescriptor {
    pub id: String,
    pub action_ref: String,
    pub title: String,
    pub is_primary: bool,
    pub kind: String,
    /// Present only when the provider requires explicit confirmation before
    /// activation. The text is a plain, provider-authored prompt for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<String>,
    /// Present only when the provider needs a value from the user before the
    /// action can run. The frontend collects it and sends it back as `input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ActionInput>,
}
/// A command a provider publishes for its whole scope. It is an action, not a
/// result: it never occupies a row, and a frontend lists it beside the actions
/// of whatever row is selected. `id` is the identity the command is authorized
/// under, so a frontend invokes it exactly like a row action.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeAction {
    pub provider_id: String,
    pub id: String,
    pub action_id: String,
    pub action_ref: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ActionInput>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Presentation {
    Navigate { route: Route },
    Close,
    KeepOpen { message: String },
    SetQuery { query: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationResult {
    pub operation_id: String,
    pub status: String,
    pub presentation: Option<Presentation>,
    pub content: Vec<ContentBlock>,
}

pub mod framing {
    use super::MAX_FRAME_BYTES;
    use std::io::{self, Read, Write};
    pub fn read_bounded_frame(reader: &mut impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
        let mut h = [0; 4];
        reader.read_exact(&mut h)?;
        let len = u32::from_be_bytes(h) as usize;
        if len == 0 || len > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame size is invalid",
            ));
        }
        let mut b = vec![0; len];
        reader.read_exact(&mut b)?;
        Ok(b)
    }
    pub fn read_frame(reader: &mut impl Read) -> io::Result<Vec<u8>> {
        read_bounded_frame(reader, MAX_FRAME_BYTES)
    }
    pub fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame size is invalid",
            ));
        }
        let len = u32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame is too large"))?;
        writer.write_all(&len.to_be_bytes())?;
        writer.write_all(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rpc_response_requires_exactly_one_result_or_error() {
        assert!(RpcResponse::success("1", Value::Null).validate().is_ok());
        let mut r = RpcResponse::success("1", Value::Null);
        r.error = Some(RpcError::new(-1, "X", "bad"));
        assert!(r.validate().is_err());
    }
    #[test]
    fn framing_rejects_zero_and_oversize_before_body_read() {
        use std::io::Cursor;
        for n in [0u32, (MAX_FRAME_BYTES + 1) as u32] {
            assert!(framing::read_frame(&mut Cursor::new(n.to_be_bytes())).is_err())
        }
    }
    #[test]
    fn unknown_content_and_domain_types_keep_safe_fallbacks() {
        let block: ContentBlock =
            serde_json::from_str(r#"{"type":"future","fallbackText":"Readable fallback"}"#)
                .unwrap();
        assert_eq!(block.fallback_text(), "Readable fallback");
        assert_eq!(serde_json::to_value(block).unwrap()["type"], "future");
        let data: DomainData =
            serde_json::from_str(r#"{"type":"future","path":"/private"}"#).unwrap();
        assert!(matches!(data, DomainData::Unknown { ref kind, .. } if kind == "future"));
        let wire = serde_json::to_value(&data).unwrap();
        assert_eq!(wire["type"], "future");
        assert!(!wire.to_string().contains("private"));
    }
    #[test]
    fn fixtures_round_trip_without_frontend_uris_or_private_payloads() {
        let fixtures: Vec<ViewSnapshot> =
            serde_json::from_str(include_str!("../../fixtures/views.json")).unwrap();
        assert_eq!(fixtures.len(), 1);
        let wire = serde_json::to_string(&fixtures).unwrap();
        assert!(!wire.contains("image://"));
        assert!(!wire.contains("payload"));
        assert!(!wire.contains("file://"));
        // The optional provider-authored prompt survives a fixture round trip.
        let prompts: Vec<&str> = fixtures[0]
            .sections
            .iter()
            .flat_map(|section| &section.items)
            .flat_map(|item| &item.actions)
            .filter_map(|action| action.confirmation.as_deref())
            .collect();
        assert_eq!(prompts, ["Apply Catppuccin? It changes the desktop theme."]);
    }
    #[test]
    fn wallpapers_are_path_free_and_round_trip_through_domain_data() {
        assert!(is_wallpaper_id("1-totoro.webp"));
        assert!(is_wallpaper_id("3-blue-eye.webp"));
        assert!(is_wallpaper_id("oma cityscape.jpg"));
        for bad in [
            "",
            ".",
            "..",
            ".hidden.webp",
            "a/b.webp",
            "a\\b.webp",
            "tab\there",
            "nul\0byte",
        ] {
            assert!(!is_wallpaper_id(bad), "accepted {bad:?}");
        }
        assert!(!is_wallpaper_id(&"a".repeat(MAX_WALLPAPER_ID_BYTES + 1)));

        let data = DomainData::ThemeSummary {
            schema_version: 1,
            slug: "catppuccin".into(),
            variant: "dark".into(),
            wallpapers: ["1-totoro.webp", "2-waves.webp"]
                .into_iter()
                .filter_map(ThemeWallpaper::new)
                .collect(),
            card: Some(ThemeCard {
                background: "#1e1e2e".into(),
                foreground: "#cdd6f4".into(),
                accent: "#cba6f7".into(),
                swatches: vec!["#f38ba8".into(), "#a6e3a1".into()],
                applied: true,
            }),
        };
        let wire = serde_json::to_value(&data).unwrap();
        assert_eq!(wire["card"]["background"], "#1e1e2e");
        assert_eq!(wire["card"]["swatches"][1], "#a6e3a1");
        assert_eq!(wire["card"]["applied"], true);
        assert_eq!(wire["wallpapers"][0]["id"], "1-totoro.webp");
        assert_eq!(wire["wallpapers"][0]["label"], "Totoro");
        assert!(!wire.to_string().contains('/'));
        // A payload written before wallpapers existed still decodes.
        let old: DomainData = serde_json::from_str(
            r#"{"type":"themeSummary","schemaVersion":1,"slug":"x","variant":"dark"}"#,
        )
        .unwrap();
        assert!(matches!(
            old,
            DomainData::ThemeSummary { ref wallpapers, card: None, .. } if wallpapers.is_empty()
        ));
    }
    #[test]
    fn asset_limits_cover_both_representations_and_fit_one_frame() {
        assert_eq!(
            AssetLimits::for_representation(REPRESENTATION_ICON),
            Some(AssetLimits::ICON)
        );
        assert_eq!(
            AssetLimits::for_representation(REPRESENTATION_PREVIEW),
            Some(AssetLimits::PREVIEW)
        );
        assert!(AssetLimits::for_representation("png").is_none());
        assert!(AssetLimits::for_representation("").is_none());
        assert_eq!(AssetLimits::ICON.capability(), "iconAssetsBgra8");
        assert_eq!(AssetLimits::PREVIEW.capability(), "previewAssetsBgra8");
        // A square preview is the worst case, and it must still fit one frame.
        let square = MAX_PREVIEW_EDGE as usize * MAX_PREVIEW_EDGE as usize * 4;
        assert!(square <= MAX_PREVIEW_BYTES);
        const { assert!(MAX_PREVIEW_BYTES < MAX_FRAME_BYTES) };
        let wide = MAX_PREVIEW_EDGE as usize * 216 * 4;
        assert!(wide <= MAX_PREVIEW_BYTES);
        const { assert!(AssetLimits::PREVIEW.max_edge > AssetLimits::ICON.max_edge) };
    }

    #[test]
    fn wallpaper_labels_drop_order_prefixes_and_extensions() {
        let label = |id: &str| ThemeWallpaper::new(id).unwrap().label;
        assert_eq!(label("1-the-backwater.jpg"), "The Backwater");
        assert_eq!(label("oma_cityscape.webp"), "Oma Cityscape");
        assert_eq!(label("0-a.webp"), "A");
        // A count only counts when it is the whole prefix.
        assert_eq!(label("2pac-live.png"), "2pac Live");
        // No extension to drop, and no word to split.
        assert_eq!(label("wallpaper"), "Wallpaper");
        assert_eq!(label("a.b.c.webp"), "A.b.c");
        // Separators alone still leave something drawable.
        assert_eq!(label("---"), "---");
        // A label longer than the cap is truncated on a character boundary.
        let long = format!("{}-{}.webp", "a".repeat(40), "b".repeat(40));
        let truncated = label(&long);
        assert_eq!(truncated.len(), MAX_WALLPAPER_LABEL_BYTES);
        assert!(truncated.starts_with("Aaaa") && !truncated.ends_with(' '));
    }
    #[test]
    fn web_link_domain_data_is_versioned_and_camel_case() {
        let data = DomainData::WebLink {
            schema_version: 1,
            url: "https://www.google.com/search?q=rust".into(),
        };
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({
                "type":"webLink",
                "schemaVersion":1,
                "url":"https://www.google.com/search?q=rust"
            })
        );
        let decoded: DomainData =
            serde_json::from_value(serde_json::to_value(&data).unwrap()).unwrap();
        assert!(
            matches!(decoded, DomainData::WebLink { ref url, .. } if url == "https://www.google.com/search?q=rust")
        );
        // A newer webLink schema stays opaque.
        let future: DomainData =
            serde_json::from_str(r#"{"type":"webLink","schemaVersion":2,"url":"x"}"#).unwrap();
        assert!(matches!(
            future,
            DomainData::Unknown {
                ref kind,
                schema_version: Some(2)
            } if kind == "webLink"
        ));
    }
    #[test]
    fn schema_is_valid_json_and_lists_the_negotiated_methods() {
        let schema: Value =
            serde_json::from_str(include_str!("../../schema/protocol.schema.json")).unwrap();
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert!(
            schema["$defs"]["contentBlock"]["oneOf"]
                .as_array()
                .is_some()
        );
        assert!(schema["$defs"].get("viewSnapshot").is_some());
        assert!(schema["$defs"].get("getAssetParams").is_some());
        assert!(schema["$defs"].get("getDetailsParams").is_some());
        assert!(schema["$defs"].get("itemDetails").is_some());
        assert!(
            schema["$defs"]["request"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|method| method["$ref"] == "#/$defs/requestDetails")
        );
        assert!(schema["$defs"].get("assetMetadata").is_some());
        assert!(schema["$defs"].get("themePalette").is_some());
        assert!(schema["$defs"].get("themeState").is_some());
        assert!(schema["$defs"].get("themeWallpaper").is_some());
        assert!(
            schema["$defs"]["domainData"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["properties"]["type"]["const"] == "webLink")
        );
        let summary = schema["$defs"]["domainData"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|branch| branch["properties"]["type"]["const"] == "themeSummary")
            .unwrap();
        assert_eq!(
            summary["properties"]["wallpapers"]["items"]["$ref"],
            "#/$defs/themeWallpaper"
        );
        assert!(
            schema["$defs"]["request"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|method| method["$ref"] == "#/$defs/requestThemeState")
        );
        for definition in [
            "themeInstallParams",
            "themeUninstallParams",
            "themePackSummary",
            "themeInstallResult",
            "themeUninstallResult",
        ] {
            assert!(schema["$defs"].get(definition).is_some(), "{definition}");
        }
        assert_eq!(
            schema["$defs"]["action"]["properties"]["input"]["$ref"],
            "#/$defs/actionInput"
        );
        assert_eq!(
            schema["$defs"]["actionInput"]["properties"]["kind"]["enum"],
            serde_json::json!(["text", "select"])
        );
        assert!(
            schema["$defs"]["actionInputOption"]
                .get("properties")
                .is_some()
        );
        assert!(
            schema["$defs"]["invokeActionParams"]["properties"]
                .get("input")
                .is_some()
        );
        for request in ["requestThemeInstall", "requestThemeUninstall"] {
            assert!(
                schema["$defs"]["request"]["oneOf"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|method| method["$ref"] == format!("#/$defs/{request}")),
                "{request}"
            );
        }
        assert!(
            schema["$defs"]["action"]["properties"]
                .get("confirmation")
                .is_some()
        );
        assert!(
            schema["$defs"]["invokeActionParams"]["properties"]
                .get("confirmed")
                .is_some()
        );
        // A scope command is an action, not a row: the snapshot carries it
        // beside its sections and it references its own field.
        assert_eq!(
            schema["$defs"]["viewSnapshot"]["properties"]["scopeActions"]["items"]["$ref"],
            "#/$defs/scopeAction"
        );
        assert_eq!(
            schema["$defs"]["scopeAction"]["properties"]["input"]["$ref"],
            "#/$defs/actionInput"
        );
        for field in ["providerId", "id", "actionId", "actionRef", "title"] {
            assert!(
                schema["$defs"]["scopeAction"]["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|required| required == field),
                "scopeAction.{field}"
            );
        }
    }
    #[test]
    fn a_scope_command_is_published_beside_the_results_and_omitted_when_empty() {
        let snapshot = ViewSnapshot {
            view_id: "v1".into(),
            revision: "2".into(),
            route: Route::Provider {
                provider_id: "themes".into(),
            },
            query: String::new(),
            sections: vec![],
            scope_actions: vec![ScopeAction {
                provider_id: "themes".into(),
                id: "themes:install".into(),
                action_id: "install".into(),
                action_ref: "s1-a1".into(),
                title: "Install a theme from GitHub…".into(),
                confirmation: None,
                input: Some(ActionInput {
                    id: "url".into(),
                    kind: ACTION_INPUT_TEXT.into(),
                    title: "Install a theme from GitHub".into(),
                    label: "Repository URL".into(),
                    placeholder: None,
                    submit_label: "Install".into(),
                    options: vec![],
                }),
            }],
            complete: true,
            truncated: false,
            errors: vec![],
        };
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            wire["scopeActions"][0]["actionId"],
            serde_json::json!("install")
        );
        assert_eq!(
            wire["scopeActions"][0]["actionRef"],
            serde_json::json!("s1-a1")
        );
        // Nothing an empty scope would need to say.
        assert!(wire["scopeActions"][0].get("confirmation").is_none());
        assert!(wire["scopeActions"][0]["input"].get("options").is_none());
        let parsed: ViewSnapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(
            parsed.scope_actions[0].title,
            "Install a theme from GitHub…"
        );

        // A scope with no commands stays byte-identical to the older shape.
        let mut empty = snapshot.clone();
        empty.scope_actions.clear();
        let wire = serde_json::to_value(&empty).unwrap();
        assert!(wire.get("scopeActions").is_none());
        // An older service that never sends the field still parses.
        let parsed: ViewSnapshot = serde_json::from_value(wire).unwrap();
        assert!(parsed.scope_actions.is_empty());
    }

    #[test]
    fn action_input_is_provider_wording_and_round_trips_as_camel_case() {
        let field = ActionInput {
            id: "url".into(),
            kind: ACTION_INPUT_TEXT.into(),
            title: "Install a theme from GitHub".into(),
            label: "Repository URL".into(),
            placeholder: Some("https://github.com/<owner>/<repo>".into()),
            submit_label: "Install".into(),
            options: Vec::new(),
        };
        field.validate().unwrap();
        let wire = serde_json::to_value(&field).unwrap();
        assert_eq!(wire["kind"], "text");
        assert_eq!(wire["submitLabel"], "Install");
        assert_eq!(wire["placeholder"], "https://github.com/<owner>/<repo>");
        // A text field carries no choices, so the key is omitted entirely.
        assert!(wire.get("options").is_none());
        assert_eq!(serde_json::from_value::<ActionInput>(wire).unwrap(), field);

        let choice = ActionInput {
            id: "slug".into(),
            kind: ACTION_INPUT_SELECT.into(),
            title: "Uninstall a theme".into(),
            label: "Choose the pack to remove".into(),
            placeholder: None,
            submit_label: "Continue".into(),
            options: vec![
                ActionInputOption {
                    value: "aura".into(),
                    label: "Aura (aura)".into(),
                },
                ActionInputOption {
                    value: "jade".into(),
                    label: "Jade (jade)".into(),
                },
            ],
        };
        choice.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&choice).unwrap()["options"][0]["value"],
            "aura"
        );
    }

    #[test]
    fn an_unusable_action_input_is_rejected_before_it_reaches_a_frontend() {
        let base = ActionInput {
            id: "url".into(),
            kind: ACTION_INPUT_TEXT.into(),
            title: "Title".into(),
            label: "Label".into(),
            placeholder: None,
            submit_label: "Go".into(),
            options: Vec::new(),
        };
        for mutate in [
            |f: &mut ActionInput| f.id = "../url".into(),
            |f: &mut ActionInput| f.id = String::new(),
            |f: &mut ActionInput| f.kind = "colour".into(),
            |f: &mut ActionInput| f.title = String::new(),
            |f: &mut ActionInput| f.label = "x".repeat(257),
            |f: &mut ActionInput| {
                f.options = vec![ActionInputOption {
                    value: "a".into(),
                    label: "A".into(),
                }]
            },
        ] {
            let mut field = base.clone();
            mutate(&mut field);
            assert!(field.validate().is_err(), "{field:?}");
        }
        // A select without choices, or with a duplicate, cannot be drawn.
        let select = ActionInput {
            kind: ACTION_INPUT_SELECT.into(),
            ..base.clone()
        };
        assert!(select.validate().is_err());
        let duplicate = ActionInput {
            options: vec![
                ActionInputOption {
                    value: "a".into(),
                    label: "A".into(),
                },
                ActionInputOption {
                    value: "a".into(),
                    label: "Again".into(),
                },
            ],
            ..select
        };
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn invoke_input_is_optional_bounded_and_path_free() {
        let params = InvokeActionParams {
            view_id: "view-1".into(),
            revision: "1".into(),
            action_ref: "ref-1".into(),
            operation_id: "op-1".into(),
            confirmed: false,
            input: Default::default(),
        };
        // An ordinary invoke omits consent and input entirely, so an older
        // strict service sees exactly the request it always did.
        let wire = serde_json::to_value(&params).unwrap();
        assert!(wire.get("input").is_none());
        assert!(wire.get("confirmed").is_none());

        let mut with_input = params.clone();
        with_input
            .input
            .insert("url".into(), "https://github.com/owner/theme".into());
        let wire = serde_json::to_value(&with_input).unwrap();
        assert_eq!(wire["input"]["url"], "https://github.com/owner/theme");
        assert_eq!(validate_action_input(&with_input.input), Ok(()));

        let mut bad = with_input.clone();
        bad.input.insert("URL".into(), "x".into());
        assert!(validate_action_input(&bad.input).is_err());
        let mut oversized = with_input.clone();
        oversized.input.insert("url".into(), "x".repeat(513));
        assert!(validate_action_input(&oversized.input).is_err());
        let mut empty = with_input.clone();
        empty.input.insert("url".into(), String::new());
        assert!(validate_action_input(&empty.input).is_err());
        let mut many = with_input;
        for index in 0..MAX_ACTION_INPUT_FIELDS {
            many.input.insert(format!("f{index}"), "x".into());
        }
        assert!(validate_action_input(&many.input).is_err());
    }

    #[test]
    fn theme_mutations_are_path_free_and_omit_false_consent_fields() {
        // An ordinary install omits `all`/`force`, so an older strict decoder
        // that knows the method keeps working; naming them is additive.
        let params = ThemeInstallParams {
            url: "https://github.com/owner/theme".into(),
            all: false,
            force: false,
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            serde_json::json!({"url": "https://github.com/owner/theme"})
        );
        let flagged = ThemeInstallParams {
            all: true,
            force: true,
            ..params
        };
        assert_eq!(
            serde_json::to_value(&flagged).unwrap(),
            serde_json::json!({
                "url": "https://github.com/owner/theme",
                "all": true,
                "force": true,
            })
        );
        // A pack summary is a count and a name, never a path.
        let result = ThemeInstallResult {
            schema_version: 1,
            status: "installed".into(),
            packs: vec![ThemePackSummary {
                slug: "aura".into(),
                name: "Aura".into(),
                wallpapers: 7,
                replaced: false,
            }],
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::json!({
                "schemaVersion": 1,
                "status": "installed",
                "packs": [{"slug": "aura", "name": "Aura", "wallpapers": 7, "replaced": false}],
            })
        );
        let removed = ThemeUninstallResult {
            schema_version: 1,
            status: "removed".into(),
            slug: "aura".into(),
        };
        assert_eq!(
            serde_json::to_value(&removed).unwrap()["schemaVersion"],
            serde_json::json!(1)
        );
        // Both directions agree, so a client can decode what it sent.
        let decoded: ThemeInstallParams =
            serde_json::from_value(serde_json::json!({"url": "https://github.com/a/b"})).unwrap();
        assert!(!decoded.all && !decoded.force);
        assert!(
            serde_json::from_value::<ThemeInstallParams>(
                serde_json::json!({"url": "x", "all": true, "force": false, "extra": 1})
            )
            .is_err()
        );
    }

    #[test]
    fn slugs_are_the_producer_contract_and_nothing_path_shaped() {
        assert!(is_theme_slug("a"));
        assert!(is_theme_slug("aura"));
        assert!(is_theme_slug("osaka-jade_2"));
        assert!(is_theme_slug(&"a".repeat(128)));
        for slug in [
            "",
            "Aura",
            "-lead",
            "_lead",
            "a.b",
            "../escape",
            "aura/child",
            "a b",
            "a\0b",
            &"a".repeat(129),
        ] {
            assert!(!is_theme_slug(slug), "accepted {slug:?}");
        }
        assert!(
            ThemePackSummary {
                slug: "aura".into(),
                name: "Aura".into(),
                wallpapers: 0,
                replaced: false,
            }
            .validate()
            .is_ok()
        );
        assert!(
            ThemePackSummary {
                slug: "../aura".into(),
                name: "Aura".into(),
                wallpapers: 0,
                replaced: false,
            }
            .validate()
            .is_err()
        );
        assert!(
            ThemePackSummary {
                slug: "aura".into(),
                name: String::new(),
                wallpapers: 0,
                replaced: false,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn camel_case_wire_types() {
        let r = serde_json::to_value(Route::Provider {
            provider_id: "apps".into(),
        })
        .unwrap();
        assert_eq!(r["providerId"], "apps");
        let state = ThemeState {
            revision: "abc".into(),
            available: false,
            palette: None,
            error: Some(ThemeStateError::new("THEME_UNAVAILABLE", "none")),
        };
        let wire = serde_json::to_value(&state).unwrap();
        assert_eq!(wire["revision"], "abc");
        assert!(wire.get("available").is_some());
        assert_eq!(wire["error"]["code"], "THEME_UNAVAILABLE");
    }
}
