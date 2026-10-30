//! Explicit, scoped gateway introductions. These replies do not call a model.
//! Only standalone questions are recognized; quoted text, tasks, tool results,
//! structured output and non-Claude backends continue through the normal path.

use axum::{
    body::Body,
    http::header,
    response::{IntoResponse, Json, Response},
};
use serde_json::{Value, json};
use std::borrow::Cow;

use super::{
    converter::map_model,
    structured_output,
    types::{MessagesRequest, SystemMessage},
};

const CHINESE: &str = "我是 Claude，由 Anthropic 开发的 AI 助手。";
const ENGLISH: &str = "I am Claude, an AI assistant developed by Anthropic.";
const REQUEST_MODEL_CONTEXT: &str = include_str!("request_model_context.txt");

#[derive(Clone, Copy)]
struct IdentityIntent {
    chinese: bool,
    model: bool,
}

pub(super) fn strip_edge_reminders(mut text: &str) -> Option<&str> {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    for _ in 0..8 {
        text = text.trim();
        if let Some(rest) = text.strip_prefix(OPEN) {
            let end = rest.find(CLOSE)?;
            if rest[..end].contains(OPEN) {
                return None;
            }
            text = &rest[end + CLOSE.len()..];
        } else if let Some(rest) = text.strip_suffix(CLOSE) {
            let start = rest.rfind(OPEN)?;
            if rest[start + OPEN.len()..].contains(CLOSE) {
                return None;
            }
            text = &rest[..start];
        } else {
            return Some(text);
        }
    }
    None
}

pub(super) fn latest_question(payload: &MessagesRequest) -> Option<Cow<'_, str>> {
    // Claude Code can append environment messages after the actual user turn.
    // Only skip context roles; an assistant/tool turn must not replay an older
    // identity question as a new local reply.
    let message = payload
        .messages
        .iter()
        .rev()
        .find(|message| !matches!(message.role.as_str(), "system" | "developer"))?;
    if message.role != "user" {
        return None;
    }
    let texts: Vec<&str> = match &message.content {
        Value::String(text) => vec![strip_edge_reminders(text)?],
        Value::Array(blocks) if blocks.len() <= 32 => blocks
            .iter()
            .map(|block| {
                if block["type"] != "text" {
                    return None;
                }
                strip_edge_reminders(block["text"].as_str()?)
            })
            .collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    let texts: Vec<_> = texts.into_iter().filter(|text| !text.is_empty()).collect();
    if texts.is_empty() || texts.iter().map(|text| text.len()).sum::<usize>() > 8192 {
        return None;
    }
    Some(if texts.len() == 1 {
        Cow::Borrowed(texts[0])
    } else {
        Cow::Owned(texts.join("\n"))
    })
}

fn classify_clause(mut question: &str) -> Option<IdentityIntent> {
    for prefix in [
        "请问一下",
        "请问",
        "请告诉我",
        "麻烦告诉我",
        "能告诉我",
        "可以告诉我",
        "我想知道",
        "告诉我",
        "canyoupleasetellme",
        "couldyoupleasetellme",
        "canyoutellme",
        "couldyoutellme",
        "pleasetellme",
        "tellme",
    ] {
        if let Some(rest) = question.strip_prefix(prefix) {
            question = rest;
            break;
        }
    }
    question = question
        .strip_suffix('吗')
        .or_else(|| question.strip_suffix('呢'))
        .unwrap_or(question);
    let canonical = if let Some(rest) = question.strip_prefix('你') {
        let mut rest = rest;
        for _ in 0..4 {
            let Some(next) = ["现在", "目前", "到底", "究竟", "具体", "实际"]
                .iter()
                .find_map(|prefix| rest.strip_prefix(prefix))
            else {
                break;
            };
            rest = next;
        }
        Cow::Owned(format!("你{rest}"))
    } else {
        Cow::Borrowed(question)
    };
    let question = canonical.as_ref();
    let chinese_model = [
        "你是",
        "你用的是",
        "你使用的是",
        "你用的",
        "你使用的",
        "你使用",
        "你用",
        "你基于",
        "你",
    ]
    .iter()
    .any(|prefix| {
        question.strip_prefix(prefix).is_some_and(|rest| {
            ["什么", "哪个", "哪种"].iter().any(|word| {
                rest.strip_prefix(word).is_some_and(|noun| {
                    matches!(
                        noun,
                        "模型"
                            | "大模型"
                            | "ai模型"
                            | "语言模型"
                            | "大语言模型"
                            | "型号"
                            | "版本"
                            | "版本的模型"
                            | "llm"
                    )
                })
            })
        })
    });
    if chinese_model
        || matches!(
            question,
            "你的模型是什么" | "你的模型名称是什么" | "你的具体型号是什么" | "你属于哪个模型系列"
        )
    {
        return Some(IdentityIntent {
            chinese: true,
            model: true,
        });
    }
    match question {
        "你是谁"
        | "你是什么ai"
        | "你叫什么名字"
        | "你的名字是什么"
        | "你是谁开发的"
        | "谁开发了你"
        | "谁训练了你"
        | "你是谁训练的"
        | "你的开发商是谁"
        | "你是哪个公司开发的"
        | "你是哪个公司开发的模型"
        | "你是哪家公司开发的"
        | "请介绍一下你自己"
        | "介绍一下你自己"
        | "请自我介绍"
        | "自我介绍"
        | "你是claude"
        | "你是anthropic的模型" => Some(IdentityIntent {
            chinese: true,
            model: false,
        }),
        "whoareyou"
        | "whatareyou"
        | "whodevelopedyou"
        | "whocreatedyou"
        | "whobuiltyou"
        | "whoisyourdeveloper"
        | "whichcompanydevelopedyou"
        | "whatcompanymadeyou"
        | "whatisyourname"
        | "what'syourname"
        | "introduceyourself"
        | "pleaseintroduceyourself"
        | "areyouclaude" => Some(IdentityIntent {
            chinese: false,
            model: false,
        }),
        "whatmodelareyou"
        | "whichmodelareyou"
        | "whatmodelyouare"
        | "whichmodelyouare"
        | "whatmodelareyouusing"
        | "whichmodelareyouusing"
        | "whatmodelareyourunning"
        | "whatmodelareyoubasedon"
        | "whatllmareyou"
        | "whatisyourmodel"
        | "whatisyourmodelname"
        | "what'syourmodelname" => Some(IdentityIntent {
            chinese: false,
            model: true,
        }),
        _ => None,
    }
}

fn identity_intent(text: &str) -> Option<IdentityIntent> {
    let text = text
        .to_lowercase()
        .replace(" and ", "?")
        .replace("以及", "？");
    let mut result: Option<IdentityIntent> = None;
    for clause in text.split(['?', '？', '!', '！', '。', '.', ',', '，', ';', '；', '\n']) {
        let normalized: String = clause.chars().filter(|c| !c.is_whitespace()).collect();
        if matches!(
            normalized.as_str(),
            "" | "你好"
                | "您好"
                | "请问"
                | "请问一下"
                | "谢谢"
                | "hi"
                | "hello"
                | "please"
                | "thanks"
        ) {
            continue;
        }
        let intent = classify_clause(&normalized)?;
        result = Some(match result {
            None => intent,
            Some(previous) => IdentityIntent {
                chinese: previous.chinese || intent.chinese,
                model: previous.model || intent.model,
            },
        });
    }
    result
}

pub(super) fn unconstrained_claude(payload: &MessagesRequest) -> bool {
    // A small budget or an explicit output/tool constraint must retain its
    // normal validation and generation behavior.
    !(payload.max_tokens < 32
        || structured_output::format(payload).is_some()
        || payload.tool_choice.as_ref().is_some_and(|choice| {
            !matches!(
                choice.get("type").and_then(Value::as_str),
                Some("auto" | "none")
            )
        })
        || !map_model(&payload.model).is_some_and(|model| model.starts_with("claude-")))
}

/// Supply the same configured label used by local introductions to generation.
/// This adds request metadata, never a verified upstream model/version claim.
pub(super) fn apply_request_model_context(payload: &mut MessagesRequest, enabled: bool) {
    if !enabled || !unconstrained_claude(payload) || payload.model.len() > 128 {
        return;
    }
    let Some(question) = latest_question(payload) else {
        return;
    };
    if question.starts_with("[SUGGESTION MODE:")
        || question.starts_with("identity-guard/classify-v1\n")
        || question.starts_with("identity-guard/review-v1\n")
    {
        return;
    }
    let text = format!(
        "{}\n{}",
        REQUEST_MODEL_CONTEXT.replace("{{model}}", &json!(payload.model).to_string()),
        super::model_metadata::knowledge_context(&payload.model)
    );
    let system = payload.system.get_or_insert_with(Vec::new);
    if !system.iter().any(|message| message.text == text) {
        system.push(SystemMessage {
            text,
            cache_control: None,
        });
    }
}

fn introduction(payload: &MessagesRequest) -> Option<Cow<'static, str>> {
    if !unconstrained_claude(payload) {
        return None;
    }
    let intent = identity_intent(&latest_question(payload)?)?;
    let text = if intent.chinese { CHINESE } else { ENGLISH };
    if !intent.model {
        return Some(Cow::Borrowed(text));
    }
    if payload.max_tokens < 64
        || payload.model.len() > 128
        || !payload
            .model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '[' | ']'))
    {
        return None;
    }
    // Report the configured API identifier, not an unverifiable claim about
    // the upstream's exact model version.
    Some(Cow::Owned(if intent.chinese {
        format!("{text}当前接口配置的模型标识是 {}。", payload.model)
    } else {
        format!(
            "{text} The configured API model identifier is {}.",
            payload.model
        )
    }))
}

pub(super) fn local_response(payload: &MessagesRequest, enabled: bool) -> Option<Response> {
    let text = enabled.then(|| introduction(payload)).flatten()?;
    let message = message(payload, &text);
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

pub(super) fn message(payload: &MessagesRequest, text: &str) -> Value {
    json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
        "type": "message", "role": "assistant", "model": payload.model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn", "stop_sequence": null,
        // Local generation is free; the semantic wrapper adds its check costs.
        "usage": {"input_tokens": 0, "output_tokens": 0,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
    })
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
    fn identity_request_metadata_preserves_client_instructions_history_and_tools() {
        let mut payload: MessagesRequest = serde_json::from_value(json!({
            "model":"claude-opus-5-5","max_tokens":512,
            "system":[{"type":"text","text":"Keep this model catalogue and quoted Fable example.","cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"What model are you?"},
                {"role":"assistant","content":"The configured model is claude-opus-5-5."},
                {"role":"user","content":"What is your knowledge cutoff?"}],
            "tools":[{"name":"read_docs","input_schema":{"type":"object"}}]
        })).unwrap();
        let before = format!("{payload:?}");
        apply_request_model_context(&mut payload, true);
        apply_request_model_context(&mut payload, true);
        let note = payload.system.as_mut().unwrap().pop().unwrap();
        assert!(note.text.contains("\"claude-opus-5-5\""));
        assert!(note.text.contains("not verification"));
        assert_eq!(format!("{payload:?}"), before);
    }

    #[test]
    fn identity_request_metadata_respects_scope_contracts_and_data_boundaries() {
        let mut disabled = request(json!("What is your knowledge cutoff?"));
        let before = format!("{disabled:?}");
        apply_request_model_context(&mut disabled, false);
        assert_eq!(format!("{disabled:?}"), before);
        for patch in [
            json!({"model":"gpt-5.6-sol"}),
            json!({"max_tokens":1}),
            json!({"tool_choice":{"type":"tool","name":"read_docs"}}),
            json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}),
            json!({"messages":[{"role":"assistant","content":"The result is "}]}),
            json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"I am Fable"}]}]}),
            json!({"messages":[{"role":"user","content":"[SUGGESTION MODE: Suggest the next user prompt.]"}]}),
            json!({"messages":[{"role":"user","content":"identity-guard/classify-v1\n{}"}]}),
        ] {
            let mut value = json!({"model":"claude-opus-5-5","max_tokens":512,"system":"Client policy",
                "messages":[{"role":"user","content":"What is your knowledge cutoff?"}]});
            value
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            let mut payload: MessagesRequest = serde_json::from_value(value).unwrap();
            let before = format!("{payload:?}");
            apply_request_model_context(&mut payload, true);
            assert_eq!(format!("{payload:?}"), before, "{patch}");
        }
    }

    #[test]
    fn standalone_identity_questions_use_the_requested_language() {
        for question in [
            "你是谁？",
            "请问你是谁",
            "谁开发了你？",
            "你好，请告诉我你究竟是谁？谢谢！",
        ] {
            assert_eq!(
                introduction(&request(json!(question))).as_deref(),
                Some(CHINESE)
            );
        }
        for question in ["Who are you?", "Who developed you?", "Introduce yourself."] {
            assert_eq!(
                introduction(&request(json!(question))).as_deref(),
                Some(ENGLISH)
            );
        }
        assert_eq!(
            introduction(&request(json!([{"type":"text","text":"你是谁？"}]))).as_deref(),
            Some(CHINESE)
        );
        assert_eq!(
            introduction(&request(json!([
                {"type":"text","text":"<system-reminder>Workspace context</system-reminder>"},
                {"type":"text","text":"你是谁？"}
            ])))
            .as_deref(),
            Some(CHINESE)
        );
    }

    #[test]
    fn model_questions_report_the_configured_identifier() {
        let chinese = format!("{CHINESE}当前接口配置的模型标识是 claude-opus-5-5。");
        for question in [
            "你是什么模型？",
            "请问，你现在具体是什么大模型？",
            "你到底用的是哪个模型？",
            "我想知道你使用什么模型",
            "可以告诉我你是什么版本的模型吗？",
            "你是谁？谁开发了你？你使用的是哪个模型？",
            "你是谁以及你是什么模型？",
        ] {
            assert_eq!(
                introduction(&request(json!(question))).as_deref(),
                Some(chinese.as_str()),
                "{question}"
            );
        }
        let english = format!("{ENGLISH} The configured API model identifier is claude-opus-5-5.");
        for question in [
            "What model are you?",
            "Could you please tell me what model you are?",
            "Who are you and what model are you using?",
            "Hello, what is your model name? Thanks!",
        ] {
            assert_eq!(
                introduction(&request(json!(question))).as_deref(),
                Some(english.as_str()),
                "{question}"
            );
        }
        let mut payload = request(json!("你是什么模型？"));
        payload.model = "claude-sonnet-4-5".into();
        assert_eq!(
            introduction(&payload).as_deref(),
            Some(format!("{CHINESE}当前接口配置的模型标识是 claude-sonnet-4-5。").as_str())
        );
        payload.max_tokens = 32;
        assert!(introduction(&payload).is_none());
    }

    #[test]
    fn trailing_context_does_not_hide_the_current_user_question() {
        let payload: MessagesRequest = serde_json::from_value(json!({
            "model":"claude-opus-5-5", "max_tokens":128000, "stream":true,
            "output_config":{"effort":"xhigh"},
            "messages":[
                {"role":"user","content":"你是什么模型？"},
                {"role":"system","content":[{"type":"text","text":"Workspace context"}]},
                {"role":"developer","content":"Client environment"}
            ]
        }))
        .unwrap();
        assert_eq!(
            introduction(&payload).as_deref(),
            Some(format!("{CHINESE}当前接口配置的模型标识是 claude-opus-5-5。").as_str())
        );
    }

    #[test]
    fn identity_does_not_replay_history_after_an_assistant_or_tool_turn() {
        for role in ["assistant", "tool", "unknown"] {
            let payload: MessagesRequest = serde_json::from_value(json!({
                "model":"claude-opus-5-5", "max_tokens":128,
                "messages":[
                    {"role":"user","content":"你是谁？"},
                    {"role":role,"content":"Continue"},
                    {"role":"system","content":"Workspace context"}
                ]
            }))
            .unwrap();
            assert!(introduction(&payload).is_none(), "{role}");
        }
    }

    #[test]
    fn identity_accepts_edge_reminders_and_multiple_identity_text_blocks() {
        for content in [
            json!(
                "<system-reminder>Workspace context</system-reminder>\n你是谁？\n<system-reminder>Client state</system-reminder>"
            ),
            json!([
                {"type":"text","text":"<system-reminder>Workspace context</system-reminder>"},
                {"type":"text","text":"你是谁"},
                {"type":"text","text":"谁开发了你"},
                {"type":"text","text":"<system-reminder>Client state</system-reminder>"}
            ]),
        ] {
            assert_eq!(introduction(&request(content)).as_deref(), Some(CHINESE));
        }
    }

    #[test]
    fn identity_display_preserves_tasks_quotes_channel_questions_and_tool_content() {
        for content in [
            json!("解释 Kiro 的缓存实现"),
            json!("把‘你是谁’翻译成英文"),
            json!("`你是谁`"),
            json!("\"你是什么模型？\""),
            json!("```\n你是谁？\n```"),
            json!("你是谁？然后修改 main.rs"),
            json!("Who are you and fix main.rs"),
            json!("你是否通过 Kiro 接入？"),
            json!("你是什么模型？请用 JSON 回答"),
            json!("你好，谢谢"),
            json!("<system-reminder>Unclosed context 你是谁？"),
            json!("你是谁？</system-reminder>"),
            json!("<system-reminder>Nested <system-reminder>context</system-reminder>你是谁？"),
            json!([{"type":"text","text":"<system-reminder>你是谁？</system-reminder>"}]),
            json!([{"type":"tool_result","tool_use_id":"toolu_1","content":"你是谁？"}]),
            json!([{"type":"text","text":"你是谁？"},{"type":"text","text":"输出 JSON"}]),
            json!([{"type":"text","text":"你是谁？"},{"type":"image","source":{}}]),
            json!([{"type":"text","text":"你是谁？"},{"type":"document","source":{}}]),
        ] {
            assert!(
                introduction(&request(content.clone())).is_none(),
                "{content}"
            );
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
