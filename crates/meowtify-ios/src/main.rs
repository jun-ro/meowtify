//! iOS entry point.
//!
//! Phase 3: the shared app (core, library, player, UI) on a phone, with search and
//! downloads through the in-process providers. There is no yt-dlp, ffmpeg or curl on iOS,
//! so `meowtify_provider::native_providers` supplies YouTube, SoundCloud and Audius.
//! Left out on purpose: Discord, deep links, the single-instance socket, self-update.

#[cfg(target_os = "ios")]
mod app {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use meowtify_autoplay::{Autoplay, Control};
    use meowtify_core::{run, AppHandle, Command, MetadataSource, PlaybackEngine};
    use meowtify_library::{config_dir, Library, Settings};
    use meowtify_player::LocalPlayer;
    use meowtify_provider::DownloadManager;
    use meowtify_ui::Services;

    /// The app's Documents folder. Info.plist shares it with the Files app, so the user
    /// can copy music into it from there.
    fn documents_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Documents"))
    }

    pub fn main() {
        let renderer = if cfg!(feature = "software") { "software" } else { "skia" };
        if let Err(e) = slint::BackendSelector::new()
            .backend_name("winit".to_string())
            .renderer_name(renderer.to_string())
            .select()
        {
            eprintln!("meowtify-ios: no {renderer} renderer: {e}");
            return;
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(16)
            .build()
            .expect("tokio runtime");

        let settings = Settings::load();
        let music = settings.library_dir.clone().or_else(documents_dir);
        let _ = std::fs::create_dir_all(config_dir());
        if let Some(dir) = &music {
            let _ = std::fs::create_dir_all(dir);
        }
        let library = Arc::new(Library::new(music).with_playlist_file(config_dir().join("playlists.json")));
        let engine: Arc<dyn PlaybackEngine> = Arc::new(LocalPlayer::new(library.clone()));
        let source: Arc<dyn MetadataSource> = library.clone();

        // The saved proxy applies to the in-process client from the first request.
        if let Err(e) = meowtify_provider::set_proxy(&settings.proxy) {
            eprintln!("ignoring the saved proxy: {e}");
        }

        let rt_handle = rt.handle().clone();
        let (handle, commands) = AppHandle::new(32, 64);
        let events = handle.subscribe();
        let sender = handle.event_sender();
        std::thread::spawn(move || rt.block_on(run(engine, commands, sender)));

        // Local playback moves on its own (progress, next track), so ask for its state every second.
        let poll = handle.clone();
        rt_handle.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if poll.send(Command::Refresh).await.is_err() {
                    return;
                }
            }
        });

        // Search and downloads run in process; there is no yt-dlp to point Settings at.
        let (provider, fallbacks) = meowtify_provider::native_providers();
        let downloads = Some(DownloadManager::start(provider.clone(), &rt_handle, meowtify_ui::downloads_notifier()));

        let control = Control::new(settings.autoplay, settings.autoplay_ahead, settings.autoplay_behind, &settings.lastfm_key);
        let autoplay = Autoplay::start(&rt_handle, handle.clone(), library.clone(), provider, fallbacks, control);
        let autoplay_control = Some(autoplay.control());

        let services = Services {
            source,
            library: Some(library),
            downloads,
            yt_dlp: None,
            discord: None,
            autoplay: autoplay_control,
            open_links: None,
            settings,
        };
        let result = meowtify_ui::run(handle, services, events, rt_handle);
        autoplay.stop();
        if let Err(e) = result {
            eprintln!("ui error: {e}");
        }
    }
}

#[cfg(target_os = "ios")]
fn main() {
    app::main();
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("meowtify-ios only runs on iOS. Build it with --target aarch64-apple-ios.");
}
