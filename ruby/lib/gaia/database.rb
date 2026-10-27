# frozen_string_literal: true

require 'fileutils'
require 'sqlite3'
require 'time'

module Gaia
  # Opens the bot's SQLite database and applies db/migrations/*.sql exactly like src/db/mod.rs.
  module Database
    MIGRATIONS_DIR = File.join(ROOT, 'db', 'migrations')

    module_function

    def migrations
      Dir[File.join(MIGRATIONS_DIR, '*.sql')].sort.map { |path| [File.basename(path, '.sql').to_i, File.read(path)] }
    end

    def open(filename)
      FileUtils.mkdir_p(File.dirname(filename))
      db = SQLite3::Database.new(filename)
      db.results_as_hash = true
      db.execute_batch('PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;')
      migrate(db)
      db
    end

    def migrate(db)
      db.execute('CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)')
      applied = db.execute('SELECT version FROM schema_migrations').map { |row| row['version'] || row[0] }
      migrations.each do |version, sql|
        next if applied.include?(version)

        db.transaction(:immediate) do
          db.execute_batch(sql)
          db.execute('INSERT INTO schema_migrations(version, applied_at) VALUES (?, ?)', [version, Time.now.utc.iso8601(3)])
        end
      end
    end

    # The three repository queries the tooling needs (same SQL as GaiaRepository).
    def command_sync_state(db, application_id)
      db.get_first_row('SELECT * FROM command_sync_state WHERE application_id=?', [application_id])
    end

    def save_command_sync_state(db, application_id, fingerprint, cleanup_complete)
      db.execute(
        'INSERT INTO command_sync_state(application_id,schema_fingerprint,guild_cleanup_complete,updated_at) VALUES (?,?,?,?) ' \
        'ON CONFLICT(application_id) DO UPDATE SET schema_fingerprint=excluded.schema_fingerprint,' \
        'guild_cleanup_complete=excluded.guild_cleanup_complete,updated_at=excluded.updated_at',
        [application_id, fingerprint, cleanup_complete ? 1 : 0, Time.now.utc.iso8601(3)]
      )
    end

    def staff?(db, guild_id, user_id)
      !db.get_first_row('SELECT 1 FROM guild_staff WHERE guild_id = ? AND user_id = ?', [guild_id, user_id]).nil?
    end
  end
end
