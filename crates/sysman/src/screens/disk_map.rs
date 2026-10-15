use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, App, AppContext, BorderStyle, Bounds, ClickEvent, ClipboardItem, ContentMask, Context, DispatchPhase,
    Entity, FocusHandle, Font, FontWeight, Hitbox, HitboxBehavior, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, PathBuilder, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, TextAlign, WeakEntity, Window, canvas, div, fill, linear_color_stop,
    linear_gradient, outline, point, prelude::FluentBuilder, px, relative, rgb, size,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::{Disableable, Icon, Sizable, WindowExt};
use gpui_kit_assets::IconName;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use schema::fs::{Filesystem, Fs};

use crate::charts::format::bytes;
use crate::charts::trend::shape;
use crate::data::diskmap::{self, DIR, FOLD, HARD_LINK, Kind, OTHER_FS, ROOT, SYMLINK, Scan, Top, Tree, UNREADABLE};
use crate::data::live::Live;
use crate::ui::shell::paused;
use crate::ui::theme::theme;
use crate::ui::widgets::segmented::segmented;
use crate::ui::widgets::sidebar::{self, Side, Sidebar, Toggled, Widths};

const RAIL: Widths = Widths::new(340., 280., 480.);
const ROW_LIMIT: usize = 200;
const STRIP: f32 = 19.;
const PAD: f32 = 2.;
const OPEN_MIN_W: f32 = 40.;
const OPEN_MIN_H: f32 = 28.;
const MAX_DEPTH: u8 = 6;
/// Children smaller than this many square pixels join the folder's "N more" tile.
const MIN_TILE_AREA: f64 = 120.;
const MAX_TILES_PER_FOLDER: usize = 400;
const LABEL_MIN_W: f32 = 56.;
const LABEL_MIN_H: f32 = 20.;
/// Treemap labels never shrink below these; a tile too small for them goes unlabelled.
const TILE_NAME: f32 = 12.;
const TILE_SIZE: f32 = 11.;
const TILE_LINE: f32 = 15.;
/// The list rows' text and height.
const ROW_TEXT: f32 = 14.;
const ROW_H: f32 = 30.;
const DEFAULT_ROOT: &str = "/home";

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn area(&self) -> f64 {
        f64::from(self.w.max(0.)) * f64::from(self.h.max(0.))
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// The aspect ratio of the worst tile in a row of `sum` total area laid along a side of length `side`.
fn worst(row: &[f64], sum: f64, side: f64) -> f64 {
    let (lo, hi) = row
        .iter()
        .fold((f64::INFINITY, 0f64), |(lo, hi), a| (lo.min(*a), hi.max(*a)));
    let (s2, w2) = (sum * sum, side * side);
    (w2 * hi / s2).max(s2 / (w2 * lo))
}

/// Squarified treemap (Bruls, Huizing, van Wijk): `values` largest first, one rect each, filling `r`.
pub fn squarify(values: &[f64], r: Rect) -> Vec<Rect> {
    let total: f64 = values.iter().sum();
    let mut out = vec![
        Rect {
            x: r.x,
            y: r.y,
            w: 0.,
            h: 0.
        };
        values.len()
    ];
    if total <= 0. || r.area() <= 0. {
        return out;
    }
    let scale = r.area() / total;
    let areas: Vec<f64> = values.iter().map(|v| v.max(0.) * scale).collect();
    let (mut x, mut y, mut w, mut h) = (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
    let mut i = 0;
    while i < areas.len() {
        let side = w.min(h);
        let mut j = i + 1;
        let mut sum = areas[i];
        let mut best = worst(&areas[i..j], sum, side);
        while j < areas.len() {
            let next = worst(&areas[i..=j], sum + areas[j], side);
            if next > best {
                break;
            }
            best = next;
            sum += areas[j];
            j += 1;
        }
        let last = j == areas.len();
        if w >= h {
            // The row stands as a column at the left of what is left.
            let cw = if last { w } else { (sum / h).min(w) };
            let mut cy = y;
            for k in i..j {
                let ch = if sum > 0. { h * areas[k] / sum } else { 0. };
                out[k] = Rect {
                    x: x as f32,
                    y: cy as f32,
                    w: cw as f32,
                    h: ch as f32,
                };
                cy += ch;
            }
            x += cw;
            w -= cw;
        } else {
            let rh = if last { h } else { (sum / w).min(h) };
            let mut cx = x;
            for k in i..j {
                let cw = if sum > 0. { w * areas[k] / sum } else { 0. };
                out[k] = Rect {
                    x: cx as f32,
                    y: y as f32,
                    w: cw as f32,
                    h: rh as f32,
                };
                cx += cw;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    Leaf,
    /// Subdivided into its children, with a title strip when there is room.
    Folder {
        strip: bool,
    },
    /// Children of `node` too small to draw one by one.
    More {
        count: u32,
        bytes: u64,
    },
}

#[derive(Debug, Clone, Copy)]
struct Tile {
    rect: Rect,
    node: u32,
    shape: Shape,
}

/// Every tile of the map with `zoom` filling `area`, parents before their children.
fn scene(tree: &Tree, zoom: u32, area: Rect) -> Vec<Tile> {
    let mut out = Vec::new();
    lay_out(tree, zoom, area, 0, &mut out);
    out
}

fn lay_out(tree: &Tree, parent: u32, r: Rect, depth: u8, out: &mut Vec<Tile>) {
    let kids: Vec<u32> = tree
        .node(parent)
        .children
        .iter()
        .copied()
        .filter(|c| tree.node(*c).bytes > 0)
        .collect();
    let total: u64 = kids.iter().map(|c| tree.node(*c).bytes).sum();
    if total == 0 || r.w < 1. || r.h < 1. {
        return;
    }
    let per_byte = r.area() / total as f64;
    let mut shown = kids
        .iter()
        .take_while(|c| tree.node(**c).bytes as f64 * per_byte >= MIN_TILE_AREA)
        .count()
        .min(MAX_TILES_PER_FOLDER);
    if kids.len() - shown == 1 {
        shown += 1;
    }
    let rest = &kids[shown..];
    let rest_bytes: u64 = rest.iter().map(|c| tree.node(*c).bytes).sum();
    let mut values: Vec<f64> = kids[..shown].iter().map(|c| tree.node(*c).bytes as f64).collect();
    if !rest.is_empty() {
        values.push(rest_bytes as f64);
    }
    for (i, rect) in squarify(&values, r).into_iter().enumerate() {
        let Some(&node) = kids.get(i).filter(|_| i < shown) else {
            out.push(Tile {
                rect,
                node: parent,
                shape: Shape::More {
                    count: rest.len() as u32,
                    bytes: rest_bytes,
                },
            });
            continue;
        };
        let open = tree.openable(node) && rect.w >= OPEN_MIN_W && rect.h >= OPEN_MIN_H && depth + 1 < MAX_DEPTH;
        if !open {
            out.push(Tile {
                rect,
                node,
                shape: Shape::Leaf,
            });
            continue;
        }
        let strip = rect.h >= STRIP + 12.;
        out.push(Tile {
            rect,
            node,
            shape: Shape::Folder { strip },
        });
        let top = if strip { STRIP } else { PAD };
        let inner = Rect {
            x: rect.x + PAD,
            y: rect.y + top,
            w: rect.w - PAD * 2.,
            h: rect.h - top - PAD,
        };
        lay_out(tree, node, inner, depth + 1, out);
    }
}

/// The innermost tile under a point.
fn hit(tiles: &[Tile], x: f32, y: f32) -> Option<usize> {
    tiles.iter().rposition(|t| t.rect.contains(x, y))
}

fn kind_colour(kind: Kind) -> Hsla {
    rgb(match kind {
        Kind::Folder => 0x5F6F87,
        Kind::Application => 0x4F8DF7,
        Kind::Video => 0x9D74F7,
        Kind::Image => 0xF08030,
        Kind::Audio => 0xE45C9E,
        Kind::Archive => 0x22B5A5,
        Kind::Document => 0xD4A517,
        Kind::Code => 0x7478F2,
        Kind::Data => 0x2AA9E4,
        Kind::Cache => 0xB07414,
        Kind::System => 0x707E94,
        Kind::Other => 0x7B8290,
    })
    .into()
}

const AGE_BANDS: [(&str, i64, u32); 5] = [
    ("Under 1 month", 30, 0x4F8DF7),
    ("1 to 6 months", 182, 0x22B5A5),
    ("6 to 12 months", 365, 0xD4A517),
    ("1 to 2 years", 730, 0xF08030),
    ("Over 2 years", i64::MAX, 0xC63B3B),
];

/// 0 (recent) to 4 (old); an unknown date lands in the oldest band so it never passes for fresh.
fn age_band(modified: i64, now: i64) -> usize {
    if modified <= 0 {
        return AGE_BANDS.len() - 1;
    }
    let days = (now - modified).max(0) / 86_400;
    AGE_BANDS
        .iter()
        .position(|(_, max, _)| days < *max)
        .unwrap_or(AGE_BANDS.len() - 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Colour {
    Kind,
    Age,
}

fn fill_of(tree: &Tree, node: u32, colour: Colour, now: i64) -> Hsla {
    let n = tree.node(node);
    match colour {
        Colour::Kind => kind_colour(if n.is(DIR) { Kind::Folder } else { n.kind }),
        Colour::Age => rgb(AGE_BANDS[age_band(n.modified, now)].2).into(),
    }
}

fn label_ink(fill: Hsla) -> Hsla {
    let c = fill.to_rgb();
    if 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b > 0.62 {
        gpui::black().opacity(0.8)
    } else {
        gpui::white().opacity(0.94)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Map,
    Largest,
    Oldest,
    Kinds,
}

impl Mode {
    const ALL: [Mode; 4] = [Mode::Map, Mode::Largest, Mode::Oldest, Mode::Kinds];

    fn label(self) -> &'static str {
        match self {
            Mode::Map => "Map",
            Mode::Largest => "Largest",
            Mode::Oldest => "Oldest",
            Mode::Kinds => "Kinds",
        }
    }
}

const AGE_CHOICES: [(&str, i64); 3] = [("Over 6 months", 182), ("Over 1 year", 365), ("Over 2 years", 730)];
const SIZE_CHOICES: [(&str, u64); 3] = [("Any size", 0), ("Over 10 MB", 10 << 20), ("Over 100 MB", 100 << 20)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    Size,
    Modified,
    Kind,
    Location,
}

/// The parts of the volume bar: scanned, unaccounted, reserved, free.
fn segments(scanned: u64, fs: &Filesystem) -> [u64; 4] {
    let used = fs.used_bytes;
    [
        scanned.min(used),
        used.saturating_sub(scanned),
        fs.total_bytes.saturating_sub(used + fs.avail_bytes),
        fs.avail_bytes,
    ]
}

fn filesystem_of<'a>(fs: &'a Fs, root: &Path) -> Option<&'a Filesystem> {
    fs.values().find(|f| f.mounts.iter().any(|m| Path::new(m) == root))
}

/// The filesystem holding `root` and the mount point it is reached by: the longest one containing it.
fn filesystem_holding<'a>(fs: &'a Fs, root: &Path) -> Option<(&'a Filesystem, &'a str)> {
    fs.values()
        .flat_map(|f| f.mounts.iter().map(move |m| (f, m.as_str())))
        .filter(|(_, m)| root.starts_with(m))
        .max_by_key(|(_, m)| Path::new(m).components().count())
}

/// `text` cut to at most `max` characters by dropping its middle, so both ends of a path stay readable.
fn middle_ellipsis(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep / 2;
    let tail = keep - head;
    chars[..head]
        .iter()
        .chain(['…'].iter())
        .chain(&chars[chars.len() - tail..])
        .collect()
}

/// `m:ss`, or `h:mm:ss` from an hour on.
fn clock(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

const GLIDE: Duration = Duration::from_millis(320);
const RING_EASE: Duration = Duration::from_millis(400);
const DONE_FADE: Duration = Duration::from_millis(450);
/// Top-level entries drawn while scanning; the rest are too small to see.
const SCAN_TILES: usize = 80;

fn ease_out(t: f32) -> f32 {
    1. - (1. - t).powi(3)
}

fn eased(since: Instant, now: Instant, over: Duration) -> f32 {
    ease_out((now.saturating_duration_since(since).as_secs_f32() / over.as_secs_f32()).min(1.))
}

fn lerp_rect(a: Rect, b: Rect, k: f32) -> Rect {
    Rect {
        x: a.x + (b.x - a.x) * k,
        y: a.y + (b.y - a.y) * k,
        w: a.w + (b.w - a.w) * k,
        h: a.h + (b.h - a.h) * k,
    }
}

/// Tiles that move to each new layout over [`GLIDE`] rather than jumping, one slot per top-level entry.
#[derive(Default)]
struct Glide {
    from: Vec<Option<Rect>>,
    to: Vec<Option<Rect>>,
    since: Option<Instant>,
    /// The area laid out; a new one is a resize, which snaps.
    area: (f32, f32),
}

impl Glide {
    fn retarget(&mut self, to: Vec<Option<Rect>>, area: (f32, f32), now: Instant) {
        if to == self.to {
            return;
        }
        if area != self.area && !self.to.is_empty() {
            self.area = area;
            self.to = to;
            self.since = None;
            return;
        }
        self.area = area;
        self.from = self.at(now);
        self.to = to;
        self.since = Some(now);
    }

    fn at(&self, now: Instant) -> Vec<Option<Rect>> {
        let k = self.since.map_or(1., |since| eased(since, now, GLIDE));
        self.to
            .iter()
            .enumerate()
            .map(|(i, to)| {
                let to = (*to)?;
                // a new tile grows out of its own centre
                let from = self.from.get(i).copied().flatten().unwrap_or(Rect {
                    x: to.x + to.w / 2.,
                    y: to.y + to.h / 2.,
                    w: 0.,
                    h: 0.,
                });
                Some(lerp_rect(from, to, k))
            })
            .collect()
    }
}

/// A number that eases to each new value over [`RING_EASE`].
#[derive(Default)]
struct Eased {
    from: f32,
    to: f32,
    since: Option<Instant>,
}

impl Eased {
    fn set(&mut self, to: f32, now: Instant) {
        if to != self.to {
            self.from = self.at(now);
            self.to = to;
            self.since = Some(now);
        }
    }

    fn at(&self, now: Instant) -> f32 {
        let k = self.since.map_or(1., |since| eased(since, now, RING_EASE));
        self.from + (self.to - self.from) * k
    }
}

/// What the scanning view animates, kept across frames for one scan.
#[derive(Default)]
struct ScanVisual {
    /// The scan and the level drawn; either changing starts the animation afresh.
    key: (usize, PathBuf),
    ring: Eased,
    glide: Glide,
    /// When each top-level entry was first seen complete, for its fade to colour.
    done_at: Vec<Option<Instant>>,
}

/// A circular arc from `from` to `to` turns, clockwise from twelve o'clock.
fn arc(c: gpui::Point<Pixels>, r: f32, from: f32, to: f32, width: f32) -> Option<gpui::Path<Pixels>> {
    let steps = ((to - from).abs() * 160.).ceil().max(2.) as usize;
    let mut path = PathBuilder::stroke(px(width));
    for i in 0..=steps {
        let a = (from + (to - from) * i as f32 / steps as f32 - 0.25) * std::f32::consts::TAU;
        let p = point(c.x + px(r * a.cos()), c.y + px(r * a.sin()));
        if i == 0 {
            path.move_to(p);
        } else {
            path.line_to(p);
        }
    }
    path.build().ok()
}

/// The progress ring: a track, the share found so far with a soft glow, and a comet that keeps circling.
fn paint_ring(
    bounds: Bounds<Pixels>,
    fraction: Option<f32>,
    turn: f32,
    (track, start, end): (Hsla, Hsla, Hsla),
    window: &mut Window,
) {
    let c = bounds.center();
    let r = f32::from(bounds.size.width.min(bounds.size.height)) / 2. - 20.;
    let stroke = 12.;
    if let Some(p) = arc(c, r, 0., 1., stroke) {
        window.paint_path(p, track);
    }
    let gradient = || linear_gradient(135., linear_color_stop(start, 0.), linear_color_stop(end, 1.));
    let (from, to) = match fraction {
        Some(f) => (0., f.clamp(0., 1.)),
        None => (turn, turn + 0.3),
    };
    if to - from > 0.002 {
        let length = (to - from) * r * std::f32::consts::TAU;
        let glow = (length / 120.).min(1.);
        for (width, alpha) in [(stroke + 22., 0.05), (stroke + 14., 0.08), (stroke + 6., 0.14)] {
            if let Some(p) = arc(c, r, from, to, width) {
                window.paint_path(p, start.opacity(alpha * glow));
            }
        }
        if let Some(p) = arc(c, r, from, to, stroke) {
            window.paint_path(p, gradient());
        }
        let a = (to - 0.25) * std::f32::consts::TAU;
        let head = point(c.x + px(r * a.cos()), c.y + px(r * a.sin()));
        let dot = px(stroke + 4.);
        window.paint_quad(
            fill(
                Bounds::new(head - point(dot / 2., dot / 2.), size(dot, dot)),
                gpui::white().opacity(0.92),
            )
            .corner_radii(dot / 2.),
        );
    }
    if fraction.is_some() {
        // a faint comet on the track, so a slow stretch of the scan still visibly runs
        let head = turn;
        for (k, alpha) in [(0.10, 0.10), (0.05, 0.16), (0.02, 0.24)] {
            if let Some(p) = arc(c, r, head - k, head, stroke) {
                window.paint_path(p, gpui::white().opacity(alpha));
            }
        }
    }
}

fn now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn ago(seconds: i64) -> String {
    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match seconds {
        ..5 => "just now".into(),
        5..60 => plural(seconds, "second"),
        60..3600 => plural(seconds / 60, "minute"),
        3600..86_400 => plural(seconds / 3600, "hour"),
        _ => plural(seconds / 86_400, "day"),
    }
}

fn date(seconds: i64) -> String {
    Timestamp::from_second(seconds)
        .map(|t| t.to_zoned(TimeZone::system()).strftime("%-d %b %Y").to_string())
        .unwrap_or_else(|_| "–".into())
}

fn modified_text(seconds: i64, now: i64) -> String {
    if seconds <= 0 {
        "–".into()
    } else if now - seconds < 7 * 86_400 {
        ago(now - seconds)
    } else {
        date(seconds)
    }
}

/// `2.47 M`, `512`.
fn compact(n: u64) -> String {
    match n {
        0..10_000 => n.to_string(),
        10_000..1_000_000 => format!("{:.1} k", n as f64 / 1e3),
        _ => format!("{:.2} M", n as f64 / 1e6),
    }
}

fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "–".into();
    }
    let pct = part as f64 / whole as f64 * 100.;
    if pct > 0. && pct < 0.1 {
        "<0.1%".into()
    } else {
        format!("{pct:.1}%")
    }
}

#[derive(Clone, PartialEq)]
struct RowsKey {
    tree: usize,
    mode: Mode,
    age: usize,
    size: usize,
    kind: Option<Kind>,
}

/// Bytes and items per kind, largest first.
type KindTotals = Rc<Vec<(Kind, u64, u64)>>;

struct SceneCache {
    key: (usize, u32, (i32, i32)),
    tiles: Rc<Vec<Tile>>,
}

pub struct DiskMap {
    live: Entity<Live>,
    rail: Entity<Sidebar>,
    root: PathBuf,
    tree: Option<Arc<Tree>>,
    scan: Option<Arc<Scan>>,
    error: Option<SharedString>,
    mode: Mode,
    colour: Colour,
    zoom: u32,
    selected: Option<u32>,
    /// What the rail shows: the selection, kept through the rail's slide out after a deselect.
    shown: Option<u32>,
    refusal: Option<&'static str>,
    hover: Rc<Cell<Option<usize>>>,
    scene: Rc<RefCell<Option<SceneCache>>>,
    sort: (Col, bool),
    age: usize,
    min_size: usize,
    kind: Option<Kind>,
    rows: Option<(RowsKey, Rc<Vec<u32>>)>,
    kinds: Option<(usize, KindTotals)>,
    focus: FocusHandle,
    visual: Rc<RefCell<ScanVisual>>,
    #[cfg(debug_assertions)]
    dev_select: bool,
}

fn dev_colour() -> Colour {
    #[cfg(debug_assertions)]
    if std::env::var("SYSMAN_DEV_DISKMAP_COLOUR").is_ok_and(|c| c.eq_ignore_ascii_case("age")) {
        return Colour::Age;
    }
    Colour::Kind
}

fn tree_key(tree: &Arc<Tree>) -> usize {
    Arc::as_ptr(tree) as usize
}

impl DiskMap {
    pub fn new(live: Entity<Live>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |_, _, cx| cx.notify()).detach();
        #[cfg(debug_assertions)]
        let root = std::env::var("SYSMAN_DEV_DISKMAP_ROOT").unwrap_or_else(|_| DEFAULT_ROOT.into());
        #[cfg(not(debug_assertions))]
        let root = DEFAULT_ROOT.to_string();
        #[cfg(debug_assertions)]
        let mode = std::env::var("SYSMAN_DEV_DISKMAP_MODE")
            .ok()
            .and_then(|m| Mode::ALL.into_iter().find(|x| x.label().eq_ignore_ascii_case(&m)))
            .unwrap_or(Mode::Map);
        #[cfg(not(debug_assertions))]
        let mode = Mode::Map;
        let mut this = Self {
            live,
            rail: cx.new(|_| Sidebar::transient("disk_map.right", Side::Right, RAIL)),
            root: PathBuf::new(),
            tree: None,
            scan: None,
            error: None,
            mode,
            colour: dev_colour(),
            zoom: ROOT,
            selected: None,
            shown: None,
            refusal: None,
            hover: Rc::default(),
            scene: Rc::default(),
            sort: (Col::Size, true),
            age: 1,
            min_size: 1,
            kind: None,
            rows: None,
            kinds: None,
            focus: cx.focus_handle(),
            visual: Rc::default(),
            #[cfg(debug_assertions)]
            dev_select: std::env::var_os("SYSMAN_DEV_DISKMAP_SELECT").is_some(),
        };
        // the rail's own hide button closes it; that is a deselect too
        cx.subscribe(&this.rail, |this, rail, _: &Toggled, cx| {
            if rail.read(cx).collapsed() && this.selected.is_some() {
                this.select(None, cx);
            }
        })
        .detach();
        this.choose(PathBuf::from(root), cx);
        this
    }

    fn choose(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.root = root;
        self.error = None;
        self.scan = diskmap::running(&self.root);
        let cached = diskmap::cached(&self.root);
        if cached.is_none() && self.scan.is_none() {
            self.scan = Some(diskmap::start(&self.root));
        }
        self.set_tree(cached, cx);
        if self.scan.is_some() {
            self.watch(cx);
        }
        cx.notify();
    }

    fn set_tree(&mut self, tree: Option<Arc<Tree>>, cx: &mut Context<Self>) {
        self.tree = tree;
        self.zoom = ROOT;
        self.hover.set(None);
        self.select(None, cx);
        self.shown = None;
        #[cfg(debug_assertions)]
        if self.dev_select
            && let Some(tree) = &self.tree
        {
            let biggest = tree.largest(tree.files(), 1).first().copied();
            self.select(biggest, cx);
        }
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.scan = Some(diskmap::start(&self.root));
        self.watch(cx);
        cx.notify();
    }

    fn watch(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let running = this.update(cx, |this, cx| {
                    this.poll(cx);
                    cx.notify();
                    this.scan.is_some()
                });
                if !running.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let Some(outcome) = self.scan.as_ref().and_then(|s| s.outcome()) else {
            return;
        };
        self.scan = None;
        match outcome {
            Ok(tree) => self.set_tree(Some(tree), cx),
            Err(e) => self.error = Some(format!("Could not scan {}: {e}", self.root.display()).into()),
        }
    }

    /// The rail slides in with a selection and out without one.
    fn select(&mut self, node: Option<u32>, cx: &mut Context<Self>) {
        self.selected = node;
        if node.is_some() {
            self.shown = node;
        }
        self.refusal = match (node, &self.tree) {
            (Some(id), Some(tree)) => Some(self.refuse(tree, id)).flatten(),
            _ => None,
        };
        self.rail.update(cx, |rail, cx| rail.set_collapsed(node.is_none(), cx));
        cx.notify();
    }

    fn refuse(&self, tree: &Tree, id: u32) -> Option<&'static str> {
        let n = tree.node(id);
        if id == ROOT {
            return Some("The scanned folder itself cannot be trashed.");
        }
        if n.is(FOLD) {
            return Some("Small files are folded together for the map; open their folder to trash them.");
        }
        let Some(trash) = diskmap::home_trash() else {
            return Some("No home folder, so no Trash.");
        };
        diskmap::trash_refusal(&tree.path(id), &trash)
    }

    fn zoom_to(&mut self, node: u32, cx: &mut Context<Self>) {
        self.zoom = node;
        self.hover.set(None);
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(tree) = &self.tree
            && self.zoom != ROOT
        {
            let parent = tree.node(self.zoom).parent;
            self.zoom_to(parent, cx);
        }
    }

    /// Switches to the map with `node` selected and its folder open.
    fn reveal(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(tree) = &self.tree else { return };
        let parent = if node == ROOT { ROOT } else { tree.node(node).parent };
        self.mode = Mode::Map;
        self.select(Some(node), cx);
        self.zoom_to(parent, cx);
    }

    fn clicked(&mut self, tile: Tile, clicks: usize, cx: &mut Context<Self>) {
        self.select(Some(tile.node), cx);
        if clicks >= 2
            && !matches!(tile.shape, Shape::More { .. })
            && self.tree.as_ref().is_some_and(|t| t.openable(tile.node))
        {
            self.zoom_to(tile.node, cx);
        }
        cx.notify();
    }

    fn ask_trash(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        let n = tree.node(id);
        let name = tree.name(id).to_string();
        let mut detail = bytes(n.bytes as f64);
        if n.is(DIR) {
            detail.push_str(&format!(", {} items", compact(n.items.saturating_sub(1))));
        }
        let description = format!(
            "{detail}. It goes to the Trash in your home folder, where you can restore it until the Trash is \
             emptied; the space is freed only then."
        );
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .title(SharedString::from(format!("Move “{name}” to the Trash?")))
                .description(SharedString::from(description.clone()))
                .ok_text("Move to Trash")
                .ok_variant(ButtonVariant::Danger)
                .show_cancel(true)
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| this.trash(id, window, cx)).ok();
                    true
                })
        });
    }

    fn trash(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        let path = tree.path(id);
        let name = tree.name(id).to_string();
        match diskmap::trash(&path) {
            Ok(_) => {
                let mut patched = (*tree).clone();
                patched.remove(id);
                let patched = Arc::new(patched);
                diskmap::store(patched.clone());
                let parent = tree.node(id).parent;
                self.tree = Some(patched);
                self.select(Some(parent), cx);
                window.push_notification(Notification::success(format!("Moved “{name}” to the Trash")), cx);
            }
            Err(e) => window.push_notification(
                Notification::error(format!("Could not move “{name}” to the Trash: {e}")),
                cx,
            ),
        }
        cx.notify();
    }

    fn rows(&mut self, tree: &Arc<Tree>) -> Rc<Vec<u32>> {
        let key = RowsKey {
            tree: tree_key(tree),
            mode: self.mode,
            age: self.age,
            size: self.min_size,
            kind: self.kind,
        };
        if let Some((k, rows)) = &self.rows
            && *k == key
        {
            return rows.clone();
        }
        let cutoff = now_s() - AGE_CHOICES[self.age].1 * 86_400;
        let min = SIZE_CHOICES[self.min_size].1;
        let kind = self.kind;
        let rows = Rc::new(match self.mode {
            Mode::Oldest => tree.largest(
                tree.files().filter(|id| {
                    let n = tree.node(*id);
                    n.modified > 0 && n.modified < cutoff && n.bytes >= min
                }),
                ROW_LIMIT,
            ),
            Mode::Kinds => tree.largest(tree.files().filter(|id| Some(tree.node(*id).kind) == kind), ROW_LIMIT),
            _ => tree.largest(tree.files(), ROW_LIMIT),
        });
        self.rows = Some((key, rows.clone()));
        rows
    }

    fn kind_totals(&mut self, tree: &Arc<Tree>) -> KindTotals {
        if let Some((k, totals)) = &self.kinds
            && *k == tree_key(tree)
        {
            return totals.clone();
        }
        let totals = Rc::new(tree.kinds());
        self.kinds = Some((tree_key(tree), totals.clone()));
        totals
    }
}

fn hline(cx: &App) -> gpui::Div {
    let t = theme(cx);
    div().h(t.hairline_width).flex_shrink_0().bg(t.hairline)
}

fn swatch(color: Hsla, side: f32) -> gpui::Div {
    div().size(px(side)).flex_shrink_0().rounded(px(2.)).bg(color)
}

fn chip(text: impl Into<SharedString>, color: Option<Hsla>, cx: &App) -> gpui::Div {
    let t = theme(cx);
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded_full()
        .text_size(t.small_caption)
        .map(|d| match color {
            Some(c) => d.bg(c.opacity(0.16)).text_color(c).font_weight(FontWeight::SEMIBOLD),
            None => d.bg(t.text.opacity(0.08)).text_color(t.text_secondary),
        })
        .child(text.into())
}

impl DiskMap {
    fn toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let this = cx.entity().downgrade();
        let fs = self.live.read(cx).latest.as_ref().and_then(|s| s.fs.clone());
        let mut mounts: Vec<(String, String)> = fs
            .iter()
            .flat_map(|fs| fs.values())
            .flat_map(|f| {
                f.mounts.iter().map(move |m| {
                    (
                        m.clone(),
                        format!("{m}    {} · {}", f.fstype, bytes(f.total_bytes as f64)),
                    )
                })
            })
            .collect();
        mounts.sort();
        if !mounts.iter().any(|(m, _)| Path::new(m) == self.root) {
            let root = self.root.display().to_string();
            mounts.insert(0, (root.clone(), root));
        }
        let current = self.root.clone();
        let scanning = self.scan.clone();
        let volume =
            Button::new("disk-map-volume")
                .ghost()
                .small()
                .icon(Icon::new(IconName::HardDrive))
                .label(SharedString::from(self.root.display().to_string()))
                .tooltip("Choose the filesystem to map")
                .disabled(scanning.is_some())
                .dropdown_menu(move |mut menu, _, _| {
                    for (mount, label) in &mounts {
                        let this = this.clone();
                        let mount = PathBuf::from(mount);
                        let checked = mount == current;
                        menu = menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
                            move |_, _, cx| {
                                this.update(cx, |this, cx| this.choose(mount.clone(), cx)).ok();
                            },
                        ));
                    }
                    menu
                });

        let status: AnyElement = match &scanning {
            Some(scan) => {
                let items = scan.progress.items.load(Ordering::Relaxed);
                let done = scan.progress.bytes.load(Ordering::Relaxed);
                let used = fs
                    .as_ref()
                    .and_then(|fs| filesystem_of(fs, &self.root))
                    .map(|f| f.used_bytes);
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        Button::new("disk-map-cancel")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(scan) = &this.scan {
                                    scan.progress.cancel();
                                }
                                cx.notify();
                            })),
                    )
                    .when_some(used.filter(|u| *u > 0), |d, used| {
                        let fraction = (done as f64 / used as f64).clamp(0., 1.);
                        d.child(
                            div()
                                .w(px(140.))
                                .h(px(5.))
                                .rounded_full()
                                .bg(t.text.opacity(0.10))
                                .child(div().h_full().rounded_full().bg(t.blue).w(relative(fraction as f32))),
                        )
                    })
                    .child(
                        div()
                            .text_size(t.caption)
                            .text_color(t.text_secondary)
                            .font_features(t.numbers.clone())
                            .child(format!(
                                "Scanning… {} items, {} · {}",
                                compact(items),
                                bytes(done as f64),
                                ago(scan.started.elapsed().as_secs() as i64).replace(" ago", "")
                            )),
                    )
                    .into_any_element()
            }
            None => div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(
                    Button::new("disk-map-rescan")
                        .ghost()
                        .small()
                        .icon(Icon::new(IconName::RefreshCw))
                        .label(if self.tree.is_some() { "Rescan" } else { "Scan" })
                        .tooltip("Scan this filesystem again")
                        .on_click(cx.listener(|this, _, _, cx| this.rescan(cx))),
                )
                .when_some(self.tree.as_ref(), |d, tree| {
                    let age = tree.scanned_at.elapsed().map_or(0, |d| d.as_secs() as i64);
                    let verb = if tree.partial { "Partial scan" } else { "Scanned" };
                    d.child(
                        div()
                            .text_size(t.caption)
                            .text_color(t.text_secondary)
                            .child(format!("{verb} {}", ago(age))),
                    )
                })
                .into_any_element(),
        };

        let modes = {
            let this = cx.entity().downgrade();
            segmented(
                "disk-map-mode",
                Mode::ALL.iter().map(|m| (*m, m.label())),
                &self.mode,
                move |mode, _, cx| {
                    this.update(cx, |this, cx| {
                        this.mode = mode;
                        cx.notify();
                    })
                    .ok();
                },
                cx,
            )
        };
        let colour = {
            let this = cx.entity().downgrade();
            let current = self.colour;
            Button::new("disk-map-colour")
                .ghost()
                .small()
                .label(match current {
                    Colour::Kind => "Kind",
                    Colour::Age => "Age",
                })
                .icon(Icon::new(IconName::ChevronsUpDown))
                .tooltip("Colour the map by what a file is, or by when it was last modified")
                .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |mut menu, _, _| {
                    for (choice, label) in [(Colour::Kind, "Kind"), (Colour::Age, "Age")] {
                        let this = this.clone();
                        menu = menu.item(PopupMenuItem::new(label).checked(choice == current).on_click(
                            move |_, _, cx| {
                                this.update(cx, |this, cx| {
                                    this.colour = choice;
                                    cx.notify();
                                })
                                .ok();
                            },
                        ));
                    }
                    menu
                })
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .px(t.page_padding)
            .py(px(8.))
            .child(volume)
            .child(status)
            .child(div().flex_1())
            .when(self.tree.is_some(), |d| {
                d.child(modes).when(self.mode == Mode::Map, |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.))
                            .child(div().text_size(t.caption).text_color(t.text_secondary).child("Colour"))
                            .child(colour),
                    )
                })
            })
    }

    fn reconciliation(&self, tree: &Tree, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let scanned = tree.node(ROOT).bytes;
        let items = tree.node(ROOT).items.saturating_sub(1);
        let fs = self.live.read(cx).latest.as_ref().and_then(|s| s.fs.clone());
        let filesystem = fs.as_ref().and_then(|fs| filesystem_of(fs, &self.root)).cloned();
        let root = self.root.display().to_string();
        let (headline, mut sub) = match &filesystem {
            Some(f) => (
                format!("{root}: {} used", bytes(f.used_bytes as f64)),
                vec![
                    format!("{} in {} items", bytes(scanned as f64), compact(items)),
                    format!("{} capacity", bytes(f.total_bytes as f64)),
                    f.fstype.clone(),
                ],
            ),
            None => (
                format!("{root}: {} scanned", bytes(scanned as f64)),
                vec![format!("{} items", compact(items))],
            ),
        };
        if tree.partial {
            sub.push("cancelled before the end".into());
        }
        let parts: Vec<(&str, u64, Hsla)> = match &filesystem {
            Some(f) => {
                let [s, u, r, free] = segments(scanned, f);
                vec![
                    ("Scanned", s, t.blue),
                    ("Unaccounted", u, t.text_secondary.opacity(0.45)),
                    ("Reserved", r, t.teal),
                    ("Free", free, t.text.opacity(0.10)),
                ]
            }
            None => vec![("Scanned", scanned, t.blue)],
        };
        let total = parts.iter().map(|p| p.1).sum::<u64>().max(1);
        let bar =
            div()
                .flex()
                .flex_row()
                .gap(px(2.))
                .h(px(12.))
                .w_full()
                .children(parts.iter().filter(|p| p.1 > 0).map(|(_, b, c)| {
                    div()
                        .h_full()
                        .min_w(px(3.))
                        .rounded(px(3.))
                        .bg(*c)
                        .w(relative(*b as f32 / total as f32))
                }));
        let legend = parts
            .iter()
            .filter(|p| p.1 > 0 || p.0 == "Scanned")
            .map(|(label, b, c)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.))
                    .child(swatch(*c, 8.))
                    .child(div().text_color(t.text).child(*label))
                    .child(
                        div()
                            .text_color(t.text_secondary)
                            .font_features(t.numbers.clone())
                            .child(bytes(*b as f64)),
                    )
                    .into_any_element()
            });
        let counts = tree.counts;
        let mut chips: Vec<AnyElement> = Vec::new();
        if tree.trashed > 0 {
            chips.push(chip(format!("In Trash {}", bytes(tree.trashed as f64)), Some(t.green), cx).into_any_element());
        }
        if let Some(f) = &filesystem
            && scanned > f.used_bytes
        {
            chips.push(
                chip(
                    format!(
                        "Scan exceeds used by {} (shared or compressed extents)",
                        bytes((scanned - f.used_bytes) as f64)
                    ),
                    None,
                    cx,
                )
                .into_any_element(),
            );
        }
        if counts.unreadable > 0 {
            chips.push(chip(format!("{} not permitted", counts.unreadable), Some(t.orange), cx).into_any_element());
        }
        if counts.other_fs > 0 {
            chips.push(chip(format!("{} skipped (other filesystem)", counts.other_fs), None, cx).into_any_element());
        }
        if counts.hard_links > 0 {
            chips.push(
                chip(
                    format!("{} hard links counted once", compact(u64::from(counts.hard_links))),
                    None,
                    cx,
                )
                .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(t.page_padding)
            .py(px(12.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .font_features(t.numbers.clone())
                            .child(headline),
                    )
                    .child(
                        div()
                            .text_size(t.caption)
                            .text_color(t.text_secondary)
                            .font_features(t.numbers.clone())
                            .child(sub.join(" · ")),
                    ),
            )
            .child(bar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(14.))
                    .text_size(t.caption)
                    .children(legend)
                    .children(chips),
            )
    }

    fn breadcrumbs(&self, tree: &Tree, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let crumbs = tree.lineage(self.zoom);
        let last = crumbs.len() - 1;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .px(px(14.))
            .py(px(6.))
            .child(
                Button::new("disk-map-back")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::ChevronLeft))
                    .tooltip("Back out one level (Escape)")
                    .disabled(self.zoom == ROOT)
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
            )
            .children(crumbs.into_iter().enumerate().flat_map(|(i, node)| {
                let name = if node == ROOT {
                    self.root.display().to_string()
                } else {
                    tree.name(node).to_string()
                };
                let sep = (i > 0).then(|| {
                    div()
                        .text_size(t.caption)
                        .text_color(t.text_tertiary)
                        .child("›")
                        .into_any_element()
                });
                let crumb = div()
                    .id(("disk-map-crumb", i))
                    .text_size(t.caption)
                    .whitespace_nowrap()
                    .when(i == last, |d| d.font_weight(FontWeight::SEMIBOLD).text_color(t.text))
                    .when(i != last, |d| {
                        d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                    })
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| this.zoom_to(node, cx)))
                    .into_any_element();
                sep.into_iter().chain([crumb])
            }))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(t.small_caption)
                    .text_color(t.text_tertiary)
                    .whitespace_nowrap()
                    .child("Double-click a folder to open it · Escape to go back"),
            )
    }

    fn map(&self, tree: &Arc<Tree>, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let legend: Vec<(SharedString, Hsla)> = match self.colour {
            Colour::Kind => Kind::ALL
                .iter()
                .map(|k| (SharedString::from(k.label()), kind_colour(*k)))
                .collect(),
            Colour::Age => AGE_BANDS
                .iter()
                .map(|(label, _, c)| (SharedString::from(*label), rgb(*c).into()))
                .collect(),
        };
        let surface = Surface {
            tree: tree.clone(),
            zoom: self.zoom,
            colour: self.colour,
            selected: self.selected,
            hover: self.hover.clone(),
            scene: self.scene.clone(),
            entity: cx.entity().downgrade(),
            focus: self.focus.clone(),
            font: gpui::font(t.font.clone()),
            now: now_s(),
        };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(self.breadcrumbs(tree, cx))
            .child(
                div().flex_1().min_h_0().px(px(14.)).pb(px(8.)).child(
                    canvas(
                        move |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                        move |bounds, hitbox, window, cx| surface.paint(bounds, hitbox, window, cx),
                    )
                    .size_full(),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_x(px(10.))
                    .gap_y(px(4.))
                    .px(px(14.))
                    .pb(px(8.))
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .children(legend.into_iter().map(|(label, c)| {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.))
                            .child(swatch(c, 9.))
                            .child(label)
                    })),
            )
    }

    fn header_cell(&self, col: Col, label: &'static str, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let t = theme(cx);
        let (current, desc) = self.sort;
        let arrow = match (current == col, desc) {
            (true, true) => " ↓",
            (true, false) => " ↑",
            _ => "",
        };
        div()
            .id(("disk-map-col", col as usize))
            .whitespace_nowrap()
            .overflow_hidden()
            .text_color(if current == col { t.text } else { t.text_secondary })
            .hover(|d| d.text_color(t.text))
            .child(format!("{label}{arrow}"))
            .on_click(cx.listener(move |this, _, _, cx| {
                let desc = if this.sort.0 == col {
                    !this.sort.1
                } else {
                    !matches!(col, Col::Name | Col::Location | Col::Kind)
                };
                this.sort = (col, desc);
                cx.notify();
            }))
    }

    fn table(&mut self, tree: &Arc<Tree>, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let mut rows: Vec<u32> = self.rows(tree).to_vec();
        let (col, desc) = self.sort;
        let parent_path = |id: u32| tree.path(tree.node(id).parent).display().to_string();
        match col {
            Col::Name => rows.sort_by_key(|id| tree.name(*id).to_lowercase()),
            Col::Size => rows.sort_by_key(|id| tree.node(*id).bytes),
            Col::Modified => rows.sort_by_key(|id| tree.node(*id).modified),
            Col::Kind => rows.sort_by_key(|id| tree.node(*id).kind.label()),
            Col::Location => rows.sort_by_cached_key(|id| parent_path(*id)),
        }
        if desc {
            rows.reverse();
        }
        let total = tree.node(ROOT).bytes;
        let count = if rows.len() >= ROW_LIMIT {
            format!("Top {}", rows.len())
        } else if rows.len() == 1 {
            "1 file".into()
        } else {
            format!("{} files", rows.len())
        };
        let controls: AnyElement = match self.mode {
            Mode::Oldest => {
                let this = cx.entity().downgrade();
                let this2 = this.clone();
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .child(segmented(
                        "disk-map-age",
                        AGE_CHOICES.iter().enumerate().map(|(i, (l, _))| (i, *l)),
                        &self.age,
                        move |i, _, cx| {
                            this.update(cx, |this, cx| {
                                this.age = i;
                                cx.notify();
                            })
                            .ok();
                        },
                        cx,
                    ))
                    .child(segmented(
                        "disk-map-size",
                        SIZE_CHOICES.iter().enumerate().map(|(i, (l, _))| (i, *l)),
                        &self.min_size,
                        move |i, _, cx| {
                            this2
                                .update(cx, |this, cx| {
                                    this.min_size = i;
                                    cx.notify();
                                })
                                .ok();
                        },
                        cx,
                    ))
                    .into_any_element()
            }
            Mode::Kinds => div()
                .text_size(t.panel_title)
                .font_weight(FontWeight::SEMIBOLD)
                .child(self.kind.map_or("", |k| k.label()))
                .into_any_element(),
            _ => div()
                .text_size(t.caption)
                .text_color(t.text_secondary)
                .child("The largest files in the scan")
                .into_any_element(),
        };
        let widths = |d: gpui::Stateful<gpui::Div>, c: Col| match c {
            Col::Name => d.flex_1().min_w(px(180.)),
            Col::Size => d.w(px(96.)).flex().justify_end(),
            Col::Modified => d.w(px(124.)).pl(px(16.)),
            Col::Kind => d.w(px(170.)),
            Col::Location => d.flex_1().min_w(px(160.)),
        };
        let header = div()
            .flex()
            .flex_row()
            .gap(px(10.))
            .px(px(16.))
            .py(px(7.))
            .text_size(t.panel_title)
            .font_weight(FontWeight::SEMIBOLD)
            .child(widths(self.header_cell(Col::Name, "Name", cx), Col::Name))
            .child(widths(self.header_cell(Col::Size, "Size", cx), Col::Size))
            .child(
                div()
                    .w(px(64.))
                    .flex()
                    .justify_end()
                    .text_color(t.text_secondary)
                    .child("Share"),
            )
            .child(widths(self.header_cell(Col::Modified, "Modified", cx), Col::Modified))
            .child(widths(self.header_cell(Col::Kind, "Kind", cx), Col::Kind))
            .child(widths(self.header_cell(Col::Location, "Location", cx), Col::Location));
        let selected = self.selected;
        let body = rows.iter().enumerate().map(|(i, &id)| {
            let n = tree.node(id);
            let cell = |id: &'static str, c: Col| widths(div().id(id), c);
            div()
                .id(("disk-map-row", id as usize))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .h(px(ROW_H))
                .px(px(16.))
                .text_size(px(ROW_TEXT))
                .when(i % 2 == 1, |d| d.bg(t.text.opacity(0.025)))
                .when(selected == Some(id), |d| d.bg(t.selection))
                .hover(|d| d.bg(t.card_hover))
                .child(
                    cell("n", Col::Name)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .overflow_hidden()
                        .child(swatch(kind_colour(n.kind), 10.))
                        .child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(tree.name(id).to_string()),
                        ),
                )
                .child(
                    cell("s", Col::Size)
                        .font_features(t.numbers.clone())
                        .child(bytes(n.bytes as f64)),
                )
                .child(
                    div()
                        .w(px(64.))
                        .flex()
                        .justify_end()
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(share(n.bytes, total)),
                )
                .child(
                    cell("m", Col::Modified)
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(if n.modified > 0 { date(n.modified) } else { "–".into() }),
                )
                .child(
                    cell("k", Col::Kind)
                        .text_color(t.text_secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(n.kind.label()),
                )
                .child(
                    cell("l", Col::Location)
                        .text_color(t.text_secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(parent_path(id)),
                )
                .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                    if e.click_count() >= 2 {
                        this.reveal(id, cx);
                    } else {
                        this.select(Some(id), cx);
                    }
                }))
        });
        let empty = rows.is_empty();
        let empty_text = match self.mode {
            Mode::Oldest => "No files that old and that large. Widen the filters to see more.",
            Mode::Kinds => "The scan found no files of this kind.",
            _ => "The scan found no files here.",
        };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(px(16.))
                    .py(px(8.))
                    .child(controls)
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(t.caption)
                            .text_color(t.text_secondary)
                            .font_features(t.numbers.clone())
                            .child(count),
                    ),
            )
            .child(hline(cx))
            .child(header)
            .child(hline(cx))
            .child(if empty {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .child(empty_text)
                    .into_any_element()
            } else {
                div()
                    .id("disk-map-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(body)
                    .into_any_element()
            })
    }

    fn kinds_view(&mut self, tree: &Arc<Tree>, cx: &Context<Self>) -> impl IntoElement {
        let totals = self.kind_totals(tree);
        if self.kind.is_none_or(|k| !totals.iter().any(|(x, _, _)| *x == k)) {
            self.kind = totals.first().map(|(k, _, _)| *k);
        }
        let t = theme(cx);
        let all = tree.node(ROOT).bytes.max(1);
        let current = self.kind;
        let list = totals.iter().map(|&(kind, b, n)| {
            let color = kind_colour(kind);
            div()
                .id(("disk-map-kind", kind as usize))
                .flex()
                .flex_col()
                .gap(px(5.))
                .px(px(12.))
                .py(px(9.))
                .rounded(px(6.))
                .when(current == Some(kind), |d| d.bg(t.selection))
                .hover(|d| d.bg(t.card_hover))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(ROW_TEXT))
                        .child(swatch(color, 10.))
                        .child(div().flex_1().child(kind.label()))
                        .child(div().font_features(t.numbers.clone()).child(bytes(b as f64))),
                )
                .child(
                    div().h(px(5.)).rounded_full().bg(t.text.opacity(0.10)).child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(color)
                            .w(relative((b as f64 / all as f64) as f32)),
                    ),
                )
                .child(
                    div()
                        .text_size(t.panel_title - px(1.))
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(format!(
                            "{} · {} {}",
                            share(b, all),
                            compact(n),
                            if n == 1 { "item" } else { "items" }
                        )),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.kind = Some(kind);
                    cx.notify();
                }))
        });
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .child(
                div()
                    .id("disk-map-kinds")
                    .w(px(300.))
                    .flex_shrink_0()
                    .overflow_y_scroll()
                    .p(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(list),
            )
            .child(div().w(t.hairline_width).bg(t.hairline))
            .child(self.table(tree, cx))
    }

    fn rail(&self, tree: &Arc<Tree>, id: u32, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        div()
            .id("disk-map-rail")
            .size_full()
            .overflow_y_scroll()
            .border_l(t.hairline_width)
            .border_color(t.hairline)
            .p(px(14.))
            .child(self.details(tree, id, cx))
    }

    fn details(&self, tree: &Arc<Tree>, id: u32, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let n = tree.node(id);
        let fold = n.is(FOLD);
        let dir = n.is(DIR);
        let path = if fold { tree.path(n.parent) } else { tree.path(id) };
        let path_text = path.display().to_string();
        let name = if id == ROOT {
            path_text.clone()
        } else {
            tree.name(id).to_string()
        };
        let kind_label = if dir { "Folder" } else { n.kind.label() };
        let colour = if dir {
            kind_colour(Kind::Folder)
        } else {
            kind_colour(n.kind)
        };
        let parent_bytes = if id == ROOT { 0 } else { tree.node(n.parent).bytes };
        let now = now_s();
        let fact = |label: &'static str, value: String| {
            div()
                .flex()
                .flex_row()
                .text_size(t.caption)
                .child(div().flex_1().text_color(t.text_secondary).child(label))
                .child(div().font_features(t.numbers.clone()).child(value))
        };
        let mut badges = Vec::new();
        for (flag, text) in [
            (FOLD, "Small files folded together"),
            (HARD_LINK, "Hard link (counted elsewhere)"),
            (SYMLINK, "Symbolic link"),
            (UNREADABLE, "Not permitted to read"),
            (OTHER_FS, "Another filesystem"),
        ] {
            if n.is(flag) {
                badges.push(chip(text, None, cx));
            }
        }
        let note = if n.is(UNREADABLE) {
            Some("Your account may not list this folder, so its contents are not counted.")
        } else if n.is(OTHER_FS) {
            Some(
                "Another filesystem is mounted here. Its contents are not part of this scan; pick it from the volume menu.",
            )
        } else {
            None
        };
        let open_path = path.clone();
        let show_path = path.clone();
        let copy_text = path_text.clone();
        let refusal = self.refusal;
        let top: Vec<u32> = if dir && !fold {
            n.children.iter().copied().take(8).collect()
        } else {
            Vec::new()
        };
        let own = n.bytes.max(1);
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(10.))
                    .child(
                        div()
                            .size(px(36.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.))
                            .bg(colour.opacity(0.18))
                            .child(
                                Icon::new(if dir { IconName::Folder } else { IconName::File })
                                    .size(px(20.))
                                    .text_color(colour),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(t.panel_title)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(t.caption)
                                    .text_color(t.text_secondary)
                                    .child(kind_label),
                            ),
                    )
                    .child(
                        Button::new("disk-map-close")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::X))
                            .tooltip("Close (Escape)")
                            .on_click(cx.listener(|this, _, _, cx| this.select(None, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(t.mono.clone())
                            .text_size(px(12.))
                            .text_color(t.text_secondary)
                            .child(path_text),
                    )
                    .child(
                        Button::new("disk-map-copy")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Copy))
                            .tooltip("Copy path")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(fact("Size", bytes(n.bytes as f64)))
                    .when(dir || fold, |d| {
                        d.child(fact("Items", compact(n.items.saturating_sub(u64::from(dir)))))
                    })
                    .child(fact("Share of scan", share(n.bytes, tree.node(ROOT).bytes)))
                    .when(id != ROOT, |d| {
                        d.child(fact("Share of parent", share(n.bytes, parent_bytes)))
                    })
                    .child(fact("Modified", modified_text(n.modified, now))),
            )
            .when(!badges.is_empty(), |d| {
                d.child(div().flex().flex_row().flex_wrap().gap(px(5.)).children(badges))
            })
            .when_some(note, |d, note| {
                d.child(div().text_size(t.caption).text_color(t.text_secondary).child(note))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(8.))
                            .when(!fold, |d| {
                                d.child(
                                    Button::new("disk-map-open")
                                        .outline()
                                        .small()
                                        .icon(Icon::new(IconName::ExternalLink))
                                        .label("Open")
                                        .tooltip("Open with the default application")
                                        .on_click(move |_, window, cx| {
                                            if let Err(e) = diskmap::open(&open_path) {
                                                window.push_notification(
                                                    Notification::error(format!("Could not run xdg-open: {e}")),
                                                    cx,
                                                );
                                            }
                                        }),
                                )
                            })
                            .child(
                                Button::new("disk-map-show")
                                    .outline()
                                    .small()
                                    .icon(Icon::new(IconName::FolderOpen))
                                    .label("Show in file manager")
                                    .on_click(move |_, _, _| diskmap::show_in_file_manager(&show_path)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(8.))
                            .when(self.mode == Mode::Map && tree.openable(id) && self.zoom != id, |d| {
                                d.child(
                                    Button::new("disk-map-zoom")
                                        .outline()
                                        .small()
                                        .icon(Icon::new(IconName::Map))
                                        .label("Open in Map")
                                        .on_click(cx.listener(move |this, _, _, cx| this.zoom_to(id, cx))),
                                )
                            })
                            .when(self.mode != Mode::Map, |d| {
                                d.child(
                                    Button::new("disk-map-reveal")
                                        .outline()
                                        .small()
                                        .icon(Icon::new(IconName::Map))
                                        .label("Show in Map")
                                        .on_click(cx.listener(move |this, _, _, cx| this.reveal(id, cx))),
                                )
                            })
                            .child(
                                Button::new("disk-map-trash")
                                    .outline()
                                    .small()
                                    .icon(Icon::new(IconName::Trash))
                                    .label("Move to Trash")
                                    .disabled(refusal.is_some())
                                    .on_click(cx.listener(move |this, _, window, cx| this.ask_trash(id, window, cx))),
                            ),
                    )
                    .when_some(refusal, |d, why| {
                        d.child(div().text_size(t.small_caption).text_color(t.text_tertiary).child(why))
                    }),
            )
            .when(!top.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_size(t.small_caption)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text_tertiary)
                                .child("TOP CONTENTS"),
                        )
                        .children(top.into_iter().map(|c| {
                            let child = tree.node(c);
                            let color = if child.is(DIR) {
                                kind_colour(Kind::Folder)
                            } else {
                                kind_colour(child.kind)
                            };
                            div()
                                .id(("disk-map-top", c as usize))
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .rounded(px(4.))
                                .hover(|d| d.bg(t.card_hover))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap(px(6.))
                                        .text_size(t.caption)
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .whitespace_nowrap()
                                                .child(tree.name(c).to_string()),
                                        )
                                        .child(
                                            div()
                                                .text_color(t.text_secondary)
                                                .font_features(t.numbers.clone())
                                                .child(bytes(child.bytes as f64)),
                                        ),
                                )
                                .child(
                                    div().h(px(3.)).rounded_full().bg(t.text.opacity(0.08)).child(
                                        div()
                                            .h_full()
                                            .rounded_full()
                                            .bg(color)
                                            .w(relative((child.bytes as f64 / own as f64) as f32)),
                                    ),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select(Some(c), cx);
                                }))
                        })),
                )
            })
    }

    fn scanning(&self, scan: &Arc<Scan>, cx: &Context<Self>) -> impl IntoElement + use<> {
        let t = theme(cx);
        let now = Instant::now();
        let progress = &scan.progress;
        let items = progress.items.load(Ordering::Relaxed);
        let found = progress.bytes.load(Ordering::Relaxed);
        let folders = progress.folders.load(Ordering::Relaxed);
        let elapsed = scan.started.elapsed();
        let rate = (elapsed.as_secs_f64() >= 1.).then(|| folders as f64 / elapsed.as_secs_f64());
        let level = progress.level();
        let tops = Rc::new(level.tops);
        let fs = self.live.read(cx).latest.as_ref().and_then(|s| s.fs.clone());
        let holding = fs
            .as_ref()
            .and_then(|fs| filesystem_holding(fs, &self.root))
            .map(|(f, m)| (f.clone(), m == self.root.as_os_str()));
        let used = holding
            .as_ref()
            .filter(|(f, whole)| *whole && f.used_bytes > 0)
            .map(|(f, _)| f.used_bytes);
        let fraction = used.map(|used| (found as f64 / used as f64).min(1.) as f32);
        {
            let mut v = self.visual.borrow_mut();
            let key = (Arc::as_ptr(scan) as usize, level.path.clone());
            if v.key != key {
                *v = ScanVisual {
                    key,
                    ..Default::default()
                };
            }
            if let Some(f) = fraction {
                v.ring.set(f, now);
            }
            v.done_at.resize(tops.len(), None);
            for (i, top) in tops.iter().enumerate() {
                if top.done && v.done_at[i].is_none() {
                    v.done_at[i] = Some(now);
                }
            }
        }

        let subtitle = holding.as_ref().map(|(f, whole)| {
            let place = if *whole {
                format!("{} · {}", f.device, f.fstype)
            } else {
                let mount = f
                    .mounts
                    .iter()
                    .filter(|m| self.root.starts_with(m))
                    .max_by_key(|m| m.len())
                    .cloned()
                    .unwrap_or_default();
                format!("On {mount} · {} · {}", f.device, f.fstype)
            };
            format!(
                "{place} · {} capacity · {} used",
                bytes(f.total_bytes as f64),
                bytes(f.used_bytes as f64)
            )
        });
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .child(
                div()
                    .size(px(48.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(12.))
                    .bg(t.blue.opacity(0.16))
                    .child(Icon::new(IconName::HardDrive).size(px(24.)).text_color(t.blue)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.blue)
                            .child(if progress.cancelled() { "STOPPING" } else { "SCANNING" }),
                    )
                    .child(
                        div()
                            .text_size(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.root.display().to_string()),
                    )
                    .when_some(subtitle, |d, sub| {
                        d.child(
                            div()
                                .text_size(px(12.))
                                .text_color(t.text_secondary)
                                .font_features(t.numbers.clone())
                                .child(sub),
                        )
                    }),
            )
            .child(
                Button::new("disk-map-cancel-scan")
                    .outline()
                    .icon(Icon::new(IconName::X))
                    .label("Cancel")
                    .tooltip("Stop scanning and map what has been found")
                    .disabled(progress.cancelled())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(scan) = &this.scan {
                            scan.progress.cancel();
                        }
                        cx.notify();
                    })),
            );

        let ring = {
            let visual = self.visual.clone();
            let started = scan.started;
            let determinate = fraction.is_some();
            let colours = (t.text.opacity(0.07), t.blue, t.teal);
            let (big, small) = match (fraction, used) {
                (Some(f), Some(used)) => (
                    format!("{:.0}%", (f * 100.).floor()),
                    format!("of {} used", bytes(used as f64)),
                ),
                _ => (bytes(found as f64), "found so far".to_string()),
            };
            div()
                .size(px(212.))
                .flex_shrink_0()
                .relative()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(2.))
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            let now = Instant::now();
                            let turn = (now.saturating_duration_since(started).as_secs_f32() * 0.55).fract();
                            let f = determinate.then(|| visual.borrow().ring.at(now));
                            paint_ring(bounds, f, turn, colours, window);
                        },
                    )
                    .absolute()
                    .size_full(),
                )
                .child(
                    div()
                        .text_size(px(if determinate { 40. } else { 28. }))
                        .font_weight(FontWeight::SEMIBOLD)
                        .font_features(t.numbers.clone())
                        .child(big),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(small),
                )
        };

        let stat = |label: &'static str, value: String| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(10.))
                .bg(t.text.opacity(0.035))
                .border_1()
                .border_color(t.hairline)
                .child(
                    div()
                        .text_size(t.small_caption)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_tertiary)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(20.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .font_features(t.numbers.clone())
                        .whitespace_nowrap()
                        .child(value),
                )
        };
        let counters = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .child(stat("ITEMS", compact(items)))
                    .child(stat("FOUND", bytes(found as f64))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .child(stat(
                        "FOLDERS / S",
                        rate.map_or("–".into(), |r| compact(r.round() as u64)),
                    ))
                    .child(stat("ELAPSED", clock(elapsed.as_secs()))),
            );

        let done = tops.iter().filter(|top| top.done).count();
        let place = level.path.display().to_string();
        let map_title = if tops.is_empty() {
            format!("Listing {}…", middle_ellipsis(&place, 60))
        } else {
            format!(
                "{done} of {} complete in {}",
                compact(tops.len() as u64),
                middle_ellipsis(&place, 60)
            )
        };
        let legend = |label: &'static str, color: Hsla| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.))
                .child(swatch(color, 9.))
                .child(label)
        };
        let partial = {
            let visual = self.visual.clone();
            let tops = tops.clone();
            let started = scan.started;
            let font = gpui::font(t.font.clone());
            div().flex_1().min_h_0().child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| paint_partial(bounds, &tops, &visual, started, &font, window, cx),
                )
                .size_full(),
            )
        };
        let map_card = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(12.))
            .rounded(px(12.))
            .bg(t.text.opacity(0.025))
            .border_1()
            .border_color(t.hairline)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(14.))
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text)
                            .font_features(t.numbers.clone())
                            .child(map_title),
                    )
                    .child(legend("Complete", t.blue))
                    .child(legend("Still counting", t.text.opacity(0.16))),
            )
            .child(partial);

        let current = progress.current();
        let path_line = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .px(px(14.))
            .py(px(9.))
            .rounded(px(8.))
            .bg(t.text.opacity(0.035))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(t.caption)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text_tertiary)
                    .child("READING"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_family(t.mono.clone())
                    .text_size(px(12.))
                    .text_color(t.text_secondary)
                    .child(if current.as_os_str().is_empty() {
                        "–".to_string()
                    } else {
                        middle_ellipsis(&current.display().to_string(), 120)
                    }),
            );

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(18.))
            .px(t.page_padding)
            .py(px(18.))
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .gap(px(18.))
                    .child(
                        div()
                            .w(px(330.))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(16.))
                            .child(ring)
                            .child(counters),
                    )
                    .child(map_card),
            )
            .child(path_line)
    }

    fn waiting(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (title, detail): (String, String) = match &self.error {
            Some(error) => ("The scan failed".into(), error.to_string()),
            None => (
                format!("Map {}", self.root.display()),
                "Scan it to see what is using the space.".into(),
            ),
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .child(
                div()
                    .text_size(t.panel_title)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(detail),
            )
    }
}

impl Render for DiskMap {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let first_scan = self.tree.is_none() && self.scan.is_some();
        let toolbar = (!first_scan).then(|| self.toolbar(cx).into_any_element());
        let body = match self.tree.clone() {
            None => match self.scan.clone() {
                Some(scan) => {
                    if !paused(cx) {
                        window.request_animation_frame();
                    }
                    self.scanning(&scan, cx).into_any_element()
                }
                None => self.waiting(cx).into_any_element(),
            },
            Some(tree) => {
                let content = match self.mode {
                    Mode::Map => self.map(&tree, cx).into_any_element(),
                    Mode::Largest | Mode::Oldest => self.table(&tree, cx).into_any_element(),
                    Mode::Kinds => self.kinds_view(&tree, cx).into_any_element(),
                };
                let rail = self
                    .shown
                    .filter(|id| (*id as usize) < tree.nodes.len() && sidebar::showing(&self.rail, cx))
                    .map(|id| self.rail(&tree, id, cx).into_any_element());
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.reconciliation(&tree, cx))
                    .child(hline(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_row()
                            .child(content)
                            .child(sidebar::sidebar(&self.rail, rail, window, cx)),
                    )
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    if this.selected.is_some() {
                        this.select(None, cx);
                    } else {
                        this.back(cx);
                    }
                }
            }))
            .children(toolbar)
            .when(!first_scan, |d| d.child(hline(cx)))
            .child(body)
    }
}

/// The treemap's canvas: lays out once per tree, zoom and size, repaints on hover.
struct Surface {
    tree: Arc<Tree>,
    zoom: u32,
    colour: Colour,
    selected: Option<u32>,
    hover: Rc<Cell<Option<usize>>>,
    scene: Rc<RefCell<Option<SceneCache>>>,
    entity: WeakEntity<DiskMap>,
    focus: FocusHandle,
    font: Font,
    now: i64,
}

fn to_bounds(r: Rect, origin: gpui::Point<Pixels>) -> Bounds<Pixels> {
    Bounds::new(point(origin.x + px(r.x), origin.y + px(r.y)), size(px(r.w), px(r.h)))
}

/// The top level of a running scan as a treemap: complete entries in colour, the rest pulsing until counted.
fn paint_partial(
    bounds: Bounds<Pixels>,
    tops: &[Top],
    visual: &RefCell<ScanVisual>,
    started: Instant,
    font: &Font,
    window: &mut Window,
    cx: &mut App,
) {
    let t = theme(cx);
    let now = Instant::now();
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let mut order: Vec<usize> = (0..tops.len()).filter(|i| tops[*i].bytes > 0).collect();
    order.sort_by_key(|i| std::cmp::Reverse(tops[*i].bytes));
    order.truncate(SCAN_TILES);
    let values: Vec<f64> = order.iter().map(|i| tops[*i].bytes as f64).collect();
    let mut target = vec![None; tops.len()];
    for (rect, i) in squarify(&values, Rect { x: 0., y: 0., w, h }).into_iter().zip(&order) {
        target[*i] = Some(rect);
    }
    let (shown, done_at) = {
        let mut v = visual.borrow_mut();
        v.glide.retarget(target, (w, h), now);
        (v.glide.at(now), v.done_at.clone())
    };
    let mut medium = font.clone();
    medium.weight = FontWeight::MEDIUM;
    let mut numbers = font.clone();
    numbers.features = t.numbers.clone();
    let (text_secondary, text_tertiary, blue) = (t.text_secondary, t.text_tertiary, t.blue);
    let line = px(TILE_LINE + 1.);
    let clock = now.saturating_duration_since(started).as_secs_f32();
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        for (i, rect) in shown.iter().enumerate() {
            let Some(r) = rect.filter(|r| r.w >= 2. && r.h >= 2.) else {
                continue;
            };
            let top = &tops[i];
            let b = to_bounds(
                Rect {
                    x: r.x + 1.5,
                    y: r.y + 1.5,
                    w: r.w - 3.,
                    h: r.h - 3.,
                },
                bounds.origin,
            );
            let radius = px(if r.w >= 24. && r.h >= 24. { 5. } else { 2. });
            let pulse = 0.5 + 0.5 * (clock * std::f32::consts::TAU / 1.8 - i as f32 * 0.45).sin();
            window.paint_quad(
                fill(b, gpui::white().opacity(0.04 + 0.04 * pulse))
                    .corner_radii(radius)
                    .border_widths(px(1.))
                    .border_color(gpui::white().opacity(0.08 + 0.05 * pulse)),
            );
            let colour = if top.dir { blue } else { kind_colour(top.kind) };
            let shade = done_at
                .get(i)
                .copied()
                .flatten()
                .map_or(0., |at| eased(at, now, DONE_FADE));
            if shade > 0. {
                window.paint_quad(
                    fill(
                        b,
                        linear_gradient(
                            160.,
                            linear_color_stop(colour.opacity(0.92 * shade), 0.),
                            linear_color_stop(colour.opacity(0.68 * shade), 1.),
                        ),
                    )
                    .corner_radii(radius),
                );
            }
            if r.w < LABEL_MIN_W + 8. || r.h < LABEL_MIN_H + 8. {
                continue;
            }
            let ink = if shade > 0.5 { label_ink(colour) } else { text_secondary };
            let max = r.w - 14.;
            let x = b.origin.x + px(7.);
            if let Some(name) = fitted(window, &medium, &top.name, px(TILE_NAME + 1.), max, ink) {
                name.paint(point(x, b.origin.y + px(6.)), line, TextAlign::Left, None, window, cx)
                    .ok();
            }
            if r.h >= 6. + 2. * (TILE_LINE + 1.) + 6. {
                let (text, size_ink) = if top.done {
                    (bytes(top.bytes as f64), ink.opacity(0.8))
                } else {
                    (format!("{}…", bytes(top.bytes as f64)), text_tertiary)
                };
                if let Some(size) = fitted(window, &numbers, &text, px(TILE_SIZE + 1.), max, size_ink) {
                    size.paint(
                        point(x, b.origin.y + px(6. + TILE_LINE + 1.)),
                        line,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
                }
            }
        }
    });
}

/// `text` cut with an ellipsis to fit `max` pixels, or `None` when not even one letter fits.
fn fitted(
    window: &mut Window,
    font: &Font,
    text: &str,
    font_size: Pixels,
    max: f32,
    color: Hsla,
) -> Option<gpui::ShapedLine> {
    let line = shape(window, font, SharedString::from(text.to_string()), font_size, color);
    if line.width <= px(max) {
        return Some(line);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = ((chars.len() as f32) * max / (line.width / px(1.))) as usize;
    for _ in 0..4 {
        keep = keep.min(chars.len().saturating_sub(1));
        if keep == 0 {
            return None;
        }
        let cut: String = chars[..keep].iter().collect::<String>() + "…";
        let line = shape(window, font, cut.into(), font_size, color);
        if line.width <= px(max) {
            return Some(line);
        }
        keep = keep.saturating_sub((keep / 8).max(1));
    }
    None
}

impl Surface {
    fn tiles(&self, bounds: Bounds<Pixels>) -> Rc<Vec<Tile>> {
        let (w, h) = (bounds.size.width / px(1.), bounds.size.height / px(1.));
        let key = (tree_key(&self.tree), self.zoom, (w as i32, h as i32));
        let mut cache = self.scene.borrow_mut();
        if let Some(c) = cache.as_ref().filter(|c| c.key == key) {
            return c.tiles.clone();
        }
        let tiles = Rc::new(scene(&self.tree, self.zoom, Rect { x: 0., y: 0., w, h }));
        *cache = Some(SceneCache {
            key,
            tiles: tiles.clone(),
        });
        tiles
    }

    fn label(&self, tile: &Tile) -> (String, String) {
        match tile.shape {
            Shape::More { count, bytes: b } => (format!("{} more", compact(u64::from(count))), bytes(b as f64)),
            _ => (
                self.tree.name(tile.node).to_string(),
                bytes(self.tree.node(tile.node).bytes as f64),
            ),
        }
    }

    fn paint(self, bounds: Bounds<Pixels>, hitbox: Hitbox, window: &mut Window, cx: &mut App) {
        let t = theme(cx);
        let (secondary, text, page) = (t.text_secondary, t.text, t.window);
        let (name_size, size_size) = (px(TILE_NAME), px(TILE_SIZE));
        let features = t.numbers.clone();
        let tiles = self.tiles(bounds);
        let origin = bounds.origin;
        let mut bold = self.font.clone();
        bold.weight = FontWeight::SEMIBOLD;
        let mut medium = self.font.clone();
        medium.weight = FontWeight::MEDIUM;
        let mut numbers = self.font.clone();
        numbers.features = features;
        let text_line = px(TILE_LINE);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for tile in tiles.iter() {
                let b = to_bounds(tile.rect, origin);
                match tile.shape {
                    Shape::Folder { strip } => {
                        window.paint_quad(
                            fill(b, gpui::white().opacity(0.07))
                                .corner_radii(px(2.))
                                .border_widths(px(1.))
                                .border_color(gpui::white().opacity(0.14)),
                        );
                        if strip && tile.rect.w > 30. {
                            let (name, size_text) = self.label(tile);
                            let size_line = shape(window, &numbers, size_text.into(), size_size, secondary);
                            let room = tile.rect.w - 10. - size_line.width / px(1.) - 6.;
                            let y = b.origin.y + px((STRIP - TILE_LINE) / 2.);
                            if let Some(name_line) = fitted(window, &bold, &name, name_size, room, text.opacity(0.88)) {
                                name_line
                                    .paint(
                                        point(b.origin.x + px(5.), y),
                                        text_line,
                                        TextAlign::Left,
                                        None,
                                        window,
                                        cx,
                                    )
                                    .ok();
                                size_line
                                    .paint(
                                        point(b.origin.x + b.size.width - px(5.) - size_line.width, y + px(0.5)),
                                        text_line,
                                        TextAlign::Left,
                                        None,
                                        window,
                                        cx,
                                    )
                                    .ok();
                            }
                        }
                    }
                    Shape::Leaf | Shape::More { .. } => {
                        let color = match tile.shape {
                            Shape::More { .. } => gpui::white().opacity(0.12),
                            _ => fill_of(&self.tree, tile.node, self.colour, self.now),
                        };
                        let inner = Bounds::new(
                            b.origin + point(px(0.5), px(0.5)),
                            size(
                                (b.size.width - px(1.)).max(px(0.)),
                                (b.size.height - px(1.)).max(px(0.)),
                            ),
                        );
                        let quad = fill(inner, color);
                        let rounded = tile.rect.w >= 24. && tile.rect.h >= 24.;
                        window.paint_quad(if rounded { quad.corner_radii(px(2.)) } else { quad });
                        let n = self.tree.node(tile.node);
                        if matches!(tile.shape, Shape::Leaf) && n.is(DIR) && tile.rect.w >= 40. && tile.rect.h >= 24. {
                            let mark = Bounds::new(
                                b.origin + point(px(2.), px(2.)),
                                size(b.size.width - px(4.), b.size.height - px(4.)),
                            );
                            window.paint_quad(
                                outline(mark, gpui::white().opacity(0.22), BorderStyle::Solid).border_widths(px(1.)),
                            );
                        }
                        if tile.rect.w >= LABEL_MIN_W && tile.rect.h >= LABEL_MIN_H {
                            let ink = label_ink(if matches!(tile.shape, Shape::More { .. }) {
                                page
                            } else {
                                color
                            });
                            let (name, size_text) = self.label(tile);
                            let max = tile.rect.w - 8.;
                            let x = b.origin.x + px(4.);
                            if let Some(line) = fitted(window, &medium, &name, name_size, max, ink) {
                                line.paint(
                                    point(x, b.origin.y + px(3.)),
                                    text_line,
                                    TextAlign::Left,
                                    None,
                                    window,
                                    cx,
                                )
                                .ok();
                            }
                            if tile.rect.h >= 3. + 2. * TILE_LINE + 2.
                                && let Some(line) =
                                    fitted(window, &numbers, &size_text, size_size, max, ink.opacity(0.8))
                            {
                                line.paint(
                                    point(x, b.origin.y + px(3. + TILE_LINE)),
                                    text_line,
                                    TextAlign::Left,
                                    None,
                                    window,
                                    cx,
                                )
                                .ok();
                            }
                        }
                    }
                }
            }
            if let Some(i) = self.hover.get().filter(|i| *i < tiles.len()) {
                let b = to_bounds(tiles[i].rect, origin);
                window.paint_quad(
                    fill(b, gpui::white().opacity(0.10))
                        .corner_radii(px(2.))
                        .border_widths(px(1.))
                        .border_color(gpui::white().opacity(0.55)),
                );
            }
            // A selection too deep to have a tile of its own marks the deepest ancestor that has one.
            let marked = self.selected.and_then(|sel| {
                self.tree.lineage(sel).into_iter().rev().find_map(|node| {
                    tiles
                        .iter()
                        .find(|t| t.node == node && !matches!(t.shape, Shape::More { .. }))
                })
            });
            if let Some(tile) = marked {
                let b = to_bounds(tile.rect, origin);
                window.paint_quad(
                    fill(b, gpui::white().opacity(0.14))
                        .corner_radii(px(3.))
                        .border_widths(px(2.))
                        .border_color(gpui::white().opacity(0.92)),
                );
            }
        });

        let local = move |p: gpui::Point<Pixels>| ((p.x - origin.x) / px(1.), (p.y - origin.y) / px(1.));
        let (hover, hover_tiles, hover_box) = (self.hover.clone(), tiles.clone(), hitbox.clone());
        window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, _| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let now = if hover_box.is_hovered(window) {
                let (x, y) = local(e.position);
                hit(&hover_tiles, x, y)
            } else {
                None
            };
            if now != hover.get() {
                hover.set(now);
                window.refresh();
            }
        });
        let (entity, focus) = (self.entity.clone(), self.focus.clone());
        window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || e.button != MouseButton::Left || !hitbox.is_hovered(window) {
                return;
            }
            focus.focus(window, cx);
            let (x, y) = local(e.position);
            if let Some(i) = hit(&tiles, x, y) {
                let tile = tiles[i];
                entity.update(cx, |this, cx| this.clicked(tile, e.click_count, cx)).ok();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;

    use super::*;
    use crate::data::diskmap::{Counts, Node};

    const EPS: f32 = 0.01;

    fn inside(inner: &Rect, outer: &Rect) -> bool {
        inner.x >= outer.x - EPS
            && inner.y >= outer.y - EPS
            && inner.x + inner.w <= outer.x + outer.w + EPS
            && inner.y + inner.h <= outer.y + outer.h + EPS
    }

    fn overlap(a: &Rect, b: &Rect) -> f32 {
        let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        w.max(0.) * h.max(0.)
    }

    #[test]
    fn squarified_tiles_have_areas_proportional_to_their_values_and_fill_the_rect() {
        let area = Rect {
            x: 10.,
            y: 20.,
            w: 600.,
            h: 400.,
        };
        let values = [500., 300., 120., 80., 60., 30., 6., 3., 1.];
        let rects = squarify(&values, area);
        let total: f64 = values.iter().sum();
        for (v, r) in values.iter().zip(&rects) {
            let expected = v / total * area.area();
            assert!(
                (r.area() - expected).abs() < expected * 1e-3 + 0.5,
                "{r:?} should cover {expected}"
            );
            assert!(inside(r, &area), "{r:?} leaves the map");
        }
        let covered: f64 = rects.iter().map(Rect::area).sum();
        assert!(
            (covered - area.area()).abs() < 1.,
            "no gaps: the tiles cover the whole map"
        );
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(overlap(a, b) < 0.5, "{a:?} and {b:?} overlap");
            }
        }
    }

    #[test]
    fn squarified_tiles_stay_close_to_square() {
        let rects = squarify(
            &[1.; 16],
            Rect {
                x: 0.,
                y: 0.,
                w: 400.,
                h: 400.,
            },
        );
        for r in rects {
            let ratio = r.w.max(r.h) / r.w.min(r.h);
            assert!(ratio < 1.5, "{r:?} is a sliver; slice-and-dice would give 16:1");
        }
    }

    #[test]
    fn squarify_survives_zero_values_and_an_empty_rect() {
        assert!(
            squarify(
                &[0., 0.],
                Rect {
                    x: 0.,
                    y: 0.,
                    w: 10.,
                    h: 10.
                }
            )
            .iter()
            .all(|r| r.area() == 0.)
        );
        assert!(squarify(&[3., 1.], Rect::default()).iter().all(|r| r.area() == 0.));
    }

    fn node(name: &str, parent: u32, bytes: u64, flags: u8) -> Node {
        Node {
            name: OsStr::new(name).into(),
            parent,
            children: Vec::new(),
            bytes,
            items: 1,
            modified: 0,
            kind: Kind::Other,
            flags,
        }
    }

    /// A root holding one big file and a folder of 10 sizeable and 490 tiny files.
    fn sample_tree() -> Tree {
        let mut nodes = vec![
            node("/r", 0, 0, DIR),
            node("big", 0, 2_000_000, 0),
            node("many", 0, 0, DIR),
        ];
        for i in 0..500 {
            nodes.push(node(&format!("f{i}"), 2, if i < 10 { 100_000 } else { 100 }, 0));
        }
        let mut tree = Tree {
            nodes,
            counts: Counts::default(),
            scanned_at: SystemTime::now(),
            took: Duration::ZERO,
            partial: false,
            trashed: 0,
        };
        let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for i in (1..tree.nodes.len()).rev() {
            let (b, p) = (tree.nodes[i].bytes, tree.nodes[i].parent);
            tree.nodes[p as usize].bytes += b;
            children.entry(p).or_default().push(i as u32);
        }
        for (p, mut c) in children {
            c.sort_by_key(|c| std::cmp::Reverse(tree.nodes[*c as usize].bytes));
            tree.nodes[p as usize].children = c;
        }
        tree
    }

    #[test]
    fn tiny_items_share_one_more_tile_and_every_tile_stays_on_the_map() {
        let tree = sample_tree();
        let area = Rect {
            x: 0.,
            y: 0.,
            w: 800.,
            h: 500.,
        };
        let tiles = scene(&tree, ROOT, area);
        assert!(tiles.iter().all(|t| inside(&t.rect, &area)));
        let more: Vec<_> = tiles.iter().filter(|t| matches!(t.shape, Shape::More { .. })).collect();
        assert_eq!(more.len(), 1, "the 490 tiny files fold into one tile");
        let Shape::More { count, bytes } = more[0].shape else {
            unreachable!()
        };
        assert_eq!(more[0].node, 2, "the tile belongs to their folder");
        assert_eq!(count, 490);
        assert_eq!(bytes, 490 * 100, "the tile carries all of their bytes");
        let named = tiles
            .iter()
            .filter(|t| t.node > 2 && matches!(t.shape, Shape::Leaf))
            .count();
        assert_eq!(named, 10, "the sizeable files keep tiles of their own");
    }

    #[test]
    fn a_zoomed_folder_fills_the_whole_map_and_its_children_nest_inside_it() {
        let tree = sample_tree();
        let area = Rect {
            x: 0.,
            y: 0.,
            w: 4000.,
            h: 3000.,
        };
        let tiles = scene(&tree, 2, area);
        let shown = tiles.iter().filter(|t| matches!(t.shape, Shape::Leaf)).count();
        assert!(shown > 100, "with room, small files get tiles of their own ({shown})");
        let covered: f64 = tiles.iter().map(|t| t.rect.area()).sum();
        assert!((covered - area.area()).abs() < 10.);
        assert_eq!(
            hit(&tiles, 1., 1.).map(|i| tiles[i].node),
            tiles.first().map(|t| t.node)
        );
    }

    #[test]
    fn the_innermost_tile_wins_a_click() {
        let tree = sample_tree();
        let tiles = scene(
            &tree,
            ROOT,
            Rect {
                x: 0.,
                y: 0.,
                w: 300.,
                h: 300.,
            },
        );
        let folder = tiles
            .iter()
            .position(|t| t.node == 2 && matches!(t.shape, Shape::Folder { .. }));
        let folder = folder.expect("the folder is big enough to open");
        let r = tiles[folder].rect;
        let i = hit(&tiles, r.x + r.w / 2., r.y + r.h / 2.).unwrap();
        assert!(i > folder, "a child drawn over the folder, not the folder itself");
    }

    #[test]
    fn age_bands_step_at_a_month_half_a_year_a_year_and_two_years() {
        let now = 1_000_000_000;
        let days = |d: i64| now - d * 86_400;
        assert_eq!(age_band(days(3), now), 0);
        assert_eq!(age_band(days(40), now), 1);
        assert_eq!(age_band(days(200), now), 2);
        assert_eq!(age_band(days(400), now), 3);
        assert_eq!(age_band(days(1000), now), 4);
        assert_eq!(age_band(0, now), 4, "an unknown date never passes for fresh");
    }

    #[test]
    fn the_bar_splits_used_space_into_scanned_and_unaccounted() {
        let fs = Filesystem {
            device: "/dev/x".into(),
            fstype: "ext4".into(),
            mounts: vec!["/home".into()],
            total_bytes: 1000,
            used_bytes: 600,
            avail_bytes: 350,
            used_pct: None,
            inodes_total: 0,
            inodes_free: 0,
        };
        assert_eq!(segments(450, &fs), [450, 150, 50, 350]);
        let [scanned, unaccounted, ..] = segments(700, &fs);
        assert_eq!(
            (scanned, unaccounted),
            (600, 0),
            "an overshoot never draws past used space"
        );
    }

    fn filesystem(device: &str, mounts: &[&str]) -> Filesystem {
        Filesystem {
            device: device.into(),
            fstype: "btrfs".into(),
            mounts: mounts.iter().map(|m| m.to_string()).collect(),
            total_bytes: 1000,
            used_bytes: 600,
            avail_bytes: 350,
            used_pct: None,
            inodes_total: 0,
            inodes_free: 0,
        }
    }

    #[test]
    fn a_folder_belongs_to_the_deepest_mount_that_contains_it() {
        // the scanning header names the volume and its size; /home on its own disk must not report the root's
        let mut fs = Fs::new();
        fs.insert("root".into(), filesystem("/dev/a", &["/"]));
        fs.insert("home".into(), filesystem("/dev/b", &["/home"]));
        let device = |p: &str| filesystem_holding(&fs, Path::new(p)).map(|(f, m)| (f.device.clone(), m.to_string()));
        assert_eq!(device("/home"), Some(("/dev/b".into(), "/home".into())));
        assert_eq!(device("/home/me/src"), Some(("/dev/b".into(), "/home".into())));
        assert_eq!(device("/usr"), Some(("/dev/a".into(), "/".into())));
        assert_eq!(
            device("/homework"),
            Some(("/dev/a".into(), "/".into())),
            "a shared prefix is not a parent"
        );
    }

    #[test]
    fn a_long_path_keeps_both_ends() {
        let path = "/home/me/.cache/some/very/deep/folder/file.bin";
        let cut = middle_ellipsis(path, 21);
        assert_eq!(cut.chars().count(), 21);
        assert!(cut.starts_with("/home/me/") && cut.ends_with("file.bin") && cut.contains('…'));
        assert_eq!(middle_ellipsis("/usr", 21), "/usr");
    }

    #[test]
    fn elapsed_time_reads_as_a_clock() {
        assert_eq!(clock(7), "0:07");
        assert_eq!(clock(125), "2:05");
        assert_eq!(clock(3 * 3600 + 61), "3:01:01");
    }

    fn rect(x: f32, w: f32) -> Option<Rect> {
        Some(Rect { x, y: 0., w, h: 10. })
    }

    #[test]
    fn scanning_tiles_glide_to_a_new_layout_by_elapsed_time() {
        // motion follows the clock, not the frame count, so a slow frame never slows the animation
        let start = Instant::now();
        let mut g = Glide::default();
        g.retarget(vec![rect(0., 100.)], (100., 10.), start);
        let grown = g.at(start + GLIDE)[0].unwrap();
        assert_eq!((grown.x, grown.w), (0., 100.), "a new tile grows into place");
        g.retarget(vec![rect(0., 50.)], (100., 10.), start + GLIDE);
        assert_eq!(g.at(start + GLIDE)[0].unwrap().w, 100.);
        let mid = g.at(start + GLIDE + GLIDE / 2)[0].unwrap().w;
        assert!(mid < 100. && mid > 50.);
        assert!(100. - mid > 25., "ease-out covers most of the way in the first half");
        assert_eq!(g.at(start + GLIDE * 3)[0].unwrap().w, 50.);
    }

    #[test]
    fn a_resize_snaps_the_scanning_tiles() {
        // a tile caught mid-glide from the old size would hang past the new edge
        let start = Instant::now();
        let mut g = Glide::default();
        g.retarget(vec![rect(0., 100.)], (100., 10.), start);
        g.retarget(vec![rect(0., 80.)], (80., 10.), start + GLIDE * 2);
        assert_eq!(g.at(start + GLIDE * 2)[0].unwrap().w, 80.);
    }

    #[test]
    fn the_ring_eases_from_where_it_is_shown() {
        let start = Instant::now();
        let mut e = Eased::default();
        e.set(0.4, start);
        let half = e.at(start + RING_EASE / 2);
        e.set(0.6, start + RING_EASE / 2);
        assert_eq!(
            e.at(start + RING_EASE / 2),
            half,
            "a new target never makes the ring jump"
        );
        assert_eq!(e.at(start + RING_EASE * 2), 0.6);
    }
}
