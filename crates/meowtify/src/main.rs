// Release builds on Windows are GUI programs: no extra console window.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod deeplink;
/// Windows only: `--login` is the sign-in window, a hidden mode of this same program.
#[cfg(windows)]
mod login;
#[cfg(any(debug_assertions, feature = "dev"))]
mod mock;

use std::sync::Arc;

use std::time::Duration;

use meowtify_core::{run, AppHandle, Command, MetadataSource, PlaybackEngine};
use meowtify_library::{config_dir, Library, Settings};
use meowtify_player::LocalPlayer;
use meowtify_provider::{Audius, DownloadManager, SoundCloud, Tools, YouTube, YtDlp};
use meowtify_autoplay::{Autoplay, Control};
use meowtify_discord::Presence;
use meowtify_ui::Services;

/// The in-memory catalogue behind `--mock`. Debug builds and builds with the `dev` feature only.
#[cfg(any(debug_assertions, feature = "dev"))]
fn mock_services() -> (Arc<dyn PlaybackEngine>, Arc<dyn MetadataSource>) {
    let tracks = mock::tracks(10_000);
    (Arc::new(mock::MockEngine::new(tracks.clone())), Arc::new(mock::MockSource::new(tracks)))
}

#[cfg(not(any(debug_assertions, feature = "dev")))]
fn mock_services() -> (Arc<dyn PlaybackEngine>, Arc<dyn MetadataSource>) {
    unreachable!("--mock is ignored in this build")
}

fn main() {
    // Windows: `--login <cookies.txt> [--anonymous-test]` is the sign-in helper, a hidden mode
    // of this same program so the updater only ever replaces meowtify.exe. It runs before the
    // runtime, settings and the single-instance check: its window is its own process.
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("--login") {
        let out = std::env::args().nth(2).map(std::path::PathBuf::from);
        let anonymous = std::env::args().any(|a| a == "--anonymous-test");
        std::process::exit(login::run(out.as_deref(), anonymous));
    }

    let mock = cfg!(any(debug_assertions, feature = "dev")) && std::env::args().any(|a| a == "--mock");

    // A `meowtify://jam/…` link is how a guest joins. When another copy is running, it gets the
    // link and this one exits. The link is held until the window is up, and the user decides.
    let (links_tx, links_rx) = tokio::sync::mpsc::unbounded_channel();
    let link = std::env::args().skip(1).find(|a| a.starts_with("meowtify://"));
    if !mock {
        if let deeplink::Instance::Forwarded = deeplink::start(link.as_deref(), links_tx.clone()) {
            return;
        }
        deeplink::register();
    }
    if let Some(link) = link {
        let _ = links_tx.send(link);
    }

    // The blocking pool defaults to 512 threads. Disk, tag and cache work needs a handful, and
    // every thread that exists costs a stack.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(16)
        .build()
        .expect("tokio runtime");

    // The settings are read once. The window starts from this copy and saves its own edits.
    let settings = Settings::load();
    let (engine, source, library): (Arc<dyn PlaybackEngine>, Arc<dyn MetadataSource>, Option<Arc<Library>>) =
        if mock {
            let (engine, source) = mock_services();
            (engine, source, None)
        } else {
            let library = Arc::new(Library::new(settings.library_dir.clone()).with_playlist_file(config_dir().join("playlists.json")));
            (Arc::new(LocalPlayer::new(library.clone())), library.clone(), Some(library))
        };

    // The proxy applies to yt-dlp, curl and the streaming download from the first request.
    if let Err(e) = meowtify_provider::set_proxy(&settings.proxy) {
        eprintln!("ignoring the saved proxy: {e}");
    }
    // Reading cookies out of the user's browser is no longer offered: it fails while Chrome-based
    // browsers run, and nobody should have to choose a browser. The in-app sign-in replaces it, so
    // an old saved choice (`settings.cookies_browser`) is ignored and yt-dlp runs without it.
    let _ = meowtify_provider::set_cookies_browser("");
    // A computer has no cellular link, so this never makes a download small here by itself;
    // the setting is read anyway so a config copied to a phone keeps working.
    meowtify_provider::set_bitrate_mode(meowtify_provider::BitrateMode::from_str(&settings.effective_bitrate_mode()));

    let rt_handle = rt.handle().clone();
    let (handle, commands) = AppHandle::new(32, 64);
    let events = handle.subscribe();
    let sender = handle.event_sender();
    // The current-thread runtime is driven from a helper thread so the main
    // thread stays free for the UI. The actor stops once the UI drops every handle.
    let driver = std::thread::spawn(move || rt.block_on(run(engine, commands, sender)));

    // yt-dlp is found at startup or installed later from Settings, so downloads
    // are always set up. A missing yt-dlp makes a download fail with a clear message.
    let mut yt_dlp = None;
    let mut autoplay_providers: Option<(Arc<dyn meowtify_provider::Provider>, Vec<Arc<dyn meowtify_provider::Provider>>)> = None;
    let downloads = if mock {
        None
    } else {
        let dir = config_dir().join("tools");
        let tools = Tools::locate_or_default(&dir);
        let yt = Arc::new(YtDlp::new(tools.yt_dlp.clone(), dir));
        yt_dlp = Some(yt.clone());
        // YouTube: the in-process client first (no program to start, no sign-in, about a second
        // a song, and it gets past the "not a bot" check that stops yt-dlp), yt-dlp behind it.
        let yt_dlp_youtube: Arc<dyn meowtify_provider::Provider> = Arc::new(YouTube::with_yt_dlp(tools.clone(), yt.clone()));
        let native_youtube: Arc<dyn meowtify_provider::Provider> = Arc::new(meowtify_provider::NativeYouTube::with_tagging(true));
        let provider: Arc<dyn meowtify_provider::Provider> = Arc::new(meowtify_provider::Chain::new(native_youtube, yt_dlp_youtube));
        // Other sources for songs YouTube does not have: SoundCloud (through yt-dlp) and Audius.
        let soundcloud: Arc<dyn meowtify_provider::Provider> = Arc::new(SoundCloud::new(YouTube::with_yt_dlp(tools, yt)));
        autoplay_providers = Some((provider.clone(), vec![soundcloud, Arc::new(Audius)]));
        Some(DownloadManager::start(provider, &rt_handle, meowtify_ui::downloads_notifier()))
    };

    // Local playback moves on its own (progress, next track), so ask for its state
    // every second. The task holds a handle, so it must be told to stop on exit or
    // the actor would never finish.
    let stop = Arc::new(tokio::sync::Notify::new());
    if !mock {
        let poll = handle.clone();
        let stop = stop.clone();
        rt_handle.spawn(async move {
            // Err means the second passed; Ok means the stop signal arrived.
            while tokio::time::timeout(Duration::from_secs(1), stop.notified()).await.is_err() {
                if poll.send(Command::Refresh).await.is_err() {
                    return;
                }
            }
        });
    }

    // Discord presence follows the same events as the window. It sends nothing
    // until the user switches it on in Settings.
    let discord: Option<Arc<dyn Fn(bool) + Send + Sync>> = if mock {
        None
    } else {
        let presence = Presence::start(&rt_handle, handle.subscribe(), settings.discord);
        Some(Arc::new(move |on| presence.set_enabled(on)))
    };

    // Autoplay queues similar songs when the queue runs out. It needs the library (to make
    // its cache files playable) and a provider (to download them), so mock mode has none.
    let control = Control::new(settings.autoplay, settings.autoplay_ahead, settings.autoplay_behind, &settings.lastfm_key);
    let autoplay = match (&library, autoplay_providers) {
        (Some(library), Some((provider, fallbacks))) => {
            Some(Autoplay::start(&rt_handle, handle.clone(), library.clone(), provider, fallbacks, control.clone()))
        }
        _ => None,
    };
    let autoplay_control = autoplay.as_ref().map(|a| a.control());

    let result = meowtify_ui::run(handle, Services { source, library, downloads, yt_dlp, discord, autoplay: autoplay_control, open_links: Some(links_rx), dictation: None, folder_picker: None, cover_picker: None, native_menu: None, settings }, events, rt_handle);
    stop.notify_one();
    if let Some(a) = &autoplay {
        a.stop();
    }

    // Closing the window ends the program. The core stops once every handle is dropped, but a
    // helper that still holds one (autoplay, a Jam link) must not keep the process alive with no
    // window. Give the core a moment to finish, so a running download is stopped cleanly, then exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !driver.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Err(e) = result {
        eprintln!("ui error: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}
