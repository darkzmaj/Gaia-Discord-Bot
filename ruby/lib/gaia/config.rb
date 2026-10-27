# frozen_string_literal: true

module Gaia
  # Same environment contract as src/config.rs.
  module Config
    module_function

    # Minimal .env loader (KEY=value lines); real environment variables win.
    def load_dotenv(path = File.join(ROOT, '.env'))
      return unless File.file?(path)

      File.foreach(path) do |line|
        next if line.strip.empty? || line.lstrip.start_with?('#')

        key, value = line.chomp.split('=', 2)
        next unless value

        ENV[key.strip] ||= value.strip.delete_prefix('"').delete_suffix('"')
      end
    end

    def token = ENV.fetch('DISCORD_TOKEN', '')
    def database_path = ENV.fetch('DATABASE_PATH', './data/gaia.sqlite')
    def application_id = ENV['APPLICATION_ID'] || ENV['CLIENT_ID'] || ENV['DISCORD_CLIENT_ID']
    def diagnostic_guild_id = ENV['DIAGNOSTIC_GUILD_ID']&.then { _1.strip.empty? ? nil : _1.strip }
    def diagnostic_user_id = ENV['DIAGNOSTIC_USER_ID']&.then { _1.strip.empty? ? nil : _1.strip }

    def require!
      raise 'DISCORD_TOKEN is required. See .env.example.' if token.strip.empty?
      raise 'DATABASE_PATH cannot be empty. See .env.example.' if database_path.strip.empty?
    end
  end
end
