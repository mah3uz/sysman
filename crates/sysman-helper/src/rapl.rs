use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use schema::helper::RaplZone;

/// Every powercap zone under `root` whose energy counter can be read.
pub fn read(root: &Path) -> Result<BTreeMap<String, RaplZone>> {
    let dir = root.join("sys/class/powercap");
    let mut zones = BTreeMap::new();
    let mut first_error = None;
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let (Ok(id), path) = (entry.file_name().into_string(), entry.path()) else {
            continue;
        };
        let num = |leaf: &str| -> Result<u64> { Ok(std::fs::read_to_string(path.join(leaf))?.trim().parse()?) };
        match (num("energy_uj"), num("max_energy_range_uj")) {
            (Ok(energy_uj), Ok(max_energy_range_uj)) => {
                let name = std::fs::read_to_string(path.join("name")).unwrap_or_default();
                zones.insert(
                    id,
                    RaplZone {
                        name: name.trim().to_string(),
                        energy_uj,
                        max_energy_range_uj,
                    },
                );
            }
            (Err(e), _) | (_, Err(e)) => {
                // the control-type directory (`intel-rapl`) has no counter and is not a failure
                if path.join("energy_uj").exists() {
                    first_error.get_or_insert(format!("{id}/energy_uj: {e}"));
                }
            }
        }
    }
    if zones.is_empty() {
        match first_error {
            Some(e) => bail!("{e}"),
            None => bail!("no RAPL zones in /sys/class/powercap"),
        }
    }
    Ok(zones)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(root: &Path, id: &str, name: &str, energy: Option<&str>) {
        let d = root.join("sys/class/powercap").join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
        std::fs::write(d.join("max_energy_range_uj"), "65532610987\n").unwrap();
        if let Some(e) = energy {
            std::fs::write(d.join("energy_uj"), e).unwrap();
        }
    }

    #[test]
    fn zones_carry_their_wrap_range() {
        // without the range a reader cannot tell a wrap from a reset
        let root = tempfile::tempdir().unwrap();
        zone(root.path(), "intel-rapl:0", "package-0", Some("123456\n"));
        zone(root.path(), "intel-rapl:0:0", "core", Some("789\n"));
        std::fs::create_dir_all(root.path().join("sys/class/powercap/intel-rapl")).unwrap();
        let z = read(root.path()).unwrap();
        assert_eq!(z.len(), 2);
        assert_eq!(
            z["intel-rapl:0"],
            RaplZone {
                name: "package-0".into(),
                energy_uj: 123456,
                max_energy_range_uj: 65532610987
            }
        );
    }

    #[test]
    fn no_readable_counter_is_an_error_naming_the_file() {
        let root = tempfile::tempdir().unwrap();
        zone(root.path(), "intel-rapl:0", "package-0", Some("not a number"));
        let e = read(root.path()).unwrap_err().to_string();
        assert!(e.contains("intel-rapl:0/energy_uj"), "{e}");
        let empty = tempfile::tempdir().unwrap();
        assert!(read(empty.path()).is_err());
    }
}
