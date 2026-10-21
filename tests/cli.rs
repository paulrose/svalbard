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
        vec!["backup"],
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
