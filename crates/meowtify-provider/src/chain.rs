//! Two providers for the same service, tried in order.
//!
//! On the desktop the in-process YouTube client comes first: it starts no program, needs no
//! sign-in and takes about a second for a song. yt-dlp stays behind it for the songs and the
//! moments when the first one fails (YouTube changes what its clients may do from time to time,
//! so no single way works forever).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::{Item, ProgressFn, Provider, ReadyFn, Result};

/// Asks `first`; when that fails (or has nothing), asks `second`.
pub struct Chain {
    first: Arc<dyn Provider>,
    second: Arc<dyn Provider>,
}

impl Chain {
    pub fn new(first: Arc<dyn Provider>, second: Arc<dyn Provider>) -> Chain {
        Chain { first, second }
    }
}

#[async_trait]
impl Provider for Chain {
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>> {
        match self.first.search(query, limit).await {
            Ok(items) if !items.is_empty() => Ok(items),
            _ => self.second.search(query, limit).await,
        }
    }

    async fn resolve(&self, url: &str) -> Result<Vec<Item>> {
        match self.first.resolve(url).await {
            Ok(items) if !items.is_empty() => Ok(items),
            _ => self.second.resolve(url).await,
        }
    }

    async fn download(&self, item: &Item, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf> {
        match self.first.download(item, dest_dir, progress.clone()).await {
            Ok(path) => Ok(path),
            Err(first) => {
                eprintln!("download: first provider failed ({first}); trying the next");
                // When both fail, the first one's error is the one that counts: the caller reads
                // it to tell "YouTube wants a sign-in" from "this song failed", and the second
                // provider (yt-dlp) being blocked must not pause the first, which works.
                self.second.download(item, dest_dir, progress).await.map_err(|_| first)
            }
        }
    }

    async fn stream(&self, item: &Item, dest_dir: &Path, progress: ProgressFn, ready: ReadyFn) -> Result<PathBuf> {
        match self.first.stream(item, dest_dir, progress.clone(), ready.clone()).await {
            Ok(path) => Ok(path),
            Err(first) => {
                eprintln!("stream: first provider failed ({first}); trying the next");
                self.second.stream(item, dest_dir, progress, ready).await.map_err(|_| first)
            }
        }
    }

    async fn finish(&self, item: &Item, path: &Path) -> Result<()> {
        match self.first.finish(item, path).await {
            Ok(()) => Ok(()),
            Err(_) => self.second.finish(item, path).await,
        }
    }

    async fn upgrade(&self, path: &Path, id: &str, progress: ProgressFn) -> Result<Option<PathBuf>> {
        match self.first.upgrade(path, id, progress.clone()).await {
            Ok(Some(p)) => Ok(Some(p)),
            _ => self.second.upgrade(path, id, progress).await,
        }
    }

    /// Only the first provider looks ahead: yt-dlp's lookup is a request of its own, and the
    /// second provider is the exception, not the rule.
    async fn prefetch(&self, item: &Item) {
        self.first.prefetch(item).await;
    }

    fn cancel_prefetch(&self) {
        self.first.cancel_prefetch();
        self.second.cancel_prefetch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        works: bool,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for Fake {
        async fn search(&self, _q: &str, _l: u32) -> Result<Vec<Item>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.works {
                Ok(vec![item()])
            } else {
                Err(ProviderError::Tool("down".into()))
            }
        }
        async fn resolve(&self, _u: &str) -> Result<Vec<Item>> {
            Ok(Vec::new())
        }
        async fn download(&self, _i: &Item, d: &Path, _p: ProgressFn) -> Result<PathBuf> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.works {
                Ok(d.join("song.m4a"))
            } else {
                Err(ProviderError::Tool("down".into()))
            }
        }
    }

    fn item() -> Item {
        Item { provider: "youtube", id: "abc".into(), title: "t".into(), artist: String::new(), duration_secs: None, url: String::new(), thumbnail: None }
    }

    fn fake(works: bool) -> Arc<Fake> {
        Arc::new(Fake { works, calls: AtomicUsize::new(0) })
    }

    #[tokio::test]
    async fn the_second_provider_is_asked_only_when_the_first_fails() {
        let (a, b) = (fake(true), fake(true));
        let chain = Chain::new(a.clone(), b.clone());
        assert_eq!(chain.search("x", 3).await.unwrap().len(), 1);
        chain.download(&item(), Path::new("."), Arc::new(|_| {})).await.unwrap();
        assert_eq!((a.calls.load(Ordering::SeqCst), b.calls.load(Ordering::SeqCst)), (2, 0));

        let (a, b) = (fake(false), fake(true));
        let chain = Chain::new(a.clone(), b.clone());
        assert_eq!(chain.search("x", 3).await.unwrap().len(), 1);
        chain.download(&item(), Path::new("."), Arc::new(|_| {})).await.unwrap();
        assert_eq!((a.calls.load(Ordering::SeqCst), b.calls.load(Ordering::SeqCst)), (2, 2));
    }

    #[tokio::test]
    async fn both_failing_gives_the_first_error() {
        let chain = Chain::new(fake(false), fake(false));
        assert!(chain.download(&item(), Path::new("."), Arc::new(|_| {})).await.is_err());
    }
}
