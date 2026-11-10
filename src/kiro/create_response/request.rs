use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::kiro::{endpoint::RequestContext, kiro_version};

pub(super) fn app_identity(ctx: &RequestContext<'_>, version: &str) -> String {
    format!(
        "KiroIDE-{version}-{}-KAS/0.66.89 acp-client/kiro-ide",
        ctx.machine_id
    )
}

pub(super) fn token_type(ctx: &RequestContext<'_>) -> Option<&'static str> {
    if let Some(kind) = ctx.credentials.token_type_header() {
        return Some(kind);
    }
    let social =
        ctx.credentials
            .auth_method
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("social"))
            || ctx.credentials.provider.as_deref().is_some_and(|p| {
                p.eq_ignore_ascii_case("github") || p.eq_ignore_ascii_case("google")
            });
    (!social).then_some("SSO_OIDC")
}

pub(crate) fn decorate_request(
    req: reqwest::RequestBuilder,
    ctx: &RequestContext<'_>,
) -> reqwest::RequestBuilder {
    let version = kiro_version::effective(&ctx.config.kiro_version);
    let mut req = req
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", ctx.token))
        .header("x-amzn-kiro-origin", "AI_EDITOR")
        .header("x-amzn-codewhisperer-optout", "true")
        .header("user-agent", app_identity(ctx, &version))
        .header("x-amzn-kiro-attempt", "1;max=1");
    if let Some(profile) = ctx.credentials.streaming_profile_arn() {
        req = req.header("x-amzn-kiro-profile", profile);
    }
    if let Some(kind) = token_type(ctx) {
        req = req.header("tokentype", kind);
    }
    req
}

/// Convert the provider's canonical Kiro conversation, preserving ordered tool
/// calls/results and multimodal inputs. This runs before CLI origin rewriting.
pub(crate) fn transform_body(body: &str, profile: Option<&str>) -> anyhow::Result<String> {
    let source: Value = serde_json::from_str(body)?;
    let state = source
        .get("conversationState")
        .context("missing conversationState")?;
    let current = state
        .pointer("/currentMessage/userInputMessage")
        .context("missing current message")?;
    let model = current
        .get("modelId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("missing modelId")?;
    let mut input = Vec::new();
    if let Some(history) = state.get("history").and_then(Value::as_array) {
        for entry in history {
            append_input(entry, &mut input)?;
        }
    }
    append_input(&state["currentMessage"], &mut input)?;
    if input.is_empty() {
        bail!("CreateResponse requires at least one input item");
    }
    let mut out = json!({"model":model, "input":input, "stream":true, "origin":"AI_EDITOR",
        "metadata":{"kiro:agent_mode":state.get("agentTaskType").and_then(Value::as_str).unwrap_or("vibe")}});
    if let Some(id) = state.get("conversationId") {
        out["metadata"]["kiro:conversation_id"] = id.clone();
    }
    if let Some(profile) = profile {
        out["profileArn"] = json!(profile);
    }
    let mut tools = Vec::new();
    if let Some(specs) = current
        .pointer("/userInputMessageContext/tools")
        .and_then(Value::as_array)
    {
        for tool in specs {
            if let Some(spec) = tool.get("toolSpecification") {
                let name = spec
                    .get("name")
                    .and_then(Value::as_str)
                    .context("missing tool name")?;
                tools.push(json!({"type":"function", "name":name,
                    "description":spec.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters":spec.pointer("/inputSchema/json").cloned().unwrap_or_else(||json!({"type":"object","properties":{}}))}));
            }
        }
    }
    if !tools.is_empty() {
        out["tools"] = json!(tools);
    }
    if let Some(fields) = source.get("additionalModelRequestFields") {
        let mut reasoning = serde_json::Map::new();
        for (dest, paths) in [
            ("type", vec!["/thinking/type"]),
            ("effort", vec!["/output_config/effort", "/reasoning/effort"]),
            ("summary", vec!["/thinking/display"]),
        ] {
            if let Some(value) = paths
                .into_iter()
                .find_map(|path| fields.pointer(path).and_then(Value::as_str))
                .filter(|v| !v.is_empty())
            {
                reasoning.insert(dest.into(), json!(value));
            }
        }
        if !reasoning.is_empty() {
            out["reasoning"] = Value::Object(reasoning);
        }
    }
    Ok(serde_json::to_string(&out)?)
}

fn append_input(entry: &Value, input: &mut Vec<Value>) -> anyhow::Result<()> {
    if let Some(user) = entry.get("userInputMessage") {
        if let Some(results) = user
            .pointer("/userInputMessageContext/toolResults")
            .and_then(Value::as_array)
        {
            for result in results {
                let call_id = result
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .context("missing tool result ID")?;
                let parts = result
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                // The converter has already lifted tool-result images to user.images.
                let text = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                input
                    .push(json!({"type":"function_call_output", "call_id":call_id, "output":text}));
            }
        }
        let text = user.get("content").and_then(Value::as_str).unwrap_or("");
        let mut images = Vec::new();
        if let Some(items) = user.get("images").and_then(Value::as_array) {
            for image in items {
                let bytes = image
                    .pointer("/source/bytes")
                    .and_then(Value::as_str)
                    .context("missing image bytes")?;
                let format = image
                    .get("format")
                    .and_then(Value::as_str)
                    .unwrap_or("png")
                    .to_ascii_lowercase();
                images.push(json!({"type":"input_image", "image_url":format!("data:image/{format};base64,{bytes}")}));
            }
        }
        if !images.is_empty() {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(json!({"type":"input_text", "text":text}));
            }
            parts.extend(images);
            input.push(json!({"role":"user", "content":parts}));
        } else if !text.is_empty() {
            input.push(json!({"role":"user", "content":text}));
        }
    }
    if let Some(assistant) = entry.get("assistantResponseMessage") {
        if let Some(text) = assistant
            .get("content")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            input.push(json!({"role":"assistant", "content":text}));
        }
        if let Some(calls) = assistant.get("toolUses").and_then(Value::as_array) {
            for call in calls {
                let id = call
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .context("missing tool call ID")?;
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .context("missing tool call name")?;
                let args = call.get("input").cloned().unwrap_or_else(|| json!({}));
                let arguments = args
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| args.to_string());
                input.push(json!({"type":"function_call", "call_id":id, "name":name, "arguments":arguments}));
            }
        }
    }
    Ok(())
}
