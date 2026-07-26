//! Command line surface.
//!
//! Deliberately hand-rolled: the whole surface is a handful of flags plus a
//! couple of diagnostic subcommands, and an installer or a systemd unit is the
//! only thing that ever passes them. That is not worth an argument-parsing
//! dependency in a binary whose selling point is that it drops onto a host
//! with nothing else.

use std::path::PathBuf;
use std::process::ExitCode;

/// What the process was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Boot the server. The default when no subcommand is given.
    Run,
    /// Load and validate configuration, then exit. What an installer calls to
    /// fail loudly before enabling a unit that would only crash-loop.
    ConfigCheck,
    /// Report on the whole installation: paths, config, port, host tooling.
    Doctor,
}

/// Parsed invocation.
#[derive(Debug)]
pub struct Cli {
    pub command: Command,
    pub config_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
}

/// Outcome of parsing: either something to do, or a message already written
/// and an exit code to leave with.
pub enum Parsed {
    Run(Cli),
    Exit(ExitCode),
}

const HELP: &str = "\
remon-server — self-hosted system monitoring server

USAGE:
    remon-server [OPTIONS] [COMMAND]

COMMANDS:
    (none)                   Run the server
    config check             Validate configuration and exit
    doctor                   Report paths, config, port and host tooling

OPTIONS:
    -c, --config-dir <DIR>   Directory holding config.toml    [env: REMON_CONFIG_DIR]
    -d, --data-dir <DIR>     Directory for the database        [env: REMON_DATA_DIR]
    -V, --version            Print version and exit
    -h, --help               Print this help and exit

CONFIGURATION:
    Defaults are compiled into the binary, so no config file is required.
    Layers, each optional and applied in order:
        <config-dir>/default.toml
        <config-dir>/config.toml
        <config-dir>/<RUN_ENV>.toml     (RUN_ENV defaults to \"development\")
        REMON__<SECTION>__<KEY> environment variables

    Run from a repo checkout, paths stay relative to it. Installed, they
    default to /etc/remon and /var/lib/remon (or the per-user equivalents
    when not running as root).
";

/// Parse `std::env::args_os`. Anything that terminates the process — help,
/// version, a bad flag — is written here and returned as an exit code, so
/// `main` stays a straight line.
pub fn parse() -> Parsed {
    parse_from(std::env::args().skip(1))
}

fn parse_from<I: Iterator<Item = String>>(args: I) -> Parsed {
    let mut config_dir = None;
    let mut data_dir = None;
    let mut command = Command::Run;
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "doctor" => command = Command::Doctor,
            "config" => match args.next().as_deref() {
                Some("check") => command = Command::ConfigCheck,
                Some(other) => {
                    eprintln!("remon-server: unknown 'config' subcommand '{other}'");
                    eprintln!("Try 'remon-server --help' for usage.");
                    return Parsed::Exit(ExitCode::FAILURE);
                }
                None => {
                    eprintln!(
                        "remon-server: 'config' needs a subcommand (did you mean 'config check'?)"
                    );
                    return Parsed::Exit(ExitCode::FAILURE);
                }
            },
            "-h" | "--help" => {
                print!("{HELP}");
                return Parsed::Exit(ExitCode::SUCCESS);
            }
            "-V" | "--version" => {
                println!("remon-server {}", env!("CARGO_PKG_VERSION"));
                return Parsed::Exit(ExitCode::SUCCESS);
            }
            "-c" | "--config-dir" => match take_value(&mut args, &arg) {
                Ok(v) => config_dir = Some(PathBuf::from(v)),
                Err(code) => return Parsed::Exit(code),
            },
            "-d" | "--data-dir" => match take_value(&mut args, &arg) {
                Ok(v) => data_dir = Some(PathBuf::from(v)),
                Err(code) => return Parsed::Exit(code),
            },
            other => {
                // `--flag=value` form, then give up.
                if let Some((flag, value)) = other.split_once('=') {
                    match flag {
                        "-c" | "--config-dir" => {
                            config_dir = Some(PathBuf::from(value));
                            continue;
                        }
                        "-d" | "--data-dir" => {
                            data_dir = Some(PathBuf::from(value));
                            continue;
                        }
                        _ => {}
                    }
                }
                eprintln!("remon-server: unrecognized argument '{other}'");
                eprintln!("Try 'remon-server --help' for usage.");
                return Parsed::Exit(ExitCode::FAILURE);
            }
        }
    }

    Parsed::Run(Cli {
        command,
        config_dir,
        data_dir,
    })
}

/// Consume the value following a flag, complaining if it is missing.
fn take_value<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Result<String, ExitCode> {
    match args.next() {
        Some(v) if !v.starts_with('-') => Ok(v),
        _ => {
            eprintln!("remon-server: {flag} requires a directory argument");
            Err(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Parsed {
        parse_from(args.iter().map(|s| s.to_string()))
    }

    fn expect_run(args: &[&str]) -> Cli {
        match parse_args(args) {
            Parsed::Run(cli) => cli,
            Parsed::Exit(_) => panic!("expected a run, got an exit"),
        }
    }

    #[test]
    fn no_arguments_runs_the_server() {
        let cli = expect_run(&[]);
        assert_eq!(cli.command, Command::Run);
        assert!(cli.config_dir.is_none());
        assert!(cli.data_dir.is_none());
    }

    #[test]
    fn directory_flags_accept_both_forms() {
        let spaced = expect_run(&["--config-dir", "/etc/remon", "--data-dir", "/srv/remon"]);
        assert_eq!(spaced.config_dir, Some(PathBuf::from("/etc/remon")));
        assert_eq!(spaced.data_dir, Some(PathBuf::from("/srv/remon")));

        let joined = expect_run(&["--config-dir=/etc/remon", "-d=/srv/remon"]);
        assert_eq!(joined.config_dir, Some(PathBuf::from("/etc/remon")));
        assert_eq!(joined.data_dir, Some(PathBuf::from("/srv/remon")));
    }

    #[test]
    fn short_flags_work() {
        let cli = expect_run(&["-c", "/etc/remon"]);
        assert_eq!(cli.config_dir, Some(PathBuf::from("/etc/remon")));
    }

    #[test]
    fn help_and_version_exit_without_running() {
        assert!(matches!(parse_args(&["--help"]), Parsed::Exit(_)));
        assert!(matches!(parse_args(&["-V"]), Parsed::Exit(_)));
    }

    #[test]
    fn a_flag_missing_its_value_is_an_error() {
        // The next token is another flag, not a directory.
        assert!(matches!(
            parse_args(&["--config-dir", "--data-dir"]),
            Parsed::Exit(_)
        ));
        assert!(matches!(parse_args(&["--data-dir"]), Parsed::Exit(_)));
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(matches!(parse_args(&["--nope"]), Parsed::Exit(_)));
    }

    #[test]
    fn subcommands_are_recognized() {
        assert_eq!(expect_run(&["doctor"]).command, Command::Doctor);
        assert_eq!(
            expect_run(&["config", "check"]).command,
            Command::ConfigCheck
        );
    }

    #[test]
    fn subcommands_combine_with_directory_flags() {
        let cli = expect_run(&["--config-dir", "/etc/remon", "doctor"]);
        assert_eq!(cli.command, Command::Doctor);
        assert_eq!(cli.config_dir, Some(PathBuf::from("/etc/remon")));
    }

    #[test]
    fn an_incomplete_config_subcommand_is_an_error() {
        assert!(matches!(parse_args(&["config"]), Parsed::Exit(_)));
        assert!(matches!(parse_args(&["config", "nope"]), Parsed::Exit(_)));
    }
}
