//! Workload measurements for "Measurement plan" in docs/database-safety.md. scripts/measure.sh
//! runs each step as one ignored test in a long-lived runner, and collects the Markdown that the
//! steps print.
//!
//! The load runs against the primary at a fixed rate of 50% of the measured maximum: pgbench for
//! PostgreSQL, and the transactions of sysbench's oltp_read_write through
//! tests/workload/oltp_paced.lua for MySQL. The latency of each transaction counts from its
//! scheduled start, so a stall counts in full.

mod common;

use std::{
    cell::RefCell,
    env, fs,
    path::Path,
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use common::{
    cli::{self, MYSQL_SECRETS, POSTGRESQL_SECRETS, Setup},
    floci, mysql, postgresql, ssh,
};
use mysql_async::prelude::Queryable;
use svalbard::db::{Mysql, Postgres, Source as _};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, Command},
    time,
};

/// The load logs, the rates and the samples of scripts/measure.sh.
const DIR: &str = "/tmp/measure";
const CLIENTS: usize = 16;
const MINUTE: Duration = Duration::from_secs(60);
/// The sessions for B8.
const SESSIONS: usize = 500;

/// The length of each measurement window: 5 minutes, or less for a trial run.
fn window_length() -> Duration {
    let seconds = env::var("SVALBARD_MEASURE_WINDOW").map_or(300, |value| value.parse().unwrap());
    Duration::from_secs(seconds)
}

/// The pgbench scale. MySQL gets 4 tables of 5,000 rows per scale unit. The default of 100 is
/// about 1.5 GB for PostgreSQL and 0.5 GB for MySQL: more than the memory of each server.
fn scale() -> u64 {
    env::var("SVALBARD_MEASURE_SCALE").map_or(100, |value| value.parse().unwrap())
}

fn epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// A command on the CPUs of the load, if scripts/measure.sh pins them.
fn pinned(program: &str) -> Command {
    match env::var("SVALBARD_MEASURE_LOAD_CPUS") {
        Ok(cpus) if !cpus.is_empty() => {
            let mut command = Command::new("taskset");
            command.args(["--cpu-list", &cpus, program]);
            command
        }
        _ => Command::new(program),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Db {
    Postgresql,
    Mysql,
}

impl Db {
    fn slug(self) -> &'static str {
        match self {
            Db::Postgresql => "postgresql",
            Db::Mysql => "mysql",
        }
    }

    fn replica(self) -> &'static str {
        match self {
            Db::Postgresql => "standby",
            Db::Mysql => "replica",
        }
    }

    fn password(self) -> &'static str {
        match self {
            Db::Postgresql => postgresql::PASSWORD,
            Db::Mysql => mysql::PASSWORD,
        }
    }

    fn secrets(self) -> &'static [&'static str] {
        match self {
            Db::Postgresql => POSTGRESQL_SECRETS,
            Db::Mysql => MYSQL_SECRETS,
        }
    }

    /// The `[source]` section for the fixture `name`, on the primary or the replica.
    fn source(self, name: &str, replica: bool) -> String {
        match self {
            Db::Postgresql => {
                let fixture = postgresql::Fixture::open(name);
                let fixture = match replica {
                    true => fixture.on(&postgresql::standby()),
                    false => fixture,
                };
                cli::postgresql_source(&fixture)
            }
            Db::Mysql => {
                let fixture = mysql::Fixture::open(name);
                let fixture = match replica {
                    true => fixture.on(mysql::replica()),
                    false => fixture,
                };
                cli::mysql_source(&fixture)
            }
        }
    }

    async fn version(self) -> String {
        match self {
            Db::Postgresql => {
                let client = postgresql::Fixture::open("measure").client().await;
                postgresql::server_version(&client).await
            }
            Db::Mysql => {
                let mut client = mysql::Fixture::open("measure").client().await;
                mysql::server_version(&mut client).await
            }
        }
    }

    /// Runs `statements` as the admin in the measured database.
    async fn execute(self, statements: &str) {
        match self {
            Db::Postgresql => postgresql::Fixture::open("measure")
                .client()
                .await
                .batch_execute(statements)
                .await
                .unwrap(),
            Db::Mysql => mysql::Fixture::open("measure")
                .client()
                .await
                .query_drop(statements)
                .await
                .unwrap(),
        }
    }

    /// The load generator, logging each transaction to `<DIR>/<name>.*`. Without a rate it runs
    /// as fast as it can.
    fn load(self, name: &str, rate: Option<f64>, duration: Duration) -> Command {
        let prefix = format!("{DIR}/{name}");
        let seconds = duration.as_secs().to_string();
        let mut command = match self {
            Db::Postgresql => {
                let admin = postgresql::admin();
                let tokio_postgres::config::Host::Tcp(host) = &admin.get_hosts()[0] else {
                    panic!("SVALBARD_TEST_POSTGRESQL must use TCP");
                };
                let mut command = pinned("pgbench");
                command
                    .args(["--host", host, "--username", admin.get_user().unwrap()])
                    .args(["--port", &admin.get_ports()[0].to_string()])
                    .args(["--client", &CLIENTS.to_string(), "--jobs", "4"])
                    .args(["--time", &seconds, "--no-vacuum", "--log"])
                    .arg(format!("--log-prefix={prefix}"))
                    .env(
                        "PGPASSWORD",
                        String::from_utf8_lossy(admin.get_password().unwrap()).as_ref(),
                    );
                if let Some(rate) = rate {
                    command.arg(format!("--rate={rate:.0}"));
                }
                command.arg("svalbard_measure");
                command
            }
            Db::Mysql => {
                let admin = mysql::admin();
                let mut command = pinned("sysbench");
                command
                    .arg(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/workload/oltp_paced.lua"
                    ))
                    .args(sysbench_options(&admin))
                    .arg(format!("--threads={CLIENTS}"))
                    .arg(format!("--time={seconds}"))
                    .arg(format!(
                        "--thread-rate={}",
                        rate.unwrap_or(0.0) / CLIENTS as f64
                    ))
                    .arg(format!("--log-prefix={prefix}"))
                    .args(["--mysql-ignore-errors=all", "run"]);
                command
            }
        };
        // finish_load reads the report.
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    }

    /// Waits until the standby or replica has applied what the primary did so far.
    async fn caught_up(self) {
        let limit = Duration::from_secs(900);
        match self {
            Db::Postgresql => {
                let fixture = postgresql::Fixture::open("measure");
                let standby = fixture.on(&postgresql::standby());
                postgresql::caught_up(&fixture.client().await, &standby.client().await, limit)
                    .await;
            }
            Db::Mysql => {
                let fixture = mysql::Fixture::open("measure");
                let replica = fixture.on(mysql::replica());
                let (mut source, mut replica) = (fixture.client().await, replica.client().await);
                mysql::caught_up(&mut source, &mut replica, limit).await;
            }
        }
    }

    /// Restores `dump` of the measured database on the primary, checks the schema and the
    /// fixture data against the server of the dump, and drops the copy. A replica differs from
    /// its source: its events are disabled.
    async fn assert_restores(self, dump: &[u8], replica: bool) {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("measure.dump");
        fs::write(&path, dump).unwrap();
        match self {
            Db::Postgresql => {
                let fixture = postgresql::Fixture::open("measure");
                let restored = fixture.restore(&path).await;
                let dumped = match replica {
                    true => fixture.on(&postgresql::standby()).client().await,
                    false => fixture.client().await,
                };
                assert_eq!(
                    postgresql::fingerprint(&restored).await,
                    postgresql::fingerprint(&dumped).await
                );
                self.execute("DROP DATABASE svalbard_measure_restored WITH (FORCE)")
                    .await;
            }
            Db::Mysql => {
                let fixture = mysql::Fixture::open("measure");
                let mut restored = fixture.restore(&path).await;
                let mut dumped = match replica {
                    true => fixture.on(mysql::replica()).client().await,
                    false => fixture.client().await,
                };
                assert_eq!(
                    mysql::fingerprint(&mut restored).await,
                    mysql::fingerprint(&mut dumped).await
                );
                self.execute("DROP DATABASE svalbard_measure_restored")
                    .await;
            }
        }
        fs::remove_file(path).unwrap();
    }

    /// Server values to record during a window, and their names.
    async fn samples(self) -> Vec<(&'static str, f64)> {
        match self {
            Db::Postgresql => {
                let client = postgresql::Fixture::open("measure").client().await;
                let row = client
                    .query_one(
                        "SELECT
                           (SELECT max(age(backend_xmin))::float8 FROM pg_stat_activity
                            WHERE application_name ~ '^svalbard-'),
                           (SELECT max(age(backend_xmin))::float8 FROM pg_stat_replication),
                           (SELECT extract(epoch FROM max(replay_lag))::float8
                            FROM pg_stat_replication)",
                        &[],
                    )
                    .await
                    .unwrap();
                let names = ["backup xmin age", "standby xmin age", "replay lag (s)"];
                (0..3)
                    .filter_map(|i| Some((names[i], row.get::<_, Option<f64>>(i)?)))
                    .collect()
            }
            Db::Mysql => {
                let mut source = mysql::Fixture::open("measure").client().await;
                let history: Option<i64> = source
                    .query_first(
                        "SELECT COUNT FROM information_schema.INNODB_METRICS
                         WHERE NAME = 'trx_rseg_history_len'",
                    )
                    .await
                    .unwrap();
                let mut replica = mysql::Fixture::open("measure")
                    .on(mysql::replica())
                    .client()
                    .await;
                let status: Option<mysql_async::Row> =
                    replica.query_first("SHOW REPLICA STATUS").await.unwrap();
                let lag: Option<i64> = status.and_then(|row| row.get("Seconds_Behind_Source"));
                [
                    ("history list length", history.map(|h| h as f64)),
                    ("replica lag (s)", lag.map(|l| l as f64)),
                ]
                .into_iter()
                .filter_map(|(name, value)| Some((name, value?)))
                .collect()
            }
        }
    }
}

/// The account of the load. sysbench 1.0 takes TLS only with client certificate files, and
/// caching_sha2_password sends a password only over TLS, so the account has no password.
const LOAD_USER: &str = "svalbard_load";
const SYSBENCH_TABLES: u64 = 4;

fn sysbench_options(admin: &mysql_async::Opts) -> Vec<String> {
    vec![
        "--db-driver=mysql".to_owned(),
        format!("--mysql-host={}", admin.ip_or_hostname()),
        format!("--mysql-port={}", admin.tcp_port()),
        format!("--mysql-user={LOAD_USER}"),
        "--mysql-db=svalbard_measure".to_owned(),
        format!("--tables={SYSBENCH_TABLES}"),
        format!("--table-size={}", sysbench_rows()),
    ]
}

fn sysbench_rows() -> u64 {
    scale() * 5000
}

/// One transaction of the load, in seconds since the epoch and milliseconds.
struct Transaction {
    scheduled: f64,
    completed: f64,
    latency: f64,
}

/// What a load generator did.
struct Load {
    transactions: Vec<Transaction>,
    errors: u64,
}

/// Waits for the load generator `name` and reads its logs.
async fn finish_load(db: Db, name: &str, child: Child) -> Load {
    let output = child.wait_with_output().await.unwrap();
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let number = |line: &str, prefix: &str| -> Option<f64> {
        let rest = line.trim().strip_prefix(prefix)?.trim();
        rest.split(|c: char| c.is_whitespace() || c == '(')
            .find(|part| !part.is_empty())?
            .parse()
            .ok()
    };
    let (mut errors, mut reported) = (0, false);
    for line in report.lines() {
        let counted = match db {
            Db::Postgresql => {
                if line.contains(" aborted") {
                    errors += 1;
                }
                number(line, "number of failed transactions:")
            }
            Db::Mysql => number(line, "ignored errors:"),
        };
        if let Some(count) = counted {
            errors += count as u64;
            reported = true;
        }
    }
    assert!(reported, "{name} reported no errors:\n{report}");
    if !output.status.success() && db == Db::Mysql {
        panic!("{name} failed:\n{report}");
    }

    let mut transactions = Vec::new();
    for entry in fs::read_dir(DIR).unwrap() {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_string_lossy();
        if !file.starts_with(&format!("{name}.")) {
            continue;
        }
        for line in fs::read_to_string(&path).unwrap().lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // pgbench: client, transaction, latency µs, script, epoch s, µs, schedule lag.
            // oltp_paced.lua: epoch µs, latency µs.
            let (completed, latency) = match db {
                Db::Postgresql => {
                    let Ok(latency) = fields[2].parse::<f64>() else {
                        continue; // A failed transaction, counted in the report.
                    };
                    let completed: f64 =
                        fields[4].parse::<f64>().unwrap() + fields[5].parse::<f64>().unwrap() / 1e6;
                    (completed, latency)
                }
                Db::Mysql => (
                    fields[0].parse::<f64>().unwrap() / 1e6,
                    fields[1].parse().unwrap(),
                ),
            };
            transactions.push(Transaction {
                scheduled: completed - latency / 1e6,
                completed,
                latency: latency / 1e3,
            });
        }
        fs::remove_file(&path).unwrap();
    }
    assert!(!transactions.is_empty(), "{name} logged nothing:\n{report}");
    Load {
        transactions,
        errors,
    }
}

async fn run_load(db: Db, name: &str, rate: Option<f64>, duration: Duration) -> Load {
    let child = db.load(name, rate, duration).spawn().unwrap();
    finish_load(db, name, child).await
}

/// Latency and throughput of the transactions in `intervals`.
struct Stats {
    p50: f64,
    p99: f64,
    max: f64,
    /// Completed transactions per second.
    throughput: f64,
}

fn stats<'a>(
    transactions: impl Iterator<Item = &'a Transaction> + Clone,
    intervals: &[(f64, f64)],
) -> Stats {
    let inside = |time: f64| {
        intervals
            .iter()
            .any(|&(from, to)| from <= time && time < to)
    };
    let mut latencies: Vec<f64> = transactions
        .clone()
        .filter(|t| inside(t.scheduled))
        .map(|t| t.latency)
        .collect();
    latencies.sort_by(f64::total_cmp);
    let completed = transactions.filter(|t| inside(t.completed)).count();
    let duration: f64 = intervals.iter().map(|(from, to)| to - from).sum();
    let percentile = |p: f64| {
        let rank = ((p * latencies.len() as f64).ceil() as usize).max(1);
        latencies.get(rank - 1).copied().unwrap_or(f64::NAN)
    };
    Stats {
        p50: percentile(0.5),
        p99: percentile(0.99),
        max: latencies.last().copied().unwrap_or(f64::NAN),
        throughput: completed as f64 / duration,
    }
}

/// One `svalbard backup` run.
struct Backup {
    /// When the dump started and ended, from the progress lines, and when Svalbard exited.
    dumping: f64,
    dumped: Option<f64>,
    ended: f64,
    result: serde_json::Value,
    /// The peak resident memory of the Svalbard process, in KiB.
    peak: u64,
}

/// Runs a backup. Shares the run ID in `current` while the dump runs.
async fn backup(setup: &Setup, db: Db, current: &RefCell<Option<String>>) -> Backup {
    let mut child = setup.spawn(db.password());
    let pid = child.id().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let progress = async {
        let (mut dumping, mut dumped, mut text) = (None, None, String::new());
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Some(rest) = line.strip_prefix("svalbard: run ") {
                dumping = Some(epoch());
                *current.borrow_mut() = rest.split(':').next().map(str::to_owned);
            } else if line.starts_with("svalbard: dumped ") {
                dumped = Some(epoch());
                *current.borrow_mut() = None;
            }
            text += &line;
            text.push('\n');
        }
        *current.borrow_mut() = None;
        (dumping, dumped, text)
    };
    // The high-water mark of the resident memory, until the process exits.
    let memory = async {
        let mut peak = 0;
        while let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
            let Some(line) = status.lines().find(|line| line.starts_with("VmHWM:")) else {
                break;
            };
            peak = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            time::sleep(Duration::from_millis(50)).await;
        }
        peak
    };
    let mut output = String::new();
    let ((dumping, dumped, stderr), peak, _) =
        tokio::join!(progress, memory, stdout.read_to_string(&mut output));
    child.wait().await.unwrap();
    for secret in db.secrets() {
        assert!(!output.contains(secret) && !stderr.contains(secret));
    }
    let result: serde_json::Value =
        serde_json::from_str(&output).unwrap_or_else(|_| panic!("{output}\n{stderr}"));
    assert!(
        fs::read_dir(&setup.staging).unwrap().next().is_none(),
        "{stderr}"
    );
    Backup {
        dumping: dumping.unwrap_or_else(|| panic!("no dump started:\n{stderr}")),
        dumped,
        ended: epoch(),
        result,
        peak,
    }
}

/// A backup configuration to SFTP, and the directory that the test empties after each run.
fn sftp(db: Db, name: &str, fixture: &str, replica: bool) -> (Setup, String) {
    let dir = ssh::directory("/srv/backups", name);
    let host = ssh::var("SVALBARD_TEST_SSH_HOST");
    let destination = cli::ssh_destination("sftp", &host, 22, &dir, "client");
    let setup = Setup::new(name, &db.source(fixture, replica), &destination, "");
    (setup, dir)
}

/// The load for one window, and the backups that ran back to back during it.
struct Window {
    name: String,
    start: f64,
    end: f64,
    load: Load,
    backups: Vec<Backup>,
    /// The remote path of the dump of the last complete backup.
    dump: Option<String>,
    /// The highest value of each sample.
    samples: Vec<(&'static str, f64)>,
}

impl Window {
    /// The dump intervals inside the window, or the whole window without backups.
    fn intervals(&self) -> Vec<(f64, f64)> {
        if self.backups.is_empty() {
            return vec![(self.start, self.end)];
        }
        self.backups
            .iter()
            .map(|b| {
                (
                    b.dumping.max(self.start),
                    b.dumped.unwrap_or(b.ended).min(self.end),
                )
            })
            .filter(|(from, to)| from < to)
            .collect()
    }

    fn dump_share(&self) -> f64 {
        let dumping: f64 = self.intervals().iter().map(|(from, to)| to - from).sum();
        dumping / (self.end - self.start)
    }
}

async fn window(db: Db, name: &str, rate: f64, backups: Option<bool>) -> Window {
    let length = window_length();
    let slug = name.replace(' ', "-");
    let start = epoch();
    let child = db.load(&slug, Some(rate), length).spawn().unwrap();
    let ends = Instant::now() + length;
    let current = RefCell::new(None);
    let run_backups = async {
        let Some(replica) = backups else {
            return (Vec::new(), None);
        };
        let (setup, dir) = sftp(db, &format!("{}-{slug}", db.slug()), "measure", replica);
        let mut done: Vec<Backup> = Vec::new();
        // The files of the last complete backup, for the restore.
        let mut kept: Option<Vec<String>> = None;
        // A backup that would start in the last 10 s would mostly run after the load.
        while Instant::now() + Duration::from_secs(10) < ends {
            let backup = backup(&setup, db, &current).await;
            if backup.result["result"] == "complete" {
                let files = ["data", "record"]
                    .map(|key| format!("{dir}/{}", backup.result[key].as_str().unwrap()));
                if let Some(old) = kept.replace(files.to_vec()) {
                    let old: Vec<String> = old.iter().map(|path| ssh::quote(path)).collect();
                    ssh::shell(&format!("rm -f -- {}", old.join(" ")));
                }
            }
            done.push(backup);
        }
        (done, kept.map(|files| files[0].clone()))
    };
    let sample = async {
        let mut highest: Vec<(&'static str, f64)> = Vec::new();
        while Instant::now() < ends {
            for (key, value) in db.samples().await {
                match highest.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, high)) => *high = high.max(value),
                    None => highest.push((key, value)),
                }
            }
            time::sleep(Duration::from_secs(1)).await;
        }
        highest
    };
    let (load, (backups, dump), samples) =
        tokio::join!(finish_load(db, &slug, child), run_backups, sample);
    Window {
        name: name.to_owned(),
        start,
        end: start + length.as_secs_f64(),
        load,
        backups,
        dump,
        samples,
    }
}

/// CPU cores and MB read and written by each server during the window, from the cgroup samples
/// of scripts/measure.sh.
fn resources(window: &Window) -> String {
    let Ok(log) = fs::read_to_string(format!("{DIR}/cgroup.log")) else {
        return "not sampled".to_owned();
    };
    let mut services: Vec<&str> = Vec::new();
    let mut parts = Vec::new();
    let samples: Vec<Vec<&str>> = log.lines().map(|l| l.split(' ').collect()).collect();
    for sample in &samples {
        if sample.len() == 5 && !services.contains(&sample[1]) {
            services.push(sample[1]);
        }
    }
    for service in services {
        let inside: Vec<&Vec<&str>> = samples
            .iter()
            .filter(|s| s.len() == 5 && s[1] == service)
            .filter(|s| (window.start..=window.end).contains(&s[0].parse::<f64>().unwrap()))
            .collect();
        let (Some(first), Some(last)) = (inside.first(), inside.last()) else {
            continue;
        };
        let value = |s: &[&str], i: usize| s[i].parse::<f64>().unwrap_or(0.0);
        let seconds = value(last, 0) - value(first, 0);
        if seconds <= 0.0 {
            continue;
        }
        parts.push(format!(
            "{service} {:.2} cores, {:.0} MB read, {:.0} MB written",
            (value(last, 2) - value(first, 2)) / 1e6 / seconds,
            (value(last, 3) - value(first, 3)) / 1e6,
            (value(last, 4) - value(first, 4)) / 1e6
        ));
    }
    parts.join("; ")
}

fn ms(value: f64) -> String {
    if value >= 1000.0 {
        format!("{:.2} s", value / 1000.0)
    } else {
        format!("{value:.1} ms")
    }
}

fn met(ok: bool) -> &'static str {
    if ok { "met" } else { "**missed**" }
}

fn mib(kib: u64) -> String {
    format!("{:.1} MiB", kib as f64 / 1024.0)
}

/// Creates the measured database: the shared fixture, the load generator's tables, and the
/// tables that S4 and B8 lock.
async fn prepare(db: Db) {
    fs::create_dir_all(DIR).unwrap();
    let output = match db {
        Db::Postgresql => {
            postgresql::Fixture::create("measure").await;
            postgresql::Fixture::create("measure_small").await;
            let admin = postgresql::admin();
            let tokio_postgres::config::Host::Tcp(host) = &admin.get_hosts()[0] else {
                panic!("SVALBARD_TEST_POSTGRESQL must use TCP");
            };
            Command::new("pgbench")
                .args(["--initialize", "--quiet", "--scale", &scale().to_string()])
                .args(["--host", host, "--username", admin.get_user().unwrap()])
                .env(
                    "PGPASSWORD",
                    String::from_utf8_lossy(admin.get_password().unwrap()).as_ref(),
                )
                .arg("svalbard_measure")
                .output()
                .await
                .unwrap()
        }
        Db::Mysql => {
            mysql::Fixture::create("measure").await;
            mysql::Fixture::create("measure_small").await;
            db.execute(&format!(
                "CREATE USER IF NOT EXISTS {LOAD_USER} IDENTIFIED BY '';
                 GRANT ALL ON svalbard_measure.* TO {LOAD_USER}"
            ))
            .await;
            Command::new("sysbench")
                .arg("oltp_read_write")
                .args(sysbench_options(&mysql::admin()))
                .args(["--threads=4", "prepare"])
                .output()
                .await
                .unwrap()
        }
    };
    let report = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{report}");
    if db == Db::Mysql {
        // A parallel prepare exits 0 when a thread fails.
        let rows: Option<u64> = mysql::Fixture::open("measure")
            .client()
            .await
            .query_first(format!("SELECT COUNT(*) FROM sbtest{SYSBENCH_TABLES}"))
            .await
            .unwrap_or_else(|error| panic!("sysbench prepare failed: {error}\n{report}"));
        assert_eq!(rows, Some(sysbench_rows()), "{report}");
    }
    db.execute("CREATE TABLE measure_stall (id INT); CREATE TABLE measure_waiters (id INT)")
        .await;
    db.caught_up().await;
}

/// S2 under load: an ALTER TABLE waits for a table that the dump holds. Returns the result of
/// the backup and the longest latency of the transactions that started while the ALTER waited.
async fn schema_change(db: Db, rate: f64) -> (String, f64, f64) {
    let name = format!("{}-schema-change", db.slug());
    let child = db.load(&name, Some(rate), MINUTE).spawn().unwrap();
    time::sleep(Duration::from_secs(10)).await;
    let (setup, _) = sftp(db, &name, "measure", false);
    let current = RefCell::new(None);
    let change = async {
        let run = until_some(async || current.borrow().clone()).await;
        // A table that the dump holds, and whose ALTER the load must wait for.
        let table = match db {
            Db::Postgresql => {
                let client = postgresql::Fixture::open("measure").client().await;
                let application = format!("svalbard-{run}");
                until_some(async || {
                    let row = client
                        .query_one(
                            "SELECT EXISTS (
                               SELECT FROM pg_locks l JOIN pg_stat_activity a USING (pid)
                               WHERE a.application_name = $1 AND l.granted
                                 AND l.database = (SELECT oid FROM pg_database
                                                   WHERE datname = current_database())
                                 AND l.relation = 'pgbench_tellers'::regclass
                             )",
                            &[&application],
                        )
                        .await
                        .unwrap();
                    row.get::<_, bool>(0).then_some(())
                })
                .await;
                "pgbench_tellers".to_owned()
            }
            Db::Mysql => {
                let mut client = mysql::Fixture::open("measure").client().await;
                until_some(async || {
                    client
                        .query_first::<String, _>(
                            "SELECT l.OBJECT_NAME FROM performance_schema.metadata_locks l
                             JOIN performance_schema.user_variables_by_thread v
                               ON v.THREAD_ID = l.OWNER_THREAD_ID
                                 AND v.VARIABLE_NAME = 'svalbard_run'
                             WHERE l.OBJECT_SCHEMA = 'svalbard_measure'
                               AND l.OBJECT_NAME LIKE 'sbtest%'
                               AND l.LOCK_STATUS = 'GRANTED' AND l.LOCK_TYPE = 'SHARED_READ'",
                        )
                        .await
                        .unwrap()
                })
                .await
            }
        };
        let started = epoch();
        db.execute(&format!("ALTER TABLE {table} ADD COLUMN measure_s2 INT"))
            .await;
        let ended = epoch();
        db.execute(&format!("ALTER TABLE {table} DROP COLUMN measure_s2"))
            .await;
        (started, ended)
    };
    let (backup, (started, ended)) = tokio::join!(backup(&setup, db, &current), change);
    let load = finish_load(db, &name, child).await;
    let waited = stats(load.transactions.iter(), &[(started, ended)]);
    (
        backup.result["error"]
            .as_str()
            .unwrap_or("complete")
            .to_owned(),
        waited.max,
        ended - started,
    )
}

/// S4 under load: the backup waits for a lock that another session holds. Returns the result,
/// how long it waited, the p99 during the wait and the errors of the load.
async fn lock_wait(db: Db, rate: f64) -> (String, f64, f64, u64) {
    let name = format!("{}-lock-wait", db.slug());
    let child = db.load(&name, Some(rate), MINUTE).spawn().unwrap();
    time::sleep(Duration::from_secs(10)).await;
    let holder = match db {
        Db::Postgresql => {
            let client = postgresql::Fixture::open("measure").client().await;
            client
                .batch_execute("BEGIN; LOCK TABLE measure_stall IN ACCESS EXCLUSIVE MODE")
                .await
                .unwrap();
            Ok(client)
        }
        Db::Mysql => {
            let mut client = mysql::Fixture::open("measure").client().await;
            client
                .query_drop("LOCK TABLES measure_stall WRITE")
                .await
                .unwrap();
            Err(client)
        }
    };
    let (setup, _) = sftp(db, &name, "measure", false);
    let started = epoch();
    let backup = backup(&setup, db, &RefCell::new(None)).await;
    let ended = epoch();
    match holder {
        Ok(client) => client.batch_execute("COMMIT").await.unwrap(),
        Err(mut client) => client.query_drop("UNLOCK TABLES").await.unwrap(),
    }
    let load = finish_load(db, &name, child).await;
    let during = stats(load.transactions.iter(), &[(started, ended)]);
    (
        backup.result["error"]
            .as_str()
            .unwrap_or("complete")
            .to_owned(),
        ended - started,
        during.p99,
        load.errors,
    )
}

/// Polls `value` every 100 ms until it returns a value, for at most a minute.
async fn until_some<T>(mut value: impl AsyncFnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = value().await {
            return value;
        }
        assert!(started.elapsed() < MINUTE, "the condition did not hold");
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// Measures the windows, the budgets B1–B4 and B7, and S2 and S4 under load.
async fn measure(db: Db) {
    fs::create_dir_all(DIR).unwrap();
    let version = db.version().await;
    run_load(db, "warm-up", None, MINUTE).await;
    // What the servers sustain: the last 2 of 3 minutes at full speed. The first minute runs
    // faster, before the servers write back what the load changed.
    let calibration = run_load(db, "calibration", None, 3 * MINUTE).await;
    let end = calibration
        .transactions
        .iter()
        .map(|t| t.completed)
        .fold(0.0, f64::max);
    let maximum = stats(calibration.transactions.iter(), &[(end - 120.0, end)]).throughput;
    let rate = maximum / 2.0;
    fs::write(format!("{DIR}/{}-rate", db.slug()), rate.to_string()).unwrap();
    // The windows start without the backlog of the load at full speed.
    db.caught_up().await;

    let windows = [
        window(db, "baseline", rate, None).await,
        window(db, "backups on the primary", rate, Some(false)).await,
        window(db, "baseline again", rate, None).await,
        window(
            db,
            &format!("backups on the {}", db.replica()),
            rate,
            Some(true),
        )
        .await,
    ];
    // S1: the last complete dump of each window with backups restores, without the load.
    let mut restored = Vec::new();
    for (window, replica) in [(&windows[1], false), (&windows[3], true)] {
        let Some(dump) = &window.dump else {
            continue;
        };
        db.assert_restores(&ssh::download(dump), replica).await;
        ssh::shell(&format!("rm -f -- {}", ssh::quote(dump)));
        restored.push(window.name.as_str());
    }
    let current = RefCell::new(None);
    let bucket = floci::bucket(&format!("{}-multipart", db.slug()), None).await;
    let s3 = Setup::new(
        &format!("{}-multipart", db.slug()),
        &db.source("measure", false),
        &cli::floci(&bucket),
        "",
    );
    let multipart = backup(&s3, db, &current).await;
    let (small_setup, _) = sftp(db, &format!("{}-small", db.slug()), "measure_small", false);
    let small = backup(&small_setup, db, &current).await;
    let (stall_result, stall, alter) = schema_change(db, rate).await;
    let (wait_result, waited, wait_p99, wait_errors) = lock_wait(db, rate).await;

    let tool = match db {
        Db::Postgresql => "pgbench",
        Db::Mysql => "sysbench oltp_read_write (paced)",
    };
    let client = windows[1].backups.first().map_or(String::new(), |b| {
        format!(" ({})", b.result["client_version"].as_str().unwrap_or(""))
    });
    println!("### {} {version}{client}\n", db.slug());
    println!(
        "{tool}, scale {}, {CLIENTS} clients at {rate:.0} transactions/s (50% of {maximum:.0}). \
         CPUs: {}. Windows of {} s.\n",
        scale(),
        env::var("SVALBARD_MEASURE_LAYOUT").unwrap_or_else(|_| "not pinned".to_owned()),
        window_length().as_secs()
    );
    println!(
        "| Window | Backups | Dump time | p50 | p99 | Throughput | Errors | Records | Servers |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for window in &windows {
        let stats = stats(window.load.transactions.iter(), &window.intervals());
        let records: Vec<String> = window
            .samples
            .iter()
            .map(|(name, value)| format!("{name} {value:.0}"))
            .collect();
        let failed = window
            .backups
            .iter()
            .filter(|b| b.result["result"] != "complete")
            .count();
        println!(
            "| {} | {} | {:.0}% | {} | {} | {:.1}% | {} | {} | {} |",
            window.name,
            match failed {
                0 => window.backups.len().to_string(),
                _ => format!("{} ({failed} failed)", window.backups.len()),
            },
            if window.backups.is_empty() {
                0.0
            } else {
                window.dump_share() * 100.0
            },
            ms(stats.p50),
            ms(stats.p99),
            stats.throughput / rate * 100.0,
            window.load.errors,
            records.join(", "),
            resources(window)
        );
    }

    // Both baselines, so a checkpoint in one of them counts like one during the backups.
    let baseline = stats(
        windows[0]
            .load
            .transactions
            .iter()
            .chain(&windows[2].load.transactions),
        &[windows[0].intervals(), windows[2].intervals()].concat(),
    );
    let during = |window: &Window| stats(window.load.transactions.iter(), &window.intervals());
    let (primary, replica) = (during(&windows[1]), during(&windows[3]));
    let errors = windows[1].load.errors + windows[3].load.errors;
    let caused = errors.saturating_sub(windows[0].load.errors + windows[2].load.errors);
    let largest = |window: &Window| {
        window
            .backups
            .iter()
            .map(|b| (b.peak, b.result["size"].as_u64().unwrap_or(0)))
            .max()
            .unwrap_or_default()
    };
    let (sftp_peak, sftp_size) = largest(&windows[1]).max(largest(&windows[3]));
    let peaks = [small.peak, sftp_peak, multipart.peak];
    let worst = primary.throughput.min(replica.throughput) / rate;
    println!("\n| Budget | Measured | Limit | Result |");
    println!("| --- | --- | --- | --- |");
    println!(
        "| B1 errors | {errors} with backups, {} without | 0 caused by the backup | {} |",
        windows[0].load.errors + windows[2].load.errors,
        met(caused == 0)
    );
    println!(
        "| B2 p99 during dumps | {} on the primary, {} on the {} | 2 × {} (both baselines) = {} | {} |",
        ms(primary.p99),
        ms(replica.p99),
        db.replica(),
        ms(baseline.p99),
        ms(2.0 * baseline.p99),
        met(primary.p99.max(replica.p99) <= 2.0 * baseline.p99)
    );
    println!(
        "| B3 throughput during dumps | {:.1}% on the primary, {:.1}% on the {} | 95% of {rate:.0}/s | {} |",
        primary.throughput / rate * 100.0,
        replica.throughput / rate * 100.0,
        db.replica(),
        met(worst >= 0.95)
    );
    println!(
        "| B4 stall after a schema change | the longest transaction took {} while the ALTER TABLE waited {:.1} s; the backup ended with `{}` | 2 s + 3 s | {} |",
        ms(stall),
        alter,
        stall_result,
        met(stall <= 5000.0)
    );
    println!(
        "| B7 memory | {} ({} B dump), {} (SFTP, {sftp_size} B), {} (S3 multipart, {} B) | 64 MiB | {} |",
        mib(small.peak),
        small.result["size"],
        mib(sftp_peak),
        mib(multipart.peak),
        multipart.result["size"],
        met(peaks.iter().all(|&peak| peak <= 64 * 1024))
    );
    for window in &windows {
        for backup in window
            .backups
            .iter()
            .filter(|b| b.result["result"] != "complete")
        {
            println!(
                "\nA backup in \"{}\" failed with `{}`: {}",
                window.name,
                backup.result["error"].as_str().unwrap_or(""),
                backup.result["message"].as_str().unwrap_or("")
            );
        }
    }
    println!(
        "\nS1: the last complete dump of these windows restored, and the fixture data matched: {}.",
        restored.join(", ")
    );
    println!(
        "\nS4 under load: the backup failed with `{wait_result}` after {waited:.1} s; p99 during \
         the wait {} against the baseline {}, {wait_errors} errors.\n",
        ms(wait_p99),
        ms(baseline.p99)
    );
}

/// B8: the detection query with 500 client sessions during a dump. Some sessions hold locks in
/// open transactions, and 10 wait for a lock that another session holds.
async fn detection(db: Db) {
    let rate: f64 = fs::read_to_string(format!("{DIR}/{}-rate", db.slug()))
        .unwrap()
        .parse()
        .unwrap();
    // The load, the holder, the waiters, the control connection of each Svalbard and pg_dump.
    let idle = SESSIONS - CLIENTS - 1 - 10 - 3;
    let mut holder = None;
    let mut postgresql_sessions = Vec::new();
    let mut mysql_sessions = Vec::new();
    let mut waiters = Vec::new();
    match db {
        Db::Postgresql => {
            let fixture = postgresql::Fixture::open("measure");
            for i in 0..idle {
                let client = fixture.client().await;
                if i % 2 == 0 {
                    client
                        .batch_execute("BEGIN; SELECT count(*) FROM measure_stall")
                        .await
                        .unwrap();
                }
                postgresql_sessions.push(client);
            }
            let client = fixture.client().await;
            client
                .batch_execute("BEGIN; LOCK TABLE measure_waiters IN ACCESS EXCLUSIVE MODE")
                .await
                .unwrap();
            holder = Some(client);
            for _ in 0..10 {
                let client = fixture.client().await;
                waiters.push(tokio::task::spawn_local(async move {
                    let _ = client.batch_execute("SELECT * FROM measure_waiters").await;
                }));
            }
        }
        Db::Mysql => {
            let fixture = mysql::Fixture::open("measure");
            for i in 0..idle {
                let mut client = fixture.client().await;
                if i % 2 == 0 {
                    client
                        .query_drop("START TRANSACTION; SELECT COUNT(*) FROM measure_stall")
                        .await
                        .unwrap();
                }
                mysql_sessions.push(client);
            }
            let mut client = fixture.client().await;
            client
                .query_drop("LOCK TABLES measure_waiters WRITE")
                .await
                .unwrap();
            mysql_sessions.push(client);
            for _ in 0..10 {
                let mut client = fixture.client().await;
                waiters.push(tokio::task::spawn_local(async move {
                    let _ = client.query_drop("SELECT * FROM measure_waiters").await;
                }));
            }
        }
    }

    let name = format!("{}-detection", db.slug());
    let child = db.load(&name, Some(rate), MINUTE).spawn().unwrap();
    let ends = Instant::now() + MINUTE;
    let (setup, dir) = sftp(db, &name, "measure", false);
    let current = RefCell::new(None);
    let backups = async {
        while Instant::now() + Duration::from_secs(10) < ends {
            backup(&setup, db, &current).await;
            ssh::shell(&format!("rm -f {}/*", ssh::quote(&dir)));
        }
    };
    let timings = async {
        // Svalbard's own detection, with a control connection of the backup account.
        let mut detector = match db {
            Db::Postgresql => Ok(Postgres::new(
                postgresql::Fixture::open("measure").source(postgresql::verify_full()),
            )
            .unwrap()),
            Db::Mysql => Err(Mysql::new(
                mysql::Fixture::open("measure").source(mysql::verify_full()),
            )
            .unwrap()),
        };
        let mut durations = Vec::new();
        let mut connected = false;
        while Instant::now() < ends {
            let run = current.borrow().clone();
            if let Some(run) = run {
                let started = Instant::now();
                let observation = match &mut detector {
                    Ok(postgres) => postgres.observe(&run).await,
                    Err(mysql) => mysql.observe(&run).await,
                };
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                let observation = observation.unwrap();
                // The first query opens the control connection, and a finished dump has no
                // session left to look at.
                if connected && !observation.sessions.is_empty() {
                    durations.push(elapsed);
                }
                connected = true;
            }
            time::sleep(Duration::from_millis(100)).await;
        }
        durations
    };
    let (_, mut durations) = tokio::join!(backups, timings);
    finish_load(db, &name, child).await;
    drop(holder);
    for waiter in waiters {
        waiter.abort();
    }
    let sessions = match db {
        Db::Postgresql => postgresql_sessions.len(),
        Db::Mysql => mysql_sessions.len(),
    };
    durations.sort_by(f64::total_cmp);
    let p99 = durations[((durations.len() as f64 * 0.99).ceil() as usize).max(1) - 1];
    println!("| Budget | Measured | Limit | Result |");
    println!("| --- | --- | --- | --- |");
    println!(
        "| B8 detection query | p99 {}, max {} over {} queries; {} sessions: {CLIENTS} load clients, \
         {sessions} more (half in open transactions), 10 lock waiters | 50 ms | {} |\n",
        ms(p99),
        ms(*durations.last().unwrap()),
        durations.len(),
        SESSIONS,
        met(p99 <= 50.0)
    );
}

#[tokio::test]
#[ignore = "run scripts/measure.sh"]
async fn postgresql_prepare() {
    prepare(Db::Postgresql).await;
}

#[tokio::test]
#[ignore = "run scripts/measure.sh"]
async fn postgresql_measure() {
    measure(Db::Postgresql).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "run scripts/measure.sh"]
async fn postgresql_detection() {
    tokio::task::LocalSet::new()
        .run_until(detection(Db::Postgresql))
        .await;
}

#[tokio::test]
#[ignore = "run scripts/measure.sh"]
async fn mysql_prepare() {
    prepare(Db::Mysql).await;
}

#[tokio::test]
#[ignore = "run scripts/measure.sh"]
async fn mysql_measure() {
    measure(Db::Mysql).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "run scripts/measure.sh"]
async fn mysql_detection() {
    tokio::task::LocalSet::new()
        .run_until(detection(Db::Mysql))
        .await;
}
