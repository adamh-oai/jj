//! Bounded BSER-v2 codec for the focused Watchman endpoint.

use std::collections::BTreeMap;
use std::fmt;

const HEADER: &[u8; 6] = b"\0\x02\0\0\0\0";
const ARRAY: u8 = 0x00;
const OBJECT: u8 = 0x01;
const STRING: u8 = 0x02;
const INT8: u8 = 0x03;
const INT16: u8 = 0x04;
const INT32: u8 = 0x05;
const INT64: u8 = 0x06;
const TRUE: u8 = 0x08;
const FALSE: u8 = 0x09;
const NULL: u8 = 0x0a;
const UTF8_STRING: u8 = 0x0d;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Object(BTreeMap<Vec<u8>, Value>),
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub frame_bytes: usize,
    pub string_bytes: usize,
    pub collection_items: usize,
    pub depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            frame_bytes: 16 * 1024 * 1024,
            string_bytes: 1024 * 1024,
            collection_items: 1_000_000,
            depth: 64,
        }
    }
}

pub fn decode_frame(bytes: &[u8], limits: Limits) -> Result<Value, BserError> {
    if bytes.len() > limits.frame_bytes {
        return Err(BserError::new("BSER frame exceeds its byte limit"));
    }
    if !bytes.starts_with(HEADER) {
        return Err(BserError::new("invalid BSER-v2 header"));
    }
    let mut cursor = Cursor::new(&bytes[HEADER.len()..], limits);
    let payload_len = cursor.integer()?;
    let payload_len = usize::try_from(payload_len)
        .map_err(|_| BserError::new("negative or oversized BSER payload length"))?;
    if payload_len > limits.frame_bytes || cursor.remaining() != payload_len {
        return Err(BserError::new(
            "BSER payload length does not match its frame",
        ));
    }
    let value = cursor.value(0)?;
    if cursor.remaining() != 0 {
        return Err(BserError::new("BSER payload has trailing bytes"));
    }
    Ok(value)
}

pub fn encode_frame(value: &Value, limits: Limits) -> Result<Vec<u8>, BserError> {
    let mut payload = Vec::new();
    encode_value(value, &mut payload, limits, 0)?;
    if payload.len() > limits.frame_bytes {
        return Err(BserError::new(
            "encoded BSER payload exceeds its byte limit",
        ));
    }
    let mut frame = Vec::with_capacity(HEADER.len() + 9 + payload.len());
    frame.extend_from_slice(HEADER);
    encode_integer(
        i64::try_from(payload.len()).map_err(|_| BserError::new("BSER payload overflow"))?,
        &mut frame,
    );
    frame.extend_from_slice(&payload);
    Ok(frame)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
    limits: Limits,
    items: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8], limits: Limits) -> Self {
        Self {
            bytes,
            offset: 0,
            limits,
            items: 0,
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], BserError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| BserError::new("BSER offset overflow"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| BserError::new("truncated BSER value"))?;
        self.offset = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, BserError> {
        Ok(self.take(1)?[0])
    }

    fn integer(&mut self) -> Result<i64, BserError> {
        match self.byte()? {
            INT8 => Ok(i64::from(i8::from_le_bytes(
                self.take(1)?.try_into().unwrap(),
            ))),
            INT16 => Ok(i64::from(i16::from_le_bytes(
                self.take(2)?.try_into().unwrap(),
            ))),
            INT32 => Ok(i64::from(i32::from_le_bytes(
                self.take(4)?.try_into().unwrap(),
            ))),
            INT64 => Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            marker => Err(BserError::new(format!(
                "expected BSER integer, found marker {marker:#x}"
            ))),
        }
    }

    fn count(&mut self) -> Result<usize, BserError> {
        let count = usize::try_from(self.integer()?)
            .map_err(|_| BserError::new("negative BSER collection length"))?;
        self.items = self
            .items
            .checked_add(count)
            .ok_or_else(|| BserError::new("BSER item count overflow"))?;
        if self.items > self.limits.collection_items {
            return Err(BserError::new("BSER collection item limit exceeded"));
        }
        Ok(count)
    }

    fn string(&mut self) -> Result<Vec<u8>, BserError> {
        let marker = self.byte()?;
        if marker != STRING && marker != UTF8_STRING {
            return Err(BserError::new("BSER object key is not a string"));
        }
        let length = usize::try_from(self.integer()?)
            .map_err(|_| BserError::new("negative BSER string length"))?;
        if length > self.limits.string_bytes {
            return Err(BserError::new("BSER string limit exceeded"));
        }
        let value = self.take(length)?.to_vec();
        if marker == UTF8_STRING && std::str::from_utf8(&value).is_err() {
            return Err(BserError::new("BSER UTF-8 string is malformed"));
        }
        Ok(value)
    }

    fn value(&mut self, depth: usize) -> Result<Value, BserError> {
        if depth > self.limits.depth {
            return Err(BserError::new("BSER nesting limit exceeded"));
        }
        match self.byte()? {
            NULL => Ok(Value::Null),
            TRUE => Ok(Value::Bool(true)),
            FALSE => Ok(Value::Bool(false)),
            marker @ (STRING | UTF8_STRING) => {
                let length = usize::try_from(self.integer()?)
                    .map_err(|_| BserError::new("negative BSER string length"))?;
                if length > self.limits.string_bytes {
                    return Err(BserError::new("BSER string limit exceeded"));
                }
                let value = self.take(length)?.to_vec();
                if marker == UTF8_STRING && std::str::from_utf8(&value).is_err() {
                    return Err(BserError::new("BSER UTF-8 string is malformed"));
                }
                Ok(Value::Bytes(value))
            }
            ARRAY => {
                let count = self.count()?;
                let mut values = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    values.push(self.value(depth + 1)?);
                }
                Ok(Value::Array(values))
            }
            OBJECT => {
                let count = self.count()?;
                let mut values = BTreeMap::new();
                for _ in 0..count {
                    let key = self.string()?;
                    if values.insert(key, self.value(depth + 1)?).is_some() {
                        return Err(BserError::new("duplicate BSER object key"));
                    }
                }
                Ok(Value::Object(values))
            }
            marker @ (INT8 | INT16 | INT32 | INT64) => {
                self.offset -= 1;
                debug_assert_eq!(self.bytes[self.offset], marker);
                Ok(Value::Integer(self.integer()?))
            }
            marker => Err(BserError::new(format!(
                "unsupported BSER marker {marker:#x}"
            ))),
        }
    }
}

fn encode_value(
    value: &Value,
    output: &mut Vec<u8>,
    limits: Limits,
    depth: usize,
) -> Result<(), BserError> {
    if depth > limits.depth {
        return Err(BserError::new("BSER nesting limit exceeded"));
    }
    match value {
        Value::Null => output.push(NULL),
        Value::Bool(true) => output.push(TRUE),
        Value::Bool(false) => output.push(FALSE),
        Value::Integer(value) => encode_integer(*value, output),
        Value::Bytes(bytes) => encode_string(bytes, output, limits)?,
        Value::Array(values) => {
            if values.len() > limits.collection_items {
                return Err(BserError::new("BSER collection item limit exceeded"));
            }
            output.push(ARRAY);
            encode_integer(values.len() as i64, output);
            for value in values {
                encode_value(value, output, limits, depth + 1)?;
            }
        }
        Value::Object(values) => {
            if values.len() > limits.collection_items {
                return Err(BserError::new("BSER collection item limit exceeded"));
            }
            output.push(OBJECT);
            encode_integer(values.len() as i64, output);
            for (key, value) in values {
                encode_string(key, output, limits)?;
                encode_value(value, output, limits, depth + 1)?;
            }
        }
    }
    if output.len() > limits.frame_bytes {
        return Err(BserError::new(
            "encoded BSER payload exceeds its byte limit",
        ));
    }
    Ok(())
}

fn encode_string(bytes: &[u8], output: &mut Vec<u8>, limits: Limits) -> Result<(), BserError> {
    if bytes.len() > limits.string_bytes {
        return Err(BserError::new("BSER string limit exceeded"));
    }
    output.push(if std::str::from_utf8(bytes).is_ok() {
        UTF8_STRING
    } else {
        STRING
    });
    encode_integer(bytes.len() as i64, output);
    output.extend_from_slice(bytes);
    Ok(())
}

fn encode_integer(value: i64, output: &mut Vec<u8>) {
    if let Ok(value) = i8::try_from(value) {
        output.push(INT8);
        output.extend_from_slice(&value.to_le_bytes());
    } else if let Ok(value) = i16::try_from(value) {
        output.push(INT16);
        output.extend_from_slice(&value.to_le_bytes());
    } else if let Ok(value) = i32::try_from(value) {
        output.push(INT32);
        output.extend_from_slice(&value.to_le_bytes());
    } else {
        output.push(INT64);
        output.extend_from_slice(&value.to_le_bytes());
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BserError {
    message: String,
}

impl BserError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BserError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_v2_object_with_raw_bytes() {
        let value = Value::Object(BTreeMap::from([
            (
                b"clock".to_vec(),
                Value::Bytes(b"c:btrfs-awacs:1:x".to_vec()),
            ),
            (
                b"files".to_vec(),
                Value::Array(vec![Value::Bytes(vec![0xff, b'x'])]),
            ),
        ]));
        let frame = encode_frame(&value, Limits::default()).unwrap();
        assert_eq!(&frame[..6], HEADER);
        assert!(frame.contains(&STRING));
        assert_eq!(decode_frame(&frame, Limits::default()).unwrap(), value);
    }

    #[test]
    fn encodes_valid_text_with_the_bser_v2_utf8_marker() {
        let value = Value::Bytes(b"watch-project".to_vec());
        let frame = encode_frame(&value, Limits::default()).unwrap();
        // Header and the small payload-length integer occupy eight bytes.
        assert_eq!(frame[8], UTF8_STRING);
        assert_eq!(decode_frame(&frame, Limits::default()).unwrap(), value);
    }

    #[test]
    fn rejects_malformed_utf8_marked_strings_but_accepts_raw_bytes() {
        let mut utf8_payload = vec![UTF8_STRING];
        encode_integer(1, &mut utf8_payload);
        utf8_payload.push(0xff);
        let mut utf8_frame = HEADER.to_vec();
        encode_integer(utf8_payload.len() as i64, &mut utf8_frame);
        utf8_frame.extend_from_slice(&utf8_payload);
        assert_eq!(
            decode_frame(&utf8_frame, Limits::default())
                .unwrap_err()
                .to_string(),
            "BSER UTF-8 string is malformed"
        );

        utf8_payload[0] = STRING;
        let mut raw_frame = HEADER.to_vec();
        encode_integer(utf8_payload.len() as i64, &mut raw_frame);
        raw_frame.extend_from_slice(&utf8_payload);
        assert_eq!(
            decode_frame(&raw_frame, Limits::default()).unwrap(),
            Value::Bytes(vec![0xff])
        );
    }

    #[test]
    fn rejects_declared_lengths_and_nesting_before_large_allocation() {
        let mut truncated = HEADER.to_vec();
        encode_integer(1_000_000, &mut truncated);
        assert!(decode_frame(&truncated, Limits::default()).is_err());

        let nested = Value::Array(vec![Value::Array(vec![Value::Null])]);
        let limits = Limits {
            depth: 0,
            ..Limits::default()
        };
        assert!(encode_frame(&nested, limits).is_err());
    }
}
