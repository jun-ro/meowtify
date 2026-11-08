//! Test plugin. Behaviour is chosen by the track title.

use std::hint::black_box;

wit_bindgen::generate!({ path: "../wit/v0.1", world: "plugin" });

use meowtify::plugin::host;
use meowtify::plugin::types::{LogLevel, LyricLine};

struct P;

fn say(msg: String) {
    host::log(LogLevel::Info, &msg);
}

impl Guest for P {
    fn on_track_change(t: Track) {
        match t.title.as_str() {
            "spin" => {
                let mut i = 0u64;
                loop {
                    i = black_box(i.wrapping_add(1));
                }
            }
            "hog" => {
                let mut keep: Vec<Vec<u8>> = Vec::new();
                loop {
                    keep.push(black_box(vec![1u8; 1 << 20]));
                }
            }
            "probe" => {
                let ok = |r: Result<(), host::HostError>| r.map(|_| "ok").map_err(|e| e.kind);
                say(format!("play={:?}", ok(host::play())));
                say(format!("volume={:?}", ok(host::set_volume(101))));
                say(format!("storage={:?}", ok(host::storage_set("k", b"v"))));
                let back = host::storage_get("k").map(|v| v.map(|b| b.len())).map_err(|e| e.kind);
                say(format!("storage_get={back:?}"));
                let f = |u: &str| host::fetch(u).map(|r| r.status).map_err(|e| e.kind);
                say(format!("fetch_ok={:?}", f("https://a.example/x")));
                say(format!("fetch_evil={:?}", f("https://evil.example/x")));
                say(format!("fetch_bad={:?}", f("ftp://a.example/x")));
            }
            other => say(format!("track {other}")),
        }
    }

    fn on_progress(position_ms: u32) {
        say(format!("progress {position_ms}"));
    }

    fn on_queue_change(queue: Vec<Track>) {
        say(format!("queue {}", queue.len()));
    }

    fn get_lyrics(t: Track) -> Option<Lyrics> {
        if t.title == "none" {
            return None;
        }
        Some(Lyrics {
            lines: vec![
                LyricLine { start_ms: 0, text: format!("{} by {}", t.title, t.artist) },
                LyricLine { start_ms: 1000, text: "第二行 שלום".to_string() },
            ],
            source: Some("test".to_string()),
        })
    }
}

export!(P);
