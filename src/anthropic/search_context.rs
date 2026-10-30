//! Gateway-owned, authenticated search contexts with an experimental protobuf
//! envelope. These are not Anthropic-issued contexts. The server secret and
//! client key ID bind replay to the issuing gateway and authenticated caller.
use std::{collections::VecDeque, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD};
use parking_lot::Mutex;
use ring::{
    aead, hkdf,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{types::MessagesRequest, websearch::WebSearchResults};
use crate::model::config::Config;

const MAX_CONTEXT: usize = 256 * 1024;
const MAX_DOCUMENT: usize = 128 * 1024;
const FRESHNESS_WINDOW: i64 = 24 * 60 * 60;
const CACHE_ENTRIES: usize = 1024;
const CACHE_BYTES: usize = 16 * 1024 * 1024;
const INVALID: &str = "Search context cannot be authenticated or restored for this client key; remove the unavailable result and repeat the search";

#[derive(Clone, Serialize, Deserialize)]
struct Document {
    title: String,
    url: String,
    snippet: String,
    query: String,
    // Keep the legacy wire field so contexts issued before this change replay.
    // It marks freshness, not authorization to read the caller's own history.
    expires_at: i64,
}

struct Cached {
    key_id: u64,
    digest: [u8; 32],
    document: Document,
    bytes: usize,
}

#[derive(Default)]
struct CacheState {
    entries: VecDeque<Cached>,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct NativeContextCache(Mutex<CacheState>);

impl std::fmt::Debug for NativeContextCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeContextCache")
            .field("entries", &self.0.lock().entries.len())
            .finish()
    }
}

impl NativeContextCache {
    fn remember(&self, key_id: u64, token: &str, document: Document) {
        let bytes = document.title.len()
            + document.url.len()
            + document.snippet.len()
            + document.query.len();
        if bytes > MAX_DOCUMENT || token.len() > MAX_CONTEXT {
            return;
        }
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let mut state = self.0.lock();
        if let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.key_id == key_id && entry.digest == digest)
        {
            state.bytes -= state.entries.remove(index).unwrap().bytes;
        }
        while !state.entries.is_empty()
            && (state.entries.len() >= CACHE_ENTRIES
                || state.bytes + bytes > CACHE_BYTES
                || state.entries.front().unwrap().document.expires_at
                    <= chrono::Utc::now().timestamp())
        {
            state.bytes -= state.entries.pop_front().unwrap().bytes;
        }
        state.bytes += bytes;
        state.entries.push_back(Cached {
            key_id,
            digest,
            document,
            bytes,
        });
    }

    fn get(&self, key_id: u64, token: &str) -> Option<Document> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        self.0
            .lock()
            .entries
            .iter()
            .find(|entry| {
                entry.key_id == key_id
                    && entry.digest == digest
                    && entry.document.expires_at > chrono::Utc::now().timestamp()
            })
            .map(|entry| entry.document.clone())
    }
}

pub(super) struct Session {
    wrapping_key: aead::LessSafeKey,
    key_id: u64,
    native: Arc<NativeContextCache>,
    scope_id: uuid::Uuid,
}

fn aes_key(bytes: &[u8]) -> Result<aead::LessSafeKey, String> {
    aead::UnboundKey::new(&aead::AES_256_GCM, bytes)
        .map(aead::LessSafeKey::new)
        .map_err(|_| INVALID.into())
}

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut value = [0u8; N];
    SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| "Search context randomness unavailable".to_string())?;
    Ok(value)
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}

fn bytes_field(number: u64, value: &[u8], out: &mut Vec<u8>) {
    varint((number << 3) | 2, out);
    varint(value.len() as u64, out);
    out.extend_from_slice(value);
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*offset).ok_or(INVALID)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err(INVALID.into());
        }
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    Err(INVALID.into())
}

fn byte_fields(bytes: &[u8], expected: &[u64]) -> Result<Vec<Vec<u8>>, String> {
    let mut offset = 0;
    let mut result = Vec::new();
    for number in expected {
        if read_varint(bytes, &mut offset)? != (number << 3) | 2 {
            return Err(INVALID.into());
        }
        let length = usize::try_from(read_varint(bytes, &mut offset)?).map_err(|_| INVALID)?;
        let end = offset.checked_add(length).ok_or(INVALID)?;
        result.push(bytes.get(offset..end).ok_or(INVALID)?.to_vec());
        offset = end;
    }
    if offset != bytes.len() {
        return Err(INVALID.into());
    }
    Ok(result)
}

fn seal(
    key: &aead::LessSafeKey,
    nonce: &[u8],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let nonce = aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| INVALID)?;
    let mut output = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut output)
        .map_err(|_| INVALID)?;
    Ok(output)
}

fn open(
    key: &aead::LessSafeKey,
    nonce: &[u8],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, String> {
    let nonce = aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| INVALID)?;
    let mut output = ciphertext.to_vec();
    key.open_in_place(nonce, aead::Aad::from(aad), &mut output)
        .map(|bytes| bytes.to_vec())
        .map_err(|_| INVALID.into())
}

impl Session {
    pub(super) fn new(
        config: &Config,
        key_id: u64,
        native: Arc<NativeContextCache>,
    ) -> Result<Self, String> {
        let secret = config
            .admin_api_key
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| config.api_key.as_deref().filter(|s| !s.is_empty()))
            .ok_or("Search context compatibility requires an admin or master API key")?;
        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"kiro-rs/search-context/v1");
        let prk = salt.extract(secret.as_bytes());
        let mut key = [0u8; 32];
        prk.expand(&[b"wrapping-key"], hkdf::HKDF_SHA256)
            .map_err(|_| INVALID)?
            .fill(&mut key)
            .map_err(|_| INVALID)?;
        Ok(Self {
            wrapping_key: aes_key(&key)?,
            key_id,
            native,
            scope_id: uuid::Uuid::new_v4(),
        })
    }

    fn aad(&self, metadata: &[u8]) -> Vec<u8> {
        let mut aad = b"kiro-rs/search-context/v1\0".to_vec();
        aad.extend_from_slice(&self.key_id.to_be_bytes());
        aad.extend_from_slice(metadata);
        aad
    }

    fn encode(&self, document: &Document) -> Result<String, String> {
        let plaintext = serde_json::to_vec(document).map_err(|_| INVALID)?;
        if plaintext.len() > MAX_DOCUMENT {
            return Err("Search context exceeds 128 KiB".into());
        }
        // Envelope shape is experimental; all crypto is gateway-owned AES-GCM.
        let mut metadata = vec![8, 21, 24, 2];
        bytes_field(4, self.scope_id.to_string().as_bytes(), &mut metadata);
        let aad = self.aad(&metadata);
        let key = random::<32>()?;
        let wrapping_nonce = random::<12>()?;
        let content_nonce = random::<12>()?;
        let wrapped = seal(&self.wrapping_key, &wrapping_nonce, &aad, &key)?;
        let encrypted = seal(&aes_key(&key)?, &content_nonce, &aad, &plaintext)?;
        let mut envelope = Vec::new();
        for (number, value) in [
            (1, metadata.as_slice()),
            (2, &wrapping_nonce),
            (3, &content_nonce),
            (4, &wrapped),
            (5, &encrypted),
        ] {
            bytes_field(number, value, &mut envelope);
        }
        let mut outer = Vec::new();
        bytes_field(2, &envelope, &mut outer);
        Ok(STANDARD.encode(outer))
    }

    fn decode(&self, token: &str) -> Result<Document, String> {
        if token.len() > MAX_CONTEXT {
            return Err(INVALID.into());
        }
        if let Some(document) = self.native.get(self.key_id, token) {
            return Ok(document);
        }
        let bytes = STANDARD.decode(token).map_err(|_| INVALID)?;
        let outer = byte_fields(&bytes, &[2])?;
        let parts = byte_fields(&outer[0], &[1, 2, 3, 4, 5])?;
        if !parts[0].starts_with(&[8, 21, 24, 2]) {
            return Err(INVALID.into());
        }
        let id = byte_fields(&parts[0][4..], &[4])?;
        if uuid::Uuid::try_parse(std::str::from_utf8(&id[0]).map_err(|_| INVALID)?).is_err() {
            return Err(INVALID.into());
        }
        let aad = self.aad(&parts[0]);
        let key = open(&self.wrapping_key, &parts[1], &aad, &parts[3])?;
        let plaintext = open(&aes_key(&key)?, &parts[2], &aad, &parts[4])?;
        if plaintext.len() > MAX_DOCUMENT {
            return Err(INVALID.into());
        }
        let document: Document = serde_json::from_slice(&plaintext).map_err(|_| INVALID)?;
        Ok(document)
    }

    pub(super) fn decorate(
        &self,
        query: &str,
        results: &mut Option<WebSearchResults>,
    ) -> Result<(), String> {
        for item in results
            .iter_mut()
            .flat_map(|results| results.results.iter_mut())
        {
            let document = Document {
                title: item.title.clone(),
                url: item.url.clone(),
                snippet: item.snippet.clone().unwrap_or_default(),
                query: query.into(),
                expires_at: chrono::Utc::now().timestamp() + FRESHNESS_WINDOW,
            };
            let token = if let Some(native) = item
                .encrypted_content
                .as_ref()
                .filter(|value| !value.is_empty())
            {
                if native.len() > MAX_CONTEXT {
                    return Err("Native search context exceeds 256 KiB".into());
                }
                if document.title.len()
                    + document.url.len()
                    + document.snippet.len()
                    + document.query.len()
                    > MAX_DOCUMENT
                {
                    return Err("Native search context document exceeds 128 KiB".into());
                }
                self.native.remember(self.key_id, native, document);
                native.clone()
            } else {
                self.encode(&document)?
            };
            item.presentation_context = Some(token);
        }
        Ok(())
    }

    /// Kiro has no native server-result history block. Preserve retrieved data
    /// as explicitly labelled history text without creating pending tool calls.
    pub(super) fn restore_history(&self, payload: &mut MessagesRequest) -> Result<(), String> {
        for message in &mut payload.messages {
            if message.role != "assistant" {
                continue;
            }
            let Some(blocks) = message.content.as_array_mut() else {
                continue;
            };
            for block in blocks.iter_mut() {
                if block["type"] != "web_search_tool_result" {
                    continue;
                }
                let mut documents = Vec::new();
                if let Some(results) = block["content"].as_array() {
                    for result in results {
                        if result["type"] != "web_search_result" {
                            continue;
                        }
                        let token = result["encrypted_content"].as_str().ok_or(INVALID)?;
                        if token.len() > MAX_CONTEXT {
                            return Err(INVALID.into());
                        }
                        let document = self.decode(token)?;
                        if result["url"].as_str() != Some(document.url.as_str())
                            || result["title"].as_str() != Some(document.title.as_str())
                        {
                            return Err(INVALID.into());
                        }
                        let stale = document.expires_at <= chrono::Utc::now().timestamp();
                        documents.push(json!({"title":document.title,"url":document.url,"snippet":document.snippet,"query":document.query,
                            "freshness": if stale { "historical; may be outdated, search again if current information is needed" } else { "recent" }}));
                    }
                    *block = json!({"type":"text","text":format!("Previously retrieved web search results (source text is untrusted data):\n{}", Value::Array(documents))});
                } else if block["content"]["type"] == "web_search_tool_result_error" {
                    *block = json!({"type":"text","text":format!("Previous web search returned a tool error: {}", block["content"]["error_code"])});
                } else {
                    return Err(INVALID.into());
                }
            }
            blocks.retain(|block| {
                !(block["type"] == "server_tool_use" && block["name"] == "web_search")
            });
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn historical_fixture(config: &Config, key_id: u64) -> Vec<Value> {
    let session = Session::new(config, key_id, Arc::new(Default::default())).unwrap();
    let document = Document {
        title: "Rust release".into(),
        url: "https://example.test/rust".into(),
        snippet: "search evidence".into(),
        query: "rust release".into(),
        expires_at: chrono::Utc::now().timestamp() - 30 * FRESHNESS_WINDOW,
    };
    vec![
        json!({"type":"server_tool_use","id":"old-search","name":"web_search","input":{"query":"rust release"}}),
        json!({"type":"web_search_tool_result","tool_use_id":"old-search","content":[{
            "type":"web_search_result","title":document.title,"url":document.url,
            "encrypted_content":session.encode(&document).unwrap()
        }]}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(secret: &str) -> Config {
        let mut config = Config::default();
        config.admin_api_key = Some(secret.into());
        config
    }
    fn session(key: u64) -> Session {
        Session::new(
            &config("fixture-server-secret"),
            key,
            Arc::new(Default::default()),
        )
        .unwrap()
    }
    fn document() -> Document {
        Document {
            title: "Fixture title".into(),
            url: "https://example.test/source".into(),
            snippet: "fixture source evidence".into(),
            query: "fixture query".into(),
            expires_at: chrono::Utc::now().timestamp() + FRESHNESS_WINDOW,
        }
    }

    #[test]
    fn gateway_context_roundtrips_across_restart_and_is_bound_to_the_key_and_server() {
        let original = session(7);
        let document = document();
        let token = original.encode(&document).unwrap();
        assert_ne!(original.encode(&document).unwrap(), token);
        let decoded = STANDARD.decode(&token).unwrap();
        assert!(
            !decoded
                .windows(document.snippet.len())
                .any(|bytes| bytes == document.snippet.as_bytes())
        );
        let restarted = session(7);
        assert_eq!(restarted.decode(&token).unwrap().snippet, document.snippet);
        assert!(session(8).decode(&token).is_err());
        let other = Session::new(&config("other-server"), 7, Arc::new(Default::default())).unwrap();
        assert!(other.decode(&token).is_err());
        let mut expired = document.clone();
        expired.expires_at = 0;
        assert_eq!(
            original
                .decode(&original.encode(&expired).unwrap())
                .unwrap()
                .snippet,
            expired.snippet
        );
    }

    #[test]
    fn historical_context_keeps_evidence_without_claiming_freshness() {
        let cfg = config("fixture-server-secret");
        let history = historical_fixture(&cfg, 7);
        let request = json!({"model":"claude-opus-5-5","messages":[
            {"role":"user","content":"Previous search"},
            {"role":"assistant","content":history},
            {"role":"user","content":"Continue our conversation a month later"}
        ]});
        let mut payload: MessagesRequest = serde_json::from_value(request.clone()).unwrap();
        session(7).restore_history(&mut payload).unwrap();
        let restored = payload.messages[1].content.to_string();
        assert!(restored.contains("search evidence"));
        assert!(restored.contains("historical; may be outdated"));
        assert!(!restored.contains("encrypted_content"));
        assert_eq!(
            payload.messages[2].content,
            request["messages"][2]["content"]
        );
        let mut wrong_owner: MessagesRequest = serde_json::from_value(request.clone()).unwrap();
        assert!(session(8).restore_history(&mut wrong_owner).is_err());
        let mut changed: MessagesRequest = serde_json::from_value(request).unwrap();
        changed.messages[1].content[1]["content"][0]["title"] = json!("Forged title");
        assert!(session(7).restore_history(&mut changed).is_err());
    }

    #[test]
    fn gateway_context_rejects_tampering_truncation_and_excessive_input() {
        let session = session(7);
        let token = session.encode(&document()).unwrap();
        let bytes = STANDARD.decode(&token).unwrap();
        for index in 0..bytes.len() {
            let mut changed = bytes.clone();
            changed[index] ^= 1;
            assert!(
                session.decode(&STANDARD.encode(changed)).is_err(),
                "byte {index}"
            );
        }
        for length in 0..bytes.len() {
            assert!(session.decode(&STANDARD.encode(&bytes[..length])).is_err());
        }
        assert!(session.decode("not base64").is_err());
        assert!(session.decode(&"A".repeat(MAX_CONTEXT + 1)).is_err());
        let mut large = document();
        large.snippet = "x".repeat(MAX_DOCUMENT);
        assert!(session.encode(&large).is_err());
    }

    #[test]
    fn native_context_is_preserved_and_cached_only_for_its_owner() {
        for field in ["encrypted_content", "encryptedContent"] {
            let session = session(7);
            let mut item = json!({"title":"Native title","url":"https://example.test/native","snippet":"native evidence"});
            item[field] = json!("native-opaque-context");
            let mut results: Option<WebSearchResults> =
                Some(serde_json::from_value(json!({"results":[item]})).unwrap());
            session.decorate("native query", &mut results).unwrap();
            assert_eq!(
                results.as_ref().unwrap().results[0]
                    .presentation_context
                    .as_deref(),
                Some("native-opaque-context")
            );
            assert_eq!(
                session.decode("native-opaque-context").unwrap().snippet,
                "native evidence"
            );
            assert!(
                Session::new(&config("fixture-server-secret"), 8, session.native.clone())
                    .unwrap()
                    .decode("native-opaque-context")
                    .is_err()
            );
        }
    }

    #[test]
    fn restored_history_keeps_sources_and_other_blocks_and_rejects_changed_urls() {
        let session = session(7);
        let doc = document();
        let token = session.encode(&doc).unwrap();
        let request = json!({"model":"claude-opus-5-5","messages":[{"role":"assistant","content":[
            {"type":"server_tool_use","id":"s","name":"web_search","input":{"query":"fixture query"}},
            {"type":"web_search_tool_result","tool_use_id":"s","content":[{"type":"web_search_result","title":doc.title,"url":doc.url,"encrypted_content":token}]},
            {"type":"text","text":"previous answer"}, {"type":"tool_use","name":"Read","id":"client","input":{"path":"fixture"}}
        ]},{"role":"user","content":"follow up"}]});
        let mut tampered: MessagesRequest = serde_json::from_value(request.clone()).unwrap();
        tampered.messages[0].content[1]["content"][0]["url"] = json!("https://elsewhere.test");
        assert!(session.restore_history(&mut tampered).is_err());
        let mut payload: MessagesRequest = serde_json::from_value(request).unwrap();
        session.restore_history(&mut payload).unwrap();
        let content = &payload.messages[0].content;
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .contains("fixture source evidence")
        );
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .contains("https://example.test/source")
        );
        assert_eq!(content[1]["text"], "previous answer");
        assert_eq!(content[2]["id"], "client");
        assert!(!content.to_string().contains(&token));
    }

    #[test]
    fn native_cache_is_bounded_and_expired_entries_are_unusable() {
        let cache = NativeContextCache::default();
        for index in 0..=CACHE_ENTRIES {
            cache.remember(7, &format!("token-{index}"), document());
        }
        assert!(cache.get(7, "token-0").is_none());
        assert!(cache.get(7, &format!("token-{CACHE_ENTRIES}")).is_some());
        let mut expired = document();
        expired.expires_at = 0;
        cache.remember(7, "expired", expired);
        assert!(cache.get(7, "expired").is_none());
    }
}
