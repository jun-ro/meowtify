//! What autoplay learns about the listener. Small and pure (no network), saved as JSON.
//!
//! Two parts that work together:
//! - A **scorer**: logistic regression over a handful of numbers per suggestion (how high the
//!   services ranked it, how many agree, how much the listener likes the artist, how strongly
//!   it is linked to the current song, and so on). It starts neutral, so autoplay behaves as
//!   before, and moves with every song that is heard to the end (good), skipped (bad) or rated.
//! - A **graph** of "played after" links between songs, built from what the listener really
//!   plays: when song B is heard to the end after song A, A links to B. The links give
//!   suggestions of their own (the third source, "history") that no service knows, and one of
//!   the numbers the scorer looks at.
//!
//! Memory: a few hundred artists, at most 1500 songs and their links, well under 1 MB.

use std::collections::HashMap;
use std::path::Path;

use meowtify_provider::{Candidate, Item};
use serde::{Deserialize, Serialize};

pub const N_FEAT: usize = 9;
/// The numbers the scorer looks at for one suggestion.
pub type Features = [f32; N_FEAT];

/// Position of each number in [`Features`].
pub mod f {
    /// Reciprocal-rank score of the suggestion, scaled to about 0..1.
    pub const RRF: usize = 0;
    /// How many more lists than one name it.
    pub const AGREE: usize = 1;
    pub const DEEZER: usize = 2;
    pub const LASTFM: usize = 3;
    pub const HISTORY: usize = 4;
    /// 1 when the artist is the seed's artist.
    pub const SAME_ARTIST: usize = 5;
    /// How much the listener likes the artist, -1.5 to 1.5.
    pub const AFFINITY: usize = 6;
    /// Difference in length from the seed, 0..1 (two minutes or more is 1).
    pub const GAP: usize = 7;
    /// Strength of the "played after" link from the seed, -1..1.
    pub const LINK: usize = 8;
}

const MAX_ARTISTS: usize = 400;
const MAX_NODES: usize = 1500;
const MAX_LINKS_PER_NODE: usize = 24;
const LEARN_RATE: f32 = 0.15;
const DECAY: f32 = 0.002;
const WEIGHT_LIMIT: f32 = 3.0;
const EDGE_MAX: f32 = 8.0;
const EDGE_MIN: f32 = -3.0;

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// A song the listener has played, as much as is needed to suggest it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub title: String,
    pub artist: String,
    pub video_id: Option<String>,
    pub secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Taste {
    weights: Vec<f32>,
    artists: HashMap<String, f32>,
    nodes: HashMap<String, Node>,
    edges: HashMap<String, HashMap<String, f32>>,
    /// When each song was last remembered (a counter, not a clock), to drop the oldest first.
    seen: HashMap<String, u64>,
    clock: u64,
    #[serde(skip)]
    dirty: bool,
}

impl Default for Taste {
    fn default() -> Taste {
        // With every number at 0 the scorer returns the neutral answer. These starting weights
        // only decide which way the numbers push before anything is learned.
        let mut weights = vec![0.0; N_FEAT];
        weights[f::AFFINITY] = 0.8;
        weights[f::LINK] = 0.8;
        weights[f::GAP] = -0.4;
        Taste { weights, artists: HashMap::new(), nodes: HashMap::new(), edges: HashMap::new(), seen: HashMap::new(), clock: 0, dirty: false }
    }
}

impl Taste {
    #[cfg(test)]
    pub fn new() -> Taste {
        Taste::default()
    }

    /// Reads a saved file. A missing or damaged file gives a new, neutral one.
    pub fn load(path: &Path) -> Taste {
        std::fs::read_to_string(path).ok().map(|t| Taste::from_json(&t)).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Taste {
        let mut t: Taste = serde_json::from_str(text).unwrap_or_default();
        if t.weights.len() != N_FEAT || t.weights.iter().any(|w| !w.is_finite()) {
            t.weights = Taste::default().weights;
        }
        t.normalize();
        t
    }

    /// Makes a loaded graph obey every limit, whatever the file held.
    fn normalize(&mut self) {
        self.artists.retain(|_, v| v.is_finite());
        while self.artists.len() > MAX_ARTISTS {
            let Some(weakest) = self.artists.iter().min_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(k, _)| k.clone()) else { break };
            self.artists.remove(&weakest);
        }
        for row in self.edges.values_mut() {
            row.retain(|_, v| v.is_finite());
            for v in row.values_mut() {
                *v = v.clamp(EDGE_MIN, EDGE_MAX);
            }
            while row.len() > MAX_LINKS_PER_NODE {
                let Some(weakest) = row.iter().min_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(k, _)| k.clone()) else { break };
                row.remove(&weakest);
            }
        }
        self.seen.retain(|k, _| self.nodes.contains_key(k));
        self.clock = self.clock.max(self.seen.values().copied().max().unwrap_or(0));
        self.prune();
    }

    /// The JSON to save, once, when something changed since the last call.
    pub fn take_json(&mut self) -> Option<String> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        serde_json::to_string(self).ok()
    }

    /// Multiplies a suggestion's score: 1.0 when neutral, up to 2.0 when liked, down to 0.15.
    pub fn multiplier(&self, x: &Features) -> f32 {
        let z: f32 = self.weights.iter().zip(x).map(|(w, v)| w * v).sum();
        (2.0 * sigmoid(z)).clamp(0.15, 2.0)
    }

    /// One step of learning. `label` is 1 for a good suggestion and 0 for a bad one;
    /// `weight` says how much to trust it (1 for listening, 3 for a click on a button).
    pub fn train(&mut self, x: &Features, label: f32, weight: f32) {
        let z: f32 = self.weights.iter().zip(x).map(|(w, v)| w * v).sum();
        let err = label - sigmoid(z);
        for (w, v) in self.weights.iter_mut().zip(x) {
            *w = (*w + LEARN_RATE * weight * err * v - DECAY * *w).clamp(-WEIGHT_LIMIT, WEIGHT_LIMIT);
        }
        self.dirty = true;
    }

    pub fn affinity(&self, artist: &str) -> f32 {
        self.artists.get(artist).copied().unwrap_or(0.0)
    }

    pub fn nudge_artist(&mut self, artist: &str, delta: f32) {
        if artist.is_empty() {
            return;
        }
        let v = (self.affinity(artist) + delta).clamp(-1.5, 1.5);
        self.artists.insert(artist.to_string(), v);
        if self.artists.len() > MAX_ARTISTS {
            // Forget the artist the listener has the least opinion about.
            if let Some(weakest) = self.artists.iter().min_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(k, _)| k.clone()) {
                self.artists.remove(&weakest);
            }
        }
        self.dirty = true;
    }

    pub fn remember(&mut self, key: &str, node: Node) {
        self.clock += 1;
        self.seen.insert(key.to_string(), self.clock);
        if self.nodes.get(key) != Some(&node) {
            self.nodes.insert(key.to_string(), node);
            self.dirty = true;
        }
        // The limit holds after every insertion, not only when a link is made.
        if self.nodes.len() > MAX_NODES {
            self.prune();
        }
    }

    /// Adds `delta` to the link `from -> to`. Links that drop to nothing are removed.
    pub fn link(&mut self, from: &str, to: &str, delta: f32) {
        if from == to || from.is_empty() || to.is_empty() {
            return;
        }
        let row = self.edges.entry(from.to_string()).or_default();
        let v = (row.get(to).copied().unwrap_or(0.0) + delta).clamp(EDGE_MIN, EDGE_MAX);
        if v.abs() < 0.05 {
            row.remove(to);
        } else {
            row.insert(to.to_string(), v);
        }
        if row.len() > MAX_LINKS_PER_NODE {
            if let Some(weakest) = row.iter().min_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(k, _)| k.clone()) {
                row.remove(&weakest);
            }
        }
        self.dirty = true;
        if self.nodes.len() > MAX_NODES {
            self.prune();
        }
    }

    /// Strength of the link `from -> to`, -1 to 1.
    pub fn link_strength(&self, from: &str, to: &str) -> f32 {
        (self.edges.get(from).and_then(|r| r.get(to)).copied().unwrap_or(0.0) / 3.0).clamp(-1.0, 1.0)
    }

    /// Drops songs until the graph fits: first those with the weakest links (none at all
    /// first), and among equals the one remembered longest ago. Then drops links that point
    /// to a song that is gone.
    fn prune(&mut self) {
        self.edges.retain(|_, row| !row.is_empty());
        if self.nodes.len() > MAX_NODES {
            let mut strength: HashMap<&str, f32> = HashMap::new();
            for (from, row) in &self.edges {
                for (to, v) in row {
                    *strength.entry(from.as_str()).or_default() += v.abs();
                    *strength.entry(to.as_str()).or_default() += v.abs();
                }
            }
            let mut order: Vec<(f32, u64, String)> = self
                .nodes
                .keys()
                .map(|k| (strength.get(k.as_str()).copied().unwrap_or(0.0), self.seen.get(k).copied().unwrap_or(0), k.clone()))
                .collect();
            order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            let excess = self.nodes.len() - MAX_NODES;
            let evicted: std::collections::HashSet<String> = order.into_iter().take(excess).map(|(_, _, k)| k).collect();
            for k in &evicted {
                self.nodes.remove(k);
                self.seen.remove(k);
                self.edges.remove(k);
            }
            for row in self.edges.values_mut() {
                row.retain(|k, _| !evicted.contains(k));
            }
            self.edges.retain(|_, row| !row.is_empty());
            self.dirty = true;
        }
    }

    /// Songs that were heard to the end after `key`, strongest link first, as suggestions.
    /// The ones that carry a video id are ready to download.
    pub fn neighbors(&self, key: &str, limit: usize) -> Vec<Candidate> {
        let Some(row) = self.edges.get(key) else { return Vec::new() };
        let mut links: Vec<(&String, f32)> = row.iter().filter(|(_, v)| **v > 0.0).map(|(k, v)| (k, *v)).collect();
        links.sort_by(|a, b| b.1.total_cmp(&a.1));
        links
            .into_iter()
            .filter_map(|(k, _)| self.nodes.get(k))
            .take(limit)
            .map(|n| Candidate {
                title: n.title.clone(),
                artist: n.artist.clone(),
                duration_secs: n.secs,
                item: n.video_id.as_ref().map(|id| Item {
                    provider: "youtube",
                    id: id.clone(),
                    title: n.title.clone(),
                    artist: n.artist.clone(),
                    duration_secs: n.secs,
                    url: format!("https://www.youtube.com/watch?v={id}"),
                    thumbnail: Some(meowtify_core::youtube_thumbnail(&id)),
                }),
                source: "history",
            })
            .collect()
    }

    #[cfg(test)]
    pub fn weights(&self) -> &[f32] {
        &self.weights
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feat(pairs: &[(usize, f32)]) -> Features {
        let mut x = [0.0; N_FEAT];
        for (i, v) in pairs {
            x[*i] = *v;
        }
        x
    }

    #[test]
    fn a_new_taste_changes_nothing() {
        let t = Taste::new();
        assert_eq!(t.multiplier(&[0.0; N_FEAT]), 1.0);
        // Liked artists and strong links push up, a different length pushes down, even untrained.
        assert!(t.multiplier(&feat(&[(f::AFFINITY, 1.0)])) > 1.0);
        assert!(t.multiplier(&feat(&[(f::GAP, 1.0)])) < 1.0);
    }

    #[test]
    fn good_and_bad_songs_move_the_scorer_the_right_way() {
        let mut t = Taste::new();
        let deezer = feat(&[(f::RRF, 0.5), (f::DEEZER, 1.0)]);
        let history = feat(&[(f::RRF, 0.5), (f::HISTORY, 1.0)]);
        for _ in 0..20 {
            t.train(&deezer, 0.0, 1.0);
            t.train(&history, 1.0, 1.0);
        }
        assert!(t.multiplier(&deezer) < 0.7, "{}", t.multiplier(&deezer));
        assert!(t.multiplier(&history) > 1.3, "{}", t.multiplier(&history));
        assert!(t.weights().iter().all(|w| w.abs() <= WEIGHT_LIMIT));
        // A button click counts for more than listening.
        let mut a = Taste::new();
        let mut b = Taste::new();
        a.train(&deezer, 1.0, 1.0);
        b.train(&deezer, 1.0, 3.0);
        assert!(b.multiplier(&deezer) > a.multiplier(&deezer));
    }

    #[test]
    fn artists_and_links_are_remembered_and_limited() {
        let mut t = Taste::new();
        t.nudge_artist("band", 0.8);
        t.nudge_artist("band", 0.8);
        assert_eq!(t.affinity("band"), 1.5, "limited to 1.5");
        t.nudge_artist("", 1.0);
        assert_eq!(t.affinity(""), 0.0);
        t.link("a", "b", 1.0);
        t.link("a", "b", 1.0);
        assert!((t.link_strength("a", "b") - 2.0 / 3.0).abs() < 1e-6);
        t.link("a", "b", -2.0);
        assert_eq!(t.link_strength("a", "b"), 0.0, "a link that reaches 0 is gone");
        t.link("a", "a", 1.0);
        assert_eq!(t.link_strength("a", "a"), 0.0, "no link to itself");
        for i in 0..(MAX_LINKS_PER_NODE + 10) {
            t.link("hub", &format!("n{i}"), 1.0 + i as f32 * 0.1);
        }
        assert!(t.edges["hub"].len() <= MAX_LINKS_PER_NODE);
    }

    #[test]
    fn neighbors_are_the_songs_heard_after_this_one() {
        let mut t = Taste::new();
        let node = |title: &str, vid: Option<&str>| Node { title: title.into(), artist: "X".into(), video_id: vid.map(str::to_string), secs: Some(200) };
        t.remember("x|one", node("One", Some("aaaaaaaaaaa")));
        t.remember("x|two", node("Two", None));
        t.remember("x|bad", node("Bad", None));
        t.link("seed", "x|one", 3.0);
        t.link("seed", "x|two", 1.0);
        t.link("seed", "x|bad", -1.0);
        let got = t.neighbors("seed", 10);
        assert_eq!(got.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["One", "Two"], "strongest first, bad links left out");
        assert_eq!(got[0].item.as_ref().map(|i| i.id.as_str()), Some("aaaaaaaaaaa"));
        assert!(got[1].item.is_none());
        assert_eq!(got[0].source, "history");
        assert!(t.neighbors("unknown", 10).is_empty());
    }

    #[test]
    fn saves_and_loads_and_survives_damage() {
        let mut t = Taste::new();
        assert!(t.take_json().is_none(), "nothing changed yet");
        t.nudge_artist("band", 0.5);
        t.train(&feat(&[(f::HISTORY, 1.0)]), 1.0, 1.0);
        t.link("a", "b", 1.0);
        let json = t.take_json().expect("changed");
        assert!(t.take_json().is_none(), "saved once");
        let back = Taste::from_json(&json);
        assert_eq!(back.affinity("band"), 0.5);
        assert_eq!(back.weights(), t.weights());
        assert!(back.link_strength("a", "b") > 0.0);
        assert_eq!(Taste::from_json("not json").multiplier(&[0.0; N_FEAT]), 1.0);
        assert_eq!(Taste::from_json(r#"{"weights":[1.0]}"#).weights().len(), N_FEAT, "a wrong size resets the weights");
    }

    #[test]
    fn the_graph_stays_within_its_size() {
        let mut t = Taste::new();
        for i in 0..(MAX_NODES + 200) {
            let key = format!("k{i}");
            t.remember(&key, Node { title: key.clone(), artist: "a".into(), video_id: None, secs: None });
            t.link(&format!("k{}", i.saturating_sub(1)), &key, 1.0);
        }
        assert!(t.nodes.len() <= MAX_NODES + 1, "{}", t.nodes.len());
    }
}
