# frozen_string_literal: true

module Gaia
  # Ruby port of CommandSynchronizer's operator paths (`commands:sync`, `commands:inspect`).
  # The bot runs the same synchronization on ready in Rust.
  class CommandSync
    ADMINISTRATOR = 1 << 3
    USE_APPLICATION_COMMANDS = 1 << 31
    ALL_PERMISSIONS = (1 << 53) - 1

    def initialize(rest, db, configured_application_id = nil)
      @rest = rest
      @db = db
      @configured_application_id = configured_application_id
    end

    def synchronize(force_cleanup: false)
      Commands.assert_schema!
      defs = Commands.definitions
      app_id = application_id
      fingerprint = Commands.fingerprint(defs)
      Logger.info('[commands] command schema loaded', applicationId: app_id, schema: fingerprint[0, 12], scope: 'global')
      Logger.info('[commands] synchronizing global application commands', count: defs.size)
      @rest.put("/applications/#{app_id}/commands", defs)
      verify_live_global_commands(app_id, defs)
      Logger.info('[commands] global commands synchronized', count: defs.size)
      state = Database.command_sync_state(@db, app_id)
      cleanup_required = force_cleanup || state.nil? || state['schema_fingerprint'] != fingerprint ||
                         state['guild_cleanup_complete'].to_i.zero?
      return { application_id: app_id, fingerprint: fingerprint, cleaned: 0, failed: 0 } unless cleanup_required

      outcome = clear_legacy_guild_commands(app_id, @rest.guilds)
      Database.save_command_sync_state(@db, app_id, fingerprint, outcome[:failed].zero?)
      { application_id: app_id, fingerprint: fingerprint, **outcome }
    end

    def inspect_live(diagnostic_guild_id = nil, diagnostic_user_id = nil)
      app_id = application_id
      bot = @rest.get('/users/@me')
      global = live_commands(@rest.get("/applications/#{app_id}/commands"), nil)
      guilds = @rest.guilds.map do |guild|
        report = { id: guild['id'], name: guild['name'] || guild['id'],
                   commands: live_commands(@rest.get("/applications/#{app_id}/guilds/#{guild['id']}/commands"), guild['id']),
                   permissionOverrides: [] }
        begin
          report[:permissionOverrides] = @rest.get("/applications/#{app_id}/guilds/#{guild['id']}/commands/permissions")
        rescue StandardError => e
          report[:permissionOverrideError] = e.message.tr("\r\n", '  ')[0, 240]
        end
        report[:member] = member_report(guild['id'], diagnostic_user_id) if diagnostic_guild_id == guild['id'] && diagnostic_user_id
        report
      end
      { application: { id: app_id, botId: bot['id'], bot: bot['username'] },
        globalCommands: global, guilds: guilds,
        comparisons: [compare_live(global, 'admin', 'unverify'), compare_live(global, 'embed', 'unverify')] }
    end

    def application_id
      app_id = @rest.get('/oauth2/applications/@me')['id']
      raise 'Discord application identity is unavailable.' unless app_id
      if @configured_application_id && !@configured_application_id.empty? && @configured_application_id != app_id
        raise 'Configured application ID does not match authenticated Gaia application.'
      end

      app_id
    end

    private

    def clear_legacy_guild_commands(app_id, guilds)
      Logger.info('[commands] clearing legacy guild commands', guilds: guilds.size)
      cleaned = failed = 0
      guilds.each do |guild|
        existing = @rest.get("/applications/#{app_id}/guilds/#{guild['id']}/commands")
        unless existing.empty?
          @rest.put("/applications/#{app_id}/guilds/#{guild['id']}/commands", [])
          cleaned += 1
        end
      rescue StandardError
        failed += 1
        Logger.warn('[commands] guild command cleanup failed', guildId: guild['id'])
      end
      Logger.info('[commands] guild command cleanup complete', cleaned: cleaned, failed: failed)
      { cleaned: cleaned, failed: failed }
    end

    def verify_live_global_commands(app_id, defs)
      live = live_commands(@rest.get("/applications/#{app_id}/commands"), nil)
      if live.size != defs.size || live.any? { |cmd| defs.none? { _1['name'] == cmd[:name] } }
        raise 'Live global command set does not match Gaia command schema.'
      end

      defs.each do |expected|
        got = live.find { _1[:name] == expected['name'] }
        same = got[:defaultMemberPermissions] == expected['default_member_permissions'] &&
               Commands.normalize_options(got[:options]) == Commands.normalize_options(expected['options']) &&
               got[:contexts] == expected['contexts'] && got[:integrationTypes] == expected['integration_types']
        raise "Live global command metadata does not match Gaia schema for /#{expected['name']}." unless same
      end
    end

    def live_commands(list, guild_id)
      (list || []).map do |cmd|
        { name: cmd['name'], id: cmd['id'], applicationId: cmd['application_id'], guildId: cmd['guild_id'] || guild_id,
          defaultMemberPermissions: cmd['default_member_permissions'], dmPermission: cmd['dm_permission'],
          contexts: cmd['contexts'], integrationTypes: cmd['integration_types'], nsfw: cmd['nsfw'] ? true : false,
          options: cmd['options'] || [] }
      end
    end

    def compare_live(commands, left, right)
      a = commands.find { _1[:name] == left }
      b = commands.find { _1[:name] == right }
      return { left: left, right: right, differences: ['One or both commands are missing from the live global command set.'] } unless a && b

      fields = %i[defaultMemberPermissions dmPermission contexts integrationTypes nsfw]
      { left: left, right: right,
        differences: fields.reject { a[_1] == b[_1] }.map { "#{_1}: #{JSON.generate(a[_1])} != #{JSON.generate(b[_1])}" } }
    end

    def member_report(guild_id, user_id)
      guild = @rest.get("/guilds/#{guild_id}")
      member = @rest.get("/guilds/#{guild_id}/members/#{user_id}")
      perms = self.class.effective_permissions(guild, member)
      admin = perms & ADMINISTRATOR != 0
      { id: user_id, guildOwner: guild['owner_id'] == user_id, administrator: admin,
        useApplicationCommands: perms & USE_APPLICATION_COMMANDS != 0, effectivePermissions: perms.to_s,
        gaiaStaff: Database.staff?(@db, guild_id, user_id), gaiaAuthorization: admin ? 'ADMIN' : 'NONE' }
    rescue DiscordRest::Error
      { id: user_id, found: false }
    end

    # Guild-level permissions: owner/Administrator get everything, else @everyone | member roles.
    def self.effective_permissions(guild, member)
      return ALL_PERMISSIONS if guild['owner_id'] == member.dig('user', 'id')

      roles = guild['roles'].to_h { [_1['id'], _1['permissions'].to_i] }
      perms = roles.fetch(guild['id'], 0)
      member['roles'].each { perms |= roles.fetch(_1, 0) }
      perms & ADMINISTRATOR != 0 ? ALL_PERMISSIONS : perms
    end
  end
end
