use gpui::{
    AnyElement, App, AppContext, Context, ElementId, Entity, Focusable, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_component::{Disableable, Icon, Sizable};
use gpui_kit_assets::IconName;
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};

use super::Explorer;
use crate::charts::trend::clock;
use crate::ui::theme::{rem_px, theme};

const CELL_W: f32 = 32.;
const CELL_H: f32 = 26.;

/// The go-to-time picker: a day, a typed time, and the month on show.
pub(super) struct Goto {
    pub input: Entity<InputState>,
    day: Date,
    month: Date,
    /// `None` while the typed text is not a time.
    time: Option<Time>,
}

impl Goto {
    pub fn new(window: &mut Window, cx: &mut Context<Explorer>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("HH:MM"));
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                if let Some(goto) = &mut this.goto {
                    goto.time = parse_time(&input.read(cx).value());
                    cx.notify();
                }
            }
            InputEvent::PressEnter { .. } => this.go_to(cx),
            _ => {}
        })
        .detach();
        let today = Zoned::now().date();
        Self {
            input,
            day: today,
            month: today.first_of_month(),
            time: None,
        }
    }

    fn set(&mut self, at: DateTime, window: &mut Window, cx: &mut App) {
        self.day = at.date();
        self.month = at.date().first_of_month();
        self.time = Some(at.time());
        self.input
            .update(cx, |s, cx| s.set_value(time_text(at.time()), window, cx));
    }

    fn at(&self) -> Option<DateTime> {
        Some(self.day.to_datetime(self.time?))
    }
}

/// `H:MM` or `HH:MM`, 24-hour.
pub fn parse_time(text: &str) -> Option<Time> {
    let (h, m) = text.trim().split_once(':')?;
    let digits = |s: &str, max: usize| !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(h, 2) || !digits(m, 2) || m.len() != 2 {
        return None;
    }
    Time::new(h.parse().ok()?, m.parse().ok()?, 0, 0).ok()
}

pub fn time_text(t: Time) -> String {
    format!("{:02}:{:02}", t.hour(), t.minute())
}

/// Minutes later (or earlier), carrying into the next or previous day.
pub fn step(at: DateTime, minutes: i64) -> DateTime {
    at.checked_add(minutes.minutes()).unwrap_or(at)
}

pub fn local(t: f64, tz: &TimeZone) -> DateTime {
    Timestamp::from_second(t as i64)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_zoned(tz.clone())
        .datetime()
}

pub fn timestamp(at: DateTime, tz: &TimeZone) -> Option<f64> {
    at.to_zoned(tz.clone()).ok().map(|z| z.timestamp().as_second() as f64)
}

/// Into recorded history: no earlier than its oldest reading and no later than now.
pub fn clamp(t: f64, oldest: Option<f64>, now: f64) -> f64 {
    oldest.map_or(t, |o| t.max(o)).min(now)
}

/// The days a month page shows: six Monday-first weeks covering the month.
pub fn month_grid(month: Date) -> Vec<Date> {
    let first = month.first_of_month();
    let back = i64::from(first.weekday().to_monday_zero_offset());
    let start = first.checked_sub(back.days()).unwrap_or(first);
    (0..42).filter_map(|i: i64| start.checked_add(i.days()).ok()).collect()
}

/// The quick choices, each `None` where its time is unknown.
pub fn chips(now: f64, boot: Option<f64>, tz: &TimeZone) -> [(&'static str, Option<f64>); 4] {
    let zoned = Timestamp::from_second(now as i64)
        .map(|ts| ts.to_zoned(tz.clone()))
        .ok();
    let seconds = |z: Zoned| z.timestamp().as_second() as f64;
    [
        ("1 hour ago", Some(now - 3600.)),
        (
            "Yesterday this time",
            zoned.as_ref().and_then(|z| z.checked_sub(1.day()).ok()).map(seconds),
        ),
        (
            "Start of today",
            zoned.as_ref().and_then(|z| z.start_of_day().ok()).map(seconds),
        ),
        ("Last boot", boot),
    ]
}

impl Explorer {
    pub(super) fn open_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let at = local(self.inspected_time(now), &TimeZone::system());
        if let Some(goto) = &mut self.goto {
            goto.set(at, window, cx);
        }
        self.goto_open = true;
        cx.notify();
    }

    pub(super) fn close_goto(&mut self, cx: &mut Context<Self>) {
        self.goto_open = false;
        cx.notify();
    }

    /// Centres the current span on the chosen time and leaves live.
    pub(super) fn go_to(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self
            .goto
            .as_ref()
            .and_then(Goto::at)
            .and_then(|at| timestamp(at, &TimeZone::system()))
        else {
            return;
        };
        let now = self.now(cx);
        let t = clamp(t, self.oldest, now);
        self.goto_open = false;
        self.view.centre_on(t, now);
        self.pin_at(t, cx);
    }

    fn goto_set(&mut self, at: DateTime, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(goto) = &mut self.goto {
            goto.set(at, window, cx);
            goto.input.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn goto_step(&mut self, minutes: i64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(at) = self.goto.as_ref().and_then(Goto::at) {
            self.goto_set(step(at, minutes), window, cx);
        }
    }

    fn goto_day(&mut self, day: Date, window: &mut Window, cx: &mut Context<Self>) {
        let Some(goto) = &mut self.goto else { return };
        let time = goto.time.unwrap_or(Time::midnight());
        self.goto_set(day.to_datetime(time), window, cx);
    }

    fn goto_month(&mut self, months: i32, cx: &mut Context<Self>) {
        if let Some(goto) = &mut self.goto {
            goto.month = goto.month.checked_add(months.months()).unwrap_or(goto.month);
            cx.notify();
        }
    }

    /// The popover's body; `me` routes its clicks back here.
    pub(super) fn goto_panel(&self, me: WeakEntity<Self>, cx: &App) -> AnyElement {
        let Some(goto) = &self.goto else {
            return div().into_any_element();
        };
        let t = theme(cx);
        let tz = TimeZone::system();
        let now = self.now(cx);
        let today = local(now, &tz).date();
        let first = self.oldest.map(|o| local(o, &tz).date());
        let selectable = |d: Date| d <= today && first.is_none_or(|f| d >= f);
        let month = goto.month;
        let boot = self
            .live
            .read(cx)
            .latest
            .as_ref()
            .and_then(|s| s.system.as_ref())
            .map(|s| s.boot_time as f64);

        let routed = me.clone();
        let update = move |f: fn(&mut Explorer, &mut Window, &mut Context<Explorer>)| {
            let me = routed.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                me.update(cx, |this, cx| f(this, window, cx)).ok();
            }
        };
        let chip_row = div().flex().flex_row().flex_wrap().gap(rem_px(6.)).children(
            chips(now, boot, &tz).into_iter().enumerate().map(|(i, (label, at))| {
                let at = at.filter(|&a| self.oldest.is_none_or(|o| a >= o) && a <= now);
                let me = me.clone();
                div()
                    .id(ElementId::NamedInteger("goto-chip".into(), i as u64))
                    .px(rem_px(8.))
                    .h(rem_px(22.))
                    .flex()
                    .items_center()
                    .rounded(rem_px(11.))
                    .bg(t.control)
                    .border_1()
                    .border_color(t.control_border)
                    .text_size(t.caption)
                    .child(label)
                    .map(|d| match at {
                        Some(at) => d
                            .text_color(t.text_secondary)
                            .cursor_pointer()
                            .hover(|d| d.bg(t.card_hover).text_color(t.text))
                            .on_click(move |_, window, cx| {
                                me.update(cx, |this, cx| this.goto_set(local(at, &TimeZone::system()), window, cx))
                                    .ok();
                            }),
                        None => d.text_color(t.text_tertiary).opacity(0.5),
                    })
            }),
        );

        let earlier_month = first.is_none_or(|f| month > f.first_of_month());
        let later_month = month < today.first_of_month();
        let nav = |id: &'static str, icon: IconName, enabled: bool, months: i32| {
            let me = me.clone();
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(Icon::new(icon))
                .disabled(!enabled)
                .on_click(move |_, _, cx| {
                    me.update(cx, |this, cx| this.goto_month(months, cx)).ok();
                })
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(nav("goto-prev-month", IconName::ChevronLeft, earlier_month, -1))
            .child(
                div()
                    .text_size(t.caption)
                    .font_weight(t.weight_semibold)
                    .child(month.strftime("%B %Y").to_string()),
            )
            .child(nav("goto-next-month", IconName::ChevronRight, later_month, 1));
        let weekdays = div()
            .flex()
            .flex_row()
            .children(["M", "T", "W", "T", "F", "S", "S"].map(|d| {
                div()
                    .w(rem_px(CELL_W))
                    .flex()
                    .justify_center()
                    .text_size(t.small_caption)
                    .text_color(t.text_tertiary)
                    .child(d)
            }));
        let days = month_grid(month);
        let grid = div()
            .flex()
            .flex_col()
            .children(days.chunks(7).enumerate().map(|(row, week)| {
                div()
                    .flex()
                    .flex_row()
                    .children(week.iter().enumerate().map(|(col, &day)| {
                        let me = me.clone();
                        let enabled = selectable(day);
                        let chosen = day == goto.day;
                        let inside = day.month() == month.month();
                        div()
                            .id(ElementId::NamedInteger("goto-day".into(), (row * 7 + col) as u64))
                            .w(rem_px(CELL_W))
                            .h(rem_px(CELL_H))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(rem_px(6.))
                            .text_size(t.caption)
                            .font_features(t.numbers.clone())
                            .child(day.day().to_string())
                            .when(day == today && !chosen, |d| {
                                d.border_1().border_color(t.blue.opacity(0.6))
                            })
                            .map(|d| {
                                if chosen {
                                    d.bg(t.blue).text_color(gpui::white())
                                } else if !enabled {
                                    d.text_color(t.text_tertiary).opacity(0.45)
                                } else if inside {
                                    d.text_color(t.text)
                                } else {
                                    d.text_color(t.text_secondary)
                                }
                            })
                            .when(enabled && !chosen, |d| d.cursor_pointer().hover(|d| d.bg(t.card_hover)))
                            .when(enabled, |d| {
                                d.on_click(move |_, window, cx| {
                                    me.update(cx, |this, cx| this.goto_day(day, window, cx)).ok();
                                })
                            })
                    }))
            }));

        let valid = goto.time.is_some();
        let stepper = |id: &'static str, icon: IconName, minutes: i64| {
            let me = me.clone();
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(Icon::new(icon))
                .disabled(!valid)
                .on_click(move |_, window, cx| {
                    me.update(cx, |this, cx| this.goto_step(minutes, window, cx)).ok();
                })
        };
        let time_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(8.))
            .child(div().text_size(t.caption).text_color(t.text_secondary).child("Time"))
            .child(
                div()
                    .w(rem_px(76.))
                    .capture_action({
                        let me = me.clone();
                        move |_: &MoveUp, window, cx| {
                            me.update(cx, |this, cx| this.goto_step(1, window, cx)).ok();
                            cx.stop_propagation();
                        }
                    })
                    .capture_action({
                        let me = me.clone();
                        move |_: &MoveDown, window, cx| {
                            me.update(cx, |this, cx| this.goto_step(-1, window, cx)).ok();
                            cx.stop_propagation();
                        }
                    })
                    .child(Input::new(&goto.input).small()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .child(stepper("goto-minus", IconName::ChevronDown, -1))
                    .child(stepper("goto-plus", IconName::ChevronUp, 1)),
            )
            .when(!valid, |d| {
                d.child(div().text_size(t.small_caption).text_color(t.red).child("Use HH:MM"))
            });

        let target = goto.at().and_then(|at| timestamp(at, &tz));
        let note = match target {
            Some(at) if clamp(at, self.oldest, now) > at => {
                format!(
                    "History starts {}",
                    clock(clamp(at, self.oldest, now), "%-d %b at %H:%M")
                )
            }
            Some(at) if at > now => "That is in the future; goes to now".into(),
            Some(_) => format!("Centres the {} window here", super::model::span_label(self.view.span)),
            None => String::new(),
        };
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap(rem_px(6.))
            .child(
                Button::new("goto-cancel")
                    .ghost()
                    .small()
                    .label("Cancel")
                    .on_click(update(|this, _, cx| this.close_goto(cx))),
            )
            .child(
                Button::new("goto-go")
                    .primary()
                    .small()
                    .label("Go")
                    .disabled(!valid)
                    .on_click(update(|this, _, cx| this.go_to(cx))),
            );

        div()
            .flex()
            .flex_col()
            .gap(rem_px(10.))
            .w(rem_px(CELL_W * 7.))
            .capture_action({
                let me = me.clone();
                move |_: &Escape, _, cx| {
                    me.update(cx, |this, cx| this.close_goto(cx)).ok();
                    cx.stop_propagation();
                }
            })
            .child(
                div()
                    .text_size(t.panel_title)
                    .font_weight(t.weight_semibold)
                    .child("Go to time"),
            )
            .child(chip_row)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(header)
                    .child(weekdays)
                    .child(grid),
            )
            .child(time_row)
            .child(div().text_size(t.small_caption).text_color(t.text_tertiary).child(note))
            .child(footer)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::explorer::model::View;
    use jiff::civil::{date, time};

    const NOW: f64 = 1_760_000_000.;

    #[test]
    fn typed_times_must_be_a_real_24_hour_clock_time() {
        assert_eq!(parse_time("09:05"), Some(time(9, 5, 0, 0)));
        assert_eq!(
            parse_time(" 9:05 "),
            Some(time(9, 5, 0, 0)),
            "a single-digit hour is fine"
        );
        assert_eq!(parse_time("23:59"), Some(time(23, 59, 0, 0)));
        for bad in ["24:00", "12:60", "12:5", "1205", "", ":30", "+1:30", "12:3a", "123:00"] {
            assert_eq!(parse_time(bad), None, "{bad:?} is not a time");
        }
        assert_eq!(time_text(time(7, 3, 0, 0)), "07:03", "shown the way it is typed back");
    }

    #[test]
    fn stepping_past_midnight_moves_the_day() {
        let late = date(2026, 10, 14).at(23, 59, 0, 0);
        assert_eq!(step(late, 1), date(2026, 10, 15).at(0, 0, 0, 0));
        assert_eq!(
            step(date(2026, 3, 1).at(0, 0, 0, 0), -1),
            date(2026, 2, 28).at(23, 59, 0, 0)
        );
    }

    #[test]
    fn a_chosen_time_is_pulled_into_recorded_history() {
        let oldest = Some(NOW - 7. * 86400.);
        assert_eq!(
            clamp(NOW - 3600., oldest, NOW),
            NOW - 3600.,
            "inside history it is kept"
        );
        assert_eq!(
            clamp(NOW - 30. * 86400., oldest, NOW),
            NOW - 7. * 86400.,
            "before the first reading"
        );
        assert_eq!(clamp(NOW + 600., oldest, NOW), NOW, "there is no data in the future");
        assert_eq!(
            clamp(NOW - 30. * 86400., None, NOW),
            NOW - 30. * 86400.,
            "unknown history start"
        );
    }

    #[test]
    fn going_to_a_time_centres_it_in_the_current_span() {
        let tz = TimeZone::UTC;
        let at = local(NOW - 5. * 3600., &tz);
        let t = clamp(timestamp(at, &tz).unwrap(), Some(NOW - 86400.), NOW);
        let mut v = View::live(3600);
        v.centre_on(t, NOW);
        let (from, to) = v.range(NOW);
        assert!(!v.live, "going to a time leaves live");
        assert_eq!(to - from, 3600., "the span is kept");
        assert_eq!((from + to) / 2., t, "the chosen time sits in the middle");

        let mut v = View::live(3600);
        v.centre_on(NOW - 600., NOW);
        assert_eq!(
            v.range(NOW),
            (NOW - 3600., NOW),
            "near now the window stops at now instead"
        );
    }

    #[test]
    fn local_times_round_trip_through_the_picker() {
        let tz = TimeZone::UTC;
        let at = local(NOW, &tz);
        assert_eq!(timestamp(at, &tz), Some(NOW));
    }

    #[test]
    fn a_month_page_is_six_monday_first_weeks() {
        let days = month_grid(date(2026, 10, 15));
        assert_eq!(days.len(), 42);
        assert_eq!(days[0], date(2026, 9, 28), "1 Oct 2026 is a Thursday");
        assert_eq!(days[3], date(2026, 10, 1));
        assert!(days.contains(&date(2026, 10, 31)));
        let june = month_grid(date(2026, 6, 1));
        assert_eq!(june[0], date(2026, 6, 1), "a month starting on Monday has no lead-in");
    }

    #[test]
    fn quick_choices_follow_the_local_calendar() {
        let tz = TimeZone::UTC;
        let [hour, yesterday, today, boot] = chips(NOW, None, &tz);
        assert_eq!(hour.1, Some(NOW - 3600.));
        assert_eq!(yesterday.1, Some(NOW - 86400.));
        let midnight = timestamp(local(NOW, &tz).date().to_datetime(Time::midnight()), &tz);
        assert_eq!(today.1, midnight);
        assert_eq!(boot, ("Last boot", None), "unknown boot time stays unknown");
    }
}
