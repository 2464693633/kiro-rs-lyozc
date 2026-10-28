//! Translate document blocks into readable text before Kiro conversion.
//! Kiro has no verified PDF wire field. This adapter supports text PDFs rather
//! than silently discarding the attachment; scanned PDFs need a separate OCR path.

use super::types::MessagesRequest;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

const MAX_DOCUMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
static DOCUMENT_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub(super) async fn prepare(payload: &mut MessagesRequest) -> Result<(), String> {
    if !payload
        .messages
        .iter()
        .any(|m| contains_document(&m.content))
    {
        return Ok(());
    }
    let permit = DOCUMENT_WORKERS
        .acquire()
        .await
        .map_err(|_| "Document adapter unavailable")?;
    let mut messages = payload.messages.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut total = 0;
        for message in &mut messages {
            convert_blocks(&mut message.content, &mut total)?;
        }
        Ok::<_, String>(messages)
    })
    .await
    .map_err(|_| "Document conversion failed".to_string())?;
    payload.messages = result?;
    Ok(())
}

fn contains_document(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_document),
        Value::Object(obj) => {
            obj.get("type").and_then(Value::as_str) == Some("document")
                || obj.get("content").is_some_and(contains_document)
        }
        _ => false,
    }
}

fn convert_blocks(value: &mut Value, total: &mut usize) -> Result<(), String> {
    match value {
        Value::Array(items) => {
            for item in items {
                convert_blocks(item, total)?;
            }
        }
        Value::Object(obj) if obj.get("type").and_then(Value::as_str) == Some("document") => {
            let text = document_text(value)?;
            *total += text.len();
            if *total > MAX_TEXT_BYTES {
                return Err("Extracted documents exceed the 2 MiB text limit".into());
            }
            let mut block = json!({"type":"text", "text":text});
            if let Some(cache) = value.get("cache_control") {
                block["cache_control"] = cache.clone();
            }
            *value = block;
        }
        Value::Object(obj) => {
            if let Some(content) = obj.get_mut("content") {
                convert_blocks(content, total)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn document_text(block: &Value) -> Result<String, String> {
    if block.pointer("/citations/enabled").and_then(Value::as_bool) == Some(true) {
        return Err("Document citations are not supported by this text extraction adapter".into());
    }
    let source = block.get("source").ok_or("document.source is required")?;
    let text = match source.get("type").and_then(Value::as_str) {
        Some("text") => source.get("data").and_then(Value::as_str)
            .ok_or("Text document source.data must be a string")?.to_owned(),
        Some("base64") => {
            let data = source.get("data").and_then(Value::as_str)
                .ok_or("Base64 document source.data must be a string")?;
            if data.len() > MAX_DOCUMENT_BYTES.div_ceil(3) * 4 {
                return Err("Document exceeds the 10 MiB limit".into());
            }
            let bytes = STANDARD.decode(data).map_err(|_| "Invalid document base64")?;
            if bytes.len() > MAX_DOCUMENT_BYTES { return Err("Document exceeds the 10 MiB limit".into()); }
            match source.get("media_type").and_then(Value::as_str) {
                Some("application/pdf") => {
                    if !bytes.starts_with(b"%PDF-") { return Err("Invalid PDF header".into()); }
                    pdf_extract::extract_text_from_mem(&bytes)
                        .map_err(|_| "Cannot extract text from this PDF (invalid or encrypted document)")?
                },
                Some("text/plain") => String::from_utf8(bytes).map_err(|_| "Document must be UTF-8")?,
                _ => return Err("Supported document media types: application/pdf and text/plain".into()),
            }
        },
        Some("content") => {
            let content = source.get("content").ok_or("document.source.content is required")?;
            if let Some(text) = content.as_str() { text.to_owned() } else {
                let blocks = content.as_array().ok_or("Document content must be text or text blocks")?;
                let mut parts = Vec::new();
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("text") {
                        return Err("Only text blocks are supported in document.source.content".into());
                    }
                    parts.push(block.get("text").and_then(Value::as_str).ok_or("Document text is missing")?);
                }
                parts.join("\n")
            }
        },
        _ => return Err("Document source must be inline base64, text or content; URL/file sources are unsupported".into()),
    };
    if text.trim().is_empty() {
        return Err("Document contains no extractable text; scanned PDFs require OCR".into());
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err("Extracted document exceeds the 2 MiB text limit".into());
    }
    let title = block
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Attached document");
    Ok(format!("{title}\n[Document text]\n{text}\n[End document]"))
}

#[cfg(test)]
pub(crate) fn test_pdf(text: &str) -> Vec<u8> {
    let stream = format!("BT /F1 20 Tf 50 750 Td ({text}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }
    let xref = pdf.len();
    pdf.push_str("xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    pdf.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_real_pdf_text() {
        for code in ["AB72TEST", "X99OTHER"] {
            let block = json!({"type":"document","source":{"type":"base64",
                "media_type":"application/pdf","data":STANDARD.encode(test_pdf(code))}});
            assert!(document_text(&block).unwrap().contains(code));
        }
    }

    #[test]
    fn rejects_bad_or_unsupported_documents() {
        for source in [
            json!({"type":"url","url":"http://127.0.0.1/private"}),
            json!({"type":"base64","media_type":"application/pdf","data":"!!!"}),
            json!({"type":"base64","media_type":"application/pdf","data":STANDARD.encode(b"%PDF-1.4 broken")}),
            json!({"type":"text","data":" "}),
        ] {
            assert!(document_text(&json!({"type":"document","source":source})).is_err());
        }
    }

    #[test]
    fn converts_nested_documents_and_preserves_cache() {
        let mut content = json!([{"type":"tool_result","tool_use_id":"t", "content":[
            {"type":"document","source":{"type":"text","data":"文件内容"},"cache_control":{"type":"ephemeral"}}
        ]}]);
        convert_blocks(&mut content, &mut 0).unwrap();
        assert_eq!(content[0]["content"][0]["type"], "text");
        assert!(
            content[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("文件内容")
        );
        assert_eq!(
            content[0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    #[ignore = "optional private replay: set KIRO_PDF_CASES_PATH to a local JSON array of {source, expected}"]
    fn extracts_local_pdf_cases() {
        let path =
            std::env::var_os("KIRO_PDF_CASES_PATH").expect("KIRO_PDF_CASES_PATH is required");
        let cases: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let cases = cases.as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let text = document_text(&json!({"type":"document","source":case["source"]})).unwrap();
            assert!(text.contains(case["expected"].as_str().unwrap()));
        }
        println!("Validated {} private PDF cases", cases.len());
    }
}
