//! `dimagine user`: account management CLI (ADR-014).
//!
//! Subcommands:
//! - `create --email <EMAIL> [--password-stdin] [--allow-weak]`
//! - `passwd --email <EMAIL> [--password-stdin] [--allow-weak]`
//! - `delete <EMAIL> [--yes]`
//! - `list`
//!
//! Any non-empty password is accepted. One shorter than 8 characters is easy
//! to guess, so it is only used after the operator confirms it: with
//! `--password-stdin` that confirmation is the `--allow-weak` flag, since
//! standard input is carrying the password.
//!
//! Accounts live outside the library in `--data-dir <DIR>`. Deleting the last
//! account puts the next `dimagine serve` start back into first-run setup.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use dimagine_serve::accounts::{
    default_data_dir, is_weak_password, AccountsStore, WEAK_PASSWORD_LENGTH,
};

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
                .arg(password_stdin_arg())
                .arg(allow_weak_arg()),
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
                .arg(password_stdin_arg())
                .arg(allow_weak_arg()),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete a user account. Deleting the last account makes the next serve start offer setup again.")
                .arg(
                    Arg::new("email")
                        .value_name("EMAIL")
                        .required(true)
                        .help("Email address of the account to delete."),
                )
                .arg(
                    Arg::new("yes")
                        .long("yes")
                        .action(ArgAction::SetTrue)
                        .help("Delete without the confirmation prompt."),
                ),
        )
        .subcommand(
            Command::new("list")
                .about("List existing user accounts (emails and roles only)."),
        )
}

fn password_stdin_arg() -> Arg {
    Arg::new("password_stdin")
        .long("password-stdin")
        .action(ArgAction::SetTrue)
        .help("Read password from standard input without prompting.")
}

fn allow_weak_arg() -> Arg {
    Arg::new("allow_weak")
        .long("allow-weak")
        .action(ArgAction::SetTrue)
        .help(format!(
            "Accept a password shorter than {WEAK_PASSWORD_LENGTH} characters without asking again."
        ))
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
        "delete" => run_delete(sub_matches, &store, json),
        "list" => run_list(&store, json),
        _ => ExitCode::from(2),
    }
}

fn read_password(password_stdin: bool) -> Result<String, String> {
    let line = if !password_stdin && std::io::stdin().is_terminal() {
        // Interactive terminal: rpassword disables echo while the password is
        // typed, so it stays off the screen and out of the scrollback.
        rpassword::prompt_password("Password: ")
            .map_err(|e| format!("cannot read password from terminal: {e}"))?
    } else {
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
        line
    };
    let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
    if trimmed.is_empty() {
        return Err("password cannot be empty".to_string());
    }
    Ok(trimmed)
}

/// Check that a weak password may be used, asking the operator when standard
/// input is not carrying the password. `Ok(())` means it may be used.
fn accept_weak_password(
    password: &str,
    password_stdin: bool,
    allow_weak: bool,
) -> Result<(), String> {
    if !is_weak_password(password) || allow_weak {
        return Ok(());
    }
    if password_stdin {
        // Standard input holds the password, so there is nobody left to ask:
        // the flag is the confirmation.
        return Err(format!(
            "password is shorter than {WEAK_PASSWORD_LENGTH} characters; \
             pass --allow-weak to use it anyway"
        ));
    }
    if confirm(&format!(
        "This password is fewer than {WEAK_PASSWORD_LENGTH} characters and is easy to guess. \
         Use it anyway? [y/N] "
    ))? {
        Ok(())
    } else {
        Err("refused a weak password".to_string())
    }
}

/// Ask a yes/no question on stderr and read the answer from standard input.
/// Anything that is not `y` or `yes` — including end of input — is a no.
fn confirm(question: &str) -> Result<bool, String> {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut answer = String::new();
    stdin
        .lock()
        .read_line(&mut answer)
        .map_err(|e| format!("cannot read the answer from stdin: {e}"))?;
    let answer = answer.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

/// Read the password for `create`/`passwd` and decide whether it may be used.
fn read_new_password(matches: &ArgMatches) -> Result<String, String> {
    let password_stdin = matches.get_flag("password_stdin");
    let allow_weak = matches.get_flag("allow_weak");
    let password = read_password(password_stdin)?;
    accept_weak_password(&password, password_stdin, allow_weak)?;
    Ok(password)
}

fn run_create(matches: &ArgMatches, store: &AccountsStore, json: bool) -> ExitCode {
    let email = match matches.get_one::<String>("email") {
        Some(email) if !email.trim().is_empty() => email.trim(),
        _ => {
            emit_failure(json, "email address cannot be empty");
            return ExitCode::from(1);
        }
    };
    let password = match read_new_password(matches) {
        Ok(pass) => pass,
        Err(err) => {
            emit_failure(json, &err);
            return ExitCode::from(1);
        }
    };

    // First user created becomes "owner", subsequent users become "user".
    // The role is stored, not enforced: every authenticated account currently
    // reaches the same read-only viewer, and `role` is reserved for the
    // future multi-user surface. An unreadable store is a hard error:
    // guessing "owner" here would create an account in a store whose state
    // is unknown.
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
    let password = match read_new_password(matches) {
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

fn run_delete(matches: &ArgMatches, store: &AccountsStore, json: bool) -> ExitCode {
    let email = match matches.get_one::<String>("email") {
        Some(email) if !email.trim().is_empty() => email.trim().to_string(),
        _ => {
            emit_failure(json, "email address cannot be empty");
            return ExitCode::from(1);
        }
    };
    // Look the account up first: a typo must not cost a yes/no answer, and the
    // prompt has to name what it is about to remove.
    let existing = match store.find_user_by_email(&email) {
        Ok(Some(record)) => record,
        Ok(None) => {
            emit_failure(json, &format!("no account with email '{email}'"));
            return ExitCode::from(1);
        }
        Err(err) => {
            emit_failure(json, &err.to_string());
            return ExitCode::from(1);
        }
    };
    if !matches.get_flag("yes") {
        match confirm(&format!(
            "Delete {} ({}), losing this login? [y/N] ",
            existing.email, existing.role
        )) {
            Ok(true) => {}
            Ok(false) => {
                emit_failure(json, "refused: no account was deleted");
                return ExitCode::from(1);
            }
            Err(err) => {
                emit_failure(json, &err);
                return ExitCode::from(1);
            }
        }
    }

    let removed = match store.delete_user(&existing.email) {
        Ok(record) => record,
        Err(err) => {
            emit_failure(json, &err.to_string());
            return ExitCode::from(1);
        }
    };
    let remaining = match store.load() {
        Ok(doc) => doc.users.len(),
        Err(err) => {
            emit_failure(json, &err.to_string());
            return ExitCode::from(1);
        }
    };
    if json {
        let doc = serde_json::json!({
            "schema": SCHEMA,
            "deleted": {
                "id": removed.id,
                "email": removed.email,
                "role": removed.role,
            },
            "accounts_left": remaining,
        });
        println!("{}", serde_json::to_string(&doc).unwrap_or_default());
    } else {
        println!("deleted user {} ({})", removed.email, removed.role);
        if remaining == 0 {
            println!("no accounts left: the next dimagine serve start offers /setup again");
        }
    }
    ExitCode::SUCCESS
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
