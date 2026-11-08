//! Manual check against real YouTube.
//! `cargo run -p meowtify-provider --example try -- "<query or link>" <dest dir>`
//! Searches (or resolves a link), prints the results, and downloads the first one.

use std::path::Path;
use std::sync::Arc;

use meowtify_provider::{Provider, Tools, YouTube};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [query, dest] = args.as_slice() else {
        eprintln!("usage: try <query or https link> <dest dir>");
        std::process::exit(2);
    };
    let tools = Tools::locate(&std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("Meowtify").join("tools")).expect("tools");
    println!("yt-dlp: {}\nffmpeg: {:?}", tools.yt_dlp.display(), tools.ffmpeg);
    let yt = YouTube::new(tools);

    let items = if query.starts_with("https://") { yt.resolve(query).await } else { yt.search(query, 5).await };
    let items = items.unwrap_or_else(|e| {
        eprintln!("search failed: {e}");
        std::process::exit(1);
    });
    for (i, it) in items.iter().enumerate() {
        println!("{i}: {} | {} | {:?}s | {}", it.title, it.artist, it.duration_secs, it.url);
    }
    let Some(first) = items.first() else { return };

    let progress = Arc::new(|p: f32| println!("  {p:.0}%"));
    match yt.download(first, Path::new(dest), progress).await {
        Ok(p) => println!("saved: {}", p.display()),
        Err(e) => {
            eprintln!("download failed: {e}");
            std::process::exit(1);
        }
    }
}
