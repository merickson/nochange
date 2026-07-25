use clap::Parser;
use nochange::app;
use nochange::cli::Cli;
use nochange::error::ExitCode;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match Cli::try_parse() {
        Ok(cli) => match app::run(cli).await {
            Ok(()) => ExitCode::Success.into(),
            Err(error) => {
                eprintln!("nochange: {error}");
                error.get_exit_code().into()
            }
        },
        Err(error) => {
            let exit_code = if error.use_stderr() {
                ExitCode::Usage
            } else {
                ExitCode::Success
            };
            let _print_result = error.print();
            exit_code.into()
        }
    }
}
