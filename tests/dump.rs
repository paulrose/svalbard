use std::{
    cell::Cell,
    fs,
    future::{self, Future},
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

use svalbard::{
    config::Safety,
    db::{self, Client, Error, Failure, Observation, Source, Wait},
    staging::Run,
};

type Detection = Box<dyn FnMut() -> Result<Observation, String>>;

/// A source whose client is a bash script. The script gets the credential file as `$1`.
struct Fake {
    script: &'static str,
    /// The result of each detection query until `terminate` runs. After it, no session remains.
    detection: Detection,
    /// The backup sessions survive `terminate`.
    stuck: bool,
    /// The failure that `verify` reports.
    verify: Option<Failure>,
    started: Cell<bool>,
    terminated: bool,
}

impl Fake {
    fn new(script: &'static str, detection: Detection) -> Fake {
        Fake {
            script,
            detection,
            stuck: false,
            verify: None,
            started: Cell::new(false),
            terminated: false,
        }
    }
}

impl Source for Fake {
    const CLIENT: &'static str = "bash";
    const FORMAT: &'static str = "test";

    async fn preflight(&mut self, client_version: &str) -> Result<String, String> {
        assert!(client_version.starts_with("GNU bash"), "{client_version}");
        Ok("test 1.0".to_owned())
    }

    fn client(&self, _run_id: &str, credentials: &Path, _safety: &Safety) -> Client {
        self.started.set(true);
        Client {
            args: vec![
                "-c".into(),
                self.script.into(),
                "fake".into(),
                credentials.into(),
            ],
            env: vec![("SVALBARD_FAKE", "set".into())],
            credentials: Some("s3cr3t".to_owned()),
        }
    }

    async fn observe(&mut self, _run_id: &str) -> Result<Observation, String> {
        if self.terminated && !self.stuck {
            return Ok(Observation::default());
        }
        (self.detection)()
    }

    async fn terminate(&mut self, _run_id: &str) -> Result<(), String> {
        self.terminated = true;
        Ok(())
    }

    async fn verify(&mut self) -> Result<(), Error> {
        match self.verify {
            Some(kind) => Err(Error {
                kind,
                message: "table app.users changed".to_owned(),
            }),
            None => Ok(()),
        }
    }
}

fn session() -> Result<Observation, String> {
    Ok(Observation {
        sessions: vec!["4242".to_owned()],
        waits: vec![],
    })
}

fn waiting(wait: Wait) -> Detection {
    Box::new(move || {
        Ok(Observation {
            sessions: vec!["4242".to_owned()],
            waits: vec![wait.clone()],
        })
    })
}

fn safety() -> Safety {
    Safety {
        lock_timeout: Duration::from_secs(10),
        contention_timeout: Duration::from_secs(2),
        dump_timeout: Duration::from_secs(60),
        max_dump_size: 1 << 30,
        min_free_space: 0,
    }
}

fn new_run() -> Run {
    Run::create(Path::new(env!("CARGO_TARGET_TMPDIR"))).expect("run directory must be created")
}

/// Runs a dump that must fail, and checks that it leaves no files.
async fn failure(
    fake: &mut Fake,
    safety: &Safety,
    cancel: impl Future<Output = ()>,
) -> (Error, Duration) {
    let run = new_run();
    let started = Instant::now();
    let error = db::dump(fake, safety, &run, cancel)
        .await
        .expect_err("the dump must fail");
    let elapsed = started.elapsed();

    assert_eq!(fs::read_dir(&run.dir).unwrap().count(), 0, "{error}");
    run.remove().unwrap();
    (error, elapsed)
}

#[tokio::test]
async fn successful_dump_returns_a_private_staged_file() {
    // Prints a variable that Svalbard sets, an inherited variable, and the credential file.
    let mut fake = Fake::new(
        r#"printf '%s|%s|' "$SVALBARD_FAKE" "${CARGO_MANIFEST_DIR-}"; cat "$1""#,
        Box::new(|| Ok(Observation::default())),
    );
    let run = new_run();
    let dump = db::dump(&mut fake, &safety(), &run, future::pending())
        .await
        .expect("the dump must succeed");

    assert_eq!(fs::read_to_string(&dump.path).unwrap(), "set||s3cr3t");
    assert_eq!(dump.size, 11);
    assert_eq!(
        dump.sha256,
        "b287a8ff3f309882607669c709380d0ee77fac582295bb76900c412cde46ef19"
    );
    assert_eq!(dump.format, "test");
    assert!(dump.client_version.starts_with("GNU bash"));
    assert_eq!(dump.server_version, "test 1.0");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&run.dir), 0o700);
    assert_eq!(mode(&dump.path), 0o600);
    // The credential file is gone.
    assert_eq!(fs::read_dir(&run.dir).unwrap().count(), 1);
    run.remove().unwrap();
}

#[tokio::test]
async fn failed_client_reports_its_error() {
    let mut fake = Fake::new(
        "printf partial; echo 'error: permission denied' >&2; exit 3",
        Box::new(session),
    );
    let (error, _) = failure(&mut fake, &safety(), future::pending()).await;

    assert_eq!(error.kind, Failure::Client);
    assert!(error.message.contains("exit status: 3"), "{error}");
    assert!(error.message.contains("permission denied"), "{error}");
}

#[tokio::test]
async fn failed_checks_prevent_the_dump() {
    let full_disk = Safety {
        min_free_space: u64::MAX,
        ..safety()
    };
    for (detection, safety, kind) in [
        (
            Box::new(session) as Detection,
            full_disk,
            Failure::StagingLimit,
        ),
        (
            Box::new(|| Err("permission denied".to_owned())),
            safety(),
            Failure::DetectionUnavailable,
        ),
    ] {
        let mut fake = Fake::new("printf dump", detection);
        let (error, _) = failure(&mut fake, &safety, future::pending()).await;

        assert_eq!(error.kind, kind, "{error}");
        assert!(!fake.started.get(), "{error}");
    }
}

#[tokio::test]
async fn oversized_dump_stops_the_client() {
    let mut fake = Fake::new("yes", Box::new(session));
    let small = Safety {
        max_dump_size: 1 << 20,
        ..safety()
    };
    let (error, _) = failure(&mut fake, &small, future::pending()).await;

    assert_eq!(error.kind, Failure::StagingLimit, "{error}");
    assert!(fake.terminated);
}

#[tokio::test]
async fn watchdog_stops_the_backup() {
    let short = || Safety {
        lock_timeout: Duration::from_secs(1),
        contention_timeout: Duration::from_secs(1),
        ..safety()
    };
    let mut calls = 0;
    let lost = move || {
        calls += 1;
        if calls == 1 {
            session()
        } else {
            Err("connection reset".to_owned())
        }
    };

    for (detection, safety, kind, message) in [
        (
            waiting(Wait::Blocked("session 7".to_owned())),
            short(),
            Failure::Contention,
            "session 7 waited",
        ),
        (
            waiting(Wait::Backup("table app.users".to_owned())),
            short(),
            Failure::LockTimeout,
            "for table app.users",
        ),
        (
            Box::new(lost),
            short(),
            Failure::DetectionLost,
            "connection reset",
        ),
        (
            Box::new(|| Ok(Observation::default())),
            short(),
            Failure::DetectionLost,
            "did not appear",
        ),
        (
            Box::new(session),
            Safety {
                dump_timeout: Duration::from_secs(1),
                ..safety()
            },
            Failure::DumpTimeout,
            "longer than 1s",
        ),
    ] {
        let mut fake = Fake::new("sleep 30", detection);
        let (error, elapsed) = failure(&mut fake, &safety, future::pending()).await;

        assert_eq!(error.kind, kind, "{error}");
        assert!(error.message.contains(message), "{error}");
        assert!(fake.terminated, "{error}");
        assert!(elapsed < Duration::from_secs(8), "{error}: {elapsed:?}");
    }
}

#[tokio::test]
async fn failed_source_check_deletes_the_dump() {
    let mut fake = Fake::new("printf dump", Box::new(|| Ok(Observation::default())));
    fake.verify = Some(Failure::SchemaChanged);
    let (error, _) = failure(&mut fake, &safety(), future::pending()).await;

    assert_eq!(error.kind, Failure::SchemaChanged, "{error}");
    assert!(error.message.contains("app.users"), "{error}");
}

#[tokio::test]
async fn cancellation_stops_the_backup() {
    let mut fake = Fake::new("sleep 30", Box::new(session));
    let cancel = tokio::time::sleep(Duration::from_millis(500));
    let (error, elapsed) = failure(&mut fake, &safety(), cancel).await;

    assert_eq!(error.kind, Failure::Cancelled);
    assert!(fake.terminated);
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[tokio::test]
async fn remaining_backup_session_fails_the_dump() {
    let mut fake = Fake::new("printf dump", Box::new(session));
    fake.stuck = true;
    let (error, _) = failure(&mut fake, &safety(), future::pending()).await;

    assert_eq!(error.kind, Failure::CleanupFailed);
    assert!(error.message.contains("4242"), "{error}");
}
