/// Tables of `history.db`, shared by the daemon that writes it and the app that reads it.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS series (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS points (
    series_id INTEGER NOT NULL, t INTEGER NOT NULL, value REAL NOT NULL,
    PRIMARY KEY (series_id, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rollup_1m (
    series_id INTEGER NOT NULL, t INTEGER NOT NULL,
    min REAL NOT NULL, avg REAL NOT NULL, max REAL NOT NULL, count INTEGER NOT NULL,
    PRIMARY KEY (series_id, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rollup_15m (
    series_id INTEGER NOT NULL, t INTEGER NOT NULL,
    min REAL NOT NULL, avg REAL NOT NULL, max REAL NOT NULL, count INTEGER NOT NULL,
    PRIMARY KEY (series_id, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS snapshots (
    module TEXT NOT NULL, t INTEGER NOT NULL, data TEXT NOT NULL,
    PRIMARY KEY (module, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS errors (
    t INTEGER NOT NULL, module TEXT NOT NULL, message TEXT NOT NULL,
    PRIMARY KEY (t, module)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS meta (t INTEGER PRIMARY KEY, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS procs (
    id INTEGER PRIMARY KEY, pid INTEGER NOT NULL, start_time INTEGER NOT NULL, comm TEXT NOT NULL,
    cmdline TEXT, user TEXT, first_t INTEGER NOT NULL, last_t INTEGER NOT NULL,
    UNIQUE (pid, start_time));
CREATE TABLE IF NOT EXISTS proc_points (
    proc_id INTEGER NOT NULL, t INTEGER NOT NULL, cpu_pct REAL, rss_bytes INTEGER, threads INTEGER,
    fds INTEGER, read_bps REAL, write_bps REAL,
    PRIMARY KEY (proc_id, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS proc_rollup_1m (
    proc_id INTEGER NOT NULL, t INTEGER NOT NULL, cpu_avg REAL, cpu_max REAL, rss_avg REAL, rss_max INTEGER,
    threads_max INTEGER, fds_max INTEGER, read_bps_avg REAL, write_bps_avg REAL, count INTEGER NOT NULL,
    PRIMARY KEY (proc_id, t)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS procs_comm ON procs (comm);
CREATE INDEX IF NOT EXISTS procs_last ON procs (last_t);
";
