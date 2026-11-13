//! 异步流的精确帧读取与完整写入；调用方负责超时和取消。
//! Exact frame reads and complete writes on async streams; callers own timeouts and cancellation.

use crate::*;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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

/// 先校验帧头再分配负载；干净 EOF 返回 None，截断帧返回错误。
/// Validates the header before payload allocation; clean EOF returns None, truncation fails.
///
/// 读取任何字节后取消会破坏帧边界，调用方必须关闭流。
/// Cancellation after any byte was read invalidates framing; callers must close the stream.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    limits: Limits,
) -> Result<Option<Frame>, ReadError> {
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
    let mut bytes = Vec::with_capacity(HEADER_LEN + parsed.payload_len);
    bytes.extend_from_slice(&header);
    bytes.resize(HEADER_LEN + parsed.payload_len, 0);
    reader
        .read_exact(&mut bytes[HEADER_LEN..])
        .await
        .map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                let mut error = DecodeError::protocol("Truncated frame payload");
                error.request_id = parsed.request_id;
                ReadError::Decode(error)
            } else {
                ReadError::Io(e)
            }
        })?;
    decode_frame(&bytes, limits)
        .map(Some)
        .map_err(ReadError::Decode)
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
