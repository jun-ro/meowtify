//! Talks to iOS itself: the audio session, the lock screen and the car stereo, and links
//! opened from outside (a Siri Shortcut opens `meowtify://play?q=<title>`).
//!
//! Everything here is `unsafe` Objective-C calls through the `objc2` crates. The player,
//! the queue and the UI know nothing about it: this module only sends the same `Command`s
//! the buttons send and reads the same `Event`s the UI reads.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use block2::RcBlock;
use meowtify_core::{AppHandle, Command, Event, PlaybackState};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_avf_audio::{AVAudioSession, AVAudioSessionCategoryPlayback, AVAudioSessionInterruptionNotification};
use objc2_foundation::{NSDictionary, NSNotification, NSNotificationCenter, NSNumber, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemPropertyArtist, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle,
    MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate, MPRemoteCommand,
    MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use objc2_ui_kit::{UIApplication, UIApplicationDelegate, UIApplicationDidFinishLaunchingNotification};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

/// True while a song plays. The lock screen's play/pause button is one toggle, so the
/// handler needs to know which way to flip.
static PLAYING: AtomicBool = AtomicBool::new(false);
/// Where opened `meowtify://` links go.
static LINKS: OnceLock<UnboundedSender<String>> = OnceLock::new();

/// Sets up the lock screen, interruptions and links. Call it on the main thread, before the UI
/// starts. `audio_session` goes even earlier, before anything can make a sound.
pub fn init(handle: AppHandle, rt: Handle, links: UnboundedSender<String>) {
    let _ = LINKS.set(links);
    interruptions(handle.clone(), rt.clone());
    remote_commands(handle.clone(), rt.clone());
    install_url_delegate();
    publish_task(handle, rt);
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
fn publish_task(handle: AppHandle, rt: Handle) {
    let mut events = handle.subscribe();
    rt.spawn(async move {
        let mut last: Option<(std::sync::Arc<str>, bool, u32, Instant)> = None;
        loop {
            let state = match events.recv().await {
                Ok(Event::PlaybackChanged(state)) => state,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            let playing = state.as_ref().is_some_and(|s| s.is_playing);
            PLAYING.store(playing, Ordering::Relaxed);
            let key = state.as_ref().and_then(|s| s.track.as_ref().map(|t| (t.uri.clone(), s.is_playing, s.progress_ms)));
            let fresh = match (&last, &key) {
                (_, None) => last.is_some(),
                (None, Some(_)) => true,
                (Some((uri, was_playing, progress, at)), Some((new_uri, now_playing, now_progress))) => {
                    let expected = *progress as i64 + if *was_playing { at.elapsed().as_millis() as i64 } else { 0 };
                    uri != new_uri || was_playing != now_playing || (expected - *now_progress as i64).abs() > 1500
                }
            };
            if !fresh {
                continue;
            }
            last = key.map(|(uri, playing, progress)| (uri, playing, progress, Instant::now()));
            // The calls are thread safe; the lock screen reads the dictionary on its own time.
            set_now_playing(state.as_ref());
        }
    });
}

fn set_now_playing(state: Option<&PlaybackState>) {
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
        let keys: [&NSString; 5] = [
            MPMediaItemPropertyTitle,
            MPMediaItemPropertyArtist,
            MPMediaItemPropertyPlaybackDuration,
            MPNowPlayingInfoPropertyElapsedPlaybackTime,
            MPNowPlayingInfoPropertyPlaybackRate,
        ];
        let values: [Retained<AnyObject>; 5] = [
            Retained::cast_unchecked(title),
            Retained::cast_unchecked(artist),
            Retained::cast_unchecked(duration),
            Retained::cast_unchecked(elapsed),
            Retained::cast_unchecked(rate),
        ];
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
