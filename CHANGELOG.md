# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Each sprint closing task appends its entry below. Per-sprint entries describe
user-visible changes (CLI behavior, exit codes, wire format, on-disk schema).
Internal refactors that do not change observable behavior are omitted.

## [Unreleased]

### Added

### Changed

### Deprecated

### Removed

### Fixed

### Security

<!--
Sprint 1 (point release) appends here on close. User-visible wins:
`--resume`, `--allow-dirty`, `gw stop` session-name fix.

Sprint 2 (minor version bump) carries the breaking changes:
- Daemon-down `gw ps/logs/queue/schedule` now exit 1 (was 0) — AC12 / T18.
- `Request::SubmitRun` wire format extended (inline fields → `RunSpec`) —
  one-release deprecation window via `#[serde(default)]`; bare-field removal
  scheduled for Sprint 3.

When promoting [Unreleased] to a versioned release, replace this section
with `## [X.Y.Z] - YYYY-MM-DD` and start a fresh [Unreleased] above it.
-->

## [0.1.0]

Initial development version. See git history prior to CHANGELOG creation
for change provenance.

[Unreleased]: https://github.com/vinhnxv/greater-will/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/vinhnxv/greater-will/releases/tag/v0.1.0
