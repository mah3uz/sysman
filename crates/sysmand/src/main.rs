mod collector;
mod collectors;
mod config;
mod daemon;
mod db;
mod deltas;
mod jsonl;
mod pool;
mod sys;
#[cfg(test)]
mod testutil;

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::collector::MODULES;
use crate::config::Config;
use crate::daemon::{Daemon, local_date, unix_now};
use crate::jsonl::Jsonl;
use crate::sys::Sys;

const USAGE: &str = "usage: sysmand [--config PATH] [--once]

  --config PATH  config file (default: $XDG_CONFIG_HOME/sysman/config.toml)
  --once         print one meta and one sample record, write nothing";

fn main() {
    if let Err(e) = run() {
        eprintln!("sysmand: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut config_path = config::config_path();
    let mut once = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => match args.next() {
                Some(p) => config_path = PathBuf::from(p),
                None => bail!("--config needs a path\n\n{USAGE}"),
            },
            "--once" => once = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => bail!("unknown argument {arg:?}\n\n{USAGE}"),
        }
    }

    let config = Config::load(&config_path)?;
    let (fsync, retention) = (config.general.fsync, config.retention.clone());
    let mut daemon = Daemon::new(config, Sys::host(), MODULES)?;
    let base = daemon.base();

    // the first reading only primes the rate counters
    daemon.sample_record(None);

    if once {
        thread::sleep(Duration::from_secs(base));
        println!("{}", serde_json::to_string_pretty(&daemon.meta_record())?);
        let sample = daemon.sample_record(None).unwrap_or_default();
        println!("{}", serde_json::to_string_pretty(&sample)?);
        return Ok(());
    }

    let data = config::data_dir();
    let mut log = Jsonl::new(data.join("log"), fsync, retention.log_days)?;
    let snapshots = MODULES
        .iter()
        .filter(|s| s.snapshot)
        .map(|s| s.name.to_string())
        .collect();
    let mut history = db::Writer::spawn(db::Db::open(&data.join("history.db"), retention, snapshots)?);
    loop {
        let now = unix_now();
        thread::sleep(Duration::from_secs_f64(base as f64 - now % base as f64));
        let now = unix_now();
        let today = local_date(now);
        if log.day() != Some(today) {
            log.open(today)?;
            let meta = daemon.meta_record();
            log.write(&meta)?;
            history.send(meta);
        }
        let tick = (now / base as f64).round() as u64;
        if let Some(record) = daemon.sample_record(Some(tick)) {
            log.write(&record)?;
            history.send(record);
        }
    }
}
