//! Finding songs that are close to a given song.
//!
//! A [`Recommender`] turns one seed song into a ranked list of candidates. Different
//! recommenders know different things: YouTube Music radio follows what people play
//! together, Deezer's artist radio follows artist similarity. The autoplay crate merges
//! the lists. A candidate that has no playable [`Item`] yet is resolved by [`resolve`],
//! which looks for the same recording on YouTube Music and checks the length.

use async_trait::async_trait;
use serde_json::Value;

use crate::youtube::{music_search, parse_clock, url_encode};
use crate::{Item, ProviderError, Result};

/// The song that other songs should be close to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub title: String,
    pub artist: String,
    /// YouTube video id, when the song came from YouTube.
    pub video_id: Option<String>,
    pub duration_secs: Option<u32>,
}

/// A song a recommender suggests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub title: String,
    pub artist: String,
    pub duration_secs: Option<u32>,
    /// Ready to download, when the recommender already knows where. Otherwise [`resolve`] finds it.
    pub item: Option<Item>,
    /// Which recommender made it.
    pub source: &'static str,
}

#[async_trait]
pub trait Recommender: Send + Sync {
    fn name(&self) -> &'static str;

    /// Candidates close to `seed`, closest first.
    async fn similar(&self, seed: &Seed) -> Result<Vec<Candidate>>;
}

// ---- text helpers --------------------------------------------------------------------

/// Lower case letters and digits only, with brackets, "feat." clauses and a trailing
/// " - Remastered 2011" style suffix removed. Two uploads of one song get the same text.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0u32;
    for c in text.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    let mut s = out.to_lowercase();
    if let Some(i) = s.find(" - ") {
        // "Song - Remastered 2011", "Song - Radio Edit": the part after the dash is a note.
        const NOTES: [&str; 9] = ["remaster", "version", "edit", "mix", "live", "mono", "stereo", "single", "from "];
        if NOTES.iter().any(|n| s[i..].contains(n)) {
            s.truncate(i);
        }
    }
    for marker in [" feat.", " feat ", " ft.", " ft ", " featuring "] {
        if let Some(i) = s.find(marker) {
            s.truncate(i);
        }
    }
    let kept: String = s.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The main artist: the first name before `,`, `&`, "feat" or "x".
pub fn main_artist(artist: &str) -> String {
    let first = artist.split([',', '&', '/']).next().unwrap_or(artist);
    normalize(first)
}

/// One text for one recording, whatever the upload is called.
pub fn song_key(title: &str, artist: &str) -> String {
    format!("{}|{}", main_artist(artist), normalize(title))
}

/// Kinds of version that are not the plain studio song.
pub fn variants(title: &str) -> Vec<&'static str> {
    const WORDS: [(&str, &str); 18] = [
        ("piano", "piano"),
        ("pianoforte", "piano"),
        ("live", "live"),
        ("concert", "live"),
        ("cover", "cover"),
        ("remix", "remix"),
        ("mix", "remix"),
        ("mashup", "remix"),
        ("bootleg", "remix"),
        ("instrumental", "instrumental"),
        ("karaoke", "instrumental"),
        ("nightcore", "sped"),
        ("reverb", "slowed"),
        ("acoustic", "acoustic"),
        ("extended", "extended"),
        ("demo", "demo"),
        ("rehearsal", "demo"),
        ("medley", "medley"),
    ];
    const PHRASES: [(&str, &str); 5] =
        [("sped up", "sped"), ("speed up", "sped"), ("slowed", "slowed"), ("8d audio", "slowed"), ("bass boosted", "slowed")];
    let lower = title.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let mut found: Vec<&'static str> = Vec::new();
    for (w, tag) in WORDS {
        if words.contains(&w) && !found.contains(&tag) {
            found.push(tag);
        }
    }
    for (p, tag) in PHRASES {
        if lower.contains(p) && !found.contains(&tag) {
            found.push(tag);
        }
    }
    // "Radio Mix" and "Radio Edit" are the normal single, not a remix.
    if lower.contains("radio mix") || lower.contains("radio edit") {
        found.retain(|t| *t != "remix");
    }
    found
}

fn tokens(s: &str) -> Vec<String> {
    normalize(s).split(' ').filter(|t| !t.is_empty()).map(str::to_string).collect()
}

/// Share of the words of the shorter text that the other text has, 0.0 to 1.0.
fn overlap(a: &str, b: &str) -> f32 {
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let (short, long) = if ta.len() <= tb.len() { (&ta, &tb) } else { (&tb, &ta) };
    short.iter().filter(|t| long.contains(t)).count() as f32 / short.len() as f32
}

/// The item that is most likely the same recording as `title` by `artist`. A length that
/// differs by more than `SLACK` seconds rules an item out when both lengths are known.
pub fn pick_match(items: &[Item], title: &str, artist: &str, secs: Option<u32>) -> Option<Item> {
    const SLACK: u32 = 8;
    let want = variants(title);
    let mut best: Option<(f32, &Item)> = None;
    for it in items {
        let t = overlap(&it.title, title);
        let a = if artist.trim().is_empty() || it.artist.trim().is_empty() { 0.5 } else { overlap(&it.artist, artist) };
        if t < 0.75 || a < 0.5 {
            continue;
        }
        if let (Some(x), Some(y)) = (secs, it.duration_secs) {
            if x.abs_diff(y) > SLACK {
                continue;
            }
        }
        // Another kind of version than the one asked for is a poor match.
        // Words the other artist has beyond the asked one ("Tribute", "Orchestra") cost a little.
        let extra = tokens(&it.artist).len().saturating_sub(tokens(artist).len()) as f32;
        let mut score = t + a - (0.05 * extra).min(0.2);
        if variants(&it.title) != want {
            score -= 0.8;
        }
        if score > best.map_or(f32::MIN, |b| b.0) {
            best = Some((score, it));
        }
    }
    best.filter(|(s, _)| *s > 0.9).map(|(_, it)| it.clone())
}

/// A playable item for `c`: the one it carries, or the matching YouTube Music song.
pub async fn resolve(c: &Candidate) -> Option<Item> {
    if let Some(it) = &c.item {
        return Some(it.clone());
    }
    let q = format!("{} {}", c.artist, c.title);
    let found = music_search(q.trim(), 8).await.ok()?;
    pick_match(&found, &c.title, &c.artist, c.duration_secs)
}

// ---- YouTube Music radio ------------------------------------------------------------

const NEXT_URL: &str = "https://music.youtube.com/youtubei/v1/next?prettyPrint=false";
const CLIENT_VERSION: &str = "1.20240724.01.00";

/// "Radio" of a song on YouTube Music: the queue it builds after the song, from what
/// listeners play together. Needs no account.
pub struct YtMusicRadio;

#[async_trait]
impl Recommender for YtMusicRadio {
    fn name(&self) -> &'static str {
        "youtube-music"
    }

    async fn similar(&self, seed: &Seed) -> Result<Vec<Candidate>> {
        let id = match &seed.video_id {
            Some(id) => id.clone(),
            None => {
                let q = format!("{} {}", seed.artist, seed.title);
                let found = music_search(q.trim(), 8).await?;
                pick_match(&found, &seed.title, &seed.artist, seed.duration_secs)
                    .map(|i| i.id)
                    .ok_or_else(|| ProviderError::Invalid("the song was not found on YouTube Music".into()))?
            }
        };
        let body = serde_json::json!({
            "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "US" } },
            "videoId": id,
            "playlistId": format!("RDAMVM{id}"),
            "isAudioOnly": true,
        })
        .to_string();
        let out = crate::ytdlp::curl_post_json(NEXT_URL, &body, 10).await?;
        parse_next(&out, &id)
    }
}

/// Reads the queue of a `next` answer. The seed itself is left out.
pub(crate) fn parse_next(body: &[u8], seed_id: &str) -> Result<Vec<Candidate>> {
    let v: Value = serde_json::from_slice(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let mut rows: Vec<&Value> = Vec::new();
    collect(&v, "playlistPanelVideoRenderer", &mut rows);
    let text = |v: Option<&Value>| -> Vec<String> {
        v.and_then(|x| x.get("runs"))
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(|x| x.get("text")?.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for r in rows {
        let Some(id) = r.get("videoId").and_then(Value::as_str).filter(|i| !i.is_empty() && *i != seed_id) else { continue };
        let title = text(r.get("title")).concat();
        if title.is_empty() {
            continue;
        }
        // "Artist • 1.8B views • 19M likes" or "Artist • Album • 2011".
        let artist = text(r.get("longBylineText")).iter().take_while(|t| t.trim() != "•").map(String::as_str).collect::<String>();
        let secs = parse_clock(&text(r.get("lengthText")).concat());
        let item = Item {
            provider: "youtube",
            id: id.to_string(),
            title: title.clone(),
            artist: artist.trim().to_string(),
            duration_secs: secs,
            url: format!("https://www.youtube.com/watch?v={id}"),
            thumbnail: Some(meowtify_core::youtube_thumbnail(&id)),
        };
        out.push(Candidate { title, artist: item.artist.clone(), duration_secs: secs, item: Some(item), source: "youtube-music" });
    }
    if out.is_empty() {
        return Err(ProviderError::Parse("the radio answer had no songs".into()));
    }
    Ok(out)
}

/// Every value under `key`, anywhere in `v`.
fn collect<'a>(v: &'a Value, key: &str, acc: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == key {
                    acc.push(x);
                } else {
                    collect(x, key, acc);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect(x, key, acc)),
        _ => {}
    }
}

// ---- Deezer artist radio ------------------------------------------------------------

/// Deezer's radio for the seed's artist: a mix of that artist and artists listeners of
/// theirs also play. Public API, no key. Candidates carry no video, so [`resolve`] finds them.
pub struct DeezerRadio;

#[async_trait]
impl Recommender for DeezerRadio {
    fn name(&self) -> &'static str {
        "deezer"
    }

    async fn similar(&self, seed: &Seed) -> Result<Vec<Candidate>> {
        if seed.artist.trim().is_empty() {
            return Err(ProviderError::Invalid("Deezer needs an artist".into()));
        }
        let q = url_encode(&format!("{} {}", seed.artist, seed.title));
        let found = crate::ytdlp::curl_text(&format!("https://api.deezer.com/search?q={q}&limit=10")).await?;
        let artist_id = deezer_artist_id(found.as_bytes(), &seed.title, &seed.artist, seed.duration_secs)
            .ok_or_else(|| ProviderError::Invalid("the artist was not found on Deezer".into()))?;
        let radio = crate::ytdlp::curl_text(&format!("https://api.deezer.com/artist/{artist_id}/radio?limit=40")).await?;
        parse_deezer_tracks(radio.as_bytes())
    }
}

/// The Deezer artist of the search result that best matches the seed song.
pub(crate) fn deezer_artist_id(body: &[u8], title: &str, artist: &str, secs: Option<u32>) -> Option<u64> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let rows = v.get("data")?.as_array()?;
    let mut as_items = Vec::new();
    for r in rows {
        let (Some(t), Some(a), Some(id)) = (
            r.get("title").and_then(Value::as_str),
            r.pointer("/artist/name").and_then(Value::as_str),
            r.pointer("/artist/id").and_then(Value::as_u64),
        ) else {
            continue;
        };
        as_items.push(Item {
            provider: "deezer",
            id: id.to_string(),
            title: t.to_string(),
            artist: a.to_string(),
            duration_secs: r.get("duration").and_then(Value::as_u64).map(|d| d as u32),
            url: String::new(),
            thumbnail: None,
        });
    }
    let best = pick_match(&as_items, title, artist, secs)?;
    best.id.parse().ok()
}

pub(crate) fn parse_deezer_tracks(body: &[u8]) -> Result<Vec<Candidate>> {
    let v: Value = serde_json::from_slice(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let rows = v.get("data").and_then(Value::as_array).ok_or_else(|| ProviderError::Parse("unexpected Deezer answer".into()))?;
    let out: Vec<Candidate> = rows
        .iter()
        .filter_map(|r| {
            Some(Candidate {
                title: r.get("title")?.as_str()?.to_string(),
                artist: r.pointer("/artist/name")?.as_str()?.to_string(),
                duration_secs: r.get("duration").and_then(Value::as_u64).map(|d| d as u32).filter(|&d| d > 0),
                item: None,
                source: "deezer",
            })
        })
        .collect();
    if out.is_empty() {
        return Err(ProviderError::Parse("the Deezer radio had no tracks".into()));
    }
    Ok(out)
}

// ---- Last.fm --------------------------------------------------------------------------

/// Whether `key` looks like a Last.fm API key: 32 hexadecimal characters.
pub fn valid_lastfm_key(key: &str) -> bool {
    key.len() == 32 && key.chars().all(|c| c.is_ascii_hexdigit())
}

/// Last.fm's "similar tracks": songs that people who play this song also play, from years of
/// listening data. Needs a free API key (https://www.last.fm/api/account/create), which the
/// user puts in Settings. Candidates carry no video, so [`resolve`] finds them.
pub struct LastFmSimilar {
    pub key: String,
}

#[async_trait]
impl Recommender for LastFmSimilar {
    fn name(&self) -> &'static str {
        "lastfm"
    }

    async fn similar(&self, seed: &Seed) -> Result<Vec<Candidate>> {
        if !valid_lastfm_key(&self.key) {
            return Err(ProviderError::Invalid("the Last.fm key must be 32 letters and digits".into()));
        }
        if seed.artist.trim().is_empty() || seed.title.trim().is_empty() {
            return Err(ProviderError::Invalid("Last.fm needs an artist and a title".into()));
        }
        // Last.fm knows "Song", not "Song (Official Video)".
        let title = crate::recommend::normalize_for_lookup(&seed.title);
        let url = format!(
            "https://ws.audioscrobbler.com/2.0/?method=track.getsimilar&artist={}&track={}&autocorrect=1&limit=50&api_key={}&format=json",
            url_encode(seed.artist.split([',', '&']).next().unwrap_or(&seed.artist).trim()),
            url_encode(&title),
            self.key
        );
        parse_lastfm(crate::ytdlp::curl_text(&url).await?.as_bytes())
    }
}

/// The title without brackets, which is how catalogues list a song.
pub(crate) fn normalize_for_lookup(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let t = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() { title.trim().to_string() } else { t }
}

pub(crate) fn parse_lastfm(body: &[u8]) -> Result<Vec<Candidate>> {
    let v: Value = serde_json::from_slice(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    if let Some(msg) = v.get("message").and_then(Value::as_str) {
        return Err(ProviderError::Tool(format!("Last.fm: {msg}")));
    }
    let tracks = v.pointer("/similartracks/track").ok_or_else(|| ProviderError::Parse("no similar tracks".into()))?;
    // One result comes as an object, many as a list.
    let rows: Vec<&Value> = match tracks {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let title = r.get("name")?.as_str()?.to_string();
            let artist = r.pointer("/artist/name")?.as_str()?.to_string();
            let secs = r.get("duration").and_then(|d| d.as_u64().or_else(|| d.as_str()?.parse().ok())).filter(|d| *d > 0);
            Some(Candidate { title, artist, duration_secs: secs.map(|d| d as u32), item: None, source: "lastfm" })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lastfm_answers() {
        let body = br#"{"similartracks":{"track":[
            {"name":"Girl","duration":221,"match":1,"artist":{"name":"F.R. David"}},
            {"name":"No Length","duration":"0","artist":{"name":"X"}},
            {"duration":5}]}}"#;
        let got = parse_lastfm(body).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].title.as_str(), got[0].artist.as_str(), got[0].duration_secs, got[0].source), ("Girl", "F.R. David", Some(221), "lastfm"));
        assert_eq!(got[1].duration_secs, None);
        let one = br#"{"similartracks":{"track":{"name":"Only","artist":{"name":"Solo"}}}}"#;
        assert_eq!(parse_lastfm(one).unwrap().len(), 1);
        let err = parse_lastfm(br#"{"error":6,"message":"Track not found"}"#).unwrap_err();
        assert!(err.to_string().contains("Track not found"));
        assert!(valid_lastfm_key("0123456789abcdef0123456789ABCDEF"));
        assert!(!valid_lastfm_key("short") && !valid_lastfm_key(&"g".repeat(32)));
        assert_eq!(normalize_for_lookup("Song (Official Video) [HD]"), "Song");
        assert_eq!(normalize_for_lookup("(Only Brackets)"), "(Only Brackets)");
    }


    fn item(id: &str, title: &str, artist: &str, secs: Option<u32>) -> Item {
        Item {
            provider: "youtube",
            id: id.into(),
            title: title.into(),
            artist: artist.into(),
            duration_secs: secs,
            url: String::new(),
            thumbnail: None,
        }
    }

    #[test]
    fn one_song_gets_one_key() {
        let a = song_key("Never Gonna Give You Up", "Rick Astley");
        assert_eq!(a, song_key("Never Gonna Give You Up (Official Video)", "Rick Astley"));
        assert_eq!(a, song_key("Never Gonna Give You Up - Remastered 2022", "Rick Astley & Friends"));
        assert_eq!(song_key("Song feat. Someone", "A"), song_key("Song", "A"));
        assert_ne!(a, song_key("Together Forever", "Rick Astley"));
        assert_eq!(normalize("Ça va ? (Live)"), "ça va");
    }

    #[test]
    fn versions_are_recognised() {
        assert!(variants("Alive").is_empty(), "'live' inside another word");
        assert_eq!(variants("Song (Live at Wembley)"), vec!["live"]);
        assert!(variants("Song - Radio Edit").is_empty());
        assert!(variants("Song (Radio Mix)").is_empty());
        assert_eq!(variants("Song (Phil Harding 12\" Mix)"), vec!["remix"]);
        assert!(variants("Song (Sped Up)").contains(&"sped"));
        assert!(variants("Song (Remastered 2011)").is_empty());
        assert!(variants("Song (Instrumental)").contains(&"instrumental"));
    }

    #[test]
    fn picks_the_same_recording_by_title_artist_and_length() {
        let items = vec![
            item("a", "Never Gonna Give You Up (Pianoforte)", "Rick Astley", Some(213)),
            item("b", "Never Gonna Give You Up", "Rick Astley Tribute", Some(213)),
            item("c", "Never Gonna Give You Up", "Rick Astley", Some(300)),
            item("d", "Never Gonna Give You Up (2022 Remaster)", "Rick Astley", Some(213)),
        ];
        let got = pick_match(&items, "Never Gonna Give You Up", "Rick Astley", Some(212)).unwrap();
        assert_eq!(got.id, "d", "same length and artist, remaster is the same recording");
        assert!(pick_match(&items, "Another Song", "Rick Astley", Some(212)).is_none());
        // Without a length, the plain version beats the piano version.
        let got = pick_match(&items[..2], "Never Gonna Give You Up", "Rick Astley", None).unwrap();
        assert_eq!(got.id, "b");
    }

    #[test]
    fn reads_a_radio_queue() {
        let row = |id: &str, title: &str, by: &[&str], len: &str| {
            let runs = |t: &[&str]| t.iter().map(|x| serde_json::json!({ "text": x })).collect::<Vec<_>>();
            serde_json::json!({ "playlistPanelVideoRenderer": {
                "videoId": id,
                "title": { "runs": runs(&[title]) },
                "longBylineText": { "runs": runs(by) },
                "lengthText": { "runs": runs(&[len]) },
            }})
        };
        let body = serde_json::json!({ "contents": { "singleColumnMusicWatchNextResultsRenderer": { "tabbedRenderer": {
            "watchNextTabbedResultsRenderer": { "tabs": [ { "tabRenderer": { "content": { "musicQueueRenderer": { "content": {
                "playlistPanelRenderer": { "contents": [
                    row("seed", "Seed Song", &["Seed Artist", " • ", "1B views"], "3:00"),
                    row("id1", "First", &["A", " & ", "B", " • ", "Album", " • ", "2011"], "3:24"),
                    // A wrapped row, as YouTube sends when a song has an audio and a video version.
                    serde_json::json!({ "playlistPanelVideoWrapperRenderer": { "primaryRenderer": row("id2", "Second", &["C", " • ", "5M views"], "1:02:03") } }),
                    serde_json::json!({ "playlistPanelVideoRenderer": { "title": { "runs": [] } } }),
                ] }
            } } } } } ] } } } } })
        .to_string();
        let got = parse_next(body.as_bytes(), "seed").unwrap();
        assert_eq!(got.iter().map(|c| c.item.as_ref().unwrap().id.as_str()).collect::<Vec<_>>(), ["id1", "id2"]);
        assert_eq!(got[0].artist, "A & B");
        assert_eq!(got[0].duration_secs, Some(204));
        assert_eq!(got[1].duration_secs, Some(3723));
        assert!(parse_next(b"{}", "seed").is_err());
    }

    #[test]
    fn reads_deezer_answers() {
        let search = br#"{"data":[
            {"id":1,"title":"Never Gonna Give You Up (Cake Mix)","duration":347,"artist":{"id":11,"name":"Rick Astley"}},
            {"id":2,"title":"Never Gonna Give You Up","duration":213,"artist":{"id":11,"name":"Rick Astley"}}]}"#;
        assert_eq!(deezer_artist_id(search, "Never Gonna Give You Up", "Rick Astley", Some(213)), Some(11));
        assert_eq!(deezer_artist_id(search, "Unrelated", "Someone Else", None), None);
        let radio = br#"{"data":[{"id":5,"title":"Girl","duration":221,"artist":{"id":9,"name":"F.R. David"}},{"id":6}]}"#;
        let got = parse_deezer_tracks(radio).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].artist.as_str(), got[0].duration_secs, got[0].item.is_none()), ("F.R. David", Some(221), true));
        assert!(parse_deezer_tracks(br#"{"error":{}}"#).is_err());
    }
}
