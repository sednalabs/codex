//! Cancels connection work when its executor's proxy scope ends, including HTTP upgrades.

use rama_core::Service;
use rama_core::rt::Executor;

#[derive(Clone)]
pub(crate) struct CancelOnShutdown<S> {
    inner: S,
    executor: Executor,
}

impl<S> CancelOnShutdown<S> {
    pub(crate) fn new(inner: S, executor: Executor) -> Self {
        Self { inner, executor }
    }
}

impl<S, Request> Service<Request> for CancelOnShutdown<S>
where
    S: Service<Request, Output = ()>,
    Request: Send + 'static,
{
    type Output = ();
    type Error = S::Error;

    async fn serve(&self, request: Request) -> Result<(), Self::Error> {
        let guard = self.executor.guard().cloned();
        match guard {
            Some(guard) => {
                tokio::select! {
                    biased;
                    _ = guard.cancelled() => Ok(()),
                    result = self.inner.serve(request) => result,
                }
            }
            None => self.inner.serve(request).await,
        }
    }
}
