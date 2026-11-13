//! Frames: a length, a kind and a payload, one after another in each direction.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The most a frame may carry after its length: a longer one ends the conversation. What the
/// other side sends is untrusted input, so nothing is allocated before this check.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

const KIND_MESSAGE: u8 = 1;
const KIND_DATA: u8 = 2;
/// Stream id and flags of a data frame.
const DATA_HEADER: usize = 9;
const FLAG_START: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// One JSON-RPC 2.0 object, in UTF-8.
    Message(Vec<u8>),
    /// Bytes of the Arrow IPC stream `stream`. `start`: they begin a new IPC stream on that id
    /// (a schema follows).
    Data {
        stream: u64,
        start: bool,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("a frame of {0} bytes is longer than the protocol allows")]
    TooLong(usize),
    #[error("a frame of an unknown kind ({0})")]
    UnknownKind(u8),
    #[error("a frame that ends before its header")]
    TooShort,
    #[error("the stream ended in the middle of a frame")]
    Truncated,
}

/// The next frame, or `None` when the stream ended between two frames.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            // `read_exact` does not say how much it read: an end right at a frame boundary and
            // one inside the length look the same here. Both end the conversation.
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(FrameError::TooLong(length));
    }
    if length == 0 {
        return Err(FrameError::TooShort);
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    })?;
    match body[0] {
        KIND_MESSAGE => {
            body.remove(0);
            Ok(Some(Frame::Message(body)))
        }
        KIND_DATA => {
            if body.len() < 1 + DATA_HEADER {
                return Err(FrameError::TooShort);
            }
            let mut stream = [0u8; 8];
            stream.copy_from_slice(&body[1..9]);
            let start = body[9] & FLAG_START != 0;
            body.drain(..1 + DATA_HEADER);
            Ok(Some(Frame::Data {
                stream: u64::from_le_bytes(stream),
                start,
                bytes: body,
            }))
        }
        other => Err(FrameError::UnknownKind(other)),
    }
}

/// Writes one frame. It does not flush: the caller does, when it has nothing more to write.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<(), FrameError> {
    let (kind, header, payload): (u8, Vec<u8>, &[u8]) = match frame {
        Frame::Message(json) => (KIND_MESSAGE, Vec::new(), json),
        Frame::Data {
            stream,
            start,
            bytes,
        } => {
            let mut header = Vec::with_capacity(DATA_HEADER);
            header.extend_from_slice(&stream.to_le_bytes());
            header.push(if *start { FLAG_START } else { 0 });
            (KIND_DATA, header, bytes)
        }
    };
    let length = 1 + header.len() + payload.len();
    if length > MAX_FRAME {
        return Err(FrameError::TooLong(length));
    }
    let mut head = Vec::with_capacity(5 + header.len());
    head.extend_from_slice(&(length as u32).to_le_bytes());
    head.push(kind);
    head.extend_from_slice(&header);
    writer.write_all(&head).await?;
    writer.write_all(payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn round_trip(frame: Frame) -> Frame {
        let mut wire = Vec::new();
        write_frame(&mut wire, &frame).await.unwrap();
        let mut reader = wire.as_slice();
        let read = read_frame(&mut reader).await.unwrap().unwrap();
        assert!(read_frame(&mut reader).await.unwrap().is_none());
        read
    }

    #[tokio::test]
    async fn frames_round_trip() {
        for frame in [
            Frame::Message(br#"{"jsonrpc":"2.0"}"#.to_vec()),
            Frame::Message(Vec::new()),
            Frame::Data {
                stream: u64::MAX,
                start: true,
                bytes: vec![1, 2, 3],
            },
            Frame::Data {
                stream: 0,
                start: false,
                bytes: Vec::new(),
            },
        ] {
            assert_eq!(round_trip(frame.clone()).await, frame);
        }
    }

    #[tokio::test]
    async fn a_frame_longer_than_the_limit_is_refused_before_it_is_read() {
        // Only the length is there: nothing after it is waited for or allocated.
        let wire = ((MAX_FRAME + 1) as u32).to_le_bytes();
        let error = read_frame(&mut wire.as_slice()).await.unwrap_err();
        assert!(matches!(error, FrameError::TooLong(n) if n == MAX_FRAME + 1));
        let wire = u32::MAX.to_le_bytes();
        assert!(matches!(
            read_frame(&mut wire.as_slice()).await,
            Err(FrameError::TooLong(_))
        ));
    }

    #[tokio::test]
    async fn broken_frames_are_errors_not_panics() {
        // No kind.
        let wire = 0u32.to_le_bytes();
        assert!(matches!(
            read_frame(&mut wire.as_slice()).await,
            Err(FrameError::TooShort)
        ));
        // An unknown kind.
        let wire = [1, 0, 0, 0, 9];
        assert!(matches!(
            read_frame(&mut wire.as_slice()).await,
            Err(FrameError::UnknownKind(9))
        ));
        // A data frame without its stream id.
        let wire = [3, 0, 0, 0, 2, 0, 0];
        assert!(matches!(
            read_frame(&mut wire.as_slice()).await,
            Err(FrameError::TooShort)
        ));
        // A body shorter than its length says.
        let wire = [10, 0, 0, 0, 1, b'{'];
        assert!(matches!(
            read_frame(&mut wire.as_slice()).await,
            Err(FrameError::Truncated)
        ));
        // Nothing at all, and an end inside the length: the conversation is over.
        assert!(read_frame(&mut [].as_slice()).await.unwrap().is_none());
        assert!(
            read_frame(&mut [1u8, 0].as_slice())
                .await
                .unwrap()
                .is_none()
        );
    }
}
