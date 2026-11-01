use gpui::{Hsla, hsla, rgba};
use launcher_protocol::ThemePalette;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Theme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub selected: Hsla,
    pub border: Hsla,
    pub divider: Hsla,
    pub selected_foreground: Hsla,
    pub selected_muted: Hsla,
}

pub(crate) fn theme() -> Theme {
    Theme {
        background: hsla(0., 0., 0.10, 1.),
        foreground: hsla(0., 0., 0.75, 1.),
        muted: hsla(0., 0., 0.65, 1.),
        selected: hsla(0., 0., 0.95, 1.),
        border: hsla(0., 0., 0.55, 1.),
        divider: hsla(0., 0., 0.24, 1.),
        selected_foreground: hsla(0., 0., 0.08, 1.),
        selected_muted: hsla(0., 0., 0.30, 1.),
    }
}

impl Theme {
    /// Map a validated mac-themer palette to the launcher's semantic colors.
    ///
    /// The service validates every required color before publishing, but this
    /// stays total: a missing key falls back to the default theme rather than
    /// panicking a live window. Only the background carries `alpha_byte`; every
    /// other role is fully opaque.
    pub(crate) fn from_palette(palette: &ThemePalette) -> Theme {
        let default = theme();
        Theme {
            background: hex(&palette.tokens, "surface", palette.alpha_byte)
                .unwrap_or(default.background),
            foreground: hex(&palette.bar, "label", 255).unwrap_or(default.foreground),
            muted: hex(&palette.bar, "muted", 255).unwrap_or(default.muted),
            selected: hex(&palette.palette, "selection_background", 255)
                .unwrap_or(default.selected),
            // Text drawn on a selected row: the palette's selection foreground.
            selected_foreground: hex(&palette.palette, "selection_foreground", 255)
                .unwrap_or(default.selected_foreground),
            border: hex(&palette.border, "active", 255).unwrap_or(default.border),
            divider: hex(&palette.tokens, "outline_variant", 255).unwrap_or(default.divider),
            selected_muted: hex(&palette.palette, "selection_foreground", 255)
                .unwrap_or(default.selected_muted),
        }
    }
}

/// Parse one `#rrggbb` color into an HSLA value, or `None` if the key is absent
/// or malformed. `alpha` is a 0..=255 byte.
fn hex(map: &BTreeMap<String, String>, key: &str, alpha: u8) -> Option<Hsla> {
    let value = u32::from_str_radix(map.get(key)?.trim_start_matches('#'), 16).ok()?;
    Some(rgba((value << 8) | u32::from(alpha)).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgba;
    use launcher_protocol::ThemePalette;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn palette() -> ThemePalette {
        ThemePalette {
            schema: 1,
            slug: "test".into(),
            name: "Test".into(),
            variant: "dark".into(),
            alpha_byte: 128,
            palette: map(&[
                ("selection_background", "#102030"),
                ("selection_foreground", "#f0e0d0"),
            ]),
            tokens: map(&[("surface", "#010203"), ("outline_variant", "#112233")]),
            border: map(&[("active", "#445566")]),
            bar: map(&[("label", "#aabbcc"), ("muted", "#778899")]),
        }
    }

    #[test]
    fn palette_maps_documented_roles_and_background_alpha() {
        let theme = Theme::from_palette(&palette());
        assert_eq!(theme.background, rgba(0x01020380).into());
        assert_eq!(theme.foreground, rgba(0xaabbccff).into());
        assert_eq!(theme.muted, rgba(0x778899ff).into());
        assert_eq!(theme.selected, rgba(0x102030ff).into());
        assert_eq!(theme.selected_foreground, rgba(0xf0e0d0ff).into());
        assert_eq!(theme.border, rgba(0x445566ff).into());
        assert_eq!(theme.divider, rgba(0x112233ff).into());
        assert_eq!(
            theme.selected_muted, theme.selected_foreground,
            "muted text on a selected row matches the selection foreground"
        );
    }

    #[test]
    fn missing_roles_fall_back_to_the_embedded_theme() {
        let mut missing = palette();
        missing.tokens.clear();
        missing.bar.clear();
        let fallback = Theme::from_palette(&missing);
        let default = theme();
        assert_eq!(fallback.background, default.background);
        assert_eq!(fallback.foreground, default.foreground);
        assert_eq!(fallback.muted, default.muted);
        // A malformed color is treated the same as a missing one.
        let mut malformed = palette();
        malformed
            .border
            .insert("active".into(), "not-a-color".into());
        assert_eq!(Theme::from_palette(&malformed).border, default.border);
    }
}
