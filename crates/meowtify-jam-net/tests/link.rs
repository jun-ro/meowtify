//! Two peers in one process find each other over the loopback interface and talk.

use std::time::Duration;

use meowtify_jam_net::{Event, Link, Options, CHUNK};

fn opts() -> Options {
    // No STUN on a loopback test: the host candidates are enough, and nothing leaves the machine.
    Options { ice: meowtify_jam_net::IceConfig { servers: vec![], relay_only: false }, bind: vec!["127.0.0.1:0".into()] }
}

/// Moves ICE candidates between the two sides until both report `Open`. Other events are kept.
async fn connect(host: &mut Link, guest: &mut Link) -> (Vec<Event>, Vec<Event>) {
    let (mut host_seen, mut guest_seen) = (Vec::new(), Vec::new());
    let (mut host_open, mut guest_open) = (false, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !(host_open && guest_open) {
        tokio::select! {
            e = host.next() => match e.expect("host link alive") {
                Event::Candidate(c) => guest.add_candidate(&c).await.unwrap(),
                Event::Open => host_open = true,
                other => host_seen.push(other),
            },
            e = guest.next() => match e.expect("guest link alive") {
                Event::Candidate(c) => host.add_candidate(&c).await.unwrap(),
                Event::Open => guest_open = true,
                other => guest_seen.push(other),
            },
            _ = tokio::time::sleep_until(deadline) => panic!("the peers did not connect in time"),
        }
    }
    (host_seen, guest_seen)
}

async fn next_matching(link: &mut Link, pick: impl Fn(&Event) -> bool) -> Event {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), link.next()).await.expect("an event in time").expect("alive");
        if pick(&e) {
            return e;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_and_a_guest_connect_and_exchange_control_and_audio() {
    let (mut host, offer) = Link::offer(opts()).await.unwrap();
    let (mut guest, answer) = Link::answer(opts(), &offer).await.unwrap();
    host.set_answer(&answer).await.unwrap();
    connect(&mut host, &mut guest).await;

    // Control, both ways.
    host.send_control(b"{\"hello\":\"guest\"}").await.unwrap();
    assert_eq!(next_matching(&mut guest, |e| matches!(e, Event::Control(_))).await, Event::Control(b"{\"hello\":\"guest\"}".to_vec()));
    guest.send_control(b"{\"hello\":\"host\"}").await.unwrap();
    assert_eq!(next_matching(&mut host, |e| matches!(e, Event::Control(_))).await, Event::Control(b"{\"hello\":\"host\"}".to_vec()));

    // Audio: 40 chunks of the biggest size, in order and complete.
    let sender = tokio::spawn(async move {
        for i in 0u8..40 {
            host.send_audio(&vec![i; CHUNK]).await.unwrap();
        }
        host
    });
    let mut got = 0usize;
    for i in 0u8..40 {
        match next_matching(&mut guest, |e| matches!(e, Event::Audio(_))).await {
            Event::Audio(chunk) => {
                assert_eq!(chunk.len(), CHUNK);
                assert!(chunk.iter().all(|&b| b == i), "chunk {i} arrived out of order or damaged");
                got += chunk.len();
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(got, 40 * CHUNK);
    let host = sender.await.unwrap();
    host.close().await;
}
