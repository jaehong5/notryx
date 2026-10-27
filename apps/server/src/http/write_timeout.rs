use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

pub(super) struct WriteTimeout<T> {
    inner: T,
    timeout: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<T> WriteTimeout<T> {
    pub(super) fn new(inner: T, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            stalled: None,
        }
    }

    fn blocked<R>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<R>> {
        let deadline = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.timeout)));
        if deadline.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP response write stalled",
            )))
        } else {
            Poll::Pending
        }
    }

    fn wrote(
        &mut self,
        cx: &mut Context<'_>,
        result: Poll<io::Result<usize>>,
    ) -> Poll<io::Result<usize>> {
        match result {
            Poll::Ready(Ok(count)) => {
                if count > 0 {
                    self.stalled = None;
                }
                Poll::Ready(Ok(count))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => self.blocked(cx),
        }
    }

    fn completed(
        &mut self,
        cx: &mut Context<'_>,
        result: Poll<io::Result<()>>,
    ) -> Poll<io::Result<()>> {
        match result {
            Poll::Ready(result) => {
                self.stalled = None;
                Poll::Ready(result)
            }
            Poll::Pending => self.blocked(cx),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for WriteTimeout<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buffer)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteTimeout<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buffer);
        this.wrote(cx, result)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, buffers);
        this.wrote(cx, result)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        this.completed(cx, result)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.completed(cx, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::time::advance;

    const LIMIT: Duration = Duration::from_secs(30);

    async fn pending<F: Future>(mut future: Pin<&mut F>) {
        poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_pending_writes_keep_the_original_deadline() {
        let (stream, _reader) = duplex(1);
        let mut stream = WriteTimeout::new(stream, LIMIT);
        stream.write_all(b"a").await.unwrap();
        let mut writing = Box::pin(stream.write_all(b"b"));
        pending(writing.as_mut()).await;
        advance(Duration::from_secs(29)).await;
        pending(writing.as_mut()).await;
        advance(Duration::from_secs(2)).await;
        assert_eq!(writing.await.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn partial_progress_starts_a_new_stall_interval() {
        let (stream, mut reader) = duplex(1);
        let mut stream = WriteTimeout::new(stream, LIMIT);
        stream.write_all(b"a").await.unwrap();
        let mut writing = Box::pin(stream.write_all(b"bc"));
        pending(writing.as_mut()).await;
        advance(Duration::from_secs(29)).await;
        assert_eq!(reader.read_u8().await.unwrap(), b'a');
        pending(writing.as_mut()).await;
        advance(Duration::from_secs(29)).await;
        pending(writing.as_mut()).await;
        assert_eq!(reader.read_u8().await.unwrap(), b'b');
        writing.await.unwrap();
        assert_eq!(reader.read_u8().await.unwrap(), b'c');
    }

    #[tokio::test(start_paused = true)]
    async fn incoming_bytes_do_not_extend_a_stalled_write() {
        let (stream, mut peer) = duplex(1);
        let mut stream = WriteTimeout::new(stream, LIMIT);
        stream.write_all(b"a").await.unwrap();
        pending(Box::pin(stream.write_all(b"b")).as_mut()).await;
        advance(Duration::from_secs(29)).await;
        peer.write_all(b"r").await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), b'r');
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            stream.write_all(b"b").await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completed_writes_allow_long_reads_and_later_responses() {
        let (stream, mut peer) = duplex(1);
        let mut stream = WriteTimeout::new(stream, LIMIT);
        stream.write_all(b"a").await.unwrap();
        let mut writing = Box::pin(stream.write_all(b"b"));
        pending(writing.as_mut()).await;
        assert_eq!(peer.read_u8().await.unwrap(), b'a');
        writing.await.unwrap();
        stream.flush().await.unwrap();
        assert_eq!(peer.read_u8().await.unwrap(), b'b');
        advance(Duration::from_secs(121)).await;
        peer.write_all(b"r").await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), b'r');
        stream.write_all(b"c").await.unwrap();
        assert_eq!(peer.read_u8().await.unwrap(), b'c');
    }

    #[tokio::test(start_paused = true)]
    async fn vectored_writes_obey_the_same_stall_limit() {
        let (stream, _peer) = duplex(1);
        let mut stream = WriteTimeout::new(stream, LIMIT);
        assert!(stream.is_write_vectored());
        assert_eq!(
            stream.write_vectored(&[IoSlice::new(b"a")]).await.unwrap(),
            1
        );
        let buffers = [IoSlice::new(b"b"), IoSlice::new(b"c")];
        let mut writing = Box::pin(stream.write_vectored(&buffers));
        pending(writing.as_mut()).await;
        advance(Duration::from_secs(31)).await;
        assert_eq!(writing.await.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
