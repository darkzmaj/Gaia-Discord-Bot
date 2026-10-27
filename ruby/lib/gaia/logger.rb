# frozen_string_literal: true

require 'json'
require 'time'

module Gaia
  # JSON log lines on stdout, matching src/logger.rs (secrets redacted).
  module Logger
    WEBHOOK = %r{(?:https?://[^\s/]+)?/api/webhooks/\d{17,20}/[A-Za-z0-9_\-.]+}i
    TOKEN = /[A-Za-z\d_-]{20,}\.[A-Za-z\d_-]{6,}\.[A-Za-z\d_-]{20,}/

    module_function

    def redact(value)
      value.gsub(WEBHOOK, '[redacted webhook]').gsub(TOKEN, '[redacted token]')
    end

    def write(level, message, context = {})
      line = { timestamp: Time.now.utc.iso8601(3), level: level, message: message }
      context.each { |key, value| line[key] = value.is_a?(String) ? redact(value) : value unless value.nil? }
      $stdout.puts JSON.generate(line)
    end

    def info(message, context = {}) = write('info', message, context)
    def warn(message, context = {}) = write('warn', message, context)
    def error(message, context = {}) = write('error', message, context)
  end
end
