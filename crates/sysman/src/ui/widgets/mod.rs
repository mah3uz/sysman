pub mod card;
pub mod legend;
pub mod panel;
pub mod range;

use gpui::{Pixels, Styled};

/// gpui's border helpers take whole pixels; the reference draws 0.5 px hairlines.
pub fn hairline<E: Styled>(mut e: E, width: Pixels) -> E {
    let w = Some(width.into());
    let edges = &mut e.style().border_widths;
    (edges.top, edges.right, edges.bottom, edges.left) = (w, w, w, w);
    e
}
