//! Opt-in experiment matching one observed signature envelope layout.
//! This changes presentation metadata, not the upstream provider or a verified
//! cryptographic identity. Unknown envelopes always pass through byte-for-byte.

use std::{borrow::Cow, ops::Range};

use base64::{Engine, engine::general_purpose::STANDARD};

const MAX_ENCODED_LENGTH: usize = 128 * 1024;
const MAX_FIELDS: usize = 64;

#[derive(Debug)]
struct Field {
    number: u64,
    wire: u8,
    raw: Range<usize>,
    payload: Range<usize>,
    tag_end: usize,
    integer: Option<u64>,
}

fn read_varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*offset)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
    }
    None
}

fn fields(data: &[u8]) -> Option<Vec<Field>> {
    let mut offset = 0usize;
    let mut result = Vec::new();
    while offset < data.len() {
        if result.len() == MAX_FIELDS {
            return None;
        }
        let start = offset;
        let tag = read_varint(data, &mut offset)?;
        let number = tag >> 3;
        if number == 0 || number > (1 << 29) - 1 {
            return None;
        }
        let wire = (tag & 7) as u8;
        let tag_end = offset;
        let (payload, integer) = match wire {
            0 => {
                let value = read_varint(data, &mut offset)?;
                (tag_end..offset, Some(value))
            }
            1 | 5 => {
                offset = offset.checked_add(if wire == 1 { 8 } else { 4 })?;
                (tag_end..offset, None)
            }
            2 => {
                let length = usize::try_from(read_varint(data, &mut offset)?).ok()?;
                let payload_start = offset;
                offset = offset.checked_add(length)?;
                (payload_start..offset, None)
            }
            _ => return None,
        };
        data.get(payload.clone())?;
        result.push(Field {
            number,
            wire,
            raw: start..offset,
            payload,
            tag_end,
            integer,
        });
    }
    Some(result)
}

fn unique_field(fields: &[Field], number: u64, wire: u8) -> Option<&Field> {
    let mut matches = fields.iter().filter(|field| field.number == number);
    let field = matches.next()?;
    (field.wire == wire && matches.next().is_none()).then_some(field)
}

fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 128 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn replace_bytes(data: &[u8], field: &Field, replacement: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    output.extend_from_slice(&data[..field.tag_end]);
    write_varint(replacement.len() as u64, &mut output);
    output.extend_from_slice(replacement);
    output.extend_from_slice(&data[field.raw.end..]);
    output
}

fn rewrite_known_envelope(data: &[u8]) -> Option<Vec<u8>> {
    let outer = fields(data)?;
    if unique_field(&outer, 1, 0)?.integer != Some(4) {
        return None;
    }
    let envelope_field = unique_field(&outer, 2, 2)?;
    let envelope = &data[envelope_field.payload.clone()];
    let envelope_fields = fields(envelope)?;
    let metadata_field = unique_field(&envelope_fields, 1, 2)?;
    let metadata = &envelope[metadata_field.payload.clone()];
    let metadata_fields = fields(metadata)?;
    // Restrict the experiment to the exact five-field layout in our captures.
    // Field numbers have no inferred official names or cryptographic meaning.
    if metadata_fields.len() != 5
        || unique_field(&metadata_fields, 1, 0)?.integer != Some(19)
        || unique_field(&metadata_fields, 2, 0)?.integer != Some(1)
        || unique_field(&metadata_fields, 3, 0)?.integer != Some(2)
        || unique_field(&metadata_fields, 7, 0)?.integer != Some(1)
        || &metadata[unique_field(&metadata_fields, 8, 2)?.payload.clone()] != b"thinking"
    {
        return None;
    }
    let omitted = unique_field(&metadata_fields, 2, 0)?;
    let mut rewritten_metadata = metadata[..omitted.raw.start].to_vec();
    rewritten_metadata.extend_from_slice(&metadata[omitted.raw.end..]);
    let rewritten_envelope = replace_bytes(envelope, metadata_field, &rewritten_metadata);
    Some(replace_bytes(data, envelope_field, &rewritten_envelope))
}

pub(super) fn present(signature: &str, enabled: bool) -> Cow<'_, str> {
    if enabled
        && signature.len() <= MAX_ENCODED_LENGTH
        && let Ok(decoded) = STANDARD.decode(signature)
        && let Some(rewritten) = rewrite_known_envelope(&decoded)
    {
        return Cow::Owned(STANDARD.encode(rewritten));
    }
    Cow::Borrowed(signature)
}

pub(super) fn present_message(message: &mut serde_json::Value, enabled: bool) {
    if !enabled {
        return;
    }
    if let Some(content) = message
        .get_mut("content")
        .and_then(|value| value.as_array_mut())
    {
        for block in content {
            if block["type"] != "thinking" {
                continue;
            }
            if let Some(signature) = block.get("signature").and_then(|value| value.as_str())
                && let Cow::Owned(rewritten) = present(signature, true)
            {
                block["signature"] = rewritten.into();
            }
        }
    }
}

#[cfg(test)]
pub(super) fn fixture_signature() -> String {
    fixture_with_metadata(&[
        8, 19, 16, 1, 24, 2, 56, 1, 66, 8, b't', b'h', b'i', b'n', b'k', b'i', b'n', b'g',
    ])
}

#[cfg(test)]
fn fixture_with_metadata(metadata: &[u8]) -> String {
    // Long payload exercises the enclosing varint length boundaries.
    let opaque = vec![0xa5; 107];
    let mut envelope = vec![10];
    write_varint(metadata.len() as u64, &mut envelope);
    envelope.extend_from_slice(metadata);
    envelope.push(18);
    write_varint(opaque.len() as u64, &mut envelope);
    envelope.extend_from_slice(&opaque);
    let mut outer = vec![8, 4, 18];
    write_varint(envelope.len() as u64, &mut outer);
    outer.extend_from_slice(&envelope);
    // Unknown outer fixed-width fields must remain untouched.
    outer.extend_from_slice(&[29, 1, 2, 3, 4, 33, 1, 2, 3, 4, 5, 6, 7, 8]);
    STANDARD.encode(outer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_envelope_matches_sample_layout_and_keeps_opaque_bytes() {
        let original = fixture_signature();
        let rewritten = present(&original, true).into_owned();
        assert_ne!(rewritten, original);
        let expected_metadata = [
            8, 19, 24, 2, 56, 1, 66, 8, b't', b'h', b'i', b'n', b'k', b'i', b'n', b'g',
        ];
        assert_eq!(rewritten, fixture_with_metadata(&expected_metadata));
        assert_eq!(present(&rewritten, true), rewritten);
        assert!(matches!(present(&original, false), Cow::Borrowed(_)));
    }

    #[test]
    fn unknown_duplicate_and_malformed_envelopes_pass_through() {
        let original = fixture_signature();
        let bytes = STANDARD.decode(&original).unwrap();
        for length in 0..bytes.len() {
            let truncated = STANDARD.encode(&bytes[..length]);
            // A prefix ending before optional outer fields can still be valid.
            if fields(&bytes[..length]).is_none() {
                assert_eq!(present(&truncated, true), truncated);
            }
        }
        for input in ["", "not-base64", "kiro-rs-thinking-signature", "CAQS/w=="] {
            assert_eq!(present(input, true), input);
        }
        for metadata in [
            vec![
                8, 12, 16, 1, 24, 2, 56, 1, 66, 8, b't', b'h', b'i', b'n', b'k', b'i', b'n', b'g',
            ],
            vec![
                8, 19, 16, 1, 16, 1, 24, 2, 56, 1, 66, 8, b't', b'h', b'i', b'n', b'k', b'i', b'n',
                b'g',
            ],
        ] {
            let unknown = fixture_with_metadata(&metadata);
            assert_eq!(present(&unknown, true), unknown);
        }
        let oversized = "A".repeat(MAX_ENCODED_LENGTH + 1);
        assert_eq!(present(&oversized, true), oversized);
        assert!(read_varint(&[0xff; 10], &mut 0).is_none());
    }

    #[test]
    fn only_thinking_signature_fields_are_rewritten() {
        let signature = fixture_signature();
        let mut message = serde_json::json!({"content":[
            {"type":"thinking","thinking":"","signature":signature},
            {"type":"redacted_thinking","data":signature},
            {"type":"text","text":signature},
            {"type":"tool_use","input":{"signature":signature}}
        ]});
        let original = message.clone();
        present_message(&mut message, false);
        assert_eq!(message, original);
        present_message(&mut message, true);
        assert_eq!(
            message["content"][0]["signature"],
            present(&signature, true).as_ref()
        );
        assert_eq!(message["content"][0]["thinking"], "");
        for index in 1..4 {
            assert_eq!(message["content"][index], original["content"][index]);
        }
    }
}
