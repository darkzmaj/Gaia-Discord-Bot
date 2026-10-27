use crate::db::repositories::GaiaRepository;
use crate::discord::Discord;
use serenity::model::guild::Member;
use serenity::model::Permissions;


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionLevel {
    Global,
    Staff,
    Administrator,
}

/// Gaia Administrator means a current effective Discord Administrator permission, never guild ownership.
pub fn is_discord_administrator(permissions: Permissions) -> bool {
    permissions.administrator()
}

/// Effective guild permissions of a member: the interaction-provided value when present,
/// otherwise computed from the cached guild's roles (empty if the guild is not cached).
/// Like discord.js `member.permissions`, the guild owner resolves to all permissions.
pub fn member_permissions(discord: &Discord, member: &Member) -> Permissions {
    if let Some(permissions) = member.permissions {
        return permissions;
    }
    discord.cache.guild(member.guild_id).map(|guild| guild.member_permissions(member)).unwrap_or_else(Permissions::empty)
}

pub fn is_administrator(discord: &Discord, member: &Member) -> bool {
    is_discord_administrator(member_permissions(discord, member))
}

/// Core check for interaction handlers (`interaction.member.permissions`).
pub async fn has_permission_with(repo: &GaiaRepository, guild_id: &str, user_id: &str, permissions: Permissions, level: PermissionLevel) -> anyhow::Result<bool> {
    if level == PermissionLevel::Global || is_discord_administrator(permissions) {
        return Ok(true);
    }
    Ok(level == PermissionLevel::Staff && repo.is_staff(guild_id, user_id)?)
}

pub async fn has_permission(repo: &GaiaRepository, discord: &Discord, member: &Member, level: PermissionLevel) -> anyhow::Result<bool> {
    let permissions = member_permissions(discord, member);
    has_permission_with(repo, &member.guild_id.to_string(), &member.user.id.to_string(), permissions, level).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> GaiaRepository {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        GaiaRepository::new(conn)
    }

    #[tokio::test]
    async fn treats_discord_administrators_as_staff_without_a_staff_record() {
        let r = repo();
        assert!(has_permission_with(&r, "a", "admin", Permissions::all(), PermissionLevel::Staff).await.unwrap());
        assert!(!has_permission_with(&r, "a", "ordinary", Permissions::empty(), PermissionLevel::Staff).await.unwrap());
    }

    #[tokio::test]
    async fn defines_administrator_by_effective_permission_not_ownership() {
        let r = repo();
        let admin = Permissions::ADMINISTRATOR;
        let none = Permissions::empty();
        for level in [PermissionLevel::Administrator, PermissionLevel::Staff] {
            assert!(has_permission_with(&r, "a", "management", admin, level).await.unwrap());
        }
        assert!(is_discord_administrator(admin));
        assert!(!has_permission_with(&r, "a", "owner", none, PermissionLevel::Administrator).await.unwrap());
        r.add_staff("staff", "gaia-staff", "owner").unwrap();
        assert!(!has_permission_with(&r, "a", "gaia-staff", none, PermissionLevel::Administrator).await.unwrap());
        assert!(!has_permission_with(&r, "a", "gaia-staff", none, PermissionLevel::Staff).await.unwrap());
        r.add_staff("a", "gaia-staff", "owner").unwrap();
        assert!(has_permission_with(&r, "a", "gaia-staff", none, PermissionLevel::Staff).await.unwrap());
        assert!(has_permission_with(&r, "a", "anyone", none, PermissionLevel::Global).await.unwrap());
    }
}
