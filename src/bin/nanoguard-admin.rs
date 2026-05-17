//! `nanoguard-admin` — offline user-management CLI for the Console DB.
//!
//! The Console (`nanoguard-console`) provides a web UI for managing users, but
//! some operations have to work without a working login — most importantly
//! resetting a forgotten admin password. This binary covers that gap.
//!
//! It reads the same `nanoguard.toml` the Console reads, opens the same SQLite
//! database (`[budget].db_path`), and reuses `nanoguard::console::auth` and
//! `nanoguard::console::db` directly so the on-disk format stays in sync.

use anyhow::{anyhow, Context, Result};
use nanoguard::console::{auth, db};
use std::io::{self, BufRead, IsTerminal, Write};
use zeroize::Zeroizing;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let sub = match args.next() {
        Some(s) => s,
        None => {
            print_usage();
            std::process::exit(2);
        }
    };

    match sub.as_str() {
        "set-password" => cmd_set_password(args.collect()),
        "list-users" => cmd_list_users(),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("unknown subcommand: {}", other);
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!(
        "nanoguard-admin — offline user management for the Console DB

USAGE:
    nanoguard-admin <SUBCOMMAND>

SUBCOMMANDS:
    set-password <username> [--password <pw>] [--password-stdin]
        Reset a user's password. With no flags, prompts on the
        terminal with echo off. --password-stdin reads one line from
        stdin (suitable for piping from a password manager). --password
        accepts an inline value but lands in shell history and process
        listings, so use it only for scripted single-shot runs.

    list-users
        Print a tabular dump of every row in the `users` table.

    help, -h, --help
        Print this message.

The TOML config (`[budget].db_path`) determines which database is read,
so run this binary from the same working directory as `nanoguard-console`
or set NANOGUARD_CONFIG appropriately."
    );
}

fn cmd_set_password(args: Vec<String>) -> Result<()> {
    let mut username: Option<String> = None;
    let mut inline_password: Option<Zeroizing<String>> = None;
    let mut from_stdin = false;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--password" => {
                let v = it
                    .next()
                    .ok_or_else(|| anyhow!("--password requires a value"))?;
                inline_password = Some(Zeroizing::new(v));
            }
            "--password-stdin" => from_stdin = true,
            other if !other.starts_with("--") => {
                if username.is_some() {
                    return Err(anyhow!("unexpected positional argument: {}", other));
                }
                username = Some(other.to_string());
            }
            other => return Err(anyhow!("unknown flag: {}", other)),
        }
    }

    let username =
        username.ok_or_else(|| anyhow!("set-password requires a <username>; see --help"))?;

    let password = match (inline_password, from_stdin) {
        (Some(p), false) => p,
        (None, true) => read_password_from_stdin()?,
        (None, false) => prompt_password_tty(&username)?,
        (Some(_), true) => {
            return Err(anyhow!(
                "--password and --password-stdin are mutually exclusive"
            ));
        }
    };

    if password.is_empty() {
        return Err(anyhow!("password is empty"));
    }
    if auth::is_common_password(&password) {
        return Err(anyhow!(
            "password matches a well-known weak password list; pick something stronger"
        ));
    }

    let cfg = nanoguard::config::Config::from_env_or_default()
        .context("loading config to discover [budget].db_path")?;
    let db = db::ConsoleDb::open(&cfg.budget.db_path)
        .with_context(|| format!("opening console DB at {}", cfg.budget.db_path))?;

    let hash = auth::hash_password(&password).context("hashing password")?;

    // Wrap the hash UPDATE and the session-sweep in one SQLite
    // transaction. SQLite auto-commits per statement otherwise, and a
    // failure between `set_password_hash` and `delete_user_sessions`
    // would leave the account in a half-rotated state: the new
    // password is live, but any session cookie still in an attacker's
    // possession keeps working. The whole point of running this CLI
    // is to recover from a compromise, so that window is exactly what
    // we cannot allow.
    let updated = db.with_conn(|conn| {
        let user = db::user_by_username(conn, &username)?
            .ok_or_else(|| anyhow!("no user named '{}' in {}", username, cfg.budget.db_path))?;
        let tx = conn
            .unchecked_transaction()
            .context("BEGIN transaction for password reset")?;
        db::set_password_hash(&tx, user.id, &hash)?;
        db::delete_user_sessions(&tx, user.id)?;
        tx.commit().context("COMMIT password-reset transaction")?;
        Ok(user.id)
    })?;

    println!(
        "password updated for '{}' (id={}); existing sessions invalidated",
        username, updated
    );
    Ok(())
}

fn cmd_list_users() -> Result<()> {
    let cfg = nanoguard::config::Config::from_env_or_default()?;
    let db = db::ConsoleDb::open(&cfg.budget.db_path)?;
    let users = db.with_conn(db::list_users)?;

    if users.is_empty() {
        println!("(no users in {})", cfg.budget.db_path);
        return Ok(());
    }

    println!(
        "{:<4}  {:<24}  {:<8}  {:<10}  {:<24}  last_login_at",
        "id", "username", "role", "disabled", "created_at"
    );
    for u in users {
        println!(
            "{:<4}  {:<24}  {:<8}  {:<10}  {:<24}  {}",
            u.id,
            truncate(&u.username, 24),
            u.role,
            u.disabled,
            u.created_at,
            u.last_login_at.unwrap_or_else(|| "—".to_string()),
        );
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn read_password_from_stdin() -> Result<Zeroizing<String>> {
    let stdin = io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .context("reading password from stdin")?;
    // Strip a single trailing newline so `echo foo | …` works as expected.
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Zeroizing::new(line))
}

/// Prompt for a password on the controlling tty with echo turned off.
/// Errors out if stdin is not a tty so the caller has to pick an
/// explicit pipe-friendly mode (`--password-stdin` or `--password`).
fn prompt_password_tty(username: &str) -> Result<Zeroizing<String>> {
    if !io::stdin().is_terminal() {
        return Err(anyhow!(
            "stdin is not a TTY; pass --password-stdin or --password instead"
        ));
    }

    // Prompt goes to stderr so a caller that redirected stdout (to
    // capture the "password updated" success line in scripts, say)
    // still sees the prompt instead of staring at a hung-looking
    // process waiting for input.
    eprint!("New password for '{}': ", username);
    io::stderr().flush().ok();

    // RAII: take a snapshot of termios, mask ECHO off, restore on drop.
    // Stays in-process — no fork/exec of `stty`. CLAUDE.md absolute rule
    // #2 ("single binary, no runtime dependencies beyond the binary
    // itself") would otherwise be at risk; `libc` is already vendored
    // for `libc::kill` in the console reload trigger, so this adds no
    // new dependency.
    let _echo_off = EchoGuard::off()?;

    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading password from tty")?;
    println!(); // newline after the suppressed echo

    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Zeroizing::new(line))
}

/// RAII guard that masks `ECHO` off on stdin's termios and restores the
/// previous setting on drop. Operates on `STDIN_FILENO` directly via
/// `tcgetattr`/`tcsetattr`. Caller is expected to have already verified
/// stdin is a tty.
struct EchoGuard {
    saved: libc::termios,
}

impl EchoGuard {
    fn off() -> Result<Self> {
        // SAFETY: zero-initialized `termios` is a valid input for
        // `tcgetattr`, which fills every field we care about. The call
        // returns -1 on error, never partial init.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("tcgetattr(stdin)");
        }
        let mut new = saved;
        new.c_lflag &= !libc::ECHO;
        // SAFETY: `new` is a fully-initialized termios derived from the
        // value tcgetattr just produced; modifying c_lflag is the
        // documented way to mask off echo.
        let rc = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &new) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("tcsetattr(stdin, ECHO off)");
        }
        Ok(EchoGuard { saved })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        // Best-effort restore. If this somehow fails the user's terminal
        // will be stuck without echo until they `stty echo` it back —
        // unfortunate but the only honest thing without a signal handler.
        // SAFETY: `self.saved` came from a successful `tcgetattr` call,
        // so it is a valid termios value to write back.
        unsafe {
            let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}
