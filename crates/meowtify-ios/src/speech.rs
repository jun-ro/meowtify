//! Speech to text for the search box: the iPhone microphone in, words out.
//!
//! The app calls [`dictate`] when the mic button is pressed. Permission is asked once, then
//! every partial and final transcription arrives through the callback. While listening the
//! session is "PlayAndRecord" so the music can keep playing; when listening ends the session
//! goes back to "Playback" so the music survives the app going to the background.
//!
//! Everything here is `unsafe` Objective-C through the `objc2` crates. Nothing touches the UI:
//! the callback is the only way out, and it may run on any thread.
//!
//! Cargo.toml additions:
//! ```toml
//! objc2-speech = { version = "0.3", default-features = false, features = [
//!     "std", "SFSpeechRecognizer", "SFSpeechRecognitionRequest",
//!     "SFSpeechRecognitionResult", "SFSpeechRecognitionTask", "SFTranscription",
//!     "block2", "objc2-avf-audio",
//! ] }
//! ```
//! These two existing lines get extra features; nothing else on them changes:
//! ```toml
//! objc2-foundation = { version = "0.3", features = ["NSString", "NSDictionary", "NSNotification", "NSValue", "NSURL", "NSObject", "NSOperation", "NSArray", "NSError", "block2"] }
//! objc2-avf-audio = { version = "0.3", features = ["AVAudioSession", "AVAudioSessionTypes", "AVAudioApplication", "AVAudioBuffer", "AVAudioEngine", "AVAudioFormat", "AVAudioIONode", "AVAudioNode", "AVAudioTime", "AVAudioTypes", "block2"] }
//! ```
//!
//! Info.plist keys, both required or the app is killed when it asks:
//! ```text
//! NSMicrophoneUsageDescription = Meowtify listens through the microphone only while you
//!     dictate a search, and stops as soon as you are done.
//! NSSpeechRecognitionUsageDescription = Meowtify turns your dictated search into text. Audio
//!     stays on this iPhone when it can be recognized there, otherwise it goes to Apple's
//!     speech service.
//! ```

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_avf_audio::{
    AVAudioApplication, AVAudioApplicationRecordPermission, AVAudioEngine, AVAudioPCMBuffer, AVAudioSession,
    AVAudioSessionCategoryOptions, AVAudioSessionCategoryPlayAndRecord, AVAudioSessionCategoryPlayback, AVAudioTime,
};
use objc2_foundation::NSError;
use objc2_speech::{
    SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask, SFSpeechRecognizer,
    SFSpeechRecognizerAuthorizationStatus,
};

/// What the app gives us to write the words into.
type Text = Arc<dyn Fn(String, bool) + Send + Sync>;
/// The audio tap. Kept alive while the tap is installed.
type TapBlock = RcBlock<dyn Fn(NonNull<AVAudioPCMBuffer>, NonNull<AVAudioTime>)>;
/// The recognition handler. Kept alive while the task runs.
type ResultBlock = RcBlock<dyn Fn(*mut SFSpeechRecognitionResult, *mut NSError)>;

/// The one running dictation. `None` when nothing listens.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// A dictation waiting for permission. `None` when no prompt is in flight.
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// Counts `dictate` and `stop_dictation` calls. A setup that finishes after a newer call is stale.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Ids sessions so a late cleanup cannot take a newer session's place.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Asks for permission once, then listens. Use it on the main thread: iOS shows the permission
/// prompts there. Results arrive on the main queue, except the closing call, which can come
/// from a helper thread, so `on_text` must be ready for any thread.
///
/// `on_text` gets the words heard so far and whether they are final. It always ends with a
/// final call, also when permission is refused, the engine fails or someone stops listening.
pub fn dictate(on_text: impl Fn(String, bool) + Send + Sync + 'static) {
    let on_text: Text = Arc::new(on_text);
    // One at a time.
    stop_dictation();

    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    *PENDING.lock().unwrap() = Some(Pending { on_text, generation, speech: None, mic: None });

    unsafe {
        match SFSpeechRecognizer::authorizationStatus() {
            SFSpeechRecognizerAuthorizationStatus::NotDetermined => {
                // The handler runs on a queue the system picks.
                let block = RcBlock::new(|status: SFSpeechRecognizerAuthorizationStatus| {
                    speech_allowed(status == SFSpeechRecognizerAuthorizationStatus::Authorized);
                });
                SFSpeechRecognizer::requestAuthorization(&block);
            }
            status => speech_allowed(status == SFSpeechRecognizerAuthorizationStatus::Authorized),
        }

        let app = AVAudioApplication::sharedInstance();
        // AVAudioApplication is the iOS 17 way; older systems are not targeted.
        if app.recordPermission() == AVAudioApplicationRecordPermission::Undetermined {
            let block = RcBlock::new(|granted: Bool| mic_allowed(granted.as_bool()));
            AVAudioApplication::requestRecordPermissionWithCompletionHandler(&block);
        } else {
            mic_allowed(app.recordPermission() == AVAudioApplicationRecordPermission::Granted);
        }
    }

    advance();
}

/// Stops listening and gives the microphone and the audio session back. Safe to call when
/// nothing listens: it then does nothing.
pub fn stop_dictation() {
    // A prompt still in flight must not start listening after the user stopped.
    *PENDING.lock().unwrap() = None;
    GENERATION.fetch_add(1, Ordering::SeqCst);
    close(None, true);
}

struct Pending {
    on_text: Text,
    generation: u64,
    /// `None` while the question is still open.
    speech: Option<bool>,
    mic: Option<bool>,
}

struct Session {
    id: u64,
    engine: Retained<AVAudioEngine>,
    /// `None` once the end of the audio was sent.
    request: Arc<Mutex<Option<Retained<SFSpeechAudioBufferRecognitionRequest>>>>,
    task: Retained<SFSpeechRecognitionTask>,
    /// The task holds it, but keeping it here is one reference less to worry about.
    _recognizer: Retained<SFSpeechRecognizer>,
    /// The last text, so a stop or an error can still hand it over.
    heard: Arc<Mutex<String>>,
    /// True once a final text was delivered, so nobody delivers a second one.
    announced: Arc<AtomicBool>,
    /// False once the session is on its way out. The audio tap reads it.
    alive: Arc<AtomicBool>,
    /// One reference each, in case a framework does not copy what it was handed.
    _tap: TapBlock,
    _handler: ResultBlock,
    on_text: Text,
}

// The objects are Objective-C singletons and thread safe. The `Mutex` above does the rest.
unsafe impl Send for Session {}

fn speech_allowed(allowed: bool) {
    if let Some(pending) = PENDING.lock().unwrap().as_mut() {
        pending.speech = Some(allowed);
    }
    advance();
}

fn mic_allowed(allowed: bool) {
    if let Some(pending) = PENDING.lock().unwrap().as_mut() {
        pending.mic = Some(allowed);
    }
    advance();
}

/// Starts listening once both answers are in. The first caller takes the pending job.
fn advance() {
    let ready = {
        let pending = PENDING.lock().unwrap();
        pending.as_ref().is_some_and(|p| p.speech.is_some() && p.mic.is_some())
    };
    if !ready {
        return;
    }
    let Some(pending) = PENDING.lock().unwrap().take() else { return };

    if pending.speech != Some(true) || pending.mic != Some(true) {
        eprintln!("meowtify-ios: speech or microphone access was refused");
        // Final with no text: the UI leaves its listening state.
        (pending.on_text)(String::new(), true);
        return;
    }
    start(pending.generation, pending.on_text);
}

fn start(generation: u64, on_text: Text) {
    unsafe {
        let recognizer = SFSpeechRecognizer::new();
        if !recognizer.isAvailable() {
            eprintln!("meowtify-ios: speech recognition is not available now");
            (on_text)(String::new(), true);
            return;
        }

        record_session();

        let request = SFSpeechAudioBufferRecognitionRequest::new();
        request.setShouldReportPartialResults(true);
        // No network and no audio leaves the phone when the phone can do it alone.
        request.setRequiresOnDeviceRecognition(recognizer.supportsOnDeviceRecognition());

        let engine = AVAudioEngine::new();
        let input = engine.inputNode();
        let format = input.outputFormatForBus(0);

        let request = Arc::new(Mutex::new(Some(request)));
        let heard = Arc::new(Mutex::new(String::new()));
        let announced = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);

        let tap_request = Arc::clone(&request);
        let tap_alive = Arc::clone(&alive);
        let tap = RcBlock::new(move |buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
            if !tap_alive.load(Ordering::SeqCst) {
                return;
            }
            // The lock also keeps a buffer from landing after `endAudio`.
            if let Some(request) = tap_request.lock().unwrap().as_ref() {
                request.appendAudioPCMBuffer(buffer.as_ref());
            }
        });
        input.installTapOnBus_bufferSize_format_block(0, 1024, Some(&format), RcBlock::as_ptr(&tap));
        engine.prepare();

        let handler_heard = Arc::clone(&heard);
        let handler_announced = Arc::clone(&announced);
        let handler_alive = Arc::clone(&alive);
        let handler_text = Arc::clone(&on_text);
        let handler = RcBlock::new(move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
            let mut ended = false;
            // Both pointers are nullable and owned by the framework.
            if let Some(result) = result.as_ref() {
                let text = result.bestTranscription().formattedString().to_string();
                let is_final = result.isFinal();
                *handler_heard.lock().unwrap() = text.clone();
                // A final after a stop or an error is yesterday's news.
                if !is_final || !handler_announced.swap(true, Ordering::SeqCst) {
                    handler_text(text, is_final);
                }
                ended = is_final;
            }
            if let Some(error) = error.as_ref() {
                eprintln!("meowtify-ios: speech recognition: {error:?}");
                ended = true;
            }
            if ended {
                handler_alive.store(false, Ordering::SeqCst);
                // The handler can run inside the task; tear down from another thread so the
                // task can finish its callback first.
                std::thread::spawn(move || close(Some(id), true));
            }
        });

        let task = {
            let request = request.lock().unwrap();
            recognizer.recognitionTaskWithRequest_resultHandler(request.as_ref().unwrap(), &handler)
        };

        let session = Session {
            id,
            engine,
            request,
            task,
            _recognizer: recognizer,
            heard,
            announced,
            alive,
            _tap: tap,
            _handler: handler,
            on_text,
        };

        // A newer dictate or a stop came in while the engine was being set up.
        if GENERATION.load(Ordering::SeqCst) != generation {
            clean(session, false);
            return;
        }

        // Hold the slot while the engine starts: a cleanup waits instead of racing it.
        let mut slot = SESSION.lock().unwrap();
        *slot = Some(session);
        let Some(session) = slot.as_ref() else { return };
        if !session.alive.load(Ordering::SeqCst) {
            // The task failed before the microphone opened.
            drop(slot);
            close(Some(id), true);
            return;
        }
        if let Err(e) = session.engine.startAndReturnError() {
            eprintln!("meowtify-ios: audio engine: {e:?}");
            drop(slot);
            close(Some(id), true);
        }
    }
}

/// Takes the session out when it is ours, then tears it down outside the lock.
fn close(id: Option<u64>, report: bool) {
    let session = {
        let mut slot = SESSION.lock().unwrap();
        let take = match slot.as_ref() {
            Some(session) => id.is_none() || id == Some(session.id),
            None => false,
        };
        if take { slot.take() } else { None }
    };
    if let Some(session) = session {
        clean(session, report);
    }
}

fn clean(session: Session, report: bool) {
    session.alive.store(false, Ordering::SeqCst);
    unsafe {
        session.engine.inputNode().removeTapOnBus(0);
        session.engine.stop();
    }
    let request = session.request.lock().unwrap().take();
    if let Some(request) = request {
        // The engine is already stopped, so no more buffers arrive.
        unsafe { request.endAudio() };
    }
    unsafe { session.task.cancel() };
    playback();

    if report && !session.announced.swap(true, Ordering::SeqCst) {
        let text = session.heard.lock().unwrap().clone();
        (session.on_text)(text, true);
    }
}

/// Music keeps playing through the silent switch with this category, in the background too.
fn record_session() {
    unsafe {
        let session = AVAudioSession::sharedInstance();
        if let Some(category) = AVAudioSessionCategoryPlayAndRecord {
            let options = AVAudioSessionCategoryOptions::DefaultToSpeaker
                | AVAudioSessionCategoryOptions::AllowBluetoothHFP
                | AVAudioSessionCategoryOptions::MixWithOthers;
            // Not every route allows every option. A plain record session still works.
            if let Err(e) = session.setCategory_withOptions_error(category, options) {
                eprintln!("meowtify-ios: record category options: {e:?}");
                let _ = session.setCategory_error(category);
            }
        }
        if let Err(e) = session.setActive_error(true) {
            eprintln!("meowtify-ios: audio session start: {e:?}");
        }
    }
}

/// Back to Playback: the microphone is free and the music plays on.
fn playback() {
    unsafe {
        let session = AVAudioSession::sharedInstance();
        if let Some(category) = AVAudioSessionCategoryPlayback {
            if let Err(e) = session.setCategory_error(category) {
                eprintln!("meowtify-ios: playback category: {e:?}");
            }
        }
    }
}
