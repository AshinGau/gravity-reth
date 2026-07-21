use std::{collections::HashMap, fmt::Debug, hash::Hash, sync::Mutex, time::Duration};

use tokio::sync::oneshot;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ChannelError {
    #[error("channel closed")]
    Closed,
    #[error("channel already has a waiter for this key")]
    DuplicateWaiter,
    #[error("channel key was already notified")]
    DuplicateNotify,
    #[error("channel mutex poisoned")]
    Poisoned,
}

#[derive(Debug)]
pub(crate) struct Channel<K, V> {
    inner: Mutex<Inner<K, V>>,
}

#[derive(Debug)]
enum State<V> {
    Waiting(oneshot::Sender<V>),
    Notified(V),
}

#[derive(Debug)]
struct Inner<K, V> {
    states: HashMap<K, State<V>>,
    closed: bool,
}

impl<K: Eq + Clone + Debug + Hash, V> Channel<K, V> {
    pub(crate) fn new() -> Self {
        Self { inner: Mutex::new(Inner { states: HashMap::new(), closed: false }) }
    }

    pub(crate) fn new_with_states<I: IntoIterator<Item = (K, V)>>(states: I) -> Self {
        let mut inner = Inner { states: HashMap::new(), closed: false };
        for (k, v) in states {
            inner.states.insert(k, State::Notified(v));
        }
        Self { inner: Mutex::new(inner) }
    }

    /// Wait until the key is notified.
    /// Returns an error if the barrier has been closed or the key already has a waiter.
    pub(crate) async fn wait(&self, key: K) -> Result<V, ChannelError> {
        Ok(self.wait_inner(key, None).await?.expect("wait without timeout cannot time out"))
    }

    /// Wait until the key is notified with a timeout.
    /// `Ok(None)` means that the timeout was reached.
    pub(crate) async fn wait_timeout(
        &self,
        key: K,
        timeout: Duration,
    ) -> Result<Option<V>, ChannelError> {
        self.wait_inner(key, Some(timeout)).await
    }

    async fn wait_inner(
        &self,
        key: K,
        timeout: Option<Duration>,
    ) -> Result<Option<V>, ChannelError> {
        // Use block scoping to ensure MutexGuard is dropped before any `.await` point.
        // This is compiler-enforced: the guard cannot escape the block, so it is
        // impossible to hold it across a thread-migration boundary.
        let rx = {
            let mut inner = self.inner.lock().map_err(|_| ChannelError::Poisoned)?;
            if inner.closed {
                return Err(ChannelError::Closed);
            }

            if matches!(inner.states.get(&key), Some(State::Waiting(_))) {
                return Err(ChannelError::DuplicateWaiter);
            }
            if let Some(State::Notified(value)) = inner.states.remove(&key) {
                return Ok(Some(value));
            }

            let (tx, rx) = oneshot::channel();
            inner.states.insert(key.clone(), State::Waiting(tx));
            rx
            // `inner` (MutexGuard) is dropped here, before any `.await`.
        };

        match timeout {
            Some(duration) => match tokio::time::timeout(duration, rx).await {
                Ok(result) => result.map(Some).map_err(|_| ChannelError::Closed),
                Err(_) => {
                    // Timeout occurred, clean up the waiting state only if still
                    // waiting. If the state is Notified, we should not remove it
                    // to avoid losing the notify signal.
                    let mut inner = self.inner.lock().map_err(|_| ChannelError::Poisoned)?;
                    if matches!(inner.states.get(&key), Some(State::Waiting(_))) {
                        inner.states.remove(&key);
                    }
                    Ok(None)
                }
            },
            None => rx.await.map(Some).map_err(|_| ChannelError::Closed),
        }
    }

    /// Notify the key with the value.
    pub(crate) fn notify(&self, key: K, val: V) -> Result<(), ChannelError> {
        let mut inner = self.inner.lock().map_err(|_| ChannelError::Poisoned)?;
        if inner.closed {
            return Err(ChannelError::Closed);
        }
        let state = inner.states.remove(&key);
        match state {
            Some(State::Waiting(tx)) => {
                // If send fails, the receiver was already dropped (likely due to timeout).
                // In this case, we store the value as Notified so it won't be lost.
                if let Err(v) = tx.send(val) {
                    inner.states.insert(key, State::Notified(v));
                }
            }
            Some(State::Notified(previous)) => {
                inner.states.insert(key, State::Notified(previous));
                return Err(ChannelError::DuplicateNotify)
            }
            None => {
                inner.states.insert(key, State::Notified(val));
            }
        }
        Ok(())
    }

    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.closed = true;
        inner.states.clear();
    }
}

#[cfg(test)]
mod test {
    use super::ChannelError;
    use rand::{rng, Rng};
    use std::{sync::Arc, time::Duration};
    use tokio::task::JoinSet;

    #[tokio::test]
    async fn test_pipe_barrier() {
        let barrier = Arc::new(super::Channel::new_with_states([(0, 0)]));

        let mut tasks = JoinSet::new();
        for i in 1..10 {
            let barrier = barrier.clone();
            let sleep_ms = rng().random_range(100..1000);
            tasks.spawn(async move {
                let v = barrier.wait(i - 1).await.unwrap();
                assert_eq!(v, i - 1);
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                barrier.notify(i, i).unwrap();
            });
        }

        tasks.join_all().await;
    }

    #[tokio::test]
    async fn timeout_is_distinct_from_close() {
        let channel = super::Channel::<u64, u64>::new();
        assert_eq!(channel.wait_timeout(1, Duration::from_millis(1)).await, Ok(None));

        channel.close();
        assert_eq!(
            channel.wait_timeout(1, Duration::from_millis(1)).await,
            Err(ChannelError::Closed)
        );
    }

    #[tokio::test]
    async fn duplicate_notify_is_an_error() {
        let channel = super::Channel::new();
        channel.notify(1, 1).unwrap();
        assert_eq!(channel.notify(1, 2), Err(ChannelError::DuplicateNotify));
        assert_eq!(channel.wait(1).await, Ok(1));
    }

    #[tokio::test]
    async fn duplicate_waiter_is_an_error() {
        let channel = Arc::new(super::Channel::new());
        let waiter_channel = channel.clone();
        let waiter = tokio::spawn(async move { waiter_channel.wait(1).await });
        tokio::task::yield_now().await;

        assert_eq!(channel.wait(1).await, Err(ChannelError::DuplicateWaiter));
        channel.notify(1, 7).unwrap();
        assert_eq!(waiter.await.unwrap(), Ok(7));
    }
}
