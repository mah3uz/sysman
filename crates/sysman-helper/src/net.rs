use std::collections::{BTreeMap, HashMap};

use anyhow::Result;

/// TCP and UDP bytes per process (tgid → rx, tx), counted by the eBPF program since it was attached.
pub struct Net {
    map: libbpf_rs::MapHandle,
    /// What is not counted and why, when some programs could not be loaded or attached.
    pub missing: Option<String>,
}

mod skel {
    include!(concat!(env!("OUT_DIR"), "/net.skel.rs"));
}

/// A program in `net.bpf.c`, and what is lost without it.
struct Hook {
    prog: &'static str,
    /// The kernel function it hooks and that function's number of arguments, which the program must match.
    kernel: Option<(&'static str, usize)>,
    lost: &'static str,
}

const TCP_SEND: &str = "TCP send not counted";
const TCP_RECEIVE: &str = "TCP receive not counted";

const HOOKS: &[Hook] = &[
    Hook {
        prog: "tcp_sendmsg_exit",
        kernel: Some(("tcp_sendmsg", 3)),
        lost: TCP_SEND,
    },
    Hook {
        prog: "tcp_cleanup_rbuf_exit",
        kernel: Some(("tcp_cleanup_rbuf", 2)),
        lost: TCP_RECEIVE,
    },
    Hook {
        prog: "udp_sendmsg_exit",
        kernel: Some(("udp_sendmsg", 3)),
        lost: "UDP send not counted",
    },
    Hook {
        prog: "udp_recvmsg_exit",
        kernel: Some(("udp_recvmsg", 4)),
        lost: "UDP receive not counted",
    },
    Hook {
        prog: "udp_recvmsg_addr_len_exit",
        kernel: Some(("udp_recvmsg", 5)),
        lost: "UDP receive not counted",
    },
    Hook {
        prog: "udp_recvmsg_noblock_exit",
        kernel: Some(("udp_recvmsg", 6)),
        lost: "UDP receive not counted",
    },
    Hook {
        prog: "udpv6_sendmsg_exit",
        kernel: Some(("udpv6_sendmsg", 3)),
        lost: "UDP over IPv6 send not counted",
    },
    Hook {
        prog: "udpv6_recvmsg_exit",
        kernel: Some(("udpv6_recvmsg", 4)),
        lost: "UDP over IPv6 receive not counted",
    },
    Hook {
        prog: "udpv6_recvmsg_addr_len_exit",
        kernel: Some(("udpv6_recvmsg", 5)),
        lost: "UDP over IPv6 receive not counted",
    },
    Hook {
        prog: "udpv6_recvmsg_noblock_exit",
        kernel: Some(("udpv6_recvmsg", 6)),
        lost: "UDP over IPv6 receive not counted",
    },
    Hook {
        prog: "process_exit",
        kernel: None,
        lost: "exited processes not dropped from the map",
    },
];

impl Net {
    pub fn load() -> Result<Self> {
        use std::mem::MaybeUninit;
        use std::sync::mpsc;

        use anyhow::Context;

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("bpf".into()).spawn(move || {
            let (mut first, mut second) = (MaybeUninit::uninit(), MaybeUninit::uninit());
            match start(&mut first, &mut second) {
                Ok((loaded, _links, missing)) => {
                    let map = libbpf_rs::MapHandle::try_from(&loaded.maps.counts).map_err(Into::into);
                    let _ = tx.send(map.map(|map| Self { map, missing }));
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
        rx.recv().context("the eBPF loader thread died")?
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

type Started<'obj> = (skel::NetSkel<'obj>, Vec<libbpf_rs::Link>, Option<String>);

/// Loads and attaches every program that works on this kernel; fails only when TCP cannot be counted.
fn start<'obj>(
    first: &'obj mut std::mem::MaybeUninit<libbpf_rs::OpenObject>,
    second: &'obj mut std::mem::MaybeUninit<libbpf_rs::OpenObject>,
) -> Result<Started<'obj>> {
    use anyhow::Context;
    use libbpf_rs::skel::Skel;

    let btf = libbpf_rs::Btf::from_vmlinux().context("reading the kernel's BTF")?;
    let (mut enabled, mut why) = choose(&arities(&btf));
    let loaded = match load(first, &enabled) {
        Ok(loaded) => loaded,
        Err(e) => {
            // the verifier rejecting one program fails the whole object, so find which by loading each alone
            enabled.retain(|&prog| {
                let mut object = std::mem::MaybeUninit::uninit();
                let alone = load(&mut object, &[prog]).map(drop);
                if let Err(e) = &alone {
                    why.entry(lost(prog)).or_insert_with(|| format!("{e:#}"));
                }
                alone.is_ok()
            });
            if enabled.is_empty() {
                return Err(e);
            }
            load(second, &enabled)?
        }
    };
    let mut links = Vec::new();
    for prog in loaded.object().progs_mut() {
        let Some(name) = prog.name().to_str().filter(|name| enabled.contains(name)) else {
            continue;
        };
        match prog.attach() {
            Ok(link) => links.push(link),
            Err(e) => {
                why.entry(lost(name)).or_insert_with(|| format!("attaching: {e:#}"));
            }
        }
    }
    let describe = || {
        why.iter()
            .map(|(lost, e)| format!("{lost}: {e}"))
            .collect::<Vec<_>>()
            .join("; ")
    };
    if why.contains_key(TCP_SEND) || why.contains_key(TCP_RECEIVE) {
        anyhow::bail!(describe());
    }
    let missing = (!why.is_empty()).then(describe);
    Ok((loaded, links, missing))
}

fn load<'obj>(
    object: &'obj mut std::mem::MaybeUninit<libbpf_rs::OpenObject>,
    enabled: &[&str],
) -> Result<skel::NetSkel<'obj>> {
    use anyhow::Context;
    use libbpf_rs::skel::{OpenSkel, SkelBuilder};

    let mut open = skel::NetSkelBuilder::default().open(object).context("opening")?;
    for mut prog in open.open_object_mut().progs_mut() {
        let on = prog.name().to_str().is_some_and(|name| enabled.contains(&name));
        prog.set_autoload(on);
    }
    open.load().context("loading")
}

fn lost(prog: &str) -> &'static str {
    HOOKS
        .iter()
        .find(|h| h.prog == prog)
        .map_or("some traffic not counted", |h| h.lost)
}

/// Number of arguments of each hooked kernel function, from the kernel's BTF.
fn arities(btf: &libbpf_rs::Btf) -> HashMap<&'static str, usize> {
    use libbpf_rs::ReferencesType;
    use libbpf_rs::btf::types::{Func, FuncProto};

    let mut out = HashMap::new();
    for func in btf.type_by_kind::<Func>() {
        let Some(name) = func.name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((hooked, _)) = HOOKS.iter().filter_map(|h| h.kernel).find(|(f, _)| *f == name) else {
            continue;
        };
        if let Ok(proto) = FuncProto::try_from(func.referenced_type()) {
            // fexit attaches to the first function of that name, as libbpf finds it
            out.entry(hooked).or_insert(proto.len());
        }
    }
    out
}

/// The programs to load, at most one variant per kernel function, and why each lost count is lost.
fn choose(arities: &HashMap<&str, usize>) -> (Vec<&'static str>, BTreeMap<&'static str, String>) {
    let mut enabled = Vec::new();
    let mut why = BTreeMap::new();
    for hook in HOOKS {
        let Some((func, args)) = hook.kernel else {
            enabled.push(hook.prog);
            continue;
        };
        match arities.get(func) {
            Some(&n) if n == args => enabled.push(hook.prog),
            Some(&n) => {
                why.insert(hook.lost, format!("{func} takes {n} arguments"));
            }
            None => {
                why.insert(hook.lost, format!("{func} is not in the kernel's BTF"));
            }
        }
    }
    // the variant that does not match leaves a reason although its sibling counts the same traffic
    for hook in HOOKS.iter().filter(|h| enabled.contains(&h.prog)) {
        why.remove(hook.lost);
    }
    (enabled, why)
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

    fn kernel(udp_recvmsg: usize) -> HashMap<&'static str, usize> {
        HashMap::from([
            ("tcp_sendmsg", 3),
            ("tcp_cleanup_rbuf", 2),
            ("udp_sendmsg", 3),
            ("udp_recvmsg", udp_recvmsg),
            ("udpv6_sendmsg", 3),
            ("udpv6_recvmsg", udp_recvmsg),
        ])
    }

    #[test]
    fn map_value_is_rx_then_tx() {
        // the order must match struct counts in net.bpf.c, or received and sent swap silently
        let mut v = 7u64.to_ne_bytes().to_vec();
        v.extend(9u64.to_ne_bytes());
        assert_eq!(parse_counts(&v), Some((7, 9)));
        assert_eq!(parse_counts(&v[..12]), None);
    }

    #[test]
    fn the_receive_variant_follows_the_kernels_argument_count() {
        // a program declaring the wrong arity reads past the context and the verifier rejects it
        for (args, suffix) in [(4, "_exit"), (5, "_addr_len_exit"), (6, "_noblock_exit")] {
            let (enabled, why) = choose(&kernel(args));
            assert!(why.is_empty(), "{args}: {why:?}");
            let receive: Vec<_> = enabled.iter().filter(|p| p.contains("recvmsg")).collect();
            assert_eq!(
                receive,
                [&format!("udp_recvmsg{suffix}"), &format!("udpv6_recvmsg{suffix}")],
                "{args} arguments"
            );
            assert!(enabled.contains(&"tcp_sendmsg_exit") && enabled.contains(&"process_exit"));
        }
    }

    #[test]
    fn an_unknown_signature_drops_only_its_own_count_with_a_reason() {
        let mut arities = kernel(7);
        arities.remove("udpv6_sendmsg");
        let (enabled, why) = choose(&arities);
        assert!(
            !enabled.iter().any(|p| p.contains("recvmsg") || p.starts_with("udpv6")),
            "{enabled:?}"
        );
        assert!(enabled.contains(&"udp_sendmsg_exit"));
        assert!(enabled.contains(&"tcp_sendmsg_exit") && enabled.contains(&"tcp_cleanup_rbuf_exit"));
        assert_eq!(why["UDP receive not counted"], "udp_recvmsg takes 7 arguments");
        assert_eq!(
            why["UDP over IPv6 send not counted"],
            "udpv6_sendmsg is not in the kernel's BTF"
        );
        assert!(!why.contains_key(TCP_SEND) && !why.contains_key("UDP send not counted"));
    }

    #[test]
    fn every_program_in_the_object_has_a_hook() {
        // a program missing from HOOKS would never be enabled, so its traffic would vanish without a reason
        use libbpf_rs::skel::{OpenSkel, SkelBuilder};

        let mut object = std::mem::MaybeUninit::uninit();
        let open = skel::NetSkelBuilder::default().open(&mut object).unwrap();
        let progs: Vec<String> = open
            .open_object()
            .progs()
            .map(|p| p.name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(progs.len(), HOOKS.len());
        for prog in progs {
            assert!(HOOKS.iter().any(|h| h.prog == prog), "{prog}");
        }
    }

    #[test]
    fn this_kernel_matches_one_variant_of_every_hook() {
        // runs unprivileged: /sys/kernel/btf/vmlinux is world-readable; this machine's kernel has neither noblock nor addr_len
        let btf = libbpf_rs::Btf::from_vmlinux().expect("reading /sys/kernel/btf/vmlinux");
        let arities = arities(&btf);
        assert_eq!(arities.get("udp_recvmsg"), Some(&4), "{arities:?}");
        assert_eq!(arities.get("udpv6_recvmsg"), Some(&4), "{arities:?}");
        let (enabled, why) = choose(&arities);
        assert!(why.is_empty(), "{why:?} with {arities:?}");
        assert!(enabled.contains(&"udp_recvmsg_exit") && enabled.contains(&"udpv6_recvmsg_exit"));
        assert_eq!(
            enabled.iter().filter(|p| p.contains("recvmsg")).count(),
            2,
            "{enabled:?}"
        );
    }
}
