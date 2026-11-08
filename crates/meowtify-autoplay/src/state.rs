//! What autoplay remembers, and when it decides to look for more songs. No network and no
//! clock reads here (the time is passed in), so the decisions are tested directly.
//!
//! It also turns what the listener does into learning: a song heard to the end is a good
//! sign, a song skipped within seconds is a bad one, a click on Keep or Dislike counts more.
//! Those signals go to [`Taste`], and two quick skips in a row throw the waiting autoplay
//! songs away so the next ones follow the song that plays now.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use meowtify_core::{PlaybackState, Queue, Repeat, Track};
use meowtify_provider::{main_artist, song_key, Candidate, Seed};

use crate::rank::Ranked;
use crate::taste::{Features, Node, Taste};

/// A search starts when the queue holds fewer than `ahead - REFILL_GAP` songs, so every
/// search brings several songs at once instead of one search per skip.
const REFILL_GAP: usize = 2;
/// Playing a song for less than this counts as "not this one".
const SKIP_MS: u32 = 15_000;
/// A pool of suggestions is used for this long before the service is asked again.
const POOL_LIFE: Duration = Duration::from_secs(30 * 60);
/// Wait after a search that found nothing (no network, a service is down).
const BACKOFF: Duration = Duration::from_secs(30);
/// Wait after songs were added, so the queue event that follows does not start another search.
const SETTLE: Duration = Duration::from_secs(2);
const HISTORY: usize = 300;
/// Artists remembered as disliked in one session. The oldest are forgotten first.
const MAX_DISLIKED: usize = 200;
/// The player reports at most this many upcoming songs. A shorter list is the whole queue.
const VISIBLE_QUEUE: usize = 50;
/// A cache file that nothing needs is deleted when it is older than this. New files are
/// spared because a download may be about to queue them.
pub const ORPHAN_AGE: Duration = Duration::from_secs(120);
/// Cache files are looked over at least this often.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// A song autoplay put in the queue.
#[derive(Debug, Clone)]
pub struct Added {
    pub key: String,
    pub artist: String,
    pub uri: String,
    /// The numbers the scorer saw, kept to learn from what the listener does with the song.
    pub feat: Features,
}

/// One search for songs, with everything it needs copied in.
#[derive(Debug, Clone)]
pub struct FillJob {
    pub seed: Seed,
    pub seed_key: String,
    pub need: usize,
    pub exclude: HashSet<String>,
    pub recent_artists: Vec<String>,
    pub disliked: HashSet<String>,
    /// Ranked suggestions kept from an earlier search for the same seed.
    pub pool: Option<Vec<Ranked>>,
    /// Songs this listener played after the seed before (a source of suggestions).
    pub history: Vec<Candidate>,
    pub taste: Taste,
}

#[derive(Debug, Clone)]
pub struct FillDone {
    pub seed_key: String,
    pub pool: Vec<Ranked>,
    pub added: Vec<Added>,
}

#[derive(Debug, Clone)]
struct Pick {
    feat: Features,
}

/// The song that plays (or played), as the graph knows it.
#[derive(Debug, Clone)]
struct Current {
    key: String,
    artist: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Good,
    Bad,
    Meh,
}

/// How the listener took a song, from how long it played.
fn verdict(progress_ms: u32, secs: u32) -> Verdict {
    if secs >= 45 && progress_ms < SKIP_MS {
        Verdict::Bad
    } else if (secs > 0 && u64::from(progress_ms) * 10 >= u64::from(secs) * 1000 * 6) || (secs == 0 && progress_ms >= 60_000) {
        Verdict::Good
    } else {
        Verdict::Meh
    }
}

pub struct State {
    /// Songs to keep downloaded ahead of the playing one.
    ahead: usize,
    /// Played autoplay songs to keep for Previous.
    behind: usize,
    /// Autoplay songs that played and are still kept, oldest first.
    played: VecDeque<String>,
    sweep_dirty: bool,
    last_sweep: Option<Instant>,
    repeat: Repeat,
    /// The song that plays now, and what is known about it.
    playing: Option<Arc<str>>,
    last_progress: u32,
    last_secs: u32,
    /// The queue ran out and the player stopped. Stays true until a song plays again.
    ended: bool,
    /// Counts the changes that make a running search obsolete: a song that was not in the
    /// queue starts, repeat changes, autoplay is switched off.
    generation: u64,
    upcoming: Vec<Arc<str>>,
    /// Every path that was seen waiting in the queue. A song that starts without being in
    /// here was chosen by the listener, which replaces the queue.
    queued_seen: HashSet<String>,
    /// Paths of songs autoplay chose, to tell its songs from the user's.
    autoplay_uris: HashSet<String>,
    /// Autoplay songs that were queued and have not played yet. Their files are kept even
    /// when they sit beyond the part of the queue the player reports.
    pending: HashSet<String>,
    picks: HashMap<String, Pick>,
    /// Songs rated with a button, which are not also learned from by listening.
    rated: HashSet<String>,
    history: VecDeque<String>,
    artists: VecDeque<String>,
    disliked: HashSet<String>,
    /// The same artists, oldest first, to keep `disliked` within [`MAX_DISLIKED`].
    disliked_order: VecDeque<String>,
    pool: Option<(String, Vec<Ranked>, Instant)>,
    cool_until: Option<Instant>,
    taste: Taste,
    cur: Option<Current>,
    /// The key of the song played before the current one.
    prior: Option<String>,
}

impl State {
    pub fn new(ahead: usize, behind: usize, taste: Taste) -> State {
        State {
            ahead: ahead.max(1),
            behind,
            played: VecDeque::new(),
            sweep_dirty: false,
            last_sweep: None,
            repeat: Repeat::Off,
            playing: None,
            last_progress: 0,
            last_secs: 0,
            ended: false,
            generation: 0,
            upcoming: Vec::new(),
            queued_seen: HashSet::new(),
            autoplay_uris: HashSet::new(),
            pending: HashSet::new(),
            picks: HashMap::new(),
            rated: HashSet::new(),
            history: VecDeque::new(),
            artists: VecDeque::new(),
            disliked: HashSet::new(),
            disliked_order: VecDeque::new(),
            pool: None,
            cool_until: None,
            taste,
            cur: None,
            prior: None,
        }
    }

    /// The number of the current plan. A search that started under another number is stale.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Marks every running search as stale.
    pub fn supersede(&mut self) {
        self.generation += 1;
        self.queued_seen.clear();
    }

    /// Applies new values of the two settings.
    pub fn configure(&mut self, ahead: usize, behind: usize) {
        self.ahead = ahead.max(1);
        if behind != self.behind {
            self.behind = behind;
            self.trim_played();
        }
    }

    fn trim_played(&mut self) {
        while self.played.len() > self.behind {
            self.played.pop_front();
            self.sweep_dirty = true;
        }
    }

    /// Paths that the cache must keep: what plays, what waits, and the kept songs behind.
    pub fn protect(&self) -> HashSet<String> {
        let mut set: HashSet<String> = self.upcoming.iter().map(|u| u.to_string()).collect();
        if let Some(p) = &self.playing {
            set.insert(p.to_string());
        }
        set.extend(self.played.iter().cloned());
        set.extend(self.pending.iter().cloned());
        set
    }

    /// The paths to protect when the cache folder should be looked over now.
    pub fn sweep_due(&mut self, now: Instant) -> Option<HashSet<String>> {
        let periodic = self.last_sweep.is_none_or(|t| now.duration_since(t) >= SWEEP_EVERY);
        if !(self.sweep_dirty || periodic) {
            return None;
        }
        self.sweep_dirty = false;
        self.last_sweep = Some(now);
        Some(self.protect())
    }

    /// Files were deleted from the cache; stop tracking them.
    pub fn forgotten(&mut self, removed: &[String]) {
        for r in removed {
            self.autoplay_uris.remove(r);
            self.pending.remove(r);
            // A song that never played, such as one left in a queue that was replaced, will
            // never be rated or finished, so nothing else would drop what it kept.
            self.picks.remove(r);
            self.rated.remove(r);
        }
    }

    /// Remembers that the listener does not want this artist for now.
    fn dislike(&mut self, artist: String) {
        if self.disliked.insert(artist.clone()) {
            self.disliked_order.push_back(artist);
            while self.disliked_order.len() > MAX_DISLIKED {
                if let Some(old) = self.disliked_order.pop_front() {
                    self.disliked.remove(&old);
                }
            }
        }
    }

    /// What was learned, as JSON to save, once, when it changed.
    pub fn taste_json(&mut self) -> Option<String> {
        self.taste.take_json()
    }

    fn remember(&mut self, key: String, artist: String) {
        if self.history.len() >= HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(key);
        if self.artists.len() >= 8 {
            self.artists.pop_front();
        }
        self.artists.push_back(artist);
    }

    /// The player's state, once a second and after every command.
    pub fn playback(&mut self, state: Option<&PlaybackState>) {
        let Some(s) = state else {
            self.playing = None;
            self.ended = false;
            return;
        };
        if self.repeat != s.repeat {
            self.supersede();
        }
        self.repeat = s.repeat;
        let Some(t) = &s.track else { return };
        if self.playing.as_ref() != Some(&t.uri) {
            if self.playing.is_some() && !self.queued_seen.contains(t.uri.as_ref()) {
                self.supersede();
            }
            self.changed_to(t);
        }
        // The player says when the queue ran out. It is a state, not an edge: it holds until
        // something plays again, however many polls repeat it.
        self.ended = s.ended;
        self.last_progress = s.progress_ms;
    }

    /// A different song started: settle the one that was playing, then note the new one.
    fn changed_to(&mut self, t: &Track) {
        if let Some(prev) = self.playing.clone() {
            self.finished(prev.as_ref());
        }
        let raw = t.artists.first().map(|a| a.name.to_string()).unwrap_or_default();
        let artist = main_artist(&raw);
        let key = song_key(&t.name, &raw);
        self.taste.remember(
            &key,
            Node {
                title: t.name.to_string(),
                artist: raw.clone(),
                video_id: video_id_of(&t.uri),
                secs: (t.duration_ms > 0).then_some(t.duration_ms / 1000),
            },
        );
        self.prior = self.cur.as_ref().map(|c| c.key.clone());
        self.cur = Some(Current { key: key.clone(), artist: artist.clone() });
        self.playing = Some(t.uri.clone());
        self.last_secs = t.duration_ms / 1000;
        if !self.autoplay_uris.contains(t.uri.as_ref()) {
            // Songs autoplay chose are remembered when chosen; the user's now.
            self.remember(key, artist);
        }
        self.last_progress = 0;
    }

    /// The song `prev` is over (played out or left). What happened to it is the lesson.
    fn finished(&mut self, prev: &str) {
        let Some(cur) = self.cur.clone() else { return };
        let was_pick = self.autoplay_uris.contains(prev);
        self.pending.remove(prev);
        let pick = self.picks.remove(prev);
        let rated = self.rated.remove(prev);
        if was_pick {
            // Keep it for Previous, for a while.
            self.played.push_back(prev.to_string());
            self.trim_played();
            self.sweep_dirty = true;
        }
        match verdict(self.last_progress, self.last_secs) {
            Verdict::Good => {
                if let Some(p) = &self.prior {
                    self.taste.link(p, &cur.key, 1.0);
                }
                self.taste.nudge_artist(&cur.artist, 0.2);
                if let (Some(pick), false) = (&pick, rated) {
                    self.taste.train(&pick.feat, 1.0, 1.0);
                }
            }
            Verdict::Bad if was_pick => {
                self.dislike(cur.artist.clone());
                if let Some(p) = &self.prior {
                    self.taste.link(p, &cur.key, -0.5);
                }
                self.taste.nudge_artist(&cur.artist, -0.35);
                if let (Some(pick), false) = (&pick, rated) {
                    self.taste.train(&pick.feat, 0.0, 1.0);
                }
            }
            Verdict::Meh => {
                if let (Some(pick), false) = (&pick, rated) {
                    self.taste.train(&pick.feat, 0.2, 0.3);
                }
            }
            Verdict::Bad => {}
        }
    }

    /// The listener rated the song that plays: Keep (liked) or Dislike. Counts for more than
    /// listening. The waiting songs stay; only the songs found from now on use what was learned.
    pub fn rate(&mut self, uri: &str, liked: bool) {
        if self.playing.as_deref() != Some(uri) {
            return;
        }
        let Some(cur) = self.cur.clone() else { return };
        self.rated.insert(uri.to_string());
        if let Some(pick) = self.picks.get(uri) {
            self.taste.train(&pick.feat, if liked { 1.0 } else { 0.0 }, 3.0);
        }
        self.taste.nudge_artist(&cur.artist, if liked { 0.6 } else { -1.0 });
        if let Some(p) = &self.prior {
            self.taste.link(p, &cur.key, if liked { 2.0 } else { -2.0 });
        }
        if !liked {
            self.dislike(cur.artist);
        }
    }

    /// The queue changed (or was read again). Returns a search to run, when one is due.
    pub fn queue(&mut self, q: &Queue, enabled: bool, busy: bool, now: Instant) -> Option<FillJob> {
        self.upcoming = q.upcoming.iter().map(|t| t.uri.clone()).collect();
        if self.queued_seen.len() > 1000 {
            self.queued_seen.clear();
        }
        self.queued_seen.extend(self.upcoming.iter().map(|u| u.to_string()));
        if self.upcoming.len() < VISIBLE_QUEUE {
            // The whole queue is visible: songs that left it without playing (cleared, replaced)
            // no longer need their files.
            let upcoming = &self.upcoming;
            self.pending.retain(|p| upcoming.iter().any(|u| u.as_ref() == p));
        }
        let low = self.ahead.saturating_sub(REFILL_GAP).max(1);
        if !enabled || busy || self.repeat != Repeat::Off || self.upcoming.len() >= low {
            return None;
        }
        if self.cool_until.is_some_and(|t| now < t) {
            return None;
        }
        let cur = q.current.as_ref()?;
        let artist = cur.artists.first().map(|a| a.name.to_string()).unwrap_or_default();
        let seed = Seed {
            title: cur.name.to_string(),
            artist,
            video_id: video_id_of(&cur.uri),
            duration_secs: (cur.duration_ms > 0).then_some(cur.duration_ms / 1000),
        };
        let seed_key = song_key(&seed.title, &seed.artist);
        let pool = match self.pool.take() {
            Some((k, list, at)) if k == seed_key && now.duration_since(at) < POOL_LIFE => Some(list),
            _ => None,
        };
        let mut exclude: HashSet<String> = self.history.iter().cloned().collect();
        exclude.insert(seed_key.clone());
        Some(FillJob {
            history: self.taste.neighbors(&seed_key, 20),
            taste: self.taste.clone(),
            seed,
            seed_key,
            need: self.ahead - self.upcoming.len(),
            exclude,
            recent_artists: self.artists.iter().cloned().collect(),
            disliked: self.disliked.clone(),
            pool,
        })
    }

    /// A search finished. Returns true when the song that was playing has already ended,
    /// so the first added song must be started.
    pub fn done(&mut self, d: FillDone, now: Instant) -> bool {
        let found = !d.added.is_empty();
        for a in &d.added {
            self.autoplay_uris.insert(a.uri.clone());
            self.pending.insert(a.uri.clone());
            self.picks.insert(a.uri.clone(), Pick { feat: a.feat });
            self.remember(a.key.clone(), a.artist.clone());
        }
        self.pool = (!d.pool.is_empty()).then(|| (d.seed_key, d.pool, now));
        self.cool_until = Some(now + if found { SETTLE } else { BACKOFF });
        found && self.ended
    }

    /// A search that was superseded finished. The songs it already queued still count, but
    /// its pool and its result say nothing about the current plan, so no waiting time starts.
    pub fn done_stale(&mut self, d: FillDone) {
        for a in &d.added {
            self.autoplay_uris.insert(a.uri.clone());
            self.pending.insert(a.uri.clone());
            self.picks.insert(a.uri.clone(), Pick { feat: a.feat });
            self.remember(a.key.clone(), a.artist.clone());
        }
    }

    /// The search could not run to its end (the task failed).
    pub fn failed(&mut self, now: Instant) {
        self.cool_until = Some(now + BACKOFF);
    }

}

/// The YouTube id in a downloaded file's name, such as `Song [dQw4w9WgXcQ].m4a`.
fn video_id_of(uri: &str) -> Option<String> {
    meowtify_core::youtube_id(uri).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::taste::N_FEAT;
    use meowtify_core::Artist;

    fn state() -> State {
        State::new(4, 2, Taste::new())
    }

    fn track(uri: &str, name: &str, artist: &str, secs: u32) -> Track {
        Track {
            id: uri.into(),
            uri: uri.into(),
            name: name.into(),
            artists: Arc::from(vec![Artist { id: artist.into(), name: artist.into() }]),
            album: None,
            duration_ms: secs * 1000,
        }
    }

    fn pb(t: &Track, playing: bool, progress_ms: u32, repeat: Repeat) -> PlaybackState {
        // The player reports the end of the queue as "stopped at the start".
        PlaybackState { is_playing: playing, progress_ms, track: Some(t.clone()), volume: None, shuffle: false, repeat, ended: !playing && progress_ms == 0 }
    }

    fn queue(cur: &Track, upcoming: &[Track]) -> Queue {
        Queue { current: Some(cur.clone()), upcoming: Arc::from(upcoming.to_vec()), manual: 0, auto: 0, rev: 0 }
    }

    fn added(t: &Track) -> Added {
        Added { key: song_key(&t.name, &t.artists[0].name), artist: main_artist(&t.artists[0].name), uri: t.uri.to_string(), feat: [0.5; N_FEAT] }
    }

    fn done(s: &mut State, picks: &[Track], now: Instant) -> bool {
        s.done(FillDone { seed_key: "k".into(), pool: Vec::new(), added: picks.iter().map(added).collect() }, now)
    }

    #[test]
    fn forgetting_a_song_that_never_played_forgets_what_was_kept_about_it() {
        let mut s = state();
        let now = Instant::now();
        let picks: Vec<Track> = (0..1000).map(|i| track(&format!("p{i}"), &format!("Pick {i}"), "Act", 200)).collect();
        done(&mut s, &picks, now);
        assert_eq!(s.picks.len(), 1000);
        let removed: Vec<String> = picks.iter().map(|t| t.uri.to_string()).collect();
        s.forgotten(&removed);
        assert!(s.autoplay_uris.is_empty() && s.pending.is_empty());
        assert!(s.picks.is_empty(), "the feature records go with the files");
    }

    #[test]
    fn the_dislike_list_is_bounded_and_forgets_the_oldest_first() {
        let mut s = state();
        for i in 0..(MAX_DISLIKED + 50) {
            s.dislike(format!("act {i}"));
        }
        assert_eq!(s.disliked.len(), MAX_DISLIKED);
        assert_eq!(s.disliked_order.len(), MAX_DISLIKED);
        assert!(!s.disliked.contains("act 0"));
        assert!(s.disliked.contains(&format!("act {}", MAX_DISLIKED + 49)));
        s.dislike("act 100".into());
        assert_eq!(s.disliked.len(), MAX_DISLIKED, "an artist already listed is not counted twice");
    }

    #[test]
    fn reads_the_video_id_from_a_file_name() {
        assert_eq!(video_id_of(r"C:\Music\Song [dQw4w9WgXcQ].m4a").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(video_id_of("/m/A - B [abc-_DEF123].opus").as_deref(), Some("abc-_DEF123"));
        assert_eq!(video_id_of("C:/Music/Song.mp3"), None);
        assert_eq!(video_id_of("C:/Music/Song [short].mp3"), None);
        assert_eq!(video_id_of("C:/Music/Song [dQw4w9WgXcQ] extra.mp3"), None);
    }

    #[test]
    fn asks_for_songs_when_the_queue_is_short() {
        let mut s = state();
        let now = Instant::now();
        let a = track("C:/m/A [aaaaaaaaaaa].m4a", "Song A", "Band", 200);
        s.playback(Some(&pb(&a, true, 1000, Repeat::Off)));
        let job = s.queue(&queue(&a, &[]), true, false, now).expect("a search is due");
        assert_eq!(job.need, 4);
        assert_eq!(job.seed.video_id.as_deref(), Some("aaaaaaaaaaa"));
        assert_eq!(job.seed.duration_secs, Some(200));
        assert!(job.exclude.contains(&song_key("Song A", "Band")));

        let one = track("x", "Next", "Other", 200);
        assert_eq!(s.queue(&queue(&a, &[one.clone()]), true, false, now).unwrap().need, 3);
        assert!(s.queue(&queue(&a, &[one.clone(), one]), true, false, now).is_none());
    }

    #[test]
    fn stays_quiet_when_off_busy_repeating_or_cooling_down() {
        let mut s = state();
        let now = Instant::now();
        let a = track("a", "Song A", "Band", 200);
        s.playback(Some(&pb(&a, true, 1000, Repeat::Off)));
        assert!(s.queue(&queue(&a, &[]), false, false, now).is_none(), "switched off");
        assert!(s.queue(&queue(&a, &[]), true, true, now).is_none(), "a search runs");
        assert!(s.queue(&Queue::default(), true, false, now).is_none(), "nothing plays");
        s.done(FillDone { seed_key: "k".into(), pool: Vec::new(), added: Vec::new() }, now);
        assert!(s.queue(&queue(&a, &[]), true, false, now + Duration::from_secs(5)).is_none(), "backing off");
        assert!(s.queue(&queue(&a, &[]), true, false, now + Duration::from_secs(31)).is_some());
        s.playback(Some(&pb(&a, true, 2000, Repeat::Track)));
        assert!(s.queue(&queue(&a, &[]), true, false, now + Duration::from_secs(60)).is_none(), "repeat is on");
    }

    #[test]
    fn keeps_the_pool_for_the_same_song_only() {
        let mut s = state();
        let now = Instant::now();
        let a = track("a", "Song A", "Band", 200);
        s.playback(Some(&pb(&a, true, 1000, Repeat::Off)));
        let cand = Candidate { title: "T".into(), artist: "U".into(), duration_secs: None, item: None, source: "t" };
        let ranked = Ranked { cand, feat: [0.0; N_FEAT] };
        let p0 = track("p0", "P0", "Act", 200);
        s.done(FillDone { seed_key: song_key("Song A", "Band"), pool: vec![ranked], added: vec![added(&p0)] }, now);
        let later = now + Duration::from_secs(10);
        assert_eq!(s.queue(&queue(&a, &[]), true, false, later).unwrap().pool.map(|p| p.len()), Some(1));
        let b = track("b", "Song B", "Band", 200);
        s.playback(Some(&pb(&b, true, 1000, Repeat::Off)));
        assert!(s.queue(&queue(&b, &[]), true, false, later).unwrap().pool.is_none());
    }

    #[test]
    fn a_quick_skip_of_an_autoplay_song_marks_its_artist() {
        let mut s = state();
        let now = Instant::now();
        let a = track("a", "Song A", "Band", 200);
        s.playback(Some(&pb(&a, true, 100_000, Repeat::Off)));
        let picked = track("p", "Picked", "Boring Act", 200);
        done(&mut s, &[picked.clone()], now);
        s.playback(Some(&pb(&picked, true, 4000, Repeat::Off)));
        let c = track("c", "Song C", "Band", 200);
        s.playback(Some(&pb(&c, true, 500, Repeat::Off)));
        let job = s.queue(&queue(&c, &[]), true, false, now + Duration::from_secs(60)).unwrap();
        assert!(job.disliked.contains("boring act"));
        assert!(job.taste.affinity("boring act") < 0.0, "and the taste notes it");
        // Hearing a whole autoplay song is not a skip.
        let d = track("d", "Song D", "Fine Act", 200);
        done(&mut s, &[d.clone()], now);
        s.playback(Some(&pb(&d, true, 190_000, Repeat::Off)));
        s.playback(Some(&pb(&c, true, 500, Repeat::Off)));
        let job = s.queue(&queue(&c, &[]), true, false, now + Duration::from_secs(60)).unwrap();
        assert!(!job.disliked.contains("fine act"));
        assert!(job.taste.affinity("fine act") > 0.0);
    }

    #[test]
    fn keeps_only_the_chosen_number_of_played_songs() {
        let mut s = state();
        let now = Instant::now();
        let user = track("u", "Start", "Band", 200);
        s.playback(Some(&pb(&user, true, 1000, Repeat::Off)));
        let picks: Vec<Track> = (0..4).map(|i| track(&format!("p{i}"), &format!("Pick {i}"), "Act", 200)).collect();
        done(&mut s, &picks, now);
        // Play all four in turn; p3 is the one that plays now.
        for t in &picks {
            s.playback(Some(&pb(t, true, 60_000, Repeat::Off)));
        }
        s.queue(&queue(&picks[3], &[]), true, false, now);
        let kept = s.protect();
        assert!(kept.contains("p3"), "the playing song");
        assert!(kept.contains("p2") && kept.contains("p1"), "two played songs stay");
        assert!(!kept.contains("p0"), "the older one goes");
        assert!(!kept.contains("u"), "the user's own song is never in the cache");
        let due = s.sweep_due(now).expect("a song left the window");
        assert_eq!(due, kept);
        assert!(s.sweep_due(now + Duration::from_secs(5)).is_none(), "nothing changed since");
        assert!(s.sweep_due(now + Duration::from_secs(61)).is_some(), "the periodic look");
        // A smaller window drops one more.
        s.configure(4, 0);
        assert!(!s.protect().contains("p1"));
        assert!(s.sweep_due(now + Duration::from_secs(62)).is_some());
    }

    #[test]
    fn says_when_the_song_already_ended() {
        let mut s = state();
        let now = Instant::now();
        let a = track("a", "Song A", "Band", 200);
        s.playback(Some(&pb(&a, true, 199_000, Repeat::Off)));
        s.playback(Some(&pb(&a, false, 0, Repeat::Off)));
        let n = track("n", "Next", "X", 200);
        assert!(done(&mut s, &[n.clone()], now), "start the new song");

        let mut s = state();
        s.playback(Some(&pb(&a, true, 30_000, Repeat::Off)));
        s.playback(Some(&pb(&a, false, 31_000, Repeat::Off)));
        assert!(!done(&mut s, &[n], now), "a pause is not an end");
    }

    #[test]
    fn buttons_teach_more_than_listening_and_the_queue_stays() {
        let mut s = state();
        let now = Instant::now();
        let start = track("s", "Start", "Band", 200);
        s.playback(Some(&pb(&start, true, 100_000, Repeat::Off)));
        let picks = [track("p0", "Pick 0", "Loved Act", 200), track("p1", "Pick 1", "Hated Act", 200)];
        done(&mut s, &picks, now);
        s.playback(Some(&pb(&picks[0], true, 20_000, Repeat::Off)));
        s.rate("p0", true);
        s.playback(Some(&pb(&picks[1], true, 5_000, Repeat::Off)));
        s.rate("p1", false);
        s.rate("not-playing", false);
        let after = track("z", "After", "Band", 200);
        s.playback(Some(&pb(&after, true, 1000, Repeat::Off)));
        let job = s.queue(&queue(&after, &[]), true, false, now + Duration::from_secs(60)).unwrap();
        assert!(job.taste.affinity("loved act") > 0.4 && job.taste.affinity("hated act") < -0.9);
        assert!(job.disliked.contains("hated act"));
        assert!(s.taste_json().is_some(), "the lesson is saved");
    }

    #[test]
    fn songs_heard_in_a_row_become_suggestions_for_the_first() {
        let mut s = state();
        let now = Instant::now();
        let a = track("C:/m/A [aaaaaaaaaaa].m4a", "Song A", "Band", 200);
        let b = track("C:/m/B [bbbbbbbbbbb].m4a", "Song B", "Band", 200);
        let c = track("c", "Song C", "Other", 200);
        // The listener plays A, then B (both to the end), then C.
        for t in [&a, &b, &c] {
            s.playback(Some(&pb(t, true, 190_000, Repeat::Off)));
        }
        // A new session reaches A again: B is a suggestion, because B was heard after A.
        let a2 = track("C:/m/A [aaaaaaaaaaa].m4a", "Song A", "Band", 200);
        let mut s2 = State::new(4, 2, Taste::from_json(&s.taste_json().unwrap()));
        s2.playback(Some(&pb(&a2, true, 1000, Repeat::Off)));
        let job = s2.queue(&queue(&a2, &[]), true, false, now).unwrap();
        let titles: Vec<&str> = job.history.iter().map(|c| c.title.as_str()).collect();
        assert!(titles.contains(&"Song B"), "{titles:?}");
        assert_eq!(job.history.iter().find(|c| c.title == "Song B").unwrap().item.as_ref().map(|i| i.id.as_str()), Some("bbbbbbbbbbb"));
    }
}
