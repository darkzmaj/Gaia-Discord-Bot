# frozen_string_literal: true

require 'minitest/autorun'
require 'tmpdir'
require_relative '../lib/gaia'

class TestCommands < Minitest::Test
  def test_schema_is_the_five_guild_commands
    Gaia::Commands.assert_schema!
    assert_equal %w[admin embed mute unmute unverify], Gaia::Commands.names
  end

  # Same value is asserted in src/services/command_sync.rs, so Rust and Ruby agree on sync state.
  def test_fingerprint_matches_rust
    assert_equal '1e98fe5bf84ba2fb43cdc8d2beeb2d3840053d0c263b5295e09d922242a89a32', Gaia::Commands.fingerprint
  end

  def test_admin_with_options_is_rejected
    defs = Gaia::Commands.definitions
    defs.first['options'] = [{ 'type' => 3, 'name' => 'x', 'description' => 'x' }]
    assert_raises(RuntimeError) { Gaia::Commands.assert_schema!(defs) }
  end

  def test_names_mention_gaia_not_cookie
    text = File.read(Gaia::Commands::SCHEMA_PATH, encoding: 'UTF-8')
    refute_match(/cookie/i, text)
    assert_match(/Gaia/, text)
  end
end

class TestDatabase < Minitest::Test
  def test_migrations_apply_once_and_sync_state_round_trips
    Dir.mktmpdir do |dir|
      db = Gaia::Database.open(File.join(dir, 'nested', 'gaia.sqlite'))
      Gaia::Database.migrate(db)
      assert_equal Gaia::Database.migrations.size, db.get_first_value('SELECT COUNT(*) FROM schema_migrations')
      assert_nil Gaia::Database.command_sync_state(db, 'app')
      Gaia::Database.save_command_sync_state(db, 'app', 'abc', false)
      Gaia::Database.save_command_sync_state(db, 'app', 'def', true)
      state = Gaia::Database.command_sync_state(db, 'app')
      assert_equal ['def', 1], [state['schema_fingerprint'], state['guild_cleanup_complete']]
      refute Gaia::Database.staff?(db, 'g', 'u')
      db.close
    end
  end
end

class TestPermissions < Minitest::Test
  GUILD = { 'id' => 'g', 'owner_id' => 'owner',
            'roles' => [{ 'id' => 'g', 'permissions' => (1 << 31).to_s }, { 'id' => 'admin', 'permissions' => '8' }] }.freeze

  def test_effective_permissions
    member = ->(id, roles) { { 'user' => { 'id' => id }, 'roles' => roles } }
    assert_equal 1 << 31, Gaia::CommandSync.effective_permissions(GUILD, member.call('u', []))
    assert_equal Gaia::CommandSync::ALL_PERMISSIONS, Gaia::CommandSync.effective_permissions(GUILD, member.call('u', ['admin']))
    assert_equal Gaia::CommandSync::ALL_PERMISSIONS, Gaia::CommandSync.effective_permissions(GUILD, member.call('owner', []))
  end
end

class TestLogger < Minitest::Test
  def test_redacts_plain_and_versioned_webhook_urls_and_tokens
    plain = 'https://discord.com/api/webhooks/123456789012345678/abc-DEF_1'
    versioned = 'https://discord.com/api/v10/webhooks/123456789012345678/secret-Token_9'
    token = 'MTIzNDU2Nzg5MDEyMzQ1Njc4OTA.GhIjKl.abcdefghijklmnopqrstuvwxyz12'
    assert_equal '[redacted webhook] [redacted webhook] [redacted token]',
                 Gaia::Logger.redact("#{plain} #{versioned} #{token}")
  end
end
