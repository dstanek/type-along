# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/dstanek/type-along/releases/tag/v0.1.0) - 2026-09-15

### Added

- add --version flag
- add accuracy and WPM to the exit report
- print keys pressed and mistakes report on exit
- initial release of type-along

### Other

- let cargo-dist own the GitHub release
- fix lints reported by newer clippy
- add install and releasing sections to README
- add cargo-dist release builds for linux, macos, and windows
- add release-plz for conventional-commit version bumps
- use syntect's pure-Rust regex backend
- add GitHub Actions workflow for fmt, clippy, and test
