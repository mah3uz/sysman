use std::rc::Rc;

use gpui::{
    App, Div, ElementId, InteractiveElement, ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled,
    Window, div, prelude::FluentBuilder, px,
};

use crate::ui::theme::theme;

/// A segmented switch in the range picker's style, the selected segment filled in the accent colour.
pub fn segmented<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (T, impl Into<SharedString>)>,
    current: &T,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let t = theme(cx);
    let id: ElementId = id.into();
    let on_select = Rc::new(on_select);
    div()
        .id(id.clone())
        .flex()
        .flex_row()
        .p(px(2.))
        .rounded(px(6.))
        .bg(t.card)
        .text_size(t.caption)
        .children(options.into_iter().enumerate().map(|(i, (value, label))| {
            let on_select = on_select.clone();
            let selected = value == *current;
            div()
                .id(ElementId::NamedInteger(format!("{id}").into(), i as u64))
                .h(px(20.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .when(selected, |d| d.bg(t.blue).text_color(gpui::white()))
                .when(!selected, |d| {
                    d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                })
                .child(label.into())
                .on_click(move |_, window, cx| on_select(value.clone(), window, cx))
        }))
}
