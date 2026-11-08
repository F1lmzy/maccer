use crate::protocol::ThemePalette;
use gpui::{Hsla, Rgba, hsla, rgba};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Theme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub divider: Hsla,
}

pub(crate) fn theme() -> Theme {
    Theme {
        background: hsla(0., 0., 0.10, 1.),
        foreground: hsla(0., 0., 0.75, 1.),
        muted: hsla(0., 0., 0.65, 1.),
        accent: hsla(0., 0., 0.85, 1.),
        divider: hsla(0., 0., 0.24, 1.),
    }
}

impl Theme {
    /// A selected row: the accent mixed 14% into the surface, as a solid
    /// colour. The window is opaque, so the tint is resolved once here instead
    /// of being composited by the display server.
    pub(crate) fn selection_fill(&self) -> Hsla {
        mix(self.accent, self.background, 0.14)
    }

    /// A quiet wash for panels that sit on the surface, such as command output
    /// or a distant filmstrip frame. Deliberately translucent: it is drawn over
    /// rows, so it has to composite with whatever is beneath it.
    pub(crate) fn panel_fill(&self) -> Hsla {
        self.accent.opacity(0.06)
    }

    /// Text selection inside the search field. The only deliberately
    /// translucent colour left: it sits over glyphs, which must stay readable.
    pub(crate) fn text_selection(&self) -> Hsla {
        self.accent.opacity(0.35)
    }

    /// Map a validated mac-themer palette to the launcher's semantic colors.
    ///
    /// The service validates every required color before publishing, but this
    /// stays total: a missing key falls back to the default theme rather than
    /// panicking a live window. Every role is fully opaque: `alpha_byte` is
    /// still carried by the palette, but the launcher window is solid, so the
    /// surface never lets the desktop through. Rows tint the surface with the
    /// accent, so a separate selected-text color is not needed.
    pub(crate) fn from_palette(palette: &ThemePalette) -> Theme {
        let default = theme();
        Theme {
            background: hex(&palette.tokens, "surface", 255).unwrap_or(default.background),
            foreground: hex(&palette.bar, "label", 255).unwrap_or(default.foreground),
            muted: hex(&palette.bar, "muted", 255).unwrap_or(default.muted),
            accent: hex(&palette.palette, "accent", 255)
                .or_else(|| hex(&palette.border, "active", 255))
                .unwrap_or(default.accent),
            divider: hex(&palette.tokens, "outline_variant", 255).unwrap_or(default.divider),
        }
    }
}

/// Mix `tint` into `base` in sRGB and return an opaque colour.
fn mix(tint: Hsla, base: Hsla, amount: f32) -> Hsla {
    let tint: Rgba = tint.into();
    let base: Rgba = base.into();
    let at = |a: f32, b: f32| a + (b - a) * amount;
    Rgba {
        r: at(base.r, tint.r),
        g: at(base.g, tint.g),
        b: at(base.b, tint.b),
        a: 1.,
    }
    .into()
}

/// Parse one `#rrggbb` colour published by a provider. Malformed values are
/// rejected rather than guessed.
pub(crate) fn parse_color(value: &str) -> Option<Hsla> {
    let value = value.trim().trim_start_matches('#');
    if value.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(value, 16).ok()?;
    Some(rgba((rgb << 8) | 0xff).into())
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
    use crate::protocol::ThemePalette;
    use gpui::rgba;

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
            palette: map(&[("accent", "#7aa2f7")]),
            tokens: map(&[("surface", "#010203"), ("outline_variant", "#112233")]),
            border: map(&[("active", "#445566")]),
            bar: map(&[("label", "#aabbcc"), ("muted", "#778899")]),
        }
    }

    #[test]
    fn palette_maps_documented_roles_and_a_solid_background() {
        let theme = Theme::from_palette(&palette());
        // The palette's alpha byte is ignored: the window is opaque.
        assert_eq!(theme.background, rgba(0x010203ff).into());
        assert_eq!(theme.foreground, rgba(0xaabbccff).into());
        assert_eq!(theme.muted, rgba(0x778899ff).into());
        assert_eq!(theme.accent, rgba(0x7aa2f7ff).into());
        assert_eq!(theme.divider, rgba(0x112233ff).into());
    }

    #[test]
    fn selection_fill_is_a_solid_tint_and_overlays_stay_translucent() {
        let theme = Theme::from_palette(&palette());
        let fill = theme.selection_fill();
        assert_eq!(fill.a, 1.);
        // Between the surface and the accent, so the row is lighter than the
        // window and darker than the accent bar.
        let surface: Rgba = theme.background.into();
        let accent: Rgba = theme.accent.into();
        let filled: Rgba = fill.into();
        assert!(filled.r > surface.r && filled.r < accent.r);
        // Panel and text selection are drawn over other content, so they stay
        // washes: an opaque one would erase what it covers.
        assert!(theme.panel_fill().a > 0. && theme.panel_fill().a < 0.1);
        assert!(theme.text_selection().a > 0.3 && theme.text_selection().a < 0.4);
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
            .tokens
            .insert("outline_variant".into(), "not-a-color".into());
        assert_eq!(Theme::from_palette(&malformed).divider, default.divider);
    }
}
