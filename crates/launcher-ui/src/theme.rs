use gpui::{Hsla, hsla};

#[derive(Clone)]
pub(crate) struct Theme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub selected: Hsla,
    pub border: Hsla,
}

pub(crate) fn theme() -> Theme {
    Theme {
        background: hsla(0.64, 0.12, 0.13, 0.98),
        foreground: hsla(0.08, 0.08, 0.94, 1.0),
        muted: hsla(0.62, 0.06, 0.67, 1.0),
        selected: hsla(0.62, 0.12, 0.24, 1.0),
        border: hsla(0.62, 0.08, 0.32, 1.0),
    }
}
