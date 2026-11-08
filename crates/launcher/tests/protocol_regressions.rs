use launcher_agent::protocol::{
    ContentBlock, DomainData, GetDetailsParams, ItemDetails, RpcResponse, THEME_BAR_KEYS,
    THEME_BORDER_KEYS, THEME_PALETTE_KEYS, THEME_TOKEN_KEYS, ThemePalette, ThemeState,
    is_hex_color,
};
use serde_json::Value;

fn valid_palette() -> ThemePalette {
    let map = |keys: &[&str]| {
        keys.iter()
            .map(|key| ((*key).to_owned(), "#102030".to_owned()))
            .collect()
    };
    ThemePalette {
        schema: 1,
        slug: "slug".into(),
        name: "Name".into(),
        variant: "dark".into(),
        alpha_byte: 200,
        palette: map(&THEME_PALETTE_KEYS),
        tokens: map(&THEME_TOKEN_KEYS),
        border: map(&THEME_BORDER_KEYS),
        bar: map(&THEME_BAR_KEYS),
    }
}

#[test]
fn theme_palette_requires_exact_rrggbb_color_keys() {
    assert!(valid_palette().validate().is_ok());
    assert!(is_hex_color("#a1b2c3"));
    assert!(!is_hex_color("a1b2c3"));
    assert!(!is_hex_color("#a1b2c3ff"));
    let mut missing = valid_palette();
    missing.palette.remove("color9");
    assert!(missing.validate().is_err());
    let mut extra = valid_palette();
    extra.bar.insert("secret".into(), "#000000".into());
    assert!(extra.validate().is_err());
    let mut bad = valid_palette();
    bad.tokens.insert("primary".into(), "url(red)".into());
    assert!(bad.validate().is_err());
}

#[test]
fn theme_state_availability_must_match_its_palette() {
    let available = ThemeState {
        revision: "r1".into(),
        available: true,
        palette: Some(valid_palette()),
        error: None,
    };
    assert!(available.validate().is_ok());
    let round_trip: ThemeState =
        serde_json::from_value(serde_json::to_value(&available).unwrap()).unwrap();
    assert_eq!(round_trip, available);
    let mismatched = ThemeState {
        revision: "r1".into(),
        available: false,
        palette: Some(valid_palette()),
        error: None,
    };
    assert!(mismatched.validate().is_err());
    let stale: ThemeState = serde_json::from_value(serde_json::json!({
        "revision":"r2", "available":true, "palette": valid_palette(),
        "error":{"code":"THEME_INVALID","message":"kept last good"}
    }))
    .unwrap();
    assert!(stale.validate().is_ok());
    assert_eq!(stale.error.unwrap().code, "THEME_INVALID");
}

#[test]
fn theme_palette_ignores_unknown_top_level_metadata_but_keeps_snake_case() {
    let mut value = serde_json::to_value(valid_palette()).unwrap();
    value["pack"] = serde_json::json!("themes/secret");
    let decoded: ThemePalette = serde_json::from_value(value).unwrap();
    assert!(decoded.validate().is_ok());
    let wire = serde_json::to_value(&decoded).unwrap();
    assert!(wire["palette"].get("selection_background").is_some());
    assert_eq!(wire["alpha_byte"], 200);
}

#[test]
fn asset_metadata_contract_is_bounded_and_params_are_strict() {
    use launcher_agent::protocol::{
        AssetMetadata, GetAssetParams, MAX_ASSET_BYTES, MAX_ICON_EDGE, PROTOCOL_MINOR,
    };
    assert_eq!(PROTOCOL_MINOR, 10);
    assert_eq!(MAX_ASSET_BYTES, 16_384);
    assert_eq!(MAX_ICON_EDGE, 64);
    let params: GetAssetParams = serde_json::from_value(
        serde_json::json!({"assetId":"opaque","revision":"7","representation":"bgra8"}),
    )
    .unwrap();
    assert_eq!(params.asset_id, "opaque");
    assert!(serde_json::from_value::<GetAssetParams>(serde_json::json!({"assetId":"x","revision":"1","representation":"bgra8","path":"/private"})).is_err());
    let meta = AssetMetadata {
        asset_id: "opaque".into(),
        revision: "7".into(),
        representation: "bgra8".into(),
        width: 64,
        height: 64,
        stride: 256,
        alpha_mode: "straight".into(),
        row_order: "topToBottom".into(),
        byte_length: MAX_ASSET_BYTES,
    };
    assert_eq!(
        serde_json::from_value::<AssetMetadata>(serde_json::to_value(meta).unwrap())
            .unwrap()
            .byte_length,
        MAX_ASSET_BYTES
    );
}

#[test]
fn details_params_and_response_use_stable_camel_case_fields() {
    let params: GetDetailsParams = serde_json::from_value(serde_json::json!({
        "viewId":"view-1", "revision":"2", "providerId":"apps", "itemId":"app-1"
    }))
    .unwrap();
    assert_eq!(params.provider_id, "apps");
    assert!(serde_json::from_value::<GetDetailsParams>(serde_json::json!({
        "viewId":"view-1", "revision":"2", "providerId":"apps", "itemId":"app-1", "path":"/private"
    })).is_err());
    let details: ItemDetails = serde_json::from_value(serde_json::json!({
        "viewId":"view-1", "revision":"2", "providerId":"apps", "itemId":"app-1",
        "content":[{"type":"text", "text":"Notes 🦀", "truncated":false, "fallbackText":"Notes"}]
    }))
    .unwrap();
    assert_eq!(details.content[0].fallback_text(), "Notes");
}

#[test]
fn bounded_frame_reader_checks_asset_cap_before_reading_body() {
    use std::io::Cursor;
    let mut reader = Cursor::new((16_385u32).to_be_bytes());
    assert!(launcher_agent::protocol::framing::read_bounded_frame(&mut reader, 16_384).is_err());
    assert_eq!(reader.position(), 4);
}

#[test]
fn a_present_null_result_is_not_a_missing_result() {
    let response: RpcResponse =
        serde_json::from_str(r#"{"jsonrpc":"2.0","id":"close","result":null}"#).unwrap();
    assert_eq!(response.result, Some(Value::Null));
    assert!(response.validate().is_ok());
    let absent: RpcResponse = serde_json::from_str(r#"{"jsonrpc":"2.0","id":"close"}"#).unwrap();
    assert!(absent.validate().is_err());
}

#[test]
fn unknown_content_still_requires_a_readable_fallback_field() {
    assert!(serde_json::from_str::<ContentBlock>(r#"{"type":"futureChart"}"#).is_err());
    assert!(
        serde_json::from_str::<ContentBlock>(r#"{"type":"futureChart","fallbackText":42}"#)
            .is_err()
    );
}

#[test]
fn future_domain_versions_do_not_require_old_domain_fields() {
    let domain: DomainData = serde_json::from_str(
        r#"{"type":"themeSummary","schemaVersion":2,"futureColorSpace":"oklch"}"#,
    )
    .unwrap();
    assert!(matches!(domain, DomainData::Unknown { ref kind, .. } if kind == "themeSummary"));
    let wire = serde_json::to_value(&domain).unwrap();
    assert_eq!(wire["schemaVersion"], 2);
    assert!(serde_json::from_value::<DomainData>(wire).is_ok());
}

#[test]
fn frontend_poll_contract_is_typed_strict_and_camel_case() {
    use launcher_agent::protocol::{
        FrontendKind, FrontendPollParams, FrontendPollResult, PROTOCOL_MINOR,
    };
    assert_eq!(PROTOCOL_MINOR, 10);
    let params = FrontendPollParams {
        kind: FrontendKind::Gpui,
        ack: Some("show-1".into()),
    };
    assert_eq!(
        serde_json::to_value(&params).unwrap(),
        serde_json::json!({"kind":"gpui","ack":"show-1"})
    );
    assert_eq!(
        serde_json::to_value(FrontendPollParams {
            kind: FrontendKind::Pill,
            ack: None,
        })
        .unwrap(),
        serde_json::json!({"kind":"pill"})
    );
    let pill: FrontendPollParams =
        serde_json::from_value(serde_json::json!({"kind":"pill"})).unwrap();
    assert_eq!(pill.kind, FrontendKind::Pill);
    assert!(
        serde_json::from_value::<FrontendPollParams>(serde_json::json!({"kind":"other"})).is_err()
    );
    assert!(
        serde_json::from_value::<FrontendPollParams>(
            serde_json::json!({"kind":"gpui","privatePath":"/secret"})
        )
        .is_err()
    );
    let result = FrontendPollResult {
        registered: true,
        show: Some("serial-1".into()),
    };
    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({"registered":true,"show":"serial-1"})
    );
    assert_eq!(
        serde_json::from_value::<FrontendPollResult>(wire).unwrap(),
        result
    );
}

#[test]
fn confirmation_transport_is_additive_and_defaults_to_unconfirmed() {
    use launcher_agent::protocol::{ActionDescriptor, InvokeActionParams, PROTOCOL_MINOR};
    assert_eq!(PROTOCOL_MINOR, 10);
    // Old clients omit `confirmed`, which must decode as an unconfirmed request.
    let params: InvokeActionParams = serde_json::from_value(serde_json::json!({
        "viewId":"view-1", "revision":"2", "actionRef":"ref-1", "operationId":"op-1"
    }))
    .unwrap();
    assert!(!params.confirmed);
    // New clients must still invoke ordinary actions on minor-3 services,
    // whose request decoder denies unknown fields.
    assert!(
        serde_json::to_value(&params)
            .unwrap()
            .get("confirmed")
            .is_none()
    );
    let confirmed: InvokeActionParams = serde_json::from_value(serde_json::json!({
        "viewId":"view-1", "revision":"2", "actionRef":"ref-1", "operationId":"op-1", "confirmed":true
    }))
    .unwrap();
    assert!(confirmed.confirmed);
    assert_eq!(serde_json::to_value(&confirmed).unwrap()["confirmed"], true);
    assert!(
        serde_json::from_value::<InvokeActionParams>(serde_json::json!({
            "viewId":"view-1", "revision":"2", "actionRef":"ref-1", "operationId":"op-1", "slug":"evil"
        }))
        .is_err()
    );
    // A descriptor without a prompt omits the field rather than emitting null.
    let plain = ActionDescriptor {
        id: "open".into(),
        action_ref: "ref-1".into(),
        title: "Open".into(),
        is_primary: true,
        kind: "invoke".into(),
        confirmation: None,
        input: None,
    };
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("confirmation")
            .is_none()
    );
    let prompted = ActionDescriptor {
        confirmation: Some("Apply now?".into()),
        ..plain
    };
    let wire = serde_json::to_value(&prompted).unwrap();
    assert_eq!(wire["confirmation"], "Apply now?");
    let back: ActionDescriptor = serde_json::from_value(wire).unwrap();
    assert_eq!(back.confirmation.as_deref(), Some("Apply now?"));
}
