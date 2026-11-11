//! The request body as the edge reads it (design specification section
//! 11.1): the client's stream, whose data bytes are counted as they
//! arrive, so that the access record of each request, and with it
//! `muru_request_bytes_total`, says how much the client sent.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};

/// A client's request body, counting the bytes of the data it yields.
#[derive(Debug)]
pub struct RequestBody {
    inner: Incoming,
    received: Arc<AtomicU64>,
}

impl RequestBody {
    /// The body, counting into `received`.
    pub fn new(inner: Incoming, received: Arc<AtomicU64>) -> Self {
        Self { inner, received }
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);

        if let Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(data) = frame.data_ref()
        {
            self.received
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }

        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
