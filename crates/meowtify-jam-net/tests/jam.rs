//! A whole jam in one process: a relay on a local port, a host serving a real file, and a guest
//! that receives it, follows the host's state, and asks for things.

use std::path::PathBuf;
use std::time::Duration;

use meowtify_jam::follower::{Local, Step};
use meowtify_jam::proto::{Action, Perms};
use meowtify_jam_net::{Guest, GuestConfig, GuestEvent, Host, HostConfig, HostEvent, IceConfig, LocalCopy, Options, RelayConfig, Snapshot, SongInfo};
use tokio::sync::mpsc::UnboundedReceiver;

const KEY: &str = "0123456789abcdef0123456789";

fn link() -> Options {
    Options { ice: IceConfig { servers: vec![], relay_only: false }, bind: vec!["127.0.0.1:0".into()] }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meowtify-jam-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn relay() -> String {
    let listener = meowtify_jam_relay::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(meowtify_jam_relay::serve(listener, Default::default()));
    format!("ws://{addr}")
}

fn song(path: &PathBuf, title: &str) -> SongInfo {
    SongInfo { path: path.clone(), title: title.into(), artist: "Artist".into(), duration_ms: 200_000, key: None }
}

/// A song file of a size that takes many chunks.
fn song_file(dir: &PathBuf, name: &str, len: usize) -> (PathBuf, Vec<u8>) {
    let bytes: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
    let path = dir.join(name);
    std::fs::write(&path, &bytes).unwrap();
    (path, bytes)
}

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), f).await.unwrap_or_else(|_| panic!("timed out: {what}"))
}

async fn host_until(events: &mut UnboundedReceiver<HostEvent>, what: &str, pick: impl Fn(&HostEvent) -> bool) -> HostEvent {
    within(what, async {
        loop {
            let e = events.recv().await.expect("host alive");
            if pick(&e) {
                return e;
            }
        }
    })
    .await
}

async fn guest_until(events: &mut UnboundedReceiver<GuestEvent>, what: &str, pick: impl Fn(&GuestEvent) -> bool) -> GuestEvent {
    within(what, async {
        loop {
            let e = events.recv().await.expect("guest alive");
            if pick(&e) {
                return e;
            }
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guest_gets_the_song_follows_the_host_and_is_held_to_its_rights() {
    let url = relay().await;
    let dir = temp_dir("jam");
    let (path, bytes) = song_file(&dir, "a.m4a", 300_000);
    let (path2, _) = song_file(&dir, "b.m4a", 40_000);

    let (host, mut host_events) = Host::start(meowtify_jam_net::HostConfig {
        relay: RelayConfig { url: url.clone(), code: "testjam1".into(), name: "Hana".into(), host_key: Some(KEY.into()), hue: None },
        link: link(),
    });
    // The room exists once the host has been welcomed; a guest cannot join before that.
    host_until(&mut host_events, "the room", |e| matches!(e, HostEvent::Members(_))).await;
    // Playing before anyone joined: a late guest must still get the position.
    host.publish(Snapshot { current: Some(song(&path, "First")), playing: true, pos_ms: 5_000, queue: vec![song(&path2, "Second")] });

    let cache = dir.join("guest-cache");
    let (guest, mut guest_events) = Guest::join(GuestConfig {
        relay: RelayConfig { url, code: "testjam1".into(), name: "Ann".into(), host_key: None, hue: None },
        link: link(),
        cache_dir: cache.clone(),
        local_copy: None,
    });

    // The guest hears the state, and is told to play the file once all of it has arrived.
    let state = guest_until(&mut guest_events, "state", |e| matches!(e, GuestEvent::State(s) if s.song.is_some())).await;
    let GuestEvent::State(state) = state else { unreachable!() };
    assert_eq!(state.song.as_ref().unwrap().title, "First");
    assert_eq!(state.queue.len(), 1);

    let play = guest_until(&mut guest_events, "play step", |e| matches!(e, GuestEvent::Do(Step::Play { .. }))).await;
    let GuestEvent::Do(Step::Play { path: cached, pos_ms, playing, .. }) = play else { unreachable!() };
    assert!(playing);
    assert!(pos_ms >= 5_000 && pos_ms < 15_000, "started near where the host is: {pos_ms}");
    assert_eq!(std::fs::read(&cached).unwrap(), bytes, "the file arrived whole and unchanged");
    assert!(!cached.contains(path.to_str().unwrap()), "a guest never gets the host's path");

    // The host sees the guest, linked, with the default rights.
    let HostEvent::Members(members) =
        host_until(&mut host_events, "linked guest", |e| matches!(e, HostEvent::Members(m) if m.iter().any(|x| x.linked))).await
    else {
        unreachable!()
    };
    let ann = members[0].clone();
    assert_eq!((ann.name.as_str(), ann.perms), ("Ann", Perms::LISTENER));

    // A listener may suggest, and may not pause.
    guest.request(Action::Pause);
    guest_until(&mut guest_events, "denial", |e| matches!(e, GuestEvent::Denied(_))).await;
    guest.request(Action::Suggest { text: "tsubi club".into() });
    host_until(&mut host_events, "suggestion", |e| matches!(e, HostEvent::Suggest { from, text } if from == "Ann" && text == "tsubi club")).await;

    // With the right, the same request goes through.
    host.set_perms(ann.id, Perms { control: true, ..Perms::LISTENER });
    guest_until(&mut guest_events, "new rights", |e| matches!(e, GuestEvent::Perms(p) if p.control)).await;
    guest.request(Action::Pause);
    host_until(&mut host_events, "pause", |e| matches!(e, HostEvent::Do { action: Action::Pause, .. })).await;

    // The host pauses: the guest is told to pause too, once its player has settled.
    host.publish(Snapshot { current: Some(song(&path, "First")), playing: false, pos_ms: 9_000, queue: vec![] });
    guest.report(Local { path: Some(cached.clone()), pos_ms: 9_000, playing: true });
    guest_until(&mut guest_events, "pause step", |e| matches!(e, GuestEvent::Do(Step::Pause))).await;

    // Leaving removes the songs from the disk.
    drop(guest);
    assert!(std::fs::read_dir(&cache).map(|d| d.flatten().count()).unwrap_or(0) == 0, "the cache is cleaned up");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_host_can_remove_a_guest_and_end_the_jam() {
    let url = relay().await;
    let dir = temp_dir("kick");
    let (host, mut host_events) = Host::start(HostConfig {
        relay: RelayConfig { url: url.clone(), code: "testjam2".into(), name: "Hana".into(), host_key: Some(KEY.into()), hue: None },
        link: link(),
    });
    host_until(&mut host_events, "the room", |e| matches!(e, HostEvent::Members(_))).await;
    let join = |name: &str, sub: &str| {
        Guest::join(GuestConfig {
            relay: RelayConfig { url: url.clone(), code: "testjam2".into(), name: name.into(), host_key: None, hue: None },
            link: link(),
            cache_dir: dir.join(sub),
            local_copy: None,
        })
    };
    let (_ann, mut ann_events) = join("Ann", "a");
    let (_bo, mut bo_events) = join("Bo", "b");
    let HostEvent::Members(members) =
        host_until(&mut host_events, "two linked guests", |e| matches!(e, HostEvent::Members(m) if m.len() == 2 && m.iter().all(|x| x.linked))).await
    else {
        unreachable!()
    };

    host.kick(members.iter().find(|m| m.name == "Ann").unwrap().id);
    let GuestEvent::Down(why) = guest_until(&mut ann_events, "removal", |e| matches!(e, GuestEvent::Down(_))).await else { unreachable!() };
    assert!(why.contains("removed"), "{why}");

    host.end();
    guest_until(&mut bo_events, "the end", |e| matches!(e, GuestEvent::Down(_))).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guest_plays_its_own_copy_and_only_receives_the_songs_it_lacks() {
    let url = relay().await;
    let dir = temp_dir("own");
    let (path, _) = song_file(&dir, "a.m4a", 120_000);
    let (path2, bytes2) = song_file(&dir, "b.m4a", 60_000);
    // The guest has the first song already, under its own name.
    let (own, _) = song_file(&dir, "mine.m4a", 90_000);

    let (host, mut host_events) = Host::start(HostConfig {
        relay: RelayConfig { url: url.clone(), code: "testjam3".into(), name: "Hana".into(), host_key: Some(KEY.into()), hue: None },
        link: link(),
    });
    host_until(&mut host_events, "the room", |e| matches!(e, HostEvent::Members(_))).await;
    let mut first = song(&path, "First");
    first.key = Some("abc123".into());
    host.publish(Snapshot { current: Some(first), playing: true, pos_ms: 1_000, queue: vec![song(&path2, "Second")] });

    let cache = dir.join("guest-cache");
    let mine = own.clone();
    let (_guest, mut guest_events) = Guest::join(GuestConfig {
        relay: RelayConfig { url, code: "testjam3".into(), name: "Ann".into(), host_key: None, hue: None },
        link: link(),
        cache_dir: cache.clone(),
        local_copy: Some(LocalCopy(std::sync::Arc::new(move |s| (s.key.as_deref() == Some("abc123")).then(|| mine.clone())))),
    });

    let GuestEvent::Do(Step::Play { path: played, .. }) = guest_until(&mut guest_events, "play step", |e| matches!(e, GuestEvent::Do(Step::Play { .. }))).await else {
        unreachable!()
    };
    assert_eq!(PathBuf::from(&played), own, "the guest plays its own file");

    // The second song is not in the guest's library: it is asked for and arrives whole.
    let got = within("the second song", async {
        loop {
            let done = std::fs::read_dir(&cache).ok().and_then(|d| d.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "audio")));
            if let Some(p) = done {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert_eq!(std::fs::read(&got).unwrap(), bytes2);
    let cached = std::fs::read_dir(&cache).unwrap().flatten().count();
    assert_eq!(cached, 1, "only the song that was missing was received");
    assert!(own.exists(), "the guest's own file is left alone");
    let _ = std::fs::remove_dir_all(&dir);
}

/// With "hide my IP" on, both sides gather only TURN candidates, and the link still opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_jam_connects_through_turn_alone() {
    let turn = meowtify_jam_relay::turn_server::Turn::start("127.0.0.1".parse().unwrap(), 34781).await.unwrap();
    let url = relay().await;
    let opts = || Options { ice: IceConfig::from_json(&turn.ice_servers()).unwrap().hiding_ip(), bind: vec!["127.0.0.1:0".into()] };

    let (_host, mut host_events) = Host::start(HostConfig {
        relay: RelayConfig { url: url.clone(), code: "testturn1".into(), name: "Hana".into(), host_key: Some(KEY.into()), hue: None },
        link: opts(),
    });
    host_until(&mut host_events, "the room", |e| matches!(e, HostEvent::Members(_))).await;
    let dir = temp_dir("turn");
    let (_guest, _guest_events) = Guest::join(GuestConfig {
        relay: RelayConfig { url, code: "testturn1".into(), name: "Ann".into(), host_key: None, hue: None },
        link: opts(),
        cache_dir: dir.join("cache"),
        local_copy: None,
    });
    host_until(&mut host_events, "a linked guest", |e| matches!(e, HostEvent::Members(m) if m.iter().any(|x| x.linked))).await;
}

/// The same jam with TURN-only and no TURN server must not connect: nothing falls back to a
/// direct path that would show the address.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_only_never_falls_back_to_a_direct_path() {
    let url = relay().await;
    let opts = || Options { ice: IceConfig { servers: vec![], relay_only: true }, bind: vec!["127.0.0.1:0".into()] };
    let (_host, mut host_events) = Host::start(HostConfig {
        relay: RelayConfig { url: url.clone(), code: "testturn2".into(), name: "Hana".into(), host_key: Some(KEY.into()), hue: None },
        link: opts(),
    });
    host_until(&mut host_events, "the room", |e| matches!(e, HostEvent::Members(_))).await;
    let dir = temp_dir("turn-none");
    let (_guest, _guest_events) = Guest::join(GuestConfig {
        relay: RelayConfig { url, code: "testturn2".into(), name: "Ann".into(), host_key: None, hue: None },
        link: opts(),
        cache_dir: dir.join("cache"),
        local_copy: None,
    });
    let linked = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(HostEvent::Members(m)) = host_events.recv().await {
                if m.iter().any(|x| x.linked) {
                    return;
                }
            }
        }
    })
    .await;
    assert!(linked.is_err(), "a direct path was used");
}
