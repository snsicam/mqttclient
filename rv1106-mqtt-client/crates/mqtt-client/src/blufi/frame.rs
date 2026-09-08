//! BluFi 帧编解码与分片（详细设计 §4.6.1~4.6.5）。

pub mod ftype {
    // 控制帧 subtype（function code）
    pub const ACK: u8 = 0x00;
    pub const NEGOTIATE: u8 = 0x01;
    pub const SET_OPMODE: u8 = 0x02;
    pub const DISCONNECT_BLE: u8 = 0x08;
    pub const GET_WIFI_LIST: u8 = 0x09;
    // 数据帧 subtype
    pub const NEG_DATA: u8 = 0x00;
    pub const SSID: u8 = 0x02;
    pub const PASSWORD: u8 = 0x03;
    pub const CONNECT_STATE: u8 = 0x0F;
    pub const VERSION: u8 = 0x10;
    pub const WIFI_LIST: u8 = 0x11;
    pub const REPORT_ERROR: u8 = 0x12;
    pub const CUSTOM_DATA: u8 = 0x13;
}

pub mod fc {
    pub const ENCRYPTED: u8 = 0x01;
    pub const HAS_CHECKSUM: u8 = 0x02;
    pub const DIRECTION: u8 = 0x04; // 1=设备→APP
    pub const NEED_ACK: u8 = 0x08;
    pub const FRAGMENTED: u8 = 0x10;
}

pub const PKG_CTRL: u8 = 0x00;
pub const PKG_DATA: u8 = 0x01;

/// Type 字节 = `(subtype << 2) | pkg_type`（★易错：subtype 是功能码，不是 Type 字节本身）。
#[inline]
pub const fn type_byte(pkg_type: u8, subtype: u8) -> u8 {
    (subtype << 2) | (pkg_type & 0x03)
}
#[inline]
pub fn split_type(t: u8) -> (u8, u8) {
    (t & 0x03, (t >> 2) & 0x3F)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    TooShort,
    #[allow(dead_code)]
    BadType,
    LenMismatch { expect: usize, actual: usize },
    ChecksumMismatch { got: u16, calc: u16 },
}

#[derive(Debug, Clone)]
pub struct BluFiFrame {
    pub pkg_type: u8, // PKG_CTRL | PKG_DATA
    pub subtype: u8,  // 功能码，见 ftype
    pub frame_ctrl: u8, // 见 fc
    pub sequence: u8,
    pub data: Vec<u8>,
}

impl BluFiFrame {
    pub fn new(pkg_type: u8, subtype: u8, frame_ctrl: u8, sequence: u8, data: Vec<u8>) -> Self {
        Self { pkg_type, subtype, frame_ctrl, sequence, data }
    }
    pub fn type_byte(&self) -> u8 {
        type_byte(self.pkg_type, self.subtype)
    }
    pub fn needs_ack(&self) -> bool {
        self.frame_ctrl & fc::NEED_ACK != 0
    }

    /// 解码一帧字节流。线格式：`[type][frame_ctrl][sequence][data_len][data..][checksum:2]`。
    /// checksum 仅当 `frame_ctrl & HAS_CHECKSUM`。
    pub fn decode(raw: &[u8]) -> Result<BluFiFrame, FrameError> {
        if raw.len() < 4 {
            return Err(FrameError::TooShort);
        }
        let (pkg_type, subtype) = split_type(raw[0]);
        let frame_ctrl = raw[1];
        let sequence = raw[2];
        let data_len = raw[3] as usize;
        let has_cs = frame_ctrl & fc::HAS_CHECKSUM != 0;
        let body_end = if has_cs {
            raw.len().saturating_sub(2)
        } else {
            raw.len()
        };
        if body_end < 4 {
            return Err(FrameError::TooShort);
        }
        let data = &raw[4..body_end];
        if data.len() != data_len {
            return Err(FrameError::LenMismatch {
                expect: data_len,
                actual: data.len(),
            });
        }
        if has_cs {
            if raw.len() < body_end + 2 {
                return Err(FrameError::TooShort);
            }
            let got = u16::from_le_bytes([raw[body_end], raw[body_end + 1]]);
            let calc = checksum(sequence, data_len as u8, data);
            if got != calc {
                return Err(FrameError::ChecksumMismatch { got, calc });
            }
        }
        Ok(BluFiFrame {
            pkg_type,
            subtype,
            frame_ctrl,
            sequence,
            data: data.to_vec(),
        })
    }

    /// 编码为字节流。若 `frame_ctrl & HAS_CHECKSUM` 则尾部追加校验（v1 L0 不启用）。
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + self.data.len() + 2);
        v.push(self.type_byte());
        v.push(self.frame_ctrl);
        v.push(self.sequence);
        v.push(self.data.len() as u8);
        v.extend_from_slice(&self.data);
        if self.frame_ctrl & fc::HAS_CHECKSUM != 0 {
            let cs = checksum(self.sequence, self.data.len() as u8, &self.data);
            v.extend_from_slice(&cs.to_le_bytes());
        }
        v
    }
}

/// 校验字段 = Sequence + Data Length + 明文 Data，返回小端 u16。
/// 乐鑫 BluFi 采用 CRC16/CCITT；**v1 L0 明文不启用**，此处仅预留，L1 启用时再核对规范精确多项式。
pub fn checksum(seq: u8, len: u8, data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in std::iter::once(&seq)
        .chain(std::iter::once(&len))
        .chain(data.iter())
    {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// 接收侧分片重组：置位 `fc::FRAGMENTED` 的首片 data 前 2 字节为整段逻辑数据总长（小端）；
/// 后续续片段 data 为续接片段。所有分片 sequence 连续递增。
pub struct FragmentAssembler {
    total: Option<usize>,
    buf: Vec<u8>,
}
impl FragmentAssembler {
    pub fn new() -> Self {
        Self {
            total: None,
            buf: Vec::new(),
        }
    }

    /// 喂入一帧：收齐返回 Some(完整 data)；未收齐返回 None；分片非法返回 Err。
    pub fn feed(&mut self, f: &BluFiFrame) -> Result<Option<Vec<u8>>, FrameError> {
        let fragmented = f.frame_ctrl & fc::FRAGMENTED != 0;
        if fragmented {
            if f.data.len() < 2 {
                return Err(FrameError::TooShort);
            }
            let total = u16::from_le_bytes([f.data[0], f.data[1]]) as usize;
            if self.total.is_none() {
                self.total = Some(total);
                self.buf.clear();
            }
            self.buf.extend_from_slice(&f.data[2..]);
        } else if let Some(total) = self.total {
            self.buf.extend_from_slice(&f.data);
            if self.buf.len() >= total {
                let out = self.buf[..total].to_vec();
                self.total = None;
                self.buf.clear();
                return Ok(Some(out));
            }
        } else {
            // 单帧、未分片：直接返回其 data
            return Ok(Some(f.data.clone()));
        }
        Ok(None)
    }
}

impl Default for FragmentAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// 把一段逻辑 data 切成多帧：首片带 `FRAGMENTED` + 2 字节总长前缀；后续续片段不带。
pub fn split_for_tx(
    pkg_type: u8,
    subtype: u8,
    frame_ctrl: u8,
    base_seq: u8,
    data: &[u8],
    max_payload: usize,
) -> Vec<BluFiFrame> {
    // BluFi 帧 data_len 为 1 字节 u8（上限 255）；每片 data 不得超过 255，否则 encode
    // 时 `data.len() as u8` 截断，APP 解码错位（长度字段与后续 data 长度不一致）。
    // 与 BLE MTU 无关（517 足以承载 259 字节帧），故强制每片上限 255，忽略调用方
    // 传入的更大值（如历史 MTU-3=514）。
    let max_payload = max_payload.min(255);
    let mut frames = Vec::new();
    if data.len() <= max_payload {
        frames.push(BluFiFrame::new(
            pkg_type,
            subtype,
            frame_ctrl,
            base_seq,
            data.to_vec(),
        ));
        return frames;
    }
    let total = data.len() as u16;
    let mut seq = base_seq;
    let mut offset = 0usize;
    let mut first = true;
    loop {
        let cap = if first {
            max_payload.saturating_sub(2)
        } else {
            max_payload
        };
        let end = (offset + cap).min(data.len());
        let mut chunk = Vec::with_capacity(end - offset + if first { 2 } else { 0 });
        if first {
            chunk.extend_from_slice(&total.to_le_bytes());
        }
        chunk.extend_from_slice(&data[offset..end]);
        let fc = if first {
            frame_ctrl | fc::FRAGMENTED
        } else {
            frame_ctrl
        };
        frames.push(BluFiFrame::new(pkg_type, subtype, fc, seq, chunk));
        seq = seq.wrapping_add(1);
        offset = end;
        first = false;
        if offset >= data.len() {
            break;
        }
    }
    frames
}

/// 构造 ACK 帧：pkg_type=CTRL, subtype=ACK, frame_ctrl 带 DIRECTION，data=[被确认帧 sequence]。
pub fn make_ack(acked_seq: u8) -> BluFiFrame {
    BluFiFrame::new(PKG_CTRL, ftype::ACK, fc::DIRECTION, 0, vec![acked_seq])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_byte_conversion() {
        assert_eq!(type_byte(PKG_CTRL, ftype::GET_WIFI_LIST), 0x24);
        assert_eq!(type_byte(PKG_DATA, ftype::CUSTOM_DATA), 0x4D);
        assert_eq!(split_type(0x24), (PKG_CTRL, ftype::GET_WIFI_LIST));
        assert_eq!(split_type(0x4D), (PKG_DATA, ftype::CUSTOM_DATA));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let f = BluFiFrame::new(PKG_DATA, ftype::WIFI_LIST, fc::DIRECTION, 7, b"hello".to_vec());
        let bytes = f.encode();
        let d = BluFiFrame::decode(&bytes).unwrap();
        assert_eq!(d.pkg_type, PKG_DATA);
        assert_eq!(d.subtype, ftype::WIFI_LIST);
        assert_eq!(d.sequence, 7);
        assert_eq!(d.data, b"hello");
    }

    #[test]
    fn fragment_reassemble() {
        let payload: Vec<u8> = (1u8..=10).collect();
        let frames = split_for_tx(PKG_DATA, ftype::WIFI_LIST, fc::DIRECTION, 1, &payload, 5);
        assert!(frames.len() >= 2);
        assert!(frames[0].frame_ctrl & fc::FRAGMENTED != 0);
        let mut asm = FragmentAssembler::new();
        let mut result = None;
        for fr in &frames {
            if let Ok(Some(p)) = asm.feed(fr) {
                result = Some(p);
            }
        }
        assert_eq!(result, Some(payload));
    }

    #[test]
    fn fragment_large_payload_respects_u8_len() {
        // data_len 为 u8，每片 data 必须 ≤ 255，否则 encode 时长度字段截断、APP 解码错位。
        let payload: Vec<u8> = (0u8..=255).cycle().take(600).collect();
        // 即便调用方误传 514（历史值），min(255) 也应保证每片 ≤ 255
        let frames = split_for_tx(PKG_DATA, ftype::WIFI_LIST, fc::DIRECTION, 1, &payload, 514);
        assert!(frames.len() >= 3, "600 字节应分多片");
        for f in &frames {
            assert!(f.data.len() <= 255, "每片 data 必须 ≤ 255");
            let enc = f.encode();
            assert_eq!(
                enc[3] as usize,
                f.data.len(),
                "data_len 字段(enc[3])不得与真实 data 长度不一致"
            );
        }
        // 重组应与原文一致
        let mut asm = FragmentAssembler::new();
        let mut result = None;
        for fr in &frames {
            if let Ok(Some(p)) = asm.feed(fr) {
                result = Some(p);
            }
        }
        assert_eq!(result, Some(payload));
    }

    #[test]
    fn ack_frame_well_formed() {
        let ack = make_ack(0x2A);
        assert_eq!(ack.pkg_type, PKG_CTRL);
        assert_eq!(ack.subtype, ftype::ACK);
        assert_eq!(ack.data, vec![0x2A]);
        assert!(ack.frame_ctrl & fc::DIRECTION != 0);
    }
}
