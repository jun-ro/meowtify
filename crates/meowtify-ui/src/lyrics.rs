//! The lyrics panel. It asks for lyrics only while it is open, so a closed panel sends nothing
//! over the network. Lines with times follow the song: all of them go to the window once, and
//! only the index of the playing line changes, so the window can scroll smoothly between lines.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use meowtify_core::{PlaybackState, Track};
use meowtify_provider::{LyricLine, Lyrics, LyricsQuery};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{LyricRow, MainWindow};

/// Rows sent to the window at most.
const MAX_ROWS: usize = 400;
/// The position reaches the window about once a second and a little late, so a line is shown
/// slightly before its time.
const LEAD_MS: u32 = 600;

#[derive(PartialEq)]
enum Status {
    /// Nothing asked yet for this song.
    Idle,
    Loading,
    /// The answer is in, whether or not there were lyrics.
    Done,
    /// The network failed. Opening the panel again tries again.
    Failed,
}

struct State {
    uri: Option<Arc<str>>,
    track: Option<Track>,
    pos_ms: u32,
    lyrics: Option<Lyrics>,
    status: Status,
    /// Counts questions, so an answer for a song that is gone is dropped.
    generation: u64,
    /// Set once the rows of this song are in the window, with the playing line last sent.
    shown: Option<Option<usize>>,
    rows: Rc<VecModel<LyricRow>>,
    win: slint::Weak<MainWindow>,
    rt: tokio::runtime::Handle,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

pub(crate) fn install(win: &MainWindow, rt: tokio::runtime::Handle) {
    let rows = Rc::new(VecModel::<LyricRow>::default());
    win.set_lyrics_rows(ModelRc::from(rows.clone()));
    STATE.with(|s| {
        *s.borrow_mut() = Some(State { uri: None, track: None, pos_ms: 0, lyrics: None, status: Status::Idle, generation: 0, shown: None, rows, win: win.as_weak(), rt });
    });
    let weak = win.as_weak();
    win.on_toggle_lyrics(move || {
        if let Some(w) = weak.upgrade() {
            let open = !w.get_lyrics_open();
            w.set_lyrics_open(open);
            if open {
                refresh(&w);
            }
        }
    });
}

/// The player moved on: a new song, or the clock of the same one.
pub(crate) fn on_playback(w: &MainWindow, state: Option<&PlaybackState>) {
    let track = state.and_then(|s| s.track.as_ref());
    let changed = with(|st| {
        let same = match (&st.uri, track) {
            (Some(u), Some(t)) => **u == *t.uri,
            (None, None) => true,
            _ => false,
        };
        st.pos_ms = state.map_or(0, |s| s.progress_ms);
        if !same {
            st.uri = track.map(|t| t.uri.clone());
            st.track = track.cloned();
            st.lyrics = None;
            st.status = Status::Idle;
            st.generation += 1; // an answer for the old song is no longer wanted
            st.shown = None;
            st.rows.set_vec(Vec::new());
        }
        !same
    })
    .unwrap_or(false);
    if changed {
        w.set_lyrics_note(slint::SharedString::default());
    }
    if w.get_lyrics_open() {
        refresh(w);
    }
}

/// Asks for the lyrics when they are not here yet, otherwise shows the right lines.
fn refresh(w: &MainWindow) {
    let ask = with(|st| {
        if st.track.is_none() {
            return None;
        }
        matches!(st.status, Status::Idle | Status::Failed).then(|| {
            st.status = Status::Loading;
            st.generation += 1;
            (st.generation, st.track.clone().unwrap(), st.rt.clone())
        })
    })
    .flatten();
    match ask {
        Some((generation, track, rt)) => {
            w.set_lyrics_note("Looking for lyrics\u{2026}".into());
            let weak = w.as_weak();
            rt.spawn(async move {
                let artist = track.artists.first().map(|a| a.name.to_string()).unwrap_or_default();
                let album = track.album.as_ref().map(|a| a.name.to_string());
                let file = std::path::PathBuf::from(track.uri.as_ref());
                let query = LyricsQuery {
                    title: &track.name,
                    artist: &artist,
                    album: album.as_deref(),
                    duration_secs: track.duration_ms / 1000,
                    audio: file.is_absolute().then_some(file.as_path()),
                };
                let cache = meowtify_library::config_dir().join("lyrics");
                let answer = meowtify_provider::find_lyrics(&query, &cache).await.map_err(|e| e.to_string());
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        deliver(&w, generation, answer);
                    }
                });
            });
        }
        None => {
            if with(|st| st.track.is_none()).unwrap_or(true) {
                w.set_lyrics_note("Nothing is playing.".into());
            } else {
                show();
            }
        }
    }
}

fn deliver(w: &MainWindow, generation: u64, answer: Result<Option<Lyrics>, String>) {
    let note = with(|st| {
        if st.generation != generation {
            return None; // another song since
        }
        let note = match answer {
            Ok(Some(Lyrics::Synced(lines))) => {
                st.lyrics = Some(Lyrics::Synced(lines));
                ""
            }
            Ok(Some(Lyrics::Plain(lines))) => {
                st.lyrics = Some(Lyrics::Plain(lines));
                "These lyrics have no times, so they do not follow the song."
            }
            Ok(Some(Lyrics::Instrumental)) => {
                st.lyrics = Some(Lyrics::Instrumental);
                "Instrumental: this song has no words."
            }
            Ok(None) => {
                st.lyrics = None;
                "No lyrics found. Put a .lrc file next to the song to add your own."
            }
            Err(e) => {
                st.status = Status::Failed;
                st.lyrics = None;
                return Some(format!("Could not get lyrics: {e}"));
            }
        };
        st.status = Status::Done;
        st.shown = None;
        Some(note.to_string())
    })
    .flatten();
    if let Some(note) = note {
        w.set_lyrics_note(note.into());
        if w.get_lyrics_open() {
            show();
        }
    }
}

/// The index of the playing line: the last one that started.
fn playing(lines: &[LyricLine], pos_ms: u32) -> Option<usize> {
    let at = pos_ms.saturating_add(LEAD_MS);
    lines.partition_point(|l| l.at_ms <= at).checked_sub(1)
}

fn row(text: &str, kind: i32) -> LyricRow {
    LyricRow { text: if text.is_empty() { "\u{266a}".into() } else { text.into() }, kind }
}

fn show() {
    with(|st| {
        let Some(w) = st.win.upgrade() else { return };
        match &st.lyrics {
            Some(Lyrics::Synced(lines)) => {
                let now = playing(lines, st.pos_ms);
                if st.shown.is_none() {
                    st.rows.set_vec(lines.iter().take(MAX_ROWS).map(|l| row(&l.text, 2)).collect::<Vec<_>>());
                    w.set_lyrics_synced(true);
                }
                if st.shown != Some(now) {
                    w.set_lyrics_current(now.map_or(-1, |n| n as i32));
                    st.shown = Some(now);
                }
            }
            Some(Lyrics::Plain(lines)) => {
                if st.shown.is_none() {
                    st.shown = Some(None);
                    w.set_lyrics_synced(false);
                    st.rows.set_vec(lines.iter().take(MAX_ROWS).map(|l| row(l, 2)).collect::<Vec<_>>());
                }
            }
            _ => {
                if st.shown.is_none() && st.rows.row_count() > 0 {
                    st.rows.set_vec(Vec::new());
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(times: &[u32]) -> Vec<LyricLine> {
        times.iter().map(|&at_ms| LyricLine { at_ms, text: String::new() }).collect()
    }

    #[test]
    fn the_playing_line_is_the_last_one_that_started() {
        let l = lines(&[1_000, 5_000, 9_000]);
        assert_eq!(playing(&l, 0), None, "before the first line");
        assert_eq!(playing(&l, 500), Some(0), "the lead shows a line slightly early");
        assert_eq!(playing(&l, 4_000), Some(0));
        assert_eq!(playing(&l, 4_500), Some(1));
        assert_eq!(playing(&l, 600_000), Some(2));
        assert_eq!(playing(&[], 5_000), None);
    }
}
