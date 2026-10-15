# GPUI stack for sysman

Date: 2026-10-15. Evidence is file:line in local checkouts or a URL. Nothing was built or run.
Paths: `INS` = /home/mahfuz/Projects/insensical, `REG` = ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f, `ZED` = ~/.cargo/git/checkouts/zed-a70e2ad075855582/1a28cff (the pinned rev, already checked out locally).

## 1. insensical: how it pins and boots gpui

Pin (`INS/Cargo.toml:10-12`):
```toml
gpui = { git = "https://github.com/zed-industries/zed", rev = "1a28cff4b409169bac058bca40dfbfeb7621d19b" }
gpui_platform = { git = "...zed", rev = "1a28cff4...", features = ["font-kit", "wayland", "x11", "runtime_shaders"] }
```
- It does NOT use gpui-component (no match in any `.rs` or Cargo.toml under `INS/crates`). Everything is hand-built with `div()` and `canvas`.
- App creation (`INS/crates/app/src/main.rs:14-16`): it builds the platform itself, then wraps it, instead of the shorter `gpui_platform::application()` (that helper exists, `ZED/crates/gpui_platform/src/gpui_platform.rs:13`):
```rust
let platform = gpui_platform::current_platform(false);
let inks = insensical::grid::Inks(platform.text_system());
let application = gpui::Application::with_platform(platform).with_assets(insensical::deck::Assets);
application.run(move |cx: &mut App| { cx.set_global(inks); insensical::theme::add_fonts(cx); ... open_window(&socket, cx); ... cx.activate(true); });
```
- Window (`main.rs:61-73`): `Bounds::centered(None, size(px(w), px(h)), cx)`, `WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), app_id: Some("insensical".into()), window_background: config::window_ground(cx), ..Default::default() }`, then `cx.open_window(options, |window, cx| cx.new(|cx| Workspace::open(..)))`.
  `app_id` is what Hyprland sees as the window class. `window_decorations` is left at default (`Server`), so insensical has no client-side decoration code. `grep` for Decorations/titlebar in INS finds nothing.
- Theme: own `Theme` struct in `INS/crates/app/src/theme.rs:70` (fonts, `foreground/background: Rgb`, 16-colour palette, `opacity`, `dark`), stored as a gpui Global (`cx.set_global`, read with `cx.try_global::<Theme>()`, `config.rs:281`). Window transparency/blur via `WindowBackgroundAppearance::{Opaque,Transparent,Blurred}` (`config.rs:281-287`).
- Custom drawing worth copying:
  - `gpui::canvas(prepaint, paint)` with `window.paint_quad(gpui::fill(bounds, gpui::linear_gradient(180., linear_color_stop(c0, 0.), linear_color_stop(c1, 1.))))` (`dialog.rs:111-116`).
  - `PathBuilder::stroke(px(w))` + `move_to`/`curve_to`/`line_to` + `window.paint_path(path, color)` (`grid.rs:766-799`, `marks.rs:46-78`); `PathBuilder::fill()` at `grid.rs:819`.
  - A zero-size `canvas` used only to measure bounds (`workspace/panes.rs:121`).

## 2. gpui-component and which gpui it needs

Facts:
- The project moved. crates.io metadata for 0.5.x says repo `longbridge/gpui-component`; from 0.6.0 it says `longbridge/gpui-kit` (https://crates.io/api/v1/crates/gpui-component/versions). Umbrella crate `gpui-kit` re-exports gpui + `gpui-base` (unstyled behaviour, incl. `plot`, `table`, `tree`, `virtual_list`) + `gpui-component` (styled). README: `gpui-kit = "0.7"` (https://github.com/longbridge/gpui-kit). The old repo URL is now the same project; the 0.5.x line is a different, older codebase.
- Latest on crates.io: **0.7.3** (2026-10-12). 0.7.0 is dated 2026-09-28. Licence Apache-2.0.
- gpui required per release (crates.io dependency API, and `REG/gpui-component-0.7.0/Cargo.toml:277-279`):

| gpui-component | gpui dependency |
|---|---|
| 0.5.0, 0.5.1 | `gpui ^0.2.2` (crates.io, Zed's published gpui) |
| 0.6.6 | `gpui-pre =0.3.6` |
| 0.7.0 | `gpui-pre =0.3.7` (`package = "gpui-pre"`) |
| 0.7.3 | `gpui-pre =0.3.9`, also `gpui-pre-macros`, `gpui-pre-sum-tree` at =0.3.9; optional `gpui-fast ^0.1.7` |

- `gpui-pre` is a weekly snapshot of Zed's gpui published under a different crate name. 0.3.7 description: "gpui-pre snapshot of zed@1a28cff" (`REG/gpui-pre-0.3.7/Cargo.toml:24`). **That is exactly the rev insensical pins** (1a28cff4...). 0.3.9 is "snapshot of zed@93d7456" (https://crates.io/api/v1/crates/gpui-pre). The platform crate is `gpui-pre-platform 0.3.7` with the same `font-kit`, `wayland`, `x11`, `runtime_shaders` features (`REG/gpui-pre-platform-0.3.7/Cargo.toml:41-57`; lib name `gpui_platform`).

Answers:
- **gpui-component with Zed git at insensical's rev: not directly.** Cargo treats git `gpui` and crates.io `gpui-pre` as two different packages, so you would get two copies of gpui and every type (`Window`, `App`, `Hsla`, `Entity`) would mismatch. A `[patch]` cannot fix it because the package names differ. Source is identical for 0.7.0, so the fix is to switch sysman to the crates.io snapshot, not to mix.
- **gpui-component with gpui 0.2.2 from crates.io: only gpui-component 0.5.x.** 0.7.x cannot use it. 0.5.x predates the `gpui-base` split and likely lacks the newer chart/plot work; I did not read 0.5.x source.
- **Mixing yields two gpui copies** in every case above except one: depend on `gpui-pre` (renamed) and `gpui-pre-platform` yourself, at the same version gpui-component pins (exact `=` pins, so 0.7.0 needs 0.3.7, 0.7.3 needs 0.3.9). Cargo would otherwise duplicate; with a matching `=` there is one copy.
```toml
gpui = { package = "gpui-pre", version = "=0.3.9" }
gpui_platform = { package = "gpui-pre-platform", version = "=0.3.9", features = ["font-kit","wayland","x11","runtime_shaders"] }
gpui-component = "=0.7.3"
```
  Not verified: that `gpui-pre-platform 0.3.9` exists with those features (0.3.7 does; the `gpui-pre` series ships weekly, so very probably). Check with `cargo tree -d` that only one gpui remains. Also: `gpui-component::init(cx)` (`REG/gpui-component-0.7.0/src/lib.rs:127`) must run, and the window root view must be wrapped in `Root`.

## 3. App shell: client-side decorations on Wayland

gpui API at the pinned rev (`ZED/crates/gpui/src`):
- `WindowOptions { window_decorations: Some(WindowDecorations::Client), titlebar: Some(TitlebarOptions{ appears_transparent: true, .. }) }` (`platform.rs:576-582`; example `examples/window_shadow.rs:225`).
- Query: `window.window_decorations() -> Decorations::{Server, Client{tiling: Tiling}}` (`window.rs:2856`, `platform.rs:586-595`); `Tiling{top,left,right,bottom}` + `is_tiled()` (`platform.rs:763-788`). `window.window_controls()` (`window.rs:2871`) says which of min/max/close the compositor supports. `window.request_decorations(..)` (`window.rs:2379`).
- Drag and resize: `window.start_window_move()` (`:2840`), `window.start_window_resize(ResizeEdge)` (`:2402`), `window.show_window_menu(pos)`, `window.titlebar_double_click()` (used by gpui-component `title_bar.rs:348-392`), `div().window_control_area(WindowControlArea::{Drag,Min,Max,Close})` (`elements/div.rs:749`).
- Shadow/inset: `window.set_client_inset(px)` (`:2845`) reserves the invisible border used for shadow and resize hit areas.
- Reference implementation: `ZED/crates/gpui/examples/window_shadow.rs` (resize-edge hit test at `:192-208`, calls `start_window_resize` at `:91`, `set_client_inset` at `:29`).

Tiling compositor behaviour (Hyprland): Hyprland tiles windows and ignores client geometry hints, so:
1. When `tiling.is_tiled()` is true on any edge, or the window is maximised/fullscreen, draw no shadow, no rounded corners on that edge, and set inset 0. The `window_shadow.rs` example does this per edge from `Decorations::Client{tiling}`.
2. Do not draw min/max buttons unless `window_controls()` reports support; with Hyprland a title bar with only a close button (or none) is normal.
3. Set `app_id` ("sysman") so window rules match, as insensical does.
4. Floating resize still needs the resize-edge hit test; tiled windows do not.
Whether Hyprland actually sends the tiled states over xdg-toplevel is compositor-dependent; I did not test it.

gpui-component helpers: `TitleBar` (`REG/.../src/title_bar.rs`, 434 lines) provides a CSD title bar with drag, double-click, window buttons on Linux, `TitleBar::window_options()` for the options (`:81`), and `window_border.rs` (461 lines) for the shadow/border/resize wrapper. A centred tab strip plus a right-side control fit as children of `TitleBar`; if its layout fights the centring, build the bar by hand (`h_flex` with three flex regions) using only the gpui calls above.

## 4. gpui-component components (all under `REG/gpui-component-0.7.0/src`)

| Need | Component | Virtualised | Notes |
|---|---|---|---|
| Table, sortable, selection, context menu | `table/` (`TableState<D: TableDelegate>`, `DataTable`) | Yes, `uniform_list` rows (`state.rs:20`, `:2139` virtual_list for horizontal) | Delegate: `rows_count`, `column`, `render_td`, `perform_sort` (`delegate.rs:29`), `context_menu(row_ix, PopupMenu)` (`:101`); `Column::sortable()` (`column.rs:107`); builder flags `col_resizable`, `col_movable`, `sortable`, `row_selectable` (`state.rs:340-364`). Flat only. |
| Tree/hierarchy | `tree.rs` (`TreeState`, `tree(...)`, `.context_menu`) | Yes, `uniform_list` in gpui-base `tree.rs:451` | Separate from the table: a single-column list, no columns or sort. A hierarchy mode of the process table means flattening the tree yourself into table rows with indent in the first cell (supported by `render_td`), plus your own expanded set. |
| Tabs / segmented | `tab/` `TabBar::new(id).segmented()`, also `.pill()`, `.underline()` (`tab_bar.rs:80-104`); `prefix`/`suffix`/`menu` (overflow) | n/a | ~10 tabs fine; `max_width` and `track_scroll` available. |
| Search input | `input/` (large; Input, InputState) | n/a | Standard text editor input. |
| Scroll | `scroll/`, `scrollbar` in base | n/a | |
| Tooltip | `tooltip.rs`; gpui `.tooltip(...)` on any element | n/a | |
| Popover/dropdown/context menu | `popover.rs`, `menu/{dropdown_menu,context_menu,popup_menu}.rs` | n/a | |
| Resizable splits | `resizable.rs` `h_resizable(id)`, `v_resizable(id)` (`:28-33`) | n/a | |
| Sidebar, collapsible | `sidebar/` (groups, menu, header, footer; `SidebarCollapsible::{Icon,Offcanvas,None}`) | n/a | Collapsible groups with `accordion.rs`/`collapsible.rs` also exist. |
| Badges/capsules | `badge.rs`, `tag.rs` (`rounded_full`) | n/a | |
| Progress | `progress/{progress,progress_circle}.rs` | n/a | |
| Other | `list/`, `virtual_list.rs`, `dock/`, `notification.rs`, `command/` | | |

Table fit for 700 processes at 2 s: virtualised, so only visible rows (~30) render. Caveat: it is a general table; per-tick cost is a `cx.notify()` on `TableState` re-running `render_td` for visible rows only. The delegate owns the data; sort is delegate-side (`perform_sort`), so re-sorting every 2 s while a column sort is active is your code. Selection by row index will jump on re-sort, so key the selection by PID yourself. Unverified: multi-select (cmd/shift) support; read `table/state.rs` selection code before relying on it.

## 5. Charts

gpui-component ships `chart/{line,area,bar,candlestick,pie,radar,sankey}_chart.rs` over gpui-base `plot/` (scales `linear/point/band`, axis, grid, `shape/{line,area,bar,stack,arc,pie}`, `path_cache`, `hover`).

Against the chart requirements in `docs/APP-DESIGN.md` §5:

| Requirement | Built in? | Evidence |
|---|---|---|
| Line, area, bar, stacked | Yes (`shape/stack.rs`, bar chart 1444 lines) | |
| Monotone cubic | No. `Curve::{Natural, Linear, StepAfter}` only | `gpui-base plot/shape/line.rs:85,179-201`. Natural spline can overshoot; the app requires no overshoot. |
| Translucent min-max band | No dedicated API. Area chart has one `y` accessor and `.fill(Background)`; no lower-bound | `chart/area_chart.rs:165-175` |
| Vertical gradient area fill | Yes: `.fill(impl Into<Background>)` accepts `linear_gradient` | `area_chart.rs:175` |
| Dashed horizontal threshold lines | Lines yes, labels no. `reference_line(value)`, repeatable, dashed | `line_chart.rs:300-308`, `area_chart.rs:308`. No text label or per-line colour. |
| Dashed grid | Yes `grid_dashed(bool)`, `dash_array` | `line_chart.rs:292`, `plot/grid.rs:37` |
| Gaps for missing data | Doubtful. Accessors return `Option<f32>` (`shape/line.rs:13-14,58,67`); None points look skipped when building dots, which would join across the hole. Not verified; test before depending on it |
| Hover cursor + tooltip | Yes: crosshair, dot, tooltip (`line_chart.rs:93-97`, `plot/tooltip.rs`). Custom tooltip content via `tooltip_content` (`:144`) |
| Shared cursor across charts | No. Hover state is per chart `id` (`line_chart.rs:84-92` says siblings sharing an id share one hover state and path cache, but that is a construction-site default, not a cross-chart cursor feature). Charts using the same id would still need the same data and layout. Plan to drive it yourself. |
| ~120 points | Yes; path cache (`path_cache.rs`, `Line::paint_cached`) avoids re-tessellating while points are unchanged. `point_count` hint. |
| Axis ticks/format | Yes `y_tick_format`, `y_domain`, `x_tick_count`, `y_padding` | |

Conclusion: the built-ins cover a plain line/area chart with a tooltip, but not monotone, band, labelled thresholds or a shared cursor. sysman will need a custom chart anyway, and it is small. Primitives at the pinned rev (`ZED/crates/gpui/src`):
- `canvas(prepaint, paint)` (`elements/canvas.rs:10`): `paint: FnOnce(Bounds<Pixels>, T, &mut Window, &mut App)`. Closures are `FnOnce` and `'static`, so capture owned/`Rc` data (cheap-clone the series).
- Stroke: `PathBuilder::stroke(px(w))`, `.dash_array(&[px(4.), px(3.)])` (`path_builder.rs:88,108`), `move_to`, `line_to`, `curve_to(to, ctrl)` (quadratic), `cubic_bezier_to` (`:144`; use for monotone cubic with Fritsch-Carlson tangents converted to Bezier controls), `build() -> Result<Path>`, then `window.paint_path(path, color_or_Background)` (`window.rs:4573`). Round caps/joins: check `PathStyle`/`StrokeOptions` at `path_builder.rs:17`; I did not confirm cap/join settings are exposed.
- Fill: `PathBuilder::fill()`, close the polygon to the baseline, `paint_path(path, linear_gradient(180., linear_color_stop(c.opacity(0.42), 0.), linear_color_stop(c.opacity(0.04), 1.)))` (`color.rs:865,893`). Insensical already uses angle 180 for a top-to-bottom gradient (`dialog.rs:68`). The band is a closed fill path (upper edge forward, lower edge back) with a flat translucent colour. Gaps: build one sub-path per contiguous run.
- Clipping: `window.with_content_mask(Some(ContentMask{bounds}), |window| ...)` (`window.rs:3981`).
- Quads: `window.paint_quad(fill(bounds, bg))`, `outline(...)` (`window.rs:4502,7620,7632`).
- Text in a canvas: `window.text_system().shape_line(text: SharedString, font_size, &[TextRun], force_width) -> ShapedLine` (`text_system.rs:638`, no newlines), then `ShapedLine::paint(origin, line_height, window, cx)` (`text_system/line.rs:108`). Measure with `shaped.width`. Insensical's `grid.rs` is a worked example of shaping and painting runs.
- Hover in a canvas: in prepaint, `window.insert_hitbox(bounds, HitboxBehavior::Normal)` (`window.rs:5109`); in paint, `window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| ...)` (`window.rs:5246`), test `hitbox.is_hovered(window)`, convert `e.position` to a data index, write it to a shared `Entity<CursorState>` and `cx.notify(..)`. A shared cursor across charts = one entity holding `Option<time index>` that all charts read and paint a vertical rule from; tooltips are a `div` child positioned absolutely, or a `deferred` overlay. `window.set_cursor_style(style, &hitbox)` (`:3945`).
- Caching: build the `Path` once per data update, keep it in the view, `paint_path` clones it each frame (the `Path` is cheap to hold; gpui-base's `PathCache` keyed by points hash is the model, `plot/path_cache.rs:25`). Rebuild only when the series or bounds change.

## 6. Treemap
No treemap in gpui-component (chart list above; `sankey_chart`, `pie_chart`, `bar_chart` only). Build custom: squarified layout in plain Rust, then either `div().absolute()` children per rect (simple, hit-testing and tooltips for free, fine up to a few hundred rects) or one `canvas` with `paint_quad` + `shape_line` labels (cheaper for thousands, hover via manual hit test). Recommend divs for the visible level with drill-down, since the Mac reference also draws a custom canvas (`TreemapSurfaceView.swift`).

## 7. Fonts and text
- Tabular digits: `FontFeatures(Arc::new(vec![("tnum".into(), 1)]))` (`ZED/crates/gpui/src/text_system/font_features.rs:8`; gpui-component does exactly this in `time/time_field.rs:18`); apply with `.font_features(f)` on a styled element (`styled.rs:714`) or via `TextRun.font.features` (`text_system.rs`-level `Font.features`, `platform.rs:1299`). It only works if the chosen font has a `tnum` feature (e.g. Inter, Noto Sans, JetBrains Mono is already monospaced). Not tested on this machine.
- System fonts on Linux: gpui with `font-kit` uses fontconfig; `cx.text_system().all_font_names()` lists them (`text_system.rs:284`). Pick a family by name via `font("Inter")`; embed a fallback with `cx.text_system().add_fonts(...)` (insensical does this in `theme::add_fonts`, `main.rs:30`). Ship one UI font to be sure of `tnum`.
- Measurement: `shape_line(..).width`, or `window.text_system().layout_line(...)` (`text_system.rs:886`) for wrapped text.

## 8. Theming (gpui-component)
`theme/` (`mod.rs`, `schema.rs`, `registry.rs`, `default-theme.json`, `default-colors.json`). Global `Theme::global(cx)` / `cx.theme()` (via `ActiveTheme`), `Theme::change(ThemeMode, window, cx)` (`mod.rs:369`), `sync_system_appearance` (`:334`), `Theme::update(cx, |t| ...)` (`:268`). Themes are JSON-schema'd colour sets (light/dark) loadable at runtime from the registry; there is also a semantic/colour/spacing/typography/shadow token layer (`semantic_tokens`, `apply_semantic_config_str`, `mod.rs:485-648`). Custom tokens (e.g. chart series colours, threshold colours): the `Theme` struct has a fixed field set, so keep sysman's own tokens in a separate gpui Global (as insensical does) derived from the active gpui-component theme. Dark-first: set `ThemeMode::Dark` at startup. I did not enumerate the colour field names.

## 9. Performance guidance
- Scope `cx.notify()` to the entity that changed: one `Entity` per panel/chart; a sampler task updates a shared model and notifies only the visible tab's views. Views of hidden tabs should not render (render only the active tab) and should skip rebuilding paths.
- Do not hold 700 rows in an element tree; use the virtualised `TableState`, or `uniform_list` directly if the table is custom. Notify the table only when visible rows changed (`visible_rows_changed` hook, `delegate.rs:202`).
- Cache chart `Path`s per series; rebuild on new sample, not per frame. A 2 s tick is 0.5 repaints/s of data, but hover repaints every mouse move, so a cached path matters.
- Hide samples while the window is hidden but keep collecting.
- Insensical's `profile.dev.package."*" opt-level = 2` (`INS/Cargo.toml`) exists because gpui is too slow unoptimised; copy it.

## 10. Recommendation

Use `gpui-pre`/`gpui-pre-platform` from crates.io (renamed to `gpui`/`gpui_platform`) with `gpui-component`. Concretely: start on `gpui-pre =0.3.7` + `gpui-component =0.7.0` (the exact snapshot insensical already runs, zed@1a28cff, and the one whose source is in the local registry, so the code in this doc is checkable), and move to `=0.3.9`/`=0.7.3` when a first screen compiles, since both are weekly and 0.7.3 is the current release. Only reasons to stay on git `1a28cff` instead: sharing code with insensical or avoiding gpui-component. Avoid gpui 0.2.2 (needs gpui-component 0.5.x, older, no gpui-base).

Trade-offs:
- Pro: table (virtualised, sortable, context menu), tabs (segmented), resizable, sidebar, popover/menus, tooltips, input, title bar and window border for CSD all exist; `Apache-2.0`.
- Con: gpui-component is a large dependency (11.9k lines in just chart/table/title bar); the project is in a rapid rename/rewrite phase (gpui-component to gpui-kit, 0.5 to 0.7 in months, weekly snapshots, `=` pins on every gpui-pre release).
- Alternative without it: copy insensical's approach (own `Theme` global, `div` + `canvas`), but you must build table, inputs, menus, tabs yourself; not recommended for a table with sort, selection and context menu.

Build ourselves regardless: monotone-cubic line with min-max band and gap handling, labelled threshold rules, shared hover cursor, treemap, tree-mode process table (flattening), the centred tab strip layout in the title bar, Hyprland-aware tiling behaviour (shadows/inset off when tiled).

Risks:
1. Version lockstep: gpui-pre and gpui-component are exact-pinned; a typo gives two gpui copies and baffling type errors. Check `cargo tree -d`.
2. API churn between snapshots (weekly) and the move from `gpui-component` to `gpui-kit`; pin exact versions and commit `Cargo.lock`.
3. Chart gaps and Option accessors, table multi-select, `PathBuilder` caps/joins, `gpui-pre-platform 0.3.9` features: not verified here.
4. Wayland CSD on a tiling compositor depends on Hyprland's tiled-state signalling; test on the real compositor early.
5. Monotone/band/shared cursor are custom work; budget for a bespoke chart module rather than gpui-component's charts.
6. Process table at 2 s with sort active: selection must be keyed by PID.
