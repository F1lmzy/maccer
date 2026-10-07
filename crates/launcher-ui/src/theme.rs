use gpui::{Hsla, hsla};

#[derive(Clone)]
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
