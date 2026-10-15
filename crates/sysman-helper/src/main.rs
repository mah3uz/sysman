mod net;
mod procs;
mod rapl;
mod server;
mod smart;

use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::server::Sources;

const USAGE: &str = "usage: sysman-helper [--socket PATH] [--group NAME]

  --socket PATH  where to listen (default: /run/sysman-helper/helper.sock)
  --group NAME   group allowed to connect (default: wheel)";

fn main() {
    if let Err(e) = run() {
        eprintln!("sysman-helper: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut socket = PathBuf::from("/run/sysman-helper/helper.sock");
    let mut group = "wheel".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => match args.next() {
                Some(p) => socket = PathBuf::from(p),
                None => bail!("--socket needs a path\n\n{USAGE}"),
            },
            "--group" => match args.next() {
                Some(g) => group = g,
                None => bail!("--group needs a name\n\n{USAGE}"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => bail!("unknown argument {arg:?}\n\n{USAGE}"),
        }
    }
    let gid = group_id(&group)?;

    if UnixStream::connect(&socket).is_ok() {
        bail!("{} is in use; is another sysman-helper running?", socket.display());
    }
    // nothing answered, so any file there is left over from a helper that died
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    std::os::unix::fs::chown(&socket, None, Some(gid))
        .with_context(|| format!("giving {} to group {group}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))?;

    let root = PathBuf::from("/");
    let sources = Sources {
        net: net::Net::load().map_err(|e| format!("{e:#}")),
        smart: Arc::default(),
        root: root.clone(),
    };
    smart::spawn(root, sources.smart.clone()).context("starting the SMART thread")?;
    let status = sources.status();
    for (feature, on) in &status.features {
        match status.errors.get(feature) {
            Some(why) => eprintln!("sysman-helper: {feature}: {} ({why})", if *on { "on" } else { "off" }),
            None => eprintln!("sysman-helper: {feature}: on"),
        }
    }
    eprintln!("sysman-helper {}: listening on {}", status.version, socket.display());
    server::serve(listener, Arc::new(sources));
    Ok(())
}

fn group_id(name: &str) -> Result<u32> {
    let c = CString::new(name).context("group name")?;
    // SAFETY: c is a valid C string; the returned record is read before any other getgr* call on this thread.
    let gr = unsafe { libc::getgrnam(c.as_ptr()) };
    if gr.is_null() {
        bail!("no group named {name:?}");
    }
    // SAFETY: gr is non-null and points at libc's static group record.
    Ok(unsafe { (*gr).gr_gid })
}
