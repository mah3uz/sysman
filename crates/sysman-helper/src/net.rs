use std::collections::HashMap;

use anyhow::Result;

/// TCP and UDP bytes per process (tgid → rx, tx), counted by the eBPF program since it was attached.
pub struct Net {
    map: libbpf_rs::MapHandle,
}

mod skel {
    include!(concat!(env!("OUT_DIR"), "/net.skel.rs"));
}

impl Net {
    pub fn load() -> Result<Self> {
        use std::mem::MaybeUninit;
        use std::sync::mpsc;

        use anyhow::Context;

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("bpf".into()).spawn(move || {
            let mut object = MaybeUninit::uninit();
            match attach(&mut object) {
                Ok(loaded) => {
                    let _ = tx.send(libbpf_rs::MapHandle::try_from(&loaded.maps.counts).map_err(Into::into));
                    // the programs stay attached only while this thread keeps their links alive
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        })?;
        Ok(Self {
            map: rx.recv().context("the eBPF loader thread died")??,
        })
    }

    pub fn counts(&self) -> HashMap<u32, (u64, u64)> {
        use libbpf_rs::{MapCore, MapFlags};

        let mut out = HashMap::new();
        for key in self.map.keys() {
            let (Ok(k), Ok(Some(v))) = (
                <[u8; 4]>::try_from(key.as_slice()),
                self.map.lookup(&key, MapFlags::ANY),
            ) else {
                continue;
            };
            if let Some(counts) = parse_counts(&v) {
                out.insert(u32::from_ne_bytes(k), counts);
            }
        }
        out
    }
}

fn attach(object: &mut std::mem::MaybeUninit<libbpf_rs::OpenObject>) -> Result<skel::NetSkel<'_>> {
    use anyhow::Context;
    use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};

    let open = skel::NetSkelBuilder::default().open(object).context("opening")?;
    let mut loaded = open.load().context("loading")?;
    loaded.attach().context("attaching")?;
    Ok(loaded)
}

/// The map value is `struct counts { u64 rx; u64 tx; }` in native byte order.
fn parse_counts(v: &[u8]) -> Option<(u64, u64)> {
    let rx = u64::from_ne_bytes(v.get(0..8)?.try_into().ok()?);
    let tx = u64::from_ne_bytes(v.get(8..16)?.try_into().ok()?);
    Some((rx, tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_value_is_rx_then_tx() {
        // the order must match struct counts in net.bpf.c, or received and sent swap silently
        let mut v = 7u64.to_ne_bytes().to_vec();
        v.extend(9u64.to_ne_bytes());
        assert_eq!(parse_counts(&v), Some((7, 9)));
        assert_eq!(parse_counts(&v[..12]), None);
    }
}
