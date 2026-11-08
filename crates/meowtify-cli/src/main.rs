//! `meowtify-cli`: a small tool for when the app's window does not work. It updates the app,
//! shows where its files are, and resets its settings or caches. It never starts the window and
//! shares no code with the UI, so a broken UI cannot break it.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use meowtify_library::config_dir;
use meowtify_provider::app_update::{self, Program};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_FILE: &str = "meowtify.exe";
/// The app listens here while it runs (`crates/meowtify/src/deeplink.rs`).
const APP_PORT: u16 = 47625;

const HELP: &str = "\
meowtify-cli: repair tool for Meowtify

Usage: meowtify-cli <command> [options]

Commands:
  update          Install the latest release of the app and of this tool
    --check         Only look, change nothing
    --force         Install the latest release even when it is not newer
    --app <path>    The app to replace (default: meowtify.exe beside this tool)
    --no-cli        Leave this tool as it is
  doctor          Check the files and settings the app needs
  paths           Print the folders and files the app uses
  reset-settings  Move config.json aside as config.json.bak, so the app starts with defaults
    --force         Do it even when the app seems to be running
  clear-cache     Delete downloaded thumbnails, lyrics and autoplay files
  version         Print the version
  help            Print this text

Exit code: 0 done, 1 failed, 2 wrong usage.";

fn main() -> ExitCode {
    app_update::clean_up();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("update") => update(rest),
        Some("doctor") => doctor(),
        Some("paths") => paths(),
        Some("reset-settings") => reset_settings(rest),
        Some("clear-cache") => clear_cache(),
        Some("version" | "--version" | "-V") => {
            println!("meowtify-cli {VERSION}");
            ExitCode::SUCCESS
        }
        Some("help" | "--help" | "-h") | None => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(other) => usage(&format!("unknown command {other:?}")),
    }
}

fn usage(why: &str) -> ExitCode {
    eprintln!("{why}\n\n{HELP}");
    ExitCode::from(2)
}

fn fail(why: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {why}");
    ExitCode::FAILURE
}

/// `true` when the app answers on its local port.
fn app_running() -> bool {
    TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, APP_PORT)), Duration::from_millis(300)).is_ok()
}

/// Folders that only hold downloaded copies. Playlists, settings, playlist covers and tools are
/// not caches and are never listed here.
fn cache_dirs() -> Vec<PathBuf> {
    vec![config_dir().join("cache"), config_dir().join("lyrics"), std::env::temp_dir().join("meowtify-jam")]
}

/// Where the app should be: beside this tool.
fn sibling_app() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.with_file_name(APP_FILE))
}

fn paths() -> ExitCode {
    let cfg = config_dir();
    println!("config folder  {}", cfg.display());
    println!("settings       {}", cfg.join("config.json").display());
    println!("playlists      {}", cfg.join("playlists.json").display());
    println!("tools          {}", cfg.join("tools").display());
    for dir in cache_dirs() {
        println!("cache          {}", dir.display());
    }
    match sibling_app() {
        Some(p) => println!("app            {}{}", p.display(), if p.exists() { "" } else { " (not found)" }),
        None => println!("app            unknown"),
    }
    ExitCode::SUCCESS
}

fn doctor() -> ExitCode {
    let mut problems = 0;
    let mut line = |ok: bool, text: String| {
        println!("[{}] {text}", if ok { "ok" } else { "!!" });
        if !ok {
            problems += 1;
        }
    };
    line(true, format!("meowtify-cli {VERSION}"));

    let cfg = config_dir();
    line(cfg.is_dir(), format!("config folder {}", cfg.display()));

    match std::fs::read(cfg.join("config.json")) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(_) => line(true, format!("config.json is valid ({} bytes)", bytes.len())),
            Err(e) => line(false, format!("config.json is damaged ({e}). The app ignores it; `reset-settings` moves it aside")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => line(true, "config.json is missing (defaults are used)".into()),
        Err(e) => line(false, format!("config.json cannot be read: {e}")),
    }

    match sibling_app() {
        Some(p) => line(p.is_file(), format!("app {}", p.display())),
        None => line(false, "cannot tell where this tool runs from".into()),
    }

    let running = app_running();
    println!("[--] app is {}", if running { "running" } else { "not running" });

    if problems == 0 {
        println!("\nNo problems found.");
        ExitCode::SUCCESS
    } else {
        println!("\n{problems} problem(s) found.");
        ExitCode::FAILURE
    }
}

fn reset_settings(args: &[String]) -> ExitCode {
    let force = args.iter().any(|a| a == "--force");
    if let Some(bad) = args.iter().find(|a| *a != "--force") {
        return usage(&format!("unknown option {bad:?}"));
    }
    if app_running() && !force {
        return fail("the app seems to be running and would write its settings back. Close it, or use --force");
    }
    let from = config_dir().join("config.json");
    let to = config_dir().join("config.json.bak");
    if !from.exists() {
        println!("Nothing to reset: {} does not exist.", from.display());
        return ExitCode::SUCCESS;
    }
    let _ = std::fs::remove_file(&to);
    match std::fs::rename(&from, &to) {
        Ok(()) => {
            println!("Moved settings to {}. The app starts with defaults next time.", to.display());
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("{}: {e}", from.display())),
    }
}

fn clear_cache() -> ExitCode {
    let mut failed = false;
    for dir in cache_dirs() {
        if !dir.exists() {
            continue;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => println!("Deleted {}", dir.display()),
            Err(e) => {
                eprintln!("error: {}: {e}", dir.display());
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn update(args: &[String]) -> ExitCode {
    let (mut check_only, mut force, mut with_cli) = (false, false, true);
    let mut app: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--check" => check_only = true,
            "--force" => force = true,
            "--no-cli" => with_cli = false,
            "--app" => match it.next() {
                Some(p) => app = Some(PathBuf::from(p)),
                None => return usage("--app needs a path"),
            },
            other => return usage(&format!("unknown option {other:?}")),
        }
    }
    let Some(app) = app.or_else(sibling_app) else {
        return fail("cannot tell where the app is. Use --app <path>");
    };

    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return fail(e),
    };
    rt.block_on(async {
        let release = match app_update::check_with(VERSION, force).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                println!("Already on the latest release ({VERSION}). Use --force to install it again.");
                return ExitCode::SUCCESS;
            }
            Err(e) => return fail(format!("could not check: {e}")),
        };
        println!("Release {} is available.", release.version);
        if !release.notes.trim().is_empty() {
            println!("\n{}\n", release.notes.trim());
        }
        if check_only {
            return ExitCode::SUCCESS;
        }
        if app_running() {
            println!("Note: the app is running. The new version starts the next time it is opened.");
        }

        println!("Installing the app to {} ...", app.display());
        if let Err(e) = app_update::install_program(&release, Program::App, &app).await {
            return fail(format!("app update failed: {e}"));
        }
        println!("App updated to {}.", release.version);

        if with_cli {
            let me = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            println!("Installing this tool ...");
            if let Err(e) = app_update::install_program(&release, Program::Cli, &me).await {
                return fail(format!("tool update failed (the app was updated): {e}"));
            }
            println!("Tool updated to {}.", release.version);
        }
        ExitCode::SUCCESS
    })
}
