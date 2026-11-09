//! Try the in-process providers without the app.
//!
//! ```text
//! cargo run -p meowtify-provider --features native-extract --example native_try -- search "tsubi club"
//! cargo run -p meowtify-provider --features native-extract --example native_try -- search sc "blinding lights"
//! cargo run -p meowtify-provider --features native-extract --example native_try -- download "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
//! ```
//!
//! A download is checked with Symphonia, the decoder the app uses, so a file is only
//! reported good when the player can actually open it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use meowtify_provider::{native_providers, Item, Provider};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

fn probe(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("the file has no audio track")?;
    let params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default())?;
    let packet = format.next_packet()?;
    let decoded = decoder.decode(&packet)?;
    println!(
        "decoded: codec={:?} frames={} rate={:?} channels={}",
        params.codec,
        decoded.frames(),
        params.sample_rate,
        decoded.spec().channels.count()
    );
    Ok(())
}

async fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("usage: native_try search [youtube|sc|audius] <query> | resolve <url> | download <url|id|sc:id|audius:id> [folder]");
        return Ok(());
    };
    let (youtube, fallbacks) = native_providers();
    match command {
        "resolve" => {
            let target = args.get(1).ok_or("resolve needs a link")?;
            let provider: Arc<dyn Provider> = if target.contains("soundcloud.com") {
                fallbacks[0].clone()
            } else if target.contains("audius.co") {
                fallbacks[1].clone()
            } else {
                youtube.clone()
            };
            let items = provider.resolve(target).await?;
            println!("{} results", items.len());
            for (i, item) in items.iter().enumerate() {
                println!("{:2}. {} — {} [{}] {}", i + 1, item.title, if item.artist.is_empty() { "?" } else { &item.artist }, item.id, item.url);
            }
            Ok(())
        }
        "search" => {
            let (which, query) = match args.get(1).map(String::as_str) {
                Some("sc" | "soundcloud") => ("soundcloud", args.get(2)),
                Some("audius") => ("audius", args.get(2)),
                Some(_) => ("youtube", args.get(1)),
                None => ("youtube", None),
            };
            let query = query.ok_or("search needs a query")?;
            let provider: Arc<dyn Provider> = if which == "soundcloud" {
                fallbacks[0].clone()
            } else if which == "audius" {
                fallbacks[1].clone()
            } else {
                youtube.clone()
            };
            let items = provider.search(query, 10).await?;
            println!("{} results from {which}", items.len());
            for (i, item) in items.iter().enumerate() {
                println!(
                    "{:2}. {} — {} ({}) [{}] {}",
                    i + 1,
                    item.title,
                    if item.artist.is_empty() { "?" } else { &item.artist },
                    item.duration_secs.map(|d| format!("{}:{:02}", d / 60, d % 60)).unwrap_or_else(|| "?".into()),
                    item.id,
                    item.url
                );
            }
            Ok(())
        }
        "download" => {
            let target = args.get(1).ok_or("download needs a link or a video id")?;
            let dir = PathBuf::from(
                args.get(2)
                    .cloned()
                    .unwrap_or_else(|| std::env::temp_dir().join("meowtify-native-try").to_string_lossy().into_owned()),
            );
            std::fs::create_dir_all(&dir)?;
            // A pasted link picks its own provider; a bare id is a YouTube video.
            let (provider, item): (Arc<dyn Provider>, Item) = if let Some(id) = target.strip_prefix("sc:") {
                let item = Item {
                    provider: "soundcloud",
                    id: id.to_string(),
                    title: format!("soundcloud {id}"),
                    artist: String::new(),
                    duration_secs: None,
                    url: String::new(),
                    thumbnail: None,
                };
                (fallbacks[0].clone(), item)
            } else if let Some(id) = target.strip_prefix("audius:") {
                let item = Item {
                    provider: "audius",
                    id: id.to_string(),
                    title: format!("audius {id}"),
                    artist: String::new(),
                    duration_secs: None,
                    url: String::new(),
                    thumbnail: None,
                };
                (fallbacks[1].clone(), item)
            } else {
                let provider: Arc<dyn Provider> = if target.contains("soundcloud.com") {
                    fallbacks[0].clone()
                } else if target.contains("audius.co") {
                    fallbacks[1].clone()
                } else {
                    youtube.clone()
                };
                let item = if target.starts_with("http") {
                    let mut items = provider.resolve(target).await?;
                    if items.is_empty() {
                        return Err("the link named nothing".into());
                    }
                    items.remove(0)
                } else {
                    Item {
                        provider: "youtube",
                        id: target.to_string(),
                        title: target.to_string(),
                        artist: String::new(),
                        duration_secs: None,
                        url: format!("https://www.youtube.com/watch?v={target}"),
                        thumbnail: None,
                    }
                };
                (provider, item)
            };
            println!("downloading {} …", item.title);
            let path = provider
                .download(&item, &dir, Arc::new(|p| {
                    if (p as i32) % 10 == 0 {
                        eprintln!("{p:.0}%");
                    }
                }))
                .await?;
            let size = std::fs::metadata(&path)?.len();
            println!("saved {size} bytes to {}", path.display());
            probe(&path)?;
            Ok(())
        }
        other => Err(format!("unknown command: {other}").into()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(run(args))
}
