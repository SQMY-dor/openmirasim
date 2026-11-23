//! Small stream adapters shared by server modules.

use futures_core::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

/// Adapts an unbounded tokio receiver into a [`Stream`].
pub struct ReceiverStream<T> {
    rx: mpsc::UnboundedReceiver<T>,
}

impl<T> ReceiverStream<T> {
    pub fn new(rx: mpsc::UnboundedReceiver<T>) -> Self {
        Self { rx }
    }
}

impl<T> Stream for ReceiverStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.rx.poll_recv(cx)
    }
}
