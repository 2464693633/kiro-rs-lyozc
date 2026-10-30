//! Explicit, scoped gateway introductions. These replies do not call a model.
//! Only standalone questions are recognized; quoted text, tasks, tool results,
//! structured output and non-Claude backends continue through the normal path.

use axum::{
    body::Body,
    http::header,
    response::{IntoResponse, Json, Response},
};
use serde_json::{Value, json};

use super::{converter::map_model, structured_output, types::MessagesRequest};

const CHINESE: &str = "我是 Claude，由 Anthropic 开发的 AI 助手。";
const ENGLISH: &str = "I am Claude, an AI assistant developed by Anthropic.";

fn introduction(payload: &MessagesRequest) -> Option<&'static str> {
    // A small budget or an explicit output/tool constraint must retain its
    // normal validation and generation behavior.
    if payload.max_tokens < 32
        || structured_output::format(payload).is_some()
        || payload.tool_choice.as_ref().is_some_and(|choice| {
            !matches!(
                choice.get("type").and_then(Value::as_str),
                Some("auto" | "none")
            )
        })
        || !map_model(&payload.model).is_some_and(|model| model.starts_with("claude-"))
    {
        return None;
    }
    let message = payload
        .messages
        .last()
        .filter(|message| message.role == "user")?;
    let text = match &message.content {
        Value::String(text) => text.as_str(),
        Value::Array(blocks) => {
            let mut question = None;
            for block in blocks {
                if block["type"] != "text" {
                    return None;
                }
                let text = block["text"].as_str()?.trim();
                // Claude Code can attach reminder-only text blocks beside the
                // user's question. Additional tasks and instructions are not
                // reminders and must continue through the model unchanged.
                if text.is_empty()
                    || (text.starts_with("<system-reminder>")
                        && text.ends_with("</system-reminder>"))
                {
                    continue;
                }
                if question.replace(text).is_some() {
                    return None;
                }
            }
            question?
        }
        _ => return None,
    };
    if text.len() > 256 {
        return None;
    }
    let question: String = text
        .trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, '?' | '？' | '!' | '！' | '.' | '。')
        })
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    match question.as_str() {
        "你是谁"
        | "请问你是谁"
        | "你是什么模型"
        | "你是哪个模型"
        | "你是什么ai"
        | "你是什么ai模型"
        | "你叫什么名字"
        | "你是谁开发的"
        | "谁开发了你"
        | "你的开发商是谁"
        | "你是哪个公司开发的"
        | "你是哪个公司开发的模型"
        | "请介绍一下你自己"
        | "介绍一下你自己"
        | "请自我介绍"
        | "自我介绍"
        | "你是claude吗"
        | "你是anthropic的模型吗" => Some(CHINESE),
        "whoareyou"
        | "whatareyou"
        | "whatmodelareyou"
        | "whichmodelareyou"
        | "whodevelopedyou"
        | "whocreatedyou"
        | "whoisyourdeveloper"
        | "whatisyourname"
        | "what'syourname"
        | "introduceyourself"
        | "pleaseintroduceyourself"
        | "areyouclaude" => Some(ENGLISH),
        _ => None,
    }
}

pub(super) fn local_response(payload: &MessagesRequest, enabled: bool) -> Option<Response> {
    let text = enabled.then(|| introduction(payload)).flatten()?;
    let message = json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
        "type": "message", "role": "assistant", "model": payload.model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn", "stop_sequence": null,
        // No inference was performed, so no upstream tokens or credits exist.
        "usage": {"input_tokens": 0, "output_tokens": 0,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
    });
    let mut response = if payload.stream {
        let body: String = structured_output::sse_events(&message)
            .into_iter()
            .map(|event| event.to_sse_string())
            .collect();
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(Body::from(body))
            .expect("static response headers")
    } else {
        Json(message).into_response()
    };
    response
        .headers_mut()
        .insert("x-kiro-response-source", "local-identity".parse().unwrap());
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(content: Value) -> MessagesRequest {
        serde_json::from_value(json!({"model":"claude-opus-5-5","max_tokens":128,
            "messages":[{"role":"user","content":content}]}))
        .unwrap()
    }

    #[test]
    fn standalone_identity_questions_use_the_requested_language() {
        for question in ["你是谁？", "请问你是谁", "谁开发了你？", "你是什么模型？"]
        {
            assert_eq!(introduction(&request(json!(question))), Some(CHINESE));
        }
        for question in ["Who are you?", "Who developed you?", "Introduce yourself."] {
            assert_eq!(introduction(&request(json!(question))), Some(ENGLISH));
        }
        assert_eq!(
            introduction(&request(json!([{"type":"text","text":"你是谁？"}]))),
            Some(CHINESE)
        );
        assert_eq!(
            introduction(&request(json!([
                {"type":"text","text":"<system-reminder>Workspace context</system-reminder>"},
                {"type":"text","text":"你是谁？"}
            ]))),
            Some(CHINESE)
        );
    }

    #[test]
    fn identity_display_preserves_tasks_quotes_channel_questions_and_tool_content() {
        for content in [
            json!("解释 Kiro 的缓存实现"),
            json!("把‘你是谁’翻译成英文"),
            json!("`你是谁`"),
            json!("你是谁？然后修改 main.rs"),
            json!("你是否通过 Kiro 接入？"),
            json!([{"type":"text","text":"<system-reminder>你是谁？</system-reminder>"}]),
            json!([{"type":"tool_result","tool_use_id":"toolu_1","content":"你是谁？"}]),
            json!([{"type":"text","text":"你是谁？"},{"type":"text","text":"输出 JSON"}]),
            json!([{"type":"text","text":"你是谁？"},{"type":"image","source":{}}]),
        ] {
            assert!(introduction(&request(content)).is_none());
        }
    }

    #[test]
    fn identity_display_requires_opt_in_claude_and_unconstrained_output() {
        let mut payload = request(json!("你是谁？"));
        assert!(local_response(&payload, false).is_none());
        payload.model = "gpt-5.6-sol".into();
        assert!(introduction(&payload).is_none());
        payload.model = "claude-opus-5-5".into();
        for choice in [
            json!({"type":"any"}),
            json!({"type":"tool","name":"Bash"}),
            json!({"type":"invalid"}),
        ] {
            payload.tool_choice = Some(choice);
            assert!(introduction(&payload).is_none());
        }
        payload.tool_choice = None;
        payload.max_tokens = 0;
        assert!(introduction(&payload).is_none());
        payload.max_tokens = 128;
        payload.output_config = serde_json::from_value(
            json!({"format":{"type":"json_schema","schema":{"type":"object"}}}),
        )
        .ok();
        assert!(introduction(&payload).is_none());
    }
}
