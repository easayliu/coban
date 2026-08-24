//! Chat Completions ↔ Responses 线格式翻译。
//!
//! 上游（`backend-api/codex`）只讲 Responses 一种线格式，而接入方里有一整类客户端只会发
//! `/v1/chat/completions`——OpenAI SDK、各种前端与 Agent 框架的默认路径。这一层把请求翻过
//! 去、再把 SSE 翻回来，让那类客户端不改代码就能接。
//!
//! **翻译只发生在这一个模块里**：转发路径（[`crate::proxy`]）的其余部分——选号、换号重试、
//! 限流头解析、用量嗅探、落库、计价——两种线格式一字不差地共用。嗅探器吃的是**上游原始
//! 字节**（翻译排在它后面），所以额度与花费这套账与线格式无关，不会因为多了一种接入形态
//! 而出现两套读数。
//!
//! 几处「只能这样」的取舍，各自在函数上注明：`system`/`developer` 消息合并进
//! `instructions`（Responses 那头没有对应的消息角色）、**上游一律走流式**（非流式会被上游
//! 拒，客户端要整块 JSON 时由这一层聚合）、`n > 1` 直接拒（上游一次只出一条）。
//!
//! **上游不认的参数一律丢掉，不往上送。** 已实测拒绝的有 `max_output_tokens`（回
//! `Unsupported parameter`，所以 `max_tokens`/`max_completion_tokens` 只能丢），
//! `temperature`/`top_p`/`seed`/`stop`/`presence_penalty`/`frequency_penalty`/`logit_bias`
//! /`logprobs`/`user`/`metadata` 这些订阅模式这条路径本来就不收。丢掉的代价是客户端设的
//! 上限不生效，而带上去的代价是**每一条请求都 400**——后者更糟，且客户端那头看到的只是
//! 一句「请求失败」。已实测**能**过的：`tools`/`tool_choice`/`parallel_tool_calls`/
//! `reasoning.effort`/`text.format`（含 `json_schema` 严格模式）/`input_image`。

use std::collections::{HashMap, HashSet};

use axum::body::Bytes;
use serde_json::{Value, json};

use crate::proxy::Usage;

/// 来访路径里的 Chat Completions 端点（去掉 `/v1` 或 `/backend-api/codex` 前缀之后那一段）。
pub const PATH: &str = "chat/completions";

/// 客户端一条 `system`/`developer` 消息都没给时补的 `instructions`。
///
/// Responses 那头**要求这个字段存在**，缺了直接 400（同 [`crate::proxy`] 探测体的注）。
/// 一句最普通的通用提示：客户端没表达任何系统意图时，这里替它编一段人格反而是加戏。
const DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";

/// 单行 SSE `data:` 的长度上限，超过就丢弃这一行。与嗅探器同一个理由：一个不带换行的巨大
/// 响应体会把行缓冲撑成无界内存。
const MAX_SSE_LINE: usize = 1024 * 1024;

// ---------- 请求翻译 ----------

/// 翻译好的上游请求，外加回给客户端时要用到的形态。
#[derive(Debug)]
pub struct Translated {
    /// 发往上游 `responses` 的请求体。
    pub body: Bytes,
    /// 客户端要的是 SSE 还是一整块 JSON。**与上游无关**——上游那条永远是流式。
    pub stream: bool,
    /// `stream_options.include_usage`：流式收尾要不要补一条只带 usage 的 chunk。
    pub include_usage: bool,
    /// 客户端请求的模型名。上游没报模型时用它回显。
    pub model: String,
    /// 这条请求的会话指纹，连同四段各自的短哈希（见 [`crate::proxy::prefix_parts`]）。
    /// **在这里算**是因为翻完的那份 Map 就在手上，为它把刚序列化出来的体再解析一遍是白花
    /// 的 CPU。
    pub prefix: Option<crate::proxy::PrefixParts>,
    /// 翻出来的 `input[]` 有几项。给缓存归因用：第一轮只有一项，多轮会一直长
    /// （见 [`crate::proxy`] 的 `cache_reason`）。
    pub input_len: usize,
}

/// 把一份 Chat Completions 请求体翻成 Responses 请求体。
///
/// 返回的 `Err` 是给客户端看的 `invalid_request_error` 文案：这类错误在 coban 这一层就能
/// 判定，送到上游只会换回一句指不到原因的 400。
pub fn translate_request(raw: &[u8], sort_tools: bool) -> Result<Translated, String> {
    let v: Value = serde_json::from_slice(raw)
        .map_err(|e| format!("the request body is not valid JSON: {e}"))?;
    let obj = v.as_object().ok_or_else(|| "the request body must be a JSON object".to_owned())?;

    let model = obj
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| "`model` is required".to_owned())?
        .to_owned();

    // `n > 1` 拒掉而不是静默给一条：按 `choices[1]` 取值的客户端会拿到越界，而那种失败
    // 出现在客户端内部，比这里回一句明确的 400 难查得多。
    if let Some(n) = obj.get("n").and_then(|n| n.as_i64()).filter(|n| *n > 1) {
        return Err(format!("`n` must be 1: the upstream returns a single choice (got {n})"));
    }

    let messages = obj
        .get("messages")
        .and_then(|m| m.as_array())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| "`messages` must be a non-empty array".to_owned())?;

    let (instructions, input) = translate_messages(messages)?;

    let stream = obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let include_usage =
        v.pointer("/stream_options/include_usage").and_then(|s| s.as_bool()).unwrap_or(false);

    // 字段顺序照官方客户端那份（见 Cargo.toml 里 preserve_order 的注）：一个 key 顺序与
    // 真实客户端不同的 body 本身就是「中间有代理」的指纹。
    let mut out = serde_json::Map::new();
    out.insert("model".into(), Value::String(model.clone()));
    out.insert("instructions".into(), Value::String(instructions));
    out.insert("input".into(), Value::Array(input));
    if let Some(tools) = obj.get("tools") {
        out.insert("tools".into(), Value::Array(translate_tools(tools)?));
    }
    if let Some(tc) = obj.get("tool_choice") {
        out.insert("tool_choice".into(), translate_tool_choice(tc)?);
    }
    if let Some(p) = obj.get("parallel_tool_calls").filter(|p| p.is_boolean()) {
        out.insert("parallel_tool_calls".into(), p.clone());
    }
    // 只在客户端明确要求时才带 `reasoning`：凭空加一个档位会让不支持推理的模型直接 400，
    // 而客户端并没有要求过这件事。
    if let Some(effort) = obj.get("reasoning_effort").and_then(|e| e.as_str()) {
        out.insert("reasoning".into(), json!({ "effort": effort }));
    }
    if let Some(text) = translate_response_format(obj.get("response_format"))? {
        // json_object 那档上游还有一道口子要过（见 [`mention_json`]）。
        if text.pointer("/format/type").and_then(|t| t.as_str()) == Some("json_object")
            && let Some(input) = out.get_mut("input").and_then(|v| v.as_array_mut())
        {
            mention_json(input);
        }
        out.insert("text".into(), text);
    }
    // 这两个是上游的硬约束，不是客户端的选择：`store` 只收 `false`（这条路径不存会话），
    // `stream` 只收 `true`（非流式被拒）。客户端要非流式时由 [`aggregate`] 聚合。
    out.insert("store".into(), Value::Bool(false));
    out.insert("stream".into(), Value::Bool(true));

    // **排在算指纹之前**：指纹里就含 tools 及其顺序，反过来的话前缀稳住了而落点还在跟着
    // 客户端那个乱序变——两件事必须用同一份顺序。
    if sort_tools {
        crate::proxy::normalize_tool_order(&mut out);
    }
    let prefix = crate::proxy::prefix_parts(&out);
    let input_len = out.get("input").and_then(|v| v.as_array()).map_or(0, |a| a.len());
    let body = serde_json::to_vec(&Value::Object(out))
        .map_err(|e| format!("failed to build the upstream request: {e}"))?;
    Ok(Translated { body: Bytes::from(body), stream, include_usage, model, prefix, input_len })
}

/// `messages[]` → (`instructions`, `input[]`)。
///
/// **`system`/`developer` 全部合并进 `instructions`**：Responses 的 `input` 里没有对应的
/// 消息角色，而 `instructions` 就是它安放这类内容的地方。代价是若客户端把系统消息夹在对话
/// 中间，合并后就丢了那个位置——换来的是不必赌上游认不认某个未经核实的角色名。
fn translate_messages(messages: &[Value]) -> Result<(String, Vec<Value>), String> {
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();
    // 这段历史里哪几次调用是 `custom` 的：它们的结果项换一种类型（见下面那两处）。
    let mut custom_calls: HashSet<String> = HashSet::new();

    for (i, m) in messages.iter().enumerate() {
        let role = m
            .get("role")
            .and_then(|r| r.as_str())
            .ok_or_else(|| format!("messages[{i}] has no `role`"))?;
        match role {
            "system" | "developer" => {
                let text = flatten_text(m.get("content"))
                    .ok_or_else(|| format!("messages[{i}] ({role}) has no text content"))?;
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(&text);
            }
            "user" => {
                let content = user_content(m.get("content"), i)?;
                input.push(json!({ "role": "user", "content": content }));
            }
            "assistant" => {
                // 助手消息可以只有 tool_calls 而没有正文，那种情况下不发空的 output_text：
                // 一个 text 为空的内容块是上游会拒的形状。
                if let Some(text) = flatten_text(m.get("content")).filter(|t| !t.is_empty()) {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for (j, tc) in m
                    .get("tool_calls")
                    .and_then(|t| t.as_array())
                    .unwrap_or(&Vec::new())
                    .iter()
                    .enumerate()
                {
                    let call_id = tc
                        .get("id")
                        .and_then(|x| x.as_str())
                        .ok_or_else(|| format!("messages[{i}].tool_calls[{j}] has no `id`"))?;
                    // `custom` 那种工具调用带的是一段自由文本（`custom.input`），不是 JSON
                    // 参数——项类型与字段名都换一套，而 `call_id` 记下来：它的结果那条消息
                    // 也得跟着换项类型（Chat 那头两种结果长得一模一样）。
                    if tc.get("type").and_then(|x| x.as_str()) == Some("custom")
                        || tc.get("custom").is_some()
                    {
                        let name = tc
                            .pointer("/custom/name")
                            .or_else(|| tc.get("name"))
                            .and_then(|x| x.as_str())
                            .ok_or_else(|| {
                                format!("messages[{i}].tool_calls[{j}] has no `custom.name`")
                            })?;
                        let text = tc
                            .pointer("/custom/input")
                            .or_else(|| tc.get("input"))
                            .and_then(|x| x.as_str())
                            .unwrap_or_default();
                        custom_calls.insert(call_id.to_owned());
                        input.push(json!({
                            "type": "custom_tool_call",
                            "call_id": call_id,
                            "name": name,
                            "input": text,
                        }));
                        continue;
                    }
                    let name =
                        tc.pointer("/function/name").and_then(|x| x.as_str()).ok_or_else(|| {
                            format!("messages[{i}].tool_calls[{j}] has no `function.name`")
                        })?;
                    // 参数是一段 JSON **字符串**（Chat 与 Responses 两头都是），空缺按 `{}`
                    // 补：上游要求这个字段存在。
                    let args = tc
                        .pointer("/function/arguments")
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("{}");
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": args,
                    }));
                }
            }
            "tool" | "function" => {
                let call_id = m
                    .get("tool_call_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("messages[{i}] (tool) has no `tool_call_id`"))?;
                // 工具结果为空是正常的（一条什么都没输出的命令），按空串送过去。
                let output = flatten_text(m.get("content")).unwrap_or_default();
                // 这次调用是 `custom` 的话，结果项也得是 `custom_tool_call_output`：Chat 那头
                // 两种结果都是一条 `role: "tool"`，只有前面那条助手消息认得出是哪一种。
                let ty = if custom_calls.contains(call_id) {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                input.push(json!({
                    "type": ty,
                    "call_id": call_id,
                    "output": output,
                }));
            }
            other => return Err(format!("messages[{i}]: unsupported role `{other}`")),
        }
    }

    // 一条系统消息都没有时补一句：`instructions` 缺了上游直接 400。
    if instructions.is_empty() {
        instructions.push_str(DEFAULT_INSTRUCTIONS);
    }
    Ok((instructions, input))
}

/// 用户消息的 `content` → Responses 的内容块数组。
///
/// 收两种形状：一段字符串，或 OpenAI 的多模态数组（`text` / `image_url`）。图片按
/// `input_image` 送过去，`image_url` 那头收 data URI 与 http(s) URL 两种。
fn user_content(content: Option<&Value>, i: usize) -> Result<Vec<Value>, String> {
    match content {
        Some(Value::String(s)) => Ok(vec![json!({ "type": "input_text", "text": s })]),
        Some(Value::Array(parts)) => {
            let mut out = Vec::with_capacity(parts.len());
            for (j, p) in parts.iter().enumerate() {
                match p.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("input_text") => {
                        let text = p.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                        out.push(json!({ "type": "input_text", "text": text }));
                    }
                    Some("image_url") | Some("input_image") => {
                        // `image_url` 有两种写法：`{"url": "..."}` 与直接一个字符串。
                        let url = p
                            .pointer("/image_url/url")
                            .and_then(|u| u.as_str())
                            .or_else(|| p.get("image_url").and_then(|u| u.as_str()))
                            .ok_or_else(|| {
                                format!("messages[{i}].content[{j}] has no image URL")
                            })?;
                        out.push(json!({ "type": "input_image", "image_url": url }));
                    }
                    other => {
                        return Err(format!(
                            "messages[{i}].content[{j}]: unsupported content type `{}`",
                            other.unwrap_or("(missing)")
                        ));
                    }
                }
            }
            Ok(out)
        }
        _ => Err(format!("messages[{i}] (user) has no `content`")),
    }
}

/// 取一条消息的纯文本：字符串原样，数组则把各 text 块按序拼起来。
///
/// 拿不到任何文本时返回 `None`（区别于「有内容但是空串」——后者是合法的工具输出）。
///
/// Responses 那头的内容块（`input_text`/`output_text`）也认：块里那个键同样叫 `text`。
/// [`crate::proxy::merge_system_messages`] 借的就是这一点——两条线格式把系统消息并进
/// `instructions` 时，该认哪些形状必须是同一份判断。
pub(crate) fn flatten_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                } else if let Some(t) = p.as_str() {
                    out.push_str(t);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// `tools[]`：Chat 的 `{type, function:{…}}` 摊平成 Responses 的 `{type, name, …}`。
///
/// `custom` 工具（freeform tool calling，工具吃的是一段自由文本而不是 JSON 参数）也认：两头
/// 的差别同样只是「嵌一层还是摊平」。**以前这里直接回一句 400 把整条请求拦在门口**，而上游
/// 本来收这种工具——那句话是这条链路自己发明的限制，客户端照它去改也改不出个所以然。
fn translate_tools(tools: &Value) -> Result<Vec<Value>, String> {
    let arr = tools.as_array().ok_or_else(|| "`tools` must be an array".to_owned())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, t) in arr.iter().enumerate() {
        let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("function");
        if ty == "custom" {
            out.push(translate_custom_tool(i, t)?);
            continue;
        }
        if ty != "function" {
            return Err(format!("tools[{i}]: only `function` tools are supported (got `{ty}`)"));
        }
        // 摊平前先认字段：`function` 缺了说明客户端发的不是 Chat 形状的工具。
        let f = t.get("function").ok_or_else(|| format!("tools[{i}] has no `function` object"))?;
        let name = f
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| format!("tools[{i}] has no `function.name`"))?;
        let mut o = serde_json::Map::new();
        o.insert("type".into(), Value::String("function".into()));
        o.insert("name".into(), Value::String(name.into()));
        o.insert("description".into(), f.get("description").cloned().unwrap_or(Value::Null));
        // 无参工具也要给一个 schema：`parameters` 缺了上游 400。
        o.insert(
            "parameters".into(),
            f.get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        );
        // **`strict` 显式写 false**：Responses 那头这个字段缺省可能按严格模式处理，而严格
        // 模式要求 schema 满足一串附加约束（`additionalProperties: false` 等），客户端给的
        // 普通 schema 多半不满足，表现是每个带工具的请求都 400。客户端明确要严格才严格。
        o.insert(
            "strict".into(),
            f.get("strict").filter(|s| s.is_boolean()).cloned().unwrap_or(Value::Bool(false)),
        );
        out.push(Value::Object(o));
    }
    Ok(out)
}

/// `{type:"custom", custom:{name, description, format}}` → Responses 那头的扁平形状。
///
/// 字段直接写在外层的写法也认（有 SDK 这么发）：取不到 `custom` 就在外层找。
/// `description`/`format` 缺了就**不送**——`format` 缺省就是自由文本，替客户端编一个语法约束
/// 是在替它改工具定义。
fn translate_custom_tool(i: usize, t: &Value) -> Result<Value, String> {
    let c = t.get("custom").filter(|c| c.is_object()).unwrap_or(t);
    let name = c
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("tools[{i}] (custom) has no `name`"))?;
    let mut o = serde_json::Map::new();
    o.insert("type".into(), Value::String("custom".into()));
    o.insert("name".into(), Value::String(name.into()));
    for k in ["description", "format"] {
        if let Some(v) = c.get(k).filter(|v| !v.is_null()) {
            o.insert(k.into(), v.clone());
        }
    }
    Ok(Value::Object(o))
}

/// `tool_choice`：字符串档位原样，指定某个工具的那种要摊平。
///
/// `custom` 工具（见 [`translate_custom_tool`]）指名时那个 `type` 得跟着换：上游按它去
/// `tools` 里找，写成 `function` 就找不到那个工具了。
fn translate_tool_choice(tc: &Value) -> Result<Value, String> {
    match tc {
        Value::String(s) if matches!(s.as_str(), "auto" | "none" | "required") => Ok(tc.clone()),
        Value::Object(_) => {
            let custom = tc.get("type").and_then(|x| x.as_str()) == Some("custom")
                || tc.get("custom").is_some();
            let name = tc
                .pointer(if custom { "/custom/name" } else { "/function/name" })
                .and_then(|x| x.as_str())
                .or_else(|| tc.get("name").and_then(|x| x.as_str()))
                .ok_or_else(|| "`tool_choice` has no `function.name`".to_owned())?;
            Ok(json!({ "type": if custom { "custom" } else { "function" }, "name": name }))
        }
        other => Err(format!("unsupported `tool_choice`: {other}")),
    }
}

/// `response_format` → Responses 的 `text.format`。
fn translate_response_format(rf: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(rf) = rf.filter(|v| !v.is_null()) else { return Ok(None) };
    match rf.get("type").and_then(|t| t.as_str()) {
        // `text` 就是默认行为，不必往上游多送一个字段。
        None | Some("text") => Ok(None),
        Some("json_object") => Ok(Some(json!({ "format": { "type": "json_object" } }))),
        Some("json_schema") => {
            let js = rf
                .get("json_schema")
                .ok_or_else(|| "`response_format.json_schema` is required".to_owned())?;
            let schema = js
                .get("schema")
                .ok_or_else(|| "`response_format.json_schema.schema` is required".to_owned())?;
            let mut format = serde_json::Map::new();
            format.insert("type".into(), Value::String("json_schema".into()));
            format.insert(
                "name".into(),
                js.get("name").cloned().unwrap_or_else(|| Value::String("response".into())),
            );
            format.insert("schema".into(), schema.clone());
            format.insert(
                "strict".into(),
                js.get("strict").filter(|s| s.is_boolean()).cloned().unwrap_or(Value::Bool(false)),
            );
            Ok(Some(Value::Object(
                vec![("format".to_owned(), Value::Object(format))].into_iter().collect(),
            )))
        }
        Some(other) => Err(format!("unsupported `response_format.type`: `{other}`")),
    }
}

/// json_object 那档要补的一句话：让 `input` 里出现 "json" 这个词。
///
/// 短、且说的正是客户端已经用 `response_format` 表达过的那件事——补一句长的等于替客户端改
/// 提示词。
const JSON_HINT: &str = "Respond in JSON.";

/// `text.format` 是 `json_object` 时，保证 `input` 里的消息提到过 "json"。
///
/// 上游的原话：`Response input messages must contain the word 'json' in some form to use
/// 'text.format' of type 'json_object'.` 那道口子**只看 `input` 里的消息，不看
/// `instructions`**——而 [`translate_messages`] 正是把 `system`/`developer` 全并进
/// `instructions` 的：客户端那句「请输出 JSON」十有八九写在系统提示里，一并过去之后
/// `input` 里就再没有这个词了。同一份请求直接发给 OpenAI 的 `/v1/chat/completions` 是能过的
/// （那头系统消息也算一条 message），**这条 400 是我们这一跳造出来的**，必须在这一跳补回去。
///
/// 补的办法是往**最后一条用户消息**后面加一个内容块，而不是新起一条消息：新起一条会凭空多
/// 一个对话轮次，客户端那头对不上自己发的 `messages`。一条用户消息都没有（只有工具结果那种
/// 请求）时才补一条。
///
/// 已经提到过就一个字都不动——`json`、`JSON`、`Json` 都算（上游那句话说的是 "in some form"，
/// 它自己也是不分大小写地找）。
///
/// 客户端的提示词里压根没提过 JSON 时也补：那种请求直接发上游一样会被这道口子拦（跟我们这
/// 一跳无关），而它要的东西已经写在 `response_format` 里了，补一句正是它的本意。
fn mention_json(input: &mut Vec<Value>) {
    if input.iter().any(message_mentions_json) {
        return;
    }
    let hint = json!({ "type": "input_text", "text": JSON_HINT });
    if let Some(content) = input
        .iter_mut()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())
    {
        content.push(hint);
        return;
    }
    input.push(json!({ "role": "user", "content": [hint] }));
}

/// 这条消息的正文里提到过 "json" 没有。
///
/// 只看**消息**的正文：上游那道口子的措辞是 "input messages"，工具调用的参数、工具结果里出
/// 现这个词算不算它没说。宁可多补一句（无害），不能少补（那是一条 400）。
fn message_mentions_json(item: &Value) -> bool {
    if item.get("role").is_none() {
        return false;
    }
    let has = |v: Option<&Value>| {
        v.and_then(|t| t.as_str()).is_some_and(|t| t.to_ascii_lowercase().contains("json"))
    };
    match item.get("content") {
        Some(Value::String(_)) => has(item.get("content")),
        Some(Value::Array(parts)) => parts.iter().any(|p| has(p.get("text"))),
        _ => false,
    }
}

// ---------- 响应事件 ----------

/// 从上游 SSE 里解出来的语义事件。
///
/// 流式与非流式两条渲染路径**共用这一个解析器**：各写一个的话，同一条流会在两条路上给出
/// 不一致的结果（最典型的是 finish_reason 与 usage 只在一条路上对）。
#[derive(Debug, PartialEq)]
enum Ev {
    /// 上游给这次响应分配的 id 与它实际选用的模型。
    Created {
        id: Option<String>,
        model: Option<String>,
    },
    Text(String),
    /// 推理摘要增量。Chat 那头没有这个概念，按事实上的通行字段 `reasoning_content` 转出。
    Reasoning(String),
    ToolStart {
        item_id: String,
        call_id: String,
        name: String,
        /// 这是个 `custom` 工具调用（自由文本），不是 JSON 参数的函数调用。
        custom: bool,
    },
    ToolArgs {
        item_id: String,
        delta: String,
    },
    /// 终局。`incomplete` 是上游说的「为什么没写完」，用来定 finish_reason。
    Completed {
        usage: Option<Usage>,
        incomplete: Option<String>,
        model: Option<String>,
    },
    Failed {
        etype: Option<String>,
        message: String,
    },
}

/// 解一行 `data:` 里的事件体。认不出的事件（`response.in_progress`、各种 `*.done`）返回
/// `None`——Chat 那头没有对应的东西，转出去只是噪声。
fn parse_event(data: &str) -> Option<Ev> {
    // 先按事件名做一次廉价筛选，再付 JSON 解析的代价：一次长回复的 SSE 有几千行，
    // 逐行解析的开销全落在转发的关键路径上。
    if !data.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    let ty = v.get("type").and_then(|t| t.as_str())?;
    let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(str::to_owned);
    match ty {
        "response.created" => {
            Some(Ev::Created { id: s("/response/id"), model: s("/response/model") })
        }
        "response.output_text.delta" => Some(Ev::Text(s("/delta")?)),
        // 上游同时存在这两个名字（摘要与正文推理），Chat 那头都只能落到一个字段上。
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            Some(Ev::Reasoning(s("/delta")?))
        }
        "response.output_item.added" => {
            let item = v.get("item")?;
            let custom = match item.get("type").and_then(|t| t.as_str())? {
                "function_call" => false,
                "custom_tool_call" => true,
                _ => return None,
            };
            let call_id = item.get("call_id").and_then(|x| x.as_str())?.to_owned();
            let name = item.get("name").and_then(|x| x.as_str()).unwrap_or_default().to_owned();
            // 后续的参数增量按 `item_id` 归位；上游没给 id 时退回 call_id 当键。
            let item_id = item.get("id").and_then(|x| x.as_str()).unwrap_or(&call_id).to_owned();
            Some(Ev::ToolStart { item_id, call_id, name, custom })
        }
        // 两种工具调用的增量事件名不同，落到的地方是同一个：调用本身是 `custom` 还是函数，
        // 在 `output_item.added` 那一步就已经记下了。
        "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
            Some(Ev::ToolArgs { item_id: s("/item_id").unwrap_or_default(), delta: s("/delta")? })
        }
        "response.completed" | "response.incomplete" => Some(Ev::Completed {
            usage: v.pointer("/response/usage").filter(|u| u.is_object()).map(Usage::from_json),
            incomplete: s("/response/incomplete_details/reason"),
            model: s("/response/model"),
        }),
        // 上游先回 200 再在流里说这次失败了，两种形状都要认（同 proxy::sse_failure）。
        "response.failed" | "error" => {
            let err = v.pointer("/response/error").or_else(|| v.get("error"))?;
            if err.is_null() {
                return None;
            }
            let e = |p: &str| err.pointer(p).and_then(|x| x.as_str()).map(str::to_owned);
            Some(Ev::Failed {
                etype: e("/type").or_else(|| e("/code")),
                message: e("/message").unwrap_or_else(|| err.to_string()),
            })
        }
        _ => None,
    }
}

/// 定 Chat 的 `finish_reason`。
///
/// 三种来源按优先级：上游说的「没写完」最硬，其次「这轮出的是工具调用」，都不是才是正常收尾。
fn finish_reason(incomplete: Option<&str>, saw_tool: bool) -> &'static str {
    match incomplete {
        Some("max_output_tokens") => "length",
        Some("content_filter") => "content_filter",
        _ if saw_tool => "tool_calls",
        _ => "stop",
    }
}

/// Chat 形态的 usage 对象。
fn usage_json(u: &Usage) -> Value {
    json!({
        "prompt_tokens": u.input_tokens,
        "completion_tokens": u.output_tokens,
        "total_tokens": u.total_tokens,
        "prompt_tokens_details": { "cached_tokens": u.cached_tokens },
        "completion_tokens_details": { "reasoning_tokens": u.reasoning_tokens },
    })
}

/// 生成一个 `chatcmpl-` id。上游还没报 id 时用它兜底。
fn fallback_id() -> String {
    use rand::Rng;
    let mut buf = [0u8; 12];
    rand::rng().fill_bytes(&mut buf);
    format!("chatcmpl-{}", crate::credentials::hex_lower(&buf))
}

/// 上游的 `resp_…` 转成 Chat 的 `chatcmpl-…`：保留原 id 好让两边日志能对上。
fn chat_id(upstream: &str) -> String {
    format!("chatcmpl-{}", upstream.strip_prefix("resp_").unwrap_or(upstream))
}

// ---------- 流式翻译 ----------

/// 把上游的 Responses SSE 边收边翻成 Chat 的 `chat.completion.chunk` SSE。
///
/// 有状态：id/模型、工具调用的下标分配、以及「收尾发过没有」。**不缓存正文**——一次长回复
/// 的增量攒下来只为最后拼一个字符串，等于给每条并发请求挂一份响应体在内存里。
pub struct StreamXlate {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    /// 上一块结尾那半行（chunk 边界不保证落在换行上）。
    pending: String,
    /// `{"role":"assistant"}` 那条首块发过没有。
    role_sent: bool,
    /// `item_id` → Chat 的 `tool_calls[].index`。上游的 `output_index` 把推理项也数在内，
    /// 与 Chat 只数工具调用的下标不是一回事，故自己分配。
    /// 值里那个 `bool` 是「这是个 `custom` 调用」：增量要往 `custom.input` 还是
    /// `function.arguments` 上落，得靠它。
    tools: HashMap<String, (usize, bool)>,
    /// 最近一个开始的工具调用：参数增量的 `item_id` 对不上时按它归位（上游是逐个生成工具
    /// 调用的），总比把参数丢掉好。
    last_tool: Option<(usize, bool)>,
    saw_tool: bool,
    /// 收尾（finish chunk + `[DONE]`）发过没有。
    closed: bool,
}

impl StreamXlate {
    pub fn new(model: String, include_usage: bool) -> Self {
        Self {
            id: fallback_id(),
            model,
            created: crate::credentials::now_secs() as i64,
            include_usage,
            pending: String::new(),
            role_sent: false,
            tools: HashMap::new(),
            last_tool: None,
            saw_tool: false,
            closed: false,
        }
    }

    /// 喂一块上游字节，返回要发给客户端的 SSE 片段（可能是空的）。
    pub fn feed(&mut self, chunk: &Bytes) -> Bytes {
        self.pending.push_str(&String::from_utf8_lossy(chunk));
        let mut out = String::new();
        while let Some(idx) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=idx).collect();
            self.consume_line(line.trim_end(), &mut out);
        }
        if self.pending.len() > MAX_SSE_LINE {
            self.pending.clear();
        }
        Bytes::from(out)
    }

    /// 上游流走完时的收尾。
    ///
    /// 正常情况下 `response.completed` 已经把收尾发过了，这里什么都不做。走到「没发过」那
    /// 一支说明上游流**在终局事件之前就断了**：必须如实报错，不能补一条 `finish_reason:
    /// "stop"` 假装正常结束——客户端会把一段截断的回复当成完整答案存下来。
    pub fn flush(&mut self) -> Bytes {
        if self.closed {
            return Bytes::new();
        }
        let mut out = String::new();
        self.fail(None, "the upstream stream ended before the response completed", &mut out);
        Bytes::from(out)
    }

    fn consume_line(&mut self, line: &str, out: &mut String) {
        // 收尾发过之后就什么都不再转：`[DONE]` 之后还冒出 chunk 的话，客户端要么忽略、
        // 要么当成协议错误，没有一种是好结果。
        if self.closed {
            return;
        }
        let Some(data) = line.strip_prefix("data:") else { return };
        let Some(ev) = parse_event(data.trim()) else { return };
        match ev {
            Ev::Created { id, model } => {
                if let Some(id) = id {
                    self.id = chat_id(&id);
                }
                if let Some(m) = model {
                    self.model = m;
                }
                // 首块只带 role，与 OpenAI 一致：客户端据它知道「开始出内容了」。
                self.ensure_role(out);
            }
            Ev::Text(t) => {
                self.ensure_role(out);
                sse(out, &self.chunk(json!({ "content": t }), None));
            }
            Ev::Reasoning(t) => {
                self.ensure_role(out);
                sse(out, &self.chunk(json!({ "reasoning_content": t }), None));
            }
            Ev::ToolStart { item_id, call_id, name, custom } => {
                self.ensure_role(out);
                let index = self.tools.len();
                self.tools.insert(item_id, (index, custom));
                self.last_tool = Some((index, custom));
                self.saw_tool = true;
                let mut tc = json!({ "index": index, "id": call_id });
                if custom {
                    tc["type"] = "custom".into();
                    tc["custom"] = json!({ "name": name, "input": "" });
                } else {
                    tc["type"] = "function".into();
                    tc["function"] = json!({ "name": name, "arguments": "" });
                }
                sse(out, &self.chunk(json!({ "tool_calls": [tc] }), None));
            }
            Ev::ToolArgs { item_id, delta } => {
                let Some((index, custom)) = self.tools.get(&item_id).copied().or(self.last_tool)
                else {
                    return;
                };
                let mut tc = json!({ "index": index });
                if custom {
                    tc["custom"] = json!({ "input": delta });
                } else {
                    tc["function"] = json!({ "arguments": delta });
                }
                sse(out, &self.chunk(json!({ "tool_calls": [tc] }), None));
            }
            Ev::Completed { usage, incomplete, model } => {
                if let Some(m) = model {
                    self.model = m;
                }
                self.ensure_role(out);
                let reason = finish_reason(incomplete.as_deref(), self.saw_tool);
                sse(out, &self.chunk(json!({}), Some(reason)));
                // 只有客户端要过才发 usage 那条：没要求的客户端会把一条 `choices: []` 的
                // chunk 当成异常形状。
                if self.include_usage {
                    let u = usage.as_ref().map(usage_json).unwrap_or(Value::Null);
                    sse(
                        out,
                        &json!({
                            "id": self.id,
                            "object": "chat.completion.chunk",
                            "created": self.created,
                            "model": self.model,
                            "choices": [],
                            "usage": u,
                        }),
                    );
                }
                self.done(out);
            }
            Ev::Failed { etype, message } => self.fail(etype.as_deref(), &message, out),
        }
    }

    /// 发首块（只带 role）。内容、工具调用、收尾之前都要先过这里。
    fn ensure_role(&mut self, out: &mut String) {
        if self.role_sent {
            return;
        }
        self.role_sent = true;
        sse(out, &self.chunk(json!({ "role": "assistant", "content": "" }), None));
    }

    /// 流中失败：按 OpenAI 的做法在流里发一个 `error` 对象，再正常收尾。
    fn fail(&mut self, etype: Option<&str>, message: &str, out: &mut String) {
        let etype = etype.unwrap_or("upstream_error");
        sse(out, &json!({ "error": { "message": message, "type": etype, "code": etype } }));
        self.done(out);
    }

    fn done(&mut self, out: &mut String) {
        out.push_str("data: [DONE]\n\n");
        self.closed = true;
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }
}

/// 写一条 SSE 事件。
fn sse(out: &mut String, v: &Value) {
    if let Ok(s) = serde_json::to_string(v) {
        out.push_str("data: ");
        out.push_str(&s);
        out.push_str("\n\n");
    }
}

// ---------- 非流式聚合 ----------

/// 把一整段上游 SSE 聚合成一个 `chat.completion` 对象。
///
/// 客户端没要流式时走这条：上游那头**只有流式**（非流式请求被拒），所以「非流式」在 coban
/// 这一层是「收完再拼」。`Err` 是流里报出来的失败（`(error.type, error.message)`），由调用
/// 方翻成一个错误响应——把一段失败的流拼成一条 `finish_reason: "stop"` 的正常回复，等于把
/// 上游的报错吞掉。
pub fn aggregate(sse_body: &[u8], model: &str) -> Result<Vec<u8>, (Option<String>, String)> {
    let mut id: Option<String> = None;
    let mut resolved_model: Option<String> = None;
    let mut text = String::new();
    let mut reasoning = String::new();
    // (call_id, name, arguments)，按上游给出的顺序。
    // `(call_id, 工具名, 攒起来的参数/自由文本, 是不是 custom 调用)`
    let mut calls: Vec<(String, String, String, bool)> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut usage: Option<Usage> = None;
    let mut incomplete: Option<String> = None;
    let mut completed = false;

    for line in String::from_utf8_lossy(sse_body).lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Some(ev) = parse_event(data.trim()) else { continue };
        match ev {
            Ev::Created { id: rid, model: m } => {
                id = rid.as_deref().map(chat_id);
                resolved_model = m;
            }
            Ev::Text(t) => text.push_str(&t),
            Ev::Reasoning(t) => reasoning.push_str(&t),
            Ev::ToolStart { item_id, call_id, name, custom } => {
                index_of.insert(item_id, calls.len());
                calls.push((call_id, name, String::new(), custom));
            }
            Ev::ToolArgs { item_id, delta } => {
                // 与流式那头同一套归位规则：`item_id` 对不上就落到最近开始的那个调用上。
                if let Some(c) = index_of
                    .get(&item_id)
                    .copied()
                    .or_else(|| calls.len().checked_sub(1))
                    .and_then(|i| calls.get_mut(i))
                {
                    c.2.push_str(&delta);
                }
            }
            Ev::Completed { usage: u, incomplete: inc, model: m } => {
                usage = u;
                incomplete = inc;
                if m.is_some() {
                    resolved_model = m;
                }
                completed = true;
            }
            Ev::Failed { etype, message } => return Err((etype, message)),
        }
    }

    // 没有终局事件就是流断了。同流式那头的理由：不能把截断的内容当成完整回复交出去。
    if !completed {
        return Err((
            Some("upstream_error".to_owned()),
            "the upstream stream ended before the response completed".to_owned(),
        ));
    }

    let mut message = serde_json::Map::new();
    message.insert("role".into(), Value::String("assistant".into()));
    // 只有工具调用、没有正文时 `content` 是 null（OpenAI 就是这么回的），不是空串。
    message.insert(
        "content".into(),
        if text.is_empty() && !calls.is_empty() { Value::Null } else { Value::String(text) },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if !calls.is_empty() {
        message.insert(
            "tool_calls".into(),
            Value::Array(
                calls
                    .into_iter()
                    .map(|(call_id, name, args, custom)| {
                        // `custom` 那种带的是自由文本：空就是空，补一个 `{}` 反而是往工具的
                        // 输入里塞了两个字符。
                        if custom {
                            return json!({
                                "id": call_id,
                                "type": "custom",
                                "custom": { "name": name, "input": args },
                            });
                        }
                        json!({
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                // 一个参数都没生成时补 `{}`：客户端多半直接 JSON.parse。
                                "arguments": if args.is_empty() { "{}".to_owned() } else { args },
                            },
                        })
                    })
                    .collect(),
            ),
        );
    }
    let saw_tool = message.contains_key("tool_calls");

    let body = json!({
        "id": id.unwrap_or_else(fallback_id),
        "object": "chat.completion",
        "created": crate::credentials::now_secs() as i64,
        "model": resolved_model.unwrap_or_else(|| model.to_owned()),
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason(incomplete.as_deref(), saw_tool),
        }],
        "usage": usage.as_ref().map(usage_json).unwrap_or(Value::Null),
    });
    serde_json::to_vec(&body).map_err(|e| (None, format!("failed to build the response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(body: &str) -> Translated {
        translate_request(body.as_bytes(), false).expect("translates")
    }

    fn upstream(body: &str) -> Value {
        serde_json::from_slice(&translate(body).body).expect("valid JSON")
    }

    /// 上游的两条硬约束在这条路上也必须成立，且是**我们**钉的（客户端从不发这两个字段）：
    /// `store: false` 与 `stream: true`。少任何一个都是一句指不到原因的 400。
    #[test]
    fn the_upstream_hard_requirements_are_pinned() {
        let v = upstream(r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        // `instructions` 缺了上游直接 400，客户端没给系统消息时也得有一句。
        assert!(v["instructions"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(v["input"][0]["role"], "user");
        assert_eq!(v["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(v["input"][0]["content"][0]["text"], "hi");
    }

    /// 客户端要不要流是**它自己的事**，与上游那条永远流式无关：非流式记成 collapse，
    /// 由 proxy 那头收拢。
    #[test]
    fn the_clients_stream_choice_is_kept_separately() {
        let base = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]"#;
        assert!(!translate(&format!("{base}}}")).stream, "漏传 stream 就是要整块 JSON");
        assert!(!translate(&format!(r#"{base},"stream":false}}"#)).stream);
        assert!(translate(&format!(r#"{base},"stream":true}}"#)).stream);
        // 上游那条无论如何都是流式。
        assert_eq!(upstream(&format!("{base}}}"))["stream"], true);

        let t = translate(&format!(
            r#"{base},"stream":true,"stream_options":{{"include_usage":true}}}}"#
        ));
        assert!(t.include_usage);
    }

    /// `system`/`developer` 合并进 `instructions`（Responses 的 input 里没有这两个角色），
    /// 多条按出现顺序拼起来。
    #[test]
    fn system_messages_become_instructions() {
        let v = upstream(
            r#"{"model":"m","messages":[
                {"role":"system","content":"be brief"},
                {"role":"user","content":"hi"},
                {"role":"developer","content":"and precise"}
            ]}"#,
        );
        assert_eq!(v["instructions"], "be brief\n\nand precise");
        // 合并掉的那两条不能又在 input 里出现一遍。
        assert_eq!(v["input"].as_array().unwrap().len(), 1);
    }

    /// 一整轮工具调用（助手发起 → 工具回结果）要翻成 Responses 的那两种 item。
    #[test]
    fn a_full_tool_round_trip_translates() {
        let v = upstream(
            r#"{"model":"m","messages":[
                {"role":"user","content":"ls"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"cmd\":\"ls\"}"}}
                ]},
                {"role":"tool","tool_call_id":"call_1","content":"a.txt"}
            ],
            "tools":[{"type":"function","function":{"name":"shell","description":"run","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"shell"}}}"#,
        );
        let input = v["input"].as_array().unwrap();
        // 助手那条只有 tool_calls 没有正文：不能凭空塞一个空的 output_text 块。
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "shell");
        assert_eq!(input[1]["arguments"], r#"{"cmd":"ls"}"#);
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "a.txt");
        // tools 要摊平：Chat 的 {type, function:{name}} → Responses 的 {type, name}。
        assert_eq!(v["tools"][0]["name"], "shell");
        assert!(v["tools"][0].get("function").is_none());
        // strict 显式写 false：缺省可能被按严格模式处理，而客户端给的 schema 多半不满足
        // 严格模式的附加约束，那会让每个带工具的请求都 400。
        assert_eq!(v["tools"][0]["strict"], false);
        assert_eq!(v["tool_choice"], json!({ "type": "function", "name": "shell" }));
    }

    /// `custom` 工具（自由文本入参）整轮都要过得去：工具定义摊平、调用与结果换成 Responses
    /// 那两种项。以前这里直接回一句「只支持 function 工具」把整条请求拦在门口。
    #[test]
    fn a_custom_tool_round_trip_translates() {
        let v = upstream(
            r#"{"model":"m","messages":[
                {"role":"user","content":"patch it"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_1","type":"custom","custom":{"name":"apply_patch","input":"*** Begin Patch"}}
                ]},
                {"role":"tool","tool_call_id":"call_1","content":"done"}
            ],
            "tools":[
                {"type":"custom","custom":{"name":"apply_patch","description":"edit files",
                 "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}},
                {"type":"function","function":{"name":"shell","parameters":{"type":"object"}}}
            ]}"#,
        );
        // 工具定义：摊平，`format` 原样带过去（那是客户端定的语法约束，不许替它改）。
        let tools = v["tools"].as_array().unwrap();
        let custom = tools.iter().find(|t| t["type"] == "custom").expect("custom 工具得在");
        assert_eq!(custom["name"], "apply_patch");
        assert_eq!(custom["description"], "edit files");
        assert_eq!(custom["format"]["syntax"], "lark");
        assert!(custom.get("custom").is_none(), "嵌的那一层该摊平");
        assert!(custom.get("parameters").is_none(), "自由文本工具没有 JSON 参数");

        let input = v["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "custom_tool_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "apply_patch");
        assert_eq!(input[1]["input"], "*** Begin Patch");
        // 结果项也得跟着换类型：Chat 那头两种结果都是一条 `role: "tool"`，只有前面那条
        // 助手消息认得出是哪一种。
        assert_eq!(input[2]["type"], "custom_tool_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "done");

        // 函数调用的结果不受影响。
        let v = upstream(
            r#"{"model":"m","messages":[
                {"role":"assistant","tool_calls":[
                    {"id":"c1","type":"function","function":{"name":"shell","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"c1","content":"a.txt"}]}"#,
        );
        assert_eq!(v["input"][1]["type"], "function_call_output");

        // 指名一个 custom 工具时 `type` 得跟着换：写成 function 上游就找不到那个工具了。
        let v = upstream(
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],
                "tools":[{"type":"custom","custom":{"name":"apply_patch"}}],
                "tool_choice":{"type":"custom","custom":{"name":"apply_patch"}}}"#,
        );
        assert_eq!(v["tool_choice"], json!({ "type": "custom", "name": "apply_patch" }));

        // 名字都没有的 custom 工具：这就不是能翻的形状，如实报错（同 function 那头）。
        assert!(
            translate_request(
                br#"{"model":"m","messages":[{"role":"user","content":"hi"}],
                    "tools":[{"type":"custom","custom":{"description":"x"}}]}"#,
                false
            )
            .is_err()
        );
        // 别的工具类型仍然拦在门口：那才是真没法翻。
        assert!(
            translate_request(
                br#"{"model":"m","messages":[{"role":"user","content":"hi"}],
                    "tools":[{"type":"web_search"}]}"#,
                false
            )
            .is_err()
        );
    }

    /// 改名要改对，而上游不认的参数**一个都不能带过去**——带了是每条请求都 400。
    #[test]
    fn only_the_parameters_the_upstream_accepts_are_forwarded() {
        let v = upstream(
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],
                "reasoning_effort":"high","response_format":{"type":"json_object"},
                "max_tokens":64,"max_completion_tokens":128,
                "temperature":0.7,"top_p":0.9,"seed":1,"stop":["x"],"user":"u",
                "presence_penalty":0.1,"frequency_penalty":0.1,"logprobs":true}"#,
        );
        assert_eq!(v["reasoning"]["effort"], "high");
        assert_eq!(v["text"]["format"]["type"], "json_object");
        // 实测上游回 `Unsupported parameter: max_output_tokens`，所以这两个只能丢。
        assert!(v.get("max_output_tokens").is_none());
        for k in [
            "temperature",
            "top_p",
            "seed",
            "stop",
            "user",
            "presence_penalty",
            "frequency_penalty",
            "logprobs",
            "max_tokens",
            "max_completion_tokens",
        ] {
            assert!(v.get(k).is_none(), "`{k}` 不该出现在发往上游的体里");
        }
        // 客户端没提推理档位时**不能**凭空加一个：不支持推理的模型会直接 400。
        let v = upstream(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#);
        assert!(v.get("reasoning").is_none());
    }

    /// json_object 那档：`input` 里必须出现 "json" 这个词，否则上游一句 400
    /// （`Response input messages must contain the word 'json' in some form…`）。那道口子不看
    /// `instructions`，而系统消息正是被我们并进 `instructions` 的——不补这一句，等于我们这一跳
    /// 把一份本来能过的请求弄成了 400。
    #[test]
    fn json_object_mode_makes_sure_the_input_says_json() {
        // 「请输出 JSON」写在系统提示里的那种（最常见）：input 里补上一句。
        let v = upstream(
            r#"{"model":"m","response_format":{"type":"json_object"},"messages":[
                {"role":"system","content":"Return a JSON object with the field `answer`."},
                {"role":"user","content":"how tall is Everest"}]}"#,
        );
        let content = v["input"][0]["content"].as_array().unwrap();
        assert_eq!(v["input"].as_array().unwrap().len(), 1, "不该凭空多一个对话轮次");
        assert_eq!(content[0]["text"], "how tall is Everest", "客户端那段话原样留着");
        assert_eq!(content[1]["text"], JSON_HINT, "补的一句挂在最后一条用户消息后面");
        assert_eq!(v["text"]["format"]["type"], "json_object");

        // 用户消息自己提过就一个字都不动，大小写不论。
        let v = upstream(
            r#"{"model":"m","response_format":{"type":"json_object"},
                "messages":[{"role":"user","content":"reply as Json"}]}"#,
        );
        assert_eq!(v["input"][0]["content"].as_array().unwrap().len(), 1);

        // 一条用户消息都没有（只有工具结果）时才新起一条。
        let v = upstream(
            r#"{"model":"m","response_format":{"type":"json_object"},"messages":[
                {"role":"assistant","tool_calls":[{"id":"c1","type":"function",
                 "function":{"name":"shell","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"c1","content":"ok"}]}"#,
        );
        let input = v["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][0]["text"], JSON_HINT);

        // 别的档不补：json_schema 与不要求格式的请求都不该被动一个字。
        let v = upstream(
            r#"{"model":"m","response_format":{"type":"json_schema","json_schema":
                {"name":"r","schema":{"type":"object"}}},
                "messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(v["input"][0]["content"].as_array().unwrap().len(), 1);
        let v = upstream(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(v["input"][0]["content"].as_array().unwrap().len(), 1);
    }

    /// 多模态：图片按 input_image 送，`image_url` 收对象与裸字符串两种写法。
    #[test]
    fn image_parts_translate() {
        let v = upstream(
            r#"{"model":"m","messages":[{"role":"user","content":[
                {"type":"text","text":"what is this"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}
            ]}]}"#,
        );
        let c = &v["input"][0]["content"];
        assert_eq!(c[0], json!({ "type": "input_text", "text": "what is this" }));
        assert_eq!(
            c[1],
            json!({ "type": "input_image", "image_url": "data:image/png;base64,AAA" })
        );
    }

    /// 这一层判得出来的形状错误就在这一层拒，别送到上游去换一句指不到原因的 400。
    #[test]
    fn requests_that_cannot_work_are_rejected_here() {
        let err = |b: &str| translate_request(b.as_bytes(), false).expect_err("rejected");
        // n > 1：上游一次只出一条，静默给一条会让按 choices[1] 取值的客户端拿到越界。
        assert!(
            err(r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"n":2}"#)
                .contains("`n`")
        );
        assert!(err(r#"{"messages":[{"role":"user","content":"hi"}]}"#).contains("model"));
        assert!(err(r#"{"model":"m","messages":[]}"#).contains("messages"));
        assert!(
            err(r#"{"model":"m","messages":[{"role":"nobody","content":"hi"}]}"#)
                .contains("nobody")
        );
        assert!(err("not json").contains("JSON"));
        // 只支持 function 工具，别的类型如实拒掉（比翻译成一个上游不认的形状好）。
        assert!(
            err(r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"web_search"}]}"#)
                .contains("web_search")
        );
    }

    /// 把整段 SSE 一次喂进去，取翻出来的那份。
    fn stream_out(sse: &str, include_usage: bool) -> String {
        let mut x = StreamXlate::new("m".into(), include_usage);
        let mut out = String::from_utf8_lossy(&x.feed(&Bytes::from(sse.to_owned()))).into_owned();
        out.push_str(&String::from_utf8_lossy(&x.flush()));
        out
    }

    fn events(out: &str) -> Vec<Value> {
        out.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).expect("chunk is valid JSON"))
            .collect()
    }

    const TEXT_SSE: &str = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_abc\",\"model\":\"gpt-5.4\"}}\n",
        "\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"He\"}\n",
        "\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n",
        "\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_abc\",\"model\":\"gpt-5.4\",\"usage\":{\"input_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":2},\"output_tokens\":3,\"output_tokens_details\":{\"reasoning_tokens\":1},\"total_tokens\":10}}}\n",
        "\n",
    );

    /// 流式的骨架：首块只带 role，正文逐块，末尾一条带 finish_reason，最后 `[DONE]`。
    /// 少了 `[DONE]` 的话，客户端会一直等一个不会来的结束标记。
    #[test]
    fn a_text_stream_becomes_chat_chunks() {
        let out = stream_out(TEXT_SSE, false);
        let evs = events(&out);
        assert_eq!(evs[0]["choices"][0]["delta"], json!({ "role": "assistant", "content": "" }));
        assert_eq!(evs[1]["choices"][0]["delta"]["content"], "He");
        assert_eq!(evs[2]["choices"][0]["delta"]["content"], "llo");
        assert_eq!(evs[3]["choices"][0]["finish_reason"], "stop");
        assert!(out.ends_with("data: [DONE]\n\n"));
        // id 沿用上游那条 response 的，好让两边日志对得上；模型报上游实际选的那个。
        assert_eq!(evs[0]["id"], "chatcmpl-abc");
        assert_eq!(evs[0]["object"], "chat.completion.chunk");
        assert_eq!(evs[3]["model"], "gpt-5.4");
        // 没要 usage 就不发那条 `choices: []` 的 chunk。
        assert!(evs.iter().all(|e| e.get("usage").is_none()));
    }

    /// chunk 边界不保证落在换行上：逐字节喂进去，结果必须与一次喂完逐字节相同。
    #[test]
    fn chunk_boundaries_do_not_change_the_output() {
        let mut x = StreamXlate::new("m".into(), false);
        let mut out = String::new();
        for b in TEXT_SSE.as_bytes() {
            out.push_str(&String::from_utf8_lossy(&x.feed(&Bytes::copy_from_slice(&[*b]))));
        }
        out.push_str(&String::from_utf8_lossy(&x.flush()));
        assert_eq!(out, stream_out(TEXT_SSE, false));
    }

    /// 要了 usage 才发那条：一条 `choices: []` 的 chunk 对没要过的客户端是个异常形状。
    #[test]
    fn include_usage_appends_a_usage_only_chunk() {
        let evs = events(&stream_out(TEXT_SSE, true));
        let last = evs.last().unwrap();
        assert_eq!(last["choices"].as_array().unwrap().len(), 0);
        assert_eq!(last["usage"]["prompt_tokens"], 7);
        assert_eq!(last["usage"]["completion_tokens"], 3);
        assert_eq!(last["usage"]["total_tokens"], 10);
        assert_eq!(last["usage"]["prompt_tokens_details"]["cached_tokens"], 2);
        assert_eq!(last["usage"]["completion_tokens_details"]["reasoning_tokens"], 1);
    }

    const TOOL_SSE: &str = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\"}}\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"shell\"}}\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"cmd\\\"\"}\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\":\\\"ls\\\"}\"}\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"id\":\"fc_2\",\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"read\"}}\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_2\",\"delta\":\"{}\"}\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n",
    );

    /// 工具调用的 `index` 必须只数工具调用。上游的 `output_index` 把推理项也数在内
    /// （这条流里第一个工具调用的 output_index 是 1），照抄会让客户端按 index 拼参数时
    /// 拼到一个不存在的调用上。
    #[test]
    fn tool_call_indexes_count_only_tool_calls() {
        let evs = events(&stream_out(TOOL_SSE, false));
        let calls: Vec<&Value> =
            evs.iter().filter_map(|e| e["choices"][0]["delta"].get("tool_calls")).collect();
        assert_eq!(calls[0][0]["index"], 0);
        assert_eq!(calls[0][0]["id"], "call_1");
        assert_eq!(calls[0][0]["function"]["name"], "shell");
        assert_eq!(calls[1][0]["index"], 0);
        assert_eq!(calls[1][0]["function"]["arguments"], "{\"cmd\"");
        assert_eq!(calls[2][0]["index"], 0);
        // 第二个工具调用才是 index 1。
        assert_eq!(calls[3][0]["index"], 1);
        assert_eq!(calls[3][0]["id"], "call_2");
        assert_eq!(calls[4][0]["index"], 1);
        // 这一轮出的是工具调用，finish_reason 不是 stop。
        assert_eq!(evs.last().unwrap()["choices"][0]["finish_reason"], "tool_calls");
    }

    /// 流断在终局事件之前时**不能**补一条 `finish_reason: "stop"` 假装正常结束：
    /// 客户端会把一段截断的回复当成完整答案存下来。
    #[test]
    fn a_truncated_stream_is_reported_as_an_error() {
        let out = stream_out(
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"half\"}\n",
            ),
            false,
        );
        let evs = events(&out);
        let last = evs.last().unwrap();
        assert!(last["error"]["message"].as_str().unwrap().contains("ended before"));
        assert!(evs.iter().all(|e| e["choices"][0]["finish_reason"].is_null()));
        // 报了错也要收尾，否则客户端等在那个不会来的 `[DONE]` 上。
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    /// 「HTTP 200 但流里说失败了」要翻成流里的 error 事件，不能当成正常收尾。
    #[test]
    fn an_in_stream_failure_becomes_an_error_event() {
        let out = stream_out(
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"error\":null}}\n",
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"boom\"}}}\n",
            ),
            false,
        );
        let evs = events(&out);
        let last = evs.last().unwrap();
        assert_eq!(last["error"]["message"], "boom");
        assert_eq!(last["error"]["type"], "server_error");
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    /// 非流式聚合：正文拼起来、usage 换名、finish_reason 定对。
    #[test]
    fn aggregate_builds_one_completion() {
        let v: Value =
            serde_json::from_slice(&aggregate(TEXT_SSE.as_bytes(), "m").unwrap()).unwrap();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["id"], "chatcmpl-abc");
        assert_eq!(v["model"], "gpt-5.4");
        assert_eq!(v["choices"][0]["message"]["role"], "assistant");
        assert_eq!(v["choices"][0]["message"]["content"], "Hello");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["prompt_tokens"], 7);
        assert_eq!(v["usage"]["completion_tokens"], 3);
    }

    /// 只有工具调用时 `content` 是 null（OpenAI 就是这么回的），参数按调用归位。
    #[test]
    fn aggregate_collects_tool_calls() {
        let v: Value =
            serde_json::from_slice(&aggregate(TOOL_SSE.as_bytes(), "m").unwrap()).unwrap();
        let msg = &v["choices"][0]["message"];
        assert!(msg["content"].is_null());
        assert_eq!(msg["tool_calls"][0]["id"], "call_1");
        assert_eq!(msg["tool_calls"][0]["function"]["arguments"], r#"{"cmd":"ls"}"#);
        assert_eq!(msg["tool_calls"][1]["id"], "call_2");
        assert_eq!(msg["tool_calls"][1]["function"]["arguments"], "{}");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    }

    const CUSTOM_TOOL_SSE: &str = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"ctc_1\",\"type\":\"custom_tool_call\",\"call_id\":\"call_1\",\"name\":\"apply_patch\"}}\n",
        "data: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"ctc_1\",\"delta\":\"*** Begin\"}\n",
        "data: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"ctc_1\",\"delta\":\" Patch\"}\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n",
    );

    /// `custom` 工具调用回程也得是 `custom` 那套字段：落到 `function.arguments` 上，客户端
    /// 会拿一段自由文本去 JSON.parse。两条渲染路径必须给出同一种形状。
    #[test]
    fn a_custom_tool_call_comes_back_in_the_custom_shape() {
        let evs = events(&stream_out(CUSTOM_TOOL_SSE, false));
        let calls: Vec<&Value> =
            evs.iter().filter_map(|e| e["choices"][0]["delta"].get("tool_calls")).collect();
        assert_eq!(calls[0][0]["type"], "custom");
        assert_eq!(calls[0][0]["id"], "call_1");
        assert_eq!(calls[0][0]["custom"]["name"], "apply_patch");
        assert_eq!(calls[0][0]["custom"]["input"], "");
        assert!(calls[0][0].get("function").is_none());
        assert_eq!(calls[1][0]["custom"]["input"], "*** Begin");
        assert_eq!(calls[2][0]["custom"]["input"], " Patch");
        assert_eq!(evs.last().unwrap()["choices"][0]["finish_reason"], "tool_calls");

        let v: Value =
            serde_json::from_slice(&aggregate(CUSTOM_TOOL_SSE.as_bytes(), "m").unwrap()).unwrap();
        let tc = &v["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["type"], "custom");
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["custom"]["name"], "apply_patch");
        assert_eq!(tc["custom"]["input"], "*** Begin Patch");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    }

    /// 失败的流不能被拼成一条 `finish_reason: "stop"` 的正常回复——那等于把上游的报错吞掉。
    #[test]
    fn aggregate_refuses_to_swallow_a_failure() {
        let (etype, msg) = aggregate(
            b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_error\",\"message\":\"boom\"}}}\n",
            "m",
        )
        .expect_err("a failed stream is an error");
        assert_eq!(etype.as_deref(), Some("server_error"));
        assert_eq!(msg, "boom");

        // 流断在终局事件之前同理。
        let (_, msg) =
            aggregate(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"half\"}\n", "m")
                .expect_err("a truncated stream is an error");
        assert!(msg.contains("ended before"));
    }

    /// 被上限截断的那一轮，finish_reason 是 length 而不是 stop——客户端据它决定要不要续写。
    #[test]
    fn hitting_the_output_cap_is_reported_as_length() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"cut\"}\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n",
        );
        let v: Value = serde_json::from_slice(&aggregate(sse.as_bytes(), "m").unwrap()).unwrap();
        assert_eq!(v["choices"][0]["finish_reason"], "length");
        let evs = events(&stream_out(sse, false));
        assert_eq!(evs.last().unwrap()["choices"][0]["finish_reason"], "length");
    }

    /// 推理摘要按事实上的通行字段转出，两条路都要有（少了它，会思考的模型在客户端那头
    /// 是一段长时间的空白）。
    #[test]
    fn reasoning_summaries_are_forwarded() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"m\"}}\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n",
        );
        let evs = events(&stream_out(sse, false));
        assert!(evs.iter().any(|e| e["choices"][0]["delta"]["reasoning_content"] == "think"));
        let v: Value = serde_json::from_slice(&aggregate(sse.as_bytes(), "m").unwrap()).unwrap();
        assert_eq!(v["choices"][0]["message"]["reasoning_content"], "think");
        assert_eq!(v["choices"][0]["message"]["content"], "ok");
    }
}
