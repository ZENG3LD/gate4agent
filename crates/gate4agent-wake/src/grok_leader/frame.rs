//! 4-byte big-endian length prefix, then JSON. Same framing as grok's
//! leader IPC. 32 MiB is under the leader's own 64 MiB cap: a larger frame
//! closes the connection rather than allocating the claimed size.

pub const MAX_FRAME_BYTES: u32 = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    TooLarge(u32),
}

pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_big_endian_length_then_body() {
        let frame = encode_frame(b"hi").unwrap();
        assert_eq!(&frame[..4], &2u32.to_be_bytes());
        assert_eq!(&frame[4..], b"hi");
    }

}
