//! Transport cancellation observed only by physical key delivery.

use std::future::Future;
use tokio::sync::watch;

tokio::task_local! {
    static DISCONNECT: watch::Receiver<bool>;
}

pub async fn scope<F: Future>(receiver: watch::Receiver<bool>, future: F) -> F::Output {
    DISCONNECT.scope(receiver, future).await
}

pub fn subscribe() -> Option<watch::Receiver<bool>> {
    DISCONNECT.try_with(Clone::clone).ok()
}
