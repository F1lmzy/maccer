//! Frontend-neutral DTOs for Maccer's local service. See `docs/provider-service-spec.md`.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 5;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_ICON_EDGE: u32 = 64;
pub const MAX_ASSET_BYTES: usize = MAX_ICON_EDGE as usize * MAX_ICON_EDGE as usize * 4;
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

/// A true `#rrggbb` color. Rejects alpha and bare hex, matching mac-themer's own consumer.
pub fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit)
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
            } => {
                serde_json::json!({"type":"themeSummary","schemaVersion":schema_version,"slug":slug,"variant":variant})
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
            "application" | "themeSummary" | "wallpaperSummary" | "calculation"
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
            "application" | "themeSummary" | "wallpaperSummary" | "calculation" => {
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
            } => Self::ThemeSummary {
                schema_version,
                slug,
                variant,
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
        }
    }
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
            serde_json::from_str(include_str!("../fixtures/views.json")).unwrap();
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
    fn schema_is_valid_json_and_lists_the_negotiated_methods() {
        let schema: Value =
            serde_json::from_str(include_str!("../schema/protocol.schema.json")).unwrap();
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
        assert!(
            schema["$defs"]["request"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|method| method["$ref"] == "#/$defs/requestThemeState")
        );
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
