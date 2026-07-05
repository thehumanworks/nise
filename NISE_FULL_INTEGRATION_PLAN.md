# Nise full integration plan

Date: 2026-07-03

Scope: turn the `nise` fork into a Nix-like environment system with `develop`, strict derivation locks, an immutable store, profiles, and safe garbage collection.

This is a design and integration plan. It is intentionally concrete enough to be implemented in phases and reviewed against current mise code.

## Non-negotiable outcomes

1. `nise develop` enters a deterministic project environment from a locked toolset and a rooted profile.
2. The store separates immutable payloads from mutable references.
3. Store objects are never deleted unless no authoritative root can reach them.
4. Backends are immutable-store compatible only after explicit capability and relocation tests.
5. Strict mode fails closed when it cannot prove lock, store, provenance, or sandbox guarantees.
6. Legacy mise behavior keeps working until a backend or user explicitly opts into stricter nise behavior.

## Current code anchors

Command and env:

- Commands are registered in `src/cli/mod.rs:206` and dispatched in `src/cli/mod.rs:281`.
- `mise en` is the nearest shell command, but it delegates to `Exec` with no purity or sandbox policy in `src/cli/en.rs:25`.
- `Exec` defines sandbox/env flags in `src/cli/exec.rs:31` and builds the toolset/env in `src/cli/exec.rs:112` and `src/cli/exec.rs:167`.
- `Exec` writes `MISE_ENV`, env-cache metadata, and `__MISE_DIFF` in `src/cli/exec.rs:183`.
- `Toolset::env_with_path` and final env construction are in `src/toolset/toolset_env.rs:69` and `src/toolset/toolset_env.rs:375`.
- PATH construction is in `src/path_env.rs:53` and stale install path filtering is in `src/path_env.rs:100`.

Install and store-adjacent behavior:

- `BackendArg` owns `cache_path`, `installs_path`, and `downloads_path` in `src/cli/args/backend_arg.rs:39`.
- `ToolVersion::install_path()` resolves to `<installs_path>/<tv_pathname>` and may fall back to shared installs in `src/toolset/tool_version.rs:169`.
- `ToolVersion::runtime_path()` may return a fuzzy runtime symlink unless locked in `src/toolset/tool_version.rs:219`.
- `Backend::install_version()` is the central wrapper around backend installs in `src/backend/mod.rs:2065`.
- Install locking currently locks `tv.install_path()` in `src/backend/mod.rs:2147`.
- Backend-specific installation is `install_version_` in `src/backend/mod.rs:2346`.
- Default uninstall removes `tv.install_path()`, download path, and cache path in `src/backend/mod.rs:2347`.
- Current install metadata is `.mise-installs.toml` plus per-tool metadata in `src/toolset/install_state.rs:40` and `src/toolset/install_state.rs:66`.
- Installed versions are filesystem-scanned in `src/toolset/install_state.rs:216`.
- Runtime symlinks are rebuilt in `src/runtime_symlinks.rs:16`.
- Current prune deletes tool versions not needed by tracked configs in `src/cli/prune.rs:108`.

Lock and derivation-adjacent behavior:

- `mise.lock` stores `LockfileTool { version, backend, options, platforms }` in `src/lockfile.rs:64`.
- `PlatformInfo` stores URL, checksum, provenance, conda deps, pkgx deps, and runtime metadata in `src/lockfile.rs:232`.
- Lockfile resolution matches short name, version prefix, and exact lock options in `src/lockfile.rs:2465`.
- Backend discovery may fall back from lockfile to registry in `src/cli/args/backend_arg.rs:388` and `src/cli/args/backend_arg.rs:393`.
- `ToolRequest` variants carry request kind/source/options in `src/toolset/tool_request.rs:25`.
- `ToolRequest::lockfile_resolve_with_prefix` is in `src/toolset/tool_request.rs:327`.
- `--locked` currently requires a lockfile URL only for URL-lockable backends in `src/backend/mod.rs:2081`.
- Missing checksum in lock-enabled mode can be filled with trust-on-first-use `blake3` in `src/backend/mod.rs:2727`.

Sandbox and platform behavior:

- `SandboxConfig` is in `src/sandbox/mod.rs:18`.
- Env filtering for sandbox `deny_env` is in `src/sandbox/mod.rs:106`.
- Linux applies Landlock/seccomp in `src/sandbox/mod.rs:185`.
- macOS wraps with `sandbox-exec` in `src/sandbox/mod.rs:202` and generates Seatbelt profiles in `src/sandbox/macos.rs:27`.
- Linux per-host network allow is unsupported in `src/sandbox/mod.rs:190`.
- Unsupported platforms warn and run unsandboxed in `src/sandbox/mod.rs:178`.

## Architecture overview

Nise needs four related but separate layers:

1. **Derivation lock**: a stable description of inputs and policies.
2. **Realisation**: the resolved platform-specific output for one derivation.
3. **Store object**: an immutable filesystem tree.
4. **Profile generation**: a mutable root that selects a set of realisations for a project/user shell.

Do not make one path mean all four things. Current mise often treats `ToolVersion::install_path()` as both build prefix and runtime prefix. That remains a compatibility path, not the store's internal truth.

```rust
pub struct InstallPaths {
    pub build_path: PathBuf,
    pub store_path: PathBuf,
    pub compatibility_path: PathBuf,
    pub runtime_path: PathBuf,
}
```

Definitions:

- `build_path`: mutable staging directory for the backend.
- `store_path`: immutable published object path.
- `compatibility_path`: old visible path, usually `installs/<tool>/<version>`.
- `runtime_path`: PATH-facing path, including fuzzy runtime symlinks and active profile paths.

Core invariant:

`store_path` is immutable and GC-managed. `compatibility_path` and `runtime_path` are references.

## Store layout

Default v1 store root:

```text
$NISE_STORE_DIR
```

Recommended default:

```text
$MISE_DATA_DIR/nise/store
```

For a local mounted partition, users can set `NISE_STORE_DIR=/Volumes/nise-store` or `/nise/store`. Nise should not manage partitions in v1. It should validate filesystem capabilities instead.

Layout:

```text
$NISE_STORE_DIR/
  objects/
    sha256/
      ab/
        abcdef...-ripgrep-14.1.1/
          .nise-object.toml
          payload...
  realisations/
    sha256/
      ab/
        abcdef....toml
  refs/
    installs/
      <tool>/<version>.toml
    profiles/
      user/default/
        current -> generations/42
        generations/
          42/.nise-profile.toml
      projects/<project-hash>/<profile>/
        current -> generations/7
        generations/
          7/.nise-profile.toml
    pins/
      <name>.toml
    transactions/
      <txn-id>.toml
    processes/
      <pid>-<nonce>.toml
  tmp/
    <txn-id>/
  trash/
    <timestamp>-<object-id>/
  locks/
```

The store must be usable without a database. A SQLite index can be added later for speed, but reachability must be provable from manifests and roots on disk.

## Store metadata

Object manifest:

```rust
#[derive(Serialize, Deserialize)]
pub struct ObjectManifest {
    pub schema_version: u32,
    pub object_id: String,
    pub tree_hash: String,
    pub hash_algorithm: String,
    pub name: String,
    pub platform: String,
    pub created_by: String,
    pub created_at: String,
    pub bytes: u64,
    pub files: u64,
    pub executable_paths: Vec<PathBuf>,
    pub bin_paths: Vec<PathBuf>,
    pub references: Vec<String>,
    pub realisations: Vec<String>,
}
```

Realisation manifest:

```rust
#[derive(Serialize, Deserialize)]
pub struct RealisationManifest {
    pub schema_version: u32,
    pub realisation_id: String,
    pub derivation_id: String,
    pub object_id: String,
    pub tool: String,
    pub backend: String,
    pub version: String,
    pub platform: String,
    pub options_hash: String,
    pub source_hash: String,
    pub lock_policy: String,
    pub provenance: Vec<ProvenanceRecord>,
    pub closure: Vec<String>,
    pub compatibility: CompatibilityRef,
}
```

Install reference manifest:

```rust
#[derive(Serialize, Deserialize)]
pub struct InstallRefManifest {
    pub schema_version: u32,
    pub tool: String,
    pub version: String,
    pub backend: String,
    pub compatibility_path: PathBuf,
    pub realisation_id: String,
    pub object_id: String,
    pub mode: InstallRefMode,
}

pub enum InstallRefMode {
    StoreSymlink,
    StorePointerFile,
    LegacyRealDirectory,
}
```

Profile manifest:

```rust
#[derive(Serialize, Deserialize)]
pub struct ProfileManifest {
    pub schema_version: u32,
    pub profile_id: String,
    pub generation: u64,
    pub project_root: Option<PathBuf>,
    pub source_config_hash: String,
    pub nise_lock_hash: String,
    pub created_at: String,
    pub realisations: Vec<String>,
    pub env_hash: String,
    pub path_entries: Vec<PathBuf>,
}
```

Transaction lease:

```rust
#[derive(Serialize, Deserialize)]
pub struct StoreTransactionManifest {
    pub schema_version: u32,
    pub txn_id: String,
    pub state: StoreTxnState,
    pub created_at: String,
    pub updated_at: String,
    pub pid: u32,
    pub derivation_id: String,
    pub realisation_id: Option<String>,
    pub object_id: Option<String>,
    pub build_path: PathBuf,
    pub store_path: Option<PathBuf>,
    pub compatibility_path: PathBuf,
}

pub enum StoreTxnState {
    Preparing,
    Building,
    Sealing,
    PublishedObject,
    LinkedCompatibility,
    ProfileRooted,
    Complete,
    Failed,
}
```

The transaction manifest is a GC root while it exists.

## Canonical hashing

Use two hashes:

- `derivation_id`: hash of declared inputs.
- `object_id`: hash of the published filesystem tree.

Derivation key input:

```text
schema_version
tool short name
request kind and value
resolved backend full name
backend implementation identity
tool options used by lock resolution
config semantic hash
registry semantic hash
platform key
resolver-affecting settings
source URL/hash where applicable
provenance policy
closure hashes
```

Object tree hash input:

```text
relative path
file type
file bytes
executable bit
symlink target
selected platform-relevant metadata
```

Do not hash:

- mtime,
- ctime,
- uid/gid,
- absolute staging path,
- generated timestamps.

If a backend embeds absolute prefixes, it is not `ImmutableRelocatable`.

## `nise.lock` schema

Keep `mise.lock` compatibility, but add `nise.lock` for derivation semantics. Existing `mise.lock` is artifact/version/backend metadata; it cannot prove source/config/registry/backend identity.

Example:

```toml
schema = "nise.lock"
schema_version = 1
hash_algorithm = "sha256"
generator = "nise 2026.x"

[policy]
mode = "required"       # advisory | required | frozen | paranoid
offline = "derivation"  # artifact | derivation | full
provenance = "lock-or-reverify"
allow_tofu = false

[sources."src:mise.toml"]
kind = "mise-toml"
path = "mise.toml"
raw_hash = "sha256:..."
semantic_hash = "sha256:..."

[registries."mise-builtin"]
kind = "mise-registry"
revision = "builtin:nise-2026.x"
semantic_hash = "sha256:..."

[[derivations]]
id = "sha256:..."
status = "strict" # strict | partial | legacy-unverified
tool = "ripgrep"
request = "14"
request_kind = "prefix"
resolved_version = "14.1.1"
backend = "aqua:BurntSushi/ripgrep"
backend_type = "aqua"
backend_identity = { kind = "builtin", nise = "2026.x" }
source = "src:mise.toml"
registries = ["mise-builtin"]
options = {}

[derivations.platforms.linux-x64]
url = "https://..."
checksum = "sha256:..."
size = 1234567
provenance = "github-attestations"
realisation = "sha256:..."
object = "sha256:..."
closure = "sha256:..."
```

Policy modes:

- `advisory`: use lock if present, warn on mismatch, allow refresh.
- `required`: fail when current inputs do not match a strict derivation.
- `frozen`: no writes, no refresh, mismatch is fatal.
- `paranoid`: frozen plus provenance re-verification and no trust-on-first-use hashes.

Strict checksum integration:

The current install flow can generate a `blake3` checksum when a platform checksum is absent and lockfiles are enabled. Strict and paranoid nise modes must fail before that trust-on-first-use path:

```rust
if policy.disallow_tofu && platform_info.checksum.is_none() {
    bail!("strict nise lock requires a predeclared checksum for {tool} on {platform}");
}
```

This check belongs before generic checksum verification/generation in the backend install flow, and it should be controlled by lock policy carried through `InstallContext`.

Offline modes:

- `artifact`: old `mise.lock`-style URL/checksum artifact use.
- `derivation`: all derivation inputs must be locally available.
- `full`: derivation, artifact, provenance, closure, and object verification must be local.

Migration:

1. `nise lock import mise.lock` creates `legacy-unverified` derivations.
2. `nise lock --refresh-derivations` recomputes inputs and promotes exact matches.
3. `nise lock --check --required` fails remaining legacy entries.
4. CI uses `nise lock --check --frozen` and `nise develop --locked --offline=full`.

## Store transaction protocol

Happy path:

1. Resolve tool request to a strict derivation.
2. Compute `derivation_id`.
3. Acquire logical install lock:
   - key: `{tool, version, backend, platform, derivation_id}`.
   - current code only locks `tv.install_path()` in `src/backend/mod.rs:2147`; store mode needs the logical key too.
4. Create transaction root under `refs/transactions/<txn-id>.toml`.
5. Create staging directory under `$NISE_STORE_DIR/tmp/<txn-id>/build`.
6. Create download/cache directories as currently expected by backends.
7. Run backend install according to capability mode.
8. Run mutating postinstall before sealing.
9. Validate:
   - no `incomplete` marker,
   - expected executable paths exist,
   - backend-specific smoke passes,
   - relocation scan passes for relocatable mode,
   - tree hash computed.
10. Write `.nise-object.toml`.
11. Fsync files, directories, object manifest, staging parent.
12. Publish object with same-filesystem rename.
13. Write realisation manifest.
14. Update compatibility path:
   - Unix: symlink `installs/<tool>/<version> -> store object`.
   - Windows: pointer file or junction, matching existing Windows path handling in `ToolVersion::install_path()` at `src/toolset/tool_version.rs:182`.
15. Update install state.
16. Build or update profile generation.
17. Remove transaction root.

Important rule:

Backends install into `build_path` only if they declare `ImmutableRelocatable`. Default is legacy.

## Backend capability API

Add conservative defaults to the `Backend` trait:

```rust
pub enum StoreInstallMode {
    LegacyMutable,
    ImmutableRelocatable,
    ImmutableFinalPrefix,
}

pub struct InstallSandboxPolicy {
    pub fetch_network: NetworkPolicy,
    pub build_network: NetworkPolicy,
    pub postinstall_network: NetworkPolicy,
    pub write_roots: Vec<PathBuf>,
    pub allow_env: Vec<String>,
}

pub trait Backend {
    fn store_install_mode(&self, tv: &ToolVersion) -> StoreInstallMode {
        StoreInstallMode::LegacyMutable
    }

    fn store_sandbox_policy(&self, tv: &ToolVersion) -> InstallSandboxPolicy {
        InstallSandboxPolicy::legacy()
    }

    fn validate_relocated_store(
        &self,
        ctx: &InstallContext,
        build_path: &Path,
        published: &ToolVersion,
    ) -> Result<()> {
        Ok(())
    }
}
```

Backend tiers:

- Tier A, first immutable candidates: `http`, selected `github`, selected `aqua`.
- Tier B, possible with prefix audit: `pkgx`, `conda`.
- Tier C, legacy initially: `npm`, `cargo`, `go`, `pipx`, `gem`, `spm`, `dotnet`.
- Tier D, legacy or new plugin API: `asdf`, `vfox`, source-building core plugins.

Compatibility gates before marking a backend immutable:

1. Install into a staging path containing a unique token.
2. Publish to a different path.
3. Search text files for staging token.
4. Run discovered bins through profile PATH.
5. Run backend-specific `exec_env`.
6. Make object read-only and run smoke again.
7. Uninstall compatibility ref and confirm GC retains object while profile root exists.

Phase separation requirement:

Current backends expose one monolithic `install_version_` hook after the wrapper creates install/download/cache dirs. Strict store sandboxing needs more structure than that. A backend is not eligible for strict immutable mode until it can either:

- declare an install plan with separate fetch, build, postinstall, and smoke phases, or
- prove that its monolithic install can safely run under one conservative sandbox policy.

Add this intermediate API before enforcing phase-specific sandboxing:

```rust
pub enum InstallPhase {
    Fetch,
    Build,
    PostInstall,
    Smoke,
}

pub struct BackendInstallPlan {
    pub phases: Vec<InstallPhase>,
    pub monolithic: bool,
    pub strict_sandbox_supported: bool,
}
```

If `monolithic = true` and `strict_sandbox_supported = false`, strict immutable mode must reject the backend instead of silently applying a weaker policy.

## Compatibility path rules

Current code scans install directories and filters runtime symlinks. A store-backed install ref must not be confused with a fuzzy runtime symlink.

Add a central resolver:

```rust
pub enum InstalledVersionEntry {
    LegacyDir { path: PathBuf },
    StoreRef { ref_manifest: InstallRefManifest },
    RuntimeAlias { path: PathBuf },
    BrokenRef { path: PathBuf, reason: String },
}
```

Replace ad hoc scans in install state, prune, uninstall, runtime symlinks, and shim generation with store-aware discovery.

Rules:

- Runtime aliases are not concrete versions.
- Store refs are concrete versions.
- Broken store refs are never silently ignored in strict mode.
- Shared/system dirs are external roots and are never swept by store GC.
- Legacy real directories remain valid until migrated.
- Discovery must not introduce new semver ordering. Existing code sorts some installed versions through `versions::Versioning`; store-aware discovery should preserve backend-provided order or stable filesystem order unless the backend explicitly provides an ordering function.
- Add non-semver fixtures such as `nightly`, `ref-main`, `lts-iron`, `20241015`, and `3.12.0a1` to discovery/runtime-symlink tests.

Store-aware uninstall/prune rule:

- For `StoreRef`, `uninstall` removes the compatibility ref/profile root and then asks store GC to collect later. It must not call backend `uninstall_version` to recursively delete the object path.
- For `LegacyDir`, existing uninstall remains valid.
- For `BrokenRef`, strict mode fails and `store repair --remove-broken-refs` is required.
- `prune` must route through the same store-aware uninstall API. Once compatibility refs are symlinks/pointers, direct recursive deletion of `tv.install_path()` is a bug unless the entry is classified as `LegacyDir`.

## Profiles

Profiles are mutable roots over immutable realisations.

Scopes:

- user default,
- project default,
- named project profile,
- CI ephemeral profile.

Commands:

```text
nise profile list
nise profile show [PROFILE]
nise profile switch PROFILE
nise profile rollback [PROFILE] [GENERATION]
nise profile diff A B
nise profile pin PROFILE
nise profile unpin PROFILE
```

Profile update protocol:

1. Resolve and realise all requested derivations.
2. Write generation directory `generations/<n>.tmp`.
3. Write `.nise-profile.toml` with realisation ids.
4. Fsync generation directory.
5. Atomic rename `generations/<n>.tmp -> generations/<n>`.
6. Atomic symlink swap `current -> generations/<n>`.
7. Keep old generations according to retention policy.

Profile retention defaults:

- Keep current generation.
- Keep previous 5 generations.
- Keep generations newer than 14 days.
- Allow explicit pins to keep indefinitely.

## `nise develop`

CLI:

```text
nise develop [DIR]
  [--shell SHELL]
  [--pure | --impure]
  [--locked]
  [--offline=artifact|derivation|full]
  [--profile NAME]
  [--realize | --no-realize]
  [--isolate=strict|best-effort|off]
  [--deny-read] [--deny-write] [--deny-net] [--deny-env] [--deny-all]
  [--allow-read PATH...] [--allow-write PATH...] [--allow-net HOST...] [--allow-env VAR...]
  [-- COMMAND...]
```

Default recommendation:

- interactive shell: `--pure --realize --profile default --isolate=strict` when strict policy is configured,
- print modes: no mutation unless `--realize`,
- Windows strict isolation: fail closed until a Windows-native isolation layer exists.

Pure env algorithm:

1. Start from an allowlisted base env, not full `PRISTINE_ENV`.
2. Remove nise/mise shims, stale install dirs, and stale store profile paths from inherited PATH.
3. Resolve profile realisations from `nise.lock`.
4. Build tool env from realisation manifests and backend `exec_env`.
5. Build deterministic PATH from active profile bin paths.
6. Add config `[env]` after non-tool env and before post env, following current `toolset_env` ordering.
7. Serialize `__MISE_DIFF` or a new `__NISE_DIFF` for nested invocation compatibility.
8. Acquire process lease root under `refs/processes/` while shell/command runs.
9. Release process lease on exit.

The process lease is important: GC must not delete a profile generation or object while a shell that uses it is running.

Strict env-cache rule:

Current env construction can use a cached env before recomputation, and `Exec` injects env-cache and diff metadata. In strict mode, cache usage must be either disabled or keyed by all strict inputs:

- `nise.lock` hash,
- profile generation id,
- realisation ids,
- object ids,
- source config semantic hash,
- settings hash,
- env-cache schema version,
- PATH base hash after stale nise/mise paths are removed.

If any key is missing or unverified, strict `develop` recomputes env and refuses to write a reusable cache entry. `__MISE_ENV_CACHE_KEY` compatibility may remain for nested mise behavior, but nise should namespace strict cache metadata as `__NISE_ENV_CACHE_KEY` and `__NISE_DIFF` once shell integration exists.

## Sandbox policy

Current sandbox is useful but not enough to claim hermetic builds for all backends.

Strict `develop`:

- Linux: require Landlock and seccomp to enforce requested read/write/net policy.
- macOS: require `sandbox-exec` profile application to succeed.
- Windows: fail closed unless explicitly `--isolate=best-effort` or `--isolate=off`.
- Unsupported OS: fail closed.

Strict install:

- Fetch phase may access only declared hosts.
- Build phase defaults to no network for locked derivations.
- Build writes only to staging/download/cache dirs.
- Postinstall runs before seal and inherits the same write/network policy.
- Post-publish smoke is read-only and no-network.
- Monolithic backend install hooks cannot claim phase-specific sandbox guarantees unless their declared install plan proves that one sandbox policy safely covers fetch, build, postinstall, and smoke.

Best-effort isolation may warn and degrade, but only if requested explicitly.

## Garbage collection

GC is mark-sweep with leases, retention, and a final reachability recheck.

Authoritative roots:

- current profile generations,
- retained historical profile generations,
- pinned profiles,
- explicit object pins,
- active transaction manifests,
- active process leases,
- compatibility install refs,
- legacy real install directories,
- shared/system install dirs as external non-owned roots.

Non-roots:

- old lockfile entries by themselves,
- stale cache files,
- broken refs,
- unreferenced transaction tmp dirs after recovery window.

GC command:

```text
nise store gc [--dry-run] [--delete] [--json]
  [--older-than DURATION]
  [--keep-generations N]
  [--keep-days N]
  [--include-trash]
```

Algorithm:

1. Acquire global GC lock.
2. Read all roots.
3. Drop expired dead process leases only after verifying process is gone.
4. Mark:
   - profile -> realisations -> objects -> object references,
   - install refs -> realisations -> objects,
   - transactions -> staged/object ids,
   - pins -> realisations/objects.
5. List store objects.
6. Candidate = object not marked, not already trash, older than grace period.
7. For each candidate:
   - acquire object delete lock,
   - re-read roots,
   - re-mark only this object's reverse reachability,
   - verify still unreachable,
   - rename object into `trash/`,
   - fsync parent,
   - optionally delete trash immediately if `--delete`.
8. Release locks.

Never delete directly from `objects/`. Rename to `trash/` first.

Default GC behavior:

- Dry-run unless `--delete`.
- Grace period: 24 hours.
- Keep trash for 7 days.
- Refuse to run if store verification detects manifest parse errors unless `--force-quarantine`.

## Crash recovery

`nise store repair` and startup opportunistic repair should handle these states:

| State | Evidence | Recovery |
|---|---|---|
| tmp dir without transaction | `tmp/<id>` exists, no root | remove if older than grace period |
| transaction `Preparing` | manifest exists, no build output | remove transaction and tmp |
| transaction `Building` | build output exists | keep as root until TTL, then mark failed |
| transaction `Sealing` | object manifest in tmp | validate or quarantine tmp |
| `PublishedObject` | object exists, no realisation | write missing realisation if derivation data exists, else quarantine |
| `LinkedCompatibility` | install ref exists, no profile | keep if explicit install ref, else eligible after retention |
| `ProfileRooted` | generation exists, transaction remains | remove transaction after verifying profile current/retained |
| broken compatibility ref | ref points to missing object | fail strict commands, repair or unlink by explicit command |
| corrupted object | tree hash mismatch | quarantine object and fail any profile that reaches it |

Repair commands:

```text
nise store verify [--deep] [--json]
nise store repair [--dry-run] [--quarantine] [--remove-broken-refs]
nise store doctor
```

## Corruption handling

Verification levels:

- `manifest`: parse manifests and confirm referenced paths exist.
- `tree`: recompute object tree hashes.
- `runtime`: run backend smoke checks for active profile.

If corruption is found:

1. Mark object `corrupt` in a sidecar report.
2. Refuse to use it in strict mode.
3. Do not GC it as ordinary unreachable data.
4. Move to quarantine only with `repair --quarantine`.
5. Offer `nise store realise <derivation-id>` to rebuild.

## Commands

Lock:

```text
nise lock
nise lock --check
nise lock --check --frozen
nise lock --refresh-derivations
nise lock import mise.lock
nise lock explain <tool>
```

Store:

```text
nise store init
nise store path <tool>
nise store realise <derivation-id>
nise store verify [--deep]
nise store gc [--dry-run|--delete]
nise store repair
nise store pin <object|realisation>
nise store unpin <pin>
nise store roots
nise store why-live <object>
```

Develop/profile:

```text
nise develop
nise develop -- COMMAND...
nise profile list|show|switch|rollback|diff|pin|unpin
```

Migration:

```text
nise store migrate --dry-run
nise store migrate --tool <tool>
nise store migrate --mode manifest-only|store-ref|immutable
```

## Implementation phases and gates

### Phase 0: foundations without behavior change

Deliver:

- `src/store/` module,
- manifest structs,
- parser/writer,
- canonical tree hash,
- root reader,
- `nise store doctor`,
- no backend install behavior changes.

Gate:

- unit tests for manifest round-trip,
- tree hash stability,
- corrupt manifest detection,
- no changes to existing install/e2e behavior.

### Phase 1: store-aware discovery

Deliver:

- central installed-version discovery API,
- support `LegacyDir`, `StoreRef`, `RuntimeAlias`, `BrokenRef`,
- update install_state, prune, uninstall, runtime symlinks, shims to use it.

Gate:

- tests prove runtime symlinks are still ignored,
- store refs are concrete versions,
- broken refs fail in strict mode,
- shared/system dirs are never owned by GC.

### Phase 2: profiles over existing installs

Deliver:

- profile manifests and generations,
- `nise develop` uses profile PATH,
- process leases,
- no immutable objects yet.

Gate:

- `nise develop` shell keeps a process lease,
- profile rollback changes PATH deterministically,
- GC dry-run keeps active and retained generations.

### Phase 3: strict derivation lock

Deliver:

- `nise.lock`,
- `nise lock --check --frozen`,
- migration from `mise.lock`,
- strict policy modes.

Gate:

- config formatting does not change derivation hash,
- tool/backend/options/settings changes do change hash,
- registry fallback is disabled in strict mode,
- offline full mode fails when inputs are missing.

### Phase 4: manifest-only legacy installs

Deliver:

- after normal install, write realisation and install-ref manifests,
- compatibility path remains a real directory,
- GC understands these refs but does not delete payloads directly.

Gate:

- uninstall still works,
- prune still works,
- shims still rebuild,
- manifests survive reinstall/force paths.

### Phase 5: immutable archive backends

Deliver:

- `StoreTxn`,
- `ImmutableRelocatable` for selected `http`, `github`, `aqua`,
- staging/seal/publish,
- read-only objects,
- relocation scan.

Gate:

- concurrent install produces one object,
- crash injection passes each transaction state,
- relocation token absent after publish,
- smoke tests pass through profile PATH,
- uninstall removes ref, GC keeps object while profile root exists.

### Phase 6: GC deletion

Deliver:

- mark-sweep GC,
- trash and grace period,
- final reachability recheck,
- process and transaction leases,
- `why-live`.

Gate:

- GC cannot delete objects held by running `develop`,
- GC cannot delete objects reachable only through retained old generation,
- GC cannot touch shared/system install dirs,
- corrupt objects are quarantined, not ordinary-deleted.

### Phase 7: prefix-sensitive backends

Deliver:

- `ImmutableFinalPrefix`,
- pkgx/conda experiments with explicit prefix validation,
- package-manager backends remain legacy unless proven.

Gate:

- backend matrix documents mode per backend,
- each promoted backend has install-move-run or final-prefix-read-only tests,
- non-promoted backends fail strict immutable mode with actionable messages.

### Phase 8: rebrand and default posture

Deliver:

- binary/package/docs/completions/env names updated deliberately,
- compatibility aliases retained,
- strict mode can become default for `nise develop` only after gates above pass.

Gate:

- generated docs/completions render,
- existing mise compatibility tests pass,
- new nise command tests pass.

## Test matrix

Store unit tests:

- manifest round-trip,
- canonical tree hash includes executable bit and symlink target,
- tree hash ignores mtime,
- object id mismatch is detected,
- transaction state recovery table.

Discovery tests:

- legacy real install dir,
- runtime symlink ignored,
- store compatibility symlink accepted,
- Windows pointer file accepted,
- broken store ref classified.

GC tests:

- active profile keeps object,
- old retained generation keeps object,
- pin keeps object,
- running process lease keeps object,
- transaction lease keeps object,
- final recheck prevents delete after new root appears,
- external shared dir never deleted,
- trash retention works.

Backend tests:

- `http` archive immutable publish,
- selected `github` immutable publish,
- selected `aqua` immutable publish,
- conda/pkgx prefix audit expected failures,
- npm/cargo/go legacy mode rejection under strict immutable.

Develop tests:

- inherited env removal,
- computed project env preservation,
- deterministic PATH from profile,
- nested `nise develop`,
- shell rc activation does not repollute pure env,
- strict Windows isolation failure,
- Linux deny-net/read/write enforcement where available,
- macOS sandbox profile generation and application.

Crash tests:

- kill before seal,
- kill after object publish before ref write,
- kill after ref write before profile root,
- kill during GC trash rename,
- repair resumes or quarantines deterministically.

## Risk register

High risk: backend path embedding.

- Mitigation: capability defaults to legacy, relocation tests required.

High risk: symlinked compatibility paths break current discovery.

- Mitigation: central store-aware discovery before immutable publish.

High risk: GC race deletes a newly referenced object.

- Mitigation: transaction/process leases, global GC lock, object locks, final recheck.

High risk: object mutates through compatibility path.

- Mitigation: read-only objects, verify before reuse, legacy mode for self-mutating tools.

High risk: strict mode overclaims provenance.

- Mitigation: distinguish verified current-platform provenance from metadata-only cross-platform provenance.

Medium risk: cross-filesystem store.

- Mitigation: stage under store root; compatibility refs are symlink/pointer only; no cross-device rename required for object publish.

Medium risk: lock churn.

- Mitigation: derivation hashes use semantic config hashes; raw hashes are audit-only.

Medium risk: Windows.

- Mitigation: pointer-file compatibility path, fail-closed strict isolation, dedicated Windows tests before defaulting strict mode.

## Completion definition

The full integration is complete only when:

1. `nise develop --locked --offline=full --isolate=strict` works on a supported Linux project from `nise.lock` and store objects only.
2. `nise store verify --deep` proves active profile objects.
3. `nise store gc --delete` cannot remove running, pinned, retained, or transaction-rooted objects.
4. At least `http`, one `github` tool, and one `aqua` tool pass immutable publish gates.
5. Legacy backends still install and run in legacy mode.
6. Strict immutable mode rejects unsupported backends with clear messages.
7. Crash-injection tests pass for publish and GC.
8. Existing mise compatibility tests for install, exec, env, shims, prune, and uninstall still pass.
