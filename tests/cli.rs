use std::process::{Command, Stdio};

#[test]
fn help_succeeds_without_input() {
    for flag in ["--help", "-h"] {
        let output = Command::new(env!("CARGO_BIN_EXE_svalbard"))
            .arg(flag)
            .stdin(Stdio::null())
            .output()
            .expect("CLI must start");

        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: svalbard"));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn backup_help_documents_the_exit_codes() {
    let output = Command::new(env!("CARGO_BIN_EXE_svalbard"))
        .args(["backup", "--help"])
        .stdin(Stdio::null())
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("Usage: svalbard backup"), "{help}");
    assert!(help.contains("5 the cleanup failed too"), "{help}");
}

#[test]
fn backup_with_an_invalid_configuration_does_nothing() {
    let output = Command::new(env!("CARGO_BIN_EXE_svalbard"))
        .args(["backup", "--json"])
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .expect("CLI must start");

    assert_eq!(output.status.code(), Some(3));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["result"], "failed");
    assert_eq!(result["stage"], "configuration");
    assert_eq!(result["error"], "invalid_configuration");
    assert!(result.get("run_id").is_none(), "{result}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("error: configuration: source.type is required"),
        "{stderr}"
    );
}

#[test]
fn version_matches_the_package() {
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_svalbard"))
            .arg(flag)
            .stdin(Stdio::null())
            .output()
            .expect("CLI must start");

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).expect("version must be UTF-8"),
            concat!("svalbard ", env!("CARGO_PKG_VERSION"), "\n")
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn invalid_arguments_fail_with_a_diagnostic() {
    for args in [
        vec![],
        vec!["--unknown"],
        vec!["backup", "--unknown"],
        vec!["config", "check", "--unknown"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_svalbard"))
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .expect("CLI must start");

        assert_eq!(output.status.code(), Some(2), "arguments: {args:?}");
        assert!(output.stdout.is_empty(), "arguments: {args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("error:"),
            "arguments: {args:?}"
        );
    }
}
