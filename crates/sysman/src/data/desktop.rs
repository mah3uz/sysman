use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const ICON_SIZE: u32 = 32;
const ICON_EXTENSIONS: [&str; 2] = ["png", "svg"];

/// Process name → icon file, built once from the `.desktop` files and the icon theme.
#[derive(Debug, Default)]
pub struct IconIndex {
    by_name: HashMap<String, PathBuf>,
}

/// Keys a `.desktop` entry is known by (its `Exec` basename and `StartupWMClass`, lower case) and its `Icon=`.
#[derive(Debug, PartialEq)]
pub struct Entry {
    pub keys: Vec<String>,
    pub icon: String,
}

pub fn parse_entry(text: &str) -> Option<Entry> {
    let mut in_entry = false;
    let (mut exec, mut class, mut icon, mut hidden) = (None, None, None, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_entry) else {
            continue;
        };
        match key.trim() {
            "Exec" => exec = exec_name(value.trim()),
            "StartupWMClass" => class = Some(value.trim().to_lowercase()),
            "Icon" => icon = Some(value.trim().to_owned()),
            "Hidden" => hidden = value.trim() == "true",
            _ => {}
        }
    }
    // Symbolic icons are monochrome masks meant to be recoloured; drawn as images they are invisible on dark.
    let icon = icon.filter(|i| !i.is_empty() && !i.ends_with("-symbolic") && !hidden)?;
    let keys: Vec<String> = exec.into_iter().chain(class).collect();
    (!keys.is_empty()).then_some(Entry { keys, icon })
}

/// Basename of the program an `Exec=` line runs, past `env` and its `VAR=value` assignments.
pub fn exec_name(exec: &str) -> Option<String> {
    let word = if let Some(quoted) = exec.strip_prefix('"') {
        quoted.split('"').next()?
    } else {
        let mut words = exec.split_whitespace();
        let first = words.next()?;
        if first == "env" || first.ends_with("/env") {
            words.find(|w| !w.contains('=') && !w.starts_with('-'))?
        } else {
            first
        }
    };
    let name = Path::new(word).file_name()?.to_str()?;
    Some(name.to_lowercase())
}

/// The names a running process may match an entry by: its executable's basename, then its `comm`.
pub fn process_keys(comm: &str, cmdline: &str) -> [Option<String>; 2] {
    let exe = cmdline
        .split_whitespace()
        .next()
        .and_then(|w| Path::new(w).file_name()?.to_str())
        .map(str::to_lowercase);
    [exe, Some(comm.to_lowercase())]
}

impl IconIndex {
    /// Reads the user's applications and icon theme; slow, so call it off the UI thread.
    pub fn load() -> Self {
        let data = data_dirs();
        let mut themes: Vec<String> = current_theme().into_iter().collect();
        themes.push("hicolor".into());
        let mut icon_bases: Vec<PathBuf> = std::env::var_os("HOME")
            .map(|h| vec![PathBuf::from(h).join(".icons")])
            .unwrap_or_default();
        icon_bases.extend(data.iter().map(|d| d.join("icons")));
        let icons = icon_files(&icon_bases, &themes);
        let apps: Vec<PathBuf> = data.iter().map(|d| d.join("applications")).collect();
        Self::build(&apps, &icons)
    }

    /// The first entry to claim a key wins, so earlier (user) directories override system ones.
    pub fn build(app_dirs: &[PathBuf], icons: &HashMap<String, PathBuf>) -> Self {
        let mut by_name = HashMap::new();
        for dir in app_dirs {
            let Ok(read) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = read
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
                .collect();
            files.sort();
            for file in files {
                let Some(entry) = std::fs::read_to_string(&file).ok().and_then(|t| parse_entry(&t)) else {
                    continue;
                };
                let path = if Path::new(&entry.icon).is_absolute() {
                    Some(PathBuf::from(&entry.icon)).filter(|p| p.is_file())
                } else {
                    icons.get(&entry.icon).cloned()
                };
                let Some(path) = path else {
                    continue;
                };
                for key in entry.keys {
                    by_name.entry(key).or_insert_with(|| path.clone());
                }
            }
        }
        Self { by_name }
    }

    pub fn lookup(&self, comm: &str, cmdline: &str) -> Option<&PathBuf> {
        process_keys(comm, cmdline)
            .into_iter()
            .flatten()
            .find_map(|key| self.by_name.get(&key))
    }
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let user = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => home.join(".local/share"),
    };
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    std::iter::once(user)
        .chain(system.split(':').filter(|d| d.starts_with('/')).map(PathBuf::from))
        .collect()
}

fn current_theme() -> Option<String> {
    let out = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .output()
        .ok()?;
    let name = String::from_utf8(out.stdout).ok()?.trim().trim_matches('\'').to_owned();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// Icon name → file across `themes` in order; within a theme, 32 px first, then scalable, then the nearest size.
pub fn icon_files(bases: &[PathBuf], themes: &[String]) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    for theme in themes {
        for base in bases {
            let root = base.join(theme);
            let Ok(index) = std::fs::read_to_string(root.join("index.theme")) else {
                continue;
            };
            for dir in theme_dirs(&index) {
                let Ok(read) = std::fs::read_dir(root.join(&dir)) else {
                    continue;
                };
                for entry in read.flatten() {
                    let path = entry.path();
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if !ICON_EXTENSIONS.contains(&ext) {
                        continue;
                    }
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        out.entry(stem.to_owned()).or_insert(path);
                    }
                }
            }
        }
    }
    out
}

/// An `index.theme`'s application directories, best for a 32 px icon first.
fn theme_dirs(index: &str) -> Vec<String> {
    let mut section = String::new();
    let mut dirs: HashMap<String, (u32, bool, bool)> = HashMap::new();
    let mut listed = Vec::new();
    for line in index.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if section == "Icon Theme" && key == "Directories" {
            listed = value
                .split(',')
                .map(|d| d.trim().to_owned())
                .filter(|d| !d.is_empty())
                .collect();
        }
        let entry = dirs.entry(section.clone()).or_insert((0, false, false));
        match key {
            "Size" => entry.0 = value.parse().unwrap_or(0),
            "Type" => entry.1 = value == "Scalable",
            "Context" => entry.2 = value == "Applications",
            _ => {}
        }
    }
    let rank = |d: &String| {
        let (size, scalable, _) = dirs.get(d).copied().unwrap_or_default();
        if size == ICON_SIZE {
            0
        } else if scalable {
            1
        } else {
            2 + size.abs_diff(ICON_SIZE) + u32::from(size < ICON_SIZE) * 1000
        }
    };
    let mut apps: Vec<String> = listed
        .into_iter()
        .filter(|d| dirs.get(d).is_some_and(|e| e.2))
        .collect();
    apps.sort_by_key(rank);
    apps
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn exec_lines_name_the_program_they_run() {
        assert_eq!(exec_name("/usr/bin/firefox %u").as_deref(), Some("firefox"));
        assert_eq!(
            exec_name("env GDK_BACKEND=x11 /opt/app/Code --flag").as_deref(),
            Some("code")
        );
        assert_eq!(exec_name("\"/opt/My App/run\" %U").as_deref(), Some("run"));
        assert_eq!(exec_name(""), None);
    }

    #[test]
    fn only_the_desktop_entry_group_counts() {
        let text = "[Desktop Entry]\nName=Files\nExec=nautilus --new-window\nIcon=org.gnome.Nautilus\n\
                    StartupWMClass=org.gnome.Nautilus\n[Desktop Action new]\nExec=other\nIcon=wrong\n";
        assert_eq!(
            parse_entry(text),
            Some(Entry {
                keys: vec!["nautilus".into(), "org.gnome.nautilus".into()],
                icon: "org.gnome.Nautilus".into()
            })
        );
        assert_eq!(parse_entry("[Desktop Entry]\nExec=x\nIcon=x\nHidden=true\n"), None);
        assert_eq!(parse_entry("[Desktop Entry]\nExec=x\n"), None, "no icon to show");
        assert_eq!(
            parse_entry("[Desktop Entry]\nExec=x\nIcon=applications-system-symbolic\n"),
            None
        );
    }

    #[test]
    fn a_process_finds_its_icon_through_exec_or_wm_class_and_the_theme() {
        let dir = tempfile::tempdir().unwrap();
        let (user, system) = (dir.path().join("user"), dir.path().join("system"));
        for d in [&user, &system] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(
            system.join("code.desktop"),
            "[Desktop Entry]\nExec=/usr/share/code/code --unity %F\nIcon=vscode\n",
        )
        .unwrap();
        fs::write(
            system.join("chat.desktop"),
            "[Desktop Entry]\nExec=flatpak run org.chat\nIcon=chat\nStartupWMClass=Signal\n",
        )
        .unwrap();
        fs::write(system.join("term.desktop"), "[Desktop Entry]\nExec=kitty\nIcon=kitty\n").unwrap();
        fs::write(
            user.join("term.desktop"),
            "[Desktop Entry]\nExec=kitty\nIcon=/nonexistent.png\n",
        )
        .unwrap();
        fs::write(
            user.join("term2.desktop"),
            "[Desktop Entry]\nExec=kitty -1\nIcon=kitty-custom\n",
        )
        .unwrap();

        let theme = dir.path().join("icons/hicolor");
        fs::create_dir_all(theme.join("32x32/apps")).unwrap();
        fs::create_dir_all(theme.join("scalable/apps")).unwrap();
        fs::create_dir_all(theme.join("16x16/apps")).unwrap();
        fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nDirectories=16x16/apps,scalable/apps,32x32/apps\n\
             [16x16/apps]\nSize=16\nContext=Applications\n\
             [scalable/apps]\nSize=128\nType=Scalable\nContext=Applications\n\
             [32x32/apps]\nSize=32\nContext=Applications\n",
        )
        .unwrap();
        for f in [
            "16x16/apps/vscode.png",
            "scalable/apps/vscode.svg",
            "16x16/apps/chat.png",
            "scalable/apps/kitty-custom.svg",
        ] {
            fs::write(theme.join(f), "").unwrap();
        }
        fs::write(theme.join("32x32/apps/kitty.png"), "").unwrap();

        let icons = icon_files(&[dir.path().join("icons")], &["hicolor".into()]);
        assert_eq!(
            icons["vscode"],
            theme.join("scalable/apps/vscode.svg"),
            "scalable beats 16 px"
        );
        let index = IconIndex::build(&[user, system], &icons);
        assert_eq!(
            index.lookup("code", "/usr/share/code/code --type=renderer"),
            Some(&theme.join("scalable/apps/vscode.svg"))
        );
        assert_eq!(
            index.lookup("signal", ""),
            Some(&theme.join("16x16/apps/chat.png")),
            "by StartupWMClass"
        );
        assert_eq!(
            index.lookup("kitty", "kitty"),
            Some(&theme.join("scalable/apps/kitty-custom.svg")),
            "a user entry whose icon is missing falls through to the next user entry, before the system one"
        );
        assert_eq!(index.lookup("bash", "/bin/bash"), None);
    }
}
