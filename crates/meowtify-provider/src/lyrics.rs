//! Song lyrics from LRCLIB (lrclib.net): free, no key, and many songs have lines with times.
//!
//! Where a song's lyrics come from, in this order:
//! 1. a `.lrc` file next to the song (the user's own, always wins);
//! 2. this app's lyrics cache folder (an earlier answer);
//! 3. LRCLIB, by artist, title, album and length; then a looser search.
//!
//! An answer from LRCLIB is saved in the cache folder, also when LRCLIB has nothing (kept for a
//! week), so opening the lyrics of the same song again asks nobody. The cache holds at most
//! [`CACHE_FILES`] files; the oldest are deleted first.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{ProviderError, Result};

const API: &str = "https://lrclib.net/api";
/// LRCLIB asks every client to say who it is.
const USER_AGENT: &str = concat!("Meowtify/", env!("CARGO_PKG_VERSION"));
const REQUEST_SECS: u32 = 10;
/// Cached answers kept. When there are more, the oldest are deleted down to `CACHE_KEEP`.
const CACHE_FILES: usize = 400;
const CACHE_KEEP: usize = 300;
/// A "no lyrics" answer is asked again after this long: lyrics get added to LRCLIB over time.
const NONE_FRESH: Duration = Duration::from_secs(7 * 24 * 3600);
/// Longest `.lrc` or cached file that is read. Lyrics are a few KB.
const MAX_FILE: u64 = 256 * 1024;
/// How far apart, in seconds, two lengths of the same song may be.
const LENGTH_SLACK: u32 = 3;

/// One line, shown from `at_ms` on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub at_ms: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lyrics {
    /// Lines with the time each one starts, in time order.
    Synced(Vec<Line>),
    /// Lines without times.
    Plain(Vec<String>),
    /// The song has no words.
    Instrumental,
}

/// The song to find lyrics for.
pub struct Query<'a> {
    pub title: &'a str,
    /// The main artist.
    pub artist: &'a str,
    pub album: Option<&'a str>,
    /// 0 when not known.
    pub duration_secs: u32,
    /// The song's file, so a `.lrc` next to it is found.
    pub audio: Option<&'a Path>,
}

/// Reads LRC text: `[mm:ss.xx] words`, with one or more times per line. Lines without a time,
/// such as `[ar:Artist]` tags, are skipped. Gives `None` when no line has a time.
pub fn parse_lrc(text: &str) -> Option<Vec<Line>> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim_start_matches('\u{feff}').trim();
        let mut times = Vec::new();
        while let Some(inner) = rest.strip_prefix('[') {
            let Some(end) = inner.find(']') else { break };
            match parse_time(&inner[..end]) {
                Some(ms) => times.push(ms),
                None => break,
            }
            rest = inner[end + 1..].trim_start();
        }
        let text = strip_word_times(rest);
        for at_ms in times {
            lines.push(Line { at_ms, text: text.clone() });
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.sort_by_key(|l| l.at_ms); // stable: equal times keep their order
    Some(lines)
}

/// Takes out the per-word times some files add inside a line, such as `<00:18.89>We're <00:19.25>no`.
/// The line is shown whole, so they are not used.
fn strip_word_times(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        match after.find('>') {
            Some(close) if parse_time(&after[..close]).is_some() => {
                out.push_str(&rest[..open]);
                rest = &after[close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// `mm:ss`, `mm:ss.x`, `mm:ss.xx` or `mm:ss.xxx` as milliseconds.
fn parse_time(tag: &str) -> Option<u32> {
    let (min, rest) = tag.split_once(':')?;
    let (sec, frac) = match rest.split_once(['.', ':']) {
        Some((s, f)) => (s, f),
        None => (rest, ""),
    };
    let digits = |s: &str| !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(min) || !digits(sec) || !(frac.is_empty() || digits(frac)) {
        return None;
    }
    let millis = match frac.len() {
        0 => 0,
        1 => frac.parse::<u32>().ok()? * 100,
        2 => frac.parse::<u32>().ok()? * 10,
        _ => frac.parse::<u32>().ok()?,
    };
    Some((min.parse::<u32>().ok()? * 60 + sec.parse::<u32>().ok()?) * 1000 + millis)
}

fn from_text(text: &str) -> Lyrics {
    match parse_lrc(text) {
        Some(lines) => Lyrics::Synced(lines),
        None => Lyrics::Plain(text.lines().map(|l| l.trim_end().to_string()).collect()),
    }
}

#[derive(Deserialize)]
struct Hit {
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    instrumental: bool,
    #[serde(default, rename = "plainLyrics")]
    plain: Option<String>,
    #[serde(default, rename = "syncedLyrics")]
    synced: Option<String>,
}

impl Hit {
    fn lyrics(&self) -> Option<Lyrics> {
        if self.instrumental {
            return Some(Lyrics::Instrumental);
        }
        if let Some(lines) = self.synced.as_deref().and_then(parse_lrc) {
            return Some(Lyrics::Synced(lines));
        }
        self.plain.as_deref().filter(|p| !p.trim().is_empty()).map(|p| from_text(p))
    }

    fn has_times(&self) -> bool {
        self.synced.as_deref().is_some_and(|s| parse_lrc(s).is_some())
    }

    fn near(&self, secs: u32) -> bool {
        secs == 0 || self.duration <= 0.0 || (self.duration - f64::from(secs)).abs() <= f64::from(LENGTH_SLACK)
    }
}

/// The best of several search hits: the right length, lines with times first.
fn best(hits: Vec<Hit>, secs: u32) -> Option<Lyrics> {
    let mut fit: Vec<&Hit> = hits.iter().filter(|h| h.near(secs) && h.lyrics().is_some()).collect();
    fit.sort_by_key(|h| std::cmp::Reverse(h.has_times()));
    fit.first().and_then(|h| h.lyrics())
}

/// Percent-encodes a query value.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether a curl failure was LRCLIB answering "no such song".
fn is_not_found(e: &ProviderError) -> bool {
    matches!(e, ProviderError::Tool(m) if m.contains("404"))
}

async fn ask(query: &str) -> Result<Vec<u8>> {
    crate::ytdlp::curl_get(&format!("{API}/{query}"), USER_AGENT, REQUEST_SECS).await
}

/// Asks LRCLIB. `Ok(None)` means it has nothing for this song.
async fn lrclib(q: &Query<'_>) -> Result<Option<Lyrics>> {
    let (artist, title) = (encode(q.artist), encode(q.title));
    if q.duration_secs > 0 {
        let album = q.album.map(|a| format!("&album_name={}", encode(a))).unwrap_or_default();
        let exact = format!("get?artist_name={artist}&track_name={title}{album}&duration={}", q.duration_secs);
        match ask(&exact).await {
            Ok(body) => {
                let hit: Hit = serde_json::from_slice(&body).map_err(|e| ProviderError::Parse(e.to_string()))?;
                if let Some(found) = hit.lyrics() {
                    return Ok(Some(found));
                }
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e),
        }
    }
    // The exact lookup missed (a different album, a slightly different length): search.
    let body = match ask(&format!("search?artist_name={artist}&track_name={title}")).await {
        Ok(b) => b,
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let hits: Vec<Hit> = serde_json::from_slice(&body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    Ok(best(hits, q.duration_secs))
}

/// The lyrics of a song, from the cheapest source that has them. `cache` is the folder for
/// answers; it is made when needed. `Ok(None)` means there are none; `Err` means the network
/// failed, which is not cached, so it is tried again next time.
pub async fn find(q: &Query<'_>, cache: &Path) -> Result<Option<Lyrics>> {
    if let Some(own) = q.audio.and_then(sidecar) {
        return Ok(Some(own));
    }
    let key = cache_key(q);
    if let Some(cached) = read_cache(cache, &key) {
        return Ok(cached);
    }
    let found = lrclib(q).await?;
    write_cache(cache, &key, found.as_ref());
    Ok(found)
}

/// The `.lrc` or `.txt` next to a song, when there is one.
fn sidecar(audio: &Path) -> Option<Lyrics> {
    for ext in ["lrc", "txt"] {
        let file = audio.with_extension(ext);
        if std::fs::metadata(&file).ok().is_some_and(|m| m.is_file() && m.len() <= MAX_FILE) {
            if let Ok(text) = std::fs::read_to_string(&file) {
                if !text.trim().is_empty() {
                    return Some(from_text(&text));
                }
            }
        }
    }
    None
}

/// Names a song's cached answer: the same recording under another upload name has the same key.
fn cache_key(q: &Query<'_>) -> String {
    let song = crate::song_key(q.title, q.artist);
    let hash = Sha256::digest(song.as_bytes());
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn cache_file(cache: &Path, key: &str, kind: &str) -> PathBuf {
    cache.join(format!("{key}.{kind}"))
}

/// `Some(answer)` when the cache has one, where `Some(None)` is "LRCLIB had nothing".
fn read_cache(cache: &Path, key: &str) -> Option<Option<Lyrics>> {
    let read = |kind: &str| {
        let file = cache_file(cache, key, kind);
        let meta = std::fs::metadata(&file).ok()?;
        (meta.len() <= MAX_FILE).then(|| (std::fs::read_to_string(&file).unwrap_or_default(), meta.modified().ok()))
    };
    if let Some((text, _)) = read("lrc") {
        return Some(parse_lrc(&text).map(Lyrics::Synced));
    }
    if let Some((text, _)) = read("txt") {
        return Some(Some(from_text(&text)));
    }
    if read("inst").is_some() {
        return Some(Some(Lyrics::Instrumental));
    }
    if let Some((_, modified)) = read("none") {
        let fresh = modified.and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|age| age < NONE_FRESH);
        if fresh {
            return Some(None);
        }
        let _ = std::fs::remove_file(cache_file(cache, key, "none"));
    }
    None
}

fn write_cache(cache: &Path, key: &str, found: Option<&Lyrics>) {
    if std::fs::create_dir_all(cache).is_err() {
        return;
    }
    let (kind, text) = match found {
        Some(Lyrics::Synced(lines)) => ("lrc", lines.iter().map(|l| format!("[{}] {}", clock(l.at_ms), l.text)).collect::<Vec<_>>().join("\n")),
        Some(Lyrics::Plain(lines)) => ("txt", lines.join("\n")),
        Some(Lyrics::Instrumental) => ("inst", String::new()),
        None => ("none", String::new()),
    };
    let _ = std::fs::write(cache_file(cache, key, kind), text);
    trim_cache(cache);
}

/// `mm:ss.xx`, the form [`parse_lrc`] reads back.
fn clock(ms: u32) -> String {
    format!("{:02}:{:02}.{:02}", ms / 60_000, ms / 1000 % 60, ms % 1000 / 10)
}

/// Keeps the cache under [`CACHE_FILES`] files by deleting the oldest.
fn trim_cache(cache: &Path) {
    let Ok(dir) = std::fs::read_dir(cache) else { return };
    let mut files: Vec<(SystemTime, PathBuf)> =
        dir.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
    if files.len() <= CACHE_FILES {
        return;
    }
    files.sort();
    for (_, path) in files.iter().take(files.len() - CACHE_KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("meowtify-lyrics-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn lrc_text_becomes_timed_lines() {
        let lines = parse_lrc("[ar:Someone]\n[00:12.50] Hello\n[01:02.5]World\n[00:05]\n[00:30.123][01:10.00] Twice").unwrap();
        let got: Vec<(u32, &str)> = lines.iter().map(|l| (l.at_ms, l.text.as_str())).collect();
        assert_eq!(got, [(5_000, ""), (12_500, "Hello"), (30_123, "Twice"), (62_500, "World"), (70_000, "Twice")]);
        assert!(parse_lrc("just words\nno times").is_none());
        let words = parse_lrc("[00:18.89] <00:18.89>We're <00:19.25>no <00:19.53>strangers").unwrap();
        assert_eq!(words[0].text, "We're no strangers", "per-word times are not part of the text");
        assert_eq!(parse_lrc("[00:01.00] a <b> c < 3 > 2").unwrap()[0].text, "a <b> c < 3 > 2", "other angle brackets stay");
        assert!(parse_lrc("[00:99999.00] bad").is_none(), "a time with too many digits is not a time");
    }

    #[test]
    fn a_hit_prefers_timed_lines_and_the_right_length() {
        let hit = |dur: f64, synced: Option<&str>, plain: Option<&str>| Hit {
            duration: dur,
            instrumental: false,
            plain: plain.map(String::from),
            synced: synced.map(String::from),
        };
        let hits = vec![hit(300.0, Some("[00:01.00] wrong length"), None), hit(200.0, None, Some("plain")), hit(201.0, Some("[00:02.00] right"), Some("right"))];
        match best(hits, 200) {
            Some(Lyrics::Synced(l)) => assert_eq!(l[0].text, "right"),
            other => panic!("{other:?}"),
        }
        assert_eq!(best(vec![hit(100.0, None, Some("x"))], 200), None, "no hit of the right length");
        assert_eq!(best(vec![hit(0.0, None, Some("x"))], 200), Some(Lyrics::Plain(vec!["x".into()])), "an unknown length is accepted");
        let inst = Hit { duration: 200.0, instrumental: true, plain: None, synced: None };
        assert_eq!(best(vec![inst], 200), Some(Lyrics::Instrumental));
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(encode("Beyoncé & Jay-Z"), "Beyonc%C3%A9%20%26%20Jay-Z");
        assert_eq!(encode("a/b?c=d"), "a%2Fb%3Fc%3Dd");
    }

    #[test]
    fn answers_are_cached_and_read_back() {
        let d = dir("cache");
        let q = Query { title: "Song (Remastered)", artist: "A, B", album: None, duration_secs: 200, audio: None };
        let key = cache_key(&q);
        assert_eq!(read_cache(&d, &key), None, "nothing cached yet");
        let timed = Lyrics::Synced(vec![Line { at_ms: 1_230, text: "hi".into() }, Line { at_ms: 65_000, text: "bye".into() }]);
        write_cache(&d, &key, Some(&timed));
        assert_eq!(read_cache(&d, &key), Some(Some(timed)));
        // The same recording under another upload name finds the same file.
        let other = Query { title: "Song", artist: "A", album: Some("x"), duration_secs: 0, audio: None };
        assert_eq!(cache_key(&other), key);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_lyrics_is_remembered_for_a_while_only() {
        let d = dir("none");
        write_cache(&d, "k", None);
        assert_eq!(read_cache(&d, "k"), Some(None));
        let old = SystemTime::now() - NONE_FRESH - Duration::from_secs(60);
        std::fs::File::options().write(true).open(d.join("k.none")).unwrap().set_modified(old).unwrap();
        assert_eq!(read_cache(&d, "k"), None, "an old no is asked again");
        assert!(!d.join("k.none").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_cache_never_holds_more_than_its_limit() {
        let d = dir("trim");
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..CACHE_FILES + 30 {
            std::fs::write(d.join(format!("{i:04}.txt")), "x").unwrap();
        }
        trim_cache(&d);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), CACHE_KEEP);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Needs the network: `cargo test -p meowtify-provider lyrics::tests::live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_lookup_finds_a_known_song_then_answers_from_the_cache() {
        let d = dir("live");
        let q = Query { title: "Never Gonna Give You Up", artist: "Rick Astley", album: None, duration_secs: 213, audio: None };
        let first = find(&q, &d).await.expect("network");
        println!("first: {:?}", first.as_ref().map(|l| match l {
            Lyrics::Synced(v) => format!("synced, {} lines, first {:?}", v.len(), v.first()),
            Lyrics::Plain(v) => format!("plain, {} lines", v.len()),
            Lyrics::Instrumental => "instrumental".to_string(),
        }));
        assert!(first.is_some());
        assert!(std::fs::read_dir(&d).unwrap().count() >= 1, "the answer was cached");
        assert_eq!(find(&q, &d).await.unwrap().is_some(), true);
        let none = Query { title: "Qqqzzz Nonexistent Song", artist: "Zzzqqq Nobody", album: None, duration_secs: 100, audio: None };
        assert_eq!(find(&none, &d).await.expect("network"), None);
        assert!(std::fs::read_dir(&d).unwrap().any(|e| e.unwrap().path().extension().is_some_and(|x| x == "none")), "a miss is cached too");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_lrc_file_next_to_the_song_wins() {
        let d = dir("sidecar");
        std::fs::create_dir_all(&d).unwrap();
        let song = d.join("Song [abcdefghijk].m4a");
        assert!(sidecar(&song).is_none());
        std::fs::write(d.join("Song [abcdefghijk].lrc"), "[00:01.00] mine").unwrap();
        assert_eq!(sidecar(&song), Some(Lyrics::Synced(vec![Line { at_ms: 1_000, text: "mine".into() }])));
        let _ = std::fs::remove_dir_all(&d);
    }
}
