# maccer design rules

The launcher is one monospace window that changes state. This folder holds the rules for
designing those states and the values that implement them.

| File | What it is |
| --- | --- |
| `design.html` (repo root) | The reference implementation. Every state at its real size, one `.win` per state. |
| `design/design-system.yaml` | The values: grid, colour, type, geometry, components, copy, motion. Machine readable, no prose. |
| `design/DESIGN.md` | This file. The rules, and how to add a state without breaking them. |
| `design/check.mjs` | Verifies that every card in `design.html` renders the size its caption claims and that content sits on the gutter. |

Change a value in **both** `design.html` and `design-system.yaml`, then run the checker. A value
that exists in only one of them is a bug.

---

## 1. What maccer looks like

- One family, monospace, weight 400. The interface is text.
- One neutral ramp (`surface`, `fg`, `muted`, `divider`) and one accent. Colour is content,
  not chrome: it appears in app icons, in a theme's own frame, and in the 2px selection bar.
- Two ways to identify a row: a 24px icon tile (an app icon, or a 11px glyph) or nothing at
  all. Never an icon that repeats the row's first letter.
- A row is 44pt tall. A window is `84 + rows × 44` tall. The footer is always there and always
  says which keys work.
- Nothing glows, nothing pulses except the caret, nothing moves more than a few pixels.

---

## 2. The spacing model

Everything is derived from two numbers: the **gutter** (16) and the **inset** (8).

```
window border 1px
└─ 16  gutter ──────────────── content edge
   └─ 0  selection fill starts at 8  (inset)
      └─ 8 row padding → 16  icon box / row text (text-only rows)
         └─ 24 icon + 10 gap → 50  title
```

| Token | Value | Applies to |
| --- | --- | --- |
| gutter | 16 | bar text, footer text, error line, preview text, confirm text, row content |
| inset | 8 | selection fill, row box, confirm box, remote filmstrip frame edge |
| row padding | 8 | so `inset + padding = gutter` |
| icon gap | 10 | icon box to title, never 8 in one card and 12 in another |
| nested panel | 12 | padding inside `.out`, `.preview`, and the filmstrip name block |
| card padding | 16 | the gallery figure around a window (page chrome, not the product) |

Rules:

1. **Content starts at 16 and ends at 16.** If a new element sits on a different x, either it
   is an icon box (16..40) or it has a reason you can state in one line.
2. **Never add a third inset.** If something needs to sit inside the gutter, it uses the
   selection fill's 8 or the nested panel's 12.
3. **Values come from the grid** (`design-system.yaml: grid.steps`). 1px is a hairline, 2px is
   a nudge inside a control, 22 and 226 are product constants that must not be copied.
4. **Vertical rhythm is row-based.** Lists are 44pt rows with no gap between them; the fill is
   what separates rows. Do not add row gaps.
5. **The window height follows the row count**, not the content: the preview pane and long
   text clip (`contain: size`), they never stretch the window. A staged confirmation is the
   one documented exception (`design-system.yaml: geometry.window.height_exception`).

---

## 3. Anatomy of a state

```html
<figure class="card">
  <figcaption><b>State name</b><span>480 × 348</span></figcaption>   <!-- width × height, real -->
  <div class="stage">
    <div class="win">                        <!-- w760 / w984 / h364 modifiers -->
      <div class="bar"><div class="field">query<span class="caret"></span></div></div>
      <div class="body">
        <div class="rows"> … .row … </div>
        <div class="preview"> … </div>       <!-- only when the pane is open -->
      </div>
      <div class="foot"><span>↵ Open · ⌘K Actions · Esc Close</span><span class="count">1/12</span></div>
    </div>
  </div>
</figure>
```

Class names map one-to-one onto the GPUI calls in `crates/launcher/src/ui/launcher.rs`
(`.win` = root div, `.bar` = 52pt header, `.row` = 44pt result, `.preview` = 280pt pane), so a
card can be read straight into the Rust renderer.

### Adding a state

1. Work out the real size. `84 + rows × 44`, or 364 for the output screen, or one of the two
   filmstrip sizes. Write it in the caption before writing the markup.
2. Copy the nearest existing card. Keep its structure; change the content.
3. Only the documented classes. If you need a new one, add it to
   `design-system.yaml: components` and to the checklist below.
4. Use real data: path shapes from this repo, real theme names and palettes from
   `crates/launcher/fixtures/theme-catalog.json`, plausible counts.
5. Copy follows section 5. No slugs, no "4 wallpapers" strings, nothing that truncates.
6. Run `node design/check.mjs`. A red line means the card lies about its size or its padding.

---

## 4. Colour

- Five roles only: `surface #1a1a1a`, `fg #bfbfbf`, `muted #a6a6a6`, `accent #d9d9d9`,
  `divider #3d3d3d`. Three derived washes: selection 14%, panel 6%, text selection 35%.
- `fg` for text that is read, `muted` for text that describes: subtitles, placeholders,
  footers, meta. Never a third grey.
- The accent marks **one** thing per screen: the selected row's bar, the focused strip frame,
  or the caret. Not the title, not the border of every card.
- A theme palette overrides the five roles and nothing else. A theme's own surface may fill the
  focused frame only — at distance the frame drops to `panel` so a light theme cannot outshine
  the focused card.
- No hue gradients, gradient text, glows, coloured shadows, blobs, or neon borders. App icon
  tiles may use a radial gradient because they are standing in for artwork.

---

## 5. Type and copy

| Role | Size / line | Colour |
| --- | --- | --- |
| search field | 16 / 22 | fg, placeholder muted |
| row title | 14 / 18 | fg |
| row subtitle | 11 / 14 | muted |
| preview text | 12 / 17 | fg |
| preview heading | 10 / 14 | muted |
| footer, confirm actions, counts | 10 / 14 | muted |
| filmstrip name | 13 (11 at distance) | theme fg |

- Sentence case. No uppercase labels, no tracking, no italic.
- No trailing period on labels, buttons, titles or captions. Paragraphs keep theirs.
- No exclamation marks anywhere.
- Counts are `1/12`, not `12 results`. Key legends are `↵ Open · ⌘K Actions · Esc Close`.
- A subtitle says what the row holds in one line and never truncates. If it would truncate,
  the row does not need a subtitle.
- Real data only. If a number or a name cannot be checked in this repo, it does not go in a
  card. Two numbers that exist: installed apps on the dev machine (101, `docs/application-search.md`),
  themes in the fixture catalog (4).

---

## 6. Components, in one table

| Component | Height | Padding | Notes |
| --- | --- | --- | --- |
| bar | 52 | 12 / 16 | field 28 tall, 16px text, caret 1.5 × 24 |
| row | 44 | 0 / 8 | radius 8, 10 gap, fill at 8, bar 2 × 20 at inset 12 |
| action row | 44 | 0 / 8 | no icon, muted until selected |
| error line | 24 | 0 / 16 | 11px, muted |
| preview | fills list | 12 / 16 | 280 wide, `border-left`, clips, never grows |
| footer | 22 | 0 / 16 | 10px, count right, tabular |
| confirm | 87 | 8 | margin 8, radius 8, fill `selection` |
| output | 240 body | 12 | radius 8, fill `panel`, window 364 |
| gallery card | auto | 16 | radius 16, caption 12 below |

The filmstrip is a component of its own: see `design-system.yaml: components.filmstrip` for the
two sizes (wallpapers 984 × 334, themes 760 × 284), the frame ladder (320/190/142 and
248/146/110) and the distance rule.

- The band is inset **16 above and below** its frames; a focused frame never touches the bar
  rule or the footer. It is **not** inset on the sides: the strip runs past both edges, so the
  outermost frames may sit on the window edge.
- Frames shrink with distance but stay on one mid-line, so the strip reads as one row.
- A **wallpaper frame bleeds**: the image is the object, padding 0.
- A **theme frame is a padded card**: name inset 32/24 from the bottom with 16/12 side padding,
  palette row at 72/48/32 from its own top so every frame's dots land on the same line.

---

## 7. Motion

- The caret blinks (1.06s). That is the only loop.
- Hover and press change colour or a surface step; they do not scale, lift or glow.
- One easing curve, `cubic-bezier(0.22, 1, 0.36, 1)`; 140ms feedback, 200ms enter.
- Animate colour, background, border, opacity, transform. Never width, height, top, left.

---

## 8. Verify before you hand it over

```bash
node design/check.mjs
```

It loads `design.html` in Chromium and fails on:

- a caption whose `width × height` is not what the `.win` renders,
- content that does not start on the 16 gutter (row text, footer, error, confirm),
- a title after an icon that does not start at 50, a pane that is not 280 wide,
- a window width that is not 480, 760 or 984, a bar that is not 52, a row that is not 44,
- a card that is missing from, or sized differently in, the `states` inventory in
  `design-system.yaml` (add the card there in the same commit).

Then read the card at 100%: one selected row, one accent, no text under 10px, no line that
ends in an ellipsis unless the row is a real result with a real long name.

---

## 9. The Rust mirror

The GPUI frontend implements this system; keep the mapping honest.

| Design | Rust |
| --- | --- |
| `.win` | root div in `crates/launcher/src/ui/launcher.rs` (`render`), `WindowBackgroundAppearance::Opaque` |
| five roles, washes | `ui/theme.rs` — `selection_fill()` is a solid 14% mix of accent into the surface; `panel_fill()` and `text_selection()` stay translucent because they are drawn over rows and glyphs |
| palette `alpha_byte` | validated but not painted: the window is solid, so the surface is opaque |
| `.bar` | 52pt header, 12px vertical padding, 28pt field (`ui/search_input.rs`) |
| `.row` | `ROW_HEIGHT` 44, 8px padding inside an 8px list inset, 10 gap, `result_row` |
| home rows | `result_row` returns title-only for `provider == "menu"` |
| `.preview` | 280pt pane, `contain: size` equivalent: it clips, never resizes the window |
| `.foot` | `FOOTER_HEIGHT` 22, 16px padding, legend + `n/total` |
| filmstrip | `StripGeometry`, `WALLPAPER_STRIP`, `THEME_STRIP`, `STRIP_INSET` 16, window = band + 76 |
| theme card | `ThemeSummary.card` → `ServiceBackend.theme_cards` → palette tile (surface, dots, name, applied dot) |

Tests that pin this: `a_wallpaper_picker_paints_a_filmstrip_and_steps_between_tiles` and
`a_theme_picker_paints_palette_cards_on_the_smaller_strip` in `ui/launcher.rs`, plus the
theme and protocol tests.

---

## 10. Changing the system

The system is only real if both files move together:

1. Change `design.html` first, since it is what people look at.
2. Change the same value in `design-system.yaml`.
3. Run `design/check.mjs`.
4. If the change touches the GPUI mirror, say so in the PR: `crates/launcher/src/ui/launcher.rs`
   constants (`ROW_HEIGHT`, `ICON_SIZE`, `PREVIEW_WIDTH`, `FOOTER_HEIGHT`) and
   `theme.rs` roles.

If a state needs a value that is not in the grid, add the step to `grid.steps` and say why in
the YAML note. Silence is how the next card ends up at 14px.
