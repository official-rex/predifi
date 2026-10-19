# Contract versioning and compatibility policy

This policy applies to the PrediFi Soroban contract release bundle: the
`predifi-contract` and `access-control` WASM artifacts and their shared
`predifi-errors` dependency. It defines release requirements, not new on-chain
enforcement. Use it with the [upgrade procedure](contract-upgrade-procedure.md).

## Release identity

Use [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html) and immutable
Git tags named `contracts-vMAJOR.MINOR.PATCH`, optionally with a prerelease suffix
such as `contracts-v0.2.0-rc.1`. Both deployable artifacts belong to the same bundle
version even when only one changes. Backend/frontend versions remain independent.
Do not move a published tag or replace its assets. A rebuilt artifact with different
bytes needs a new release; build metadata is not a substitute for a version bump.

For versions at or above 1.0.0, classify the public contract surface as follows:

| Change | Required bump |
| --- | --- |
| Compatible correction preserving documented calls, state and event interpretation | PATCH |
| Compatible addition, such as a new entry point, or announced deprecation | MINOR |
| Removed/changed call, return encoding, event topic/payload, error code meaning, authorization requirement, or incompatible settlement behavior | MAJOR |
| Storage change requiring migration or making previously supported state unreadable | MAJOR |

Evaluate financial semantics too: payout rounding, fee treatment, deadlines and
oracle assumptions can break clients without changing a function signature.
During `0.y.z`, a breaking change requires a new MINOR and explicit migration notes;
PATCH remains compatible within that minor. Do not infer compatibility between
different `0.y` lines. Prereleases are opt-in and supported only at the exact
tested version, never implicitly included in stable client support ranges.

## Separate the version numbers

| Identifier | Meaning and source of truth |
| --- | --- |
| Bundle SemVer | Published tag and release manifest; identifies reviewed behavior |
| WASM SHA-256 | Exact final uploaded bytes for one artifact; never a ZIP digest or pre-optimization hash |
| Storage schema version | Positive integer per contract in the manifest; increment whenever persisted layout/encoding or interpretation changes |
| Manifest format version | Structure of the release metadata; unrelated to contract storage |
| `get_version()` | Per-instance upgrade counter; not SemVer or a schema version |
| `get_version_string()` | Currently hard-coded `0_0_0`; not sufficient release identification |

At this policy's introduction, the Cargo packages are `0.0.0`, and the
[admin implementation](../contract/contracts/predifi-contract/src/admin.rs)
initializes the counter from `CONTRACT_VERSION` and increments it on each upgrade,
including a rollback. `get_contract_info().version_string` is also hard-coded.
There is no dedicated on-chain storage-schema getter. Do not parse a counter into
SemVer or assume the crate version proves which WASM a deployment uses.

Before a real release, align the participating crate versions and reported version
strings with the chosen release and document any prerelease representation limit.
The manifest and observed WASM hash remain authoritative. This documentation change
does not retroactively assign versions to historical deployments or update getters.

## Storage and upgrade compatibility

Each artifact declares its output `storage_schema_version`, the schema versions it
can read without migration, and a migration plan (or explicit `null`). Assign the
first documented schema baseline only after inspecting the stored types and state;
an unknown legacy schema is not automatically schema 1. This is contract storage,
not the backend SQL migration sequence or fee-schedule counter.

For every supported upgrade, test old state under the new code, including open and
resolved pools, claims/refunds, roles, fees, and paused state. Changing a Rust struct
or enum is not proof of storage compatibility. Even a comment claiming automatic
XDR compatibility must be supported by tests against representative prior state.
An additive schema change can retain MINOR compatibility only if existing state
remains readable and supported operations need no migration; still increment the
schema version and declare both read directions explicitly.

If migration is required, release notes must specify source/target schemas,
preconditions, authorization, pause/unpause ordering, data transformation,
invariants, completion evidence, and failure/recovery steps. `migrate_state` currently
checks that the contract is not paused and contains no data rewrite. Do not assume
calling it performs a required migration. Resolve this constraint in the release's
tested procedure before approving an upgrade.

Rollback is another upgrade to an older WASM hash; it does not revert storage or
decrease the upgrade counter. Declare whether the older artifact can read the new
state. If not, require a tested compensating migration or forward fix, not a blind
rollback. The access-control contract has no upgrade entry point: a changed release
requires a new deployment and a reviewed integration/role handover, not an in-place
WASM replacement. A release tag by itself does not authorize any deployment.

## Supported clients

Every release includes a compatibility matrix for frontend, backend/indexer, and
external SDK/bindings (where one exists). Identify a client by repository/package
and exact tested commit or package version, plus an explicit supported contract
range. Include required capabilities, event/error decoding expectations, and test
evidence. Use `not supported` or `not tested` instead of an empty or guessed range.
Do not advertise unpublished SDK versions. Removing promised client support is a
breaking release change under the version rules above.

Integrators must match the network passphrase, contract address, observed WASM hash,
and schema to the manifest before enabling writes. An address is not a version:
the same PrediFi address can execute different WASM after upgrade. Read the latest
confirmed deployment record, not just the initial deployment transaction. Reject
unknown hashes/schemas and unsupported ranges; ask for an explicitly tested update
rather than silently treating a newer version as compatible. Stable compatibility
claims require regression tests using the oldest supported client in each range.

## Release manifest and deployment records

Publish `release.json`, final WASM assets, checksums, changelog, and test/build
evidence together with each tag. Required manifest information:

- `format_version`, bundle `version`, source repository, immutable tag, and full
  resolved commit SHA. Record the lockfile digest and exact Rust, Stellar CLI,
  target, Soroban SDK and optimizer versions, commands and flags used to build.
- One entry per artifact: stable artifact ID, crate, final filename, SHA-256,
  storage schema version, readable schemas, migration and rollback constraints.
  Record the uploaded WASM hash and check it equals the final artifact digest.
- Client compatibility matrix and links to dated test results and approvals.
- Append-only deployment records linking the release/tag/commit and artifact ID
  to the network passphrase (not merely an alias), contract address, confirmed
  transaction hash, ledger, timestamp, observed WASM hash and storage schema.
  Include initialization parameters, access-control linkage, and pre/post upgrade
  counter when available. No private keys or signing secrets belong in metadata.

Record uploads separately from successful deployments: uploading WASM does not
establish a deployed address. Promote the same verified bytes between networks;
each deployment has its own record. Correct a bad deployment record with a new
record referencing it, preserving the original evidence. Record upgrades and
rollbacks as new entries even when the contract address is unchanged.

The existing [deploy script](../contract/scripts/deploy.sh) writes addresses and
initialization details to `deployed_contracts_<network>.json`, but omits source,
artifact digest, schema and confirmed transaction evidence. Preserve that output
and supplement it with the release record; it alone does not satisfy this policy.
Build scripts use different output names/optimization steps, so hash the exact file
actually uploaded after the final optimization, not a guessed filename.

## Changelog and release checklist

Keep the bundle [changelog](../contract/CHANGELOG.md) and the
[PrediFi crate changelog](../contract/contracts/predifi-contract/CHANGELOG.md)
consistent. Each dated release entry must link its tag and comparison, and cover
ABI, errors, events, behavior/security, storage/migrations, supported clients,
deprecations/removals, deployment artifacts, and rollback limits. Explicitly state
`unchanged` where relevant. The bundle entry also covers access-control changes.
Do not treat old links to other repository owners as proof of a current deployment.

The release author and a contract reviewer complete this checklist before promotion:

1. Review the public-interface and storage diff; choose the version bump and
   supported client matrix. Document breaking changes and migration directions.
2. Run contract tests, formatting and Clippy from the
   [contribution guide](../contract/CONTRIBUTING.md), plus old-state and supported
   client compatibility tests. Record actual results and unresolved failures.
3. Build from the tagged clean commit with the locked dependencies. Record the
   actual target/toolchain and optimization steps; reconcile any conflicting setup
   instructions. Rebuild independently and compare final digests before release.
4. Check that tag, commit, package/reported versions, changelog and manifest agree.
   Hash each final WASM, upload it to the test network, and compare the upload hash.
5. Test deployment/upgrade and required migration on the test network. Verify the
   observed address/hash, initialization, counters, schema, events and client flows.
   Exercise rollback only where the declared storage compatibility permits it.
6. Record confirmed transaction/ledger evidence and reviewer sign-off. Any missing
   evidence, unknown schema, unsupported client or digest mismatch blocks promotion.
7. Publish immutable release assets and deployment records. Production deployment
   requires separate authorization; append its verified network-specific record.

## Test release walkthrough

[contract-release.example.json](releases/contract-release.example.json) is an
**offline synthetic test release**, not a deployed PrediFi release. Its source
tag/commit, client identities, addresses and transaction records are fixtures.
The tiny WASM byte strings only exercise digest mapping; they have no Soroban
contract implementation and must not be uploaded. No actual release tag is created.

Run from the repository root with Python 3:

```sh
python docs/releases/validate_example.py
```

The walkthrough checks the tag-to-version link, independently hashes both fixture
artifacts, and resolves every deployment to exactly one artifact, schema and source
revision. It then demonstrates rejection of an altered digest, unknown artifact,
wrong schema, wrong source revision and unsupported client version. Passing this
offline check establishes example consistency only; checklist steps 2, 3 and 5
still require real build, migration, client and network evidence for a real release.
