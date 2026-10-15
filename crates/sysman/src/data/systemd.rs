use std::io;
use std::process::{Command, Stdio};

/// A unit the app turns on and off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    pub name: &'static str,
    /// A user unit, managed with `systemctl --user` and needing no authentication.
    pub user: bool,
    /// Enabling also starts it and disabling stops it.
    pub now: bool,
}

pub const HELPER: Service = Service {
    name: "sysman-helper.service",
    user: false,
    now: true,
};

pub const COLLECTOR: Service = Service {
    name: "sysmand.service",
    user: true,
    now: false,
};

impl Service {
    pub fn args(&self, verb: &'static str) -> Vec<&'static str> {
        let mut args = Vec::with_capacity(4);
        if self.user {
            args.push("--user");
        }
        args.push(verb);
        if self.now && matches!(verb, "enable" | "disable") {
            args.push("--now");
        }
        args.push(self.name);
        args
    }

    pub fn toggle_args(&self, on: bool) -> Vec<&'static str> {
        self.args(if on { "enable" } else { "disable" })
    }

    /// Where its switch sits.
    pub fn switched_on(&self, unit: &Unit) -> bool {
        if self.now { unit.on() } else { unit.enabled }
    }
}

/// A finished command's outcome.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a program to completion; tests put a fake in place of `systemctl`.
pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran>;
}

pub struct System;

impl Runner for System {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran> {
        // No stdin, so systemctl asks the desktop's polkit agent rather than a terminal.
        let out = Command::new(program).args(args).stdin(Stdio::null()).output()?;
        Ok(Ran {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// The service as systemd sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub installed: bool,
    pub enabled: bool,
    /// `is-active` output: `active`, `inactive`, `failed`, `activating`, …
    pub active: String,
}

impl Unit {
    /// On once enabled or started, whichever systemd reports first.
    pub fn on(&self) -> bool {
        self.enabled || matches!(self.active.as_str(), "active" | "activating")
    }
}

/// Older systemd prints nothing on stdout for a missing unit and says so on stderr.
pub fn parse_unit(is_enabled: &Ran, is_active: &Ran) -> Unit {
    let enabled = is_enabled.stdout.trim();
    let missing = enabled == "not-found"
        || (enabled.is_empty()
            && (is_enabled.stderr.contains("No such file") || is_enabled.stderr.contains("not found")));
    Unit {
        installed: !missing,
        enabled: matches!(enabled, "enabled" | "enabled-runtime"),
        active: is_active.stdout.trim().to_owned(),
    }
}

pub fn query(runner: &dyn Runner, service: Service) -> Result<Unit, String> {
    let run = |verb| {
        runner
            .run("systemctl", &service.args(verb))
            .map_err(|e| format!("Could not run systemctl: {e}"))
    };
    Ok(parse_unit(&run("is-enabled")?, &run("is-active")?))
}

/// For a system unit systemd asks polkit, which shows the system's authentication dialog.
pub fn set(runner: &dyn Runner, service: Service, on: bool) -> Result<(), String> {
    let args = service.toggle_args(on);
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn ran(ok: bool, stdout: &str, stderr: &str) -> Ran {
        Ran {
            ok,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Answers by verb and records every call, standing in for systemctl.
    struct Fake {
        answers: Vec<(&'static str, Ran)>,
        calls: Mutex<Vec<String>>,
    }

    impl Runner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran> {
            self.calls.lock().unwrap().push(format!("{program} {}", args.join(" ")));
            let verb = args.iter().find(|a| !a.starts_with("--")).copied().unwrap_or_default();
            self.answers
                .iter()
                .find(|(v, _)| *v == verb)
                .map(|(_, r)| r.clone())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such program"))
        }
    }

    fn fake(answers: Vec<(&'static str, Ran)>) -> Fake {
        Fake {
            answers,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn unit(installed: bool, enabled: bool, active: &str) -> Unit {
        Unit {
            installed,
            enabled,
            active: active.into(),
        }
    }

    #[test]
    fn systemd_tells_not_installed_from_installed_but_off() {
        let missing = parse_unit(&ran(false, "not-found\n", ""), &ran(false, "inactive\n", ""));
        assert!(!missing.installed);
        let old_systemd = parse_unit(
            &ran(
                false,
                "",
                "Failed to get unit file state for sysman-helper.service: No such file or directory",
            ),
            &ran(false, "inactive", ""),
        );
        assert!(
            !old_systemd.installed,
            "older systemd reports a missing unit on stderr only"
        );
        let off = parse_unit(&ran(false, "disabled\n", ""), &ran(false, "inactive\n", ""));
        assert_eq!(off, unit(true, false, "inactive"));
        assert!(!off.on());
        let on = parse_unit(&ran(true, "enabled\n", ""), &ran(true, "active\n", ""));
        assert!(on.installed && on.enabled && on.on());
    }

    #[test]
    fn the_helper_is_enabled_and_started_together_as_a_system_unit() {
        assert_eq!(HELPER.toggle_args(true), ["enable", "--now", "sysman-helper.service"]);
        assert_eq!(HELPER.toggle_args(false), ["disable", "--now", "sysman-helper.service"]);
        let runner = fake(vec![("enable", ran(true, "", ""))]);
        assert_eq!(set(&runner, HELPER, true), Ok(()));
        assert_eq!(
            *runner.calls.lock().unwrap(),
            ["systemctl enable --now sysman-helper.service"]
        );
    }

    #[test]
    fn start_at_login_only_enables_the_users_collector_and_leaves_it_running() {
        assert_eq!(COLLECTOR.toggle_args(true), ["--user", "enable", "sysmand.service"]);
        assert_eq!(COLLECTOR.toggle_args(false), ["--user", "disable", "sysmand.service"]);
        assert_eq!(
            COLLECTOR.args("is-enabled"),
            ["--user", "is-enabled", "sysmand.service"]
        );
        let running_not_enabled = unit(true, false, "active");
        assert!(
            !COLLECTOR.switched_on(&running_not_enabled),
            "running now is not starting at login"
        );
        assert!(HELPER.switched_on(&running_not_enabled));
    }

    #[test]
    fn a_failed_toggle_reports_what_systemctl_said() {
        let cancelled = fake(vec![(
            "enable",
            ran(false, "", "Failed to enable unit: Access denied\n"),
        )]);
        assert_eq!(
            set(&cancelled, HELPER, true),
            Err("Failed to enable unit: Access denied".into())
        );
        let silent = fake(vec![("disable", ran(false, "", ""))]);
        assert_eq!(
            set(&silent, HELPER, false),
            Err("systemctl disable --now sysman-helper.service failed".into())
        );
        let absent = fake(vec![]);
        assert!(
            set(&absent, HELPER, true)
                .unwrap_err()
                .starts_with("Could not run systemctl")
        );
    }

    #[test]
    fn query_asks_is_enabled_and_is_active() {
        let runner = fake(vec![
            ("is-enabled", ran(false, "disabled\n", "")),
            ("is-active", ran(false, "inactive\n", "")),
        ]);
        assert_eq!(query(&runner, HELPER), Ok(unit(true, false, "inactive")));
        assert_eq!(
            *runner.calls.lock().unwrap(),
            [
                "systemctl is-enabled sysman-helper.service",
                "systemctl is-active sysman-helper.service"
            ]
        );
    }
}
