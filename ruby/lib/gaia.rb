# frozen_string_literal: true

# Gaia operations tooling: migrations, command synchronization and diagnostics.
# The bot itself runs in Rust (../src); both sides share db/migrations and shared/commands.json.
module Gaia
  ROOT = File.expand_path('../..', __dir__)
end

require_relative 'gaia/config'
require_relative 'gaia/logger'
require_relative 'gaia/database'
require_relative 'gaia/commands'
require_relative 'gaia/discord_rest'
require_relative 'gaia/command_sync'
