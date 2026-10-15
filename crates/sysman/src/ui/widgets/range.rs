use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    App, Global, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};

use crate::ui::shell::Tab;
use crate::ui::theme::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    M5,
    M30,
    H1,
    H6,
    H24,
    D7,
}

impl Range {
    pub const ALL: [Range; 6] = [Self::M5, Self::M30, Self::H1, Self::H6, Self::H24, Self::D7];

    pub fn label(self) -> &'static str {
        match self {
            Self::M5 => "5 min",
            Self::M30 => "30 min",
            Self::H1 => "1 hr",
            Self::H6 => "6 hr",
            Self::H24 => "24 hr",
            Self::D7 => "7 day",
        }
    }

    pub fn span_s(self) -> i64 {
        match self {
            Self::M5 => 300,
            Self::M30 => 1800,
            Self::H1 => 3600,
            Self::H6 => 6 * 3600,
            Self::H24 => 86400,
            Self::D7 => 7 * 86400,
        }
    }
}

/// Each tab's last range, kept for the session because tabs are unmounted when hidden.
#[derive(Default)]
struct Remembered(HashMap<Tab, Range>);

impl Global for Remembered {}

pub fn remembered(tab: Tab, cx: &App) -> Range {
    cx.try_global::<Remembered>()
        .and_then(|r| r.0.get(&tab).copied())
        .unwrap_or(Range::M30)
}

pub fn remember(tab: Tab, range: Range, cx: &mut App) {
    cx.default_global::<Remembered>().0.insert(tab, range);
}

pub fn range_picker(
    current: Range,
    on_select: impl Fn(Range, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let t = theme(cx);
    let on_select = Rc::new(on_select);
    div()
        .flex()
        .flex_row()
        .p(px(2.))
        .rounded(px(6.))
        .bg(t.card)
        .text_size(t.caption)
        .children(Range::ALL.into_iter().map(|range| {
            let on_select = on_select.clone();
            let selected = range == current;
            div()
                .id(range.label())
                .h(px(20.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .when(selected, |d| d.bg(t.card_hover).text_color(t.text))
                .when(!selected, |d| {
                    d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                })
                .child(range.label())
                .on_click(move |_, window, cx| on_select(range, window, cx))
        }))
}
