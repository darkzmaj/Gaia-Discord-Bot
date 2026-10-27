use super::embed_validation::{js_len, validate_embed_payload};
use crate::db::repositories::{EmbedAttachment, GaiaRepository};
use anyhow::{anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serenity::builder::{CreateAllowedMentions, CreateAttachment, CreateEmbed, CreateMessage};
use serenity::model::channel::Embed;
use serenity::model::guild::Guild;
use serenity::model::id::{RoleId, UserId};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::LazyLock;

pub const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TEMPLATE_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeliveryMention {
    pub roles: Vec<String>,
    pub everyone: bool,
    pub here: bool,
}

pub fn no_mention() -> DeliveryMention {
    DeliveryMention::default()
}

pub fn parse_mention(value: Option<&str>) -> DeliveryMention {
    let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(value.unwrap_or("")) else {
        return no_mention();
    };
    let truthy = |key: &str| super::embed_validation::truthy(parsed.get(key));
    DeliveryMention {
        roles: parsed
            .get("roles")
            .and_then(Value::as_array)
            .map(|roles| roles.iter().filter_map(Value::as_str).take(25).map(String::from).collect())
            .unwrap_or_default(),
        everyone: truthy("everyone"),
        here: truthy("here"),
    }
}

pub fn mention_label(mention: &DeliveryMention) -> String {
    let mut parts: Vec<String> = mention.roles.iter().map(|id| format!("<@&{id}>")).collect();
    if mention.everyone {
        parts.push("@everyone".into());
    }
    if mention.here {
        parts.push("@here".into());
    }
    if parts.is_empty() { "None".into() } else { parts.join(", ") }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMention {
    pub mention: DeliveryMention,
    pub warning: Option<String>,
}

/// Pure core of `resolve_mention`: keep only existing unmanaged roles, and mass
/// mentions only when Gaia may mention everyone.
pub fn resolve_mention_with(role_usable: impl Fn(&str) -> bool, mass_allowed: bool, requested: &DeliveryMention) -> ResolvedMention {
    let roles: Vec<String> = requested.roles.iter().filter(|id| role_usable(id)).cloned().collect();
    let mention = DeliveryMention { everyone: requested.everyone && mass_allowed, here: requested.here && mass_allowed, roles };
    let dropped = requested.roles.len() - mention.roles.len() + usize::from(requested.everyone && !mention.everyone) + usize::from(requested.here && !mention.here);
    ResolvedMention {
        mention,
        warning: (dropped > 0).then(|| "One or more requested mentions were unavailable or Gaia no longer has permission to make a mass mention.".to_string()),
    }
}

/// `bot_id` is Gaia's own user id (`cache.current_user().id`).
pub fn resolve_mention(guild: &Guild, bot_id: UserId, requested: &DeliveryMention) -> ResolvedMention {
    let mass_allowed = guild.members.get(&bot_id).is_some_and(|me| guild.member_permissions(me).mention_everyone());
    resolve_mention_with(
        |id| id.parse::<u64>().ok().filter(|&n| n != 0).and_then(|n| guild.roles.get(&RoleId::new(n))).is_some_and(|role| !role.managed),
        mass_allowed,
        requested,
    )
}

/// A rendered template, ready to become a `CreateMessage` (or any other builder via its fields).
#[derive(Clone, Debug, Default)]
pub struct RenderedMessage {
    pub content: Option<String>,
    pub embeds: Vec<CreateEmbed>,
    pub allowed_mentions: Option<CreateAllowedMentions>,
    pub files: Vec<CreateAttachment>,
}

impl RenderedMessage {
    pub fn create_message(self) -> CreateMessage {
        let mut message = CreateMessage::new().embeds(self.embeds).add_files(self.files);
        if let Some(content) = self.content {
            message = message.content(content);
        }
        if let Some(allowed) = self.allowed_mentions {
            message = message.allowed_mentions(allowed);
        }
        message
    }

}

/// Builds `CreateAllowedMentions` from Discord's raw `allowed_mentions` JSON.
pub fn allowed_mentions_from_value(value: &Value) -> CreateAllowedMentions {
    let ids = |key: &str| -> Vec<u64> {
        value.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str()?.parse().ok()).filter(|&n| n != 0).collect()).unwrap_or_default()
    };
    let mut allowed = CreateAllowedMentions::new()
        .users(ids("users").into_iter().map(UserId::new))
        .roles(ids("roles").into_iter().map(RoleId::new));
    for kind in value.get("parse").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        allowed = match kind {
            "users" => allowed.all_users(true),
            "roles" => allowed.all_roles(true),
            "everyone" => allowed.everyone(true),
            _ => allowed,
        };
    }
    if let Some(replied) = value.get("replied_user").and_then(Value::as_bool) {
        allowed = allowed.replied_user(replied);
    }
    allowed
}

pub fn with_delivery_mention(mut message: RenderedMessage, mention: &DeliveryMention) -> RenderedMessage {
    let mut prefix: Vec<String> = Vec::new();
    if mention.everyone {
        prefix.push("@everyone".into());
    }
    if mention.here {
        prefix.push("@here".into());
    }
    prefix.extend(mention.roles.iter().map(|id| format!("<@&{id}>")));
    let prefix = prefix.join(" ");
    if !prefix.is_empty() {
        message.content = Some(match message.content.filter(|c| !c.is_empty()) {
            Some(content) => format!("{prefix}\n{content}"),
            None => prefix,
        });
    }
    let roles = mention.roles.iter().filter_map(|id| id.parse::<u64>().ok()).filter(|&n| n != 0).map(RoleId::new);
    message.allowed_mentions = Some(
        CreateAllowedMentions::new().everyone(mention.everyone || mention.here).roles(roles).users(Vec::<UserId>::new()).replied_user(false),
    );
    message
}

static UNSAFE_FILENAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[\\/:*?"<>|\x00-\x1f]"#).unwrap());

pub fn sanitize_attachment_filename(value: &str) -> anyhow::Result<String> {
    if value.contains('/') || value.contains('\\') || value.contains("..") {
        bail!("Attachment filename is invalid.");
    }
    let normalized: String = unicode_normalization::UnicodeNormalization::nfkc(value).collect();
    let replaced = UNSAFE_FILENAME.replace_all(&normalized, "_");
    let name = replaced.trim_start_matches('.').trim().to_string();
    if name.is_empty() || js_len(&name) > 100 || name == "." || name == ".." {
        bail!("Attachment filename is invalid.");
    }
    Ok(name)
}

const SAFE_MIME: [&str; 6] = ["image/png", "image/jpeg", "image/gif", "image/webp", "application/pdf", "text/plain"];

/// Template embeds without their own colour use Gaia purple.
pub fn with_default_color(embed: &Value) -> Value {
    let mut embed = embed.clone();
    if let Some(obj) = embed.as_object_mut() {
        if obj.get("color").is_none_or(Value::is_null) {
            obj.insert("color".into(), crate::discord::GAIA_PURPLE.into());
        }
    }
    embed
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn attachment_record(template_id: &str, filename: &str, content_type: &str, data: Vec<u8>, actor: &str) -> anyhow::Result<EmbedAttachment> {
    let clean = sanitize_attachment_filename(filename)?;
    if !SAFE_MIME.contains(&content_type) {
        bail!("Attachment type is not allowed. Use PNG, JPEG, GIF, WEBP, PDF, or plain text.");
    }
    if data.is_empty() || data.len() > MAX_ATTACHMENT_BYTES {
        bail!("Attachment must be between 1 byte and {} MB.", MAX_ATTACHMENT_BYTES / 1024 / 1024);
    }
    Ok(EmbedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        template_id: template_id.into(),
        filename: clean,
        content_type: content_type.into(),
        size_bytes: data.len() as i64,
        sha256: sha256_hex(&data),
        data,
        created_by_id: actor.into(),
        created_at: crate::db::now(),
    })
}

pub async fn render_template(repo: &GaiaRepository, template_id: &str, payload_json: &str) -> anyhow::Result<RenderedMessage> {
    let message = validate_embed_payload(payload_json).map_err(|errors| anyhow!("Template is invalid: {}", errors.join(" ")))?;
    let attachments = repo.attachments(template_id)?;
    if attachments.iter().map(|a| a.size_bytes).sum::<i64>() > MAX_TEMPLATE_ATTACHMENT_BYTES as i64 {
        bail!("Template attachment total exceeds the 20 MB limit.");
    }
    let empty = Vec::new();
    let embeds = message.get("embeds").and_then(Value::as_array).unwrap_or(&empty);
    let mut referenced = Vec::new();
    for embed in embeds {
        for key in ["image", "thumbnail", "video"] {
            if let Some(name) = embed.get(key).and_then(|v| v.get("url")).and_then(Value::as_str).and_then(|u| u.strip_prefix("attachment://")) {
                if !referenced.contains(&name) {
                    referenced.push(name);
                }
            }
        }
    }
    let mut files = Vec::new();
    for attachment in &attachments {
        if sha256_hex(&attachment.data) != attachment.sha256 || attachment.data.len() as i64 != attachment.size_bytes {
            bail!("Attachment {} failed its integrity check.", attachment.filename);
        }
        files.push(CreateAttachment::bytes(attachment.data.clone(), attachment.filename.clone()).description(attachment.filename.clone()));
    }
    let names: HashSet<&str> = attachments.iter().map(|a| a.filename.as_str()).collect();
    if let Some(missing) = referenced.iter().find(|name| !names.contains(*name)) {
        bail!("Template references missing attachment {missing}.");
    }
    Ok(RenderedMessage {
        content: message.get("content").and_then(Value::as_str).map(String::from),
        embeds: embeds.iter().map(|e| Ok(CreateEmbed::from(serde_json::from_value::<Embed>(with_default_color(e))?))).collect::<anyhow::Result<_>>()?,
        allowed_mentions: message.get("allowed_mentions").filter(|v| !v.is_null()).map(allowed_mentions_from_value),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embeds_default_to_gaia_purple() {
        assert_eq!(with_default_color(&serde_json::json!({"title": "a"}))["color"], 0x9b59b6);
        assert_eq!(with_default_color(&serde_json::json!({"color": null}))["color"], 0x9b59b6);
        assert_eq!(with_default_color(&serde_json::json!({"color": 255}))["color"], 255);
    }

    fn repo() -> GaiaRepository {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        GaiaRepository::new(conn)
    }

    #[tokio::test]
    async fn stores_attachment_bytes_and_resolves_attachment_urls() {
        let r = repo();
        r.save_embed("Image", r#"{"embeds":[{"image":{"url":"attachment://card.png"}}]}"#, "admin", None).unwrap();
        let template = r.embed("Image").unwrap().unwrap();
        let record = attachment_record(&template.template_id, "card.png", "image/png", b"image".to_vec(), "admin").unwrap();
        r.save_attachment(&record).unwrap();
        let rendered = render_template(&r, &template.template_id, &template.payload_json).await.unwrap();
        assert_eq!(rendered.files.len(), 1);
        assert_eq!(serde_json::to_value(&rendered.embeds[0]).unwrap()["image"]["url"], "attachment://card.png");
    }

    #[tokio::test]
    async fn rejects_unsafe_attachment_names_and_missing_references() {
        assert!(sanitize_attachment_filename("../secret.png").unwrap_err().to_string().contains("invalid"));
        let r = repo();
        r.save_embed("Broken", r#"{"embeds":[{"image":{"url":"attachment://missing.png"}}]}"#, "admin", None).unwrap();
        let template = r.embed("Broken").unwrap().unwrap();
        let error = render_template(&r, &template.template_id, &template.payload_json).await.unwrap_err();
        assert!(error.to_string().contains("missing attachment"));
    }

    #[test]
    fn uses_explicit_allowed_mentions_and_never_parses_template_text() {
        let base = || RenderedMessage { content: Some("@everyone ordinary text".into()), ..Default::default() };
        let plain = with_delivery_mention(base(), &no_mention());
        assert_eq!(plain.content.as_deref(), Some("@everyone ordinary text"));
        let allowed = serde_json::to_value(plain.allowed_mentions.unwrap()).unwrap();
        assert_eq!(allowed["parse"], json!([]));
        assert_eq!(allowed["roles"], json!([]));

        let roles = with_delivery_mention(base(), &DeliveryMention { roles: vec!["111".into(), "222".into()], everyone: false, here: false });
        assert_eq!(roles.content.as_deref(), Some("<@&111> <@&222>\n@everyone ordinary text"));
        let allowed = serde_json::to_value(roles.allowed_mentions.unwrap()).unwrap();
        assert_eq!(allowed["parse"], json!([]));
        assert_eq!(allowed["roles"], json!(["111", "222"]));

        let everyone = with_delivery_mention(base(), &DeliveryMention { roles: vec![], everyone: true, here: false });
        assert_eq!(serde_json::to_value(everyone.allowed_mentions.unwrap()).unwrap()["parse"], json!(["everyone"]));
    }

    #[test]
    fn drops_deleted_or_unpermitted_scheduled_mentions() {
        let resolved = resolve_mention_with(|id| id == "live", false, &DeliveryMention { roles: vec!["live".into(), "missing".into(), "managed".into()], everyone: true, here: false });
        assert_eq!(resolved.mention, DeliveryMention { roles: vec!["live".into()], everyone: false, here: false });
        assert!(resolved.warning.is_some());
    }

    #[test]
    fn parses_and_labels_mentions() {
        assert_eq!(parse_mention(Some(r#"{"roles":["1",2],"everyone":1}"#)), DeliveryMention { roles: vec!["1".into()], everyone: true, here: false });
        assert_eq!(parse_mention(None), no_mention());
        assert_eq!(mention_label(&no_mention()), "None");
        assert_eq!(mention_label(&DeliveryMention { roles: vec!["1".into()], everyone: false, here: true }), "<@&1>, @here");
    }
}
