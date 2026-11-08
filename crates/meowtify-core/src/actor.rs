//! The app actor. It owns no tasks: the caller spawns [`run`] on whatever
//! runtime it uses. The actor stops when every [`AppHandle`] has been dropped.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::error::Error;
use crate::message::{Command, Event};
use crate::traits::PlaybackEngine;

/// Cheap, cloneable front door to the actor.
#[derive(Clone)]
pub struct AppHandle {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<Event>,
}

impl AppHandle {
    /// Creates a handle plus the receiver that must be given to [`run`].
    pub fn new(command_capacity: usize, event_capacity: usize) -> (AppHandle, mpsc::Receiver<Command>) {
        let (commands, rx) = mpsc::channel(command_capacity.max(1));
        let (events, _) = broadcast::channel(event_capacity.max(1));
        (AppHandle { commands, events }, rx)
    }

    /// Queues a command. Fails only when the actor is gone.
    pub async fn send(&self, command: Command) -> Result<(), mpsc::error::SendError<Command>> {
        self.commands.send(command).await
    }

    /// Subscribes to events published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Sender for the event channel, to pass to [`run`] and to other publishers.
    pub fn event_sender(&self) -> broadcast::Sender<Event> {
        self.events.clone()
    }
}

/// What an action changes, and so what is published after it.
#[derive(Clone, Copy)]
struct Refresh {
    playback: bool,
    queue: bool,
}

impl Refresh {
    const PLAYBACK: Refresh = Refresh { playback: true, queue: false };
    const QUEUE: Refresh = Refresh { playback: false, queue: true };
    const ALL: Refresh = Refresh { playback: true, queue: true };
}

/// Runs until the command channel closes. Drop the [`AppHandle`] (and any
/// clones) to stop it.
///
/// The actor holds only a broadcast sender, so a handle must not be kept
/// inside the engine, or the channel would never close.
pub async fn run(
    engine: Arc<dyn PlaybackEngine>,
    mut commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<Event>,
) {
    while let Some(command) = commands.recv().await {
        let (result, scope) = match command {
            Command::Play => (engine.play().await, Refresh::PLAYBACK),
            Command::Pause => (engine.pause().await, Refresh::PLAYBACK),
            Command::Next => (engine.next().await, Refresh::PLAYBACK),
            Command::Previous => (engine.previous().await, Refresh::PLAYBACK),
            Command::Seek(ms) => (engine.seek(ms).await, Refresh::PLAYBACK),
            Command::SetVolume(v) => (engine.set_volume(v).await, Refresh::PLAYBACK),
            Command::SetShuffle(on) => (engine.set_shuffle(on).await, Refresh::PLAYBACK),
            Command::SetRepeat(mode) => (engine.set_repeat(mode).await, Refresh::PLAYBACK),
            Command::Enqueue { uri, place } => (engine.enqueue(&uri, place).await, Refresh::QUEUE),
            Command::Jump(uri) => (engine.jump(&uri).await, Refresh::ALL),
            Command::PlayFrom(selection) => (engine.play_from(&selection).await, Refresh::ALL),
            Command::Refresh => (Ok(()), Refresh::ALL),
        };

        match result {
            Ok(()) => refresh(engine.as_ref(), &events, scope).await,
            Err(e) => publish_error(&events, e),
        }
    }
}

async fn refresh(engine: &dyn PlaybackEngine, events: &broadcast::Sender<Event>, scope: Refresh) {
    if scope.playback {
        match engine.playback_state().await {
            Ok(state) => publish(events, Event::PlaybackChanged(state)),
            Err(e) => publish_error(events, e),
        }
    }
    if scope.queue {
        match engine.queue().await {
            Ok(queue) => publish(events, Event::QueueChanged(queue)),
            Err(e) => publish_error(events, e),
        }
    }
}

fn publish_error(events: &broadcast::Sender<Event>, error: Error) {
    publish(events, Event::Error(error));
}

fn publish(events: &broadcast::Sender<Event>, event: Event) {
    // No subscribers is fine: the event is simply dropped.
    let _ = events.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::message::{Place, Selection};
    use crate::types::{PlaybackState, Queue};
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        fail_with: Mutex<Option<Error>>,
    }

    impl Fake {
        fn record(&self, call: impl Into<String>) -> Result<()> {
            self.calls.lock().unwrap().push(call.into());
            match self.fail_with.lock().unwrap().clone() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PlaybackEngine for Fake {
        async fn play(&self) -> Result<()> {
            self.record("play")
        }
        async fn pause(&self) -> Result<()> {
            self.record("pause")
        }
        async fn next(&self) -> Result<()> {
            self.record("next")
        }
        async fn previous(&self) -> Result<()> {
            self.record("previous")
        }
        async fn seek(&self, position_ms: u32) -> Result<()> {
            self.record(format!("seek {position_ms}"))
        }
        async fn set_volume(&self, percent: u8) -> Result<()> {
            self.record(format!("volume {percent}"))
        }
        async fn enqueue(&self, uri: &str, place: Place) -> Result<()> {
            self.record(format!("enqueue {uri} {place:?}"))
        }
        async fn jump(&self, uri: &str) -> Result<()> {
            self.record(format!("jump {uri}"))
        }
        async fn play_from(&self, selection: &Selection) -> Result<()> {
            self.record(format!("play {selection:?}"))
        }
        async fn playback_state(&self) -> Result<Option<PlaybackState>> {
            self.calls.lock().unwrap().push("state".into());
            Ok(None)
        }
        async fn queue(&self) -> Result<Queue> {
            self.calls.lock().unwrap().push("queue".into());
            Ok(Queue::default())
        }
    }

    struct Rig {
        handle: AppHandle,
        events: broadcast::Receiver<Event>,
        fake: Arc<Fake>,
        task: tokio::task::JoinHandle<()>,
    }

    fn rig() -> Rig {
        let fake = Arc::new(Fake::default());
        let (handle, rx) = AppHandle::new(8, 32);
        let events = handle.subscribe();
        let task = tokio::spawn(run(fake.clone(), rx, handle.event_sender()));
        Rig { handle, events, fake, task }
    }

    /// Drops the handle, waits for the actor to stop, and returns every event.
    async fn finish(rig: Rig) -> (Vec<Event>, Arc<Fake>) {
        let Rig { handle, mut events, fake, task } = rig;
        drop(handle);
        task.await.unwrap();
        let mut out = Vec::new();
        while let Ok(e) = events.try_recv() {
            out.push(e);
        }
        (out, fake)
    }

    #[tokio::test]
    async fn pause_refreshes_playback_only() {
        let r = rig();
        r.handle.send(Command::Pause).await.unwrap();
        let (events, fake) = finish(r).await;
        assert_eq!(events, vec![Event::PlaybackChanged(None)]);
        assert_eq!(fake.calls(), ["pause", "state"]);
    }

    #[tokio::test]
    async fn refresh_publishes_playback_and_queue() {
        let r = rig();
        r.handle.send(Command::Refresh).await.unwrap();
        let (events, _) = finish(r).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Event::PlaybackChanged(None)));
        assert!(matches!(events[1], Event::QueueChanged(_)));
    }

    #[tokio::test]
    async fn enqueue_publishes_queue_only() {
        let r = rig();
        r.handle.send(Command::Enqueue { uri: "a".into(), place: Place::Next }).await.unwrap();
        let (events, fake) = finish(r).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::QueueChanged(_)));
        assert_eq!(fake.calls(), ["enqueue a Next", "queue"]);
    }

    #[tokio::test]
    async fn jumping_publishes_playback_and_queue() {
        // The queue panel's click must not need a second command to show the new song.
        let r = rig();
        r.handle.send(Command::Jump("a".into())).await.unwrap();
        let (events, fake) = finish(r).await;
        assert_eq!(events.len(), 2);
        assert_eq!(fake.calls(), ["jump a", "state", "queue"]);
    }

    #[tokio::test]
    async fn error_publishes_error_without_refresh() {
        let r = rig();
        *r.fake.fail_with.lock().unwrap() = Some(Error::NotFound);
        r.handle.send(Command::Next).await.unwrap();
        let (events, fake) = finish(r).await;
        assert_eq!(events, vec![Event::Error(Error::NotFound)]);
        assert_eq!(fake.calls(), ["next"]);
    }

    #[tokio::test]
    async fn arguments_pass_through() {
        let r = rig();
        r.handle.send(Command::Seek(1234)).await.unwrap();
        r.handle.send(Command::SetVolume(40)).await.unwrap();
        r.handle.send(Command::PlayFrom(Selection::Single("s".into()))).await.unwrap();
        let (_, fake) = finish(r).await;
        let calls = fake.calls();
        assert!(calls.contains(&"seek 1234".to_string()));
        assert!(calls.contains(&"volume 40".to_string()));
        assert!(calls.contains(&"play Single(\"s\")".to_string()));
    }

    #[tokio::test]
    async fn actor_stops_when_handles_drop() {
        let r = rig();
        let Rig { handle, task, .. } = r;
        drop(handle);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("actor did not stop")
            .unwrap();
    }
}
