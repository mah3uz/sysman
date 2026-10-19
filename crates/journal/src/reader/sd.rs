use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;

use super::{Boot, Change, DEFAULT_THRESHOLD, Fields, Filter, Seek, value, wants};
use crate::Entry;

/// Leaves out files from other machines, as `journalctl` does without `--merge`.
const LOCAL_ONLY: c_int = 1;

type Handle = *mut c_void;

/// `sd_id128_t` is a union with `uint64_t[2]`, hence the alignment.
#[repr(C, align(8))]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Id128([u8; 16]);

pub(super) struct Api {
    open: unsafe extern "C" fn(*mut Handle, c_int) -> c_int,
    close: unsafe extern "C" fn(Handle),
    add_match: unsafe extern "C" fn(Handle, *const c_void, usize) -> c_int,
    add_disjunction: unsafe extern "C" fn(Handle) -> c_int,
    add_conjunction: unsafe extern "C" fn(Handle) -> c_int,
    flush_matches: unsafe extern "C" fn(Handle),
    seek_head: unsafe extern "C" fn(Handle) -> c_int,
    seek_tail: unsafe extern "C" fn(Handle) -> c_int,
    seek_realtime_usec: unsafe extern "C" fn(Handle, u64) -> c_int,
    seek_cursor: unsafe extern "C" fn(Handle, *const c_char) -> c_int,
    next: unsafe extern "C" fn(Handle) -> c_int,
    previous: unsafe extern "C" fn(Handle) -> c_int,
    get_realtime_usec: unsafe extern "C" fn(Handle, *mut u64) -> c_int,
    get_monotonic_usec: unsafe extern "C" fn(Handle, *mut u64, *mut Id128) -> c_int,
    get_cursor: unsafe extern "C" fn(Handle, *mut *mut c_char) -> c_int,
    set_data_threshold: unsafe extern "C" fn(Handle, usize) -> c_int,
    restart_data: unsafe extern "C" fn(Handle),
    enumerate_available_data: unsafe extern "C" fn(Handle, *mut *const c_void, *mut usize) -> c_int,
    query_unique: unsafe extern "C" fn(Handle, *const c_char) -> c_int,
    enumerate_available_unique: unsafe extern "C" fn(Handle, *mut *const c_void, *mut usize) -> c_int,
    get_fd: unsafe extern "C" fn(Handle) -> c_int,
    wait: unsafe extern "C" fn(Handle, u64) -> c_int,
    _lib: Library,
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

pub(super) fn api() -> Result<&'static Api> {
    API.get_or_init(|| load().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow!("loading libsystemd.so.0: {e}"))
}

fn load() -> Result<Api, libloading::Error> {
    // SAFETY: libsystemd's initialisers only set up its own state, and it is never unloaded: `Api` lives in a static.
    let lib = unsafe { Library::new("libsystemd.so.0")? };
    // SAFETY: each field's type is the function's prototype in sd-journal.h (v246+, which added the
    // `enumerate_available_*` pair), and `lib` is kept beside the pointers so they stay valid.
    unsafe {
        Ok(Api {
            open: sym(&lib, b"sd_journal_open\0")?,
            close: sym(&lib, b"sd_journal_close\0")?,
            add_match: sym(&lib, b"sd_journal_add_match\0")?,
            add_disjunction: sym(&lib, b"sd_journal_add_disjunction\0")?,
            add_conjunction: sym(&lib, b"sd_journal_add_conjunction\0")?,
            flush_matches: sym(&lib, b"sd_journal_flush_matches\0")?,
            seek_head: sym(&lib, b"sd_journal_seek_head\0")?,
            seek_tail: sym(&lib, b"sd_journal_seek_tail\0")?,
            seek_realtime_usec: sym(&lib, b"sd_journal_seek_realtime_usec\0")?,
            seek_cursor: sym(&lib, b"sd_journal_seek_cursor\0")?,
            next: sym(&lib, b"sd_journal_next\0")?,
            previous: sym(&lib, b"sd_journal_previous\0")?,
            get_realtime_usec: sym(&lib, b"sd_journal_get_realtime_usec\0")?,
            get_monotonic_usec: sym(&lib, b"sd_journal_get_monotonic_usec\0")?,
            get_cursor: sym(&lib, b"sd_journal_get_cursor\0")?,
            set_data_threshold: sym(&lib, b"sd_journal_set_data_threshold\0")?,
            restart_data: sym(&lib, b"sd_journal_restart_data\0")?,
            enumerate_available_data: sym(&lib, b"sd_journal_enumerate_available_data\0")?,
            query_unique: sym(&lib, b"sd_journal_query_unique\0")?,
            enumerate_available_unique: sym(&lib, b"sd_journal_enumerate_available_unique\0")?,
            get_fd: sym(&lib, b"sd_journal_get_fd\0")?,
            wait: sym(&lib, b"sd_journal_wait\0")?,
            _lib: lib,
        })
    }
}

/// # Safety
/// `T` must be the symbol's C function type.
unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, libloading::Error> {
    // SAFETY: the caller guarantees `T`.
    Ok(*unsafe { lib.get::<T>(name)? })
}

fn ok(r: c_int, what: &'static str) -> Result<c_int> {
    if r < 0 {
        return Err(std::io::Error::from_raw_os_error(-r)).context(what);
    }
    Ok(r)
}

pub(super) struct Sd {
    api: &'static Api,
    j: Handle,
    filter: Filter,
    threshold: usize,
    watching: bool,
    tail: Tail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    No,
    /// Seeked to the tail and not stepped since.
    Seeked,
    /// Stepped forward from the tail and found nothing yet.
    Past,
}

// SAFETY: sd_journal(3): "only a single thread may operate on a given object at any given time. Different threads
// may access the same object at different times." Moving an `Sd` hands the handle to one other thread, and every
// call takes `&mut self`; the raw pointer keeps it from being `Sync`.
unsafe impl Send for Sd {}

impl Drop for Sd {
    fn drop(&mut self) {
        // SAFETY: `j` is open, ours alone, and not used after this.
        unsafe { (self.api.close)(self.j) }
    }
}

impl Sd {
    pub(super) fn open() -> Result<Sd> {
        let api = api()?;
        let mut j = std::ptr::null_mut();
        // SAFETY: `j` is a valid out-pointer for the new handle.
        ok(unsafe { (api.open)(&mut j, LOCAL_ONLY) }, "sd_journal_open")?;
        let mut sd = Sd {
            api,
            j,
            filter: Filter::default(),
            threshold: 0,
            watching: false,
            tail: Tail::No,
        };
        sd.set_threshold(DEFAULT_THRESHOLD)?;
        Ok(sd)
    }

    fn call(&mut self, f: unsafe extern "C" fn(Handle) -> c_int, what: &'static str) -> Result<c_int> {
        // SAFETY: `f` is an sd_journal function taking only the handle, which is open and used by this thread
        // alone (`&mut self`).
        ok(unsafe { f(self.j) }, what)
    }

    fn flush(&mut self) {
        // SAFETY: as in `call`.
        unsafe { (self.api.flush_matches)(self.j) }
    }

    fn add_match(&mut self, m: &str) -> Result<()> {
        // SAFETY: `m` is readable for `m.len()` bytes; libsystemd copies it.
        ok(
            unsafe { (self.api.add_match)(self.j, m.as_ptr().cast(), m.len()) },
            "sd_journal_add_match",
        )?;
        Ok(())
    }

    fn apply(&mut self, filter: &Filter) -> Result<()> {
        self.flush();
        for group in &filter.0 {
            for (f, v) in group {
                self.add_match(&format!("{f}={v}"))?;
                self.call(self.api.add_disjunction, "sd_journal_add_disjunction")?;
            }
            self.call(self.api.add_conjunction, "sd_journal_add_conjunction")?;
        }
        Ok(())
    }

    pub(super) fn set_filter(&mut self, filter: &Filter) -> Result<()> {
        // A half-applied filter would pass entries the caller meant to exclude.
        if let Err(e) = self.apply(filter) {
            let old = std::mem::take(&mut self.filter);
            self.apply(&old)?;
            self.filter = old;
            return Err(e);
        }
        self.filter = filter.clone();
        Ok(())
    }

    pub(super) fn set_threshold(&mut self, bytes: usize) -> Result<()> {
        // SAFETY: as in `call`.
        ok(
            unsafe { (self.api.set_data_threshold)(self.j, bytes) },
            "sd_journal_set_data_threshold",
        )?;
        self.threshold = bytes;
        Ok(())
    }

    pub(super) fn seek(&mut self, to: &Seek) -> Result<()> {
        self.tail = if *to == Seek::Tail { Tail::Seeked } else { Tail::No };
        match to {
            Seek::Head => self.call(self.api.seek_head, "sd_journal_seek_head")?,
            Seek::Tail => self.call(self.api.seek_tail, "sd_journal_seek_tail")?,
            // SAFETY: as in `call`.
            Seek::Realtime(t) => ok(
                unsafe { (self.api.seek_realtime_usec)(self.j, *t) },
                "sd_journal_seek_realtime",
            )?,
            Seek::Cursor(c) => {
                let c = CString::new(c.as_str()).context("cursor contains NUL")?;
                // SAFETY: `c` is a NUL-terminated string that outlives the call.
                ok(
                    unsafe { (self.api.seek_cursor)(self.j, c.as_ptr()) },
                    "sd_journal_seek_cursor",
                )?
            }
        };
        Ok(())
    }

    pub(super) fn step(&mut self, forward: bool, fields: Fields) -> Result<Option<Entry>> {
        // From the tail itself sd_journal_next never finds entries appended later, so the first forward step anchors
        // on the last entry (or the head, when none passes the filter yet) and steps on from there. Until that finds
        // an entry, a step back starts from the tail again and lands on the last entry.
        let tail = std::mem::replace(&mut self.tail, Tail::No);
        match (tail, forward) {
            (Tail::Seeked, true) => {
                if self.call(self.api.previous, "sd_journal_previous")? == 0 {
                    self.call(self.api.seek_head, "sd_journal_seek_head")?;
                }
            }
            (Tail::Past, false) => {
                self.call(self.api.seek_tail, "sd_journal_seek_tail")?;
            }
            _ => {}
        }
        let f = if forward { self.api.next } else { self.api.previous };
        if self.call(f, "sd_journal_next")? == 0 {
            if tail != Tail::No {
                self.tail = if forward { Tail::Past } else { Tail::Seeked };
            }
            return Ok(None);
        }
        self.read(fields).map(Some)
    }

    fn realtime(&mut self) -> Result<u64> {
        let mut t = 0;
        // SAFETY: as in `call`; `t` is a valid out-pointer.
        ok(
            unsafe { (self.api.get_realtime_usec)(self.j, &mut t) },
            "sd_journal_get_realtime_usec",
        )?;
        Ok(t)
    }

    fn boot_id(&mut self) -> Result<Id128> {
        let mut id = Id128([0; 16]);
        // SAFETY: as in `call`; `id` is a valid, aligned out-pointer and the monotonic one may be null.
        let r = unsafe { (self.api.get_monotonic_usec)(self.j, std::ptr::null_mut(), &mut id) };
        ok(r, "sd_journal_get_monotonic_usec")?;
        Ok(id)
    }

    /// The data is valid only until the next call on the handle.
    fn data(&self, f: unsafe extern "C" fn(Handle, *mut *const c_void, *mut usize) -> c_int) -> Result<Option<&[u8]>> {
        let (mut p, mut len) = (std::ptr::null(), 0);
        // SAFETY: as in `call`; `p` and `len` are valid out-pointers.
        if ok(unsafe { f(self.j, &mut p, &mut len) }, "sd_journal_enumerate_data")? == 0 {
            return Ok(None);
        }
        // SAFETY: libsystemd returned `len` readable bytes at `p`; the borrow of `self` ends before the next call.
        Ok(Some(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) }))
    }

    fn read(&mut self, fields: Fields) -> Result<Entry> {
        let mut e = Entry {
            t_us: self.realtime()?,
            ..Entry::default()
        };
        let only = match fields {
            Fields::All => None,
            Fields::Only(list) => Some(list),
        };
        let wanted = only.map(|l| l.iter().filter(|f| !f.starts_with("__")).count());
        // SAFETY: as in `call`.
        unsafe { (self.api.restart_data)(self.j) };
        while wanted.is_none_or(|n| e.fields.len() < n)
            && let Some(d) = self.data(self.api.enumerate_available_data)?
        {
            let Some(eq) = d.iter().position(|&b| b == b'=') else {
                continue;
            };
            let name = &d[..eq];
            if only.is_some_and(|l| !l.iter().any(|f| f.as_bytes() == name)) {
                continue;
            }
            let name = String::from_utf8_lossy(name).into_owned();
            let threshold = self.threshold;
            e.fields
                .entry(name)
                .or_insert_with(|| value(eq, &d[eq + 1..], threshold));
        }
        if wants(fields, "__CURSOR") {
            let mut c = std::ptr::null_mut();
            // SAFETY: as in `call`; `c` is a valid out-pointer.
            ok(
                unsafe { (self.api.get_cursor)(self.j, &mut c) },
                "sd_journal_get_cursor",
            )?;
            // SAFETY: on success `c` is a NUL-terminated string libsystemd allocated for the caller to free.
            let cursor = unsafe {
                let s = CStr::from_ptr(c).to_string_lossy().into_owned();
                libc::free(c.cast());
                s
            };
            e.fields.insert("__CURSOR".into(), cursor);
        }
        Ok(e)
    }

    pub(super) fn unique(&mut self, field: &str) -> Result<Vec<String>> {
        let c = CString::new(field)?;
        // SAFETY: as in `call`; `c` is a NUL-terminated string that outlives the call.
        ok(
            unsafe { (self.api.query_unique)(self.j, c.as_ptr()) },
            "sd_journal_query_unique",
        )?;
        let mut out = vec![];
        while let Some(d) = self.data(self.api.enumerate_available_unique)? {
            if let Some(v) = d.get(field.len() + 1..) {
                out.push(value(field.len(), v, self.threshold));
            }
        }
        Ok(out)
    }

    pub(super) fn boots(&mut self) -> Result<Vec<Boot>> {
        self.tail = Tail::No;
        let boots = self.walk_boots();
        let filter = std::mem::take(&mut self.filter);
        self.set_filter(&filter)?;
        boots
    }

    /// From the head, the next entry of another boot finds that boot; a `_BOOT_ID` match from the head and from the
    /// tail finds its first and last entries, and stepping on from the last without the match reaches the next boot.
    /// The first entry by match can come before the one the walk met, when files interleave at boot.
    fn walk_boots(&mut self) -> Result<Vec<Boot>> {
        self.flush();
        self.call(self.api.seek_head, "sd_journal_seek_head")?;
        let mut boots = vec![];
        let mut prev = None;
        loop {
            self.flush();
            let id = loop {
                if self.call(self.api.next, "sd_journal_next")? == 0 {
                    return Ok(boots);
                }
                let id = self.boot_id()?;
                if prev != Some(id) {
                    break id;
                }
            };
            let hex = id.0.iter().fold(String::with_capacity(32), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            });
            self.add_match(&format!("_BOOT_ID={hex}"))?;
            self.call(self.api.seek_head, "sd_journal_seek_head")?;
            if self.call(self.api.next, "sd_journal_next")? == 0 {
                bail!("boot {hex} lost its entries while listing boots");
            }
            let first_us = self.realtime()?;
            self.call(self.api.seek_tail, "sd_journal_seek_tail")?;
            if self.call(self.api.previous, "sd_journal_previous")? == 0 {
                bail!("boot {hex} lost its entries while listing boots");
            }
            boots.push(Boot {
                id: hex,
                first_us,
                last_us: self.realtime()?,
            });
            prev = Some(id);
        }
    }

    pub(super) fn wait(&mut self, timeout: Duration) -> Result<Change> {
        if !self.watching {
            self.call(self.api.get_fd, "sd_journal_get_fd")?;
            self.watching = true;
        }
        let us = u64::try_from(timeout.as_micros()).unwrap_or(u64::MAX);
        // SAFETY: as in `call`.
        let r = unsafe { (self.api.wait)(self.j, us) };
        if r == -libc::EINTR {
            return Ok(Change::Nothing);
        }
        Ok(match ok(r, "sd_journal_wait")? {
            1 => Change::Appended,
            2 => Change::FilesChanged,
            _ => Change::Nothing,
        })
    }
}
