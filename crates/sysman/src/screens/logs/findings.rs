use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use journal::findings::FindingKind;

use super::inspector::heading;
use super::toolbar::{hair_b, severity_colour};
use super::{Logs, model, recorded};
use crate::ui::theme::{rem_px, theme};
use crate::ui::widgets::hairline;

const CARD_MIN_WIDTH: f32 = 250.;
const ROWS: usize = 2;

pub(super) fn kind_icon(kind: FindingKind) -> IconName {
    match kind {
        FindingKind::Crash => IconName::Skull,
        FindingKind::OomKill => IconName::MemoryStick,
        FindingKind::FailedUnit => IconName::CircleX,
        FindingKind::RestartLoop => IconName::RefreshCw,
        FindingKind::BootWithoutShutdown => IconName::Zap,
        FindingKind::ThermalThrottle => IconName::Thermometer,
        FindingKind::HardwareError => IconName::CircuitBoard,
        FindingKind::FailedSignIns => IconName::Lock,
        FindingKind::PackageChange => IconName::Blocks,
    }
}

impl Logs {
    /// "Worth a look": the scope's findings as cards, most severe first, in at most two rows unless all are asked
    /// for. Shown once the scan has ended, as the rules need the whole stream.
    pub(super) fn findings(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.view.scanned || super::scan_failed(&self.scan.state()).is_some() {
            return None;
        }
        let t = theme(cx);
        let cards = &self.view.cards;
        let width = window.viewport_size().width - self.left.read(cx).width() - self.right.read(cx).width();
        let rem = f32::from(window.rem_size()) / 16.;
        let cols = ((f32::from(width) - 28.) / (CARD_MIN_WIDTH * rem))
            .floor()
            .clamp(1., 4.) as usize;
        let fits = cols * ROWS;
        let shown = if self.findings_all {
            cards.len()
        } else {
            cards.len().min(fits)
        };
        let collapsed = self.findings_collapsed;
        let caption = match cards.len() {
            0 => format!("Nothing stands out in {}", self.scope_name()),
            1 => format!("1 finding in {}", self.scope_name()),
            n => format!("{n} findings in {}", self.scope_name()),
        };
        let more = (cards.len() > fits && !collapsed).then(|| {
            div()
                .id("logs-findings-more")
                .text_size(t.caption)
                .text_color(t.blue)
                .cursor_pointer()
                .hover(|d| d.underline())
                .child(if self.findings_all {
                    "Show fewer".to_string()
                } else {
                    format!("Show all {}", cards.len())
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.findings_all = !this.findings_all;
                    cx.notify();
                }))
        });
        let heading = div()
            .id("logs-findings-heading")
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(7.))
            .cursor_pointer()
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(t.text_tertiary),
            )
            .child(Icon::new(IconName::Sparkles).small().text_color(t.text_secondary))
            .child(
                div()
                    .text_size(t.panel_title)
                    .font_weight(t.weight_semibold)
                    .child("Worth a look"),
            )
            .child(
                div()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(caption),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.set_findings_collapsed(!collapsed, cx)));
        let grid = (!collapsed && !cards.is_empty()).then(|| {
            div()
                .grid()
                .grid_cols(cols as u16)
                .gap(rem_px(8.))
                .children(cards[..shown].iter().map(|card| {
                    let colour = severity_colour(card.severity, t);
                    let selected = self.controls.finding == Some(card.index);
                    let index = card.index;
                    div()
                        .id(("logs-finding", card.index))
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(rem_px(9.))
                        .px(rem_px(10.))
                        .py(rem_px(8.))
                        .rounded(t.card_radius)
                        .cursor_pointer()
                        .map(|d| {
                            if selected {
                                d.bg(colour.opacity(0.10)).border_1().border_color(colour.opacity(0.55))
                            } else {
                                hairline(d, t.hairline_width)
                                    .bg(t.panel)
                                    .border_color(t.hairline)
                                    .hover(|d| d.bg(t.card_hover))
                            }
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let next = if this.controls.finding == Some(index) {
                                None
                            } else {
                                Some(index)
                            };
                            this.set_finding(next, cx);
                        }))
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(rem_px(22.))
                                .rounded(rem_px(6.))
                                .bg(colour.opacity(if selected { 1. } else { 0.14 }))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(Icon::new(kind_icon(card.kind)).xsmall().text_color(if selected {
                                    gpui::white()
                                } else {
                                    colour
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(1.))
                                .child(
                                    div()
                                        .text_size(t.caption)
                                        .font_weight(t.weight_semibold)
                                        .truncate()
                                        .child(card.title.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(t.small_caption)
                                        .text_color(t.text_secondary)
                                        .truncate()
                                        .child(card.evidence.clone()),
                                ),
                        )
                        .when(card.count > 1, |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(t.caption)
                                    .font_weight(t.weight_semibold)
                                    .font_features(t.numbers.clone())
                                    .text_color(colour)
                                    .child(format!("×{}", card.count)),
                            )
                        })
                }))
        });
        Some(
            hair_b(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(rem_px(8.))
                    .px(rem_px(14.))
                    .pt(rem_px(4.))
                    .pb(rem_px(if collapsed || cards.is_empty() { 8. } else { 12. })),
                t,
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(heading)
                    .child(div().flex_1())
                    .children(more),
            )
            .children(grid)
            .into_any_element(),
        )
    }

    /// Above the list while a finding narrows it: what it is, when, and what sysman recorded over its span.
    pub(super) fn finding_header(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let index = self.controls.finding?;
        let card = self.view.cards.iter().find(|c| c.index == index)?;
        let colour = severity_colour(card.severity, t);
        let pattern = if model::same_day(card.first_us, card.last_us, &self.tz) {
            "%H:%M:%S"
        } else {
            "%b %-d %H:%M"
        };
        let when = if card.last_us > card.first_us {
            format!(
                "{} – {}",
                model::clock(card.first_us, pattern, &self.tz),
                model::clock(card.last_us, pattern, &self.tz)
            )
        } else {
            model::clock(card.first_us, "%b %-d %H:%M:%S", &self.tz)
        };
        let entries = match card.count {
            1 => "1 entry".to_string(),
            n => format!("{n} entries"),
        };
        let recorded = self.finding_rec.as_ref().map(|r| {
            div()
                .flex()
                .flex_col()
                .gap(rem_px(6.))
                .child(heading("What sysman recorded", t))
                .child(recorded::charts("logs-finding-chart", r, true, 28., t))
        });
        Some(
            hair_b(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(rem_px(10.))
                    .px(rem_px(14.))
                    .pt(rem_px(10.))
                    .pb(rem_px(12.)),
                t,
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(rem_px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(rem_px(7.))
                                    .child(Icon::new(kind_icon(card.kind)).small().text_color(colour))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(t.panel_title)
                                            .font_weight(t.weight_semibold)
                                            .child(card.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(t.caption)
                                            .text_color(t.text_secondary)
                                            .font_features(t.numbers.clone())
                                            .child(format!("· {entries} · {when}")),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(t.caption)
                                    .text_color(t.text_secondary)
                                    .child(card.evidence.clone()),
                            ),
                    )
                    .child(recorded::open_in_explorer(
                        "logs-finding-explorer",
                        (card.last_us / 1_000_000) as i64,
                        self,
                    ))
                    .child(
                        Button::new("logs-finding-close")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::X))
                            .tooltip("Show all entries")
                            .on_click(cx.listener(|this, _, _, cx| this.set_finding(None, cx))),
                    ),
            )
            .children(recorded)
            .into_any_element(),
        )
    }
}
