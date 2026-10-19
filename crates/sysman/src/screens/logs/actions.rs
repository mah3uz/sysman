use std::process::{Command, Stdio};
use std::time::Duration;

use gpui::{
    AppContext, ClipboardItem, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Textarea, TextareaState};
use gpui_component::{Icon, Sizable, WindowExt};
use gpui_kit_assets::IconName;
use journal::Entry;
use journal::categories::source;
use journal::findings::FindingKind;

use super::list::priority_label;
use super::{Inspected, Logs, inspector, model};
use crate::data::systemd::{Runner, System};
use crate::ui::theme::{rem_px, theme};

const COPIED: Duration = Duration::from_millis(1500);
const COREDUMPCTL_TIMEOUT_S: u32 = 10;
/// How far around the entry's time to look for its dump; systemd-coredump logs a few seconds after the crash.
const DUMP_WINDOW_S: u64 = 120;
const SHEET_WIDTH: f32 = 760.;
const SHEET_TEXT_HEIGHT: f32 = 440.;

/// What the restart action is doing for the selected row.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Restart {
    Confirm,
    Running,
    Done(Result<(), String>),
}

/// A unit a failure entry names, which the inspector may offer to restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub unit: String,
    pub user: bool,
}

impl Target {
    pub fn args(&self) -> Vec<&str> {
        let mut args = Vec::with_capacity(3);
        if self.user {
            args.push("--user");
        }
        args.extend(["restart", self.unit.as_str()]);
        args
    }
}

/// Only failures are offered a restart, and a user unit only when it is this user's, since `systemctl --user` reaches
/// no one else's manager.
pub(super) fn restart_target(kind: Option<FindingKind>, e: &Entry, uid: u32) -> Option<Target> {
    if !matches!(kind, Some(FindingKind::FailedUnit | FindingKind::RestartLoop)) {
        return None;
    }
    if let Some(unit) = e.get("UNIT") {
        return Some(Target {
            unit: unit.to_string(),
            user: false,
        });
    }
    let unit = e.get("USER_UNIT")?;
    let mine = e.get("_UID").and_then(|u| u.parse::<u32>().ok()) == Some(uid);
    mine.then(|| Target {
        unit: unit.to_string(),
        user: true,
    })
}

fn restart(runner: &dyn Runner, target: &Target) -> Result<(), String> {
    let args = target.args();
    let ran = runner
        .run("systemctl", &args)
        .map_err(|e| format!("Could not run systemctl: {e}"))?;
    if ran.ok {
        return Ok(());
    }
    let message = ran.stderr.trim();
    Err(if message.is_empty() {
        format!("systemctl {} failed", args.join(" "))
    } else {
        message.to_owned()
    })
}

/// The stack traces systemd-coredump wrote into the entry's message, from the first thread on.
pub(super) fn stack_trace(message: &str) -> Option<&str> {
    let at = message.find("Stack trace of thread")?;
    Some(message[at..].trim_end())
}

/// `coredumpctl info` for the dump of `pid` made around `t_us`, under `timeout` so a slow debuginfod lookup cannot hang
/// the sheet.
pub(super) fn coredumpctl_args(pid: u32, t_us: u64) -> Vec<String> {
    let t = t_us / 1_000_000;
    vec![
        COREDUMPCTL_TIMEOUT_S.to_string(),
        "coredumpctl".into(),
        "info".into(),
        "--no-pager".into(),
        "-1".into(),
        format!("--since=@{}", t.saturating_sub(DUMP_WINDOW_S)),
        format!("--until=@{}", t + DUMP_WINDOW_S),
        pid.to_string(),
    ]
}

fn coredumpctl(pid: u32, t_us: u64) -> Result<String, String> {
    let out = Command::new("timeout")
        .args(coredumpctl_args(pid, t_us))
        .env("SYSTEMD_COLORS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not run coredumpctl: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    match out.status.code() {
        Some(124) => Err(format!("coredumpctl did not answer within {COREDUMPCTL_TIMEOUT_S} seconds.")),
        _ if !stdout.is_empty() => Ok(stdout),
        _ => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if stderr.is_empty() { "coredumpctl found no dump for this process.".into() } else { stderr })
        }
    }
}

/// The entry as one would paste it into a bug report.
pub(super) fn entry_text(time: &str, priority: u8, from: &str, message: &str) -> String {
    format!("{time} {} {from}: {message}", priority_label(priority))
}

pub(super) fn fields_json(e: &Entry) -> String {
    serde_json::to_string_pretty(&e.fields).unwrap_or_default()
}

pub(super) fn is_crash(e: &Entry) -> bool {
    e.get("COREDUMP_PID").is_some()
}

impl Logs {
    /// The inspector's actions for the selected entry, under its message.
    pub(super) fn actions(&self, i: &Inspected, cx: &Context<Self>) -> impl IntoElement + use<> {
        let t = theme(cx);
        let row = i.row;
        let copied = self.copied.filter(|(r, _)| *r == row).map(|(_, which)| which);
        let copy = |id: &'static str, label: &'static str, icon: IconName, text: String, cx: &Context<Self>| {
            let done = copied == Some(id);
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(Icon::new(if done { IconName::Check } else { icon }))
                .label(if done { "Copied" } else { label })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    this.copied_feedback(row, id, cx);
                }))
        };
        let (t_us, priority, from) = {
            let index = self.scan.index();
            index
                .row(row)
                .map(|r| (r.t_us, r.priority, index.name(r.source).to_string()))
                .unwrap_or_default()
        };
        let from = i.entry.as_deref().map_or(from, |e| source(e).to_string());
        let time = model::clock(t_us, "%Y-%m-%d %H:%M:%S%.3f", &self.tz);
        let text = entry_text(&time, priority, &from, &inspector::full_message(i));
        let kind = i.finding.and_then(|f| self.view.singles.get(f)).map(|c| c.kind);
        let target = i
            .entry
            .as_deref()
            .and_then(|e| restart_target(kind, e, unsafe { libc::getuid() }));
        let crash = i.entry.as_deref().filter(|e| is_crash(e)).cloned();
        let restarting = self.restart.as_ref().filter(|(r, _)| *r == row).map(|(_, s)| s.clone());
        let buttons = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(rem_px(2.))
            .ml(rem_px(-6.))
            .child(copy("logs-copy", "Copy", IconName::Copy, text, cx))
            .children(
                i.entry
                    .as_deref()
                    .map(|e| copy("logs-copy-fields", "Copy fields", IconName::Copy, fields_json(e), cx)),
            )
            .children(crash.map(|e| {
                Button::new("logs-backtrace")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::ListTree))
                    .label("Show backtrace")
                    .on_click(move |_, window, cx| open_backtrace(&e, window, cx))
            }))
            .children(target.clone().filter(|_| restarting.is_none()).map(|_| {
                Button::new("logs-restart")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::RotateCw))
                    .label("Restart unit")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.restart = Some((row, Restart::Confirm));
                        cx.notify();
                    }))
            }));
        let status = target.zip(restarting).map(|(target, state)| {
            let unit = target.unit.clone();
            let line = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(rem_px(8.))
                .px(rem_px(10.))
                .py(rem_px(7.))
                .rounded(t.card_radius)
                .bg(t.card)
                .text_size(t.caption);
            match state {
                Restart::Confirm => line
                    .child(
                        div()
                            .flex_1()
                            .min_w(rem_px(140.))
                            .flex()
                            .flex_col()
                            .child(div().font_weight(t.weight_semibold).child(format!("Restart {unit}?")))
                            .child(div().text_size(t.small_caption).text_color(t.text_secondary).child(
                                if target.user {
                                    "Your user's service manager restarts it."
                                } else {
                                    "The system asks for your password first."
                                },
                            )),
                    )
                    .child(
                        Button::new("logs-restart-cancel")
                            .xsmall()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.restart = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("logs-restart-ok")
                            .xsmall()
                            .primary()
                            .label("Restart")
                            .on_click(cx.listener(move |this, _, _, cx| this.run_restart(row, target.clone(), cx))),
                    ),
                Restart::Running => line
                    .text_color(t.text_secondary)
                    .child(format!("Restarting {unit}…")),
                Restart::Done(Ok(())) => line
                    .text_color(t.green)
                    .child(Icon::new(IconName::Check).xsmall())
                    .child(format!("Restarted {unit}")),
                Restart::Done(Err(e)) => line
                    .flex_col()
                    .items_start()
                    .gap(rem_px(2.))
                    .child(
                        div()
                            .text_color(t.red)
                            .font_weight(t.weight_semibold)
                            .child(format!("Could not restart {unit}")),
                    )
                    .child(div().text_size(t.small_caption).text_color(t.text_secondary).child(e)),
            }
        });
        div()
            .flex()
            .flex_col()
            .gap(rem_px(6.))
            .child(buttons)
            .children(status)
    }

    fn copied_feedback(&mut self, row: u32, which: &'static str, cx: &mut Context<Self>) {
        self.copied = Some((row, which));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED).await;
            this.update(cx, |this, cx| {
                if this.copied == Some((row, which)) {
                    this.copied = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn run_restart(&mut self, row: u32, target: Target, cx: &mut Context<Self>) {
        self.restart = Some((row, Restart::Running));
        cx.notify();
        let task = cx.background_executor().spawn(async move { restart(&System, &target) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.restart.as_ref().is_some_and(|(r, _)| *r == row) {
                    this.restart = Some((row, Restart::Done(result)));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

pub(super) fn open_backtrace(e: &Entry, window: &mut Window, cx: &mut gpui::App) {
    let comm = e.get("COREDUMP_COMM").unwrap_or("process").to_string();
    let pid: Option<u32> = e.get("COREDUMP_PID").and_then(|p| p.parse().ok());
    let subtitle = match pid {
        Some(pid) => format!("{comm} · pid {pid}"),
        None => comm,
    };
    let message = model::plain(e.message());
    let t_us = e
        .get("COREDUMP_TIMESTAMP")
        .and_then(|t| t.parse().ok())
        .unwrap_or(e.t_us);
    let sheet = cx.new(|cx| {
        let state = cx.new(|cx| {
            let mut s = TextareaState::new(window, cx);
            s.set_readonly(true, cx);
            s
        });
        let mut sheet = Backtrace {
            subtitle: subtitle.into(),
            state,
            text: None,
            fresh: false,
            from: "From the journal entry",
            copied: false,
        };
        match (stack_trace(&message), pid) {
            (Some(trace), _) => sheet.show(Ok(trace.to_string())),
            (None, Some(pid)) => {
                sheet.from = "From coredumpctl info";
                let task = cx.background_executor().spawn(async move { coredumpctl(pid, t_us) });
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    this.update(cx, |this: &mut Backtrace, cx| {
                        this.show(result);
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            (None, None) => sheet.show(Err("The entry names no process to look up.".into())),
        }
        sheet
    });
    window.open_dialog(cx, move |dialog, window, cx| {
        let t = theme(cx);
        let rem = window.rem_size();
        dialog
            .title("Backtrace")
            .w(rem_px(SHEET_WIDTH).to_pixels(rem))
            .margin_top(rem_px(40.).to_pixels(rem))
            .bg(t.window)
            .border_color(t.control_border)
            .rounded(rem_px(12.))
            .child(sheet.clone())
    });
}

/// A crash's stack trace in a sheet, selectable and copyable.
pub(super) struct Backtrace {
    subtitle: SharedString,
    state: Entity<TextareaState>,
    text: Option<Result<String, String>>,
    /// The text changed and the field has yet to be given it, which needs the window a frame has.
    fresh: bool,
    from: &'static str,
    copied: bool,
}

impl Backtrace {
    fn show(&mut self, text: Result<String, String>) {
        self.text = Some(text);
        self.fresh = true;
    }
}

impl Render for Backtrace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.fresh)
            && let Some(Ok(text)) = &self.text
        {
            let text = text.clone();
            self.state.update(cx, |s, cx| s.set_value(text, window, cx));
        }
        let t = theme(cx);
        let copy = self.text.as_ref().and_then(|r| r.as_ref().ok()).cloned();
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .truncate()
                    .child(format!("{} · {}", self.subtitle, self.from)),
            )
            .children(copy.map(|text| {
                Button::new("backtrace-copy")
                    .small()
                    .icon(Icon::new(if self.copied { IconName::Check } else { IconName::Copy }))
                    .label(if self.copied { "Copied" } else { "Copy" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        this.copied = true;
                        cx.notify();
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(COPIED).await;
                            this.update(cx, |this, cx| {
                                this.copied = false;
                                cx.notify();
                            })
                            .ok();
                        })
                        .detach();
                    }))
            }));
        let body = div()
            .h(rem_px(SHEET_TEXT_HEIGHT))
            .rounded(t.card_radius)
            .border_1()
            .border_color(t.hairline)
            .bg(t.card)
            .p(rem_px(10.))
            .when_some(self.text.as_ref(), |d, r| match r {
                Ok(_) => d.font_family(t.mono.clone()).child(
                    Textarea::new(&self.state)
                        .appearance(false)
                        .small()
                        .p_0()
                        .h_full()
                        .text_size(rem_px(12.)),
                ),
                Err(e) => d.text_size(t.caption).text_color(t.red).child(e.clone()),
            })
            .when(self.text.is_none(), |d| {
                d.text_size(t.caption)
                    .text_color(t.text_secondary)
                    .child("Asking coredumpctl for the dump…")
            });
        div()
            .flex()
            .flex_col()
            .gap(rem_px(10.))
            .pb(px(4.))
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn entry(fields: &[(&str, &str)]) -> Entry {
        Entry {
            t_us: 0,
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn only_failures_are_offered_a_restart() {
        let failed = entry(&[("UNIT", "keyd.service")]);
        assert_eq!(
            restart_target(Some(FindingKind::FailedUnit), &failed, 1000),
            Some(Target {
                unit: "keyd.service".into(),
                user: false
            })
        );
        assert!(restart_target(Some(FindingKind::RestartLoop), &failed, 1000).is_some());
        assert_eq!(restart_target(None, &failed, 1000), None, "a unit's ordinary message is not a failure");
        assert_eq!(restart_target(Some(FindingKind::Crash), &failed, 1000), None);
    }

    #[test]
    fn a_user_unit_is_restarted_only_for_its_own_user() {
        let mine = entry(&[("USER_UNIT", "pipewire.service"), ("_UID", "1000")]);
        let target = restart_target(Some(FindingKind::FailedUnit), &mine, 1000).unwrap();
        assert_eq!(target.args(), ["--user", "restart", "pipewire.service"]);
        assert_eq!(restart_target(Some(FindingKind::FailedUnit), &mine, 1001), None);
    }

    #[test]
    fn a_system_unit_is_restarted_through_systemd_not_the_user_manager() {
        let target = Target {
            unit: "keyd.service".into(),
            user: false,
        };
        assert_eq!(target.args(), ["restart", "keyd.service"]);
    }

    #[test]
    fn the_journal_trace_starts_at_the_first_thread() {
        let message = "Process 42 (app) of user 1000 dumped core.\n\nModule libc.so.6\nStack trace of thread 42:\n\
                       #0  0x1 raise (libc.so.6 + 0x1)\n\nStack trace of thread 43:\n#0  0x2 poll\n";
        let trace = stack_trace(message).unwrap();
        assert!(trace.starts_with("Stack trace of thread 42:"));
        assert!(trace.ends_with("#0  0x2 poll"), "every thread is kept");
        assert_eq!(stack_trace("Process 42 (app) of user 1000 dumped core."), None);
    }

    #[test]
    fn coredumpctl_picks_the_dump_of_that_pid_near_that_time_and_gives_up() {
        let args = coredumpctl_args(4242, 1_760_000_000_500_000);
        assert_eq!(args[0], "10", "timeout(1) stops it after ten seconds");
        assert_eq!(&args[1..5], ["coredumpctl", "info", "--no-pager", "-1"]);
        assert!(args.contains(&"--since=@1759999880".to_string()));
        assert!(args.contains(&"--until=@1760000120".to_string()));
        assert_eq!(args.last().unwrap(), "4242");
    }

    #[test]
    fn copied_entries_read_like_a_journal_line() {
        assert_eq!(
            entry_text("2026-10-15 21:20:50.123", 3, "keyd", "boom"),
            "2026-10-15 21:20:50.123 Error keyd: boom"
        );
        let e = entry(&[("MESSAGE", "boom"), ("_PID", "7")]);
        let json: BTreeMap<String, String> = serde_json::from_str(&fields_json(&e)).unwrap();
        assert_eq!(json["_PID"], "7");
    }
}
