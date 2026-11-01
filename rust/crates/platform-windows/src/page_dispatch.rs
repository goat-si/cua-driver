use std::future::Future;

pub(crate) enum BookmarkFailure {
    BeforeDispatch(anyhow::Error),
    AfterDispatch(anyhow::Error),
    PageException(String),
}

pub(crate) async fn run_bookmark_then_cdp<B, C, F>(bookmark: B, cdp: C) -> anyhow::Result<String>
where
    B: Future<Output = Result<String, BookmarkFailure>>,
    C: FnOnce() -> F,
    F: Future<Output = anyhow::Result<String>>,
{
    match bookmark.await {
        Ok(value) => Ok(value),
        Err(_) => cdp().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[tokio::test]
    async fn sent_failure_never_replays_through_cdp() {
        let calls = Cell::new(0);
        let _ = run_bookmark_then_cdp(
            async { Err(BookmarkFailure::AfterDispatch(anyhow::anyhow!("sent"))) },
            || async { calls.set(calls.get() + 1); Ok("cdp".into()) },
        ).await;
        assert_eq!(calls.get(), 0);
    }
}
