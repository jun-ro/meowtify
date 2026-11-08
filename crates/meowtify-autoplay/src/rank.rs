//! Merging and ordering the suggestions of several recommenders. Pure code, so it is
//! tested without a network.
//!
//! Each recommender returns a list, closest first. A song gets a score for every list
//! it is in (reciprocal rank, so a high place counts most), and a bonus for being named
//! by more than one recommender, because two services that know different things
//! agreeing is the best sign of a close match. Then songs that do not belong are
//! removed, and the order is made varied: the same artist many times in a row is a
//! playlist of one band, not a radio.

use std::collections::HashSet;

use meowtify_provider::{main_artist, song_key, variants, Candidate, Seed};

use crate::taste::{f, Features, Taste, N_FEAT};

/// Shortest and longest song that autoplay accepts, in seconds. Shorter is an intro or a
/// clip, longer is a mix or a full album upload.
pub const MIN_SECS: u32 = 60;
pub const MAX_SECS: u32 = 720;

/// Smaller values favour the top of each list more.
const RRF_K: f32 = 20.0;
/// Each extra list that names a song multiplies its score by one plus this.
const AGREEMENT_BONUS: f32 = 0.5;
/// Score factor for each recent pick by the same artist.
const REPEAT_ARTIST: f32 = 0.55;
/// Score factor for an artist the user skipped.
const DISLIKED: f32 = 0.25;
/// How many latest picks count for the "same artist again" penalty.
const RECENT: usize = 3;

pub struct Context<'a> {
    /// The song to be close to.
    pub seed: &'a Seed,
    /// `song_key`s that must not come back: played or already queued.
    pub exclude: &'a HashSet<String>,
    /// Main artists of the songs played or queued last, newest last.
    pub recent_artists: &'a [String],
    /// Main artists the user skipped quickly in this session.
    pub disliked: &'a HashSet<String>,
    /// What was learned about the listener: scales each score and adds the "played after" links.
    pub taste: &'a Taste,
}

/// A suggestion in its place, with the numbers the scorer saw, so what the listener does
/// with it can teach the scorer afterwards.
#[derive(Debug, Clone)]
pub struct Ranked {
    pub cand: Candidate,
    pub feat: Features,
}

struct Entry {
    score: f32,
    lists: u32,
    key: String,
    artist: String,
    cand: Candidate,
    /// The list that named it first.
    source: &'static str,
    feat: Features,
}

/// How much a recommender's list counts. YouTube Music follows what people play after this
/// very song. Deezer's radio follows the artist, so it is a good second opinion, but its
/// first places are often a different song of a loosely related artist.
fn source_weight(source: &str) -> f32 {
    match source {
        "deezer" => 0.7,
        "lastfm" => 1.1,
        // Songs this listener really played after the current one.
        "history" => 1.3,
        _ => 1.0,
    }
}

/// True when the candidate is not wanted at all.
fn rejected(c: &Candidate, cx: &Context, seed_key: &str, seed_variants: &[&'static str]) -> bool {
    if c.title.trim().is_empty() {
        return true;
    }
    if c.duration_secs.is_some_and(|s| !(MIN_SECS..=MAX_SECS).contains(&s)) {
        return true;
    }
    // A live version, a remix or a slowed copy only fits when the seed is one too.
    if variants(&c.title).iter().any(|v| !seed_variants.contains(v)) {
        return true;
    }
    let key = song_key(&c.title, &c.artist);
    key == seed_key || cx.exclude.contains(&key)
}

/// The suggestions merged into one list: the best next song first.
pub fn rank(lists: &[Vec<Candidate>], cx: &Context) -> Vec<Ranked> {
    let seed_key = song_key(&cx.seed.title, &cx.seed.artist);
    let seed_variants = variants(&cx.seed.title);
    let mut entries: Vec<Entry> = Vec::new();

    for list in lists {
        let mut seen_here: HashSet<String> = HashSet::new();
        for (place, c) in list.iter().enumerate() {
            if rejected(c, cx, &seed_key, &seed_variants) {
                continue;
            }
            let key = song_key(&c.title, &c.artist);
            if !seen_here.insert(key.clone()) {
                continue;
            }
            let gain = source_weight(c.source) / (RRF_K + place as f32);
            match entries.iter_mut().find(|e| e.key == key) {
                Some(e) => {
                    e.score += gain;
                    e.lists += 1;
                    // Keep the version that already knows where to download from.
                    if e.cand.item.is_none() && c.item.is_some() {
                        e.cand = c.clone();
                    }
                }
                None => entries.push(Entry {
                    score: gain,
                    lists: 1,
                    artist: main_artist(&c.artist),
                    key,
                    cand: c.clone(),
                    source: c.source,
                    feat: [0.0; N_FEAT],
                }),
            }
        }
    }
    let seed_artist = main_artist(&cx.seed.artist);
    for e in &mut entries {
        e.score *= 1.0 + AGREEMENT_BONUS * (e.lists - 1) as f32;
        let mut x = [0.0; N_FEAT];
        x[f::RRF] = (e.score * 20.0).min(2.0);
        x[f::AGREE] = (e.lists - 1) as f32;
        match e.source {
            "deezer" => x[f::DEEZER] = 1.0,
            "lastfm" => x[f::LASTFM] = 1.0,
            "history" => x[f::HISTORY] = 1.0,
            _ => {}
        }
        x[f::SAME_ARTIST] = f32::from(u8::from(e.artist == seed_artist));
        x[f::AFFINITY] = cx.taste.affinity(&e.artist);
        if let (Some(a), Some(b)) = (e.cand.duration_secs, cx.seed.duration_secs) {
            x[f::GAP] = (a.abs_diff(b) as f32 / 120.0).min(1.0);
        }
        x[f::LINK] = cx.taste.link_strength(&seed_key, &e.key);
        e.feat = x;
        e.score *= cx.taste.multiplier(&x);
    }

    // Pick one at a time, so the artists already chosen weigh on the next choice.
    let mut recent: Vec<String> = cx.recent_artists.to_vec();
    let mut out = Vec::with_capacity(entries.len());
    while !entries.is_empty() {
        let mut best = 0;
        let mut best_score = f32::MIN;
        for (i, e) in entries.iter().enumerate() {
            let tail = &recent[recent.len().saturating_sub(RECENT)..];
            let again = tail.iter().filter(|a| **a == e.artist).count() as i32;
            let mut s = e.score * REPEAT_ARTIST.powi(again);
            if cx.disliked.contains(&e.artist) {
                s *= DISLIKED;
            }
            // Strictly greater: on a tie the earlier (better placed) song stays.
            if s > best_score {
                best_score = s;
                best = i;
            }
        }
        let e = entries.remove(best);
        recent.push(e.artist);
        out.push(Ranked { cand: e.cand, feat: e.feat });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> Seed {
        Seed { title: "Seed Song".into(), artist: "Seed Artist".into(), video_id: None, duration_secs: Some(200) }
    }

    fn c(title: &str, artist: &str, secs: Option<u32>) -> Candidate {
        Candidate { title: title.into(), artist: artist.into(), duration_secs: secs, item: None, source: "test" }
    }

    fn run(lists: &[Vec<Candidate>], exclude: &[&str], recent: &[&str], disliked: &[&str]) -> Vec<String> {
        let exclude: HashSet<String> = exclude.iter().map(|s| s.to_string()).collect();
        let recent: Vec<String> = recent.iter().map(|s| s.to_string()).collect();
        let disliked: HashSet<String> = disliked.iter().map(|s| s.to_string()).collect();
        let seed = seed();
        let taste = Taste::new();
        let cx = Context { seed: &seed, exclude: &exclude, recent_artists: &recent, disliked: &disliked, taste: &taste };
        rank(lists, &cx).into_iter().map(|r| r.cand.title).collect()
    }

    #[test]
    fn two_lists_agreeing_beat_one_list_leading() {
        let a = vec![c("Solo", "A", Some(200)), c("Both", "B", Some(200))];
        let b = vec![c("Other", "C", Some(200)), c("Both (Official Video)", "B", Some(200))];
        let got = run(&[a, b], &[], &[], &[]);
        assert_eq!(got[0], "Both");
        assert_eq!(got.len(), 3, "one entry for the song both lists named");
    }

    #[test]
    fn leaves_out_what_does_not_belong() {
        let list = vec![
            c("Seed Song (Official Audio)", "Seed Artist", Some(200)),
            c("Played Before", "X", Some(200)),
            c("Too Short", "Y", Some(20)),
            c("Whole Album", "Y", Some(3000)),
            c("Song (Live at Wembley)", "Z", Some(200)),
            c("Song (Sped Up)", "Z", Some(120)),
            c("Unknown Length", "W", None),
            c("Good One", "V", Some(210)),
        ];
        let got = run(&[list], &["x|played before"], &[], &[]);
        assert_eq!(got, ["Unknown Length", "Good One"]);
    }

    #[test]
    fn a_live_seed_accepts_live_songs() {
        let seed = Seed { title: "Seed (Live)".into(), artist: "S".into(), video_id: None, duration_secs: None };
        let none = HashSet::new();
        let taste = Taste::new();
        let cx = Context { seed: &seed, exclude: &none, recent_artists: &[], disliked: &none, taste: &taste };
        let got = rank(&[vec![c("Other (Live)", "T", Some(200)), c("Plain", "U", Some(200))]], &cx);
        assert_eq!(got.len(), 2, "plain songs have no version marker, so they fit too");
        let got = rank(&[vec![c("Other (Live)", "T", Some(200))]], &cx);
        assert_eq!(got[0].cand.title, "Other (Live)");
    }

    #[test]
    fn the_same_artist_does_not_fill_the_queue() {
        let list = vec![
            c("A1", "Band", Some(200)),
            c("A2", "Band", Some(200)),
            c("A3", "Band", Some(200)),
            c("B1", "Other", Some(200)),
        ];
        let got = run(&[list], &[], &[], &[]);
        assert_eq!(got[0], "A1");
        assert_eq!(got[1], "B1", "the second song comes from someone else");
    }

    #[test]
    fn recent_and_skipped_artists_move_down() {
        let list = vec![c("A1", "Band", Some(200)), c("B1", "Other", Some(200))];
        assert_eq!(run(&[list.clone()], &[], &["band", "band"], &[])[0], "B1");
        assert_eq!(run(&[list], &[], &[], &["band"])[0], "B1");
    }

    #[test]
    fn what_was_learned_changes_the_order() {
        let seed = seed();
        let none = HashSet::new();
        let lists = vec![vec![c("Liked Artist Song", "Fan Favourite", Some(200)), c("Other", "Stranger", Some(200))]];
        let mut taste = Taste::new();
        let plain = Context { seed: &seed, exclude: &none, recent_artists: &[], disliked: &none, taste: &taste };
        assert_eq!(rank(&lists, &plain)[0].cand.title, "Liked Artist Song", "first place wins when nothing is known");
        let mut lists = lists;
        lists[0].reverse();
        assert_eq!(rank(&lists, &plain)[0].cand.title, "Other");
        taste.nudge_artist("fan favourite", 1.5);
        let learned = Context { seed: &seed, exclude: &none, recent_artists: &[], disliked: &none, taste: &taste };
        let got = rank(&lists, &learned);
        assert_eq!(got[0].cand.title, "Liked Artist Song", "a liked artist moves ahead of the better placed stranger");
        assert!(got[0].feat[f::AFFINITY] > 1.0, "the numbers are kept for learning");
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(run(&[], &[], &[], &[]).is_empty());
        assert!(run(&[Vec::new()], &[], &[], &[]).is_empty());
    }
}
