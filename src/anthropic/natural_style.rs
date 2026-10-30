//! Optional prose guidance applied once to generation, without rewriting output.

use super::{
    identity,
    types::{MessagesRequest, SystemMessage},
};

const PROMPT: &str = include_str!("natural_style.txt");

pub(super) fn apply(payload: &mut MessagesRequest, enabled: bool) {
    if !enabled || !identity::unconstrained_claude(payload) {
        return;
    }
    let Some(last) = payload
        .messages
        .iter()
        .rev()
        .find(|message| !matches!(message.role.as_str(), "system" | "developer"))
    else {
        return;
    };
    if last.role != "user" {
        return;
    }
    // Suggestions and internal checks have their own narrowly defined output.
    if identity::latest_question(payload).is_some_and(|question| {
        question.starts_with("[SUGGESTION MODE:")
            || question.starts_with("identity-guard/classify-v1\n")
            || question.starts_with("identity-guard/review-v1\n")
    }) {
        return;
    }
    let system = payload.system.get_or_insert_with(Vec::new);
    if !system.iter().any(|message| message.text == PROMPT) {
        // Keep the client system blocks and their cache controls verbatim and
        // later in the same message. Explicit client preferences take priority.
        system.insert(
            0,
            SystemMessage {
                text: PROMPT.into(),
                cache_control: None,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> MessagesRequest {
        serde_json::from_value(json!({"model":"claude-opus-5-5","max_tokens":512,
            "system":[{"type":"text","text":"Answer with a numbered list.","cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"Explain HTTP."}]})).unwrap()
    }

    #[test]
    fn natural_style_preserves_client_instructions_and_is_idempotent() {
        let mut payload = request();
        let original_system = serde_json::to_value(&payload.system).unwrap();
        let original_messages = serde_json::to_value(&payload.messages).unwrap();
        apply(&mut payload, true);
        apply(&mut payload, true);
        let system = payload.system.as_ref().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0].text, PROMPT);
        assert_eq!(serde_json::to_value(&system[1..]).unwrap(), original_system);
        assert_eq!(
            serde_json::to_value(&payload.messages).unwrap(),
            original_messages
        );
    }

    #[test]
    fn natural_style_skips_disabled_non_claude_prefills_and_output_contracts() {
        let mut disabled = request();
        apply(&mut disabled, false);
        assert_eq!(disabled.system.unwrap().len(), 1);
        for patch in [
            json!({"model":"gpt-5.6-sol"}),
            json!({"tool_choice":{"type":"tool","name":"run"}}),
            json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}),
            json!({"messages":[{"role":"assistant","content":"Continue"}]}),
            json!({"messages":[{"role":"user","content":"[SUGGESTION MODE: Suggest what the user might type next.]"}]}),
            json!({"messages":[{"role":"user","content":"identity-guard/review-v1\n{}"}]}),
        ] {
            let mut value = json!({"model":"claude-opus-5-5","max_tokens":512,"system":"client policy",
                "messages":[{"role":"user","content":"Explain HTTP."}]});
            value
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            let mut payload: MessagesRequest = serde_json::from_value(value).unwrap();
            apply(&mut payload, true);
            assert_eq!(payload.system.unwrap()[0].text, "client policy", "{patch}");
        }
    }

    #[test]
    fn natural_style_keeps_tool_results_and_arguments_verbatim() {
        let mut payload = request();
        payload.messages[0].content =
            json!([{"type":"tool_result","tool_use_id":"call_1","content":"literal Kiro output"}]);
        payload.tools =
            serde_json::from_value(json!([{"name":"run","input_schema":{"type":"object"}}])).ok();
        payload.tool_choice = Some(json!({"type":"auto"}));
        let messages = serde_json::to_value(&payload.messages).unwrap();
        let tools = serde_json::to_value(&payload.tools).unwrap();
        apply(&mut payload, true);
        assert_eq!(payload.system.unwrap().len(), 2);
        assert_eq!(serde_json::to_value(&payload.messages).unwrap(), messages);
        assert_eq!(serde_json::to_value(&payload.tools).unwrap(), tools);
        assert_eq!(payload.tool_choice, Some(json!({"type":"auto"})));
    }
}
