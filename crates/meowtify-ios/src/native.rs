//! Talks to iOS itself: the audio session, the lock screen and the car stereo, and links
//! opened from outside (a Siri Shortcut opens `meowtify://play?q=<title>`).
//!
//! Everything here is `unsafe` Objective-C calls through the `objc2` crates. The player,
//! the queue and the UI know nothing about it: this module only sends the same `Command`s
//! the buttons send and reads the same `Event`s the UI reads.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use block2::RcBlock;
use meowtify_core::{AppHandle, Command, Event, PlaybackState};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_avf_audio::{AVAudioSession, AVAudioSessionCategoryPlayback, AVAudioSessionInterruptionNotification};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSData, NSDictionary, NSNotification, NSNotificationCenter, NSNumber, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork,
    MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use objc2_ui_kit::{UIImage, UIApplication, UIApplicationDelegate, UIApplicationDidFinishLaunchingNotification};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

/// True while a song plays. The lock screen's play/pause button is one toggle, so the
/// handler needs to know which way to flip.
static PLAYING: AtomicBool = AtomicBool::new(false);
/// Where opened `meowtify://` links go.
static LINKS: OnceLock<UnboundedSender<String>> = OnceLock::new();
/// Where the UI sends the cover of the song that plays, for the lock screen.
static ART: OnceLock<UnboundedSender<Option<Vec<u8>>>> = OnceLock::new();

thread_local! {
    /// The lock screen's cover. Kept so a new position or a pause, which rewrites the whole
    /// dictionary, carries it again without building the artwork again. Only the now-playing
    /// task touches it, on the thread its runtime runs on.
    static ARTWORK: RefCell<Option<Retained<MPMediaItemArtwork>>> = const { RefCell::new(None) };
}

/// The cover of the song on the lock screen, as the bytes the cover cache fetched. iOS decodes
/// them itself, so nothing here keeps a pixel copy. `None` takes the cover away. Called from the
/// UI thread; the now-playing task builds the artwork.
pub fn set_now_playing_artwork(bytes: Option<&[u8]>) {
    if let Some(art) = ART.get() {
        let _ = art.send(bytes.map(|b| b.to_vec()));
    }
}

/// Sets up the lock screen, interruptions and links. Call it on the main thread, before the UI
/// starts. `audio_session` goes even earlier, before anything can make a sound.
pub fn init(handle: AppHandle, rt: Handle, links: UnboundedSender<String>) {
    let _ = LINKS.set(links);
    interruptions(handle.clone(), rt.clone());
    remote_commands(handle.clone(), rt.clone());
    install_url_delegate();
    link_monitor();
    let (art, art_rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = ART.set(art);
    publish_task(handle, rt, art_rx);
}

/// Tells the provider whether the phone is on a cellular link, so the data-saving mode can
/// take the low bitrate copy while it is, and replace it once Wi-Fi is back.
///
/// `NWPathMonitor` from Network.framework, called through `objc2` like everything else here.
/// `NWInterfaceType` is 1 for Wi-Fi and 2 for cellular; `NWPathStatus` is 1 when satisfied.
fn link_monitor() {
    const WIFI: isize = 1;
    const CELLULAR: isize = 2;
    const SATISFIED: isize = 1;

    #[link(name = "Network", kind = "framework")]
    extern "C" {}

    extern "C" {
        fn dispatch_get_main_queue() -> *mut AnyObject;
    }

    unsafe {
        let Some(class) = AnyClass::get(c"NWPathMonitor") else {
            eprintln!("meowtify-ios: Network.framework did not load; cellular is treated as unmetered");
            return;
        };
        let monitor: Retained<AnyObject> = msg_send![class, new];
        let block = RcBlock::new(move |path: NonNull<AnyObject>| {
            let path = path.as_ref();
            let status: isize = msg_send![path, status];
            let uses = |kind: isize| {
                let yes: objc2::runtime::Bool = msg_send![path, usesInterfaceType: kind];
                yes.as_bool()
            };
            meowtify_provider::set_metered(status == SATISFIED && uses(CELLULAR) && !uses(WIFI));
        });
        let _: () = msg_send![&*monitor, setPathUpdateHandler: &*block];
        let _: () = msg_send![&*monitor, startWithQueue: dispatch_get_main_queue()];
        // The monitor holds the block and reports until the app ends.
        std::mem::forget(block);
        std::mem::forget(monitor);
    }
}

/// "Playback" keeps the sound going with the screen locked and the app in the background, and
/// plays through the speaker while the silent switch is on. `UIBackgroundModes: audio` in
/// Info.plist allows it. The session turns on by itself when the output starts.
pub fn audio_session() {
    unsafe {
        let session = AVAudioSession::sharedInstance();
        if let Some(category) = AVAudioSessionCategoryPlayback {
            if let Err(e) = session.setCategory_error(category) {
                eprintln!("meowtify-ios: audio session category: {e:?}");
            }
        }
    }
}

/// A call, an alarm or Siri takes the speaker: pause. When iOS says it is over and playback
/// may resume, turn the session back on and play.
fn interruptions(handle: AppHandle, rt: Handle) {
    let Some(name) = (unsafe { AVAudioSessionInterruptionNotification }) else { return };
    let block = RcBlock::new(move |note: NonNull<NSNotification>| {
        let note = unsafe { note.as_ref() };
        let Some(info) = note.userInfo() else { return };
        let number = |key: &str| -> Option<usize> {
            let key = NSString::from_str(key);
            let value = info.objectForKey(&*key)?;
            let number = value.downcast::<NSNumber>().ok()?;
            Some(number.unsignedIntegerValue())
        };
        // AVAudioSessionInterruptionType: 1 began, 0 ended. Option 1: should resume.
        match number("AVAudioSessionInterruptionTypeKey") {
            Some(1) => send(&rt, &handle, Command::Pause),
            Some(0) => {
                if number("AVAudioSessionInterruptionOptionKey").is_some_and(|o| o & 1 == 1) {
                    unsafe {
                        let _ = AVAudioSession::sharedInstance().setActive_error(true);
                    }
                    send(&rt, &handle, Command::Play);
                }
            }
            _ => {}
        }
    });
    let observer = unsafe { NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) };
    // Observing lasts as long as the app does.
    std::mem::forget(observer);
}

fn send(rt: &Handle, handle: &AppHandle, command: Command) {
    let handle = handle.clone();
    rt.spawn(async move {
        let _ = handle.send(command).await;
    });
}

/// The lock screen, Control Center, headphone buttons and a car stereo all use these.
fn remote_commands(handle: AppHandle, rt: Handle) {
    unsafe {
        let center = MPRemoteCommandCenter::sharedCommandCenter();
        let add = |command: &MPRemoteCommand, f: Box<dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus>| {
            let block = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| f(event));
            let token = command.addTargetWithHandler(&block);
            std::mem::forget(token);
            command.setEnabled(true);
        };
        let ok = MPRemoteCommandHandlerStatus::Success;
        let (h, r) = (handle.clone(), rt.clone());
        add(&center.playCommand(), Box::new(move |_| {
            send(&r, &h, Command::Play);
            ok
        }));
        let (h, r) = (handle.clone(), rt.clone());
        add(&center.pauseCommand(), Box::new(move |_| {
            send(&r, &h, Command::Pause);
            ok
        }));
        let (h, r) = (handle.clone(), rt.clone());
        add(&center.togglePlayPauseCommand(), Box::new(move |_| {
            send(&r, &h, if PLAYING.load(Ordering::Relaxed) { Command::Pause } else { Command::Play });
            ok
        }));
        let (h, r) = (handle.clone(), rt.clone());
        add(&center.nextTrackCommand(), Box::new(move |_| {
            send(&r, &h, Command::Next);
            ok
        }));
        let (h, r) = (handle.clone(), rt.clone());
        add(&center.previousTrackCommand(), Box::new(move |_| {
            send(&r, &h, Command::Previous);
            ok
        }));
        let (h, r) = (handle, rt);
        let seek = center.changePlaybackPositionCommand();
        let block = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
            let event = event.cast::<MPChangePlaybackPositionCommandEvent>();
            let seconds = event.as_ref().positionTime();
            send(&r, &h, Command::Seek((seconds.max(0.0) * 1000.0) as u32));
            MPRemoteCommandHandlerStatus::Success
        });
        std::mem::forget(seek.addTargetWithHandler(&block));
        seek.setEnabled(true);
    }
}

/// The Now Playing card: title, artist, length, position, and whether it is moving. iOS counts the
/// position on its own while the rate is 1, so this only needs a fresh value when something
/// other than time passing changed.
fn publish_task(handle: AppHandle, rt: Handle, mut art: tokio::sync::mpsc::UnboundedReceiver<Option<Vec<u8>>>) {
    let mut events = handle.subscribe();
    rt.spawn(async move {
        let mut last: Option<(std::sync::Arc<str>, bool, u32, Instant)> = None;
        let mut state: Option<PlaybackState> = None;
        loop {
            tokio::select! {
                event = events.recv() => {
                    let new = match event {
                        Ok(Event::PlaybackChanged(state)) => state,
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    };
                    let playing = new.as_ref().is_some_and(|s| s.is_playing);
                    PLAYING.store(playing, Ordering::Relaxed);
                    let key = new.as_ref().and_then(|s| s.track.as_ref().map(|t| (t.uri.clone(), s.is_playing, s.progress_ms)));
                    let fresh = match (&last, &key) {
                        (_, None) => last.is_some(),
                        (None, Some(_)) => true,
                        (Some((uri, was_playing, progress, at)), Some((new_uri, now_playing, now_progress))) => {
                            let expected = *progress as i64 + if *was_playing { at.elapsed().as_millis() as i64 } else { 0 };
                            uri != new_uri || was_playing != now_playing || (expected - *now_progress as i64).abs() > 1500
                        }
                    };
                    state = new;
                    if !fresh {
                        continue;
                    }
                    last = key.map(|(uri, playing, progress)| (uri, playing, progress, Instant::now()));
                    // The calls are thread safe; the lock screen reads the dictionary on its own time.
                    set_now_playing(state.as_ref());
                }
                bytes = art.recv() => {
                    // The sender lives as long as the app does.
                    let Some(bytes) = bytes else { return };
                    set_artwork(bytes.as_deref());
                    set_now_playing(state.as_ref());
                }
            }
        }
    });
}

/// The lock screen's artwork for a cover. `MPMediaItemArtwork` is told how big it is and asked
/// for the image later; it keeps the block, and with it the image, so no copy is held here.
fn make_artwork(bytes: &[u8]) -> Option<Retained<MPMediaItemArtwork>> {
    unsafe {
        let data = NSData::with_bytes(bytes);
        let image = UIImage::initWithData(UIImage::alloc(), &data)?;
        let size = image.size();
        let block: RcBlock<dyn Fn(CGSize) -> NonNull<UIImage>> =
            RcBlock::new(move |_size: CGSize| -> NonNull<UIImage> { NonNull::from(&*image) });
        let artwork: Retained<MPMediaItemArtwork> =
            msg_send![MPMediaItemArtwork::alloc(), initWithBoundsSize: size, requestHandler: &*block];
        Some(artwork)
    }
}

fn set_artwork(bytes: Option<&[u8]>) {
    let art = bytes.and_then(make_artwork);
    ARTWORK.with(|slot| *slot.borrow_mut() = art);
}

fn set_now_playing(state: Option<&PlaybackState>) {
    let artwork = ARTWORK.with(|slot| slot.borrow().clone());
    unsafe {
        let center = MPNowPlayingInfoCenter::defaultCenter();
        let Some(track) = state.and_then(|s| s.track.as_ref().map(|t| (s, t))) else {
            center.setNowPlayingInfo(None);
            return;
        };
        let (state, track) = track;
        let artist = track.artists.iter().map(|a| a.name.as_ref()).collect::<Vec<_>>().join(", ");
        let title = NSString::from_str(&track.name);
        let artist = NSString::from_str(&artist);
        let duration = NSNumber::new_f64(f64::from(track.duration_ms) / 1000.0);
        let elapsed = NSNumber::new_f64(f64::from(state.progress_ms) / 1000.0);
        let rate = NSNumber::new_f64(if state.is_playing { 1.0 } else { 0.0 });
        let mut keys: Vec<&NSString> = vec![
            MPMediaItemPropertyTitle,
            MPMediaItemPropertyArtist,
            MPMediaItemPropertyPlaybackDuration,
            MPNowPlayingInfoPropertyElapsedPlaybackTime,
            MPNowPlayingInfoPropertyPlaybackRate,
        ];
        let mut values: Vec<Retained<AnyObject>> = vec![
            Retained::cast_unchecked(title),
            Retained::cast_unchecked(artist),
            Retained::cast_unchecked(duration),
            Retained::cast_unchecked(elapsed),
            Retained::cast_unchecked(rate),
        ];
        if let Some(artwork) = artwork {
            keys.push(MPMediaItemPropertyArtwork);
            values.push(Retained::cast_unchecked(artwork));
        }
        let info = NSDictionary::from_retained_objects(&keys, &values);
        center.setNowPlayingInfo(Some(&info));
    }
}

define_class!(
    /// winit starts the app without a delegate, so iOS has nobody to hand an opened link to.
    /// This one takes `meowtify://` links and passes them on.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MeowtifyLinkDelegate"]
    struct LinkDelegate;

    unsafe impl NSObjectProtocol for LinkDelegate {}

    unsafe impl UIApplicationDelegate for LinkDelegate {
        #[unsafe(method(application:openURL:options:))]
        fn open_url(&self, _app: &UIApplication, url: &NSURL, _options: &NSDictionary<NSString, AnyObject>) -> bool {
            let link = url.absoluteString().map(|s| s.to_string());
            match (link, LINKS.get()) {
                (Some(link), Some(tx)) => tx.send(link).is_ok(),
                _ => false,
            }
        }
    }
);

/// The app object exists only once `UIApplicationMain` runs, which is after `init`. Wait for the
/// launch notification, then give the app its delegate. The delegate lives for the whole run.
fn install_url_delegate() {
    let name = unsafe { UIApplicationDidFinishLaunchingNotification };
    let block = RcBlock::new(move |_: NonNull<NSNotification>| {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let delegate: Retained<LinkDelegate> = unsafe { msg_send![mtm.alloc::<LinkDelegate>(), init] };
        let app = UIApplication::sharedApplication(mtm);
        unsafe { app.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        // `delegate` is a weak reference on the app: keep this one alive.
        std::mem::forget(delegate);
    });
    let observer = unsafe { NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) };
    std::mem::forget(observer);
}
