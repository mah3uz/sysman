use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// What a file is, for colour and for the per-kind totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Folder,
    Application,
    Video,
    Image,
    Audio,
    Archive,
    Document,
    Code,
    Data,
    Cache,
    System,
    Other,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Folder,
        Kind::Application,
        Kind::Video,
        Kind::Image,
        Kind::Audio,
        Kind::Archive,
        Kind::Document,
        Kind::Code,
        Kind::Data,
        Kind::Cache,
        Kind::System,
        Kind::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Folder => "Folders",
            Kind::Application => "Applications",
            Kind::Video => "Video",
            Kind::Image => "Images",
            Kind::Audio => "Audio",
            Kind::Archive => "Archives and disk images",
            Kind::Document => "Documents",
            Kind::Code => "Code and developer",
            Kind::Data => "Data and databases",
            Kind::Cache => "Logs and caches",
            Kind::System => "System",
            Kind::Other => "Other",
        }
    }
}

/// A file's kind from its name; `executable` is any execute bit on a regular file.
pub fn classify(name: &str, executable: bool) -> Kind {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".so") || lower.contains(".so.") {
        return Kind::System;
    }
    let ext = match lower.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    };
    match ext {
        "appimage" | "exe" | "msi" | "apk" | "desktop" | "flatpak" | "flatpakref" | "snap" => Kind::Application,
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp" | "ogv" | "mts"
        | "m2ts" => Kind::Video,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "tif" | "tiff" | "bmp" | "svg" | "raw" | "cr2"
        | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "psd" | "xcf" | "ico" | "avif" | "jxl" | "kra" => Kind::Image,
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac" | "wma" | "aiff" | "aif" | "alac" | "mid" | "midi" => {
            Kind::Audio
        }
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "iso" | "img" | "dmg" | "qcow2"
        | "vdi" | "vmdk" | "vhd" | "vhdx" | "deb" | "rpm" | "lz4" | "lzma" | "cab" | "squashfs" | "sfs" => {
            Kind::Archive
        }
        "pdf" | "doc" | "docx" | "odt" | "ods" | "odp" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf"
        | "epub" | "mobi" | "tex" | "djvu" | "pages" | "key" | "numbers" => Kind::Document,
        "rs" | "c" | "h" | "cpp" | "hpp" | "cc" | "py" | "pyc" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go"
        | "java" | "class" | "jar" | "kt" | "swift" | "rb" | "php" | "lua" | "sh" | "zsh" | "bash" | "fish" | "pl"
        | "cs" | "scala" | "hs" | "ml" | "el" | "vim" | "toml" | "yaml" | "yml" | "html" | "css" | "scss" | "vue"
        | "svelte" | "o" | "a" | "rlib" | "rmeta" | "wasm" | "d" | "pack" | "idx" | "lock" | "node" => Kind::Code,
        "json" | "db" | "sqlite" | "sqlite3" | "db-wal" | "sqlite-wal" | "csv" | "tsv" | "parquet" | "xml" | "npy"
        | "npz" | "h5" | "hdf5" | "ldb" | "mdb" | "safetensors" | "gguf" | "onnx" | "pt" | "ckpt" | "bin" => Kind::Data,
        "log" | "tmp" | "temp" | "cache" | "journal" | "swp" | "old" => Kind::Cache,
        "ko" | "efi" | "service" | "conf" | "mo" | "ttf" | "otf" | "woff" | "woff2" | "pcf" => Kind::System,
        "" if executable => Kind::Application,
        _ => Kind::Other,
    }
}

/// Folders whose contents regenerate; everything under them is a cache whatever its extension.
pub fn cache_dir(name: &str) -> bool {
    matches!(
        name,
        ".cache" | "cache" | "Cache" | "caches" | "Caches" | "__pycache__" | "CachedData" | "GPUCache" | "Code Cache"
    )
}

pub const DIR: u8 = 1;
/// Small files of one kind in one folder, drawn as one cell.
pub const FOLD: u8 = 2;
pub const UNREADABLE: u8 = 4;
pub const OTHER_FS: u8 = 8;
/// A second name for a file counted elsewhere; it carries no bytes.
pub const HARD_LINK: u8 = 16;
pub const SYMLINK: u8 = 32;
const IN_CACHE: u8 = 64;

#[derive(Debug, Clone)]
pub struct Node {
    pub name: Box<OsStr>,
    pub parent: u32,
    /// Largest first once the scan has finished.
    pub children: Vec<u32>,
    /// Allocated bytes (`st_blocks * 512`), the whole subtree for a folder.
    pub bytes: u64,
    /// Entries in the subtree, this one included; a fold counts its files.
    pub items: u64,
    pub modified: i64,
    pub kind: Kind,
    pub flags: u8,
}

impl Node {
    pub fn is(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Folders the scan was not allowed to list.
    pub unreadable: u32,
    /// Mount points inside the scan; their contents belong to another filesystem.
    pub other_fs: u32,
    pub hard_links: u32,
    /// Folders that disappeared between being found and being listed.
    pub vanished: u32,
}

#[derive(Debug, Clone)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub counts: Counts,
    pub scanned_at: SystemTime,
    pub took: Duration,
    /// Cancelled before every folder was listed.
    pub partial: bool,
    /// Moved to the Trash from the map since the scan.
    pub trashed: u64,
}

pub const ROOT: u32 = 0;

impl Tree {
    pub fn node(&self, id: u32) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn root_path(&self) -> &Path {
        Path::new(&*self.nodes[0].name)
    }

    pub fn name(&self, id: u32) -> Cow<'_, str> {
        self.node(id).name.to_string_lossy()
    }

    /// Root first, `id` last.
    pub fn lineage(&self, id: u32) -> Vec<u32> {
        let mut out = vec![id];
        let mut at = id;
        while at != ROOT {
            at = self.node(at).parent;
            out.push(at);
        }
        out.reverse();
        out
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut path = PathBuf::new();
        for n in self.lineage(id) {
            path.push(&*self.node(n).name);
        }
        path
    }

    /// A folder that can be opened in the map.
    pub fn openable(&self, id: u32) -> bool {
        let n = self.node(id);
        n.is(DIR) && !n.children.is_empty()
    }

    /// Real files with bytes of their own: no folds, no second hard links.
    pub fn files(&self) -> impl Iterator<Item = u32> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.flags & (DIR | FOLD | HARD_LINK) == 0 && n.bytes > 0)
            .map(|(i, _)| i as u32)
    }

    /// The `limit` largest of `ids`, largest first.
    pub fn largest(&self, ids: impl Iterator<Item = u32>, limit: usize) -> Vec<u32> {
        let mut ids: Vec<u32> = ids.collect();
        let key = |id: &u32| std::cmp::Reverse(self.node(*id).bytes);
        if ids.len() > limit {
            ids.select_nth_unstable_by_key(limit, key);
            ids.truncate(limit);
        }
        ids.sort_by_key(key);
        ids
    }

    /// Bytes and items per kind over every file, folds included, largest first.
    pub fn kinds(&self) -> Vec<(Kind, u64, u64)> {
        let mut totals: BTreeMap<Kind, (u64, u64)> = BTreeMap::new();
        for n in &self.nodes {
            if n.is(DIR) || n.is(HARD_LINK) {
                continue;
            }
            let t = totals.entry(n.kind).or_default();
            t.0 += n.bytes;
            t.1 += n.items;
        }
        let mut out: Vec<_> = totals.into_iter().map(|(k, (b, c))| (k, b, c)).collect();
        out.sort_by_key(|(k, b, _)| (std::cmp::Reverse(*b), *k));
        out
    }

    /// Takes a trashed item out of the totals; the node stays so a stale index never dangles.
    pub fn remove(&mut self, id: u32) {
        if id == ROOT {
            return;
        }
        let (bytes, items, parent) = {
            let n = self.node(id);
            (n.bytes, n.items, n.parent)
        };
        self.nodes[parent as usize].children.retain(|c| *c != id);
        let mut at = parent;
        loop {
            let n = &mut self.nodes[at as usize];
            n.bytes = n.bytes.saturating_sub(bytes);
            n.items = n.items.saturating_sub(items);
            if at == ROOT {
                break;
            }
            at = n.parent;
        }
        self.trashed += bytes;
    }
}

/// The device a directory lives on; injected so tests can fake a mount point.
pub type DevFn = Arc<dyn Fn(&Path, &Metadata) -> u64 + Send + Sync>;

pub struct Options {
    pub threads: usize,
    /// Files below this fold into one cell per kind when a folder has many of them.
    pub fold_below: u64,
    pub dev: DevFn,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            threads: thread::available_parallelism().map_or(4, |n| n.get().min(8)),
            fold_below: 16 * 1024,
            dev: Arc::new(|_, m| m.dev()),
        }
    }
}

const FOLD_MIN_FILES: usize = 24;

/// One entry of the level a running scan reports, as far as the scan has got.
#[derive(Debug, Clone, PartialEq)]
pub struct Top {
    pub name: String,
    pub bytes: u64,
    pub kind: Kind,
    pub dir: bool,
    /// Every folder below it has been listed, so `bytes` is final.
    pub done: bool,
}

/// The first level below the scanned folder with more than one entry, and its entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Level {
    pub path: PathBuf,
    pub tops: Vec<Top>,
}

#[derive(Default)]
pub struct Progress {
    pub items: AtomicU64,
    pub bytes: AtomicU64,
    /// Folders listed so far.
    pub folders: AtomicU64,
    cancel: AtomicBool,
    current: Mutex<PathBuf>,
    level: Mutex<Level>,
}

impl Progress {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The folder listed last.
    pub fn current(&self) -> PathBuf {
        self.current.lock().unwrap().clone()
    }

    /// Empty until the level's folder has been listed.
    pub fn level(&self) -> Level {
        self.level.lock().unwrap().clone()
    }
}

struct Job {
    node: u32,
    path: PathBuf,
}

struct Entry {
    name: OsString,
    meta: Metadata,
    dev: u64,
}

enum Listing {
    Read {
        node: u32,
        path: PathBuf,
        entries: Vec<Entry>,
    },
    Failed {
        node: u32,
        error: io::ErrorKind,
    },
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<(Vec<Job>, bool)>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, job: Job) {
        self.jobs.lock().unwrap().0.push(job);
        self.ready.notify_one();
    }

    fn pop(&self) -> Option<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        loop {
            if jobs.1 {
                return None;
            }
            if let Some(job) = jobs.0.pop() {
                return Some(job);
            }
            jobs = self.ready.wait(jobs).unwrap();
        }
    }

    fn close(&self) {
        self.jobs.lock().unwrap().1 = true;
        self.ready.notify_all();
    }
}

fn list(job: Job, dev: &DevFn) -> Listing {
    let read = match fs::read_dir(&job.path) {
        Ok(read) => read,
        Err(e) => {
            return Listing::Failed {
                node: job.node,
                error: e.kind(),
            };
        }
    };
    let mut entries = Vec::new();
    for entry in read.flatten() {
        // DirEntry::metadata is an fstatat on the open directory and never follows a symlink.
        let Ok(meta) = entry.metadata() else { continue };
        let dev = if meta.is_dir() {
            dev(&entry.path(), &meta)
        } else {
            meta.dev()
        };
        entries.push(Entry {
            name: entry.file_name(),
            meta,
            dev,
        });
    }
    Listing::Read {
        node: job.node,
        path: job.path,
        entries,
    }
}

fn allocated(meta: &Metadata) -> u64 {
    meta.blocks() * 512
}

struct Builder {
    nodes: Vec<Node>,
    /// The folder whose entries the progress reports: the root, or below it while it holds a single folder.
    focus: u32,
    /// Per entry of the focus, listings still to merge below it.
    outstanding: Vec<usize>,
    counts: Counts,
    seen: HashSet<(u64, u64)>,
    root_dev: u64,
    fold_below: u64,
}

impl Builder {
    fn push(&mut self, parent: u32, node: Node) -> u32 {
        let id = self.nodes.len() as u32;
        self.nodes.push(node);
        self.nodes[parent as usize].children.push(id);
        id
    }

    fn merge(&mut self, listing: Listing, progress: &Progress) -> Vec<Job> {
        progress.folders.fetch_add(1, Ordering::Relaxed);
        let (node, path, entries) = match listing {
            Listing::Failed { node, error } => {
                if error == io::ErrorKind::NotFound {
                    self.counts.vanished += 1;
                } else {
                    self.counts.unreadable += 1;
                    self.nodes[node as usize].flags |= UNREADABLE;
                }
                self.report(node, 0, 0, progress);
                return Vec::new();
            }
            Listing::Read { node, path, entries } => (node, path, entries),
        };
        let in_cache = self.nodes[node as usize].is(IN_CACHE);
        let small = entries
            .iter()
            .filter(|e| e.meta.is_file() && allocated(&e.meta) < self.fold_below)
            .count();
        let fold = small > FOLD_MIN_FILES;
        let mut folds: BTreeMap<Kind, (u64, u64, i64)> = BTreeMap::new();
        let mut jobs = Vec::new();
        let (mut items, mut total) = (0, 0);
        for Entry { name, meta, dev } in entries {
            items += 1;
            let modified = meta.mtime();
            if meta.is_dir() {
                if dev != self.root_dev {
                    self.counts.other_fs += 1;
                    let node = Node {
                        name: name.into_boxed_os_str(),
                        parent: node,
                        children: Vec::new(),
                        bytes: 0,
                        items: 1,
                        modified,
                        kind: Kind::Folder,
                        flags: DIR | OTHER_FS,
                    };
                    self.push(node.parent, node);
                    continue;
                }
                let cache = in_cache || cache_dir(&name.to_string_lossy());
                let bytes = allocated(&meta);
                total += bytes;
                let child_path = path.join(&name);
                let id = self.push(
                    node,
                    Node {
                        name: name.into_boxed_os_str(),
                        parent: node,
                        children: Vec::new(),
                        bytes,
                        items: 1,
                        modified,
                        kind: Kind::Folder,
                        flags: DIR | if cache { IN_CACHE } else { 0 },
                    },
                );
                jobs.push(Job {
                    node: id,
                    path: child_path,
                });
                continue;
            }
            let mut bytes = allocated(&meta);
            let mut flags = 0;
            if meta.is_file() && meta.nlink() > 1 && !self.seen.insert((meta.dev(), meta.ino())) {
                bytes = 0;
                flags |= HARD_LINK;
                self.counts.hard_links += 1;
            }
            if meta.file_type().is_symlink() {
                flags |= SYMLINK;
            }
            let kind = if !meta.is_file() {
                Kind::Other
            } else if in_cache {
                Kind::Cache
            } else {
                classify(&name.to_string_lossy(), meta.mode() & 0o111 != 0)
            };
            total += bytes;
            if fold && flags == 0 && meta.is_file() && bytes < self.fold_below {
                let f = folds.entry(kind).or_default();
                f.0 += bytes;
                f.1 += 1;
                f.2 = f.2.max(modified);
                continue;
            }
            self.push(
                node,
                Node {
                    name: name.into_boxed_os_str(),
                    parent: node,
                    children: Vec::new(),
                    bytes,
                    items: 1,
                    modified,
                    kind,
                    flags,
                },
            );
        }
        for (kind, (bytes, count, modified)) in folds {
            let name = OsString::from(format!("{count} small files"));
            self.push(
                node,
                Node {
                    name: name.into_boxed_os_str(),
                    parent: node,
                    children: Vec::new(),
                    bytes,
                    items: count,
                    modified,
                    kind,
                    flags: FOLD,
                },
            );
        }
        progress.items.fetch_add(items, Ordering::Relaxed);
        progress.bytes.fetch_add(total, Ordering::Relaxed);
        *progress.current.lock().unwrap() = path;
        self.report(node, total, jobs.len(), progress);
        jobs
    }

    fn path_of(&self, id: u32) -> PathBuf {
        let mut names = vec![&*self.nodes[id as usize].name];
        let mut at = id;
        while at != ROOT {
            at = self.nodes[at as usize].parent;
            names.push(&*self.nodes[at as usize].name);
        }
        names.into_iter().rev().collect()
    }

    /// Credits a merged listing of `node` to the entry of the focus it lies in; the focus's own listing
    /// creates the entries, or moves the focus down when it holds a single folder.
    fn report(&mut self, node: u32, total: u64, jobs: usize, progress: &Progress) {
        let mut level = progress.level.lock().unwrap();
        let children = &self.nodes[node as usize].children;
        if node == self.focus {
            if let [only] = children[..]
                && self.nodes[only as usize].is(DIR)
                && !self.nodes[only as usize].is(OTHER_FS)
            {
                self.focus = only;
                *level = Level {
                    path: self.path_of(only),
                    tops: Vec::new(),
                };
                return;
            }
            level.path = self.path_of(node);
            for &id in children {
                let n = &self.nodes[id as usize];
                let pending = n.is(DIR) && !n.is(OTHER_FS);
                self.outstanding.push(usize::from(pending));
                level.tops.push(Top {
                    name: n.name.to_string_lossy().into_owned(),
                    bytes: n.bytes,
                    kind: n.kind,
                    dir: n.is(DIR),
                    done: !pending,
                });
            }
            return;
        }
        let mut at = node;
        while self.nodes[at as usize].parent != self.focus {
            if at == ROOT {
                return;
            }
            at = self.nodes[at as usize].parent;
        }
        // the focus's entries were pushed in one listing, so their ids are consecutive
        let first = self.nodes[self.focus as usize].children.first().copied().unwrap_or(at);
        let Some(i) = at
            .checked_sub(first)
            .map(|i| i as usize)
            .filter(|i| *i < self.outstanding.len())
        else {
            return;
        };
        self.outstanding[i] = (self.outstanding[i] + jobs).saturating_sub(1);
        level.tops[i].bytes += total;
        level.tops[i].done = self.outstanding[i] == 0;
    }

    fn finish(mut self, started: Instant, partial: bool) -> Tree {
        // Children always have larger indices than their parent, so one backward pass totals every subtree.
        for i in (1..self.nodes.len()).rev() {
            let (bytes, items, parent) = (self.nodes[i].bytes, self.nodes[i].items, self.nodes[i].parent);
            let p = &mut self.nodes[parent as usize];
            p.bytes += bytes;
            p.items += items;
        }
        for i in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[i].children);
            children.sort_by_key(|c| std::cmp::Reverse(self.nodes[*c as usize].bytes));
            self.nodes[i].children = children;
        }
        Tree {
            nodes: self.nodes,
            counts: self.counts,
            scanned_at: SystemTime::now(),
            took: started.elapsed(),
            partial,
            trashed: 0,
        }
    }
}

/// Walks `root` on its own filesystem with `opts.threads` listers; the calling thread builds the tree.
pub fn walk(root: &Path, opts: &Options, progress: &Progress) -> io::Result<Tree> {
    let started = Instant::now();
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a folder"));
    }
    let mut builder = Builder {
        nodes: vec![Node {
            name: root.as_os_str().into(),
            parent: ROOT,
            children: Vec::new(),
            bytes: allocated(&meta),
            items: 1,
            modified: meta.mtime(),
            kind: Kind::Folder,
            flags: DIR,
        }],
        focus: ROOT,
        outstanding: Vec::new(),
        counts: Counts::default(),
        seen: HashSet::new(),
        root_dev: (opts.dev)(root, &meta),
        fold_below: opts.fold_below,
    };
    let queue = Queue::default();
    let (tx, rx) = mpsc::channel();
    let mut partial = false;
    thread::scope(|s| {
        for _ in 0..opts.threads.max(1) {
            let (tx, queue, dev) = (tx.clone(), &queue, &opts.dev);
            s.spawn(move || {
                while let Some(job) = queue.pop() {
                    if tx.send(list(job, dev)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        queue.push(Job {
            node: ROOT,
            path: root.to_path_buf(),
        });
        let mut pending = 1usize;
        while pending > 0 {
            if progress.cancel.load(Ordering::Relaxed) {
                partial = true;
                break;
            }
            let listing = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(listing) => listing,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            pending -= 1;
            for job in builder.merge(listing, progress) {
                queue.push(job);
                pending += 1;
            }
            #[cfg(debug_assertions)]
            dev_throttle(progress);
        }
        queue.close();
    });
    Ok(builder.finish(started, partial))
}

/// `SYSMAN_DEV_DISKMAP_SCAN=<ms>` waits that long after each folder; `pause:<n>` stops after `n` folders
/// until the scan is cancelled, so the scanning state holds still for a screenshot.
#[cfg(debug_assertions)]
fn dev_throttle(progress: &Progress) {
    static SETTING: LazyLock<Option<(Duration, u64)>> = LazyLock::new(|| {
        let v = std::env::var("SYSMAN_DEV_DISKMAP_SCAN").ok()?;
        match v.strip_prefix("pause:") {
            Some(n) => Some((Duration::ZERO, n.parse().ok()?)),
            None => Some((Duration::from_millis(v.parse().ok()?), u64::MAX)),
        }
    });
    let Some((delay, pause_at)) = *SETTING else { return };
    thread::sleep(delay);
    while progress.folders.load(Ordering::Relaxed) >= pause_at && !progress.cancelled() {
        thread::sleep(Duration::from_millis(50));
    }
}

/// One scan of one folder, shared by every view of it.
pub struct Scan {
    pub root: PathBuf,
    pub progress: Progress,
    pub started: Instant,
    outcome: Mutex<Option<Result<Arc<Tree>, String>>>,
}

impl Scan {
    pub fn outcome(&self) -> Option<Result<Arc<Tree>, String>> {
        self.outcome.lock().unwrap().clone()
    }

    pub fn running(&self) -> bool {
        self.outcome.lock().unwrap().is_none()
    }
}

/// Scans and finished trees per root, kept for the life of the process so switching tabs loses nothing.
static SCANS: LazyLock<Mutex<HashMap<PathBuf, Arc<Scan>>>> = LazyLock::new(Default::default);
static TREES: LazyLock<Mutex<HashMap<PathBuf, Arc<Tree>>>> = LazyLock::new(Default::default);

/// Starts a scan of `root`, or returns the one already running.
pub fn start(root: &Path) -> Arc<Scan> {
    if let Some(scan) = running(root) {
        return scan;
    }
    let scan = Arc::new(Scan {
        root: root.to_path_buf(),
        progress: Progress::default(),
        started: Instant::now(),
        outcome: Mutex::new(None),
    });
    SCANS.lock().unwrap().insert(root.to_path_buf(), scan.clone());
    let job = scan.clone();
    let spawned = thread::Builder::new().name("disk-map".into()).spawn(move || {
        let outcome = walk(&job.root, &Options::default(), &job.progress)
            .map(Arc::new)
            .map_err(|e| e.to_string());
        if let Ok(tree) = &outcome {
            TREES.lock().unwrap().insert(job.root.clone(), tree.clone());
        }
        *job.outcome.lock().unwrap() = Some(outcome);
    });
    if let Err(e) = spawned {
        *scan.outcome.lock().unwrap() = Some(Err(e.to_string()));
    }
    scan
}

pub fn running(root: &Path) -> Option<Arc<Scan>> {
    SCANS.lock().unwrap().get(root).filter(|s| s.running()).cloned()
}

pub fn cached(root: &Path) -> Option<Arc<Tree>> {
    TREES.lock().unwrap().get(root).cloned()
}

pub fn store(tree: Arc<Tree>) {
    TREES.lock().unwrap().insert(tree.root_path().to_path_buf(), tree);
}

/// `$XDG_DATA_HOME/Trash`, the home trash of the freedesktop trash spec.
pub fn home_trash() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(data.join("Trash"))
}

/// Why `path` cannot go to the home trash, or `None` when it can.
pub fn trash_refusal(path: &Path, trash: &Path) -> Option<&'static str> {
    if path.starts_with(trash) {
        return Some("Already in the Trash.");
    }
    if std::env::var_os("HOME").is_some_and(|h| Path::new(&h) == path) || path.parent().is_none() {
        return Some("The home folder and the root cannot be trashed.");
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Some("It no longer exists.");
    };
    let base = trash.ancestors().find_map(|a| fs::metadata(a).ok());
    // The spec's home trash is a rename, which only works within one filesystem.
    if base.is_none_or(|b| b.dev() != meta.dev()) {
        return Some("Only items on the home filesystem can go to the Trash.");
    }
    None
}

fn encode(path: &Path) -> String {
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Moves `path` into `trash` with its `.trashinfo`, as the freedesktop trash spec describes.
pub fn trash_into(path: &Path, trash: &Path, deleted_at: &str) -> io::Result<PathBuf> {
    let (files, info) = (trash.join("files"), trash.join("info"));
    for dir in [&files, &info] {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let base = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    for n in 1..10_000 {
        let mut name = base.to_os_string();
        if n > 1 {
            name.push(format!(".{n}"));
        }
        let mut info_name = name.clone();
        info_name.push(".trashinfo");
        let info_path = info.join(&info_name);
        // Creating the info file exclusively first is the spec's lock on the name.
        let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(&info_path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let target = files.join(&name);
        if fs::symlink_metadata(&target).is_ok() {
            drop(file);
            fs::remove_file(&info_path).ok();
            continue;
        }
        let written = write!(file, "[Trash Info]\nPath={}\nDeletionDate={deleted_at}\n", encode(path))
            .and_then(|()| file.sync_all());
        let moved = written.and_then(|()| fs::rename(path, &target));
        if let Err(e) = moved {
            fs::remove_file(&info_path).ok();
            return Err(e);
        }
        return Ok(target);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name in the Trash",
    ))
}

pub fn trash(path: &Path) -> io::Result<PathBuf> {
    let trash = home_trash().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home folder"))?;
    if let Some(reason) = trash_refusal(path, &trash) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
    }
    let now = jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S").to_string();
    trash_into(path, &trash, &now)
}

fn spawn_detached(mut command: Command) -> io::Result<()> {
    let mut child = command.spawn()?;
    thread::spawn(move || child.wait());
    Ok(())
}

pub fn open(path: &Path) -> io::Result<()> {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    spawn_detached(command)
}

/// Selects the item in the desktop's file manager; opens the parent folder when no file manager answers.
pub fn show_in_file_manager(path: &Path) {
    let path = path.to_path_buf();
    thread::spawn(move || {
        let uri = format!("file://{}", encode(&path));
        let shown = zbus::blocking::Connection::session().and_then(|bus| {
            bus.call_method(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                Some("org.freedesktop.FileManager1"),
                "ShowItems",
                &(vec![uri], ""),
            )
        });
        if shown.is_err()
            && let Some(parent) = path.parent()
        {
            open(parent).ok();
        }
    });
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn write(path: &Path, len: usize) {
        fs::write(path, vec![7u8; len]).unwrap();
    }

    fn opts(dev: DevFn) -> Options {
        Options {
            threads: 3,
            fold_below: 0,
            dev,
        }
    }

    fn find(tree: &Tree, name: &str) -> u32 {
        tree.nodes
            .iter()
            .position(|n| &*n.name == OsStr::new(name))
            .unwrap_or_else(|| panic!("{name} not in the tree")) as u32
    }

    #[test]
    fn kinds_follow_the_extension_whatever_the_case() {
        assert_eq!(classify("Holiday.MKV", false), Kind::Video);
        assert_eq!(classify("photo.jpeg", false), Kind::Image);
        assert_eq!(
            classify("backup.tar.zst", false),
            Kind::Archive,
            "the last extension decides"
        );
        assert_eq!(classify("libfoo.so.1.2.3", false), Kind::System, "versioned libraries");
        assert_eq!(classify("history.sqlite", false), Kind::Data);
        assert_eq!(classify("main.rs", false), Kind::Code);
        assert_eq!(classify("Xorg.0.log", false), Kind::Cache);
        assert_eq!(classify("Krita.AppImage", true), Kind::Application);
    }

    #[test]
    fn a_bare_executable_is_an_application_but_a_dotfile_is_not_an_extension() {
        assert_eq!(classify("btop", true), Kind::Application);
        assert_eq!(classify("btop", false), Kind::Other);
        assert_eq!(
            classify(".bashrc", false),
            Kind::Other,
            "a leading dot names the file, not its type"
        );
        assert_eq!(classify("README", false), Kind::Other);
    }

    #[test]
    fn hard_links_count_once_and_totals_add_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        write(&root.join("a/big.bin"), 256 * 1024);
        fs::hard_link(root.join("a/big.bin"), root.join("b/same.bin")).unwrap();
        write(&root.join("b/other.txt"), 8 * 1024);

        let tree = walk(root, &opts(Arc::new(|_, m| m.dev())), &Progress::default()).unwrap();
        let (big, same) = (find(&tree, "big.bin"), find(&tree, "same.bin"));
        let firsts = [big, same].iter().filter(|id| !tree.node(**id).is(HARD_LINK)).count();
        assert_eq!(firsts, 1, "one of the two names carries the bytes");
        assert_eq!(tree.counts.hard_links, 1);
        let blocks = fs::metadata(root.join("a/big.bin")).unwrap().blocks() * 512;
        assert_eq!(tree.node(big).bytes + tree.node(same).bytes, blocks);
        let sum: u64 = tree.node(ROOT).children.iter().map(|c| tree.node(*c).bytes).sum();
        let own = fs::metadata(root).unwrap().blocks() * 512;
        assert_eq!(
            tree.node(ROOT).bytes,
            sum + own,
            "a folder is its own blocks plus its children"
        );
        assert_eq!(tree.path(big), root.join("a/big.bin"));
    }

    #[test]
    fn another_filesystem_is_counted_but_not_entered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("mnt/inside")).unwrap();
        write(&root.join("mnt/inside/hidden.bin"), 64 * 1024);
        write(&root.join("kept.bin"), 4096);
        let mount = root.join("mnt");
        let dev: DevFn = Arc::new(move |p, m| if p == mount { m.dev() + 1 } else { m.dev() });

        let tree = walk(&root, &opts(dev), &Progress::default()).unwrap();
        let mnt = find(&tree, "mnt");
        assert!(tree.node(mnt).is(OTHER_FS));
        assert_eq!(tree.node(mnt).bytes, 0, "nothing under a mount point is counted");
        assert!(tree.node(mnt).children.is_empty());
        assert!(!tree.nodes.iter().any(|n| &*n.name == OsStr::new("hidden.bin")));
        assert_eq!(tree.counts.other_fs, 1);
        find(&tree, "kept.bin");
    }

    #[test]
    fn unreadable_folders_are_skipped_and_counted() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        write(&locked.join("secret"), 4096);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let tree = walk(dir.path(), &opts(Arc::new(|_, m| m.dev())), &Progress::default());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let tree = tree.unwrap();
        assert_eq!(tree.counts.unreadable, 1);
        assert!(tree.node(find(&tree, "locked")).is(UNREADABLE));
    }

    #[test]
    fn the_top_level_progress_ends_with_the_same_sizes_as_the_tree() {
        // the scanning view draws these as the partial map, so a top folder marked done must already be final
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        write(&root.join("a/b/c/deep.bin"), 300_000);
        write(&root.join("a/one.txt"), 5_000);
        fs::create_dir(root.join("empty")).unwrap();
        write(&root.join("loose.mkv"), 70_000);
        let progress = Progress::default();
        let tree = walk(root, &opts(Arc::new(|_, m| m.dev())), &progress).unwrap();
        let level = progress.level();
        assert_eq!(level.path, root);
        let tops = level.tops;
        assert_eq!(tops.len(), tree.node(ROOT).children.len());
        for top in &tops {
            let n = tree.node(find(&tree, &top.name));
            assert!(top.done, "{} is still pending after the walk", top.name);
            assert_eq!(top.bytes, n.bytes, "{}", top.name);
            assert_eq!(top.dir, n.is(DIR));
        }
        assert_eq!(
            progress.folders.load(Ordering::Relaxed),
            5,
            "the root, a, a/b, a/b/c and empty"
        );
        assert!(progress.current().starts_with(root));
    }

    #[test]
    fn a_folder_holding_one_folder_reports_the_level_below() {
        // a home folder with one user in it would otherwise draw as a single tile for the whole scan
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("user/x")).unwrap();
        write(&root.join("user/x/big.bin"), 200_000);
        write(&root.join("user/notes.txt"), 9_000);
        let progress = Progress::default();
        let tree = walk(root, &opts(Arc::new(|_, m| m.dev())), &progress).unwrap();
        let level = progress.level();
        assert_eq!(level.path, root.join("user"));
        let names: Vec<&str> = level.tops.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"x") && names.contains(&"notes.txt"));
        for top in &level.tops {
            assert!(top.done);
            assert_eq!(top.bytes, tree.node(find(&tree, &top.name)).bytes, "{}", top.name);
        }
    }

    #[test]
    fn many_small_files_fold_into_one_cell_per_kind_with_exact_totals() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..30 {
            write(&dir.path().join(format!("{i}.txt")), 10);
        }
        write(&dir.path().join("large.mkv"), 128 * 1024);
        let options = Options {
            fold_below: 16 * 1024,
            ..opts(Arc::new(|_, m| m.dev()))
        };
        let tree = walk(dir.path(), &options, &Progress::default()).unwrap();
        let root = tree.node(ROOT);
        assert_eq!(root.children.len(), 2, "the large file and one fold");
        let fold = tree.node(find(&tree, "30 small files"));
        assert!(fold.is(FOLD));
        assert_eq!((fold.kind, fold.items), (Kind::Document, 30));
        assert_eq!(root.items, 32, "the folder, the large file and every folded file");
    }

    #[test]
    fn removing_an_item_takes_its_bytes_off_every_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        write(&dir.path().join("a/f.bin"), 64 * 1024);
        let mut tree = walk(dir.path(), &opts(Arc::new(|_, m| m.dev())), &Progress::default()).unwrap();
        let (a, f) = (find(&tree, "a"), find(&tree, "f.bin"));
        let (root_before, a_before, bytes) = (tree.node(ROOT).bytes, tree.node(a).bytes, tree.node(f).bytes);
        tree.remove(f);
        assert_eq!(tree.node(a).bytes, a_before - bytes);
        assert_eq!(tree.node(ROOT).bytes, root_before - bytes);
        assert!(tree.node(a).children.is_empty());
        assert_eq!(tree.trashed, bytes);
    }

    #[test]
    fn trashing_writes_a_trashinfo_and_never_overwrites_an_earlier_item() {
        let dir = tempfile::tempdir().unwrap();
        let trash = dir.path().join("Trash");
        let item = dir.path().join("my report.pdf");
        write(&item, 100);
        let first = trash_into(&item, &trash, "2026-10-15T09:30:00").unwrap();
        assert_eq!(first, trash.join("files/my report.pdf"));
        assert!(!item.exists());
        let info = fs::read_to_string(trash.join("info/my report.pdf.trashinfo")).unwrap();
        let expected = format!(
            "[Trash Info]\nPath={}/my%20report.pdf\nDeletionDate=2026-10-15T09:30:00\n",
            encode(dir.path())
        );
        assert_eq!(info, expected, "the path is percent-encoded and absolute");

        write(&item, 50);
        let second = trash_into(&item, &trash, "2026-10-15T09:31:00").unwrap();
        assert_eq!(second, trash.join("files/my report.pdf.2"));
        assert!(trash.join("info/my report.pdf.2.trashinfo").exists());
        assert_eq!(fs::metadata(&first).unwrap().len(), 100, "the first item is untouched");
    }

    #[test]
    fn trash_refuses_items_already_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let trash = dir.path().join("Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        write(&trash.join("files/x"), 1);
        assert!(trash_refusal(&trash.join("files/x"), &trash).is_some());
        write(&dir.path().join("y"), 1);
        assert_eq!(trash_refusal(&dir.path().join("y"), &trash), None);
    }
}
