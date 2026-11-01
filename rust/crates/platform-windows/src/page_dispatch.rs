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
        Err(BookmarkFailure::BeforeDispatch(e)) => {
            tracing::debug!(target: "page.execute_javascript", "bookmark script was not dispatched: {e:?}; falling back to CDP");
            cdp().await
        }
        Err(BookmarkFailure::AfterDispatch(e)) => Err(anyhow::anyhow!("{e}: the bookmark script was dispatched and may have run; it was not run again through CDP. Read the page state before retrying.")),
        Err(BookmarkFailure::PageException(e)) => Err(anyhow::anyhow!("bookmark-exec page exception: {e}")),
    }
}

pub(crate) enum InvokeResult {
    NotSent(anyhow::Error),
    Sent(i32),
}

pub(crate) trait BookmarkSteps {
    fn prepare(&mut self) -> anyhow::Result<()>;
    fn invoke(&mut self) -> InvokeResult;
    fn restore_foreground(&mut self);
    fn poll_marker(&mut self) -> anyhow::Result<String>;
}

pub(crate) fn run_bookmark_steps(
    steps: &mut impl BookmarkSteps,
) -> Result<String, BookmarkFailure> {
    steps.prepare().map_err(BookmarkFailure::BeforeDispatch)?;
    let hr = match steps.invoke() {
        InvokeResult::NotSent(e) => return Err(BookmarkFailure::BeforeDispatch(e)),
        InvokeResult::Sent(hr) => hr,
    };
    steps.restore_foreground();
    if hr != 0 {
        return Err(BookmarkFailure::AfterDispatch(anyhow::anyhow!(
            "bookmark Invoke returned HRESULT 0x{:08X}",
            hr as u32
        )));
    }
    let marker = steps
        .poll_marker()
        .map_err(BookmarkFailure::AfterDispatch)?;
    if let Some(e) = marker.strip_prefix("CUA_ERR:") {
        return Err(BookmarkFailure::PageException(e.to_owned()));
    }
    marker
        .strip_prefix("CUA:")
        .map(str::to_owned)
        .ok_or_else(|| {
            BookmarkFailure::AfterDispatch(anyhow::anyhow!(
                "internal: marker poll returned non-CUA result"
            ))
        })
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
            || async {
                calls.set(calls.get() + 1);
                Ok("cdp".into())
            },
        )
        .await;
        assert_eq!(calls.get(), 0);
    }
    struct Fake {
        stage: u8,
        hr: i32,
        restored: usize,
    }
    impl BookmarkSteps for Fake {
        fn prepare(&mut self) -> anyhow::Result<()> {
            if self.stage == 0 {
                anyhow::bail!("prepare")
            }
            Ok(())
        }
        fn invoke(&mut self) -> InvokeResult {
            if self.stage == 1 {
                InvokeResult::NotSent(anyhow::anyhow!("not sent"))
            } else {
                InvokeResult::Sent(self.hr)
            }
        }
        fn restore_foreground(&mut self) {
            self.restored += 1;
        }
        fn poll_marker(&mut self) -> anyhow::Result<String> {
            match self.stage {
                2 => Err(anyhow::anyhow!("timeout")),
                3 => Err(anyhow::anyhow!("poll error")),
                4 => Ok("CUA_ERR:exception".into()),
                _ => Ok("CUA:value".into()),
            }
        }
    }
    #[tokio::test]
    async fn stages_route_and_restore() {
        for (stage, hr, expected_cdp, expected_restore, success) in [
            (0, 0, 1, 0, false),
            (1, 0, 1, 0, false),
            (5, 1, 0, 1, false),
            (5, -1, 0, 1, false),
            (2, 0, 0, 1, false),
            (3, 0, 0, 1, false),
            (4, 0, 0, 1, false),
            (5, 0, 0, 1, true),
        ] {
            let mut fake = Fake {
                stage,
                hr,
                restored: 0,
            };
            let outcome = run_bookmark_steps(&mut fake);
            let calls = Cell::new(0);
            let result = run_bookmark_then_cdp(async { outcome }, || async {
                calls.set(calls.get() + 1);
                Ok("fallback".into())
            })
            .await;
            assert_eq!(calls.get(), expected_cdp, "stage {stage} hr {hr}");
            assert_eq!(fake.restored, expected_restore, "stage {stage} hr {hr}");
            if success {
                assert_eq!(result.unwrap(), "value");
            } else if expected_cdp == 0 {
                assert!(result.is_err());
            }
        }
    }
    #[tokio::test]
    async fn join_failure_never_replays() {
        let join = tokio::spawn(async { panic!("worker") }).await.unwrap_err();
        let calls = Cell::new(0);
        let result = run_bookmark_then_cdp(
            async {
                Err(BookmarkFailure::AfterDispatch(anyhow::anyhow!(
                    "join error: {join}"
                )))
            },
            || async {
                calls.set(calls.get() + 1);
                Ok("fallback".into())
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 0);
    }
}
