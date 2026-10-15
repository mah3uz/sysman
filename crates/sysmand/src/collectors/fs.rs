use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use anyhow::Result;
use schema::fs::Filesystem;
use serde_json::Value;

use crate::collector::Collector;
use crate::sys::Sys;

pub use sysman_config::modules::fs::Options;

pub struct Usage {
    pub frsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
}

type Statvfs = fn(&Path) -> io::Result<Usage>;

fn statvfs(path: &Path) -> io::Result<Usage> {
    let s = rustix::fs::statvfs(path)?;
    Ok(Usage {
        frsize: s.f_frsize,
        blocks: s.f_blocks,
        bfree: s.f_bfree,
        bavail: s.f_bavail,
        files: s.f_files,
        ffree: s.f_ffree,
    })
}

pub struct Fs {
    sys: Sys,
    opts: Options,
    statvfs: Statvfs,
}

impl Fs {
    pub fn new(opts: Options, sys: Sys) -> Result<Self> {
        Ok(Self { sys, opts, statvfs })
    }
}

struct Mount {
    dev_id: String,
    mount_point: String,
    fstype: String,
    source: String,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = (b[i] == b'\\')
            .then(|| s.get(i + 1..i + 4))
            .flatten()
            .filter(|d| d.bytes().all(|c| (b'0'..=b'7').contains(&c)))
            .and_then(|d| u8::from_str_radix(d, 8).ok());
        match octal {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let sep = f.iter().position(|w| *w == "-")?;
            if sep < 6 || f.len() < sep + 3 {
                return None;
            }
            Some(Mount {
                dev_id: f[2].to_string(),
                mount_point: unescape(f[4]),
                fstype: f[sep + 1].to_string(),
                source: unescape(f[sep + 2]),
            })
        })
        .collect()
}

impl Collector for Fs {
    fn sample(&mut self) -> Result<Option<Value>> {
        let mounts = parse_mountinfo(&self.sys.read("/proc/self/mountinfo")?);
        // btrfs subvolumes and bind mounts share a maj:min and would repeat the same totals
        let mut by_dev: BTreeMap<&str, (usize, Vec<&Mount>)> = BTreeMap::new();
        for (i, m) in mounts.iter().enumerate() {
            if m.source.starts_with("/dev/") && !self.opts.exclude_fstypes.contains(&m.fstype) {
                by_dev.entry(&m.dev_id).or_insert_with(|| (i, Vec::new())).1.push(m);
            }
        }
        let mut groups: Vec<(usize, Vec<&Mount>)> = by_dev.into_values().collect();
        groups.sort_by_key(|(i, _)| *i);
        let mut out: schema::fs::Fs = BTreeMap::new();
        for (_, mut group) in groups {
            group.sort_by(|a, b| (a.mount_point.len(), &a.mount_point).cmp(&(b.mount_point.len(), &b.mount_point)));
            group.dedup_by(|a, b| a.mount_point == b.mount_point);
            let Some((first, st)) = group
                .iter()
                .find_map(|m| Some((*m, (self.statvfs)(Path::new(&m.mount_point)).ok()?)))
            else {
                continue;
            };
            // an overmount hides the earlier device; the first in mount order keeps the entry
            if out.contains_key(&first.mount_point) {
                continue;
            }
            let used = st.blocks.saturating_sub(st.bfree) * st.frsize;
            let avail = st.bavail * st.frsize;
            out.insert(
                first.mount_point.clone(),
                Filesystem {
                    device: first.source.clone(),
                    fstype: first.fstype.clone(),
                    mounts: group.iter().map(|m| m.mount_point.clone()).collect(),
                    total_bytes: st.blocks * st.frsize,
                    used_bytes: used,
                    avail_bytes: avail,
                    used_pct: (used + avail > 0).then(|| (1000.0 * used as f64 / (used + avail) as f64).round() / 10.0),
                    inodes_total: st.files,
                    inodes_free: st.ffree,
                },
            );
        }
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    const THIS_MACHINE: &str = "\
31 2 0:28 /@ / rw,relatime shared:1 - btrfs /dev/nvme1n1p2 rw,compress=zstd:3,ssd,discard=async,space_cache=v2,subvolid=256,subvol=/@
33 28 0:31 / /dev/shm rw,nosuid,nodev shared:3 - tmpfs tmpfs rw,inode64,huge=advise,usrquota
56 31 0:28 /@home /home rw,relatime shared:193 - btrfs /dev/nvme1n1p2 rw,compress=zstd:3,ssd,discard=async,space_cache=v2,subvolid=257,subvol=/@home
101 31 0:28 /@pkg /var/cache/pacman/pkg rw,relatime shared:199 - btrfs /dev/nvme1n1p2 rw,compress=zstd:3,ssd,discard=async,space_cache=v2,subvolid=259,subvol=/@pkg
213 31 259:7 / /boot rw,relatime shared:211 - vfat /dev/nvme1n1p1 rw,fmask=0077,dmask=0077,codepage=437,iocharset=ascii,shortname=mixed,utf8,errors=remount-ro
";

    fn fake_statvfs(path: &Path) -> io::Result<Usage> {
        match path.to_str().unwrap() {
            "/" | "/mnt/b" => Ok(Usage {
                frsize: 4096,
                blocks: 1000,
                bfree: 400,
                bavail: 300,
                files: 5000,
                ffree: 4000,
            }),
            "/boot" | "/mnt/my disk" | "/srv/data" | "/mnt/iso" => Ok(Usage {
                frsize: 512,
                blocks: 100,
                bfree: 50,
                bavail: 50,
                files: 0,
                ffree: 0,
            }),
            _ => Err(io::ErrorKind::NotFound.into()),
        }
    }

    fn run(mountinfo: &str, opts: Options) -> Value {
        let f = Fixture::new().file("proc/self/mountinfo", mountinfo);
        let mut c = Fs::new(opts, f.sys()).unwrap();
        c.statvfs = fake_statvfs;
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::fs::Fs>(&v);
        v
    }

    #[test]
    fn octal_escapes_are_decoded() {
        assert_eq!(unescape(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(unescape(r"/a\011b\134c\012d"), "/a\tb\\c\nd");
        assert_eq!(unescape(r"/odd\04"), r"/odd\04");
        assert_eq!(unescape(r"/tail\040"), "/tail ");
        assert_eq!(unescape(r"/caf\303\251"), "/café");
    }

    #[test]
    fn mount_point_with_a_space_is_found_by_its_real_path() {
        // statvfs on the escaped text would fail and drop the filesystem
        let v = run(
            "40 2 8:1 / /mnt/my\\040disk rw - ext4 /dev/sda1 rw\n",
            Options::default(),
        );
        assert_eq!(v["/mnt/my disk"]["device"], "/dev/sda1");
    }

    #[test]
    fn btrfs_subvolumes_are_one_filesystem_keyed_by_shortest_mount() {
        let v = run(THIS_MACHINE, Options::default());
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["/", "/boot"]);
        assert_eq!(
            v["/"]["mounts"],
            serde_json::json!(["/", "/home", "/var/cache/pacman/pkg"])
        );
        assert_eq!(v["/"]["fstype"], "btrfs");
        assert_eq!(v["/"]["device"], "/dev/nvme1n1p2");
    }

    #[test]
    fn bind_mount_of_a_filesystem_is_listed_under_the_shortest_mount_point() {
        let v = run(
            "40 2 8:1 / /srv/data rw - ext4 /dev/sda1 rw\n41 2 8:1 /data /mnt/b rw - ext4 /dev/sda1 rw\n",
            Options::default(),
        );
        assert_eq!(v.as_object().unwrap().len(), 1);
        assert_eq!(v["/mnt/b"]["mounts"], serde_json::json!(["/mnt/b", "/srv/data"]));
    }

    #[test]
    fn excluded_fstypes_and_non_dev_sources_are_skipped() {
        let v = run(
            "40 2 7:0 / /mnt/iso ro - iso9660 /dev/sr0 ro\n41 2 0:31 / /run tmpfs tmpfs rw\n42 2 8:1 / /srv/data rw - ext4 /dev/sda1 rw\n",
            Options::default(),
        );
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["/srv/data"]);

        let v = run(
            "40 2 7:0 / /mnt/iso ro - iso9660 /dev/sr0 ro\n",
            Options {
                exclude_fstypes: vec![],
            },
        );
        assert!(v.get("/mnt/iso").is_some());
    }

    #[test]
    fn used_pct_is_df_style_and_excludes_reserved_blocks_from_the_denominator() {
        // used 600, avail 300 of 1000 blocks (100 reserved): df shows 66.7%, not 60%
        let v = run(THIS_MACHINE, Options::default());
        assert_eq!(v["/"]["used_pct"], 66.7);
        assert_eq!(v["/"]["used_bytes"], 600 * 4096);
        assert_eq!(v["/"]["avail_bytes"], 300 * 4096);
        assert_eq!(v["/"]["total_bytes"], 1000 * 4096);
        assert_eq!(v["/boot"]["used_pct"], 50.0);
    }

    #[test]
    fn filesystem_with_no_blocks_has_null_used_pct() {
        let f = Fixture::new().file("proc/self/mountinfo", "40 2 8:1 / /srv/data rw - ext4 /dev/sda1 rw\n");
        let mut c = Fs::new(Options::default(), f.sys()).unwrap();
        c.statvfs = |_| {
            Ok(Usage {
                frsize: 4096,
                blocks: 0,
                bfree: 0,
                bavail: 0,
                files: 0,
                ffree: 0,
            })
        };
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::fs::Fs>(&v);
        assert!(v["/srv/data"]["used_pct"].is_null());
    }

    #[test]
    fn device_overmounting_another_mount_point_is_skipped_in_favour_of_the_first_in_mount_order() {
        let v = run(
            "40 2 8:2 / /srv/data rw - ext4 /dev/sdb1 rw\n41 2 8:1 / /srv/data rw - ext4 /dev/sda1 rw\n",
            Options::default(),
        );
        assert_eq!(v.as_object().unwrap().len(), 1);
        assert_eq!(v["/srv/data"]["device"], "/dev/sdb1");
    }

    #[test]
    fn another_mount_point_of_the_device_is_tried_when_the_first_cannot_be_statted() {
        let v = run(
            "40 2 8:1 / /gone rw - ext4 /dev/sda1 rw\n41 2 8:1 /d /srv/data rw - ext4 /dev/sda1 rw\n",
            Options::default(),
        );
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["/srv/data"]);
        assert_eq!(v["/srv/data"]["mounts"], serde_json::json!(["/gone", "/srv/data"]));
    }

    #[test]
    fn mount_that_cannot_be_statted_is_skipped() {
        let v = run("40 2 8:9 / /gone rw - ext4 /dev/sdb1 rw\n", Options::default());
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn missing_mountinfo_is_an_error() {
        let mut c = Fs::new(Options::default(), Fixture::new().sys()).unwrap();
        assert!(c.sample().is_err());
    }
}
