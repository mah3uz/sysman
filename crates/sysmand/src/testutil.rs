use std::path::Path;

use crate::sys::Sys;

/// A throwaway root directory standing in for `/`.
pub struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    pub fn file(self, path: &str, contents: &str) -> Self {
        let full = self.dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
        self
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn sys(&self) -> Sys {
        Sys::at(self.dir.path())
    }
}

/// Fails if `T` drops, renames or reshapes anything in `value`: the app reads
/// records through these types, so anything lost here is invisible to it.
pub fn assert_schema<T: serde::Serialize + serde::de::DeserializeOwned>(value: &serde_json::Value) {
    let typed: T = serde_json::from_value(value.clone()).expect("value does not fit the schema type");
    assert_eq!(
        &serde_json::to_value(typed).unwrap(),
        value,
        "schema type does not round-trip the value"
    );
}
