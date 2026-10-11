//! CDP over `--remote-debugging-pipe`: Chrome reads commands on fd 3 and writes responses and
//! events on fd 4, each message a JSON text terminated by a NUL byte.

/// Splits complete NUL-terminated messages off the front of `buf`; a trailing partial message
/// stays in `buf` for the next read.
pub(crate) fn take_frames(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(i) = buf.iter().position(|&b| b == 0) {
        let rest = buf.split_off(i + 1);
        let mut frame = std::mem::replace(buf, rest);
        frame.pop(); // the NUL
        if !frame.is_empty() {
            out.push(frame);
        }
    }
    out
}

/// One outgoing message: the JSON text and its terminator.
pub(crate) fn frame(msg: &serde_json::Value) -> Vec<u8> {
    let mut v = serde_json::to_vec(msg).expect("a JSON value serializes");
    v.push(0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_on_nul_and_keep_a_partial_tail() {
        let mut buf = b"{\"id\":1}\0{\"id\":2}\0{\"id\"".to_vec();
        let f = take_frames(&mut buf);
        assert_eq!(f, vec![b"{\"id\":1}".to_vec(), b"{\"id\":2}".to_vec()]);
        assert_eq!(buf, b"{\"id\"".to_vec());
        buf.extend_from_slice(b":3}\0");
        assert_eq!(take_frames(&mut buf), vec![b"{\"id\":3}".to_vec()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn empty_frames_are_dropped_and_outgoing_frames_end_with_nul() {
        let mut buf = b"\0\0{}\0".to_vec();
        assert_eq!(take_frames(&mut buf), vec![b"{}".to_vec()]);
        let out = frame(&serde_json::json!({"id": 1}));
        assert_eq!(out.last(), Some(&0));
        assert!(!out[..out.len() - 1].contains(&0));
    }
}
