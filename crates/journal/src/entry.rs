use std::collections::BTreeMap;

/// One journal entry with the fields that were read. Values are lossy UTF-8; the cursor, when read,
/// is under `__CURSOR` and the boot under `_BOOT_ID`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    /// `__REALTIME_TIMESTAMP`, microseconds since the Unix epoch.
    pub t_us: u64,
    pub fields: BTreeMap<String, String>,
}

impl Entry {
    pub fn get(&self, field: &str) -> Option<&str> {
        self.fields.get(field).map(String::as_str)
    }

    /// Syslog priority, 0 (emerg) to 7 (debug); an entry without a valid one counts as info.
    pub fn priority(&self) -> u8 {
        self.get("PRIORITY")
            .and_then(|p| p.parse().ok())
            .filter(|p| *p <= 7)
            .unwrap_or(6)
    }

    pub fn message(&self) -> &str {
        self.get("MESSAGE").unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_without_a_usable_priority_reads_as_info() {
        let mut e = Entry::default();
        assert_eq!(e.priority(), 6, "native entries may carry no PRIORITY");
        e.fields.insert("PRIORITY".into(), "3".into());
        assert_eq!(e.priority(), 3);
        e.fields.insert("PRIORITY".into(), "12".into());
        assert_eq!(e.priority(), 6, "out of the syslog range");
    }
}
