# Contributing to Gaia

Thanks for helping. Bug reports, fixes and focused features are welcome.

1. Open an issue first for anything larger than a small fix, so we can agree on the approach.
2. Install Rust 1.85+ and Ruby 3.1+ with Bundler, then run `bundle install`.
3. Before opening a pull request, run:
   ```bash
   cargo clippy --all-targets -- -D warnings
   cargo test
   bundle exec rake test:ruby
   ```
4. Keep pull requests small and describe what changed and how you tested it.
5. If you change `shared/commands.json`, update the fingerprint asserted in both
   `src/services/command_sync.rs` and `ruby/test/test_gaia.rb`.

Security issues: follow [SECURITY.md](SECURITY.md) instead of opening an issue.

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).
