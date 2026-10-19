//! Which of the Logs screen's categories an entry belongs to, decided from its fields alone.

use crate::entry::Entry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Kernel,
    Hardware,
    Services,
    Apps,
    Scheduled,
    SignIns,
    BootPower,
    Crashes,
    /// Set by the package parser, never by a journal rule.
    Packages,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::Kernel,
        Category::Hardware,
        Category::Services,
        Category::Apps,
        Category::Scheduled,
        Category::SignIns,
        Category::BootPower,
        Category::Crashes,
        Category::Packages,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Kernel => "Kernel",
            Category::Hardware => "Hardware",
            Category::Services => "Services",
            Category::Apps => "Apps",
            Category::Scheduled => "Scheduled",
            Category::SignIns => "Sign-ins",
            Category::BootPower => "Boot & power",
            Category::Crashes => "Crashes",
            Category::Packages => "Packages",
        }
    }

    /// The word after `cat:` in a query.
    pub fn key(self) -> &'static str {
        match self {
            Category::Kernel => "kernel",
            Category::Hardware => "hardware",
            Category::Services => "services",
            Category::Apps => "apps",
            Category::Scheduled => "scheduled",
            Category::SignIns => "signins",
            Category::BootPower => "power",
            Category::Crashes => "crashes",
            Category::Packages => "packages",
        }
    }

    pub fn from_key(key: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.key() == key)
    }

    fn bit(self) -> u16 {
        1 << self as u16
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Categories(u16);

impl Categories {
    pub fn contains(self, c: Category) -> bool {
        self.0 & c.bit() != 0
    }

    pub fn insert(&mut self, c: Category) {
        self.0 |= c.bit();
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Category> {
        Category::ALL.into_iter().filter(move |c| self.contains(*c))
    }
}

pub const INDEX_FIELDS: &[&str] = &[
    "MESSAGE",
    "PRIORITY",
    "_BOOT_ID",
    "MESSAGE_ID",
    "_TRANSPORT",
    "_KERNEL_SUBSYSTEM",
    "_SYSTEMD_UNIT",
    "_SYSTEMD_USER_UNIT",
    "_SYSTEMD_SESSION",
    "UNIT",
    "USER_UNIT",
    "UNIT_RESULT",
    "SYSLOG_IDENTIFIER",
    "SYSLOG_FACILITY",
    "_COMM",
    "COREDUMP_PID",
    "COREDUMP_COMM",
    "COREDUMP_SIGNAL_NAME",
    "COREDUMP_UNIT",
];

pub mod ids {
    pub const COREDUMP: &str = "fc2e22bc6ee647b6b90729ab34a250b1";
    pub const OOMD_KILL: &str = "d989611b15e44c9dbf31e3c81256e4ed";
    pub const UNIT_OOM: &str = "fe6faa94e7774663a0da52717891d8ef";
    pub const UNIT_FAILURE: &str = "d9b373ed55a64feb8242e02dbe79a49c";
    pub const RESTART_SCHEDULED: &str = "5eb03494b6584870a536b337290809b3";
    pub const SHUTDOWN: &str = "98268866d1d54a499c4e98921d93bc40";
    pub const JOURNAL_STOPPED: &str = "d93fb3c9c24d451a97cea615ce59c00b";
    pub const JOURNAL_STARTED: &str = "f77379a8490b408bbe5f6940505a777b";
    pub const STARTUP_FINISHED: &str = "b07a249cd024414a82dd00cd181378ff";
    pub const SLEEP_START: &str = "6bbd95ee977941e497c48be27c254128";
    pub const SLEEP_STOP: &str = "8811e6df2a8e40f58a94cea26f8ebf14";
    pub const SESSION_NEW: &str = "8d45620c1a4348dbb17410da57c60c66";
    pub const SESSION_STOP: &str = "3354939424b4456d9802ca8333ed424a";
    pub const SEAT_NEW: &str = "fcbefc5da23d428093f97c82a9290f7b";
    pub const SEAT_REMOVED: &str = "e7852bfe46784ed0accde04bc864c2d5";
}

const POWER_IDS: &[&str] = &[
    ids::SHUTDOWN,
    ids::JOURNAL_STOPPED,
    ids::JOURNAL_STARTED,
    ids::STARTUP_FINISHED,
    ids::SLEEP_START,
    ids::SLEEP_STOP,
];
const SESSION_IDS: &[&str] = &[ids::SESSION_NEW, ids::SESSION_STOP, ids::SEAT_NEW, ids::SEAT_REMOVED];
const CRON_IDENTIFIERS: &[&str] = &["crond", "CROND", "CRON", "cron", "anacron", "atd", "crontab"];
const HARDWARE_SUBSYSTEMS: &[&str] = &[
    "usb",
    "pci",
    "pci_bus",
    "drm",
    "nvme",
    "scsi",
    "ata",
    "thermal",
    "hid",
    "i2c",
    "acpi",
    "block",
    "sound",
    "bluetooth",
    "mmc",
    "input",
    "net",
    "thunderbolt",
];
const HARDWARE_MESSAGES: &[&str] = &["mce:", "Machine check", "EDAC", "AER:"];
const POWER_KERNEL_MESSAGES: &[&str] = &["Linux version ", "PM: suspend", "PM: hibernation"];

pub fn categorize(e: &Entry) -> Categories {
    let mut set = Categories::default();
    let kernel = e.get("_TRANSPORT") == Some("kernel");
    let message_id = e.get("MESSAGE_ID").unwrap_or_default();
    let identifier = e.get("SYSLOG_IDENTIFIER").unwrap_or_default();
    let unit = e.get("_SYSTEMD_UNIT").unwrap_or_default();

    if kernel {
        set.insert(Category::Kernel);
        let message = e.message();
        let subsystem = e.get("_KERNEL_SUBSYSTEM").unwrap_or_default();
        if HARDWARE_SUBSYSTEMS.contains(&subsystem) || HARDWARE_MESSAGES.iter().any(|m| message.contains(m)) {
            set.insert(Category::Hardware);
        }
        if POWER_KERNEL_MESSAGES.iter().any(|m| message.starts_with(m)) {
            set.insert(Category::BootPower);
        }
    }
    if identifier == "systemd-udevd" || unit == "systemd-udevd.service" {
        set.insert(Category::Hardware);
    }

    if e.get("UNIT").is_some() || (!unit.is_empty() && !unit.ends_with(".scope")) {
        set.insert(Category::Services);
    }
    if e.get("_SYSTEMD_USER_UNIT").is_some() || e.get("USER_UNIT").is_some() || e.get("_SYSTEMD_SESSION").is_some() {
        set.insert(Category::Apps);
    }

    let timer = |u: &str| u.ends_with(".timer");
    if CRON_IDENTIFIERS.contains(&identifier) || timer(unit) || timer(e.get("UNIT").unwrap_or_default()) {
        set.insert(Category::Scheduled);
    }

    let facility = e.get("SYSLOG_FACILITY");
    let logind = identifier == "systemd-logind";
    if matches!(facility, Some("4" | "10"))
        || SESSION_IDS.contains(&message_id)
        || (logind && is_session_line(e.message()))
    {
        set.insert(Category::SignIns);
    }

    if POWER_IDS.contains(&message_id) {
        set.insert(Category::BootPower);
    }
    if message_id == ids::COREDUMP {
        set.insert(Category::Crashes);
    }
    set
}

fn is_session_line(message: &str) -> bool {
    message.starts_with("New session ") || message.starts_with("Removed session ") || message.starts_with("Session ")
}

/// The short source shown in the list.
pub fn source(e: &Entry) -> &str {
    if e.get("_TRANSPORT") == Some("kernel") {
        return "kernel";
    }
    let unit = e.get("_SYSTEMD_USER_UNIT").or_else(|| e.get("_SYSTEMD_UNIT"));
    let named = unit.filter(|u| !u.ends_with(".scope"));
    let s = named
        .or_else(|| e.get("SYSLOG_IDENTIFIER"))
        .or_else(|| e.get("_COMM"))
        .or(unit)
        .unwrap_or("unknown");
    s.strip_suffix(".service").unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn entry(fields: &[(&str, &str)]) -> Entry {
        Entry {
            t_us: 0,
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    fn cats(fields: &[(&str, &str)]) -> Vec<Category> {
        categorize(&entry(fields)).iter().collect()
    }

    #[test]
    fn a_kernel_usb_line_is_kernel_and_hardware() {
        let c = cats(&[
            ("_TRANSPORT", "kernel"),
            ("_KERNEL_SUBSYSTEM", "usb"),
            ("MESSAGE", "usb 1-2: new device"),
        ]);
        assert_eq!(c, [Category::Kernel, Category::Hardware]);
    }

    #[test]
    fn a_plain_kernel_line_is_not_hardware() {
        let c = cats(&[("_TRANSPORT", "kernel"), ("MESSAGE", "random: crng init done")]);
        assert_eq!(c, [Category::Kernel]);
    }

    #[test]
    fn machine_check_and_aer_text_marks_hardware_without_a_subsystem() {
        for m in [
            "mce: [Hardware Error]: CPU 0",
            "pcieport 0000:00:1c.0: AER: Corrected error",
        ] {
            assert!(
                categorize(&entry(&[("_TRANSPORT", "kernel"), ("MESSAGE", m)])).contains(Category::Hardware),
                "{m}"
            );
        }
    }

    #[test]
    fn a_service_message_is_services_but_a_session_scope_is_not() {
        assert_eq!(cats(&[("_SYSTEMD_UNIT", "sshd.service")]), [Category::Services]);
        assert!(cats(&[("_SYSTEMD_UNIT", "session-3.scope")]).is_empty());
    }

    #[test]
    fn the_manager_talking_about_a_unit_is_services_and_a_user_unit_is_apps() {
        assert_eq!(
            cats(&[("_SYSTEMD_UNIT", "init.scope"), ("UNIT", "foo.service")]),
            [Category::Services]
        );
        assert_eq!(cats(&[("_SYSTEMD_USER_UNIT", "app.service")]), [Category::Apps]);
        assert_eq!(
            cats(&[("_SYSTEMD_UNIT", "session-3.scope"), ("_SYSTEMD_SESSION", "3")]),
            [Category::Apps]
        );
    }

    #[test]
    fn cron_and_timers_are_scheduled() {
        assert_eq!(cats(&[("SYSLOG_IDENTIFIER", "CROND")]), [Category::Scheduled]);
        assert!(cats(&[("UNIT", "backup.timer")]).contains(&Category::Scheduled));
    }

    #[test]
    fn auth_facility_and_logind_sessions_are_sign_ins() {
        assert!(cats(&[("SYSLOG_FACILITY", "10")]).contains(&Category::SignIns));
        assert!(cats(&[("SYSLOG_FACILITY", "4")]).contains(&Category::SignIns));
        let c = cats(&[
            ("SYSLOG_IDENTIFIER", "systemd-logind"),
            ("MESSAGE", "New session 2 of user alice."),
        ]);
        assert!(c.contains(&Category::SignIns));
        assert!(!cats(&[("SYSLOG_FACILITY", "3")]).contains(&Category::SignIns));
    }

    #[test]
    fn shutdown_and_startup_messages_are_boot_and_power() {
        assert!(cats(&[("MESSAGE_ID", ids::SHUTDOWN)]).contains(&Category::BootPower));
        assert!(cats(&[("MESSAGE_ID", ids::STARTUP_FINISHED)]).contains(&Category::BootPower));
        assert!(
            cats(&[("_TRANSPORT", "kernel"), ("MESSAGE", "PM: suspend entry (s2idle)")]).contains(&Category::BootPower)
        );
    }

    #[test]
    fn a_core_dump_is_a_crash_and_also_a_service_line_when_it_has_a_unit() {
        let c = cats(&[
            ("MESSAGE_ID", ids::COREDUMP),
            ("_SYSTEMD_UNIT", "systemd-coredump@0.service"),
        ]);
        assert_eq!(c, [Category::Services, Category::Crashes]);
    }

    #[test]
    fn an_entry_matching_nothing_has_no_category() {
        assert!(categorize(&entry(&[("MESSAGE", "hello")])).is_empty());
    }

    #[test]
    fn keys_round_trip() {
        for c in Category::ALL {
            assert_eq!(Category::from_key(c.key()), Some(c));
        }
        assert_eq!(Category::from_key("nope"), None);
    }

    #[test]
    fn the_source_prefers_the_unit_then_the_identifier_and_never_a_session_scope() {
        assert_eq!(
            source(&entry(&[("_TRANSPORT", "kernel"), ("SYSLOG_IDENTIFIER", "kernel")])),
            "kernel"
        );
        assert_eq!(
            source(&entry(&[
                ("_SYSTEMD_UNIT", "sshd.service"),
                ("SYSLOG_IDENTIFIER", "sshd-session")
            ])),
            "sshd"
        );
        assert_eq!(
            source(&entry(&[
                ("_SYSTEMD_UNIT", "session-2.scope"),
                ("SYSLOG_IDENTIFIER", "sudo")
            ])),
            "sudo"
        );
        assert_eq!(source(&entry(&[("_COMM", "foo")])), "foo");
        assert_eq!(
            source(&entry(&[("_SYSTEMD_UNIT", "session-2.scope")])),
            "session-2.scope"
        );
        assert_eq!(source(&entry(&[])), "unknown");
    }
}
