//! In-memory engine for `--mock`. No network, no audio.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use meowtify_core::{
    Album, Artist, Error, MetadataSource, Page, PlaybackEngine, PlaybackState, Place, Playlist, Queue,
    Repeat, Result, Selection, Track,
};

const TITLES: [&str; 7] = ["Midnight Drive", "夜に駆ける", "봄날", "月亮代表我的心", "حبيبي", "שלום", "🎵🐱"];

pub fn tracks(n: usize) -> Arc<[Track]> {
    let artists: Vec<Artist> = (0..500)
        .map(|i| Artist { id: format!("ar{i}").into(), name: format!("Artist {i}").into() })
        .collect();
    let albums: Vec<Arc<Album>> = (0..1000)
        .map(|i| {
            Arc::new(Album {
                id: format!("al{i}").into(),
                name: format!("Album {i}").into(),
                artists: Arc::from(vec![artists[i % 500].clone()]),
            })
        })
        .collect();
    (0..n)
        .map(|i| {
            let k = i / TITLES.len();
            let base = TITLES[i % TITLES.len()];
            let name = if k == 0 { base.to_string() } else { format!("{base} {k}") };
            Track {
                id: format!("t{i}").into(),
                uri: format!("spotify:track:t{i}").into(),
                name: name.into(),
                artists: Arc::from(vec![artists[i % 500].clone()]),
                album: Some(albums[i % 1000].clone()),
                duration_ms: (90 + ((i * 37) % 330) as u32) * 1000,
            }
        })
        .collect::<Vec<_>>()
        .into()
}

struct State {
    all: Arc<[Track]>,
    current: Option<usize>,
    playing: bool,
    progress_ms: u32,
    volume: u8,
}

pub struct MockEngine {
    state: Mutex<State>,
}

impl MockEngine {
    pub fn new(all: Arc<[Track]>) -> Self {
        MockEngine { state: Mutex::new(State { all, current: None, playing: false, progress_ms: 0, volume: 50 }) }
    }
}

#[async_trait]
impl PlaybackEngine for MockEngine {
    async fn play(&self) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        if s.current.is_none() {
            s.current = Some(0);
        }
        s.playing = true;
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.state.lock().unwrap().playing = false;
        Ok(())
    }

    async fn next(&self) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        let n = s.all.len();
        s.current = Some(s.current.map_or(0, |c| (c + 1) % n));
        s.progress_ms = 0;
        s.playing = true;
        Ok(())
    }

    async fn previous(&self) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        let n = s.all.len();
        s.current = Some(s.current.map_or(0, |c| (c + n - 1) % n));
        s.progress_ms = 0;
        s.playing = true;
        Ok(())
    }

    async fn seek(&self, position_ms: u32) -> Result<()> {
        self.state.lock().unwrap().progress_ms = position_ms;
        Ok(())
    }

    async fn set_volume(&self, percent: u8) -> Result<()> {
        self.state.lock().unwrap().volume = percent.min(100);
        Ok(())
    }

    async fn enqueue(&self, _uri: &str, _place: Place) -> Result<()> {
        Ok(())
    }

    async fn jump(&self, _uri: &str) -> Result<()> {
        Err(Error::Unsupported)
    }

    async fn play_from(&self, selection: &Selection) -> Result<()> {
        let (Selection::Library(uri) | Selection::Single(uri)) = selection else { return Err(Error::Unsupported) };
        let mut s = self.state.lock().unwrap();
        let idx = s.all.iter().position(|t| t.uri == *uri).ok_or(Error::NotFound)?;
        s.current = Some(idx);
        s.progress_ms = 0;
        s.playing = true;
        Ok(())
    }

    async fn playback_state(&self) -> Result<Option<PlaybackState>> {
        let s = self.state.lock().unwrap();
        Ok(s.current.map(|i| PlaybackState {
            is_playing: s.playing,
            progress_ms: s.progress_ms,
            track: Some(s.all[i].clone()),
            volume: Some(s.volume),
            shuffle: false,
            repeat: Repeat::Off,
            ended: false,
        }))
    }

    async fn queue(&self) -> Result<Queue> {
        Ok(Queue::default())
    }
}

/// In-memory catalogue for `--mock`: liked songs, three playlists, substring search.
pub struct MockSource {
    all: Arc<[Track]>,
}

impl MockSource {
    pub fn new(all: Arc<[Track]>) -> Self {
        MockSource { all }
    }

    fn page(items: &[Track], offset: u32, limit: u32) -> Page<Track> {
        let start = (offset as usize).min(items.len());
        let end = (start + limit as usize).min(items.len());
        Page {
            items: items[start..end].to_vec(),
            offset,
            limit,
            total: items.len() as u32,
            has_next: end < items.len(),
        }
    }

    /// Playlist `n` holds every third track starting at `n`.
    fn playlist_tracks(&self, id: &str) -> Vec<Track> {
        let n: usize = id.strip_prefix("pl").and_then(|s| s.parse().ok()).unwrap_or(0);
        self.all.iter().skip(n).step_by(3).cloned().collect()
    }
}

#[async_trait]
impl MetadataSource for MockSource {
    async fn user_playlists(&self, offset: u32, limit: u32) -> Result<Page<Playlist>> {
        let all: Vec<Playlist> = (0..3)
            .map(|i| Playlist {
                id: format!("pl{i}").into(),
                name: format!("Mock playlist {i}").into(),
            })
            .collect();
        let start = (offset as usize).min(all.len());
        let end = (start + limit as usize).min(all.len());
        Ok(Page { items: all[start..end].to_vec(), offset, limit, total: 3, has_next: end < all.len() })
    }

    async fn playlist_items(&self, id: &str, offset: u32, limit: u32) -> Result<Page<Track>> {
        Ok(Self::page(&self.playlist_tracks(id), offset, limit))
    }

    async fn saved_tracks(&self, offset: u32, limit: u32) -> Result<Page<Track>> {
        Ok(Self::page(&self.all, offset, limit))
    }
}
