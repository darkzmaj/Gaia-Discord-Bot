use serde_json::{Map, Value};
use serenity::model::channel::Embed;

/// A stored Discord message object: `content`, `embeds`, `allowed_mentions`.
pub type StoredMessage = Map<String, Value>;

/// JavaScript `string.length` (UTF-16 code units).
pub fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn str_len(value: Option<&Value>) -> usize {
    value.and_then(Value::as_str).map(js_len).unwrap_or(0)
}

fn max(value: Option<&Value>, length: usize, name: &str, errors: &mut Vec<String>) {
    if let Some(text) = value.and_then(Value::as_str) {
        if js_len(text) > length {
            errors.push(format!("{name} must be at most {length} characters."));
        }
    }
}

/// JavaScript truthiness of a JSON value (`undefined` = None).
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

pub fn is_url(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .and_then(|text| reqwest::Url::parse(text).ok())
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// Validates supported Discord message/embed JSON without rewriting its design.
pub fn validate_embed_payload(raw: &str) -> Result<StoredMessage, Vec<String>> {
    let value: Value = serde_json::from_str(raw).map_err(|_| vec!["JSON is malformed.".to_string()])?;
    let Value::Object(mut message) = value else {
        return Err(vec!["JSON must be a message object.".into()]);
    };
    let mut errors = Vec::new();
    // Discord exports these harmless defaults. Keep meaningful unsupported data explicit.
    if message.get("attachments").and_then(Value::as_array).is_some_and(Vec::is_empty) {
        message.remove("attachments");
    }
    if message.get("tts") == Some(&Value::Bool(false)) {
        message.remove("tts");
    }
    if message.get("flags").and_then(Value::as_f64) == Some(0.0) {
        message.remove("flags");
    }
    for key in message.keys() {
        if !matches!(key.as_str(), "content" | "embeds" | "allowed_mentions") {
            errors.push(if key == "attachments" {
                "message.attachments must be uploaded through Gaia attachment management.".to_string()
            } else {
                format!("message.{key} is not supported.")
            });
        }
    }
    max(message.get("content"), 2000, "content", &mut errors);
    let embeds_value = message.get("embeds").filter(|v| !v.is_null());
    if message.contains_key("embeds") && !message["embeds"].is_array() {
        errors.push("embeds must be an array.".into());
    }
    let empty = Vec::new();
    let embeds = embeds_value.and_then(Value::as_array).unwrap_or(&empty);
    if !truthy(message.get("content")) && embeds.is_empty() {
        errors.push("Provide content, at least one embed, or both.".into());
    }
    if embeds.len() > 10 {
        errors.push("Discord allows at most 10 embeds.".into());
    }
    for (index, embed) in embeds.iter().enumerate() {
        let Some(e) = embed.as_object() else {
            errors.push(format!("embeds[{index}] must be an object."));
            continue;
        };
        max(e.get("title"), 256, &format!("embeds[{index}].title"), &mut errors);
        max(e.get("description"), 4096, &format!("embeds[{index}].description"), &mut errors);
        if truthy(e.get("url")) && !is_url(e.get("url")) {
            errors.push(format!("embeds[{index}].url must be an http(s) URL."));
        }
        for key in ["image", "thumbnail", "video"] {
            let url = e.get(key).and_then(|v| v.get("url"));
            let attachment = url.and_then(Value::as_str).is_some_and(|u| u.starts_with("attachment://"));
            if truthy(url) && !is_url(url) && !attachment {
                errors.push(format!("embeds[{index}].{key}.url must be an http(s) URL or attachment:// filename."));
            }
        }
        let footer_text = e.get("footer").and_then(|f| f.get("text"));
        if truthy(e.get("footer")) {
            max(footer_text, 2048, &format!("embeds[{index}].footer.text"), &mut errors);
        }
        let author_name = e.get("author").and_then(|a| a.get("name"));
        if truthy(e.get("author")) {
            max(author_name, 256, &format!("embeds[{index}].author.name"), &mut errors);
            let url = e["author"].get("url");
            if truthy(url) && !is_url(url) {
                errors.push(format!("embeds[{index}].author.url must be an http(s) URL."));
            }
        }
        if e.contains_key("fields") && !e["fields"].is_array() {
            errors.push(format!("embeds[{index}].fields must be an array."));
        }
        let fields = e.get("fields").and_then(Value::as_array).unwrap_or(&empty);
        if fields.len() > 25 {
            errors.push(format!("embeds[{index}] has more than 25 fields."));
        }
        let mut total = str_len(e.get("title")) + str_len(e.get("description")) + str_len(footer_text) + str_len(author_name);
        for (field_index, field) in fields.iter().enumerate() {
            max(field.get("name"), 256, &format!("embeds[{index}].fields[{field_index}].name"), &mut errors);
            max(field.get("value"), 1024, &format!("embeds[{index}].fields[{field_index}].value"), &mut errors);
            total += str_len(field.get("name")) + str_len(field.get("value"));
        }
        if total > 6000 {
            errors.push(format!("embeds[{index}] totals more than 6000 characters."));
        }
        if serde_json::from_value::<Embed>(embed.clone()).is_err() {
            errors.push(format!("embeds[{index}] is not Discord-compatible."));
        }
    }
    if errors.is_empty() { Ok(message) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::validate_embed_payload;

    #[test]
    fn validates_discord_limits() {
        assert!(validate_embed_payload(r#"{"embeds":[{"title":"Hello"}]}"#).is_ok());
        let long = format!(r#"{{"embeds":[{{"description":"{}"}}]}}"#, "x".repeat(4097));
        assert!(validate_embed_payload(&long).is_err());
    }

    #[test]
    fn rejects_unsupported_message_payload_keys() {
        assert!(validate_embed_payload(r#"{"content":"hi","components":[]}"#).is_err());
    }

    #[test]
    fn normalizes_empty_exported_attachments_and_rejects_unmanaged_ones() {
        let empty = validate_embed_payload(r#"{"content":null,"embeds":[{"title":"Hello"}],"attachments":[]}"#).unwrap();
        assert!(!empty.contains_key("attachments"));
        let errors = validate_embed_payload(r#"{"content":"x","attachments":[{"filename":"x.png"}]}"#).unwrap_err();
        assert_eq!(errors, vec!["message.attachments must be uploaded through Gaia attachment management."]);
    }

    #[test]
    fn rejects_malformed_and_empty_messages() {
        assert_eq!(validate_embed_payload("{").unwrap_err(), vec!["JSON is malformed."]);
        assert_eq!(validate_embed_payload("[]").unwrap_err(), vec!["JSON must be a message object."]);
        assert_eq!(validate_embed_payload("{}").unwrap_err(), vec!["Provide content, at least one embed, or both."]);
        assert!(validate_embed_payload(r#"{"embeds":[{"image":{"url":"attachment://a.png"}}]}"#).is_ok());
        assert!(validate_embed_payload(r#"{"embeds":[{"image":{"url":"ftp://x"}}]}"#).is_err());
    }
}
