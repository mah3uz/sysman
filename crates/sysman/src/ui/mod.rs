use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub mod shell;
pub mod theme;

gpui_kit_assets::icon_assets!(
    AppIcons,
    [
        Gauge, List, Monitor, Zap, Network, HardDrive, Cpu, Activity, Lightbulb, Layers, Timer
    ]
);

/// Our icons first, then the component library's own.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match AppIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = AppIcons.list(path)?;
        paths.extend(gpui_kit_assets::Assets.list(path)?);
        Ok(paths)
    }
}
