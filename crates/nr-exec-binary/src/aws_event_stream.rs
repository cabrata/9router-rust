//! Port of open-sse/utils/awsEventStream.js
//! AWS EventStream binary framing: CRC32 and single-frame decoding/encoding.

use serde_json::{json, Value as Json};
use std::collections::BTreeMap;

/// AWS EventStream constants from config/awsConstants.js
pub const MAX_MESSAGE_BYTES: usize = 100 * 1024 * 1024; // 100 MB
pub const MAX_HEADERS_BYTES: usize = 8 * 1024; // 8 KB

/// Pre-computed CRC32 lookup table for fast CRC computation.
/// Generated once at module initialization.
fn make_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for index in 0..256 {
        let mut value = index as u32;
        for _ in 0..8 {
            value = if (value & 1) != 0 {
                (value >> 1) ^ 0xedb88320
            } else {
                value >> 1
            };
        }
        table[index] = value;
    }
    table
}

lazy_static::lazy_static! {
    static ref CRC32_TABLE: [u32; 256] = make_crc32_table();
}

/// Compute CRC32 checksum of bytes.
/// Port of JS `crc32(bytes)`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffffffff;
    for byte in data {
        let index = ((crc ^ (*byte as u32)) & 0xff) as usize;
        crc = CRC32_TABLE[index] ^ (crc >> 8);
    }
    crc ^ 0xffffffff
}

/// Parsed EventStream frame: headers and optional payload.
#[derive(Debug, Clone)]
pub struct EventFrame {
    pub headers: BTreeMap<String, EventHeaderValue>,
    pub payload: Option<Json>,
}

/// EventStream header value — types are stored as discriminated unions.
#[derive(Debug, Clone)]
pub enum EventHeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    ByteArray(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid(Vec<u8>), // 16 bytes
}

/// Parse one complete EventStream frame.
/// Throws on any integrity failure (length mismatch, CRC fail, bounds error).
///
/// Frame layout: [totalLength u32][headersLength u32][preludeCrc u32][headers][payload][messageCrc u32]
pub fn parse_event_frame(data: &[u8]) -> Result<EventFrame, String> {
    parse_event_frame_with_limits(data, MAX_MESSAGE_BYTES, MAX_HEADERS_BYTES)
}

/// Parse with custom limits (mainly for testing).
pub fn parse_event_frame_with_limits(
    data: &[u8],
    max_message_bytes: usize,
    max_headers_bytes: usize,
) -> Result<EventFrame, String> {
    if data.len() < 16 {
        return Err("AWS EventStream frame is shorter than 16 bytes".to_string());
    }

    let total_length = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let headers_length = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;

    if total_length != data.len() {
        return Err("AWS EventStream frame length does not match its prelude".to_string());
    }
    if total_length > max_message_bytes
        || headers_length > max_headers_bytes
        || headers_length > total_length - 16
    {
        return Err("AWS EventStream frame bounds are invalid".to_string());
    }

    // Verify prelude CRC
    let prelude_crc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    if prelude_crc != crc32(&data[0..8]) {
        return Err("AWS EventStream prelude CRC mismatch".to_string());
    }

    // Verify message CRC
    let message_crc = u32::from_be_bytes([
        data[total_length - 4],
        data[total_length - 3],
        data[total_length - 2],
        data[total_length - 1],
    ]);
    if message_crc != crc32(&data[0..total_length - 4]) {
        return Err("AWS EventStream message CRC mismatch".to_string());
    }

    // Parse headers
    let mut headers = BTreeMap::new();
    let mut offset = 12;
    let header_end = offset + headers_length;

    while offset < header_end {
        if offset >= data.len() {
            return Err("AWS EventStream header exceeds its declared bounds".to_string());
        }

        let name_length = data[offset] as usize;
        offset += 1;

        if offset + name_length > header_end {
            return Err("AWS EventStream header exceeds its declared bounds".to_string());
        }

        let name = String::from_utf8(data[offset..offset + name_length].to_vec())
            .map_err(|_| "AWS EventStream header name is not UTF-8")?;
        offset += name_length;

        if headers.contains_key(&name) {
            return Err(format!(
                "AWS EventStream contains duplicate header: {}",
                name
            ));
        }

        if offset >= data.len() {
            return Err("AWS EventStream header exceeds its declared bounds".to_string());
        }

        let type_byte = data[offset];
        offset += 1;

        let value = match type_byte {
            0 => EventHeaderValue::Bool(false),
            1 => EventHeaderValue::Bool(true),
            2 => {
                if offset >= data.len() {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                let val = data[offset] as i8;
                offset += 1;
                EventHeaderValue::Byte(val)
            }
            3 => {
                if offset + 2 > data.len() {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                let val = i16::from_be_bytes([data[offset], data[offset + 1]]);
                offset += 2;
                EventHeaderValue::Short(val)
            }
            4 => {
                if offset + 4 > data.len() {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                let val = i32::from_be_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]);
                offset += 4;
                EventHeaderValue::Int(val)
            }
            5 | 8 => {
                // Long or Timestamp - both 8 bytes
                if offset + 8 > data.len() {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                offset += 8;
                // We'll just skip these for now as they're rarely used
                continue;
            }
            6 => {
                // Byte array
                if offset + 2 > data.len() {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                let len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
                offset += 2;
                if offset + len > header_end {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                let val = data[offset..offset + len].to_vec();
                offset += len;
                EventHeaderValue::ByteArray(val)
            }
            7 => {
                // String
                if offset + 2 > data.len() {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                let len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
                offset += 2;
                if offset + len > header_end {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                let val = String::from_utf8(data[offset..offset + len].to_vec())
                    .map_err(|_| "AWS EventStream header value is not UTF-8")?;
                offset += len;
                EventHeaderValue::String(val)
            }
            9 => {
                // UUID - 16 bytes
                if offset + 16 > data.len() {
                    return Err("AWS EventStream header exceeds its declared bounds".to_string());
                }
                offset += 16;
                continue;
            }
            _ => {
                return Err(format!(
                    "AWS EventStream header {} has unknown type {}",
                    name, type_byte
                ));
            }
        };

        headers.insert(name, value);
    }

    // Parse payload
    let payload_bytes = &data[header_end..total_length - 4];
    let payload = if payload_bytes.is_empty() {
        None
    } else {
        let text = String::from_utf8(payload_bytes.to_vec())
            .map_err(|_| "AWS EventStream payload is not UTF-8")?;
        if text.trim().is_empty() {
            None
        } else {
            match serde_json::from_str::<Json>(&text) {
                Ok(json) => Some(json),
                Err(e) => {
                    return Err(format!(
                        "AWS EventStream payload is not valid JSON ({})",
                        e
                    ))
                }
            }
        }
    };

    Ok(EventFrame { headers, payload })
}

/// Encode one complete EventStream frame.
/// Only used by tests and fixtures — nothing in the request path emits EventStream.
pub fn encode_event_frame(headers: &BTreeMap<String, String>, payload: Option<&Json>) -> Vec<u8> {
    let mut header_chunks = Vec::new();

    for (name, value) in headers {
        let name_bytes = name.as_bytes();
        let value_bytes = value.as_bytes();

        // Header format: [nameLength u8][name][type u8][valueLength u16][value]
        let mut chunk = Vec::new();
        chunk.push(name_bytes.len() as u8);
        chunk.extend_from_slice(name_bytes);
        chunk.push(7); // type = string
        chunk.extend_from_slice(&(value_bytes.len() as u16).to_be_bytes());
        chunk.extend_from_slice(value_bytes);

        header_chunks.push(chunk);
    }

    let headers_length: usize = header_chunks.iter().map(|c| c.len()).sum();
    let payload_bytes = if let Some(p) = payload {
        serde_json::to_vec(p).unwrap_or_default()
    } else {
        Vec::new()
    };

    let total_length = 16 + headers_length + payload_bytes.len();

    let mut frame = vec![0u8; total_length];
    frame[0..4].copy_from_slice(&(total_length as u32).to_be_bytes());
    frame[4..8].copy_from_slice(&(headers_length as u32).to_be_bytes());
    let prelude_crc = crc32(&frame[0..8]);
    frame[8..12].copy_from_slice(&prelude_crc.to_be_bytes());

    let mut offset = 12;
    for chunk in header_chunks {
        frame[offset..offset + chunk.len()].copy_from_slice(&chunk);
        offset += chunk.len();
    }

    frame[offset..offset + payload_bytes.len()].copy_from_slice(&payload_bytes);
    offset += payload_bytes.len();

    let message_crc = crc32(&frame[0..offset]);
    frame[offset..offset + 4].copy_from_slice(&message_crc.to_be_bytes());

    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_js() {
        // Standard CRC-32, same as the JS table implementation.
        // Vector produced by open-sse/utils/awsEventStream.js crc32().
        assert_eq!(crc32(b"hello"), 0x3610a686);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn encode_and_parse_roundtrip() {
        let mut headers = BTreeMap::new();
        headers.insert(":message-type".to_string(), "event".to_string());
        headers.insert(":event-type".to_string(), "PayloadEvent".to_string());

        let payload = json!({"bytes": "test"});
        let encoded = encode_event_frame(&headers, Some(&payload));

        let frame = parse_event_frame(&encoded).expect("parsing should succeed");
        assert_eq!(frame.payload, Some(payload));
        if let EventHeaderValue::String(s) = &frame.headers[":message-type"] {
            assert_eq!(s, "event");
        }
    }

    #[test]
    fn parse_rejects_truncated_frame() {
        let result = parse_event_frame(&[0, 0, 0, 100, 0, 0, 0, 0]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("shorter"));
    }
}
