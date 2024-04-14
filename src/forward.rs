use std::{
    net::Ipv4Addr,
    ops::{Deref, DerefMut}, sync::{Arc, Mutex, Condvar},
    time::Duration,
    error::Error,
};

pub const LEASE: Duration = Duration::from_secs(10 * 60);

/// A wrapper for `TcpListener` that will stop the forwarding on drop.
/// Implements `Deref` and `DerefMut` to the inner `TcpListener`.
pub struct Forwarded<T> {
    pub(crate) inner: T,
    pub(crate) external: Ipv4Addr,
    pub(crate) state: Arc<(Mutex<State>, Condvar)>
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum State {
    Running,
    Error,
    Terminate,
}

impl<T> Forwarded<T> {
    /// External ip address
    pub fn external(&self) -> Ipv4Addr {
        self.external.clone()
    }
    /// Is forwarding still active
    pub fn is_forwarded(&self) -> bool {
        *self.state.0.lock().unwrap() == State::Running
    }
}

impl<T> Drop for Forwarded<T> {
    fn drop(&mut self) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        *guard = State::Terminate;
        drop(guard);
        cv.notify_one();
    }
}

impl<T> Deref for Forwarded<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for Forwarded<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[derive(Debug)]
pub enum ForwardingError {
    DiscoveryFailed,
    CannotFetchExternalIp,
    NonIpv4
}

impl std::fmt::Display for ForwardingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for ForwardingError {}