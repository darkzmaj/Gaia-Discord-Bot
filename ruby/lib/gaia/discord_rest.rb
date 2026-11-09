# frozen_string_literal: true

require 'json'
require 'net/http'
require 'uri'

module Gaia
  # Minimal Discord REST v10 client with 429 retry; no gateway connection is needed for the tooling.
  class DiscordRest
    BASE = 'https://discord.com/api/v10'

    class Error < StandardError
      attr_reader :status, :code

      def initialize(status, code, message)
        @status = status
        @code = code
        super(message)
      end
    end

    def initialize(token)
      @token = token
    end

    def get(path) = request(Net::HTTP::Get, path)
    def put(path, body) = request(Net::HTTP::Put, path, body)

    # All guilds the bot is in (paginated by snowflake).
    def guilds
      all = []
      after = '0'
      loop do
        page = get("/users/@me/guilds?limit=200&after=#{after}")
        all.concat(page)
        break if page.size < 200

        after = page.last['id']
      end
      all
    end

    private

    def request(klass, path, body = nil, attempt = 0)
      uri = URI("#{BASE}#{path}")
      req = klass.new(uri)
      req['Authorization'] = "Bot #{@token}"
      req['User-Agent'] = 'DiscordBot (https://github.com/darkzmaj/Gaia-Discord-Bot, 2.0.0) Gaia'
      if body
        req['Content-Type'] = 'application/json'
        req.body = JSON.generate(body)
      end
      res = Net::HTTP.start(uri.host, uri.port, use_ssl: true) { _1.request(req) }
      if res.code == '429' && attempt < 5
        sleep(JSON.parse(res.body)['retry_after'].to_f.clamp(0.5, 60))
        return request(klass, path, body, attempt + 1)
      end
      data = res.body.to_s.empty? ? nil : JSON.parse(res.body)
      return data if res.is_a?(Net::HTTPSuccess)

      code = data.is_a?(Hash) ? data['code'] : nil
      message = data.is_a?(Hash) ? data['message'] : res.message
      raise Error.new(res.code.to_i, code, "#{klass::METHOD} #{path} failed: #{res.code} #{message}")
    end
  end
end
