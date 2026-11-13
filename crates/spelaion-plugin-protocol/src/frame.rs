//! Frames: a length, a kind and a payload, one after another in each direction.

use std::io::{Read, Write};

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
///
/// Blocking: each side reads its pipe on a thread of its own, into a buffer (a frame's length,
/// kind and payload are then one read from the pipe when they arrive together).
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length) {
        Ok(()) => {}
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
    let truncated = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    };
    let mut kind = [0u8; 1];
    reader.read_exact(&mut kind).map_err(truncated)?;
    // The payload is read into its own bytes: nothing is moved afterwards.
    match kind[0] {
        KIND_MESSAGE => {
            let mut json = vec![0u8; length - 1];
            reader.read_exact(&mut json).map_err(truncated)?;
            Ok(Some(Frame::Message(json)))
        }
        KIND_DATA => {
            if length < 1 + DATA_HEADER {
                // What there is of it is read, so the error is about this frame.
                let mut rest = vec![0u8; length - 1];
                reader.read_exact(&mut rest).map_err(truncated)?;
                return Err(FrameError::TooShort);
            }
            let mut header = [0u8; DATA_HEADER];
            reader.read_exact(&mut header).map_err(truncated)?;
            let mut stream = [0u8; 8];
            stream.copy_from_slice(&header[..8]);
            let mut bytes = vec![0u8; length - 1 - DATA_HEADER];
            reader.read_exact(&mut bytes).map_err(truncated)?;
            Ok(Some(Frame::Data {
                stream: u64::from_le_bytes(stream),
                start: header[8] & FLAG_START != 0,
                bytes,
            }))
        }
        other => Err(FrameError::UnknownKind(other)),
    }
}

/// Writes one frame. It does not flush: the caller does, when it has nothing more to write.
pub fn write_frame<W: Write>(writer: &mut W, frame: &Frame) -> Result<(), FrameError> {
    let mut head = [0u8; 5 + DATA_HEADER];
    let (used, payload): (usize, &[u8]) = match frame {
        Frame::Message(json) => {
            head[4] = KIND_MESSAGE;
            (5, json)
        }
        Frame::Data {
            stream,
            start,
            bytes,
        } => {
            head[4] = KIND_DATA;
            head[5..13].copy_from_slice(&stream.to_le_bytes());
            head[13] = if *start { FLAG_START } else { 0 };
            (5 + DATA_HEADER, bytes)
        }
    };
    let length = used - 4 + payload.len();
    if length > MAX_FRAME {
        return Err(FrameError::TooLong(length));
    }
    head[..4].copy_from_slice(&(length as u32).to_le_bytes());
    writer.write_all(&head[..used])?;
    writer.write_all(payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(frame: Frame) -> Frame {
        let mut wire = Vec::new();
        write_frame(&mut wire, &frame).unwrap();
        let mut reader = wire.as_slice();
        let read = read_frame(&mut reader).unwrap().unwrap();
        assert!(read_frame(&mut reader).unwrap().is_none());
        read
    }

    #[test]
    fn frames_round_trip() {
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
            assert_eq!(round_trip(frame.clone()), frame);
        }
    }

    #[test]
    fn a_frame_longer_than_the_limit_is_refused_before_it_is_read() {
        // Only the length is there: nothing after it is waited for or allocated.
        let wire = ((MAX_FRAME + 1) as u32).to_le_bytes();
        let error = read_frame(&mut wire.as_slice()).unwrap_err();
        assert!(matches!(error, FrameError::TooLong(n) if n == MAX_FRAME + 1));
        let wire = u32::MAX.to_le_bytes();
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(FrameError::TooLong(_))
        ));
    }

    #[test]
    fn broken_frames_are_errors_not_panics() {
        // No kind.
        let wire = 0u32.to_le_bytes();
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(FrameError::TooShort)
        ));
        // An unknown kind.
        let wire = [1, 0, 0, 0, 9];
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(FrameError::UnknownKind(9))
        ));
        // A data frame without its stream id.
        let wire = [3, 0, 0, 0, 2, 0, 0];
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(FrameError::TooShort)
        ));
        // A body shorter than its length says.
        let wire = [10, 0, 0, 0, 1, b'{'];
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(FrameError::Truncated)
        ));
        // Nothing at all, and an end inside the length: the conversation is over.
        assert!(read_frame(&mut [].as_slice()).unwrap().is_none());
        assert!(read_frame(&mut [1u8, 0].as_slice()).unwrap().is_none());
    }
}
