use gpui::{
    AnyElement, Context, Div, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, div,
    prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::Input;
use gpui_component::{Disableable, Icon, Selectable, Sizable};
use gpui_kit_assets::IconName;
use journal::findings::{FindingKind, Severity};
use journal::reader::{Access, Backend};

use super::{Logs, model};
use crate::ui::theme::{Theme, rem_px, theme};
use crate::ui::widgets::segmented::segmented;

const TOOLBAR_HEIGHT: f32 = 44.;
const STATUS_HEIGHT: f32 = 26.;
const JOIN_GROUP: &str = "sudo usermod -aG systemd-journal $USER";

pub(super) fn hair_b<E: Styled>(mut e: E, t: &Theme) -> E {
    e.style().border_widths.bottom = Some(t.hairline_width.into());
    e.border_color(t.hairline)
}

fn hair_t<E: Styled>(mut e: E, t: &Theme) -> E {
    e.style().border_widths.top = Some(t.hairline_width.into());
    e.border_color(t.hairline)
}

pub(super) fn dot(colour: gpui::Hsla, size: f32) -> Div {
    div().flex_shrink_0().size(rem_px(size)).rounded_full().bg(colour)
}

pub(super) fn severity_colour(s: Severity, t: &Theme) -> gpui::Hsla {
    match s {
        Severity::High => t.red,
        Severity::Medium => t.orange,
        Severity::Low => t.blue,
    }
}

impl Logs {
    pub(super) fn toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let sources = !self.left.read(cx).collapsed();
        let (selected, open) = (self.selected.is_some(), self.inspector_open());
        let follows = self.span.reaches_now();
        let tool = |id: &'static str, icon: IconName| Button::new(id).small().icon(Icon::new(icon));
        let title = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(7.))
            .mr(rem_px(6.))
            .text_size(t.panel_title)
            .font_weight(t.weight_semibold)
            .child(Icon::new(IconName::FileText).size_4().text_color(t.text_secondary))
            .child("Logs");
        hair_b(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .h(rem_px(TOOLBAR_HEIGHT))
                .px(rem_px(14.))
                .gap(rem_px(8.)),
            t,
        )
        .child(
            tool("logs-sources", IconName::PanelLeft)
                .selected(sources)
                .tooltip(if sources { "Hide sidebar" } else { "Show sidebar" })
                .on_click(cx.listener(|this, _, _, cx| this.left.update(cx, |s, cx| s.toggle(cx)))),
        )
        .child(title)
        .child(self.search_field(cx).flex_1().min_w(rem_px(180.)).max_w(rem_px(520.)))
        .child(segmented(
            "logs-level",
            model::Level::ALL,
            &self.controls.level,
            {
                let this = cx.entity().downgrade();
                move |level, _, cx| {
                    this.update(cx, |this, cx| this.set_controls(|c| c.level = level, cx))
                        .ok();
                }
            },
            cx,
        ))
        .child(
            Button::new("logs-follow")
                .ghost()
                .small()
                .icon(Icon::new(if self.following() {
                    IconName::Pause
                } else {
                    IconName::Play
                }))
                .label(if self.following() { "Following" } else { "Follow" })
                .selected(self.following())
                .disabled(!follows || self.follow_capped())
                .tooltip(match (follows, self.following()) {
                    (false, _) => "This boot has ended; following needs a range that reaches now",
                    _ if self.follow_capped() => {
                        "Stopped: the list holds as many entries as it keeps; narrow the range"
                    }
                    (true, true) => "Stop adding new entries as they arrive",
                    (true, false) => "Add new entries as they arrive",
                })
                .on_click(cx.listener(|this, _, _, cx| this.set_follow(!this.follow, cx))),
        )
        .child(div().flex_1())
        .child(
            tool(
                "logs-inspector",
                if open {
                    IconName::PanelRightClose
                } else {
                    IconName::PanelRightOpen
                },
            )
            .selected(open)
            .disabled(!selected)
            .tooltip(match (open, selected) {
                (true, _) => "Hide details",
                (false, true) => "Show details of the selected entry",
                (false, false) => "Select an entry to see its details",
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector(cx))),
        )
    }

    /// The field, with the finding the list is narrowed to as a chip before the text.
    fn search_field(&self, cx: &Context<Self>) -> Div {
        let t = theme(cx);
        let finding = self.shown_card().map(|card| {
            let colour = severity_colour(card.severity, t);
            div()
                .id("logs-finding-chip")
                .flex_shrink_0()
                .max_w(rem_px(220.))
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(4.))
                .h(rem_px(20.))
                .pl(rem_px(6.))
                .pr(rem_px(3.))
                .rounded(rem_px(5.))
                .bg(colour.opacity(0.14))
                .text_size(t.small_caption)
                .child(dot(colour, 6.))
                .child(div().min_w_0().truncate().child(card.title.clone()))
                .child(
                    div()
                        .id("logs-finding-clear")
                        .flex_shrink_0()
                        .p(rem_px(2.))
                        .rounded(rem_px(3.))
                        .text_color(t.text_secondary)
                        .hover(|d| d.bg(colour.opacity(0.2)).text_color(t.text))
                        .cursor_pointer()
                        .child(Icon::new(IconName::X).xsmall())
                        .tooltip(|window, cx| {
                            gpui_component::tooltip::Tooltip::new("Show every entry again").build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.set_finding(None, cx))),
                )
        });
        div()
            .h(rem_px(30.))
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(4.))
            .pl(rem_px(9.))
            .pr(rem_px(2.))
            .rounded(rem_px(8.))
            .bg(if t.mode.is_dark() { t.card } else { t.panel })
            .border_1()
            .border_color(t.control_border)
            .child(Icon::new(IconName::Search).small().text_color(t.text_secondary))
            .children(finding)
            .child(
                div().flex_1().min_w_0().children(
                    self.input
                        .as_ref()
                        .map(|s| Input::new(s).appearance(false).small().cleanable(true)),
                ),
            )
    }

    /// The access banner, or the note that no journal reader works, and the note that the list holds only the
    /// newest entries.
    pub(super) fn banner(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let capped = self.scan.index().dropped() > 0;
        let line = |icon: IconName, text: String| {
            hair_b(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_shrink_0()
                    .gap(rem_px(8.))
                    .px(rem_px(14.))
                    .py(rem_px(6.))
                    .bg(t.orange.opacity(0.08))
                    .text_size(t.caption),
                t,
            )
            .child(Icon::new(icon).small().text_color(t.orange).flex_shrink_0())
            .child(div().min_w_0().child(text))
        };
        let cap = capped.then(|| {
            line(
                IconName::Info,
                format!(
                    "Showing the latest {} entries — narrow the range to see older ones.",
                    model::thousands(self.scan.row_cap() as u64)
                ),
            )
        });
        let status = self.status;
        let own_only = status.is_some_and(|s| s.access == Access::OwnOnly);
        let (icon, text) = match status.map(|s| (s.backend, own_only)) {
            Some((None, _)) => (
                IconName::TriangleAlert,
                "The journal cannot be read: neither libsystemd nor journalctl is available.",
            ),
            Some((Some(_), true)) => (
                IconName::Lock,
                "Showing only your own entries; how boots ended needs the system journal. To read it, join the \
                 systemd-journal group and sign in again:",
            ),
            _ => return cap.map(IntoElement::into_any_element),
        };
        let access = line(icon, text.into()).when(own_only, |d| {
            d.child(
                div()
                    .flex_shrink_0()
                    .px(rem_px(6.))
                    .py(rem_px(1.))
                    .rounded(rem_px(4.))
                    .bg(t.card)
                    .font_family(t.mono.clone())
                    .text_size(t.small_caption)
                    .child(JOIN_GROUP),
            )
            .child(
                Button::new("logs-copy-usermod")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Copy))
                    .tooltip("Copy the command")
                    .on_click(|_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(JOIN_GROUP.into()))),
            )
        });
        Some(div().flex().flex_col().child(access).children(cap).into_any_element())
    }

    pub(super) fn status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let state = self.scan.state();
        let (colour, summary) = if state.is_running() {
            (t.blue, "Reading the journal".to_string())
        } else if let Some(e) = super::scan_failed(&state) {
            (t.red, format!("Could not read the journal: {e}"))
        } else if self.following() && state.is_following() {
            (t.green, format!("Following {}", self.scope_name()))
        } else if self.follow_capped() {
            (
                t.orange,
                format!(
                    "Stopped following at {} entries",
                    model::thousands(self.scan.row_cap() as u64)
                ),
            )
        } else {
            (t.text_tertiary, format!("Paused on {}", self.scope_name()))
        };
        let rule = |colour, text: String| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(5.))
                .child(dot(colour, 6.))
                .child(text)
        };
        let tallies = self.view.tallies.iter().take(5).map(|&(kind, n, units)| {
            let colour = severity_colour(kind.severity(), t);
            let colour = if kind == FindingKind::PackageChange {
                t.text_tertiary
            } else {
                colour
            };
            rule(colour, model::tally_label(kind, n, units))
        });
        let backend = match (self.scan.backend(), self.status.and_then(|s| s.backend)) {
            (Some(Backend::Journalctl), _) | (None, Some(Backend::Journalctl)) => "Journal through journalctl",
            _ => "Journal",
        };
        let mut source = match self.view.packages {
            Some(p) => format!("{backend} and {p}"),
            None => backend.to_string(),
        };
        let took = self.scan.elapsed();
        if !took.is_zero() {
            source.push_str(&format!(" · read {:.2} s", took.as_secs_f64()));
        }
        hair_t(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap(rem_px(14.))
                .h(rem_px(STATUS_HEIGHT))
                .px(rem_px(14.))
                .text_size(t.small_caption)
                .text_color(t.text_secondary)
                .font_features(t.numbers.clone()),
            t,
        )
        .child(rule(colour, summary).min_w_0().truncate())
        .children(tallies)
        .child(div().flex_1())
        .child(div().flex_shrink_0().child(source))
    }
}
