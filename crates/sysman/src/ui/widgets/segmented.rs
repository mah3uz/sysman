use std::rc::Rc;

use gpui::{
    App, Div, ElementId, InteractiveElement, ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled,
    Window, div, prelude::FluentBuilder, px,
};

use crate::ui::theme::{rem_px, theme};

/// A segmented switch in the range picker's style, the selected segment filled in the accent colour.
pub fn segmented<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (T, impl Into<SharedString>)>,
    current: &T,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    build(id.into(), options, current, Rc::new(on_select), false, cx)
}

/// [`segmented`] stretched to its container's width, the segments sharing it equally.
pub fn segmented_full_width<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (T, impl Into<SharedString>)>,
    current: &T,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    build(id.into(), options, current, Rc::new(on_select), true, cx)
}

type OnSelect<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

fn build<T: Clone + PartialEq + 'static>(
    id: ElementId,
    options: impl IntoIterator<Item = (T, impl Into<SharedString>)>,
    current: &T,
    on_select: OnSelect<T>,
    full_width: bool,
    cx: &App,
) -> Stateful<Div> {
    let t = theme(cx);
    div()
        .id(id.clone())
        .flex()
        .flex_row()
        .p(px(2.))
        .rounded(rem_px(6.))
        .bg(t.card)
        .text_size(t.caption)
        .when(full_width, |d| d.w_full())
        .children(options.into_iter().enumerate().map(|(i, (value, label))| {
            let on_select = on_select.clone();
            let selected = value == *current;
            div()
                .id(ElementId::NamedInteger(format!("{id}").into(), i as u64))
                .h(rem_px(20.))
                .px(rem_px(8.))
                .flex()
                .items_center()
                // a zero basis makes every segment the same width whatever its label
                .when(full_width, |d| d.flex_1().flex_basis(px(0.)).min_w_0().justify_center())
                .rounded(rem_px(4.))
                .when(selected, |d| d.bg(t.blue).text_color(gpui::white()))
                .when(!selected, |d| {
                    d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                })
                .child(label.into())
                .on_click(move |_, window, cx| on_select(value.clone(), window, cx))
        }))
}
