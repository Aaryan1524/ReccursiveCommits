# Changelog

All notable user-facing changes are recorded here. The project follows
[Semantic Versioning](https://semver.org/).

## Unreleased

### Added

- Interactive scheduling can split one dirty checkout into multiple independently
  captured and scheduled batches without touching the user's checkout.
- A tag-driven macOS release workflow that verifies, signs, notarizes, checksums,
  and publishes Intel and Apple-silicon command-line archives.
- A reviewed Homebrew-formula renderer for publishing those archives through a
  separate tap.
