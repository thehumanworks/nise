# Nise deep dive

Date: 2026-07-03

Fork: https://github.com/thehumanworks/nise

Local checkout: `/Users/tomasroda/Documents/Codex/2026-07-03/fork-mise-and-using-subagents-deep/nise`

## Goal

Turn mise into `nise`: a Nix-like environment manager with a local/global store, stronger environment isolation, and a `develop` command that enters a pure project shell.

The practical interpretation should be:

- `nise develop` starts as a pure shell over the resolved project toolset and env.
- Tool installs move from mutable per-user prefixes toward immutable store paths.
- Lockfiles become stricter than version/artifact locks and eventually describe derivation-like inputs.
- OS sandboxing is used where it is real, and strict modes fail closed where it is not.
- A configurable store root is used first. Managing a literal disk partition should be optional and later; it is platform-specific and not needed for the core semantics.

## Executive summary

This is feasible, but not as a small rename or a single command addition.

The lowest-risk first slice is `nise develop --pure`, implemented as a stricter sibling of `mise en` and `mise exec`. The repo already has most of the command launching, toolset resolution, env construction, and sandbox flags needed for that.

The hard part is a true Nix-like store. Today mise has install roots, shared install directories, lockfiles, artifact checksums, provenance metadata, and OS sandboxing. It does not have content-addressed immutable store paths, derivation identities, full input closures, profile roots, or garbage collection. Those need new architecture.

The deepest risk is backend behavior. Backends like `http`, `github`, `aqua`, `conda`, and some core binary tools are plausible early candidates. Installer-driven backends such as `npm`, `cargo`, `pipx`, `gem`, `go`, `asdf`, and `vfox` are not Nix-like without tracing, sandboxing, or ecosystem-specific lock/closure capture.

## Existing building blocks

### Command surface

- CLI commands are Clap enum variants in `src/cli/mod.rs:206` and dispatched in `src/cli/mod.rs:281`.
- `src/cli/en.rs:6` is the closest current command: it starts a shell with the mise environment.
- `src/cli/en.rs:25` delegates to `Exec`, so `en` inherits the same toolset/env/launch behavior as `exec`.
- `src/cli/exec.rs:31` defines the useful existing flags: tools, command, `--fresh-env`, `--deny-env`, `--deny-read`, `--deny-write`, `--deny-net`, and allowlists.
- `src/cli/exec.rs:112` builds the `Toolset` and installs missing versions.
- `src/cli/exec.rs:183` writes `MISE_ENV`, cache metadata, and `__MISE_DIFF`.
- `src/cli/exec.rs:237` builds `SandboxConfig`, filters env when sandbox env isolation is active, and launches the command.
- `src/cli/exec.rs:263` uses Unix `exec`; `src/cli/exec.rs:348` handles Windows separately and warns that sandboxing is unsupported.

Current issue: `en` and `exec` are not pure by default. They start from inherited process state, and `--deny-env` is a sandbox env filter rather than a "clear inherited env but keep computed project env" mode.

### Env and install roots

- Data/cache/config/state/install/shim dirs are env-driven in `src/env.rs:98`.
- System install roots exist through `MISE_SYSTEM_DATA_DIR` and `MISE_SYSTEM_INSTALLS_DIR` in `src/env.rs:130`.
- Shared install roots are already modeled in `src/env.rs:152`.
- Install path origin can be categorized as system/shared/local in `src/env.rs:188`.
- Directory wrappers live in `src/dirs.rs:9`.
- A `ToolVersion` computes its install path from the active install root in `src/toolset/tool_version.rs:169`.
- Runtime paths may point through a fuzzy-version symlink in `src/toolset/tool_version.rs:219`.
- Current install path names are tool/version/ref/path-derived, not content-addressed derivation paths, in `src/toolset/tool_version.rs:293`.

Current issue: shared install dirs are lookup roots, not immutable store paths. Strict mode must disable or explicitly audit shared fallback, otherwise a "local partition" can silently consume global state.

### Install and backend flow

- `src/toolset/toolset_install.rs:30` installs missing versions.
- `src/toolset/toolset_install.rs:297` schedules installs with dependency ordering.
- `src/toolset/toolset_install.rs:461` resolves a `ToolRequest`, may set system/shared install path overrides, creates `InstallContext`, and calls `backend.install_version`.
- The `Backend` trait starts around `src/backend/mod.rs:1122`.
- Default install flow in `src/backend/mod.rs:2065` handles locked URLs, force, install locks, directory creation, backend install, and install-state writes.
- Default uninstall removes the install path, download path, and cache path around `src/backend/mod.rs:2346`.
- Default `list_bin_paths` returns `<runtime_path>/bin` around `src/backend/mod.rs:2386`.
- Backend-provided env hooks are exposed through `exec_env` around `src/backend/mod.rs:2406`.
- Install manifests are tracked in `src/toolset/install_state.rs:30`, with per-tool files around `src/toolset/install_state.rs:66`.

Current issue: backends own the write process and can write mutable, non-relocatable outputs. A store design needs a staging directory, validation, sealing, and atomic publish step around this boundary.

### PATH and project env

- `Toolset::list_paths` assembles tool bin paths in `src/toolset/toolset_paths.rs:18`.
- Final PATH ordering includes config paths, virtual envs, tool paths, and env paths in `src/toolset/toolset_paths.rs:64`.
- `full_env` starts from `PRISTINE_ENV` in `src/toolset/toolset_env.rs:33`.
- `env_with_path` caches final env and PATH construction in `src/toolset/toolset_env.rs:69`.
- Backend env and config env are merged in `src/toolset/toolset_env.rs:340`.
- Final env computation is in `src/toolset/toolset_env.rs:375`.
- PATH manipulation and stale install path filtering are in `src/path_env.rs:53` and `src/path_env.rs:100`.

Current issue: Nix-like pure shell semantics should not begin from `PRISTINE_ENV`. It should construct a minimal base env, then add project config env, tool env, and a deterministic PATH.

## `nise develop`

### Recommended initial behavior

Add a new `develop` command rather than changing `en`.

Proposed semantics:

- Resolve the project toolset from config and lockfile.
- Install missing tools unless `--offline`, `--locked`, or strict policy prevents it.
- Start an interactive shell from `$SHELL` on Unix and a sensible shell on Windows.
- Default to pure env for `nise develop`; optionally expose `--impure` for legacy behavior.
- Preserve only a small base allowlist such as `HOME`, `USER`, `LOGNAME`, `SHELL`, `TERM`, `LANG`, `LC_*`, and explicit `--allow-env`.
- Add computed project env, backend env, and deterministic PATH.
- Avoid auto-activation re-pollution from shell rc files. The current `en` e2e around `e2e/cli/test_en_mise_env` is the right regression surface.
- Expose sandbox flags separately from env purity: `--deny-net`, `--deny-read`, `--deny-write`, `--allow-read`, `--allow-write`, `--allow-net`.

Implementation shape:

- Add `Develop` to `Commands` in `src/cli/mod.rs:206`.
- Add dispatch in `src/cli/mod.rs:281`.
- Create `src/cli/develop.rs`, using `src/cli/en.rs` as command-shape reference and `src/cli/exec.rs` as behavior reference.
- Refactor the reusable parts of `Exec::run` into a launch helper rather than duplicating toolset/env/sandbox/process code.
- Add a separate env-purity mode instead of overloading `SandboxConfig::deny_env`.
- Add e2e coverage beside `e2e/cli/test_en_mise_env`, `e2e/cli/test_exec_stale_install_path`, and `e2e/env/test_env_cache_fresh`.

### What this does not prove

`develop --pure` can be a clean shell. It is not a hermetic build sandbox by itself. File system and network isolation depend on the OS sandbox layer.

## Store design

### Phase 1 store: configured root

Start with a store root, not disk partition management:

- `NISE_STORE_DIR`, defaulting to an OS-appropriate data location.
- Compatibility aliases from `MISE_*` may be accepted early, but new docs and generated help should use `NISE_*`.
- Strict mode should disable accidental lookup from `MISE_SHARED_INSTALL_DIRS` unless explicitly configured.
- Existing `<install_root>/<tool>/<version>` paths can become profile pointers to store realizations.

This gives isolation value without requiring root permissions, APFS volumes, Linux mounts, Windows volume management, or cross-platform partition code.

### Phase 2 store: immutable realisations

Introduce a `src/store/` module:

- `StoreRoot`
- `StorePath`
- `DerivationKey`
- `Realisation`
- `StoreManifest`
- `ProfileRoot`

Install flow:

1. Resolve tool/version/backend/lock metadata.
2. Compute a derivation key.
3. Build into a temp staging directory outside the final store path.
4. Validate expected files, checksums, and relocatability policy.
5. Atomically publish into the store.
6. Mark the path read-only where supported.
7. Link project/user profiles to the store path.

### Phase 3 store: content-addressed and GC-able

Add:

- content or output hashes in path identity,
- closure metadata,
- reference roots for profiles and active projects,
- `nise store verify`,
- `nise store gc`,
- `nise store path`,
- `nise store closure`.

This is the point where it becomes fair to call the global store Nix-like.

## Reproducibility and lock model

### What exists now

- `mise.lock` stores tool version, backend, options, and platform metadata in `src/lockfile.rs:59`.
- Platform metadata includes URL, checksum, provenance, conda deps, pkgx deps, and GitHub attestation status in `src/lockfile.rs:232`.
- Tool requests consult the lockfile before local installed prefix matching in `src/toolset/tool_request.rs:321`.
- Lockfile lookup is scoped and option-aware in `src/lockfile.rs:2465`.
- Backend identity can come from lockfile in `src/lockfile.rs:2530`.
- `InstallContext.locked` exists in `src/install_context.rs:8`.
- Locked install mode requires lockfile URL presence for URL-lockable backends in `src/backend/mod.rs:2065`.
- Generic checksum verification and trust-on-first-use `blake3` generation are in `src/backend/mod.rs:2704`.
- Lockfile docs describe reproducible env goals and locked mode in `docs/dev-tools/mise-lock.md:1` and `docs/dev-tools/mise-lock.md:151`.
- Settings for `locked`, provenance re-verification, lockfiles, lockfile platforms, and offline mode are in `settings.toml:1288`, `settings.toml:1313`, `settings.toml:1332`, `settings.toml:1366`, and `settings.toml:1734`.

### What is missing

Current mise has artifact/version/backend lock semantics, not derivation semantics.

Missing for Nix-like `nise`:

- builder/backend implementation identity,
- plugin repository revision,
- registry snapshot hash,
- config file set hash,
- local `path:` input content hash,
- source archive hash as a required fixed-output input,
- normalized build args and env,
- declared outputs,
- transitive dependency closure,
- output hash,
- policy for provenance and trust-on-first-use hashes.

### Strict lock phase

Add a strict policy mode above current `locked`:

- fail if backend is absent,
- fail if version is not exact,
- fail if current platform has no locked URL or install source,
- fail if checksum is missing,
- fail if checksum is only generated trust-on-first-use when policy forbids it,
- fail if provenance is missing when policy requires it,
- disable registry fallback,
- disable network unless command is explicitly a lock/update/fetch operation.

Start with backends that already have strong artifact metadata: `http`, `github`, `aqua`, `conda`, and selected core binary backends.

## Platform isolation

### Existing sandbox model

- `SandboxConfig` fields are in `src/sandbox/mod.rs:18`.
- Env filtering for deny-env is in `src/sandbox/mod.rs:106`.
- Filesystem and network sandbox application starts in `src/sandbox/mod.rs:151`.
- Linux uses Landlock for filesystem and seccomp for network in `src/sandbox/mod.rs:185`.
- macOS uses generated Seatbelt profiles in `src/sandbox/macos.rs:27`.
- Linux readable system paths and allowed path logic are in `src/sandbox/landlock.rs:9` and `src/sandbox/landlock.rs:60`.
- Linux network blocking uses seccomp in `src/sandbox/seccomp.rs:9`.
- Non-Linux/non-macOS sandbox paths warn and run unsandboxed in `src/sandbox/mod.rs:178`.

### Platform conclusions

Linux is the strongest target. Landlock and seccomp give a real foundation for read/write/net isolation, though it is still not a full information-flow proof.

macOS is useful but partial. `sandbox-exec`/Seatbelt can restrict many commands, but host/network allowlists and long-term OS support need explicit testing and fail-closed policy.

Windows currently has no equivalent enforcement in the inspected paths. Strict isolation should fail closed on Windows unless an explicit `--allow-unsandboxed` or config opt-in is provided.

## Backend classification

Early candidates:

- `http`: closest to fixed-output artifact installs because it can resolve URLs and checksums.
- `github` and `aqua`: strong artifact metadata and provenance possibilities.
- `conda`: records main URL/checksum and dependency package URLs/checksums; locked install flow is in `src/backend/conda.rs:430`.
- `pkgx`: has dependency closure metadata, but wrappers embed absolute paths and need relocatability audits around `src/backend/pkgx.rs:682` and `src/backend/pkgx.rs:895`.

Needs more work:

- core language builds,
- `cargo`,
- `npm`,
- `pipx`,
- `gem`,
- `go`,
- `spm`.

Exclude from strict mode until redesigned:

- system packages,
- Homebrew/Cask/MAS,
- `asdf`,
- `vfox`.

Reason: many installer-driven flows assume mutable caches, external toolchains, network, global prefixes, or arbitrary scripts. OCI code already rejects `asdf`/`vfox` for packaging because scripts can write outside the per-version directory around `src/oci/builder.rs:841`.

## OCI relevance

OCI is useful, but it is not the local store.

- OCI docs describe one layer per installed tool because installs are under `MISE_DATA_DIR/installs/<plugin>/<version>` in `docs/dev-tools/mise-oci.md:3`.
- OCI CLI is experimental and shells out to external tools in `docs/dev-tools/mise-oci.md:27`.
- OCI images place tools under `/mise/installs` and synthesize config in `docs/dev-tools/mise-oci.md:65`.
- The builder requires an absolute mount point in `src/oci/builder.rs:99`.
- Non-Linux host builds warn about host binaries in Linux containers in `src/oci/builder.rs:193`.
- Builder packages existing host install dirs into layers in `src/oci/builder.rs:209`.
- Host OS is normalized to Linux for OCI metadata in `src/oci/mod.rs:32`.
- OCI apt support is Linux/apt-specific in `docs/dev-tools/mise-oci.md:227`.

Use OCI later as a Linux runtime/build packaging target. Do not conflate it with the local/global store design.

## Fork and rebrand work

The fork exists as `nise`, but the code is still mise-branded.

Minimal fork hygiene:

- Keep package internals as `mise` until architecture work starts, to avoid noisy churn.
- Add a `nise` binary name only when tests and shell completions can be updated together.
- Keep `mise` compatibility aliases and env vars for migration.

Full rebrand surface includes:

- `Cargo.toml` package/bin metadata,
- `package.json`,
- `mise.toml` schema path,
- generated `mise.usage.kdl`,
- docs under `docs/`,
- completions under `completions/`,
- shims in `src/shims.rs`,
- install docs and release metadata,
- env var docs/settings generated from `settings.toml`.

Recommendation: do not start with a wholesale rebrand. First land `develop` and strict-store architecture behind `nise` naming, then rebrand generated surfaces deliberately.

## Phased implementation plan

### Phase 0: fork hygiene and design gates

- Keep upstream remote.
- Add this report and a tracking issue/roadmap.
- Define product vocabulary: store root, profile, realisation, derivation key, pure shell, strict mode.
- Decide env compatibility: support both `MISE_*` and `NISE_*`, with `NISE_*` taking precedence.

### Phase 1: `nise develop --pure`

- Add `develop` CLI command.
- Refactor `Exec` launch flow into shared command-launch helper.
- Implement pure env base separate from `deny_env`.
- Add e2e tests for inherited env removal, computed project env preservation, PATH determinism, rc-file activation, and platform fallback.
- Keep store semantics unchanged in this phase.

### Phase 2: strict lock policy

- Add strict lock mode that requires exact backend/version/platform/checksum.
- Forbid registry fallback in strict mode.
- Forbid TOFU checksums unless policy allows them.
- Require provenance where configured.
- Add failure-mode e2e tests.

### Phase 3: store root and profile links

- Add `NISE_STORE_DIR`.
- Add a store module with typed paths and profile roots.
- Install into staging, validate, then publish.
- Link old install root paths to store realisations for compatibility.
- Disable shared fallback in strict mode unless explicit.

### Phase 4: immutable store and GC

- Add store manifests and closure metadata.
- Add `nise store verify`, `nise store gc`, `nise store closure`, and `nise profile`.
- Make profile roots first-class GC roots.

### Phase 5: derivation lock model

- Extend or supplement `mise.lock` with `nise.lock` derivation records.
- Capture source hash, config hash set, backend/plugin revision, builder identity, inputs, env, outputs, output hash, and provenance policy.
- Start with fixed-output artifacts and conda/pkgx.

### Phase 6: build sandboxing and OCI builders

- Linux-first sandboxed build pipeline.
- macOS best-effort with explicit caveats.
- Windows fail-closed unless a native isolation implementation is added.
- OCI build isolation for unsafe Linux build backends.

## Verification performed

Commands run in the fork:

```sh
cargo metadata --locked --no-deps --format-version 1
cargo check --locked --all-targets
MISE_TRUSTED_CONFIG_PATHS="$PWD" mise run test:e2e e2e/cli/test_en_mise_env e2e/cli/test_exec_stale_install_path e2e/env/test_env_cache_fresh
```

Results:

- `cargo metadata --locked --no-deps --format-version 1`: passed.
- `cargo check --locked --all-targets`: passed.
- Targeted e2e command: passed in the subagent run after using `MISE_TRUSTED_CONFIG_PATHS="$PWD"` to avoid mutating local trust state.

Side-effect watchpoint:

- The e2e/tool installation path can update generated `mise.lock` with host-platform entries. Check `git status` before committing unrelated changes.

## Subagent coverage

Subagents were used for separate read-only investigations:

- command/env/shell surface for `develop`,
- reproducibility and lock/build semantics,
- platform isolation, store feasibility, and OCI relevance,
- fork/rebrand surface.

Two Claude-provider subagents failed with authentication errors and were replaced with available Codex/Pi agents. The useful outputs were incorporated above.

## Main open decisions

1. Should `nise develop` default to installing missing tools, or should pure mode require pre-realised store paths unless `--install` is explicit?
2. Should `NISE_STORE_DIR` be the only store root, or should `MISE_INSTALLS_DIR` remain a compatibility alias in strict mode?
3. Should strict mode reject all non-checksummed tools immediately, or warn first for migration?
4. Should derivation records live inside `mise.lock`, or should `nise.lock` be introduced to avoid upstream format churn?
5. Should Windows `develop --pure` be env-only, or fail closed until process/file/network isolation exists?
