//! 异步流的精确帧读取与完整写入；调用方负责超时和取消。
//! Exact frame reads and complete writes on async streams; callers own timeouts and cancellation.

use crate::*;
use std::{io, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[cfg(test)]
thread_local! {
    /// 测试观测当前帧缓冲的真实容量，不将广告长度当成已收到数据。
    /// Tests observe actual frame-buffer capacity rather than treating advertised length as received data.
    static BUFFER_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 区分传输错误与协议错误，避免丢失致命性信息。
/// Separates transport and protocol failures without losing fatality information.
#[derive(Debug)]
pub enum ReadError {
    /// 底层 I/O 失败；不暗示请求未执行。
    /// Underlying I/O failure; does not imply the request was unexecuted.
    Io(io::Error),
    /// 结构、语义或编码预算失败。
    /// Structural, semantic, or encoding-budget failure.
    Decode(DecodeError),
}

impl std::fmt::Display for ReadError {
    /// 保留 I/O 或协议说明，不伪装为业务缺失。
    /// Preserves I/O or protocol diagnostics without turning them into business absence.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Decode(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for ReadError {}

/// 校验帧头后仅为实际收到的字节增长缓冲；干净 EOF 返回 None，截断帧返回错误。
/// After validating the header, grows the buffer only for received bytes; clean EOF returns None and truncation fails.
///
/// 读取任何字节后取消会破坏帧边界，调用方必须关闭流。
/// Cancellation after any byte was read invalidates framing; callers must close the stream.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    limits: Limits,
) -> Result<Option<Frame>, ReadError> {
    read_frame_inner(reader, limits, None)
        .await
        .map(|frame| frame.map(|(frame, _)| frame))
}

/// 取得共享在途帧许可后读取负载，返回的许可须保留至解码帧消费完毕。
/// Acquires a shared in-flight-frame permit before reading the payload; retain the returned permit until consuming the decoded frame.
///
/// 每个许可至少按 `max_frame_bytes + max_decode_bytes` 预留预算，复制解码结果的调用方须另计；等待与读取共用调用方的期限。
/// Budget each permit for at least `max_frame_bytes + max_decode_bytes`, plus any decoded copies; waiting and reading share the caller's deadline.
/// 只读固定帧头不占许可，错误和取消会自动归还，取消后必须关闭流。
/// Fixed-header reads need no permit; errors and cancellation release it automatically, and cancelled streams must be closed.
pub async fn read_frame_budgeted<R: AsyncRead + Unpin>(
    reader: &mut R,
    limits: Limits,
    budget: Arc<Semaphore>,
) -> Result<Option<(Frame, OwnedSemaphorePermit)>, ReadError> {
    read_frame_inner(reader, limits, Some(budget))
        .await
        .map(|frame| {
            frame.map(|(frame, permit)| {
                // 此入口始终提供共享池，成功读帧必定已取得并保留许可。
                // This entry always supplies a pool, so every successful frame retains its acquired permit.
                (frame, permit.expect("budgeted frame owns its permit"))
            })
        })
}

/// 未预算的 SDK 与共享预算的服务端共用同一增量读取路径。
/// SDK reads and budgeted server reads share the same incremental path.
async fn read_frame_inner<R: AsyncRead + Unpin>(
    reader: &mut R,
    limits: Limits,
    budget: Option<Arc<Semaphore>>,
) -> Result<Option<(Frame, Option<OwnedSemaphorePermit>)>, ReadError> {
    let mut header = [0; HEADER_LEN];
    if reader.read(&mut header[..1]).await.map_err(ReadError::Io)? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            ReadError::Decode(DecodeError::protocol("Truncated frame header"))
        } else {
            ReadError::Io(e)
        }
    })?;
    let parsed = parse_header(&header, limits).map_err(ReadError::Decode)?;
    let permit = match budget {
        Some(budget) => Some(budget.acquire_owned().await.map_err(|_| {
            ReadError::Io(io::Error::new(
                io::ErrorKind::Interrupted,
                "Read budget closed",
            ))
        })?),
        None => None,
    };
    let mut bytes = header.to_vec();
    #[cfg(test)]
    BUFFER_CAPACITY.set(bytes.capacity());
    let mut chunk = [0; 8192];
    while bytes.len() - HEADER_LEN < parsed.payload_len {
        let remaining = parsed.payload_len - (bytes.len() - HEADER_LEN);
        let count = reader
            .read(&mut chunk[..remaining.min(8192)])
            .await
            .map_err(ReadError::Io)?;
        if count == 0 {
            let mut error = DecodeError::protocol("Truncated frame payload");
            error.request_id = parsed.request_id;
            return Err(ReadError::Decode(error));
        }
        append_received(&mut bytes, &chunk[..count], HEADER_LEN + parsed.payload_len)?;
    }
    decode_frame(&bytes, limits)
        .map(|frame| Some((frame, permit)))
        .map_err(ReadError::Decode)
}

/// 随收到字节几何增长且不超过整帧限额，避免逐块重分配或按广告长度预占整帧。
/// Grows geometrically with received bytes, capped at frame size, avoiding per-chunk reallocations and full advertised reservations.
fn append_received(
    bytes: &mut Vec<u8>,
    received: &[u8],
    frame_len: usize,
) -> Result<(), ReadError> {
    let required = bytes.len() + received.len();
    if required > bytes.capacity() {
        let capacity = required
            .checked_next_power_of_two()
            .unwrap_or(frame_len)
            .min(frame_len);
        bytes
            .try_reserve_exact(capacity - bytes.len())
            .map_err(|_| ReadError::Decode(DecodeError::limit()))?;
    }
    bytes.extend_from_slice(received);
    #[cfg(test)]
    BUFFER_CAPACITY.set(bytes.capacity());
    Ok(())
}

/// 发送前完成整帧编码与限额校验；I/O 失败可能已经写出部分字节。
/// Encodes and validates the entire frame first; an I/O failure may follow partial transmission.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
    limits: Limits,
) -> Result<(), ReadError> {
    let bytes = encode_frame(frame, limits).map_err(|error| {
        ReadError::Decode(DecodeError {
            error,
            fatal: false,
            request_id: frame.request_id,
        })
    })?;
    writer.write_all(&bytes).await.map_err(ReadError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll, Waker},
    };
    use tokio::io::ReadBuf;

    /// 只提供有界前缀，随后永久 Pending；计数证明读取不会越过许可或整帧预分配。
    /// Supplies a bounded prefix then stays pending; counters detect reads before permits and full-frame preallocation.
    struct PrefixReader {
        bytes: Vec<u8>,
        consumed: usize,
        largest_read: usize,
    }

    impl AsyncRead for PrefixReader {
        /// 每次只交付调用方已提供的字节，不制造 EOF。
        /// Delivers only the supplied bytes on each call without manufacturing EOF.
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.largest_read = self.largest_read.max(out.remaining());
            if self.consumed == self.bytes.len() {
                return Poll::Pending;
            }
            let count = out.remaining().min(self.bytes.len() - self.consumed);
            out.put_slice(&self.bytes[self.consumed..self.consumed + count]);
            self.consumed += count;
            Poll::Ready(Ok(()))
        }
    }

    /// 未握手和握手后使用同一读路径：64 MiB 广告只收到几字节时只分配几字节。
    /// The same pre/post-handshake path allocates only received bytes for a 64 MiB advertisement.
    #[tokio::test]
    async fn advertised_payload_does_not_reserve_the_frame() {
        let mut header = [0; HEADER_LEN];
        header[..4].copy_from_slice(b"LG\x01\x01");
        header[13] = 1;
        header[14..].copy_from_slice(&(64u32 * 1024 * 1024).to_be_bytes());
        let budget = Arc::new(Semaphore::new(1));
        for prefix in [0, 7, 8192] {
            let mut reader = PrefixReader {
                bytes: [header.as_slice(), &vec![0; prefix]].concat(),
                consumed: 0,
                largest_read: 0,
            };
            let mut read = Box::pin(read_frame_budgeted(
                &mut reader,
                Limits::default(),
                budget.clone(),
            ));
            assert!(
                read.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert!(BUFFER_CAPACITY.get() < 2 * (HEADER_LEN + prefix));
            assert_eq!(budget.available_permits(), 0);
            drop(read);
            assert_eq!(budget.available_permits(), 1);
            assert_eq!(reader.consumed, HEADER_LEN + prefix);
            assert!(reader.largest_read <= 8192);
        }
    }

    /// 共享池耗尽时只读头，释放许可后同一合法大帧可成功，并由消费者持有许可。
    /// Exhaustion reads only the header; releasing a permit lets the same valid large frame complete and transfers ownership to its consumer.
    #[tokio::test]
    async fn shared_budget_survives_waiting_and_returns_on_completion_or_error() {
        let expected = Frame {
            request_id: 1,
            body: Body::Request(Request::new(
                "SET",
                vec![Value::String("k".into()), Value::Bytes(vec![0; 128 * 1024])],
            )),
        };
        let bytes = encode_frame(&expected, Limits::default()).unwrap();
        let budget = Arc::new(Semaphore::new(1));
        let occupied = budget.clone().acquire_owned().await.unwrap();
        let mut reader = PrefixReader {
            bytes: bytes.clone(),
            consumed: 0,
            largest_read: 0,
        };
        let mut read = Box::pin(read_frame_budgeted(
            &mut reader,
            Limits::default(),
            budget.clone(),
        ));
        assert!(
            read.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(read);
        assert_eq!(reader.consumed, HEADER_LEN);
        drop(occupied);
        let (frame, permit) = read_frame_budgeted(
            &mut std::io::Cursor::new(&bytes),
            Limits::default(),
            budget.clone(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(frame, expected);
        assert_eq!(budget.available_permits(), 0);
        drop(permit);
        assert_eq!(budget.available_permits(), 1);
        assert!(
            read_frame_budgeted(
                &mut std::io::Cursor::new(&bytes[..HEADER_LEN + 1]),
                Limits::default(),
                budget.clone()
            )
            .await
            .is_err()
        );
        assert_eq!(budget.available_permits(), 1);
    }
}
