use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use schema::cpu::CpuMeta;
use schema::gpu::Meta as GpuMeta;
use schema::hwmon::HwmonMeta;
use schema::series::escape;
use schema::storage::StorageMeta;
use schema::system::SystemMeta;
use schema::{Meta, Sample};
use serde::Serialize;

pub mod item;

pub use item::Item;

const HWDATA: &str = "/usr/share/hwdata";
const SECURE_BOOT: &str = "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";
const SETUP_MODE: &str = "/sys/firmware/efi/efivars/SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c";
/// Values firmware vendors leave in DMI fields they never filled in.
const PLACEHOLDERS: [&str; 14] = [
    "to be filled by o.e.m.",
    "default string",
    "system product name",
    "system version",
    "system manufacturer",
    "not applicable",
    "not specified",
    "none",
    "o.e.m.",
    "oem",
    "sku",
    "type1productconfigid",
    "unknown",
    "123456789",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Category {
    System,
    Processor,
    Graphics,
    Memory,
    Displays,
    Storage,
    Power,
    Sensors,
    Network,
    Wifi,
    Bluetooth,
    Usb,
    Audio,
    Pci,
    Input,
    Security,
    Software,
}

impl Category {
    pub const ALL: [Category; 17] = [
        Self::System,
        Self::Processor,
        Self::Graphics,
        Self::Memory,
        Self::Displays,
        Self::Storage,
        Self::Power,
        Self::Sensors,
        Self::Network,
        Self::Wifi,
        Self::Bluetooth,
        Self::Usb,
        Self::Audio,
        Self::Pci,
        Self::Input,
        Self::Security,
        Self::Software,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Processor => "Processor",
            Self::Graphics => "Graphics",
            Self::Memory => "Memory",
            Self::Displays => "Displays",
            Self::Storage => "Storage",
            Self::Power => "Power and battery",
            Self::Sensors => "Sensors",
            Self::Network => "Network",
            Self::Wifi => "Wi-Fi",
            Self::Bluetooth => "Bluetooth",
            Self::Usb => "USB",
            Self::Audio => "Audio",
            Self::Pci => "PCI",
            Self::Input => "Input devices",
            Self::Security => "Security",
            Self::Software => "Software",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Node {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub props: Vec<(String, String)>,
    #[serde(skip)]
    pub item: Option<Item>,
}

impl Node {
    fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    fn sub(mut self, subtitle: Option<impl Into<String>>) -> Self {
        self.subtitle = subtitle.map(Into::into).filter(|s: &String| !s.is_empty());
        self
    }

    fn item(mut self, item: Item) -> Self {
        self.item = Some(item);
        self
    }

    /// Absent or empty values are left out rather than shown as blanks.
    fn prop(mut self, label: &str, value: Option<impl Into<String>>) -> Self {
        if let Some(v) = value.map(Into::into).filter(|v: &String| !v.trim().is_empty()) {
            self.props.push((label.to_owned(), v));
        }
        self
    }

    /// `needle` is already lower case.
    pub fn matches(&self, needle: &str) -> bool {
        self.title.to_lowercase().contains(needle)
            || self
                .subtitle
                .as_ref()
                .is_some_and(|s| s.to_lowercase().contains(needle))
            || self.props.iter().any(|(l, v)| prop_matches(l, v, needle))
    }
}

pub fn prop_matches(label: &str, value: &str, needle: &str) -> bool {
    !needle.is_empty() && (label.to_lowercase().contains(needle) || value.to_lowercase().contains(needle))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Section {
    pub category: Category,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub nodes: Vec<Node>,
}

// ---------- hwdata id databases ----------

/// `pci.ids` / `usb.ids`: vendors with their devices, and classes with their subclasses.
#[derive(Debug, Default)]
pub struct Ids {
    vendors: HashMap<u16, (String, HashMap<u16, String>)>,
    classes: HashMap<u8, (String, HashMap<u8, String>)>,
}

enum IdScope {
    Vendor(u16),
    Class(u8),
    Other,
}

impl Ids {
    pub fn parse(text: &str) -> Self {
        let mut ids = Self::default();
        let mut scope = IdScope::Other;
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') || line.starts_with("\t\t") {
                continue;
            }
            if let Some(child) = line.strip_prefix('\t') {
                let Some((id, name)) = child.split_once("  ") else {
                    continue;
                };
                match scope {
                    IdScope::Vendor(v) => {
                        if let (Ok(id), Some(entry)) = (u16::from_str_radix(id, 16), ids.vendors.get_mut(&v)) {
                            entry.1.insert(id, name.trim().to_owned());
                        }
                    }
                    IdScope::Class(c) => {
                        if let (Ok(id), Some(entry)) = (u8::from_str_radix(id, 16), ids.classes.get_mut(&c)) {
                            entry.1.insert(id, name.trim().to_owned());
                        }
                    }
                    IdScope::Other => {}
                }
                continue;
            }
            scope = IdScope::Other;
            if let Some(rest) = line.strip_prefix("C ") {
                if let Some((id, name)) = rest.split_once("  ")
                    && let Ok(c) = u8::from_str_radix(id, 16)
                {
                    ids.classes.insert(c, (name.trim().to_owned(), HashMap::new()));
                    scope = IdScope::Class(c);
                }
            } else if let Some((id, name)) = line.split_once("  ")
                && id.len() == 4
                && let Ok(v) = u16::from_str_radix(id, 16)
            {
                ids.vendors.insert(v, (name.trim().to_owned(), HashMap::new()));
                scope = IdScope::Vendor(v);
            }
        }
        ids
    }

    fn load(name: &str) -> Self {
        fs::read_to_string(Path::new(HWDATA).join(name)).map_or_else(|_| Self::default(), |t| Self::parse(&t))
    }

    pub fn vendor(&self, v: u16) -> Option<&str> {
        self.vendors.get(&v).map(|e| e.0.as_str())
    }

    pub fn device(&self, v: u16, d: u16) -> Option<&str> {
        self.vendors.get(&v)?.1.get(&d).map(String::as_str)
    }

    /// The subclass name when known, else the class name.
    pub fn class(&self, class: u8, sub: u8) -> Option<&str> {
        let (name, subs) = self.classes.get(&class)?;
        Some(subs.get(&sub).unwrap_or(name))
    }
}

/// `pnp.ids`: three-letter EDID manufacturer code, a tab, the company.
pub fn pnp_name(text: &str, code: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(code)?.strip_prefix('\t'))
        .map(|n| n.trim().to_owned())
}

// ---------- EDID ----------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Edid {
    pub manufacturer: String,
    pub product: u16,
    pub name: Option<String>,
    pub serial: Option<String>,
    pub year: Option<u16>,
    /// Preferred mode: width, height, refresh in Hz.
    pub mode: Option<(u32, u32, f64)>,
    pub size_mm: Option<(u32, u32)>,
}

pub fn parse_edid(b: &[u8]) -> Option<Edid> {
    if b.len() < 128 || b[..8] != [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0] {
        return None;
    }
    let id = u16::from_be_bytes([b[8], b[9]]);
    let manufacturer: String = [10, 5, 0]
        .iter()
        .map(|shift| char::from(b'@' + ((id >> shift) & 0x1F) as u8))
        .collect();
    let mut edid = Edid {
        manufacturer,
        product: u16::from_le_bytes([b[10], b[11]]),
        year: (b[17] > 0).then(|| 1990 + u16::from(b[17])),
        size_mm: (b[21] > 0 && b[22] > 0).then(|| (u32::from(b[21]) * 10, u32::from(b[22]) * 10)),
        ..Edid::default()
    };
    for d in b[54..126].as_chunks::<18>().0 {
        if d[0] != 0 || d[1] != 0 {
            if edid.mode.is_none() {
                let clock = f64::from(u16::from_le_bytes([d[0], d[1]])) * 10_000.;
                let h = u32::from(d[2]) | (u32::from(d[4] & 0xF0) << 4);
                let hb = u32::from(d[3]) | (u32::from(d[4] & 0x0F) << 8);
                let v = u32::from(d[5]) | (u32::from(d[7] & 0xF0) << 4);
                let vb = u32::from(d[6]) | (u32::from(d[7] & 0x0F) << 8);
                let total = f64::from((h + hb) * (v + vb));
                edid.mode = (total > 0.).then(|| (h, v, clock / total));
                let w = u32::from(d[12]) | (u32::from(d[14] & 0xF0) << 4);
                let hmm = u32::from(d[13]) | (u32::from(d[14] & 0x0F) << 8);
                if w > 0 && hmm > 0 {
                    edid.size_mm = Some((w, hmm));
                }
            }
            continue;
        }
        let text = || {
            let s: String = d[5..18]
                .iter()
                .take_while(|&&c| c != b'\n')
                .map(|&c| char::from(c))
                .collect();
            Some(s.trim().to_owned()).filter(|s| !s.is_empty())
        };
        match d[3] {
            0xFC => edid.name = text(),
            0xFF => edid.serial = text(),
            _ => {}
        }
    }
    Some(edid)
}

/// Diagonal in inches from a physical size in millimetres.
pub fn diagonal_in(w_mm: u32, h_mm: u32) -> f64 {
    f64::from(w_mm).hypot(f64::from(h_mm)) / 25.4
}

// ---------- procfs and efivar parsers ----------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SoundCard {
    pub index: u32,
    pub id: String,
    pub driver: String,
    pub name: String,
    pub long_name: String,
}

/// `/proc/asound/cards`: a header line per card, then its long name indented.
pub fn parse_asound(text: &str) -> Vec<SoundCard> {
    let mut cards: Vec<SoundCard> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let header = trimmed
            .split_once(' ')
            .and_then(|(n, rest)| Some((n.parse::<u32>().ok()?, rest)));
        if let Some((index, rest)) = header
            && let Some((id, rest)) = rest.trim_start().strip_prefix('[').and_then(|r| r.split_once(']'))
        {
            let rest = rest.trim_start_matches(':').trim();
            let (driver, name) = rest.split_once(" - ").unwrap_or(("", rest));
            cards.push(SoundCard {
                index,
                id: id.trim().to_owned(),
                driver: driver.trim().to_owned(),
                name: name.trim().to_owned(),
                long_name: String::new(),
            });
        } else if let Some(card) = cards.last_mut()
            && card.long_name.is_empty()
            && !trimmed.is_empty()
        {
            card.long_name = trimmed.to_owned();
        }
    }
    cards
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputDevice {
    pub name: String,
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub phys: String,
    pub handlers: Vec<String>,
    pub keyboard: bool,
}

impl InputDevice {
    pub fn kind(&self) -> &'static str {
        let has = |p: &str| self.handlers.iter().any(|h| h.starts_with(p));
        if has("js") {
            "Game controller"
        } else if has("mouse") {
            "Pointer"
        } else if self.keyboard {
            "Keyboard"
        } else if has("kbd") {
            "Buttons"
        } else {
            "Other"
        }
    }
}

pub fn bus_name(bus: u16) -> &'static str {
    match bus {
        0x01 => "PCI",
        0x03 => "USB",
        0x05 => "Bluetooth",
        0x06 => "Virtual",
        0x10 => "ISA",
        0x11 => "PS/2",
        0x18 => "I²C",
        0x19 => "Platform",
        0x1C => "SPI",
        _ => "Other",
    }
}

/// Whether bit `n` is set in a `/proc/bus/input/devices` bitmap: hex words, most significant first.
fn bitmap_has(words: &str, n: usize) -> bool {
    let words: Vec<&str> = words.split_whitespace().collect();
    let (index, bit) = (n / 64, n % 64);
    words
        .len()
        .checked_sub(index + 1)
        .and_then(|i| u64::from_str_radix(words[i], 16).ok())
        .is_some_and(|w| w >> bit & 1 == 1)
}

/// `/proc/bus/input/devices`: blank-line separated blocks of `X: key=value` lines.
pub fn parse_input_devices(text: &str) -> Vec<InputDevice> {
    const KEY_A: usize = 30;
    const KEY_Q: usize = 16;
    text.split("\n\n")
        .filter_map(|block| {
            let mut dev = InputDevice {
                name: String::new(),
                bus: 0,
                vendor: 0,
                product: 0,
                phys: String::new(),
                handlers: Vec::new(),
                keyboard: false,
            };
            for line in block.lines() {
                let Some((tag, rest)) = line.split_once(": ") else {
                    continue;
                };
                match tag {
                    "I" => {
                        for kv in rest.split_whitespace() {
                            let Some((k, v)) = kv.split_once('=') else {
                                continue;
                            };
                            let v = u16::from_str_radix(v, 16).unwrap_or(0);
                            match k {
                                "Bus" => dev.bus = v,
                                "Vendor" => dev.vendor = v,
                                "Product" => dev.product = v,
                                _ => {}
                            }
                        }
                    }
                    "N" => dev.name = rest.strip_prefix("Name=").unwrap_or(rest).trim_matches('"').to_owned(),
                    "P" => dev.phys = rest.strip_prefix("Phys=").unwrap_or("").to_owned(),
                    "H" => {
                        dev.handlers = rest
                            .strip_prefix("Handlers=")
                            .unwrap_or("")
                            .split_whitespace()
                            .map(str::to_owned)
                            .collect();
                    }
                    "B" => {
                        if let Some(keys) = rest.strip_prefix("KEY=") {
                            dev.keyboard = bitmap_has(keys, KEY_A) && bitmap_has(keys, KEY_Q);
                        }
                    }
                    _ => {}
                }
            }
            (!dev.name.is_empty()).then_some(dev)
        })
        .collect()
}

/// An EFI variable is four attribute bytes then its data; boolean variables keep their value in the last byte.
pub fn parse_efivar_bool(bytes: &[u8]) -> Option<bool> {
    (bytes.len() >= 5).then(|| bytes[bytes.len() - 1] == 1)
}

/// Firmware placeholders become `None`.
pub fn meaningful(v: &str) -> Option<&str> {
    let v = v.trim();
    (!v.is_empty() && !PLACEHOLDERS.contains(&v.to_lowercase().as_str())).then_some(v)
}

pub fn chassis(kind: u8) -> &'static str {
    match kind {
        3 => "Desktop",
        4 => "Low-profile desktop",
        5 => "Pizza box",
        6 => "Mini tower",
        7 => "Tower",
        8 => "Portable",
        9 => "Laptop",
        10 => "Notebook",
        11 => "Handheld",
        12 => "Docking station",
        13 => "All-in-one",
        14 => "Sub-notebook",
        15 => "Space-saving",
        16 => "Lunch box",
        17 => "Server",
        23 => "Rack mount",
        30 => "Tablet",
        31 => "Convertible",
        32 => "Detachable",
        35 => "Mini PC",
        36 => "Stick PC",
        _ => "Other",
    }
}

/// Installed memory from `MemTotal`, which is short of the modules by what firmware and the kernel reserve.
pub fn installed_gib(total_bytes: u64) -> u64 {
    let gib = total_bytes as f64 / (1u64 << 30) as f64;
    let step = if gib < 16. {
        2.
    } else if gib < 64. {
        8.
    } else {
        16.
    };
    ((gib / step).ceil() * step) as u64
}

// ---------- raw inventory from sysfs ----------

#[derive(Debug, Clone, Serialize)]
pub struct PciDevice {
    pub slot: String,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u32,
    pub vendor: Option<String>,
    pub device: Option<String>,
    pub class_name: Option<String>,
    pub subsystem: Option<String>,
    pub driver: Option<String>,
    pub revision: Option<String>,
    pub link: Option<String>,
    pub max_link: Option<String>,
    /// `D0` … `D3cold`.
    pub power_state: Option<String>,
    /// The bridge or root port above, by slot.
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsbDevice {
    pub port: String,
    pub bus: u32,
    pub dev: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub speed_mbps: Option<String>,
    pub class: u8,
    pub version: Option<String>,
    pub root: bool,
    /// Drivers bound to the device's interfaces, deduplicated.
    pub drivers: Vec<String>,
    /// Runtime power management: `active`, `suspended`.
    pub power: Option<String>,
}

impl UsbDevice {
    pub fn title(&self) -> String {
        self.product
            .clone()
            .unwrap_or_else(|| format!("{:04x}:{:04x}", self.vendor_id, self.product_id))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Display {
    pub connector: String,
    pub card: String,
    pub name: String,
    pub vendor: Option<String>,
    pub width: u32,
    pub height: u32,
    pub refresh: Option<f64>,
    pub size_mm: Option<(u32, u32)>,
    pub year: Option<u16>,
    pub modes: usize,
    /// Distinct resolutions offered, best first.
    pub resolutions: Vec<String>,
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bluetooth {
    pub name: String,
    pub bus: Option<String>,
    pub chip: Option<String>,
    pub radio: Option<String>,
    pub connections: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetDevice {
    pub name: String,
    pub mac: Option<String>,
    pub state: Option<String>,
    pub speed_mbps: Option<i64>,
    pub driver: Option<String>,
    pub mtu: Option<String>,
    pub wireless: bool,
    pub signal_dbm: Option<f64>,
    pub bus: Option<String>,
    /// The wireless phy (`phy0`), which names the card's hwmon chip.
    pub phy: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Supply {
    pub name: String,
    pub fields: BTreeMap<String, String>,
}

impl Supply {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    fn num(&self, key: &str) -> Option<f64> {
        self.get(key)?.parse().ok()
    }

    pub fn is_system_battery(&self) -> bool {
        self.get("TYPE") == Some("Battery") && self.get("SCOPE") != Some("Device")
    }

    /// Full capacity against design, from energy or charge counters, whichever the battery reports.
    pub fn health_pct(&self) -> Option<f64> {
        let ratio = |full: &str, design: &str| Some(self.num(full)? / self.num(design).filter(|d| *d > 0.)?);
        ratio("ENERGY_FULL", "ENERGY_FULL_DESIGN")
            .or_else(|| ratio("CHARGE_FULL", "CHARGE_FULL_DESIGN"))
            .map(|r| r * 100.)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Inventory {
    pub at: f64,
    pub took_s: f64,
    pub dmi: BTreeMap<String, String>,
    pub pci: Vec<PciDevice>,
    pub usb: Vec<UsbDevice>,
    pub displays: Vec<Display>,
    pub sound: Vec<SoundCard>,
    pub bluetooth: Vec<Bluetooth>,
    pub inputs: Vec<InputDevice>,
    pub nets: Vec<NetDevice>,
    pub supplies: Vec<Supply>,
    pub uefi: bool,
    pub secure_boot: Option<bool>,
    pub setup_mode: Option<bool>,
    pub tpm: Option<String>,
    pub lockdown: Option<String>,
    pub os: Option<String>,
    pub kernel: Option<String>,
    pub boot_time: Option<u64>,
    pub desktop: Option<String>,
    /// Block device path (`/dev/mapper/root`, `/dev/nvme0n1p2`) → the disk under it.
    pub holders: BTreeMap<String, String>,
    /// Disk → the PCI slot of its controller.
    pub block_pci: BTreeMap<String, String>,
    /// How many of the inventory's sources returned anything.
    pub sources: usize,
}

fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn hex<T: TryFrom<u32>>(path: impl AsRef<Path>) -> Option<T> {
    let s = read(path)?;
    T::try_from(u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()?).ok()
}

fn link_name(path: impl AsRef<Path>) -> Option<String> {
    Some(fs::read_link(path).ok()?.file_name()?.to_string_lossy().into_owned())
}

fn entries(dir: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn now_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

/// Reads everything; blocking, so run it off the UI thread.
pub fn read_inventory() -> Inventory {
    let started = Instant::now();
    let pci_ids = Ids::load("pci.ids");
    let usb_ids = Ids::load("usb.ids");
    let pnp = fs::read_to_string(Path::new(HWDATA).join("pnp.ids")).unwrap_or_default();
    let usb = read_usb(&usb_ids);
    let mut inv = Inventory {
        at: now_s(),
        dmi: read_dmi(),
        pci: read_pci(&pci_ids),
        displays: read_displays(&pnp),
        sound: fs::read_to_string("/proc/asound/cards").map_or_else(|_| Vec::new(), |t| parse_asound(&t)),
        bluetooth: read_bluetooth(&usb_ids),
        inputs: fs::read_to_string("/proc/bus/input/devices").map_or_else(|_| Vec::new(), |t| parse_input_devices(&t)),
        nets: read_nets(),
        supplies: read_supplies(),
        uefi: Path::new("/sys/firmware/efi").exists(),
        secure_boot: fs::read(SECURE_BOOT).ok().and_then(|b| parse_efivar_bool(&b)),
        setup_mode: fs::read(SETUP_MODE).ok().and_then(|b| parse_efivar_bool(&b)),
        tpm: entries("/sys/class/tpm")
            .first()
            .and_then(|p| read(p.join("tpm_version_major")))
            .map(|v| format!("TPM {v}.0")),
        lockdown: read("/sys/kernel/security/lockdown")
            .and_then(|l| l.split('[').nth(1)?.split(']').next().map(str::to_owned)),
        os: fs::read_to_string("/etc/os-release").ok().and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_owned())
        }),
        kernel: read("/proc/version"),
        boot_time: fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())),
        desktop: std::env::var("XDG_CURRENT_DESKTOP").ok().filter(|d| !d.is_empty()),
        usb,
        took_s: 0.,
        holders: BTreeMap::new(),
        block_pci: BTreeMap::new(),
        sources: 0,
    };
    (inv.holders, inv.block_pci) = read_blocks();
    inv.sources = [
        !inv.dmi.is_empty(),
        !inv.pci.is_empty(),
        !inv.usb.is_empty(),
        !inv.displays.is_empty(),
        !inv.sound.is_empty(),
        !inv.bluetooth.is_empty(),
        !inv.inputs.is_empty(),
        !inv.nets.is_empty(),
        !inv.supplies.is_empty(),
        !inv.holders.is_empty(),
        inv.os.is_some(),
        inv.kernel.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    inv.took_s = started.elapsed().as_secs_f64();
    inv
}

fn read_dmi() -> BTreeMap<String, String> {
    entries("/sys/class/dmi/id")
        .into_iter()
        .filter(|p| p.is_file() && file_name(p) != "uevent" && file_name(p) != "modalias")
        .filter_map(|p| Some((file_name(&p), read(&p)?)))
        .collect()
}

fn read_pci(ids: &Ids) -> Vec<PciDevice> {
    entries("/sys/bus/pci/devices")
        .into_iter()
        .filter_map(|p| {
            let vendor_id: u16 = hex(p.join("vendor"))?;
            let device_id: u16 = hex(p.join("device"))?;
            let class: u32 = hex(p.join("class")).unwrap_or(0);
            let sub_v: Option<u16> = hex(p.join("subsystem_vendor"));
            let sub_d: Option<u16> = hex(p.join("subsystem_device"));
            let subsystem = sub_v.filter(|v| *v != 0).map(|v| {
                let vendor = ids.vendor(v).map_or_else(|| format!("{v:04x}"), str::to_owned);
                match sub_d {
                    Some(d) => format!("{vendor} ({v:04x}:{d:04x})"),
                    None => vendor,
                }
            });
            let link = |speed: &str, width: &str| {
                read(p.join(speed))
                    .zip(read(p.join(width)))
                    .filter(|(s, _)| !s.starts_with("Unknown"))
                    .map(|(s, w)| format!("{s} x{w}"))
            };
            let canonical = fs::canonicalize(&p).ok();
            let slots = canonical
                .as_ref()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            let chain = item::pci_slots(&slots);
            Some(PciDevice {
                slot: file_name(&p),
                vendor_id,
                device_id,
                class,
                vendor: ids.vendor(vendor_id).map(str::to_owned),
                device: ids.device(vendor_id, device_id).map(str::to_owned),
                class_name: ids.class((class >> 16) as u8, (class >> 8) as u8).map(str::to_owned),
                subsystem,
                driver: link_name(p.join("driver")),
                revision: read(p.join("revision")),
                link: link("current_link_speed", "current_link_width"),
                max_link: link("max_link_speed", "max_link_width"),
                power_state: read(p.join("power_state")),
                parent: chain.len().checked_sub(2).map(|i| chain[i].to_owned()),
            })
        })
        .collect()
}

fn usb_device(p: &Path, ids: &Ids) -> Option<UsbDevice> {
    let vendor_id: u16 = hex(p.join("idVendor"))?;
    let product_id: u16 = hex(p.join("idProduct"))?;
    let port = file_name(p);
    let mut drivers: Vec<String> = entries(&p.to_string_lossy())
        .iter()
        .filter(|e| file_name(e).starts_with(&format!("{port}:")))
        .filter_map(|e| link_name(e.join("driver")))
        .collect();
    drivers.sort();
    drivers.dedup();
    Some(UsbDevice {
        drivers,
        power: read(p.join("power/runtime_status")),
        root: port.starts_with("usb"),
        bus: read(p.join("busnum"))?.parse().ok()?,
        dev: read(p.join("devnum"))?.parse().ok()?,
        manufacturer: read(p.join("manufacturer")).or_else(|| ids.vendor(vendor_id).map(str::to_owned)),
        product: read(p.join("product")).or_else(|| ids.device(vendor_id, product_id).map(str::to_owned)),
        speed_mbps: read(p.join("speed")),
        class: hex(p.join("bDeviceClass")).unwrap_or(0),
        version: read(p.join("version")),
        port,
        vendor_id,
        product_id,
    })
}

fn read_usb(ids: &Ids) -> Vec<UsbDevice> {
    entries("/sys/bus/usb/devices")
        .iter()
        .filter(|p| !file_name(p).contains(':'))
        .filter_map(|p| usb_device(p, ids))
        .collect()
}

fn read_displays(pnp: &str) -> Vec<Display> {
    entries("/sys/class/drm")
        .into_iter()
        .filter(|p| file_name(p).contains('-') && read(p.join("status")).as_deref() == Some("connected"))
        .map(|p| {
            let full = file_name(&p);
            let (card, connector) = full.split_once('-').unwrap_or(("", &full));
            let edid = fs::read(p.join("edid")).ok().and_then(|b| parse_edid(&b));
            let modes = read(p.join("modes")).unwrap_or_default();
            let first = modes.lines().next().and_then(|m| m.split_once('x')).and_then(|(w, h)| {
                Some((
                    w.parse().ok()?,
                    h.trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok()?,
                ))
            });
            let mode = edid.as_ref().and_then(|e| e.mode);
            let (width, height) = mode.map(|(w, h, _)| (w, h)).or(first).unwrap_or((0, 0));
            Display {
                name: edid
                    .as_ref()
                    .and_then(|e| e.name.clone())
                    .unwrap_or_else(|| connector.to_owned()),
                vendor: edid
                    .as_ref()
                    .map(|e| pnp_name(pnp, &e.manufacturer).unwrap_or_else(|| e.manufacturer.clone())),
                refresh: mode.map(|m| m.2),
                size_mm: edid.as_ref().and_then(|e| e.size_mm),
                year: edid.as_ref().and_then(|e| e.year),
                modes: modes.lines().count(),
                resolutions: {
                    let mut seen: Vec<String> = Vec::new();
                    for m in modes.lines().map(|m| m.trim().to_owned()) {
                        if !seen.contains(&m) {
                            seen.push(m);
                        }
                    }
                    seen
                },
                builtin: ["eDP", "LVDS", "DSI"].iter().any(|k| connector.starts_with(k)),
                connector: connector.to_owned(),
                card: card.to_owned(),
                width,
                height,
            }
        })
        .collect()
}

fn read_bluetooth(ids: &Ids) -> Vec<Bluetooth> {
    let all = entries("/sys/class/bluetooth");
    all.iter()
        .filter(|p| !file_name(p).contains(':'))
        .map(|p| {
            let name = file_name(p);
            let device = fs::canonicalize(p.join("device")).ok();
            let usb = device
                .as_ref()
                .and_then(|d| d.parent())
                .and_then(|parent| usb_device(parent, ids));
            let radio = entries(&p.to_string_lossy())
                .into_iter()
                .find(|e| file_name(e).starts_with("rfkill"))
                .and_then(|r| match read(r.join("state"))?.as_str() {
                    "1" => Some("On".to_owned()),
                    "0" => Some("Off (soft blocked)".to_owned()),
                    "2" => Some("Off (hard blocked)".to_owned()),
                    _ => None,
                });
            let prefix = format!("{name}:");
            Bluetooth {
                bus: device
                    .as_ref()
                    .map(|_| if usb.is_some() { "USB" } else { "Other" }.to_owned()),
                chip: usb.map(|u| match (u.manufacturer, u.product) {
                    (Some(m), Some(p)) => format!("{m} {p}"),
                    (m, p) => p.or(m).unwrap_or_default(),
                }),
                connections: all.iter().filter(|e| file_name(e).starts_with(&prefix)).count(),
                radio,
                name,
            }
        })
        .collect()
}

fn read_nets() -> Vec<NetDevice> {
    let signal: HashMap<String, f64> = fs::read_to_string("/proc/net/wireless")
        .unwrap_or_default()
        .lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let level = rest.split_whitespace().nth(2)?.trim_end_matches('.').parse().ok()?;
            Some((name.trim().to_owned(), level))
        })
        .collect();
    entries("/sys/class/net")
        .into_iter()
        .filter(|p| p.join("device").exists())
        .map(|p| {
            let name = file_name(&p);
            let device = fs::canonicalize(p.join("device")).ok();
            NetDevice {
                mac: read(p.join("address")),
                state: read(p.join("operstate")),
                speed_mbps: read(p.join("speed"))
                    .and_then(|s| s.parse().ok())
                    .filter(|s: &i64| *s > 0),
                driver: link_name(p.join("device/driver")),
                mtu: read(p.join("mtu")),
                wireless: p.join("wireless").exists() || p.join("phy80211").exists(),
                signal_dbm: signal.get(&name).copied(),
                phy: link_name(p.join("phy80211")),
                bus: device.map(|d| {
                    if d.to_string_lossy().contains("/usb") {
                        "USB".to_owned()
                    } else {
                        format!("PCI {}", file_name(&d))
                    }
                }),
                name,
            }
        })
        .collect()
}

/// Which disk each block device sits on, following partitions and device-mapper slaves; and each disk's PCI slot.
fn read_blocks() -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    fn disk(name: &str, depth: u8) -> Option<String> {
        let dir = Path::new("/sys/class/block").join(name);
        if dir.join("partition").exists() {
            return fs::canonicalize(&dir).ok()?.parent().map(file_name);
        }
        let slaves = entries(&dir.join("slaves").to_string_lossy());
        match slaves.first() {
            Some(s) if depth < 8 => disk(&file_name(s), depth + 1),
            _ => Some(name.to_owned()),
        }
    }
    let mut holders = BTreeMap::new();
    let mut block_pci = BTreeMap::new();
    for p in entries("/sys/class/block") {
        let name = file_name(&p);
        let Some(d) = disk(&name, 0) else {
            continue;
        };
        if let Some(dm) = read(p.join("dm/name")) {
            holders.insert(format!("/dev/mapper/{dm}"), d.clone());
        }
        holders.insert(format!("/dev/{name}"), d);
        if !p.join("partition").exists()
            && let Ok(dev) = fs::canonicalize(p.join("device"))
            && let Some(slot) = item::pci_slots(&dev.to_string_lossy()).last()
        {
            block_pci.insert(name, (*slot).to_owned());
        }
    }
    (holders, block_pci)
}

fn read_supplies() -> Vec<Supply> {
    entries("/sys/class/power_supply")
        .into_iter()
        .map(|p| Supply {
            name: file_name(&p),
            fields: read(p.join("uevent"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let (k, v) = l.strip_prefix("POWER_SUPPLY_")?.split_once('=')?;
                    Some((k.to_owned(), v.to_owned()))
                })
                .collect(),
        })
        .collect()
}

// ---------- sensors ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Domain {
    Cpu,
    Graphics,
    Memory,
    Storage,
    Board,
    Wireless,
    Ethernet,
    Usb,
    Other,
    Fans,
}

impl Domain {
    pub fn title(self) -> &'static str {
        match self {
            Self::Cpu => "CPU die",
            Self::Graphics => "Graphics",
            Self::Memory => "Memory modules",
            Self::Storage => "Storage",
            Self::Board => "Motherboard",
            Self::Wireless => "Wireless",
            Self::Ethernet => "Ethernet",
            Self::Usb => "USB controllers",
            Self::Other => "Other",
            Self::Fans => "Fans",
        }
    }
}

/// The domain of an hwmon chip, from its driver name (the part of `name/device` before the slash).
pub fn domain_of(chip: &str) -> Domain {
    let name = chip.split('/').next().unwrap_or(chip);
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| name.starts_with(p));
    if starts(&[
        "k10temp",
        "coretemp",
        "zenpower",
        "cpu_thermal",
        "k8temp",
        "via_cputemp",
    ]) {
        Domain::Cpu
    } else if starts(&["amdgpu", "radeon", "nouveau", "i915", "xe", "nvidia"]) {
        Domain::Graphics
    } else if starts(&["spd5118", "jc42", "ee1004"]) {
        Domain::Memory
    } else if starts(&["nvme", "drivetemp"]) {
        Domain::Storage
    } else if starts(&[
        "nct", "it87", "it86", "asus", "acpitz", "f71", "w83", "dell", "thinkpad", "gigabyte",
    ]) {
        Domain::Board
    } else if starts(&["mt7", "iwlwifi", "ath", "rtw", "brcm", "phy"]) || name.ends_with("_phy0") {
        Domain::Wireless
    } else if starts(&["en", "eth", "r8169", "igc", "igb", "atlantic", "bnxt", "mlx"]) {
        Domain::Ethernet
    } else if name.contains("xhci") {
        Domain::Usb
    } else {
        Domain::Other
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    pub domain: Domain,
    pub chip: String,
    pub label: String,
    pub path: String,
    pub value: f64,
    /// The sensor's own `crit`, else `max`, when believable.
    pub limit: Option<f64>,
}

/// Disconnected probes read far below zero, unset limits read absurdly high.
fn plausible_temp(v: f64) -> bool {
    (-30. ..=150.).contains(&v)
}

/// Every temperature and fan from the latest sample, hottest first within each domain.
pub fn sensors(sample: Option<&Sample>, hwmon_meta: Option<&HwmonMeta>) -> Vec<Sensor> {
    let mut out = Vec::new();
    let Some(sample) = sample else {
        return out;
    };
    let hwmon = sample.hwmon.as_ref();
    for (chip, kinds) in hwmon.into_iter().flatten() {
        for (kind, domain) in [("temp", domain_of(chip)), ("fan", Domain::Fans)] {
            for (label, value) in kinds.get(kind).into_iter().flatten() {
                let Some(value) = *value else {
                    continue;
                };
                if kind == "temp" && !plausible_temp(value) {
                    continue;
                }
                // Board controllers also report the CPU they sit beside; that reading belongs with the CPU.
                let domain = if domain == Domain::Board && label.to_uppercase().contains("CPU") {
                    Domain::Cpu
                } else {
                    domain
                };
                let limits = hwmon_meta.and_then(|m| m.limits.get(chip)?.get(kind)?.get(label));
                let limit = limits
                    .and_then(|l| l.get("crit").or_else(|| l.get("max")).copied())
                    .filter(|&l| kind == "temp" && plausible_temp(l) && l > 30.);
                out.push(Sensor {
                    domain,
                    chip: chip.clone(),
                    label: label.clone(),
                    path: format!("/hwmon/{}/{kind}/{}", escape(chip), escape(label)),
                    value,
                    limit,
                });
            }
        }
    }
    for (pci, dev) in sample.gpu.iter().flat_map(|g| &g.devices) {
        let covered = hwmon.is_some_and(|h| h.keys().any(|c| c.ends_with(pci.as_str())));
        if let (Some(t), false) = (dev.temp_c, covered) {
            out.push(Sensor {
                domain: Domain::Graphics,
                chip: format!("{}/{pci}", dev.driver),
                label: "GPU".into(),
                path: format!("/gpu/devices/{}/temp_c", escape(pci)),
                value: t,
                limit: None,
            });
        }
    }
    out.sort_by(|a, b| a.domain.cmp(&b.domain).then(b.value.total_cmp(&a.value)));
    out
}

/// 0 at room temperature, 1 at the sensor's limit (100 °C without one).
pub fn heat(value: f64, limit: Option<f64>) -> f64 {
    let top = limit.unwrap_or(100.).max(40.);
    ((value - 25.) / (top - 25.)).clamp(0., 1.)
}

/// A round axis top that leaves the line in the lower four fifths: 60, 80, 100, 120…
pub fn temp_axis_top(max: f64) -> f64 {
    let mut top = 60.;
    while max > top * 0.8 {
        top += 20.;
    }
    top
}

pub fn fan_axis_top(max: f64) -> f64 {
    let mut top = 2000.;
    while max > top * 0.8 {
        top += 500.;
    }
    top
}

// ---------- sections ----------

fn module<T: serde::de::DeserializeOwned>(meta: Option<&Meta>, name: &str) -> Option<T> {
    serde_json::from_value(meta?.modules.get(name)?.info.clone()?).ok()
}

pub fn gib(bytes: u64) -> String {
    let g = bytes as f64 / (1u64 << 30) as f64;
    if g >= 1000. {
        format!("{:.1} TB", g / 1024.)
    } else if g >= 100. {
        format!("{g:.0} GB")
    } else {
        format!("{g:.1} GB")
    }
}

fn size_label(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1000. {
        format!("{:.1} TB", gb / 1000.)
    } else {
        format!("{gb:.0} GB")
    }
}

/// A short machine name: vendor and product, or the board when the product is a placeholder.
pub fn product_title(dmi: &BTreeMap<String, String>) -> Option<String> {
    let get = |k: &str| dmi.get(k).and_then(|v| meaningful(v));
    let vendor = get("sys_vendor").or_else(|| get("board_vendor"));
    let model = get("product_name").or_else(|| get("board_name"))?;
    Some(match vendor {
        Some(v) if !model.to_lowercase().starts_with(&v.to_lowercase()) => format!("{v} {model}"),
        _ => model.to_owned(),
    })
}

pub fn clock_time(t: u64, pattern: &str) -> String {
    jiff::Timestamp::from_second(t as i64)
        .map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()).strftime(pattern).to_string())
        .unwrap_or_default()
}

pub fn uptime(seconds: u64) -> String {
    let (d, h, m) = (seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h {m}m"),
    }
}

/// Logical CPUs per physical core.
pub fn threads_per_core(cpu: &CpuMeta) -> u32 {
    cpu.topology
        .cores
        .filter(|c| *c > 0)
        .map_or(1, |c| (cpu.topology.threads / c).max(1))
}

/// Every category's nodes, from the sysfs read plus the collector's metadata and latest sample.
pub fn sections(inv: &Inventory, meta: Option<&Meta>, sample: Option<&Sample>) -> Vec<Section> {
    let cpu: Option<CpuMeta> = module(meta, "cpu");
    let system: Option<SystemMeta> = module(meta, "system");
    let storage: Option<StorageMeta> = module(meta, "storage");
    let gpu: Option<GpuMeta> = module(meta, "gpu");
    let hwmon: Option<HwmonMeta> = module(meta, "hwmon");
    let mut out = Vec::new();
    let mut push = |category, note: Option<&str>, nodes: Vec<Node>| {
        out.push(Section {
            category,
            note: note.map(str::to_owned),
            nodes,
        })
    };
    let dmi = |k: &str| inv.dmi.get(k).and_then(|v| meaningful(v)).map(str::to_owned);

    let mut system_nodes = vec![
        Node::new(product_title(&inv.dmi).unwrap_or_else(|| "This computer".into()))
            .sub(dmi("chassis_type").and_then(|c| c.parse().ok()).map(chassis))
            .prop("Manufacturer", dmi("sys_vendor"))
            .prop("Product", dmi("product_name"))
            .prop("Version", dmi("product_version"))
            .prop("Family", dmi("product_family"))
            .prop("SKU", dmi("product_sku"))
            .prop("Chassis", dmi("chassis_type").and_then(|c| c.parse().ok()).map(chassis))
            .prop("Host name", system.as_ref().and_then(|s| s.host.clone()))
            .prop("Architecture", system.as_ref().map(|s| s.arch.clone())),
        Node::new("Motherboard")
            .sub(dmi("board_name"))
            .prop("Manufacturer", dmi("board_vendor"))
            .prop("Model", dmi("board_name"))
            .prop("Revision", dmi("board_version")),
        Node::new("Firmware")
            .sub(dmi("bios_version").map(|v| format!("Version {v}")))
            .prop("Vendor", dmi("bios_vendor"))
            .prop("Version", dmi("bios_version"))
            .prop("Release", dmi("bios_release"))
            .prop("Date", dmi("bios_date"))
            .prop("Interface", Some(if inv.uefi { "UEFI" } else { "Legacy BIOS" })),
    ];
    system_nodes.retain(|n| !n.props.is_empty());
    push(Category::System, None, system_nodes);

    let mut processor = Vec::new();
    if let Some(c) = &cpu {
        let t = &c.topology;
        processor.push(
            Node::new(c.model.clone().unwrap_or_else(|| "Processor".into()))
                .item(Item::Cpu)
                .sub(t.cores.map(|n| format!("{n} cores, {} threads", t.threads)))
                .prop("Vendor", c.vendor.clone())
                .prop("Packages", t.packages.map(|p| p.to_string()))
                .prop("Cores", t.cores.map(|p| p.to_string()))
                .prop("Threads", Some(t.threads.to_string()))
                .prop(
                    "Frequency range",
                    c.min_mhz.zip(c.max_mhz).map(|(a, b)| format!("{a}–{b} MHz")),
                )
                .prop("Scaling driver", c.driver.clone())
                .prop("Governor", c.governor.clone())
                .prop("Energy preference", c.energy_performance_preference.clone())
                .prop("Temperature sensor", c.temp_source.clone())
                .prop(
                    "Core types",
                    t.core_types.as_ref().map(|ct| {
                        format!(
                            "{} performance, {} efficiency threads",
                            ct.performance.len(),
                            ct.efficiency.len()
                        )
                    }),
                ),
        );
        let tpc = threads_per_core(c);
        for (i, g) in t.l3_groups.iter().enumerate() {
            processor.push(
                Node::new(format!("CCD {i}"))
                    .item(Item::Ccd(i))
                    .sub(Some(format!(
                        "{} cores, {} MB L3",
                        g.cpus.len() as u32 / tpc,
                        g.l3_kib / 1024
                    )))
                    .prop("Cores", Some((g.cpus.len() as u32 / tpc).to_string()))
                    .prop("Logical CPUs", Some(cpu_list(&g.cpus)))
                    .prop("L3 cache", Some(format!("{} MB", g.l3_kib / 1024))),
            );
        }
    }
    push(Category::Processor, None, processor);

    let graphics = gpu
        .iter()
        .flat_map(|g| &g.devices)
        .map(|(slot, d)| {
            let pci = inv.pci.iter().find(|p| &p.slot == slot);
            let live = sample.and_then(|s| s.gpu.as_ref()?.devices.get(slot));
            Node::new(gpu_name(d.name.as_deref(), pci).unwrap_or_else(|| slot.clone()))
                .sub(pci.and_then(|p| p.vendor.clone()))
                .item(Item::Gpu(slot.clone()))
                .prop("PCI slot", Some(slot.clone()))
                .prop("PCI ID", Some(format!("{}:{}", d.vendor_id, d.device_id)))
                .prop("Driver", live.map(|l| l.driver.clone()))
                .prop("Backend", live.map(|l| l.backend.clone()))
                .prop("Video memory", live.and_then(|l| l.vram_total_bytes).map(gib))
                .prop(
                    "PCIe maximum",
                    d.pcie_max_gen
                        .zip(d.pcie_max_width)
                        .map(|(g, w)| format!("Gen {g} x{w}")),
                )
                .prop("PCIe link", pci.and_then(|p| p.link.clone()))
                .prop(
                    "Power limit range",
                    d.power_limit_min_w
                        .zip(d.power_limit_max_w)
                        .map(|(a, b)| format!("{a:.0}–{b:.0} W")),
                )
        })
        .collect();
    push(Category::Graphics, None, graphics);

    let mem = sample.and_then(|s| s.memory.as_ref());
    let mut memory = vec![
        Node::new("System memory")
            .item(Item::Memory)
            .sub(mem.map(|m| format!("{} GB installed", installed_gib(m.total_bytes))))
            .prop("Usable", mem.map(|m| gib(m.total_bytes)))
            .prop("Available", mem.map(|m| gib(m.available_bytes)))
            .prop(
                "Swap",
                mem.filter(|m| m.swap_total_bytes > 0).map(|m| gib(m.swap_total_bytes)),
            )
            .prop(
                "Compressed swap (zram)",
                mem.filter(|m| !m.zram.is_empty())
                    .map(|m| m.zram.keys().cloned().collect::<Vec<_>>().join(", ")),
            ),
    ];
    let dimms = sensors(sample, hwmon.as_ref());
    let mut modules: Vec<&Sensor> = dimms.iter().filter(|s| s.domain == Domain::Memory).collect();
    modules.sort_by(|a, b| a.chip.cmp(&b.chip));
    for s in modules {
        let address = s.chip.split('/').nth(1).unwrap_or(&s.chip);
        memory.push(
            Node::new(format!("Module at {address}"))
                .item(Item::Dimm(s.chip.clone()))
                .sub(Some(crate::charts::format::temperature(s.value, 1)))
                .prop("Temperature", Some(crate::charts::format::temperature(s.value, 1)))
                .prop("Sensor", Some(s.chip.clone())),
        );
    }
    push(
        Category::Memory,
        Some("Module details (size, speed, part number) need administrator access."),
        memory,
    );

    let displays = inv
        .displays
        .iter()
        .map(|d| {
            Node::new(d.name.clone())
                .item(Item::Display(format!("{}-{}", d.card, d.connector)))
                .sub(Some(format!(
                    "{}, {}",
                    d.connector,
                    if d.builtin { "built-in" } else { "external" }
                )))
                .prop("Manufacturer", d.vendor.clone())
                .prop("Connector", Some(format!("{} on {}", d.connector, d.card)))
                .prop("Preferred mode", display_mode(d))
                .prop(
                    "Size",
                    d.size_mm
                        .map(|(w, h)| format!("{w} × {h} mm ({:.1}″)", diagonal_in(w, h))),
                )
                .prop("Year", d.year.map(|y| y.to_string()))
                .prop("Modes offered", Some(d.modes.to_string()))
        })
        .collect();
    push(Category::Displays, None, displays);

    let mut drives: Vec<Node> = storage
        .iter()
        .flat_map(|s| &s.devices)
        .map(|(name, d)| {
            Node::new(d.model.clone().unwrap_or_else(|| name.clone()))
                .item(Item::Drive(name.clone()))
                .sub(Some(format!(
                    "{name}{}",
                    d.size_bytes.map(|b| format!(", {}", size_label(b))).unwrap_or_default()
                )))
                .prop("Device", Some(format!("/dev/{name}")))
                .prop("Capacity", d.size_bytes.map(size_label))
                .prop("Kind", Some(if d.rotational { "Hard disk" } else { "Solid state" }))
                .prop("Serial number", d.serial.clone())
                .prop("Firmware", d.firmware.clone())
                .prop(
                    "State",
                    sample.and_then(|s| s.storage.as_ref()?.devices.get(name)?.state.clone()),
                )
        })
        .collect();
    for (mount, f) in sample.and_then(|s| s.fs.as_ref()).into_iter().flatten() {
        drives.push(
            Node::new(mount.clone())
                .item(Item::Volume(mount.clone()))
                .sub(Some(format!("{}, {} free", f.fstype, gib(f.avail_bytes))))
                .prop("Device", Some(f.device.clone()))
                .prop("File system", Some(f.fstype.clone()))
                .prop("Size", Some(gib(f.total_bytes)))
                .prop("Used", Some(gib(f.used_bytes)))
                .prop("Free", Some(gib(f.avail_bytes)))
                .prop("Mounted at", Some(f.mounts.join(", "))),
        );
    }
    push(Category::Storage, storage.as_ref().map(|s| s.note.as_str()), drives);

    let supplies = inv
        .supplies
        .iter()
        .map(|s| {
            let title = match (s.get("MANUFACTURER"), s.get("MODEL_NAME")) {
                (Some(m), Some(n)) => format!("{m} {n}"),
                (m, n) => n.or(m).unwrap_or(&s.name).to_owned(),
            };
            let kind = match (s.get("TYPE"), s.get("SCOPE")) {
                (Some("Battery"), Some("Device")) => "Device battery".to_owned(),
                (Some("Battery"), _) => "System battery".to_owned(),
                (Some(t), _) => t.to_owned(),
                _ => "Supply".to_owned(),
            };
            Node::new(title)
                .item(Item::Supply(s.name.clone()))
                .sub(Some(kind))
                .prop("Name", Some(s.name.clone()))
                .prop("Status", s.get("STATUS"))
                .prop("Charge", s.get("CAPACITY").map(|c| format!("{c}%")))
                .prop("Level", s.get("CAPACITY_LEVEL"))
                .prop("Health", s.health_pct().map(|h| format!("{h:.0}% of design capacity")))
                .prop("Cycle count", s.get("CYCLE_COUNT"))
                .prop("Technology", s.get("TECHNOLOGY"))
                .prop("Online", s.get("ONLINE").map(|o| if o == "1" { "Yes" } else { "No" }))
        })
        .collect();
    push(Category::Power, None, supplies);

    let all_sensors = sensors(sample, hwmon.as_ref());
    let mut chips: BTreeMap<&str, Vec<&Sensor>> = BTreeMap::new();
    for s in &all_sensors {
        chips.entry(&s.chip).or_default().push(s);
    }
    let sensor_nodes = chips
        .into_iter()
        .map(|(chip, list)| {
            // A GPU read through its own library has no hwmon chip; its readings live on the GPU's page.
            let in_hwmon = sample
                .and_then(|s| s.hwmon.as_ref())
                .is_some_and(|h| h.contains_key(chip));
            let item = match chip.split_once('/') {
                Some((_, slot)) if !in_hwmon && list[0].domain == Domain::Graphics => Item::Gpu(slot.to_owned()),
                _ => Item::Chip(chip.to_owned()),
            };
            let mut node = Node::new(chip.to_owned()).item(item).sub(Some(list[0].domain.title()));
            for s in list {
                let value = if s.domain == Domain::Fans {
                    format!("{:.0} rpm", s.value)
                } else {
                    let limit = s
                        .limit
                        .map(|l| format!(" (limit {})", crate::charts::format::temperature(l, 0)))
                        .unwrap_or_default();
                    format!("{}{limit}", crate::charts::format::temperature(s.value, 1))
                };
                node.props.push((s.label.clone(), value));
            }
            node
        })
        .collect();
    push(Category::Sensors, hwmon.as_ref().map(|h| h.note.as_str()), sensor_nodes);

    let netinfo = sample.and_then(|s| s.netinfo.as_ref());
    let net_node = |n: &NetDevice| {
        let addresses = netinfo
            .and_then(|ni| ni.interfaces.get(&n.name))
            .map(|i| i.addresses.join(", "));
        Node::new(n.name.clone())
            .item(Item::Net(n.name.clone()))
            .sub(Some(
                [n.driver.clone(), n.state.clone()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", "),
            ))
            .prop("Driver", n.driver.clone())
            .prop("Bus", n.bus.clone())
            .prop("State", n.state.clone())
            .prop("Link speed", n.speed_mbps.map(|s| format!("{s} Mb/s")))
            .prop("Signal", n.signal_dbm.map(|s| format!("{s:.0} dBm")))
            .prop("MAC address", n.mac.clone())
            .prop("MTU", n.mtu.clone())
            .prop("Addresses", addresses)
    };
    push(Category::Network, None, inv.nets.iter().map(net_node).collect());
    let wifi: Vec<Node> = inv.nets.iter().filter(|n| n.wireless).map(net_node).collect();
    if !wifi.is_empty() {
        push(Category::Wifi, None, wifi);
    }

    let bluetooth = inv
        .bluetooth
        .iter()
        .map(|b| {
            Node::new(b.chip.clone().unwrap_or_else(|| b.name.clone()))
                .sub(Some(b.name.clone()))
                .prop("Adapter", Some(b.name.clone()))
                .prop("Bus", b.bus.clone())
                .prop("Radio", b.radio.clone())
                .prop("Connected devices", Some(b.connections.to_string()))
        })
        .collect();
    push(Category::Bluetooth, None, bluetooth);

    let usb = inv
        .usb
        .iter()
        .map(|u| {
            Node::new(u.title())
                .sub(u.manufacturer.clone())
                .prop("Port", Some(u.port.clone()))
                .prop("Bus and device", Some(format!("{:03}:{:03}", u.bus, u.dev)))
                .prop("ID", Some(format!("{:04x}:{:04x}", u.vendor_id, u.product_id)))
                .prop("Manufacturer", u.manufacturer.clone())
                .prop("Speed", u.speed_mbps.as_ref().map(|s| format!("{s} Mb/s")))
                .prop("USB version", u.version.clone())
                .prop("Class", usb_class(u.class))
                .prop("Drivers", (!u.drivers.is_empty()).then(|| u.drivers.join(", ")))
                .prop("Power", u.power.clone())
                .item(Item::Usb(u.port.clone()))
        })
        .collect();
    push(Category::Usb, None, usb);

    let audio = inv
        .sound
        .iter()
        .map(|c| {
            Node::new(c.name.clone())
                .sub(Some(c.driver.clone()))
                .prop("Card", Some(format!("{} ({})", c.index, c.id)))
                .prop("Driver", Some(c.driver.clone()))
                .prop("Description", Some(c.long_name.clone()))
        })
        .collect();
    push(Category::Audio, None, audio);

    let pci = inv
        .pci
        .iter()
        .map(|p| {
            Node::new(
                p.device
                    .clone()
                    .unwrap_or_else(|| format!("Device {:04x}", p.device_id)),
            )
            .sub(Some(p.slot.clone()))
            .prop("Slot", Some(p.slot.clone()))
            .prop("Class", p.class_name.clone())
            .prop("Vendor", p.vendor.clone())
            .prop("ID", Some(format!("{:04x}:{:04x}", p.vendor_id, p.device_id)))
            .prop("Subsystem", p.subsystem.clone())
            .prop("Driver", p.driver.clone())
            .prop("Revision", p.revision.clone())
            .prop("Link", p.link.clone())
            .prop("Maximum link", p.max_link.clone())
            .prop("Power state", p.power_state.clone())
            .prop("Upstream", p.parent.clone())
            .item(Item::Pci(p.slot.clone()))
        })
        .collect();
    push(Category::Pci, None, pci);

    let inputs = inv
        .inputs
        .iter()
        .map(|d| {
            Node::new(d.name.clone())
                .sub(Some(format!("{}, {}", d.kind(), bus_name(d.bus))))
                .prop("Kind", Some(d.kind()))
                .prop("Bus", Some(bus_name(d.bus)))
                .prop(
                    "ID",
                    (d.vendor != 0 || d.product != 0).then(|| format!("{:04x}:{:04x}", d.vendor, d.product)),
                )
                .prop("Physical path", Some(d.phys.clone()))
                .prop("Handlers", Some(d.handlers.join(" ")))
        })
        .collect();
    push(Category::Input, None, inputs);

    let security = vec![
        Node::new("Boot")
            .sub(Some(secure_boot_label(inv)))
            .prop(
                "Firmware interface",
                Some(if inv.uefi { "UEFI" } else { "Legacy BIOS" }),
            )
            .prop("Secure Boot", Some(secure_boot_label(inv)))
            .prop("Setup mode", inv.setup_mode.map(|s| if s { "On" } else { "Off" }))
            .prop("Kernel lockdown", inv.lockdown.clone()),
        Node::new("Trusted Platform Module")
            .sub(Some(inv.tpm.clone().unwrap_or_else(|| "Not present".into())))
            .prop("Version", Some(inv.tpm.clone().unwrap_or_else(|| "Not present".into()))),
    ];
    push(Category::Security, None, security);

    let now = now_s() as u64;
    let software = vec![
        Node::new(
            system
                .as_ref()
                .and_then(|s| s.distro.clone())
                .or_else(|| inv.os.clone())
                .unwrap_or_else(|| "Linux".into()),
        )
        .sub(system.as_ref().and_then(|s| s.kernel.clone()))
        .prop("Operating system", inv.os.clone())
        .prop("Kernel", inv.kernel.clone())
        .prop("Desktop", inv.desktop.clone())
        .prop("Last boot", inv.boot_time.map(|b| clock_time(b, "%-d %b %Y at %H:%M")))
        .prop("Uptime", inv.boot_time.map(|b| uptime(now.saturating_sub(b))))
        .prop("Secure Boot", Some(secure_boot_label(inv))),
    ];
    push(Category::Software, None, software);
    out
}

/// The collector's GPU name, unless it is only a fallback of hex ids and `pci.ids` knows better.
pub fn gpu_name(collector: Option<&str>, pci: Option<&PciDevice>) -> Option<String> {
    let known = pci.and_then(|p| p.device.as_deref());
    match (collector, known) {
        (Some(name), Some(device)) if name.contains("(0x") => Some(device.to_owned()),
        (Some(name), _) => Some(name.to_owned()),
        (None, device) => device.map(str::to_owned),
    }
}

/// `/proc/version` without the builder and toolchain: release and build stamp.
pub fn kernel_summary(version: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in version.strip_prefix("Linux version ").unwrap_or(version).chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn secure_boot_label(inv: &Inventory) -> String {
    match (inv.uefi, inv.secure_boot) {
        (false, _) => "Not available (legacy BIOS)".into(),
        (true, Some(true)) => "Enabled".into(),
        (true, Some(false)) => "Disabled".into(),
        (true, None) => "Not supported".into(),
    }
}

pub fn display_mode(d: &Display) -> Option<String> {
    (d.width > 0).then(|| match d.refresh {
        Some(r) => format!("{} × {} @ {r:.2} Hz", d.width, d.height),
        None => format!("{} × {}", d.width, d.height),
    })
}

pub fn usb_class(class: u8) -> Option<String> {
    let name = match class {
        0x00 => "Per interface",
        0x01 => "Audio",
        0x02 => "Communications",
        0x03 => "Human interface",
        0x08 => "Mass storage",
        0x09 => "Hub",
        0x0E => "Video",
        0xE0 => "Wireless",
        0xEF => "Miscellaneous",
        0xFF => "Vendor specific",
        _ => return None,
    };
    Some(name.to_owned())
}

/// `0-7,16-23` from a sorted list of CPU numbers.
pub fn cpu_list(cpus: &[u32]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        while i + 1 < sorted.len() && sorted[i + 1] == sorted[i] + 1 {
            i += 1;
        }
        parts.push(if sorted[i] == start {
            start.to_string()
        } else {
            format!("{start}-{}", sorted[i])
        });
        i += 1;
    }
    parts.join(",")
}

// ---------- reports ----------

pub fn markdown(title: &str, sections: &[Section], at: f64) -> String {
    let mut out = format!(
        "# Hardware report: {title}\n\nRead {}.\n",
        clock_time(at as u64, "%Y-%m-%d %H:%M:%S")
    );
    for s in sections {
        out += &format!("\n## {}\n", s.category.title());
        if let Some(note) = &s.note {
            out += &format!("\n_{note}_\n");
        }
        for n in &s.nodes {
            out += &format!("\n### {}\n", n.title.replace('\n', " "));
            if let Some(sub) = &n.subtitle {
                out += &format!("\n{sub}\n");
            }
            if !n.props.is_empty() {
                out += "\n| Property | Value |\n| --- | --- |\n";
                for (l, v) in &n.props {
                    out += &format!("| {} | {} |\n", cell(l), cell(v));
                }
            }
        }
    }
    out
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

pub fn json(title: &str, sections: &[Section], at: f64) -> String {
    let doc = serde_json::json!({ "title": title, "read_at": at, "sections": sections });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PCI_IDS: &str = "\
# comment line
10de  NVIDIA Corporation
\t2684  AD102 [GeForce RTX 4090]
\t\t10de 167c  GeForce RTX 4090
1022  Advanced Micro Devices, Inc. [AMD]
\t14d8  Raphael/Granite Ridge Root Complex
C 03  Display controller
\t00  VGA compatible controller
\t\t00  VGA controller
C 0c  Serial bus controller
\t03  USB controller
";

    #[test]
    fn pci_ids_resolve_vendor_device_and_class() {
        let ids = Ids::parse(PCI_IDS);
        assert_eq!(ids.vendor(0x10de), Some("NVIDIA Corporation"));
        assert_eq!(ids.device(0x10de, 0x2684), Some("AD102 [GeForce RTX 4090]"));
        assert_eq!(ids.device(0x1022, 0x14d8), Some("Raphael/Granite Ridge Root Complex"));
        assert_eq!(ids.class(0x03, 0x00), Some("VGA compatible controller"));
        assert_eq!(
            ids.class(0x03, 0x80),
            Some("Display controller"),
            "unknown subclass falls back to the class"
        );
        assert_eq!(ids.device(0x10de, 0x167c), None, "subsystem lines are not devices");
    }

    #[test]
    fn usb_ids_stop_attributing_devices_after_the_vendor_list() {
        let text = "\
1038  SteelSeries ApS
\t22a1  Arctis Nova 7
C 03  Human Interface Device
\t01  Boot Interface Subclass
AT 0001  Microphone
\t0002  not a device
";
        let ids = Ids::parse(text);
        assert_eq!(ids.device(0x1038, 0x22a1), Some("Arctis Nova 7"));
        assert_eq!(ids.class(0x03, 0x01), Some("Boot Interface Subclass"));
        assert_eq!(
            ids.device(0x1038, 0x0002),
            None,
            "lines under an unknown section must not join the last vendor"
        );
    }

    #[test]
    fn pnp_ids_name_the_monitor_maker() {
        let text = "AAA\tAvolites Ltd\nBNQ\tBenQ Corporation\n";
        assert_eq!(pnp_name(text, "BNQ").as_deref(), Some("BenQ Corporation"));
        assert_eq!(pnp_name(text, "XYZ"), None);
    }

    fn edid_fixture() -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[..8].copy_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
        // "BNQ": 2, 14, 17 in five-bit letters.
        b[8..10].copy_from_slice(&[0x09, 0xD1]);
        b[10..12].copy_from_slice(&0x7F4Bu16.to_le_bytes());
        b[17] = 32;
        b[21] = 70;
        b[22] = 39;
        // 3840x2160 at 533.25 MHz with 160/62 blanking: 60 Hz, 697 x 392 mm.
        let dtd = &mut b[54..72];
        dtd[..2].copy_from_slice(&53325u16.to_le_bytes());
        dtd[2] = 0x00;
        dtd[3] = 0xA0;
        dtd[4] = 0xF0;
        dtd[5] = 0x70;
        dtd[6] = 0x3E;
        dtd[7] = 0x80;
        dtd[12] = 0xB9;
        dtd[13] = 0x88;
        dtd[14] = 0x21;
        let name = &mut b[72..90];
        name[3] = 0xFC;
        name[5..18].copy_from_slice(b"BenQ PD3220U\n");
        let serial = &mut b[90..108];
        serial[3] = 0xFF;
        serial[5..18].copy_from_slice(b"ET1234\n      ");
        b
    }

    #[test]
    fn edid_gives_name_size_and_preferred_mode() {
        let e = parse_edid(&edid_fixture()).unwrap();
        assert_eq!(e.manufacturer, "BNQ");
        assert_eq!(e.product, 0x7F4B);
        assert_eq!(e.name.as_deref(), Some("BenQ PD3220U"));
        assert_eq!(e.serial.as_deref(), Some("ET1234"));
        assert_eq!(e.year, Some(2022));
        let (w, h, hz) = e.mode.unwrap();
        assert_eq!((w, h), (3840, 2160));
        assert!((hz - 60.).abs() < 0.01, "{hz}");
        assert_eq!(
            e.size_mm,
            Some((697, 392)),
            "the timing's millimetres beat the header's centimetres"
        );
        assert!((diagonal_in(697, 392) - 31.5).abs() < 0.1);
    }

    #[test]
    fn edid_without_a_header_is_rejected() {
        assert_eq!(parse_edid(&[0u8; 128]), None);
        assert_eq!(parse_edid(&edid_fixture()[..100]), None);
    }

    #[test]
    fn asound_cards_pair_each_header_with_its_long_name() {
        let text = " 0 [A7             ]: USB-Audio - Arctis Nova 7
                      SteelSeries Arctis Nova 7 at usb-0000:0e:00.0-1, full speed
 1 [NVidia         ]: HDA-Intel - HDA NVidia
                      HDA NVidia at 0xde080000 irq 170
10 [Generic_1      ]: HDA-Intel - HD-Audio Generic
                      HD-Audio Generic at 0xdf980000 irq 173
";
        let cards = parse_asound(text);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].id, "A7");
        assert_eq!(cards[0].driver, "USB-Audio");
        assert_eq!(cards[0].name, "Arctis Nova 7");
        assert!(cards[0].long_name.starts_with("SteelSeries Arctis Nova 7 at usb"));
        assert_eq!(cards[2].index, 10, "two-digit card numbers have no leading space");
        assert_eq!(cards[2].id, "Generic_1");
        assert_eq!(parse_asound("--- no soundcards ---\n"), Vec::new());
    }

    #[test]
    fn input_devices_are_classified_from_handlers_and_key_bits() {
        let text = "I: Bus=0019 Vendor=0000 Product=0001 Version=0000
N: Name=\"Power Button\"
P: Phys=LNXPWRBN/button/input0
H: Handlers=kbd event1
B: KEY=8000 10000000000000 0

I: Bus=0003 Vendor=046d Product=c548 Version=0111
N: Name=\"Logitech USB Receiver\"
P: Phys=usb-0000:0e:00.0-2/input0
H: Handlers=sysrq kbd leds event4
B: KEY=1000000000007 ff9f207ac14057ff febeffdfffefffff fffffffffffffffe

I: Bus=0005 Vendor=046d Product=b034 Version=0012
N: Name=\"MX Master 3 Mouse\"
H: Handlers=mouse0 event5
B: KEY=1f0000 0 0 0 0

I: Bus=0003 Vendor=054c Product=0ce6 Version=8111
N: Name=\"DualSense Wireless Controller\"
H: Handlers=event6 js0
";
        let devices = parse_input_devices(text);
        let kinds: Vec<&str> = devices.iter().map(InputDevice::kind).collect();
        assert_eq!(kinds, ["Buttons", "Keyboard", "Pointer", "Game controller"]);
        assert_eq!(devices[1].vendor, 0x046d);
        assert_eq!(devices[1].product, 0xc548);
        assert_eq!(bus_name(devices[2].bus), "Bluetooth");
        assert_eq!(devices[0].phys, "LNXPWRBN/button/input0");
    }

    #[test]
    fn secure_boot_reads_the_value_after_the_attribute_bytes() {
        assert_eq!(parse_efivar_bool(&[0x06, 0, 0, 0, 1]), Some(true));
        assert_eq!(parse_efivar_bool(&[0x06, 0, 0, 0, 0]), Some(false));
        assert_eq!(parse_efivar_bool(&[0x06, 0, 0, 0]), None, "attributes without data");
    }

    #[test]
    fn placeholder_dmi_falls_back_to_the_board() {
        let dmi: BTreeMap<String, String> = [
            ("sys_vendor", "ASUS"),
            ("product_name", "System Product Name"),
            ("board_vendor", "ASUSTeK COMPUTER INC."),
            ("board_name", "ProArt X870E-CREATOR WIFI"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(product_title(&dmi).as_deref(), Some("ASUS ProArt X870E-CREATOR WIFI"));
        let laptop: BTreeMap<String, String> = [("sys_vendor", "LENOVO"), ("product_name", "LENOVO 21K5")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(
            product_title(&laptop).as_deref(),
            Some("LENOVO 21K5"),
            "vendor not repeated"
        );
    }

    #[test]
    fn installed_memory_rounds_memtotal_up_to_a_module_size() {
        let gib = |g: f64| (g * (1u64 << 30) as f64) as u64;
        assert_eq!(installed_gib(gib(125.1)), 128);
        assert_eq!(installed_gib(gib(62.6)), 64);
        assert_eq!(installed_gib(gib(46.9)), 48);
        assert_eq!(installed_gib(gib(15.4)), 16);
        assert_eq!(installed_gib(gib(7.6)), 8);
        assert_eq!(super::gib(1_999_000_000_000), "1.8 TB");
        assert_eq!(super::gib(1 << 30), "1.0 GB");
    }

    #[test]
    fn chips_fall_into_sensor_domains() {
        assert_eq!(domain_of("k10temp/0000:00:18.3"), Domain::Cpu);
        assert_eq!(domain_of("amdgpu/0000:7a:00.0"), Domain::Graphics);
        assert_eq!(domain_of("spd5118/7-0050"), Domain::Memory);
        assert_eq!(domain_of("nvme/nvme0"), Domain::Storage);
        assert_eq!(domain_of("asusec/asus-ec-sensors"), Domain::Board);
        assert_eq!(domain_of("mt7925_phy0/phy0"), Domain::Wireless);
        assert_eq!(domain_of("enp12s0/0000:0c:00.0"), Domain::Ethernet);
        assert_eq!(domain_of("prom21_xhci/0000:0e:00.0"), Domain::Usb);
        assert_eq!(domain_of("hidpp_battery_0/x"), Domain::Other);
    }

    #[test]
    fn sensors_drop_disconnected_probes_and_keep_believable_limits() {
        let sample: Sample = serde_json::from_value(serde_json::json!({
            "type": "sample", "schema": 3, "t": 1.0, "time": "x",
            "hwmon": {
                "asusec/asus-ec-sensors": {"temp": {"T_Sensor": -61.0, "VRM": 55.0, "CPU Package": 66.0},
                                           "fan": {"CPU_Opt": 0.0}},
                "nvme/nvme0": {"temp": {"Composite": 37.85, "Sensor 1": 42.0}},
                "amdgpu/0000:7a:00.0": {"temp": {"edge": 52.0}}
            },
            "gpu": {"devices": {
                "0000:7a:00.0": {"vendor": "amd", "driver": "amdgpu", "backend": "sysfs", "temp_c": 52.0},
                "0000:01:00.0": {"vendor": "nvidia", "driver": "nvidia", "backend": "nvml", "temp_c": 44.0}
            }}
        }))
        .unwrap();
        let meta: HwmonMeta = serde_json::from_value(serde_json::json!({
            "units": {}, "note": "",
            "limits": {"nvme/nvme0": {"temp": {"Composite": {"crit": 84.85, "max": 81.85},
                                               "Sensor 1": {"max": 65261.85}}}}
        }))
        .unwrap();
        let list = sensors(Some(&sample), Some(&meta));
        assert!(
            !list.iter().any(|s| s.label == "T_Sensor"),
            "a -61 °C probe is unplugged"
        );
        let storage: Vec<&Sensor> = list.iter().filter(|s| s.domain == Domain::Storage).collect();
        assert_eq!(storage[0].label, "Sensor 1", "hottest first");
        assert_eq!(storage[0].limit, None, "65261 °C is an unset limit");
        assert_eq!(storage[1].limit, Some(84.85), "crit wins over max");
        let gpus: Vec<&Sensor> = list.iter().filter(|s| s.domain == Domain::Graphics).collect();
        assert_eq!(
            gpus.len(),
            2,
            "the AMD GPU appears once, through hwmon; NVIDIA through the GPU module"
        );
        assert_eq!(gpus[1].path, "/gpu/devices/0000:01:00.0/temp_c");
        assert_eq!(storage[0].path, "/hwmon/nvme~1nvme0/temp/Sensor 1");
        assert!(list.iter().any(|s| s.domain == Domain::Fans && s.label == "CPU_Opt"));
        let package = list.iter().find(|s| s.label == "CPU Package").unwrap();
        assert_eq!(
            package.domain,
            Domain::Cpu,
            "the board's CPU reading joins the CPU chart"
        );
        assert_eq!(list.iter().find(|s| s.label == "VRM").unwrap().domain, Domain::Board);
    }

    #[test]
    fn heat_runs_from_room_temperature_to_the_limit() {
        assert_eq!(heat(20., None), 0.);
        assert_eq!(heat(100., None), 1.);
        assert!((heat(55., Some(85.)) - 0.5).abs() < 1e-9);
        assert_eq!(heat(90., Some(85.)), 1.);
    }

    #[test]
    fn axis_tops_leave_headroom_above_the_line() {
        assert_eq!(temp_axis_top(39.), 60.);
        assert_eq!(temp_axis_top(50.), 80.);
        assert_eq!(temp_axis_top(68.), 100.);
        assert_eq!(temp_axis_top(73.), 100.);
        assert_eq!(fan_axis_top(0.), 2000.);
        assert_eq!(fan_axis_top(1800.), 2500.);
    }

    #[test]
    fn kernel_summary_drops_builder_and_toolchain() {
        let v = "Linux version 7.2.9-arch1-1 (linux@archlinux) (gcc (GCC) 16.2.1 20260810, GNU ld (GNU Binutils) 2.47) \
                 #1 SMP PREEMPT_DYNAMIC Sat, 03 Oct 2026 11:43:28 +0000";
        assert_eq!(
            kernel_summary(v),
            "7.2.9-arch1-1 #1 SMP PREEMPT_DYNAMIC Sat, 03 Oct 2026 11:43:28 +0000"
        );
    }

    #[test]
    fn every_node_with_a_page_names_the_item_behind_it() {
        let sample: Sample = serde_json::from_value(serde_json::json!({
            "schema": 3, "t": 0.0, "time": "x",
            "hwmon": {"nvme/nvme0": {"temp": {"Composite": 40.0}}},
            "gpu": {"devices": {"0000:01:00.0": {"vendor": "nvidia", "driver": "nvidia", "backend": "nvml", "temp_c": 50.0}}},
            "fs": {"/": {"device": "/dev/nvme0n1p2", "fstype": "btrfs", "mounts": ["/"], "total_bytes": 10,
                          "used_bytes": 5, "avail_bytes": 5, "used_pct": 50.0, "inodes_total": 0, "inodes_free": 0}}
        }))
        .unwrap();
        let meta: Meta = serde_json::from_value(serde_json::json!({
            "schema": 3, "t": 0.0, "time": "x", "config": {}, "failed_modules": {},
            "modules": {"storage": {"interval_s": 60, "info": {"note": "", "devices": {
                "nvme0n1": {"model": "Disk", "serial": null, "firmware": null, "size_bytes": null, "rotational": false}
            }}}}
        }))
        .unwrap();
        let sections = sections(&Inventory::default(), Some(&meta), Some(&sample));
        let items = |c: Category| -> Vec<Option<Item>> {
            sections
                .iter()
                .find(|s| s.category == c)
                .map(|s| s.nodes.iter().map(|n| n.item.clone()).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            items(Category::Storage),
            [Some(Item::Drive("nvme0n1".into())), Some(Item::Volume("/".into()))]
        );
        assert_eq!(
            items(Category::Sensors),
            [
                Some(Item::Gpu("0000:01:00.0".into())),
                Some(Item::Chip("nvme/nvme0".into()))
            ],
            "a GPU read through its own library has no chip page; its sensor opens the GPU"
        );
        assert_eq!(
            items(Category::System),
            [None],
            "pages without live data keep the plain view"
        );
    }

    #[test]
    fn gpu_names_prefer_pci_ids_over_hex_fallbacks() {
        let pci = PciDevice {
            slot: "0000:7a:00.0".into(),
            vendor_id: 0x1002,
            device_id: 0x164e,
            class: 0x030000,
            vendor: None,
            device: Some("Raphael [Radeon 610M]".into()),
            class_name: None,
            subsystem: None,
            driver: None,
            revision: None,
            link: None,
            max_link: None,
            power_state: None,
            parent: None,
        };
        assert_eq!(
            gpu_name(Some("AMD GPU (0x164e)"), Some(&pci)).as_deref(),
            Some("Raphael [Radeon 610M]")
        );
        assert_eq!(
            gpu_name(Some("NVIDIA GeForce RTX 4090"), Some(&pci)).as_deref(),
            Some("NVIDIA GeForce RTX 4090")
        );
        assert_eq!(
            gpu_name(Some("AMD GPU (0x164e)"), None).as_deref(),
            Some("AMD GPU (0x164e)")
        );
    }

    #[test]
    fn cpu_lists_collapse_runs() {
        assert_eq!(cpu_list(&[0, 1, 2, 3, 16, 17, 18, 19]), "0-3,16-19");
        assert_eq!(cpu_list(&[5, 7]), "5,7");
    }

    #[test]
    fn markdown_report_escapes_table_cells() {
        let sections = vec![Section {
            category: Category::Pci,
            note: None,
            nodes: vec![Node::new("Bridge").prop("Class", Some("PCI | bridge"))],
        }];
        let md = markdown("Test", &sections, 0.);
        assert!(md.contains("## PCI"));
        assert!(md.contains("| Class | PCI \\| bridge |"));
    }

    #[test]
    fn empty_values_are_left_out_of_a_node() {
        let n = Node::new("x")
            .prop("A", Some(""))
            .prop("B", None::<String>)
            .prop("C", Some("1"));
        assert_eq!(n.props, vec![("C".to_owned(), "1".to_owned())]);
        assert!(n.matches("x"));
        assert!(!n.matches("zzz"));
    }
}
