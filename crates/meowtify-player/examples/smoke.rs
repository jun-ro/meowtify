//! Manual check: plays the first track of a folder for a few seconds and
//! prints the position. `cargo run -p meowtify-player --example smoke -- <music dir>`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use meowtify_core::PlaybackEngine;
use meowtify_library::Library;
use meowtify_core::Selection;
use meowtify_player::LocalPlayer;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: smoke <music dir>"));
    let library = Arc::new(Library::new(Some(dir)));
    println!("tracks: {}", library.rescan_blocking());
    let player = LocalPlayer::new(library.clone());

    let first = library.tracks().first().cloned().expect("no tracks");
    println!("playing: {} - {}", first.artists[0].name, first.name);
    player.play_from(&Selection::Library(first.uri.clone())).await.expect("play_from");
    player.set_volume(10).await.unwrap();

    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let s = player.playback_state().await.unwrap().expect("state");
        println!("playing={} position={} ms", s.is_playing, s.progress_ms);
    }
    player.seek(100_000).await.expect("seek");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = player.playback_state().await.unwrap().unwrap();
    println!("after seek: position={} ms", s.progress_ms);
    player.pause().await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let s = player.playback_state().await.unwrap().unwrap();
    println!("after pause: playing={}", s.is_playing);
}
