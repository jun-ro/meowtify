//! Sign in to YouTube in a small WebView2 window, hidden inside the app's own program.
//!
//! This is a hidden mode of meowtify.exe, not a second program: `meowtify.exe --login
//! <cookies.txt> [--anonymous-test]` runs before the runtime, settings or the single-instance
//! check, and exits with its own code. The auto-updater therefore only ever replaces
//! meowtify.exe and nothing has to stay in step with it.
//!
//! It opens the Google sign-in flow, waits for a SAPISID / __Secure-3PAPISID cookie, and
//! writes a Netscape cookies.txt that yt-dlp consumes with `--cookies`. The profile is a
//! private folder in %TEMP%, navigation is allowlisted, no JavaScript is injected, and the
//! cookies come from the native WebView2 cookie API.
//!
//! SAFETY: this helper never types, requests, reads or stores credentials. It only reads
//! cookie NAMES/DOMAINS, and only those are printed, never values. The user performs the
//! sign-in. Values reach the output file and nowhere else.
//!
//! Exit codes: 0 = wrote cookies, 2 = window closed before cookies were available,
//! 64 = bad arguments, 1 = an error.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::cookie::Expiration;
use wry::{NewWindowResponse, WebContext, WebView, WebViewBuilder};

const SIGN_IN_URL: &str =
    "https://accounts.google.com/ServiceLogin?service=youtube&continue=https://www.youtube.com/";
const ANONYMOUS_URL: &str = "https://www.youtube.com/";
const ANONYMOUS_WAIT: Duration = Duration::from_secs(8);

/// Hosts the embedded browser may navigate to. Everything else is blocked.
const ALLOWED_HOST_SUFFIXES: &[&str] = &[
    "google.com",
    "youtube.com",
    "gstatic.com",
    "googleusercontent.com",
    "ggpht.com",
    // YouTube page assets; needed for the post-login youtube.com landing page to render.
    "ytimg.com",
];

enum UserEvent {
    Tick,
}

#[derive(Debug)]
struct ExportedCookie {
    domain: String,
    path: String,
    secure: bool,
    expires: i64,
    name: String,
    value: String,
}

/// Writes the cookies to `out`. `anonymous` skips the sign-in for a test visit that only
/// checks the mode starts and exports something without any account.
pub fn run(out: Option<&Path>, anonymous: bool) -> i32 {
    let Some(out) = out else {
        eprintln!("usage: meowtify.exe --login <cookies.txt> [--anonymous-test]");
        return 64;
    };
    let cookies_path = absolute(out);

    println!("mode: {}", if anonymous { "anonymous-test" } else { "sign-in" });
    println!("cookies out: {}", cookies_path.display());

    // Private, per-run WebView2 profile in %TEMP%; removed on exit.
    let profile_dir = std::env::temp_dir().join(format!("meowtify-login-{}", std::process::id()));
    if let Err(e) = fs::create_dir_all(&profile_dir) {
        eprintln!("failed to create temp profile {}: {e}", profile_dir.display());
        return 1;
    }
    println!("temp profile: {}", profile_dir.display());

    let exit_code = run_window(anonymous, &profile_dir, &cookies_path);

    // WebView2 releases its locks asynchronously after the controller is gone.
    remove_profile(&profile_dir, 10);

    exit_code
}

fn run_window(anonymous: bool, profile_dir: &Path, cookies_path: &Path) -> i32 {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    let window = match WindowBuilder::new()
        .with_title(if anonymous { "Meowtify (anonymous test)" } else { "Sign in to YouTube" })
        .with_inner_size(tao::dpi::LogicalSize::new(480.0, 700.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(360.0, 480.0))
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("failed to create window: {e}");
            return 1;
        }
    };

    let mut web_context = WebContext::new(Some(profile_dir.to_path_buf()));
    let initial_url = if anonymous { ANONYMOUS_URL } else { SIGN_IN_URL };

    let webview = match WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url(initial_url)
        .with_navigation_handler(|url| {
            let allowed = host_allowed(&url);
            if !allowed {
                println!("blocked navigation to non-allowlisted host");
            }
            allowed
        })
        // Keep everything inside the single window (no popups from sign-in flows).
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .build(&window)
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("failed to create webview: {e}");
            return 1;
        }
    };

    let result_code = Arc::new(AtomicI32::new(2)); // default: user closed the window
    let stop_timer = Arc::new(AtomicBool::new(false));

    spawn_ticker(&event_loop, stop_timer.clone());

    let started = Instant::now();

    {
        let result_code = result_code.clone();
        event_loop.run_return(move |event, _target, control_flow| {
                *control_flow = ControlFlow::Wait;

                match event {
                    Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                        *control_flow = ControlFlow::Exit;
                    }
                    Event::UserEvent(UserEvent::Tick) => {
                        // Anonymous mode: no sign-in, just let youtube.com set its guest cookies.
                        if anonymous && started.elapsed() >= ANONYMOUS_WAIT {
                            match export_cookies(&webview, cookies_path) {
                                Ok(cookies) => {
                                    report(&cookies, "anonymous-test");
                                    result_code.store(0, Ordering::SeqCst);
                                }
                                Err(e) => {
                                    eprintln!("cookie export failed: {e}");
                                    result_code.store(1, Ordering::SeqCst);
                                }
                            }
                            *control_flow = ControlFlow::Exit;
                            return;
                        }

                        // Sign-in mode: wait for an account-bound cookie.
                        let cookies = match collect_cookies(&webview) {
                            Ok(c) => c,
                            Err(e) => {
                                eprintln!("cookie read failed: {e}");
                                return;
                            }
                        };

                        let signed_in = cookies.iter().any(|c| {
                            (c.name == "SAPISID" || c.name == "__Secure-3PAPISID")
                                && (c.domain.ends_with("youtube.com") || c.domain.ends_with("google.com"))
                        });

                        if signed_in {
                            report(&cookies, "signed-in");
                            match write_netscape(cookies_path, &cookies) {
                                Ok(()) => {
                                    println!("signed-in, wrote {} cookies", cookies.len());
                                    result_code.store(0, Ordering::SeqCst);
                                }
                                Err(e) => {
                                    eprintln!("failed to write {}: {e}", cookies_path.display());
                                    result_code.store(1, Ordering::SeqCst);
                                }
                            }
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                    _ => {}
                }
            });
    }

    stop_timer.store(true, Ordering::SeqCst);
    // The event-loop closure (and the webview it owns) is dropped when run_return ends.
    drop(web_context);
    drop(window);

    // 0 = cookies written, 2 = closed without cookies, 1 = error.
    result_code.load(Ordering::SeqCst)
}

/// Poll once per second from the event loop (cookie APIs are not thread-safe off-thread).
fn spawn_ticker(event_loop: &tao::event_loop::EventLoop<UserEvent>, stop: Arc<AtomicBool>) {
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(1));
            if proxy.send_event(UserEvent::Tick).is_err() {
                break;
            }
        }
    });
}

fn export_cookies(webview: &WebView, out: &Path) -> Result<Vec<ExportedCookie>, String> {
    let cookies = collect_cookies(webview)?;
    write_netscape(out, &cookies)?;
    Ok(cookies)
}

fn collect_cookies(webview: &WebView) -> Result<Vec<ExportedCookie>, String> {
    let all = webview.cookies().map_err(|e| e.to_string())?;

    // wry may not populate Cookie::domain() on every platform; use per-URL queries as a
    // fallback to attribute each cookie to youtube.com vs google.com.
    let for_youtube = names_for_url(webview, "https://www.youtube.com/");
    let for_google = names_for_url(webview, "https://accounts.google.com/");

    let mut out = Vec::new();
    for c in all {
        let raw_domain = c.domain().unwrap_or("").trim().to_ascii_lowercase();
        let domain = if raw_domain.is_empty() {
            if for_youtube.contains(c.name()) {
                ".youtube.com".to_string()
            } else if for_google.contains(c.name()) {
                ".google.com".to_string()
            } else {
                // Unknown origin: never export it.
                continue;
            }
        } else {
            // Outside the two domains of interest is never exported.
            let Some(domain) = cookie_domain(&raw_domain) else { continue };
            domain.to_string()
        };

        let expires = match c.expires() {
            Some(Expiration::DateTime(dt)) => dt.unix_timestamp(),
            _ => 0,
        };

        out.push(ExportedCookie {
            domain,
            path: c.path().unwrap_or("/").to_string(),
            secure: c.secure().unwrap_or(true),
            expires,
            name: c.name().to_string(),
            value: c.value().to_string(),
        });
    }

    Ok(out)
}

/// The Netscape domain a cookie belongs to, or `None` when it is not a YouTube/Google
/// cookie. Only these two are ever written: a sign-in page can set other cookies too.
fn cookie_domain(raw: &str) -> Option<&'static str> {
    let host = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    if host == "youtube.com" || host.ends_with(".youtube.com") {
        Some(".youtube.com")
    } else if host == "google.com" || host.ends_with(".google.com") {
        Some(".google.com")
    } else {
        None
    }
}

fn names_for_url(webview: &WebView, url: &str) -> std::collections::HashSet<String> {
    webview
        .cookies_for_url(url)
        .map(|v| v.into_iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default()
}

/// Netscape cookies.txt. Only names/domains are ever logged; values stay in the file.
fn write_netscape(path: &Path, cookies: &[ExportedCookie]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }

    let mut text = String::new();
    text.push_str("# Netscape HTTP Cookie File\n");
    text.push_str("# Generated by meowtify.exe --login (WebView2 native cookie export)\n");
    text.push_str("# https://curl.se/docs/http-cookies.html\n\n");

    for c in cookies {
        text.push_str(&c.domain);
        text.push('\t');
        text.push_str(if c.domain.starts_with('.') { "TRUE" } else { "FALSE" });
        text.push('\t');
        text.push_str(&c.path);
        text.push('\t');
        text.push_str(if c.secure { "TRUE" } else { "FALSE" });
        text.push('\t');
        text.push_str(&c.expires.to_string());
        text.push('\t');
        text.push_str(&c.name);
        text.push('\t');
        text.push_str(&c.value);
        text.push('\n');
    }

    fs::write(path, text).map_err(|e| e.to_string())
}

/// Logs cookie names + domains only. Never prints values.
fn report(cookies: &[ExportedCookie], what: &str) {
    for c in cookies {
        println!("cookie: {} domain={}", c.name, c.domain);
    }
    println!("{what}: {} cookies available", cookies.len());
}

fn host_allowed(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };

    match parsed.scheme() {
        "about" | "data" | "blob" => return true,
        "https" | "http" => {}
        _ => return false,
    }

    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();

    ALLOWED_HOST_SUFFIXES.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf())
    }
}

fn remove_profile(dir: &Path, attempts: u32) {
    for i in 0..attempts {
        match fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(e) => {
                if i + 1 == attempts {
                    eprintln!(
                        "warning: could not delete temp profile {} ({e}); WebView2 may still hold a lock",
                        dir.display()
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str, domain: &str) -> ExportedCookie {
        ExportedCookie {
            domain: domain.to_string(),
            path: "/".to_string(),
            secure: true,
            expires: 1_700_000_000,
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn only_youtube_and_google_cookies_are_kept() {
        assert_eq!(cookie_domain(".youtube.com"), Some(".youtube.com"));
        assert_eq!(cookie_domain("youtube.com"), Some(".youtube.com"));
        assert_eq!(cookie_domain("music.youtube.com"), Some(".youtube.com"));
        assert_eq!(cookie_domain(".google.com"), Some(".google.com"));
        assert_eq!(cookie_domain("accounts.google.com"), Some(".google.com"));
        for other in ["evil.com", "youtube.com.evil.com", "notgoogle.com", "", "example.org"] {
            assert_eq!(cookie_domain(other), None, "{other:?}");
        }
    }

    #[test]
    fn the_netscape_file_holds_the_usual_columns() {
        let dir = std::env::temp_dir().join(format!("meowtify-login-test-{}", std::process::id()));
        let path = dir.join("cookies.txt");
        let cookies = [
            cookie("SAPISID", "value-one", ".youtube.com"),
            cookie("__Secure-3PAPISID", "value-two", ".google.com"),
        ];
        write_netscape(&path, &cookies).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let rows: Vec<&str> = text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ".youtube.com\tTRUE\t/\tTRUE\t1700000000\tSAPISID\tvalue-one");
        assert_eq!(rows[1], ".google.com\tTRUE\t/\tTRUE\t1700000000\t__Secure-3PAPISID\tvalue-two");
        assert!(text.starts_with("# Netscape HTTP Cookie File\n"), "yt-dlp expects this header");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_list_still_writes_a_usable_header() {
        let dir = std::env::temp_dir().join(format!("meowtify-login-empty-{}", std::process::id()));
        let path = dir.join("cookies.txt");
        write_netscape(&path, &[]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Netscape HTTP Cookie File\n"));
        assert!(!text.lines().any(|l| !l.starts_with('#') && !l.is_empty()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_allowlisted_hosts_may_be_visited() {
        for ok in [
            "https://accounts.google.com/ServiceLogin",
            "https://www.youtube.com/",
            "https://m.youtube.com/watch?v=x",
            "https://i.ytimg.com/x.png",
            "about:blank",
        ] {
            assert!(host_allowed(ok), "should allow {ok}");
        }
        for bad in ["https://evil.com/", "https://youtube.com.evil.com/", "file:///C:/x", "javascript:alert(1)"] {
            assert!(!host_allowed(bad), "should block {bad}");
        }
    }
}
