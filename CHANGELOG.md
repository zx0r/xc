# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-11-16

### Fixed
- **Nucleo Invariant Contract:** Eliminated upstream matcher assertion panic (`fuzzy_optimal.rs:37:13: should have been caught by prefilter`) via zero-copy `ProcessedQuery` with deterministic Smart-Case resolution.
- **Bitmask False Negatives:** Replaced case-discriminating `b & 63` prefilter with a 64-byte cacheline-aligned 256-byte LUT (`CHAR_TO_BIT_LUT`, `#[repr(align(64))]`), unifying `A..=Z` and `a..=z` into shared 26 slots to prevent silent candidate pruning.

### Optimized
- **Zero-Allocation Hot Path:** Enforced 0 heap allocations on query preprocessing via `[u8; 256]` stack scratch buffer and immutable caller-frame `Utf32Str` borrowing across Rayon threads.
- **L1d Cache Chunk Partitioning:** Reduced Rayon chunk size from 32,768 to 4,096 entries (32 KB working set), guaranteeing 100% L1 Data Cache residency per physical core.
- **Branchless 4-Way ILP Bitmask:** Modernized ASCII presence scanning with `as_chunks::<4>()` unrolling, retiring at ~0.4 cycles per byte.

## [0.1.0] - 2026-10-30

### Added
- Initial release of **xc** &mdash; a minimalist, high-throughput Unix shell history engine.
- Instant, sub-millisecond full-text search with interactive Ratatui TUI picker.
- Zero-allocation memory-mapped storage arena for high-performance command history.
- Seamless Bash, Zsh, and Fish shell hooks for transparent execution time and exit code tracking.
- Cross-platform distribution pipeline powered by `cargo-dist` with Homebrew tap integration.

[0.1.1]: https://github.com/zx0r/xc/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/zx0r/xc/releases/tag/v0.1.0
