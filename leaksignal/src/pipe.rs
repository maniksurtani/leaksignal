use std::{
    cell::UnsafeCell,
    io::{self, ErrorKind, Write},
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{task::ArcWake, AsyncRead, AsyncWrite};

struct PipeInner {
    max_persistance: usize,
    data: UnsafeCell<Vec<Vec<u8>>>,
}

pub struct PipeReader {
    segment_index: usize,
    segment_subindex: usize,
    total_read: usize,
    inner: Rc<PipeInner>,
}

pub struct PipeWriter {
    inner: Rc<PipeInner>,
}

pub struct DummyWaker;

impl ArcWake for DummyWaker {
    fn wake_by_ref(_arc_self: &Arc<Self>) {}
}

/// Creates a new reactor-less pipe which will persist read data up to `max_persistance` bytes, after which it is cleared
/// Dropping either reader or writer closes the pipe
pub fn pipe(max_persistance: usize) -> (PipeReader, PipeWriter) {
    let inner = Rc::new(PipeInner {
        max_persistance,
        data: UnsafeCell::new(vec![]),
    });
    (
        PipeReader {
            segment_index: 0,
            segment_subindex: 0,
            total_read: 0,
            inner: inner.clone(),
        },
        PipeWriter { inner },
    )
}

impl PipeWriter {
    /// Returns true if successfully appended
    pub fn append(&mut self, data: impl Into<Vec<u8>>) -> bool {
        if Rc::strong_count(&self.inner) != 2 {
            return false;
        }
        unsafe { self.inner.data.get().as_mut().unwrap() }.push(data.into());
        true
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.append(buf) {
            Ok(buf.len())
        } else {
            Err(io::Error::new(ErrorKind::ConnectionReset, "pipe closed"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsyncWrite for PipeWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.write(buf))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl PipeReader {
    /// Fetches the entire content of the pipe if `total_read` < `max_persistence`
    pub fn fetch_full_content(&self) -> Option<Vec<u8>> {
        if self.total_read >= self.inner.max_persistance {
            return None;
        }
        let mut out = Vec::with_capacity(self.total_read);
        let data = unsafe { self.inner.data.get().as_ref().unwrap() };
        for item in data {
            out.extend_from_slice(&item[..]);
        }
        assert_eq!(self.total_read, out.len());
        Some(out)
    }

    pub fn total_read(&self) -> usize {
        self.total_read
    }
}

impl AsyncRead for PipeReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut self_ = self.as_mut();
        let data = unsafe { self_.inner.data.get().as_mut().unwrap() };
        let mut written = 0usize;
        loop {
            match data.get_mut(self_.segment_index) {
                Some(full_segment) => {
                    let segment = &full_segment[self_.segment_subindex..];
                    if segment.is_empty() {
                        self_.segment_subindex = 0;
                        if self_.total_read >= self_.inner.max_persistance {
                            full_segment.clear();
                        }
                        self_.segment_index += 1;
                        continue;
                    }
                    let size_written = segment.len().min(buf.len());
                    buf[..size_written].copy_from_slice(&segment[..size_written]);
                    buf = &mut buf[size_written..];
                    let segment_len = segment.len();
                    written += size_written;
                    if size_written == segment_len {
                        self_.segment_subindex = 0;
                        if self_.total_read + size_written >= self_.inner.max_persistance {
                            full_segment.clear();
                        }
                        self_.segment_index += 1;
                    } else {
                        self_.segment_subindex += size_written;
                    }
                    // remove old persistance data
                    if self_.total_read < self_.inner.max_persistance
                        && self_.total_read + size_written >= self_.inner.max_persistance
                    {
                        for i in 0..self_.segment_subindex {
                            data[i].clear();
                        }
                    }
                    self_.total_read += size_written;

                    if buf.is_empty() {
                        return Poll::Ready(Ok(written));
                    }
                    if size_written == segment_len {
                        continue;
                    }
                }
                None => {
                    if written == 0 {
                        if Rc::strong_count(&self.inner) != 2 {
                            return Poll::Ready(Ok(0));
                        } else {
                            return Poll::Pending;
                        }
                    } else {
                        return Poll::Ready(Ok(written));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{task::waker, AsyncReadExt, FutureExt};

    use super::*;

    #[test]
    fn test_pipe() {
        let (mut reader, mut writer) = pipe(0);
        let mut scratch = [0u8; 64];
        let waker = waker(Arc::new(DummyWaker));
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            reader.read(&mut scratch[..]).poll_unpin(&mut context),
            Poll::Pending
        ));

        const TEST_MESSAGE: &[u8] = b"HELLO WORLD";
        writer.write_all(TEST_MESSAGE).unwrap();
        assert!(
            matches!(reader.read(&mut scratch[..]).poll_unpin(&mut context), Poll::Ready(Ok(x)) if x == TEST_MESSAGE.len())
        );
        assert_eq!(&scratch[..TEST_MESSAGE.len()], TEST_MESSAGE);

        assert!(matches!(
            reader
                .read(&mut scratch[..TEST_MESSAGE.len()])
                .poll_unpin(&mut context),
            Poll::Pending
        ));

        writer.write_all(TEST_MESSAGE).unwrap();
        writer.write_all(TEST_MESSAGE).unwrap();
        writer.write_all(TEST_MESSAGE).unwrap();
        assert!(
            matches!(reader.read(&mut scratch[..]).poll_unpin(&mut context), Poll::Ready(Ok(x)) if x == TEST_MESSAGE.len() * 3)
        );
        assert_eq!(&scratch[..TEST_MESSAGE.len()], TEST_MESSAGE);
        assert_eq!(
            &scratch[TEST_MESSAGE.len()..TEST_MESSAGE.len() * 2],
            TEST_MESSAGE
        );
        assert_eq!(
            &scratch[TEST_MESSAGE.len() * 2..TEST_MESSAGE.len() * 3],
            TEST_MESSAGE
        );

        assert!(matches!(
            reader.read(&mut scratch[..]).poll_unpin(&mut context),
            Poll::Pending
        ));
        writer.write_all(TEST_MESSAGE).unwrap();
        drop(writer);
        assert!(
            matches!(reader.read(&mut scratch[..]).poll_unpin(&mut context), Poll::Ready(Ok(x)) if x == TEST_MESSAGE.len())
        );
        assert_eq!(&scratch[..TEST_MESSAGE.len()], TEST_MESSAGE);
        assert!(matches!(
            reader.read(&mut scratch[..]).poll_unpin(&mut context),
            Poll::Ready(Err(_))
        ));
        drop(reader);

        let (reader, mut writer) = pipe(0);
        drop(reader);
        writer.write_all(TEST_MESSAGE).err().unwrap();

        let mut scratch = [0u8; 5];
        let (mut reader, mut writer) = pipe(0);
        writer.write_all(TEST_MESSAGE).unwrap();
        assert!(
            matches!(reader.read(&mut scratch[..]).poll_unpin(&mut context), Poll::Ready(Ok(x)) if x == 5)
        );
        assert!(
            matches!(reader.read(&mut scratch[..]).poll_unpin(&mut context), Poll::Ready(Ok(x)) if x == 5)
        );
        assert!(
            matches!(reader.read(&mut scratch[..]).poll_unpin(&mut context), Poll::Ready(Ok(x)) if x == 1)
        );
        assert!(matches!(
            reader.read(&mut scratch[..]).poll_unpin(&mut context),
            Poll::Pending
        ));
    }

    #[test]
    fn test_pipe_persistence() {
        let (mut reader, mut writer) = pipe(10240);
        let mut scratch = [0u8; 64];
        let waker = waker(Arc::new(DummyWaker));
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            reader.read(&mut scratch[..]).poll_unpin(&mut context),
            Poll::Pending
        ));

        const TEST_MESSAGE: &[u8] = b"HELLO WORLD";
        for _ in 0..100 {
            writer.write_all(TEST_MESSAGE).unwrap();
            assert!(
                matches!(reader.read(&mut scratch[..]).poll_unpin(&mut context), Poll::Ready(Ok(x)) if x == TEST_MESSAGE.len())
            );
            assert_eq!(&scratch[..TEST_MESSAGE.len()], TEST_MESSAGE);
        }
        assert!(matches!(
            reader
                .read(&mut scratch[..TEST_MESSAGE.len()])
                .poll_unpin(&mut context),
            Poll::Pending
        ));

        let raw = reader.fetch_full_content().expect("missing content");
        assert_eq!(raw.len(), TEST_MESSAGE.len() * 100);
        for i in 0..100 {
            assert_eq!(
                &raw[i * TEST_MESSAGE.len()..(i + 1) * TEST_MESSAGE.len()],
                TEST_MESSAGE
            );
        }

        for _ in 0..1000 {
            writer.write_all(TEST_MESSAGE).unwrap();
            assert!(
                matches!(reader.read(&mut scratch[..]).poll_unpin(&mut context), Poll::Ready(Ok(x)) if x == TEST_MESSAGE.len())
            );
            assert_eq!(&scratch[..TEST_MESSAGE.len()], TEST_MESSAGE);
        }
        assert!(matches!(
            reader
                .read(&mut scratch[..TEST_MESSAGE.len()])
                .poll_unpin(&mut context),
            Poll::Pending
        ));
        assert!(reader.fetch_full_content().is_none())
    }
}
