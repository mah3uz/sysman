use anyhow::Result;
use schema::power::Supply;
use serde_json::Value;

use crate::collector::{Collector, NoOptions};
use crate::sys::Sys;

pub struct Power {
    sys: Sys,
}

impl Power {
    pub fn new(_: NoOptions, sys: Sys) -> Result<Self> {
        Ok(Self { sys })
    }

    fn supply(&self, name: &str) -> Supply {
        let text = |k: &str| self.sys.read_opt(format!("/sys/class/power_supply/{name}/{k}"));
        let int = |k: &str| text(k).and_then(|v| v.parse::<i64>().ok());
        let micro = |k: &str| int(k).map(|v| v as f64 / 1_000_000.0);
        Supply {
            kind: text("type"),
            status: text("status"),
            model_name: text("model_name"),
            manufacturer: text("manufacturer"),
            capacity_level: text("capacity_level"),
            scope: text("scope"),
            capacity: int("capacity"),
            online: int("online"),
            cycle_count: int("cycle_count"),
            energy_wh: micro("energy_now"),
            energy_full_wh: micro("energy_full"),
            power_w: micro("power_now"),
            voltage_v: micro("voltage_now"),
        }
    }
}

impl Collector for Power {
    fn sample(&mut self) -> Result<Option<Value>> {
        let out: schema::power::Power = self
            .sys
            .list("/sys/class/power_supply")
            .into_iter()
            .map(|name| {
                let supply = self.supply(&name);
                (name, supply)
            })
            .collect();
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    const BASE: &str = "sys/class/power_supply";

    fn laptop() -> Fixture {
        Fixture::new()
            .file(&format!("{BASE}/BAT0/type"), "Battery\n")
            .file(&format!("{BASE}/BAT0/status"), "Discharging\n")
            .file(&format!("{BASE}/BAT0/model_name"), "DELL 8N0YK \n")
            .file(&format!("{BASE}/BAT0/manufacturer"), "SMP\n")
            .file(&format!("{BASE}/BAT0/capacity_level"), "Normal\n")
            .file(&format!("{BASE}/BAT0/capacity"), "79\n")
            .file(&format!("{BASE}/BAT0/cycle_count"), "213\n")
            .file(&format!("{BASE}/BAT0/energy_now"), "41230000\n")
            .file(&format!("{BASE}/BAT0/energy_full"), "52000000\n")
            .file(&format!("{BASE}/BAT0/power_now"), "8500000\n")
            .file(&format!("{BASE}/BAT0/voltage_now"), "12100000\n")
            .file(&format!("{BASE}/AC/type"), "Mains\n")
            .file(&format!("{BASE}/AC/online"), "0\n")
            .file(&format!("{BASE}/hidpp_battery_0/type"), "Battery\n")
            .file(&format!("{BASE}/hidpp_battery_0/status"), "Discharging\n")
            .file(
                &format!("{BASE}/hidpp_battery_0/model_name"),
                "Wireless Mouse MX Master 3\n",
            )
            .file(&format!("{BASE}/hidpp_battery_0/manufacturer"), "Logitech\n")
            .file(&format!("{BASE}/hidpp_battery_0/capacity_level"), "Full\n")
            .file(&format!("{BASE}/hidpp_battery_0/online"), "1\n")
            .file(&format!("{BASE}/hidpp_battery_0/scope"), "Device\n")
            .file(&format!("{BASE}/hidpp_battery_0/serial_number"), "46-59-53-80\n")
    }

    fn sample(f: &Fixture) -> Value {
        let v = Power::new(NoOptions {}, f.sys()).unwrap().sample().unwrap().unwrap();
        assert_schema::<schema::power::Power>(&v);
        v
    }

    #[test]
    fn supplies_are_keyed_by_sysfs_name() {
        let v = sample(&laptop());
        assert_eq!(
            v.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["AC", "BAT0", "hidpp_battery_0"]
        );
    }

    #[test]
    fn peripheral_battery_keeps_scope_device_and_system_battery_has_none() {
        // without scope, a mouse at 40% is indistinguishable from the laptop battery
        let v = sample(&laptop());
        assert_eq!(v["hidpp_battery_0"]["scope"], "Device");
        assert!(v["BAT0"].get("scope").is_none());
    }

    #[test]
    fn laptop_battery_fields_and_unit_conversions() {
        let b = &sample(&laptop())["BAT0"];
        assert_eq!(b["type"], "Battery");
        assert_eq!(b["status"], "Discharging");
        assert_eq!(b["model_name"], "DELL 8N0YK");
        assert_eq!(b["capacity"], 79);
        assert_eq!(b["cycle_count"], 213);
        assert_eq!(b["energy_wh"], 41.23);
        assert_eq!(b["energy_full_wh"], 52.0);
        assert_eq!(b["power_w"], 8.5);
        assert_eq!(b["voltage_v"], 12.1);
    }

    #[test]
    fn fields_a_supply_does_not_expose_are_absent_not_zero() {
        // an AC adapter has no capacity; a 0 would read as an empty battery
        let v = sample(&laptop());
        assert_eq!(v["AC"], serde_json::json!({"type": "Mains", "online": 0}));
        assert!(v["hidpp_battery_0"].get("capacity").is_none());
    }

    #[test]
    fn machine_without_supplies_gives_an_empty_map() {
        assert_eq!(sample(&Fixture::new()), serde_json::json!({}));
    }
}
