use std::{future::Future, time::Duration};

use cua_driver_core::actionability::{Clock, ElementFacts};

pub(crate) async fn gate_then_write<C, P, F, W, Fut, T>(
    _clock: &C,
    _budget: Duration,
    _probe: P,
    write: W,
) -> T
where
    C: Clock + Sync,
    P: FnMut(Duration) -> F + Send,
    F: Future<Output = ElementFacts> + Send,
    W: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    write().await
}

#[cfg(test)]
#[path = "actionability_tests.rs"]
mod tests;
