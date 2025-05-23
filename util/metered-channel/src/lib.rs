// Copyright (c) 2018-2022 The MobileCoin Foundation

//! Provides an mpsc (multi-producer single-consumer) channel wrapped in an
//! [`IntGauge`](mc_util_metrics::IntGauge)

use crossbeam_channel::{RecvError, RecvTimeoutError, SendError, TryRecvError, TrySendError};
use mc_util_metrics::IntGauge;
use std::{fmt, iter::FusedIterator, time::Duration};

/// Similar to `crossbeam_channel::Sender`, but with an `IntGauge`.
pub struct Sender<T> {
    inner: crossbeam_channel::Sender<T>,
    gauge: IntGauge,
}

/// Similar to `crossbeam_channel::Receiver`, but with an `IntGauge`.
pub struct Receiver<T> {
    inner: crossbeam_channel::Receiver<T>,
    gauge: IntGauge,
}

/// Sender API implementation.
impl<T> Sender<T> {
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.gauge.inc();
        self.inner.try_send(msg).inspect_err(|_e| {
            self.gauge.dec();
        })
    }

    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.gauge.inc();
        self.inner.send(msg).inspect_err(|_e| {
            self.gauge.dec();
        })
    }
}

// #[derive(Clone)] adds an implementation of Clone that is conditional on all
// the type parameters also implementing Clone. Since we do not require that, we
// have to manually implement clone(). See https://github.com/rust-lang/rust/issues/41481
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            gauge: self.gauge.clone(),
        }
    }
}

/// Receiver API implementation.
impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.try_recv().inspect(|_msg| {
            self.gauge.dec();
        })
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        self.inner.recv().inspect(|_msg| {
            self.gauge.dec();
        })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.inner.recv_timeout(timeout).inspect(|_msg| {
            self.gauge.dec();
        })
    }

    pub fn iter(&self) -> Iter<T> {
        Iter { receiver: self }
    }

    pub fn try_iter(&self) -> TryIter<T> {
        TryIter { receiver: self }
    }
}

// #[derive(Clone)] adds an implementation of Clone that is conditional on all
// the type parameters also implementing Clone. Since we do not require that, we
// have to manually implement clone(). See https://github.com/rust-lang/rust/issues/41481
impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            gauge: self.gauge.clone(),
        }
    }
}

/// Iterator for `Receiver::iter()` - copied from the crossbeam_channel
/// implementation.
pub struct Iter<'a, T: 'a> {
    receiver: &'a Receiver<T>,
}

impl<T> FusedIterator for Iter<'_, T> {}

impl<T> Iterator for Iter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv().ok()
    }
}

impl<T> fmt::Debug for Iter<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("Iter { .. }")
    }
}

/// Iterator for `Receiver::try_iter()` - copied from the crossbeam_channel
/// implementation.
pub struct TryIter<'a, T: 'a> {
    receiver: &'a Receiver<T>,
}

impl<T> Iterator for TryIter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.try_recv().ok()
    }
}

impl<T> fmt::Debug for TryIter<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("TryIter { .. }")
    }
}

/// Similar to `crossbeam_channel::bounded`, `bounded` creates a pair of
/// `Sender` and `Receiver`.
pub fn bounded<T>(cap: usize, gauge: &IntGauge) -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = crossbeam_channel::bounded(cap);
    (
        Sender {
            inner: sender,
            gauge: gauge.clone(),
        },
        Receiver {
            inner: receiver,
            gauge: gauge.clone(),
        },
    )
}

/// Similar to `crossbeam_channel::unbounded`, `unbounded` creates a pair of
/// `Sender` and `Receiver`.
pub fn unbounded<T>(gauge: &IntGauge) -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = crossbeam_channel::unbounded();
    (
        Sender {
            inner: sender,
            gauge: gauge.clone(),
        },
        Receiver {
            inner: receiver,
            gauge: gauge.clone(),
        },
    )
}
