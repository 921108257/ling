use crate::*;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    Decode(DecodeError),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Decode(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for ReadError {}

/// Cancellation after any byte was read invalidates this stream; callers close it.
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
