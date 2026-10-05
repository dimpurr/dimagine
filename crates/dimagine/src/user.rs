//! `dimagine user`: account management CLI (ADR-014).
//!
//! Subcommands:
//! - `create --email <EMAIL> [--password-stdin]`
//! - `passwd --email <EMAIL> [--password-stdin]`
//! - `list`
//!
//! Accounts live outside the library in `--data-dir <DIR>`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use dimagine_serve::accounts::{default_data_dir, AccountsStore, MIN_PASSWORD_LENGTH};

use crate::emit_failure;

const SCHEMA: &str = "dimagine.user/0.1";

pub fn command() -> Command {
    Command::new("user")
        .about("Manage viewer user accounts.")
        .arg(
            Arg::new("data_dir")
                .long("data-dir")
                .value_name("DIR")
                .global(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("State directory where accounts are stored (default: $XDG_STATE_HOME/dimagine or ~/.local/state/dimagine)."),
        )
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("create")
                .about("Create a new user account.")
                .arg(
                    Arg::new("email")
                        .long("email")
                        .value_name("EMAIL")
                        .required(true)
                        .help("Email address for the user."),
                )
                .arg(
                    Arg::new("password_stdin")
                        .long("password-stdin")
                        .action(ArgAction::SetTrue)
                        .help("Read password from standard input without prompting."),
                ),
        )
        .subcommand(
            Command::new("passwd")
                .about("Change password for an existing user account.")
                .arg(
                    Arg::new("email")
                        .long("email")
                        .value_name("EMAIL")
                        .required(true)
                        .help("Email address of the user."),
                )
                .arg(
                    Arg::new("password_stdin")
                        .long("password-stdin")
                        .action(ArgAction::SetTrue)
                        .help("Read new password from standard input without prompting."),
                ),
        )
        .subcommand(
            Command::new("list")
                .about("List existing user accounts (emails and roles only)."),
        )
}

pub fn run(sub: &ArgMatches) -> ExitCode {
    let json = sub.get_flag("json");
    let Some((subcommand, sub_matches)) = sub.subcommand() else {
        return ExitCode::from(2);
    };

    let data_dir = sub
        .get_one::<PathBuf>("data_dir")
        .or_else(|| sub_matches.get_one::<PathBuf>("data_dir"))
        .cloned()
        .unwrap_or_else(default_data_dir);

    let store = AccountsStore::new(&data_dir);

    match subcommand {
        "create" => run_create(sub_matches, &store, json),
        "passwd" => run_passwd(sub_matches, &store, json),
        "list" => run_list(&store, json),
        _ => ExitCode::from(2),
    }
}

fn read_password(password_stdin: bool) -> Result<String, String> {
    if !password_stdin {
        eprint!("Password: ");
        let _ = std::io::stderr().flush();
    }
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read password from stdin: {e}"))?;
    let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
    if trimmed.is_empty() {
        return Err("password cannot be empty".to_string());
    }
    if trimmed.len() < MIN_PASSWORD_LENGTH {
        return Err(format!(
            "password must be at least {MIN_PASSWORD_LENGTH} characters"
        ));
    }
    Ok(trimmed)
}

fn run_create(matches: &ArgMatches, store: &AccountsStore, json: bool) -> ExitCode {
    let email = match matches.get_one::<String>("email") {
        Some(email) if !email.trim().is_empty() => email.trim(),
        _ => {
            emit_failure(json, "email address cannot be empty");
            return ExitCode::from(1);
        }
    };
    let password_stdin = matches.get_flag("password_stdin");
    let password = match read_password(password_stdin) {
        Ok(pass) => pass,
        Err(err) => {
            emit_failure(json, &err);
            return ExitCode::from(1);
        }
    };

    // First user created becomes "owner", subsequent users become "user".
    // An unreadable store is a hard error: guessing "owner" here would create
    // an account in a store whose state is unknown.
    let role = match store.has_users() {
        Ok(false) => "owner",
        Ok(true) => "user",
        Err(err) => {
            emit_failure(json, &err.to_string());
            return ExitCode::from(1);
        }
    };

    match store.create_user(email, &password, role) {
        Ok(record) => {
            if json {
                let doc = serde_json::json!({
                    "schema": SCHEMA,
                    "created": {
                        "id": record.id,
                        "email": record.email,
                        "role": record.role,
                    }
                });
                println!("{}", serde_json::to_string(&doc).unwrap_or_default());
            } else {
                println!("created user {} ({})", record.email, record.role);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            emit_failure(json, &err.to_string());
            ExitCode::from(1)
        }
    }
}

fn run_passwd(matches: &ArgMatches, store: &AccountsStore, json: bool) -> ExitCode {
    let email = match matches.get_one::<String>("email") {
        Some(email) if !email.trim().is_empty() => email.trim(),
        _ => {
            emit_failure(json, "email address cannot be empty");
            return ExitCode::from(1);
        }
    };
    let password_stdin = matches.get_flag("password_stdin");
    let password = match read_password(password_stdin) {
        Ok(pass) => pass,
        Err(err) => {
            emit_failure(json, &err);
            return ExitCode::from(1);
        }
    };

    match store.set_password(email, &password) {
        Ok(()) => {
            if json {
                let doc = serde_json::json!({
                    "schema": SCHEMA,
                    "updated": {
                        "email": email,
                    }
                });
                println!("{}", serde_json::to_string(&doc).unwrap_or_default());
            } else {
                println!("updated password for {email}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            emit_failure(json, &err.to_string());
            ExitCode::from(1)
        }
    }
}

fn run_list(store: &AccountsStore, json: bool) -> ExitCode {
    match store.load() {
        Ok(doc) => {
            if json {
                let users: Vec<_> = doc
                    .users
                    .iter()
                    .map(|u| {
                        serde_json::json!({
                            "email": u.email,
                            "role": u.role,
                        })
                    })
                    .collect();
                let output = serde_json::json!({
                    "schema": SCHEMA,
                    "users": users,
                });
                println!("{}", serde_json::to_string(&output).unwrap_or_default());
            } else {
                for u in &doc.users {
                    println!("{}\t{}", u.email, u.role);
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            emit_failure(json, &err.to_string());
            ExitCode::from(1)
        }
    }
}
