# frozen_string_literal: true

# Task runner for Gaia (replaces the old npm scripts). The bot is Rust; tooling is Ruby.
require 'rake/testtask'

RUBY_BIN = File.expand_path('ruby/bin', __dir__)

desc 'Run the bot in development'
task(:dev) { sh 'cargo run' }

desc 'Build the release binary (target/release/gaia)'
task(:build) { sh 'cargo build --release' }

desc 'Run the release binary'
task(start: :build) { sh File.join('target', 'release', Gem.win_platform? ? 'gaia.exe' : 'gaia') }

desc 'Lint Rust (clippy) with warnings as errors'
task(:lint) { sh 'cargo clippy --all-targets -- -D warnings' }

Rake::TestTask.new('test:ruby') do |t|
  t.libs << 'ruby/lib'
  t.test_files = FileList['ruby/test/test_*.rb']
end

desc 'Run Rust unit tests'
task('test:rust') { sh 'cargo test' }

desc 'Run all tests (Rust + Ruby)'
task test: %w[test:rust test:ruby]

desc 'Apply database migrations (the bot also migrates on startup)'
task(:migrate) { sh RbConfig.ruby, File.join(RUBY_BIN, 'gaia-migrate') }

namespace :commands do
  desc 'Bulk-overwrite global commands, verify them, clear legacy guild commands'
  task(:sync) { sh RbConfig.ruby, File.join(RUBY_BIN, 'gaia-commands-sync') }

  desc 'Validate and print the offline command schema (no token needed)'
  task('sync:dry') { sh RbConfig.ruby, File.join(RUBY_BIN, 'gaia-commands-sync'), '--dry-run' }

  desc 'Read-only diagnosis of live commands and permissions'
  task(:inspect) { sh RbConfig.ruby, File.join(RUBY_BIN, 'gaia-commands-inspect') }
end

task default: :test
