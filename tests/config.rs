use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};

const BASE: &str = r#"
[source]
type = "postgresql"
host = "db.example.com"
database = "app"
user = "backup"

[destination]
type = "s3"
bucket = "example-backups"
"#;

const SECRET: &str = "s3cr3t-sentinel";

fn config_file(name: &str, contents: &str) -> String {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.toml"));
    fs::write(&path, contents).expect("config file must be written");
    path.into_os_string()
        .into_string()
        .expect("path must be UTF-8")
}

/// Runs `svalbard config check` with only the given environment variables.
fn check(args: &[&str], env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_svalbard"))
        .args(["config", "check"])
        .args(args)
        .env_clear()
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .expect("CLI must start");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("stdout must be UTF-8"),
        String::from_utf8(output.stderr).expect("stderr must be UTF-8"),
    )
}

#[test]
fn documented_examples_are_valid() {
    for example in ["svalbard.toml", "r2.toml", "sftp.toml", "scp.toml"] {
        let path = format!("{}/examples/{example}", env!("CARGO_MANIFEST_DIR"));
        let (code, stdout, stderr) = check(&["--config", &path], &[]);

        assert_eq!(code, Some(0), "{example}: {stderr}");
        assert_eq!(stdout, "Configuration is valid.\n");
        assert!(stderr.is_empty());
    }
}

#[test]
fn flags_override_environment_which_overrides_file_and_defaults() {
    let defaults = config_file("precedence-defaults", BASE);
    let file = config_file(
        "precedence-file",
        &format!("{BASE}\n[safety]\nlock_timeout = \"10min\"\n"),
    );
    let env = [("SVALBARD_SAFETY_LOCK_TIMEOUT", "30s")];

    assert_eq!(check(&["--config", &defaults], &[]).0, Some(0));
    assert_eq!(check(&["--config", &file], &[]).0, Some(3));
    assert_eq!(check(&["--config", &file], &env).0, Some(0));
    assert_eq!(
        check(&["--config", &file, "--safety-lock-timeout", "10min"], &env).0,
        Some(3)
    );
}

#[test]
fn excluded_tables_follow_the_precedence_and_appear_in_the_output() {
    let file = config_file(
        "exclude-tables",
        &BASE.replace(
            "user = \"backup\"\n",
            "user = \"backup\"\nexclude_tables = [\"public.sessions\", \"jobs.done,old\"]\n",
        ),
    );
    let env = [("SVALBARD_SOURCE_EXCLUDE_TABLES", "public.a,public.b")];
    let flags = [
        "--source-exclude-tables",
        "public.c",
        "--source-exclude-tables",
        "public.d",
    ];

    let (code, stdout, _) = check(&["--config", &file], &[]);
    assert_eq!(code, Some(0));
    assert_eq!(
        stdout,
        "Configuration is valid.\nBackups leave out these tables: public.sessions, jobs.done,old.\n"
    );
    for (args, env, tables) in [
        (&[][..], &env[..], ["public.a", "public.b"]),
        (&flags[..], &env[..], ["public.c", "public.d"]),
    ] {
        let (code, stdout, stderr) = check(&[&["--config", &file, "--json"], args].concat(), env);
        let result: serde_json::Value = serde_json::from_str(&stdout).expect("stdout must be JSON");
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(result["excluded_tables"], serde_json::json!(tables));
    }
}

#[test]
fn invalid_excluded_table_names_are_rejected() {
    let postgresql = config_file("exclude-postgresql", BASE);
    let mysql = config_file("exclude-mysql", &BASE.replace("postgresql", "mysql"));

    for (file, names, error) in [
        (&postgresql, "public.a,,public.b", Some("has an empty name")),
        (
            &postgresql,
            "sessions",
            Some("has `sessions`. For PostgreSQL, use schema.table"),
        ),
        (
            &postgresql,
            ".sessions",
            Some("has `.sessions`. For PostgreSQL"),
        ),
        (
            &postgresql,
            "public.a,public.a",
            Some("has `public.a` twice"),
        ),
        (
            &mysql,
            "app.sessions",
            Some("has `app.sessions`. For MySQL, use the table name"),
        ),
        (&mysql, "sessions", None),
    ] {
        expect(file, &["--source-exclude-tables", names], error);
    }
    // CI systems give an empty value for a missing variable.
    let (code, _, stderr) = check(
        &["--config", &postgresql],
        &[("SVALBARD_SOURCE_EXCLUDE_TABLES", "")],
    );
    assert_eq!(code, Some(3));
    assert!(stderr.contains("has an empty name"), "{stderr}");
}

#[test]
fn json_output_reports_the_result() {
    let file = config_file("json", BASE);

    let (code, stdout, _) = check(&["--config", &file, "--json"], &[]);
    assert_eq!(code, Some(0));
    assert_eq!(stdout, "{\"excluded_tables\":[],\"valid\":true}\n");

    let (code, stdout, stderr) = check(
        &["--config", &file, "--json", "--safety-dump-timeout", "2d"],
        &[],
    );
    let result: serde_json::Value = serde_json::from_str(&stdout).expect("stdout must be JSON");
    assert_eq!(code, Some(3));
    assert_eq!(result["valid"], false);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .starts_with("safety.dump_timeout")
    );
    assert!(stderr.starts_with("error: safety.dump_timeout"));
}

#[test]
fn invalid_files_report_the_path_and_line() {
    for (name, contents, message) in [
        (
            "syntax",
            "[source]\nhost =\n",
            "string values must be quoted",
        ),
        (
            "unknown-key",
            "[source]\nhots = \"db\"\n",
            "unknown field `hots`",
        ),
        ("wrong-type", "[source]\nport = \"5432\"\n", "expected u16"),
    ] {
        let file = config_file(name, contents);
        let (code, _, stderr) = check(&["--config", &file], &[]);

        assert_eq!(code, Some(3), "{name}");
        assert!(stderr.contains(&format!("{file}:2: ")), "{name}: {stderr}");
        assert!(stderr.contains(message), "{name}: {stderr}");
    }
}

#[test]
fn missing_values_name_every_place_to_set_them() {
    let file = config_file(
        "missing-host",
        &BASE.replace("host = \"db.example.com\"\n", ""),
    );
    let (code, _, stderr) = check(&["--config", &file], &[]);

    assert_eq!(code, Some(3));
    assert!(stderr.contains("source.host is required"), "{stderr}");
    assert!(stderr.contains("--source-host"), "{stderr}");
    assert!(stderr.contains("SVALBARD_SOURCE_HOST"), "{stderr}");
}

#[test]
fn safety_limits_must_be_in_range() {
    let file = config_file("limits", BASE);
    for (flag, value, valid) in [
        ("--safety-lock-timeout", "1s", true),
        ("--safety-lock-timeout", "5min", true),
        ("--safety-lock-timeout", "0s", false),
        ("--safety-lock-timeout", "301s", false),
        ("--safety-lock-timeout", "10", false),
        ("--safety-contention-timeout", "60s", true),
        ("--safety-contention-timeout", "61s", false),
        ("--safety-dump-timeout", "24h", true),
        ("--safety-dump-timeout", "59s", false),
        ("--safety-dump-timeout", "1.5h", false),
        ("--safety-max-dump-size", "16TiB", true),
        ("--safety-max-dump-size", "1023KiB", false),
        ("--safety-min-free-space", "0B", true),
        ("--safety-min-free-space", "1025GiB", false),
    ] {
        let (code, _, stderr) = check(&["--config", &file, flag, value], &[]);
        let expected = if valid { 0 } else { 3 };

        assert_eq!(code, Some(expected), "{flag} {value}: {stderr}");
    }
}

#[test]
fn password_sources_are_checked_without_reading_them() {
    let file = config_file("password", BASE);
    let password_file = config_file("password-secret", SECRET);
    let directory = env!("CARGO_TARGET_TMPDIR");

    for (args, env, error) in [
        (vec!["--source-password-file", &password_file], vec![], None),
        (
            vec!["--source-password-file", &password_file],
            vec![("SVALBARD_SOURCE_PASSWORD", SECRET)],
            Some("not both"),
        ),
        (
            vec![],
            vec![("SVALBARD_SOURCE_PASSWORD", "")],
            Some("is empty"),
        ),
        (
            vec!["--source-password-file", "missing-password"],
            vec![],
            Some("cannot be opened"),
        ),
        (
            vec!["--source-password-file", directory],
            vec![],
            Some("is not a file"),
        ),
    ] {
        let (code, _, stderr) = check(&[&["--config", file.as_str()], &args[..]].concat(), &env);

        match error {
            None => assert_eq!(code, Some(0), "{args:?}: {stderr}"),
            Some(error) => {
                assert_eq!(code, Some(3), "{args:?}");
                assert!(stderr.contains(error), "{args:?}: {stderr}");
            }
        }
    }
}

#[test]
fn secrets_stay_out_of_output() {
    let quoted = config_file(
        "secret-quoted",
        &format!("[source]\npassword = \"{SECRET}\"\n"),
    );
    let bare = config_file("secret-bare", &format!("[source]\npassword = {SECRET}\n"));
    let valid = config_file("secret-valid", BASE);
    let env = [("SVALBARD_SOURCE_PASSWORD", SECRET)];

    for (args, env) in [
        (vec!["--config", &quoted, "--json"], &[][..]),
        (vec!["--config", &bare, "--json"], &[][..]),
        (vec!["--config", &valid], &env[..]),
        (vec!["--config", &valid, "--json"], &env[..]),
        (
            vec!["--config", &valid, "--safety-dump-timeout", "2d"],
            &env[..],
        ),
        (vec!["--help"], &env[..]),
    ] {
        let (_, stdout, stderr) = check(&args, env);

        assert!(!stdout.contains(SECRET), "{args:?}: {stdout}");
        assert!(!stderr.contains(SECRET), "{args:?}: {stderr}");
    }
}

#[test]
fn tls_settings_are_checked() {
    let file = config_file("tls", BASE);
    let ca_file = config_file("tls-ca", "not read by the check");

    for (args, error) in [
        (vec!["--source-tls", "verify-full"], None),
        (vec!["--source-tls", "disable"], None),
        (vec!["--source-tls-ca-file", &ca_file], None),
        (
            vec!["--source-tls", "require"],
            Some("Use `verify-full` or `disable`"),
        ),
        (
            vec!["--source-tls-ca-file", "missing-ca"],
            Some("source.tls_ca_file missing-ca cannot be opened"),
        ),
        (
            vec!["--source-tls", "disable", "--source-tls-ca-file", &ca_file],
            Some("needs source.tls `verify-full`"),
        ),
    ] {
        let (code, _, stderr) = check(&[&["--config", file.as_str()], &args[..]].concat(), &[]);

        match error {
            None => assert_eq!(code, Some(0), "{args:?}: {stderr}"),
            Some(error) => {
                assert_eq!(code, Some(3), "{args:?}");
                assert!(stderr.contains(error), "{args:?}: {stderr}");
            }
        }
    }
}

#[test]
fn source_type_is_postgresql_or_mysql() {
    let file = config_file("source-type", BASE);

    for (kind, code) in [("postgresql", 0), ("mysql", 0), ("mariadb", 3)] {
        let (actual, _, stderr) = check(&["--config", &file, "--source-type", kind], &[]);

        assert_eq!(actual, Some(code), "{kind}: {stderr}");
    }
    let (_, _, stderr) = check(&["--config", &file, "--source-type", "mariadb"], &[]);
    assert!(stderr.contains("Use `postgresql` or `mysql`"), "{stderr}");
}

#[test]
fn destination_type_endpoint_and_upload_timeout_are_checked() {
    let file = config_file("destination", BASE);
    let r2 = |endpoint| {
        vec![
            "--destination-type",
            "r2",
            "--destination-endpoint",
            endpoint,
        ]
    };

    for (args, error) in [
        (
            vec!["--destination-type", "gcs"],
            Some("destination.type is `gcs`. Use `s3`, `r2`, `sftp` or `scp`."),
        ),
        (
            vec!["--destination-host", "backup.example.com"],
            Some("destination.host does not apply to destination.type `s3`."),
        ),
        (
            vec!["--destination-type", "r2"],
            Some("destination.endpoint is required"),
        ),
        (r2("https://acct.r2.cloudflarestorage.com"), None),
        (r2("https://acct.eu.r2.cloudflarestorage.com/"), None),
        (
            r2("https://acct.r2.cloudflarestorage.com/backups"),
            Some("Remove the path: the bucket goes in destination.bucket."),
        ),
        (
            vec![
                "--destination-endpoint",
                "https://acct.r2.cloudflarestorage.com",
            ],
            Some("a Cloudflare R2 endpoint. Set destination.type to `r2`."),
        ),
        (
            vec!["--destination-endpoint", "https://s3.example.com"],
            None,
        ),
        (vec!["--destination-endpoint", "http://floci:4566"], None),
        (
            vec!["--destination-endpoint", "s3.example.com"],
            Some("Use an http:// or https:// URL"),
        ),
        (
            vec![
                "--destination-endpoint",
                &format!("key:{SECRET}@s3.example.com"),
            ],
            Some("must not contain credentials"),
        ),
        (vec!["--destination-upload-timeout", "24h"], None),
        (
            vec!["--destination-upload-timeout", "25h"],
            Some("destination.upload_timeout must be from 1min to 24h"),
        ),
    ] {
        expect(&file, &args, error);
    }
}

#[test]
fn ssh_destinations_are_checked() {
    let file = config_file(
        "ssh",
        &BASE.replace(
            "type = \"s3\"\nbucket = \"example-backups\"",
            "type = \"sftp\"\nhost = \"backup.example.com\"\nuser = \"svalbard\"\n\
             path = \"/srv/backups\"",
        ),
    );
    let key = config_file("ssh-key", "");

    for (args, error) in [
        (vec![], None),
        (
            vec!["--destination-type", "scp", "--destination-port", "2222"],
            None,
        ),
        (vec!["--destination-host", "2001:db8::1"], None),
        (
            vec![
                "--destination-identity-file",
                &key,
                "--destination-known-hosts-file",
                &key,
            ],
            None,
        ),
        (vec!["--destination-prefix", "app/daily-"], None),
        (
            vec!["--destination-host=-oProxyCommand=x"],
            Some("destination.host is `-oProxyCommand=x`. Use a host name or an IP address."),
        ),
        (
            vec!["--destination-user", "svalbard@example"],
            Some("destination.user is `svalbard@example`."),
        ),
        (
            vec!["--destination-path", "srv/backups"],
            Some("destination.path is `srv/backups`. Use an absolute path"),
        ),
        (
            vec!["--destination-path", "/srv/a\"b"],
            Some("Use an absolute path without"),
        ),
        (
            vec!["--destination-prefix", "app $(id)/"],
            Some("For SSH, use letters, digits"),
        ),
        (
            vec!["--destination-port", "0"],
            Some("destination.port is `0`."),
        ),
        (
            vec!["--destination-bucket", "example-backups"],
            Some("destination.bucket does not apply to destination.type `sftp`."),
        ),
        (
            vec!["--destination-identity-file", "/nonexistent/key"],
            Some("destination.identity_file /nonexistent/key cannot be opened"),
        ),
    ] {
        expect(&file, &args, error);
    }
}

/// Checks the configuration with `args`, which must fail with `error` or pass.
fn expect(file: &str, args: &[&str], error: Option<&str>) {
    let (code, _, stderr) = check(&[&["--config", file], args].concat(), &[]);

    match error {
        None => assert_eq!(code, Some(0), "{args:?}: {stderr}"),
        Some(error) => {
            assert_eq!(code, Some(3), "{args:?}");
            assert!(stderr.contains(error), "{args:?}: {stderr}");
            assert!(!stderr.contains(SECRET), "{args:?}: {stderr}");
        }
    }
}

#[test]
fn staging_dir_must_be_a_directory() {
    let file = config_file("staging", BASE);

    for (args, error) in [
        (
            vec!["--safety-staging-dir", env!("CARGO_TARGET_TMPDIR")],
            None,
        ),
        (
            vec!["--safety-staging-dir", "/nonexistent/staging"],
            Some("safety.staging_dir /nonexistent/staging cannot be opened"),
        ),
        (
            vec!["--safety-staging-dir", &file],
            Some("is not a directory."),
        ),
    ] {
        expect(&file, &args, error);
    }
}
