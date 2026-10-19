use std::time::{Duration, Instant};

use super::*;

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let out = f();
    println!("  {label}: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    out
}

#[test]
#[ignore = "scans this machine's journal; run with CARGO_PROFILE_DEV_OPT_LEVEL=2 and --nocapture"]
fn measure_scans_queries_and_fetches_on_the_local_journal() {
    println!("status: {:?}", status());
    let week = 7 * 86_400 * 1_000_000;
    for (name, scope) in [
        ("this boot", Scope::this_boot()),
        ("7 days", Scope::since(now_us() - week)),
    ] {
        let mut logs = Logs::default();
        let t = Instant::now();
        let scan = logs.scan(scope);
        let mut first_rows = None;
        while scan.state().is_running() {
            std::thread::sleep(Duration::from_millis(10));
            if first_rows.is_none() && !scan.index().is_empty() {
                first_rows = Some(t.elapsed());
            }
        }
        assert_eq!(scan.state(), ScanState::Done);
        let index = scan.index();
        println!(
            "{name}: {} entries, {} rows in {:.2} s (first rows after {:?}), {:.1} MiB, {} findings, backend {:?}",
            scan.entries_read(),
            index.len(),
            t.elapsed().as_secs_f64(),
            first_rows,
            index.memory_bytes() as f64 / 1_048_576.0,
            index.findings().len(),
            scan.backend()
        );
        let now = now_us();
        let parse = |s: &str| Query::parse(s, now, &jiff::tz::TimeZone::system());
        let (from, to) = (index.rows().first().map_or(0, |r| r.t_us), now);
        let empty = parse("");
        let warn = parse("prio:warning cat:kernel");
        let m = timed("matching, no filter", || index.matching(&empty, None));
        println!("    {} visible of {} entries", m.rows.len(), m.entries);
        timed("matching, prio:warning cat:kernel", || index.matching(&warn, None));
        timed("category_counts", || index.category_counts(&warn, None));
        timed("histogram, 200 buckets", || index.histogram(&warn, None, from, to, 200));
        timed("lanes, 200 buckets", || index.lanes(&empty, None, from, to, 200));
        timed("boot_summaries", || index.boot_summaries());
        drop(index);

        let mut fetcher = logs.fetcher().unwrap();
        let visible: Vec<u32> = scan
            .index()
            .matching(&empty, None)
            .rows
            .iter()
            .rev()
            .take(60)
            .map(|v| v.row)
            .collect();
        timed("fetch 60 rows cold", || {
            visible.iter().for_each(|r| drop(fetcher.entry(*r).unwrap()))
        });
        timed("fetch 60 rows cached", || {
            visible.iter().for_each(|r| drop(fetcher.entry(*r).unwrap()))
        });
        let around = timed("around(6)", || fetcher.around(visible[30], 6).unwrap());
        assert_eq!((around.before.len(), around.after.len()), (6, 6));
        for r in visible.iter().take(20) {
            let index = scan.index();
            let row = index.row(*r).unwrap();
            if !row.package {
                let e = fetcher.entry(*r).unwrap().unwrap();
                assert_eq!(e.t_us, row.t_us, "the fetched entry is the row's");
                assert_eq!(super::scan::entry_hash(&e), row.hash);
            }
        }

        let q = parse("error");
        let t = Instant::now();
        let search = logs.search(&q).unwrap();
        while search.state().is_running() {
            std::thread::sleep(Duration::from_millis(10));
        }
        let hits = search.hits().clone();
        println!(
            "  search \"error\": {:?} in {:.2} s, {} hits over {} rows",
            search.state(),
            t.elapsed().as_secs_f64(),
            hits.len(),
            search.rows_done()
        );
        let shown = timed("matching with hits", || scan.index().matching(&q, Some(&hits)));
        println!("    {} visible", shown.rows.len());
    }
}
