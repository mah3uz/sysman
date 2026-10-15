use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::{Disableable, Icon, Sizable};
use gpui_kit_assets::IconName;
use sysman_config::edit::{Draft, Polled, Watched};
use sysman_config::modules::{MODULES, Module};
use sysman_config::{General, Retention};
use toml_edit::{Item, Value};

use super::{error_row, footnote, group, group_title, row};
use crate::data::systemd::{self, COLLECTOR, Runner};
use crate::ui::theme::{rem_px, theme};

#[derive(Debug, Clone, PartialEq)]
enum Restart {
    Idle,
    Running,
    Done,
    Failed(SharedString),
}

/// What a chip list holds: name prefixes, or seconds by name prefix.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Chips {
    Prefixes,
    Seconds,
}

type Entries = Vec<(String, Option<i64>)>;

/// The collector's `config.toml`, edited in place: comments and order kept, checked by the daemon's own parser
/// before every write, and following edits made to it by hand.
pub struct CollectorForm {
    file: Watched,
    runner: Arc<dyn Runner>,
    inputs: HashMap<String, Entity<InputState>>,
    /// Text in a field that is not a valid value for it; the draft keeps its last valid value meanwhile.
    invalid: HashMap<String, SharedString>,
    subscriptions: Vec<Subscription>,
    save_error: Option<SharedString>,
    /// A save happened since Settings opened, so the running collector may be out of date.
    saved: bool,
    restart: Restart,
}

impl CollectorForm {
    pub fn new(path: PathBuf, runner: Arc<dyn Runner>) -> Self {
        Self {
            file: Watched::open(path),
            runner,
            inputs: HashMap::new(),
            invalid: HashMap::new(),
            subscriptions: Vec::new(),
            save_error: None,
            saved: false,
            restart: Restart::Idle,
        }
    }

    pub fn poll(&mut self, cx: &mut Context<Self>) {
        match self.file.poll() {
            Polled::Unchanged => {}
            Polled::Reloaded => self.forget_fields(cx),
            Polled::Conflict => cx.notify(),
        }
    }

    /// The fields are rebuilt from the draft on the next render.
    fn forget_fields(&mut self, cx: &mut Context<Self>) {
        self.inputs.clear();
        self.invalid.clear();
        self.subscriptions.clear();
        self.save_error = None;
        cx.notify();
    }

    fn draft(&self) -> Option<&Draft> {
        self.file.draft.as_ref().ok()
    }

    fn explicit(&self, path: &[String]) -> Option<&Item> {
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        self.draft()?.get(&path)
    }

    fn edit(&mut self, path: &[String], value: Option<Value>, cx: &mut Context<Self>) {
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        if let Ok(draft) = &mut self.file.draft {
            match value {
                Some(v) => draft.set(&path, v),
                None => draft.remove(&path),
            }
            self.save_error = None;
            cx.notify();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.file.save() {
            Ok(()) => {
                self.saved = true;
                self.restart = Restart::Idle;
                self.save_error = None;
            }
            Err(e) => self.save_error = Some(format!("{e:#}").into()),
        }
        cx.notify();
    }

    fn revert(&mut self, cx: &mut Context<Self>) {
        self.file.reload();
        self.forget_fields(cx);
    }

    fn restart_collector(&mut self, cx: &mut Context<Self>) {
        self.restart = Restart::Running;
        cx.notify();
        let runner = self.runner.clone();
        let task = cx
            .background_executor()
            .spawn(async move { systemd::restart(runner.as_ref(), COLLECTOR) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.restart = match result {
                    Ok(()) => Restart::Done,
                    Err(e) => Restart::Failed(e.into()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn input(
        &mut self,
        key: String,
        placeholder: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_event: impl Fn(&mut Self, &Entity<InputState>, &InputEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> Entity<InputState> {
        if let Some(state) = self.inputs.get(&key) {
            return state.clone();
        }
        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(value)
        });
        self.subscriptions
            .push(cx.subscribe_in(&state, window, move |this, state, event, window, cx| {
                on_event(this, state, event, window, cx)
            }));
        self.inputs.insert(key, state.clone());
        state
    }

    /// A whole number; left empty, the key is removed so the daemon's default applies.
    fn number(
        &mut self,
        label: &'static str,
        path: Vec<String>,
        default: u64,
        min: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = path.join(".");
        let value = self
            .explicit(&path)
            .and_then(Item::as_value)
            .map(|v| v.to_string().trim().to_owned())
            .unwrap_or_default();
        let field = key.clone();
        let state = self.input(
            key.clone(),
            default.to_string(),
            value,
            window,
            cx,
            move |this, state, event, _, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                match parse_number(&state.read(cx).value(), min) {
                    Ok(n) => {
                        this.invalid.remove(&field);
                        this.edit(&path, n.map(Value::from), cx);
                    }
                    Err(e) => {
                        this.invalid.insert(field.clone(), e.into());
                        cx.notify();
                    }
                }
            },
        );
        let control = text_field(&state, 96., cx);
        with_error(row(label, control, cx), self.invalid.get(&key).cloned(), cx)
    }

    fn toggle(&mut self, label: &'static str, path: Vec<String>, default: bool, cx: &mut Context<Self>) -> AnyElement {
        let on = self.explicit(&path).and_then(Item::as_bool).unwrap_or(default);
        let control = Switch::new(SharedString::from(path.join(".")))
            .color(theme(cx).blue)
            .checked(on)
            .on_click(cx.listener(move |this, on: &bool, _, cx| this.edit(&path, Some((*on).into()), cx)));
        row(label, control, cx)
    }

    fn entries(&self, path: &[String], kind: Chips, default: &toml::Value) -> Entries {
        match (kind, self.explicit(path)) {
            (Chips::Prefixes, Some(item)) => item
                .as_array()
                .map(|a| a.iter().filter_map(|v| Some((v.as_str()?.to_owned(), None))).collect())
                .unwrap_or_default(),
            (Chips::Seconds, Some(item)) => item
                .as_table_like()
                .map(|t| t.iter().map(|(k, v)| (k.to_owned(), v.as_integer())).collect())
                .unwrap_or_default(),
            (Chips::Prefixes, None) => default
                .as_array()
                .map(|a| a.iter().filter_map(|v| Some((v.as_str()?.to_owned(), None))).collect())
                .unwrap_or_default(),
            (Chips::Seconds, None) => default
                .as_table()
                .map(|t| t.iter().map(|(k, v)| (k.clone(), v.as_integer())).collect())
                .unwrap_or_default(),
        }
    }

    fn store(&mut self, path: &[String], kind: Chips, entries: &Entries, cx: &mut Context<Self>) {
        let value = match kind {
            Chips::Prefixes => Value::Array(entries.iter().map(|(k, _)| k.as_str()).collect()),
            Chips::Seconds => Value::InlineTable(
                entries
                    .iter()
                    .map(|(k, s)| (k.as_str(), Value::from(s.unwrap_or(1))))
                    .collect(),
            ),
        };
        self.edit(path, Some(value), cx);
    }

    /// Removable chips, and a field that adds one on Enter.
    fn chips(
        &mut self,
        label: &'static str,
        path: Vec<String>,
        kind: Chips,
        default: toml::Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("{}+", path.join("."));
        let entries = self.entries(&path, kind, &default);
        let (field, at, fallback) = (key.clone(), path.clone(), default.clone());
        let state = self.input(
            key.clone(),
            match kind {
                Chips::Prefixes => "Add a prefix".into(),
                Chips::Seconds => "prefix=seconds".into(),
            },
            String::new(),
            window,
            cx,
            move |this, state, event, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = state.read(cx).value().trim().to_owned();
                    if text.is_empty() {
                        return;
                    }
                    let entry = match kind {
                        Chips::Prefixes => parse_prefix(&text).map(|p| (p, None)),
                        Chips::Seconds => parse_pair(&text).map(|(p, s)| (p, Some(s))),
                    };
                    match entry {
                        Ok(entry) => {
                            let mut entries = this.entries(&at, kind, &fallback);
                            entries.retain(|(k, _)| *k != entry.0);
                            entries.push(entry);
                            this.invalid.remove(&field);
                            this.store(&at, kind, &entries, cx);
                            state.update(cx, |s, cx| s.set_value("", window, cx));
                        }
                        Err(e) => {
                            this.invalid.insert(field.clone(), e.into());
                            cx.notify();
                        }
                    }
                }
                InputEvent::Change if this.invalid.remove(&field).is_some() => cx.notify(),
                _ => {}
            },
        );
        let t = theme(cx);
        let chips = entries.iter().enumerate().map(|(i, (name, secs))| {
            let (path, fallback, name) = (path.clone(), default.clone(), name.clone());
            let text = match secs {
                Some(s) => format!("{name} · {s} s"),
                None => name.clone(),
            };
            div()
                .id(SharedString::from(format!("{key}{i}")))
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(3.))
                .h(rem_px(22.))
                .pl(rem_px(8.))
                .pr(rem_px(4.))
                .rounded(rem_px(5.))
                .bg(t.card_hover)
                .text_size(t.caption)
                .child(text)
                .child(
                    div()
                        .id(SharedString::from(format!("{key}{i}x")))
                        .rounded(rem_px(3.))
                        .text_color(t.text_secondary)
                        .hover(|d| d.text_color(t.text))
                        .child(Icon::new(IconName::X).xsmall())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let mut entries = this.entries(&path, kind, &fallback);
                            entries.retain(|(k, _)| *k != name);
                            this.store(&path, kind, &entries, cx);
                        })),
                )
        });
        let body = div()
            .px(rem_px(12.))
            .py(rem_px(8.))
            .flex()
            .flex_col()
            .gap(rem_px(6.))
            .child(div().text_size(t.panel_title).child(label))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(rem_px(4.))
                    .children(chips)
                    .child(text_field(&state, 150., cx)),
            )
            .into_any_element();
        with_error(body, self.invalid.get(&key).cloned(), cx)
    }

    /// The Collector tab's editable part: sampling and retention.
    pub fn collector_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if let Some(broken) = self.broken(cx) {
            return broken;
        }
        let (g, r) = (General::default(), Retention::default());
        let p = |a: &str, b: &str| vec![a.to_owned(), b.to_owned()];
        let sampling = vec![
            self.number(
                "Interval (seconds)",
                p("general", "interval"),
                g.interval,
                1,
                window,
                cx,
            ),
            self.toggle("Sync each write to disk", p("general", "fsync"), g.fsync, cx),
        ];
        let retention = vec![
            self.number(
                "Crash log (days)",
                p("retention", "log_days"),
                r.log_days.into(),
                1,
                window,
                cx,
            ),
            self.number(
                "Samples (hours)",
                p("retention", "raw_hours"),
                r.raw_hours.into(),
                1,
                window,
                cx,
            ),
            self.number(
                "1-minute averages (days)",
                p("retention", "rollup_1m_days"),
                r.rollup_1m_days.into(),
                1,
                window,
                cx,
            ),
            self.number(
                "15-minute averages (days)",
                p("retention", "rollup_15m_days"),
                r.rollup_15m_days.into(),
                1,
                window,
                cx,
            ),
            self.number(
                "Process averages (days)",
                p("retention", "proc_rollup_days"),
                r.proc_rollup_days.into(),
                1,
                window,
                cx,
            ),
        ];
        vec![
            group_title("Sampling", cx).into_any_element(),
            group(sampling, cx).into_any_element(),
            footnote(
                "Every module samples on a multiple of this interval. Syncing keeps the crash log intact through a \
                 power loss, at the cost of a disk write each sample. An empty field uses the default shown.",
                cx,
            )
            .into_any_element(),
            group_title("History", cx).into_any_element(),
            group(retention, cx).into_any_element(),
            footnote(
                "How long each resolution is kept before it is pruned. Longer keeps more history on disk.",
                cx,
            )
            .into_any_element(),
        ]
    }

    pub fn modules_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if let Some(broken) = self.broken(cx) {
            return broken;
        }
        let mut out = vec![
            footnote(
                "Each module reads one part of the system. Its interval is rounded to a multiple of the collector's.",
                cx,
            )
            .into_any_element(),
        ];
        for module in MODULES {
            out.push(group_title(module_label(module.name), cx).into_any_element());
            out.push(group(self.module_rows(module, window, cx), cx).into_any_element());
        }
        out
    }

    fn module_rows(&mut self, module: &Module, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let at = |key: &str| vec!["modules".to_owned(), module.name.to_owned(), key.to_owned()];
        let enabled = self.explicit(&at("enabled")).and_then(Item::as_bool).unwrap_or(true);
        let mut rows = vec![self.toggle("Enabled", at("enabled"), true, cx)];
        if !enabled {
            return rows;
        }
        rows.push(self.number(
            "Interval (seconds)",
            at("interval"),
            module.default_interval,
            1,
            window,
            cx,
        ));
        for (key, default) in module.defaults() {
            let label = option_label(&key);
            let path = at(&key);
            rows.push(match &default {
                toml::Value::Boolean(b) => self.toggle(label, path, *b, cx),
                toml::Value::Integer(n) => self.number(label, path, *n as u64, 0, window, cx),
                toml::Value::Array(_) => self.chips(label, path, Chips::Prefixes, default.clone(), window, cx),
                toml::Value::Table(_) => self.chips(label, path, Chips::Seconds, default.clone(), window, cx),
                _ => continue,
            });
        }
        rows
    }

    /// The file on disk is not TOML: nothing to edit until it is fixed or removed.
    fn broken(&self, cx: &mut Context<Self>) -> Option<Vec<AnyElement>> {
        let error = self.file.draft.as_ref().err()?;
        Some(vec![
            group(
                vec![error_row(
                    format!("{} cannot be read: {error}", self.file.path().display()).into(),
                    cx,
                )],
                cx,
            )
            .into_any_element(),
            footnote(
                "Fix the file by hand; Settings reads it again as soon as it changes.",
                cx,
            )
            .into_any_element(),
        ])
    }

    /// Pinned under the Collector and Modules tabs: the outside-change notice, the save bar and the restart offer.
    pub fn footer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let dirty = self.file.dirty();
        let check = self.draft().map(Draft::check);
        let invalid = self
            .save_error
            .clone()
            .or_else(|| match &check {
                Some(Err(e)) => Some(format!("{e:#}").into()),
                _ => None,
            })
            .or_else(|| (!self.invalid.is_empty()).then(|| "Fix the fields marked above before saving.".into()));
        let can_save = dirty && invalid.is_none() && !self.file.conflict;
        let mut bar = div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(rem_px(6.))
            .px(t.page_padding)
            .py(rem_px(10.))
            .border_t_1()
            .border_color(t.hairline)
            .text_size(t.caption);
        if self.file.conflict {
            bar = bar.child(
                notice(t.orange, "config.toml changed on disk while you were editing it.", cx)
                    .child(
                        Button::new("config-reload")
                            .small()
                            .ghost()
                            .label("Reload")
                            .on_click(cx.listener(|this, _, _, cx| this.revert(cx))),
                    )
                    .child(
                        Button::new("config-keep")
                            .small()
                            .ghost()
                            .label("Keep mine")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.file.keep_mine();
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(e) = invalid {
            bar = bar.child(error_row(e, cx));
        }
        if self.saved && !dirty {
            let restart = match &self.restart {
                Restart::Idle => None,
                Restart::Running => Some(Spinner::new().small().into_any_element()),
                Restart::Done => Some(div().text_color(t.green).child("Restarted").into_any_element()),
                Restart::Failed(e) => Some(div().text_color(t.red).child(e.clone()).into_any_element()),
            };
            bar = bar.child(
                notice(
                    t.blue,
                    "Saved. Collection settings apply after the collector restarts.",
                    cx,
                )
                .children(restart)
                .child(
                    Button::new("collector-restart")
                        .small()
                        .ghost()
                        .label("Restart collector")
                        .disabled(self.restart == Restart::Running)
                        .on_click(cx.listener(|this, _, _, cx| this.restart_collector(cx))),
                ),
            );
        }
        bar.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(t.text_tertiary)
                        .child(tilde(self.file.path())),
                )
                .child(
                    Button::new("config-revert")
                        .small()
                        .ghost()
                        .label("Revert")
                        .disabled(!dirty)
                        .on_click(cx.listener(|this, _, _, cx| this.revert(cx))),
                )
                .child(
                    Button::new("config-save")
                        .small()
                        .primary()
                        .label("Save")
                        .disabled(!can_save)
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                ),
        )
        .into_any_element()
    }
}

fn notice(tint: gpui::Hsla, text: &'static str, cx: &App) -> gpui::Div {
    let t = theme(cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(rem_px(8.))
        .px(rem_px(10.))
        .py(rem_px(4.))
        .rounded(t.sub_radius)
        .bg(tint.opacity(0.12))
        .child(div().flex_1().min_w_0().text_color(t.text).child(text))
}

/// A text field drawn like the title bar's controls, so it reads as editable on the grouped rows.
fn text_field(state: &Entity<InputState>, width: f32, cx: &App) -> gpui::Div {
    let t = theme(cx);
    crate::ui::widgets::hairline(
        div()
            .w(rem_px(width))
            .px(rem_px(4.))
            .rounded(rem_px(6.))
            .bg(t.control)
            .border_color(t.control_border),
        t.hairline_width,
    )
    .child(Input::new(state).small().appearance(false))
}

fn tilde(path: &std::path::Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(|p| p.display().to_string())) {
        Some(rest) => format!("~/{rest}"),
        None => path.display().to_string(),
    }
}

/// A row with its field's error under it.
fn with_error(row: AnyElement, error: Option<SharedString>, cx: &App) -> AnyElement {
    match error {
        None => row,
        Some(e) => div()
            .flex()
            .flex_col()
            .child(row)
            .child(div().mt(rem_px(-6.)).child(error_row(e, cx)))
            .into_any_element(),
    }
}

/// Empty means the default; anything else must be a whole number of at least `min`.
fn parse_number(text: &str, min: i64) -> Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<i64>() {
        Ok(n) if n >= min => Ok(Some(n)),
        _ => Err(format!("Enter a whole number of at least {min}.")),
    }
}

fn parse_prefix(text: &str) -> Result<String, String> {
    if text.contains(char::is_whitespace) || text.contains(['=', '"']) {
        return Err("A prefix is one word, such as loop or veth.".into());
    }
    Ok(text.to_owned())
}

/// `nct67=10` or `nct67 = 10`.
fn parse_pair(text: &str) -> Result<(String, i64), String> {
    let error = || "Enter a chip prefix and seconds, such as nct67=10.".to_owned();
    let (prefix, secs) = text.split_once('=').ok_or_else(error)?;
    let prefix = parse_prefix(prefix.trim()).map_err(|_| error())?;
    match secs.trim().parse::<i64>() {
        Ok(s) if s >= 1 && !prefix.is_empty() => Ok((prefix, s)),
        _ => Err(error()),
    }
}

fn module_label(name: &str) -> &'static str {
    match name {
        "cgroups" => "Control groups",
        "cpu" => "CPU",
        "disk" => "Disk I/O",
        "fs" => "File systems",
        "gpu" => "GPU",
        "helper" => "Privileged helper",
        "hwmon" => "Sensors",
        "memory" => "Memory",
        "net" => "Network traffic",
        "netinfo" => "Network interfaces",
        "power" => "Power",
        "pressure" => "Pressure",
        "processes" => "Processes",
        "storage" => "Drives",
        "system" => "System",
        _ => "Module",
    }
}

fn option_label(key: &str) -> &'static str {
    match key {
        "top_n" => "Top entries kept",
        "per_core" => "Per-core usage",
        "frequency" => "Frequency",
        "partitions" => "Partitions",
        "exclude" => "Skip names starting with",
        "exclude_fstypes" => "Skip file system types",
        "processes" => "Per-process usage",
        "chip_intervals" => "Slower reads by chip",
        "io" => "Per-process I/O",
        "groups" => "App groups",
        "cmdline_max" => "Command line length",
        _ => "Option",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_number_field_means_the_default() {
        assert_eq!(parse_number("", 1), Ok(None));
        assert_eq!(parse_number("  ", 1), Ok(None));
        assert_eq!(parse_number(" 15 ", 1), Ok(Some(15)));
    }

    #[test]
    fn number_fields_refuse_what_the_daemon_would() {
        // zero intervals and fractional seconds are rejected by the daemon's parser too
        assert!(parse_number("0", 1).is_err());
        assert!(parse_number("1.5", 1).is_err());
        assert!(parse_number("-3", 0).is_err());
        assert_eq!(parse_number("0", 0), Ok(Some(0)), "a count may be zero");
    }

    #[test]
    fn chip_entries_are_single_words() {
        assert_eq!(parse_prefix("zram"), Ok("zram".into()));
        assert!(parse_prefix("two words").is_err());
        assert_eq!(parse_pair("nct67=10"), Ok(("nct67".into(), 10)));
        assert_eq!(parse_pair(" it87 = 5 "), Ok(("it87".into(), 5)));
        assert!(parse_pair("nct67").is_err());
        assert!(parse_pair("nct67=0").is_err());
        assert!(parse_pair("=5").is_err());
    }

    #[test]
    fn every_module_and_option_has_a_label() {
        // a new option or module must not show up as a bare "Option" in the form
        for module in MODULES {
            assert_ne!(module_label(module.name), "Module", "{}", module.name);
            for key in module.defaults().keys() {
                assert_ne!(option_label(key), "Option", "{}.{key}", module.name);
            }
        }
    }
}
