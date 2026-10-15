use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub mod settings;
pub mod shell;
pub mod theme;
pub mod widgets;

gpui_kit_assets::icon_assets!(
    AppIcons,
    [
        Gauge,
        List,
        Monitor,
        Zap,
        Network,
        HardDrive,
        Cpu,
        Activity,
        Lightbulb,
        Layers,
        Timer,
        Info,
        Maximize2,
        Clock,
        Globe,
        Thermometer,
        ChartColumnStacked,
        ArrowDown,
        ArrowUp,
        Database,
        Flame,
        Search,
        ListTree,
        ChevronRight,
        ChevronDown,
        MemoryStick,
        FileText,
        AppWindow,
        SquareTerminal,
        Wifi,
        EthernetPort,
        Waypoints,
        Settings,
        ArrowLeftRight,
        Plug,
        ChartLine,
        HeartPulse,
        ChartArea,
        CircleCheck,
        SlidersHorizontal,
        X,
        Lock,
        Pencil,
        PencilLine,
        Trash,
        Mouse,
        Keyboard,
        Headphones,
        Gamepad2,
        Battery,
        BatteryFull,
        BatteryMedium,
        BatteryLow,
        BatteryCharging,
        Laptop,
        ChevronLeft,
        Calendar,
        ZoomIn,
        ZoomOut,
        Pause,
        Play,
        LayoutGrid,
        LayoutList,
        RefreshCw,
        PanelLeft,
        PanelRight,
        Ellipsis,
        StepBack,
        StepForward,
        Pin,
        PinOff,
        Crosshair,
        Plus,
        ListFilter,
        PcCase,
        Gpu,
        Bluetooth,
        Usb,
        Volume2,
        CircuitBoard,
        ShieldCheck,
        LayoutDashboard,
        Share,
        Fan,
        Copy,
        Radio,
        Sparkles,
        TrendingUp,
        ChartColumn,
        Skull,
    ]
);

pub fn open_settings(cx: &mut gpui::App) {
    settings::open(cx);
}

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
