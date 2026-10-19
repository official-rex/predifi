# Changelog

All notable changes to the `predifi-contract` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### ABI

- Added contract-level ceilings for pool outcomes (`MAX_OUTCOMES_PER_POOL`),
  distinct predictors per pool (`MAX_PREDICTIONS_PER_POOL`), and aggregate
  stake (`MAX_AGGREGATE_STAKE_PER_POOL`).

### Error Codes

- Added `MaxOutcomesExceeded` (114), `MaxPredictionsPerPoolExceeded` (115),
  and `MaxAggregateStakeExceeded` (116).

### Events

### Internal

- Enforced all three resource limits before any prediction or pool state is
  written, with overflow-safe aggregate stake accounting.
- Added boundary tests at maximum, maximum-minus-one, and maximum-plus-one,
  plus resource benchmarks covering CPU, memory, and persistent storage.

## [0.0.0] - 2026-09-17

### ABI

### Error Codes

### Events

### Internal
- Initial contract deployment with prediction pool lifecycle, oracle resolution, and token whitelist support.

[Unreleased]: https://github.com/Web3Novalabs/predifi/compare/v0.0.0...HEAD
[0.0.0]: https://github.com/Web3Novalabs/predifi/releases/tag/v0.0.0
