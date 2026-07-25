use assert_cmd::Command;
use clap::Parser;
use nochange::cli::{Cli, Command as NochangeCommand};
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn parses_init_options_and_global_options() {
    let cli = Cli::try_parse_from([
        "nochange",
        "--config",
        "/tmp/nochange.conf",
        "--verbose",
        "init",
        "--account",
        "work",
        "--device-code",
    ])
    .expect("the documented init command should parse");

    assert_eq!(cli.config, Some(PathBuf::from("/tmp/nochange.conf")));
    assert!(cli.verbose);
    assert!(matches!(
        cli.command,
        NochangeCommand::Init(args)
            if args.account.as_deref() == Some("work") && args.device_code
    ));
}

#[test]
fn parses_sync_options() {
    let cli = Cli::try_parse_from([
        "nochange",
        "sync",
        "--account",
        "work",
        "--dry-run",
        "--no-fsync",
    ])
    .expect("the documented sync command should parse");

    assert!(matches!(
        cli.command,
        NochangeCommand::Sync(args)
            if args.account.as_deref() == Some("work") && args.dry_run && args.no_fsync
    ));

    let defaults =
        Cli::try_parse_from(["nochange", "sync"]).expect("default sync command should parse");
    assert!(matches!(
        defaults.command,
        NochangeCommand::Sync(args) if !args.dry_run && !args.no_fsync
    ));
}

#[test]
fn parses_sendmail_compatible_options_and_recipients() {
    let cli = Cli::try_parse_from([
        "nochange",
        "send",
        "-a",
        "work",
        "-f",
        "sender@example.com",
        "-t",
        "-oi",
        "--",
        "one@example.com",
        "two@example.com",
    ])
    .expect("the documented send command should parse");

    assert!(matches!(
        cli.command,
        NochangeCommand::Send(args)
            if args.account.as_deref() == Some("work")
                && args.from.as_deref() == Some("sender@example.com")
                && args.read_recipients_from_headers
                && args.ignore_dot
                && args.recipients == ["one@example.com", "two@example.com"]
    ));
}

#[test]
fn rejects_partial_sendmail_oi_option() {
    let error = Cli::try_parse_from(["nochange", "send", "-o"])
        .expect_err("-o alone is not part of the supported sendmail subset");

    assert_eq!(error.exit_code(), 2);
}

#[test]
fn reports_usage_errors_with_sendmail_exit_code() {
    let mut command = Command::cargo_bin("nochange").expect("binary should build");

    command
        .arg("unknown")
        .assert()
        .code(64)
        .stderr(predicate::str::contains("Usage:"));
}

#[test]
fn prints_help_and_returns_success() {
    let mut command = Command::cargo_bin("nochange").expect("binary should build");

    command
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"));
}

#[test]
fn accepts_a_documented_command() {
    let mut command = Command::cargo_bin("nochange").expect("binary should build");

    command.args(["sync", "--help"]).assert().success();
}

#[test]
fn init_reports_missing_configuration_with_the_configuration_exit_code() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let missing = temp.path().join("missing.conf");
    let mut command = Command::cargo_bin("nochange").expect("binary should build");

    command
        .args([
            "--config",
            missing.to_str().expect("path should be UTF-8"),
            "init",
        ])
        .assert()
        .code(78)
        .stderr(predicate::str::contains("could not load configuration"));
}

#[test]
fn init_rejects_an_unknown_account_before_attempting_login() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let config = temp.path().join("nochange.conf");
    fs::write(
        &config,
        format!(
            "[global]\naccounts = work\n[work]\nmaildir = {}\nuser = me@example.com\nclientid = client-id\n",
            temp.path().join("mail").display()
        ),
    )
    .expect("configuration should be writable");
    let mut command = Command::cargo_bin("nochange").expect("binary should build");

    command
        .args([
            "--config",
            config.to_str().expect("path should be UTF-8"),
            "init",
            "--account",
            "missing",
        ])
        .assert()
        .code(78)
        .stderr(predicate::str::contains("unknown account 'missing'"));
}

#[test]
fn sync_reports_missing_configuration_with_the_configuration_exit_code() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let missing = temp.path().join("missing.conf");
    let mut command = Command::cargo_bin("nochange").expect("binary should build");

    command
        .args([
            "--config",
            missing.to_str().expect("path should be UTF-8"),
            "sync",
            "--dry-run",
        ])
        .assert()
        .code(78)
        .stderr(predicate::str::contains("could not load configuration"));
}

#[test]
fn sync_rejects_an_unknown_account_before_accessing_credentials() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let config = temp.path().join("nochange.conf");
    fs::write(
        &config,
        format!(
            "[global]\naccounts = work\n[work]\nmaildir = {}\nuser = me@example.com\nclientid = client-id\n",
            temp.path().join("mail").display()
        ),
    )
    .expect("configuration should be writable");
    let mut command = Command::cargo_bin("nochange").expect("binary should build");

    command
        .args([
            "--config",
            config.to_str().expect("path should be UTF-8"),
            "sync",
            "--account",
            "missing",
        ])
        .assert()
        .code(78)
        .stderr(predicate::str::contains("unknown account 'missing'"));
}
