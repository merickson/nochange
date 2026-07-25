use nochange::config::ConfigError;
use nochange::error::{AppError, ExitCode};
use nochange::init::InitError;
use nochange::sync::SyncError;

#[test]
fn classifies_application_errors_with_stable_exit_codes() {
    let cases = [
        (
            AppError::from(ConfigError::HomeDirectoryUnavailable),
            ExitCode::Configuration,
        ),
        (
            AppError::from(InitError::MissingRefreshToken),
            ExitCode::Unavailable,
        ),
        (
            AppError::from(SyncError::InvalidDeltaPage),
            ExitCode::TemporaryFailure,
        ),
        (
            AppError::Temporary("one account failed".into()),
            ExitCode::TemporaryFailure,
        ),
        (AppError::Usage("bad arguments".into()), ExitCode::Usage),
        (
            AppError::Software("internal failure".into()),
            ExitCode::Software,
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.get_exit_code(), expected);
    }
}

#[test]
fn exposes_all_sendmail_exit_code_values() {
    let cases = [
        (ExitCode::Success, 0),
        (ExitCode::Usage, 64),
        (ExitCode::DataError, 65),
        (ExitCode::Unavailable, 69),
        (ExitCode::Software, 70),
        (ExitCode::TemporaryFailure, 75),
        (ExitCode::Configuration, 78),
    ];

    for (exit_code, expected) in cases {
        assert_eq!(exit_code as u8, expected);
        let _: std::process::ExitCode = exit_code.into();
    }
}
