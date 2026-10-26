//! Minecraft Java wire format: VarInts, strings, packet frames and network NBT.

use anyhow::{bail, Result};
use tokio::io::{AsyncRead, AsyncReadExt};

pub fn varint(value: i32) -> Vec<u8> {
    let mut n = value as u32;
    let mut out = Vec::with_capacity(5);
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

pub fn mc_string(s: &str) -> Vec<u8> {
    let mut out = varint(s.len() as i32);
    out.extend_from_slice(s.as_bytes());
    out
}

pub fn frame(id: i32, payload: &[u8]) -> Vec<u8> {
    let mut body = varint(id);
    body.extend_from_slice(payload);
    let mut out = varint(body.len() as i32);
    out.extend(body);
    out
}

/// A text component as JSON, as used by the login state's Disconnect packet.
pub fn json_text(message: &str) -> Vec<u8> {
    mc_string(&serde_json::json!({ "text": message }).to_string())
}

pub async fn read_varint<R: AsyncRead + Unpin>(r: &mut R, first: Option<u8>) -> Result<i32> {
    let mut n: u32 = 0;
    for i in 0..5 {
        let b = match (i, first) {
            (0, Some(f)) => f,
            _ => r.read_u8().await?,
        };
        n |= ((b & 0x7F) as u32) << (7 * i);
        if b & 0x80 == 0 {
            return Ok(n as i32);
        }
    }
    bail!("varint too long")
}

pub struct Packet {
    /// The whole frame as received (length prefix included), for relaying.
    pub raw: Vec<u8>,
    pub id: i32,
    pub data: Buf,
}

pub async fn read_packet<R: AsyncRead + Unpin>(r: &mut R, max_len: usize, first: Option<u8>) -> Result<Packet> {
    let len = read_varint(r, first).await?;
    if len <= 0 || len as usize > max_len {
        bail!("bad packet length {len}");
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    let mut raw = varint(len);
    raw.extend_from_slice(&body);
    let mut data = Buf { data: body, pos: 0 };
    let id = data.varint()?;
    Ok(Packet { raw, id, data })
}

pub struct Buf {
    data: Vec<u8>,
    pos: usize,
}

impl Buf {
    pub fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.pos + n > self.data.len() {
            bail!("packet too short");
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn varint(&mut self) -> Result<i32> {
        let mut n: u32 = 0;
        for i in 0..5 {
            let b = self.take(1)?[0];
            n |= ((b & 0x7F) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(n as i32);
            }
        }
        bail!("varint too long")
    }

    pub fn string(&mut self, max_chars: usize) -> Result<String> {
        let n = self.varint()?;
        if n < 0 || n as usize > max_chars * 3 {
            bail!("string too long");
        }
        Ok(String::from_utf8_lossy(self.take(n as usize)?).into_owned())
    }

    pub fn ushort(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
}

/// Network NBT (since 1.20.2 the root tag has no name).
pub enum Nbt {
    Bool(bool),
    Int(i32),
    Str(String),
    List(Vec<Nbt>),
    Compound(Vec<(String, Nbt)>),
}

/// Java's "modified UTF-8": UTF-16 code units, with NUL and surrogates written as 2/3-byte sequences.
fn mutf8(s: &str, out: &mut Vec<u8>) {
    let mut bytes = Vec::with_capacity(s.len());
    for cu in s.encode_utf16() {
        match cu {
            1..=0x7F => bytes.push(cu as u8),
            0 | 0x80..=0x7FF => bytes.extend([0xC0 | (cu >> 6) as u8, 0x80 | (cu & 0x3F) as u8]),
            _ => bytes.extend([0xE0 | (cu >> 12) as u8, 0x80 | ((cu >> 6) & 0x3F) as u8, 0x80 | (cu & 0x3F) as u8]),
        }
    }
    let len = bytes.len().min(u16::MAX as usize);
    out.extend((len as u16).to_be_bytes());
    out.extend(&bytes[..len]);
}

impl Nbt {
    fn tag(&self) -> u8 {
        match self {
            Nbt::Bool(_) => 1,
            Nbt::Int(_) => 3,
            Nbt::Str(_) => 8,
            Nbt::List(_) => 9,
            Nbt::Compound(_) => 10,
        }
    }

    fn payload(&self, out: &mut Vec<u8>) {
        match self {
            Nbt::Bool(b) => out.push(*b as u8),
            Nbt::Int(i) => out.extend(i.to_be_bytes()),
            Nbt::Str(s) => mutf8(s, out),
            Nbt::List(items) => {
                out.push(items.first().map_or(0, Nbt::tag));
                out.extend((items.len() as i32).to_be_bytes());
                for item in items {
                    item.payload(out);
                }
            }
            Nbt::Compound(fields) => {
                for (k, v) in fields {
                    out.push(v.tag());
                    mutf8(k, out);
                    v.payload(out);
                }
                out.push(0);
            }
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![self.tag()];
        self.payload(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for v in [0, 1, 127, 128, 255, 25565, 2097151, i32::MAX, -1] {
            let bytes = varint(v);
            let mut buf = Buf { data: bytes.clone(), pos: 0 };
            assert_eq!(buf.varint().unwrap(), v, "{bytes:?}");
        }
        assert_eq!(varint(-1), vec![0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
    }

    #[test]
    fn nbt_matches_java_layout() {
        let nbt = Nbt::Compound(vec![("a".into(), Nbt::Str("\u{e9}\u{0}".into())), ("b".into(), Nbt::Bool(true))]);
        assert_eq!(nbt.encode(), vec![10, 8, 0, 1, b'a', 0, 4, 0xC3, 0xA9, 0xC0, 0x80, 1, 0, 1, b'b', 1, 0]);
    }
}
