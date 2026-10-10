//! Client <-> server wire format.
//!
//! Every message is `[tag: u8][len: u32 big-endian][payload]`. tmux uses
//! OpenBSD's imsg for this (proc.c, `MSG_*` in tmux-protocol.h): a fixed
//! header with type, length, peer id and pid, and optionally a passed fd.
//! screen sends a fixed-size `struct msg` union (screen.h / socket.c).

use std::io::{self, Write};

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    // client -> server
    /// First message after connecting: the client's terminal size.
    Hello { rows: u16, cols: u16 },
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Detach,
    Kill,
    /// Ask for the screen contents as text; the server replies and hangs up.
    Capture,
    // server -> client
    Output(Vec<u8>),
    Exited,
}

const HEADER: usize = 5;
const MAX_PAYLOAD: usize = 1 << 20;

impl Msg {
    fn tag(&self) -> u8 {
        match self {
            Msg::Hello { .. } => 1,
            Msg::Input(_) => 2,
            Msg::Resize { .. } => 3,
            Msg::Detach => 4,
            Msg::Kill => 5,
            Msg::Output(_) => 6,
            Msg::Exited => 7,
            Msg::Capture => 8,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let payload = match self {
            Msg::Hello { rows, cols } | Msg::Resize { rows, cols } => {
                [rows.to_be_bytes(), cols.to_be_bytes()].concat()
            }
            Msg::Input(b) | Msg::Output(b) => b.clone(),
            Msg::Detach | Msg::Kill | Msg::Exited | Msg::Capture => Vec::new(),
        };
        let mut out = Vec::with_capacity(HEADER + payload.len());
        out.push(self.tag());
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
        out
    }

    pub fn send(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())
    }
}

/// Removes one complete message from the front of `buf`, if one has arrived.
pub fn decode(buf: &mut Vec<u8>) -> Result<Option<Msg>> {
    if buf.len() < HEADER {
        return Ok(None);
    }
    let len = u32::from_be_bytes(buf[1..HEADER].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        bail!("message too large: {len} bytes");
    }
    if buf.len() < HEADER + len {
        return Ok(None);
    }
    let tag = buf[0];
    let payload: Vec<u8> = buf.drain(..HEADER + len).skip(HEADER).collect();
    let size = || -> Result<(u16, u16)> {
        match payload[..] {
            [r0, r1, c0, c1] => Ok((u16::from_be_bytes([r0, r1]), u16::from_be_bytes([c0, c1]))),
            _ => bail!("bad size payload"),
        }
    };
    let msg = match tag {
        1 => {
            let (rows, cols) = size()?;
            Msg::Hello { rows, cols }
        }
        2 => Msg::Input(payload),
        3 => {
            let (rows, cols) = size()?;
            Msg::Resize { rows, cols }
        }
        4 => Msg::Detach,
        5 => Msg::Kill,
        6 => Msg::Output(payload),
        7 => Msg::Exited,
        8 => Msg::Capture,
        _ => bail!("unknown message tag {tag}"),
    };
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = [
            Msg::Hello { rows: 24, cols: 80 },
            Msg::Input(b"ls\r".to_vec()),
            Msg::Resize { rows: 300, cols: 1000 },
            Msg::Detach,
            Msg::Kill,
            Msg::Output(vec![0x1b, b'[', b'H']),
            Msg::Exited,
            Msg::Capture,
        ];
        let mut buf: Vec<u8> = msgs.iter().flat_map(Msg::encode).collect();
        for m in &msgs {
            assert_eq!(decode(&mut buf).unwrap().as_ref(), Some(m));
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_message_waits() {
        let full = Msg::Input(b"hello".to_vec()).encode();
        let mut buf = full[..7].to_vec();
        assert_eq!(decode(&mut buf).unwrap(), None);
        buf.extend(&full[7..]);
        assert_eq!(decode(&mut buf).unwrap(), Some(Msg::Input(b"hello".to_vec())));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(&mut vec![99, 0, 0, 0, 0]).is_err());
        assert!(decode(&mut vec![2, 0xff, 0xff, 0xff, 0xff]).is_err());
    }
}
