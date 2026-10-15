pub mod app_icon;
pub mod card;
pub mod detail_sheet;
pub mod hint;
pub mod legend;
pub mod panel;
pub mod range;
pub mod segmented;
pub mod sidebar;
pub mod text;

use gpui::{Pixels, Styled};

/// gpui's border helpers take whole pixels; hairlines are 0.5 px.
pub fn hairline<E: Styled>(mut e: E, width: Pixels) -> E {
    let w = Some(width.into());
    let edges = &mut e.style().border_widths;
    (edges.top, edges.right, edges.bottom, edges.left) = (w, w, w, w);
    e
}
