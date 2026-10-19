//! `SYSMAN_DEV_LOGS_DEMO=1`: the Logs screen reads a made-up week on a machine called "workstation" instead of the
//! real journal and package log, so screenshots show realistic content and nothing of the machine they were taken on.
//! The week is written once per run as a journal file through `systemd-journal-remote`, so the real reader reads it.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Context, Result, ensure};
use journal::categories::ids;

use super::now_us;

const SECOND: u64 = 1_000_000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const MACHINE: &str = "4f8e1c2d9a7b4e6f8c3d2b1a0e9f8d7c";
const BOOTS: [&str; 4] = [
    "a3c94e1f27d84b6e9f0c5d7a1b2e3f40",
    "5e2d8c7b1a9f4e3d8c6b5a4f3e2d1c0b",
    "c81f4a2e9d3b47c6a5e8f1d2b3c4a5e6",
    "e7d6c5b4a3f24e1d9c8b7a6f5e4d3c2b",
];

type Fields = Vec<(String, String)>;

pub struct Demo {
    pub dir: PathBuf,
    pub journal: PathBuf,
    pub boot: String,
}

pub fn get() -> Option<&'static Demo> {
    static DEMO: OnceLock<Option<Demo>> = OnceLock::new();
    DEMO.get_or_init(|| {
        std::env::var_os("SYSMAN_DEV_LOGS_DEMO")?;
        Some(build().expect("SYSMAN_DEV_LOGS_DEMO: writing the demo journal"))
    })
    .as_ref()
}

fn build() -> Result<Demo> {
    let dir = std::env::temp_dir().join(format!("sysman-logs-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let journal = dir.join("demo.journal");
    let mut week = Week::default();
    let pacman = week.write(now_us());
    std::fs::write(dir.join("pacman.log"), pacman)?;

    let mut child = Command::new("/usr/lib/systemd/systemd-journal-remote")
        .arg(format!("--output={}", journal.display()))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting systemd-journal-remote")?;
    child.stdin.take().unwrap().write_all(&week.export())?;
    ensure!(child.wait()?.success(), "systemd-journal-remote failed");
    Ok(Demo {
        dir,
        journal,
        boot: BOOTS[3].to_string(),
    })
}

#[derive(Default)]
struct Week {
    boot: usize,
    starts: [u64; 4],
    entries: Vec<(u64, usize, Fields)>,
    pacman: String,
    seed: u64,
    pid: u32,
}

impl Week {
    /// Writes the journal entries and returns the pacman log.
    fn write(&mut self, now: u64) -> String {
        self.seed = 0x9e37_79b9_7f4a_7c15;
        self.pid = 2000;
        let base = now - 7 * DAY;
        let plan = [
            (base + 9 * HOUR, base + DAY + 23 * HOUR),
            (base + 2 * DAY + 8 * HOUR, base + 3 * DAY + 2 * HOUR + 17 * MINUTE),
            (
                base + 3 * DAY + 2 * HOUR + 19 * MINUTE,
                base + 5 * DAY + 23 * HOUR + 40 * MINUTE,
            ),
            (now - 9 * HOUR - 12 * MINUTE, now),
        ];
        for (b, &(from, to)) in plan.iter().enumerate() {
            self.boot = b;
            self.starts[b] = from;
            let kernel = if b == 0 { "6.17.1-arch1-1" } else { "6.17.2-arch1-1" };
            self.boot_up(from, kernel, b == 2);
            self.routine(from + 2 * MINUTE, to - 5 * MINUTE);
            match b {
                0 => {
                    self.upgrade(from + 3 * HOUR + 7 * MINUTE);
                    self.failed_sudo(from + 26 * HOUR + 41 * MINUTE);
                    self.shut_down(to);
                }
                1 => self.gpu_hang(to - 40 * SECOND),
                2 => {
                    self.usb_flap(from + 5 * HOUR + 3 * MINUTE);
                    self.oom(from + DAY + 14 * HOUR + 22 * MINUTE);
                    self.small_update(from + 2 * DAY + 2 * HOUR);
                    self.shut_down(to);
                }
                _ => {
                    self.restart_loop(now - 3 * HOUR - 26 * MINUTE);
                    self.repeat_warning(now - 58 * MINUTE);
                    self.crash(now - 41 * MINUTE);
                }
            }
        }
        std::mem::take(&mut self.pacman)
    }

    fn rand(&mut self, n: u64) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed % n
    }

    fn next_pid(&mut self) -> u32 {
        self.pid += 1 + self.rand(40) as u32;
        self.pid
    }

    fn entry(&mut self, t: u64, prio: u8, fields: &[(&str, &str)]) {
        let mut all: Vec<(String, String)> = vec![
            ("PRIORITY".into(), prio.to_string()),
            ("_HOSTNAME".into(), "workstation".into()),
            ("_MACHINE_ID".into(), MACHINE.into()),
        ];
        all.extend(fields.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        self.entries.push((t, self.boot, all));
    }

    fn kernel(&mut self, t: u64, prio: u8, msg: &str) {
        let subsystem = ["usb ", "nvme", "amdgpu", "input:"]
            .iter()
            .find(|p| msg.starts_with(**p))
            .map(|p| match *p {
                "usb " => "usb",
                "input:" => "input",
                "amdgpu" => "pci",
                p => p,
            });
        let mut f = vec![
            ("_TRANSPORT", "kernel"),
            ("SYSLOG_IDENTIFIER", "kernel"),
            ("SYSLOG_FACILITY", "0"),
            ("MESSAGE", msg),
        ];
        if let Some(s) = subsystem {
            f.push(("_KERNEL_SUBSYSTEM", s));
        }
        self.entry(t, prio, &f);
    }

    fn manager(&mut self, t: u64, prio: u8, unit: &str, msg: &str, extra: &[(&str, &str)]) {
        let mut f = vec![
            ("_TRANSPORT", "journal"),
            ("_PID", "1"),
            ("_UID", "0"),
            ("_COMM", "systemd"),
            ("SYSLOG_IDENTIFIER", "systemd"),
            ("SYSLOG_FACILITY", "3"),
            ("UNIT", unit),
            ("MESSAGE", msg),
        ];
        f.extend_from_slice(extra);
        self.entry(t, prio, &f);
    }

    fn daemon(&mut self, t: u64, prio: u8, unit: &str, ident: &str, pid: u32, msg: &str) {
        let pid = pid.to_string();
        let f = [
            ("_TRANSPORT", "syslog"),
            ("_PID", pid.as_str()),
            ("_UID", "0"),
            ("_COMM", ident),
            ("SYSLOG_IDENTIFIER", ident),
            ("SYSLOG_FACILITY", "3"),
            ("_SYSTEMD_UNIT", unit),
            ("MESSAGE", msg),
        ];
        self.entry(t, prio, &f);
    }

    fn app(&mut self, t: u64, prio: u8, unit: &str, ident: &str, pid: u32, msg: &str) {
        let pid = pid.to_string();
        let f = [
            ("_TRANSPORT", "stdout"),
            ("_PID", pid.as_str()),
            ("_UID", "1000"),
            ("_COMM", ident),
            ("SYSLOG_IDENTIFIER", ident),
            ("_SYSTEMD_UNIT", "user@1000.service"),
            ("_SYSTEMD_USER_UNIT", unit),
            ("MESSAGE", msg),
        ];
        self.entry(t, prio, &f);
    }

    fn auth(&mut self, t: u64, prio: u8, ident: &str, pid: u32, msg: &str) {
        let pid = pid.to_string();
        let f = [
            ("_TRANSPORT", "syslog"),
            ("_PID", pid.as_str()),
            ("_UID", "0"),
            ("_COMM", ident),
            ("SYSLOG_IDENTIFIER", ident),
            ("SYSLOG_FACILITY", "10"),
            ("MESSAGE", msg),
        ];
        self.entry(t, prio, &f);
    }

    fn boot_up(&mut self, t: u64, kernel: &str, after_reset: bool) {
        let ms = |n: u64| t + n * 1000;
        self.kernel(
            t,
            5,
            &format!(
                "Linux version {kernel} (linux@archlinux) (gcc (GCC) 15.2.1 20250813, GNU ld (GNU Binutils) 2.45.0) #1 SMP PREEMPT_DYNAMIC"
            ),
        );
        let reason = if after_reset {
            "[0x00010000]: reset pin BP_SYS_RST_L was tripped"
        } else {
            "[0x00200800]: software wrote 0x6 to reset control register 0xCF9"
        };
        self.kernel(ms(2), 6, &format!("x86/amd: Previous system reset reason {reason}"));
        for (at, prio, line) in [
            (180, 6, "nvme nvme0: 16/0/0 default/read/poll queues"),
            (185, 6, " nvme0n1: p1 p2"),
            (410, 6, "usb 1-4: new full-speed USB device number 3 using xhci_hcd"),
            (
                690,
                6,
                "input: Logitech USB Receiver as /devices/pci0000:00/0000:00:08.1/0000:0e:00.3/usb1/1-4/1-4:1.0/0003:046D:C548.0001/input/input5",
            ),
            (
                1210,
                6,
                "amdgpu 0000:0c:00.0: amdgpu: VRAM: 16368M 0x0000008000000000 - 0x00000083FEFFFFFF (16368M used)",
            ),
            (1460, 6, "[drm] Initialized amdgpu 3.64.0 for 0000:0c:00.0 on minor 1"),
            (
                1890,
                6,
                "EXT4-fs (nvme0n1p2): mounted filesystem with ordered data mode. Quota mode: none.",
            ),
        ] {
            self.kernel(ms(at), prio, line);
        }
        let journald = if after_reset {
            format!(
                "File /var/log/journal/{MACHINE}/system.journal corrupted or uncleanly shut down, renaming and replacing."
            )
        } else {
            "System Journal is 1.2G, max 4G, 2.8G free.".to_string()
        };
        self.daemon(
            ms(2100),
            6,
            "systemd-journald.service",
            "systemd-journald",
            312,
            "Journal started",
        );
        self.daemon(
            ms(2110),
            4,
            "systemd-journald.service",
            "systemd-journald",
            312,
            &journald,
        );
        for (at, unit, msg) in [
            (
                2300,
                "systemd-udevd.service",
                "Started systemd-udevd.service - Rule-based Manager for Device Events and Files.",
            ),
            (
                3100,
                "shadow.timer",
                "Started shadow.timer - Daily verification of password and group files.",
            ),
            (
                3105,
                "fstrim.timer",
                "Started fstrim.timer - Discard unused filesystem blocks once a week.",
            ),
            (
                3110,
                "logrotate.timer",
                "Started logrotate.timer - Daily rotation of log files.",
            ),
            (
                3400,
                "NetworkManager.service",
                "Started NetworkManager.service - Network Manager.",
            ),
            (
                3600,
                "bluetooth.service",
                "Started bluetooth.service - Bluetooth service.",
            ),
            (3900, "cronie.service", "Started cronie.service - Command Scheduler."),
            (4200, "gdm.service", "Started gdm.service - GNOME Display Manager."),
            (
                4300,
                "multi-user.target",
                "Reached target multi-user.target - Multi-User System.",
            ),
        ] {
            self.manager(ms(at), 6, unit, msg, &[]);
        }
        self.daemon(
            ms(3420),
            6,
            "NetworkManager.service",
            "NetworkManager",
            640,
            "<info>  NetworkManager (version 1.54.1) is starting...",
        );
        self.daemon(
            ms(5800),
            6,
            "NetworkManager.service",
            "NetworkManager",
            640,
            "<info>  device (enp5s0): state change: ip-config -> activated",
        );
        self.manager(
            ms(4400),
            6,
            "init.scope",
            "Startup finished in 6.912s (firmware) + 2.104s (loader) + 1.873s (kernel) + 4.512s (userspace) = 15.401s.",
            &[("MESSAGE_ID", ids::STARTUP_FINISHED)],
        );
        let f = [
            ("_TRANSPORT", "journal"),
            ("_PID", "655"),
            ("_UID", "0"),
            ("_COMM", "systemd-logind"),
            ("SYSLOG_IDENTIFIER", "systemd-logind"),
            ("_SYSTEMD_UNIT", "systemd-logind.service"),
            ("MESSAGE_ID", ids::SESSION_NEW),
            ("MESSAGE", "New session 2 of user alex."),
        ];
        self.entry(ms(14_000), 6, &f);
        self.manager(
            ms(14_200),
            6,
            "user@1000.service",
            "Started user@1000.service - User Manager for UID 1000.",
            &[],
        );
        self.app(
            ms(15_100),
            6,
            "pipewire.service",
            "pipewire",
            1662,
            "mod.rtkit: RTKit does not give us MaxRealtimePriority, using 1",
        );
        self.app(
            ms(15_400),
            4,
            "wireplumber.service",
            "wireplumber",
            1667,
            "spa.bluez5: BlueZ system service is not available",
        );
    }

    fn routine(&mut self, from: u64, to: u64) {
        let mut hour = from - from % HOUR + HOUR + MINUTE;
        let mut t = from;
        while t < to {
            t += MINUTE + self.rand(9 * MINUTE);
            while hour < t.min(to) {
                let pid = self.next_pid();
                self.cron(hour, pid);
                hour += HOUR;
            }
            self.noise(t);
        }
    }

    fn cron(&mut self, t: u64, pid: u32) {
        let p = pid.to_string();
        for (dt, msg, facility) in [
            (
                0,
                "pam_unix(crond:session): session opened for user root(uid=0) by root(uid=0)",
                "10",
            ),
            (SECOND / 10, "(root) CMD (run-parts /etc/cron.hourly)", "9"),
            (2 * SECOND, "(root) CMDEND (run-parts /etc/cron.hourly)", "9"),
            (
                2 * SECOND + 1000,
                "pam_unix(crond:session): session closed for user root",
                "10",
            ),
        ] {
            let f = [
                ("_TRANSPORT", "syslog"),
                ("_PID", p.as_str()),
                ("_UID", "0"),
                ("_COMM", "crond"),
                ("SYSLOG_IDENTIFIER", "CROND"),
                ("SYSLOG_FACILITY", facility),
                ("_SYSTEMD_UNIT", "cronie.service"),
                ("MESSAGE", msg),
            ];
            self.entry(t + dt, 6, &f);
        }
    }

    fn noise(&mut self, t: u64) {
        let pid = self.next_pid();
        match self.rand(14) {
            0 | 1 => self.app(t, 6, "app-gnome-firefox-4127.scope", "firefox", 4127, "[Parent 4127, Main Thread] WARNING: Theme parsing error: gtk.css:68:35: The style property GtkButton:child-displacement-x is deprecated"),
            2 => self.app(t, 4, "org.gnome.Shell@wayland.service", "gnome-shell", 1590, "Window manager warning: Overwriting existing binding of keysym 31 with keysym 31 (keycode a)."),
            3 => self.daemon(t, 6, "rtkit-daemon.service", "rtkit-daemon", 1011, "Supervising 8 threads of 5 processes of 1 users."),
            4 => self.app(t, 4, "xdg-desktop-portal-gnome.service", "xdg-desktop-portal-gnome", 1904, "Failed to associate portal window with parent window"),
            5 => self.daemon(t, 6, "NetworkManager.service", "NetworkManager", 640, "<info>  dhcp4 (enp5s0): state changed new lease"),
            6 => self.app(t, 6, "app-gnome-code-2290.scope", "code", 2290, "[main] update#setState idle"),
            7 => {
                let msg = format!("usb 1-2: new high-speed USB device number {} using xhci_hcd", 4 + pid % 20);
                self.kernel(t, 6, &msg);
            }
            8 => self.daemon(t, 6, "bluetooth.service", "bluetoothd", 702, "Battery Provider Manager created"),
            9 => self.app(t, 4, "pipewire.service", "pipewire", 1662, "pw.node: (alsa_output.pci-0000_0e_00.6.analog-stereo-52) graph xrun not-triggered (0 suppressed)"),
            10 => self.daemon(t, 6, "systemd-timesyncd.service", "systemd-timesyncd", 488, "Network configuration changed, trying to establish connection."),
            11 => self.app(t, 6, "app-gnome-steam-3121.scope", "steam", 3121, "src/steamexe/updateui_xpm.cpp (397) : Assertion Failed: m_pUpdateUI"),
            _ => self.daemon(t, 6, "udisks2.service", "udisksd", 860, "Mounted /dev/sda1 at /run/media/alex/USB on behalf of uid 1000"),
        }
    }

    fn sudo(&mut self, t: u64, command: &str) {
        let pid = self.next_pid();
        self.auth(
            t,
            5,
            "sudo",
            pid,
            &format!("    alex : TTY=pts/1 ; PWD=/home/alex ; USER=root ; COMMAND={command}"),
        );
        self.auth(
            t + 4000,
            6,
            "sudo",
            pid,
            "pam_unix(sudo:session): session opened for user root(uid=0) by alex(uid=1000)",
        );
    }

    fn transaction(&mut self, t: u64, command: &str, changes: &[&str]) {
        let stamp = |t: u64| {
            jiff::Timestamp::from_microsecond(t as i64)
                .map(|ts| ts.strftime("%Y-%m-%dT%H:%M:%S+0000").to_string())
                .unwrap_or_default()
        };
        let mut log = format!("[{}] [PACMAN] Running '{command}'\n", stamp(t));
        log += &format!("[{}] [ALPM] transaction started\n", stamp(t + 9 * SECOND));
        for (i, c) in changes.iter().enumerate() {
            log += &format!("[{}] [ALPM] {c}\n", stamp(t + (12 + 3 * i as u64) * SECOND));
        }
        log += &format!(
            "[{}] [ALPM] transaction completed\n",
            stamp(t + (14 + 3 * changes.len() as u64) * SECOND)
        );
        self.pacman += &log;
    }

    fn upgrade(&mut self, t: u64) {
        self.sudo(t, "/usr/bin/pacman -Syu");
        self.transaction(
            t,
            "pacman -Syu",
            &[
                "upgraded linux (6.17.1.arch1-1 -> 6.17.2.arch1-1)",
                "upgraded linux-firmware (20250917-1 -> 20251011-1)",
                "upgraded mesa (1:25.2.4-1 -> 1:25.2.5-1)",
                "upgraded vulkan-radeon (1:25.2.4-1 -> 1:25.2.5-1)",
                "upgraded firefox (143.0.4-1 -> 144.0-1)",
                "upgraded systemd (258.1-1 -> 258.2-1)",
            ],
        );
    }

    fn small_update(&mut self, t: u64) {
        self.sudo(t, "/usr/bin/pacman -Syu");
        self.transaction(
            t,
            "pacman -Syu",
            &[
                "upgraded firefox (144.0-1 -> 144.0.2-1)",
                "upgraded python (3.13.7-1 -> 3.13.8-1)",
            ],
        );
        self.sudo(t + 6 * MINUTE, "/usr/bin/pacman -S htop");
        self.transaction(t + 6 * MINUTE, "pacman -S htop", &["installed htop (3.4.1-1)"]);
    }

    fn failed_sudo(&mut self, t: u64) {
        let pid = self.next_pid();
        for i in 0..3 {
            self.auth(
                t + i * 9 * SECOND,
                5,
                "sudo",
                pid,
                "pam_unix(sudo:auth): authentication failure; logname=alex uid=1000 euid=0 tty=/dev/pts/2 ruser=alex rhost=  user=alex",
            );
        }
        self.auth(t + 27 * SECOND, 1, "sudo", pid, "    alex : 3 incorrect password attempts ; TTY=pts/2 ; PWD=/home/alex ; USER=root ; COMMAND=/usr/bin/systemctl restart bluetooth");
    }

    fn shut_down(&mut self, t: u64) {
        let f = [
            ("_TRANSPORT", "journal"),
            ("_PID", "655"),
            ("_UID", "0"),
            ("_COMM", "systemd-logind"),
            ("SYSLOG_IDENTIFIER", "systemd-logind"),
            ("_SYSTEMD_UNIT", "systemd-logind.service"),
            ("MESSAGE_ID", ids::SHUTDOWN),
            ("MESSAGE", "System is powering down."),
        ];
        self.entry(t, 6, &f);
        self.manager(
            t + 2 * SECOND,
            6,
            "multi-user.target",
            "Stopped target multi-user.target - Multi-User System.",
            &[("MESSAGE_ID", ids::UNIT_STOPPED)],
        );
        let f = [
            ("_TRANSPORT", "journal"),
            ("_PID", "312"),
            ("_UID", "0"),
            ("_COMM", "systemd-journald"),
            ("SYSLOG_IDENTIFIER", "systemd-journald"),
            ("_SYSTEMD_UNIT", "systemd-journald.service"),
            ("MESSAGE_ID", ids::JOURNAL_STOPPED),
            ("MESSAGE", "Journal stopped"),
        ];
        self.entry(t + 4 * SECOND, 6, &f);
    }

    fn gpu_hang(&mut self, t: u64) {
        let dev = "amdgpu 0000:0c:00.0:";
        for (dt, prio, msg) in [
            (
                0,
                3,
                format!("{dev} [drm] *ERROR* ring gfx_0.0.0 timeout, signaled seq=1834211, emitted seq=1834213"),
            ),
            (
                1,
                3,
                format!("{dev} [drm] *ERROR* Process information: process steam pid 3121 thread steam:cs0 pid 3188"),
            ),
            (2, 6, format!("{dev} amdgpu: GPU reset begin!")),
            (4, 3, format!("{dev} amdgpu: MODE1 reset")),
            (9, 3, format!("{dev} amdgpu: GPU reset(2) failed")),
        ] {
            self.kernel(t + dt * SECOND, prio, &msg);
        }
    }

    fn usb_flap(&mut self, t: u64) {
        for i in 0..6 {
            self.kernel(t + i * 1200 * 1000, 3, "usb 3-2: device descriptor read/64, error -71");
        }
        self.kernel(
            t + 8 * SECOND,
            6,
            "usb 3-2: new high-speed USB device number 9 using xhci_hcd",
        );
    }

    fn oom(&mut self, t: u64) {
        self.kernel(
            t,
            4,
            "java invoked oom-killer: gfp_mask=0x140cca(GFP_HIGHUSER_MOVABLE|__GFP_COMP), order=0, oom_score_adj=0",
        );
        self.kernel(t + 500, 3, "Out of memory: Killed process 31877 (java) total-vm:21843252kB, anon-rss:12581004kB, file-rss:0kB, shmem-rss:0kB, UID:1000 pgtables:26812kB oom_score_adj:0");
    }

    fn restart_loop(&mut self, t: u64) {
        let unit = "syncthing@alex.service";
        for i in 0..5u64 {
            let at = t + i * 22 * SECOND;
            let pid = self.next_pid();
            let start = format!("Started {unit} - Syncthing - Open Source Continuous File Synchronization for alex.");
            self.manager(at, 6, unit, &start, &[]);
            let p = pid.to_string();
            let f = [
                ("_TRANSPORT", "stdout"),
                ("_PID", p.as_str()),
                ("_UID", "1000"),
                ("_COMM", "syncthing"),
                ("SYSLOG_IDENTIFIER", "syncthing"),
                ("_SYSTEMD_UNIT", unit),
                (
                    "MESSAGE",
                    "[start] FATAL: Error opening database: resource temporarily unavailable",
                ),
            ];
            self.entry(at + 900 * 1000, 3, &f);
            self.manager(
                at + SECOND,
                5,
                unit,
                &format!("{unit}: Main process exited, code=exited, status=1/FAILURE"),
                &[],
            );
            let failed = [("MESSAGE_ID", ids::UNIT_FAILURE), ("UNIT_RESULT", "exit-code")];
            self.manager(
                at + SECOND + 1000,
                4,
                unit,
                &format!("{unit}: Failed with result 'exit-code'."),
                &failed,
            );
            if i < 4 {
                let msg = format!("{unit}: Scheduled restart job, restart counter is at {}.", i + 1);
                self.manager(
                    at + 21 * SECOND,
                    6,
                    unit,
                    &msg,
                    &[("MESSAGE_ID", ids::RESTART_SCHEDULED)],
                );
            }
        }
        let end = t + 4 * 22 * SECOND + 2 * SECOND;
        self.manager(
            end,
            3,
            unit,
            &format!("{unit}: Start request repeated too quickly."),
            &[],
        );
        self.manager(
            end + 1000,
            3,
            unit,
            &format!("Failed to start {unit} - Syncthing - Open Source Continuous File Synchronization for alex."),
            &[],
        );
        let pid = self.next_pid();
        self.auth(
            end + 9 * MINUTE,
            5,
            "polkitd",
            pid,
            "Operator of unix-session:2 successfully authenticated as unix-user:alex to gain TEMPORARY authorization for action org.freedesktop.systemd1.manage-units for system-bus-name::1.212 [systemctl restart syncthing@alex.service] (owned by unix-user:alex)",
        );
        let start = format!("Started {unit} - Syncthing - Open Source Continuous File Synchronization for alex.");
        self.manager(end + 9 * MINUTE + 2 * SECOND, 6, unit, &start, &[]);
    }

    fn repeat_warning(&mut self, t: u64) {
        for i in 0..9 {
            self.app(
                t + i * 3 * SECOND,
                4,
                "wireplumber.service",
                "wireplumber",
                1667,
                "spa.bluez5: BlueZ system service is not available",
            );
        }
    }

    fn crash(&mut self, t: u64) {
        self.kernel(
            t,
            6,
            "firefox[48213]: segfault at 0 ip 00007f2c8e3a1d1c sp 00007f2c71bfe2a0 error 6 in libxul.so[3ba1d1c,7f2c8a800000+6c35000] likely on CPU 5 (core 2, socket 0)",
        );
        let message = "Process 48213 (firefox) of user 1000 dumped core.\n\n\
            Stack trace of thread 48241:\n\
            #0  0x00007f2c8e3a1d1c n/a (libxul.so + 0x4ba1d1c)\n\
            #1  0x00007f2c8e39f6a2 n/a (libxul.so + 0x4b9f6a2)\n\
            #2  0x00007f2c8b0c4e51 n/a (libxul.so + 0x18c4e51)\n\
            #3  0x00007f2c9a4a17eb start_thread (libc.so.6 + 0x947eb)\n\
            #4  0x00007f2c9a52518c __clone3 (libc.so.6 + 0x11818c)\n\
            ELF object binary architecture: AMD x86-64";
        let f = [
            ("_TRANSPORT", "journal"),
            ("_PID", "48302"),
            ("_UID", "0"),
            ("_COMM", "systemd-coredum"),
            ("SYSLOG_IDENTIFIER", "systemd-coredump"),
            ("_SYSTEMD_UNIT", "systemd-coredump@3-48300-0.service"),
            ("MESSAGE_ID", ids::COREDUMP),
            ("COREDUMP_PID", "48213"),
            ("COREDUMP_UID", "1000"),
            ("COREDUMP_GID", "1000"),
            ("COREDUMP_SIGNAL", "11"),
            ("COREDUMP_SIGNAL_NAME", "SIGSEGV"),
            ("COREDUMP_COMM", "firefox"),
            ("COREDUMP_EXE", "/usr/lib/firefox/firefox"),
            ("COREDUMP_CMDLINE", "/usr/lib/firefox/firefox"),
            ("COREDUMP_UNIT", "user@1000.service"),
            ("COREDUMP_USER_UNIT", "app-gnome-firefox-4127.scope"),
            ("COREDUMP_HOSTNAME", "workstation"),
            ("MESSAGE", message),
        ];
        self.entry(t + 3 * SECOND, 2, &f);
        self.app(
            t + 9 * MINUTE,
            6,
            "app-gnome-firefox-49102.scope",
            "firefox",
            49102,
            "ExceptionHandler::GenerateDump cloned child 49188",
        );
    }

    fn export(&mut self) -> Vec<u8> {
        self.entries.sort_by_key(|e| e.0);
        let mut out = Vec::new();
        for (t, boot, fields) in &self.entries {
            let mono = t - self.starts[*boot] + 1_800_000;
            out.extend(
                format!(
                    "__REALTIME_TIMESTAMP={t}\n__MONOTONIC_TIMESTAMP={mono}\n_BOOT_ID={}\n",
                    BOOTS[*boot]
                )
                .bytes(),
            );
            for (k, v) in fields {
                if v.contains('\n') {
                    out.extend(format!("{k}\n").bytes());
                    out.extend((v.len() as u64).to_le_bytes());
                    out.extend(v.bytes());
                    out.push(b'\n');
                } else {
                    out.extend(format!("{k}={v}\n").bytes());
                }
            }
            out.push(b'\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use journal::Entry;
    use journal::findings::{Detector, FindingKind};

    use super::*;

    #[test]
    fn the_demo_week_shows_each_kind_of_finding_once() {
        let mut week = Week::default();
        let pacman = week.write(now_us());
        week.entries.sort_by_key(|e| e.0);
        let mut detector = Detector::new();
        for (i, (t, boot, fields)) in week.entries.iter().enumerate() {
            let mut fields: std::collections::BTreeMap<_, _> = fields.iter().cloned().collect();
            fields.insert("_BOOT_ID".into(), BOOTS[*boot].into());
            detector.push(i, &Entry { t_us: *t, fields });
        }
        let kinds: Vec<FindingKind> = detector.finish(Some(BOOTS[3])).into_iter().map(|f| f.kind).collect();
        use FindingKind::*;
        for kind in [
            Crash,
            OomKill,
            FailedUnit,
            RestartLoop,
            BootWithoutShutdown,
            HardwareError,
            FailedSignIns,
        ] {
            assert_eq!(kinds.iter().filter(|k| **k == kind).count(), 1, "{kind:?} in {kinds:?}");
        }
        assert_eq!(journal::packages::parse_pacman(&pacman).len(), 3);
    }
}
