# User-Specified Virtual Packages

## Proposal for Conda Channel-Defined Virtual Package Plugins

### Status

Implemented behind the `experimental-virtual-package-plugins` cargo feature: the registration is
parsed from repodata and handed to callers, and a plugin's output can be parsed and checked against
that registration. What is missing is the machinery that produces the output -- installing a plugin,
running it, caching what it said. Nothing is visible unless the feature is enabled, and rattler still
fetches or executes no plugin today.

| Part | State |
| --- | --- |
| `info.virtual_package_plugins` parsing (`repodata.json` and sharded index) | Implemented |
| `Gateway::virtual_package_plugins(channel, platform)` accessor | Implemented |
| Registrations on `RepoDataQueryOutput`, per channel subdir | Implemented |
| Inherited and shadowed registrations along the CEP-42 `base` chain | Implemented (reported, not resolved) |
| Plugin output protocol (JSON Lines) | Implemented |
| Contract check of output against the registration | Implemented |
| `ChannelVirtualPackage` result type | Implemented |
| `rattler virtual-packages -c <channel> [--plugin/--check-output]` | Implemented |
| Conflict resolution across channels | Deliberately not done -- reported as declared, caller decides |
| Running a plugin out of an existing environment | Implemented |
| Detection result cache | Implemented |
| Plugin environment creation | Not implemented |
| Solver injection, `CONDA_OVERRIDE_*`, lockfile representation | Not implemented |
| Trust / opt-in model | Open, blocks execution |
| prefix.dev upload validation | Not implemented (server side) |

### Crate Layout

| Piece | Crate | Status |
| --- | --- | --- |
| `info.virtual_package_plugins` type and parsing | `rattler_conda_types` | done |
| `ChannelVirtualPackage` | `rattler_conda_types` | done |
| Registration accessors and query output | `rattler_repodata_gateway` | done |
| Output protocol, contract check | `rattler_virtual_package_plugins` | done |
| Running a plugin (`runner`) | `rattler_virtual_package_plugins` | done |
| Environment creation, orchestrator | `rattler_virtual_package_plugins` | to do |
| Detection result cache | `rattler_cache` | done |

`ChannelVirtualPackage` sits in `rattler_conda_types` rather than next to the code that produces it
because the result cache belongs in `rattler_cache`, beside `package_cache` and `run_exports_cache`,
and `rattler_cache` cannot depend on `rattler_virtual_package_plugins` without a cycle.

`rattler_virtual_package_plugins` gates its own contents on the feature and compiles to an empty
crate without it. That is not vanity: cargo features are additive, so a workspace member that enabled
`rattler_conda_types/experimental-virtual-package-plugins` unconditionally would switch the field on
for every build in the workspace.

`rattler_index` does not yet propagate the field: with the feature off it drops
`info.virtual_package_plugins` on a repodata round-trip, and with the feature on it writes an empty
map. Only a channel server publishing the field directly exercises the path today.

### Problem

Today, virtual packages like `__cuda` are hardcoded in the solver client. This made sense when NVIDIA
was the only accelerator that mattered, but the hardware landscape is diversifying fast. AMD ROCm,
Intel oneAPI, and other accelerator stacks each have their own driver versions, runtime libraries, and
capability matrices. Hardcoding detection logic for every new accelerator in every client release
doesn't scale. Channel operators who ship packages targeting these accelerators need a way to define
virtual packages like `__rocm` or `__oneapi` without waiting for upstream client changes.

### Proposal

We introduce a plugin-based virtual package system where channel operators on prefix.dev define custom
virtual packages backed by detection plugins. The solver treats them identically to built-in virtual
packages -- packages can depend on `__rocm >= 6.0` or `__oneapi >= 2025.1` the same way they depend on
`__cuda` today.

The system has two parts:

1. **Channel-side: plugin registration and validation**
2. **Client-side: plugin execution and caching**

---

### 1. Channel-Side: Plugin Registration

Channel operators register virtual package plugins as part of their channel configuration on
prefix.dev. Each registration names a conda package containing the detection logic and lists the
virtual packages that plugin provides.

During package upload, prefix.dev validates that any virtual package dependency declared in a
package's metadata has a corresponding plugin registered in the channel. Uploads referencing undefined
virtual packages are rejected.

The registration is published in the channel's `repodata.json` under a new `info.virtual_package_plugins`
field, keyed by **plugin package name**:

```json
{
  "info": {
    "virtual_package_plugins": {
      "cuda-detect": ["__cuda", "__cuda_arch"],
      "rocm-detect": ["__rocm"]
    }
  },
  "packages": { ... }
}
```

Keying by plugin rather than by virtual package is deliberate. The reverse direction --
`{"__cuda": "cuda-detect", "__cuda_arch": "cuda-detect"}` -- registers the same detector twice and
gives the client no way to know the two entries are one program doing one piece of work. Keying by
plugin makes "several virtual packages from one plugin" the ordinary case, which is what `__cuda` and
`__cuda_arch` actually need.

The client resolves the plugin package from the same channel, picking the latest available version. No
version constraint is expressible in the registration.

The `virtual_package -> plugin` mapping is *derived* by the client if it needs it. That inversion is
many-to-many: two plugins in one channel, or plugins in different channels, may each claim `__rocm`.
Nothing in the metadata prevents it and the client must resolve it.

The same field is published in the sharded repodata index (`repodata_shards.msgpack.zst`) under
`info`, so sharded channels carry the registration too.

**Per-subdir, not channel-wide.** `info` lives in each subdir's repodata, so the registration must be
repeated in every subdir of a channel, and different subdirs *may* declare different registrations.
Consumers see one entry per subdir and may union them. A channel-wide location would be better and
needs a CEP.

**Lenient parsing.** Plugin and virtual package names are parsed without validation, so a channel
publishing a malformed name does not make the whole `repodata.json` unusable.

### 2. Client-Side: Plugin Execution and Caching

*The output protocol and the contract check are implemented; fetching, installing, running and
caching are not. The decisions below are settled and constrain what remains.*

When a client resolves an environment and encounters a virtual package provided by a registered
plugin, it:

1. **Solves the plugin's environment** from the same channel, for the **host** platform. Detection
   inspects the running machine, so a plugin is never solved for a cross-compilation target.

   That solve uses **built-in virtual packages only**. Resolving a plugin's own dependencies is
   itself a solve against a channel whose plugin data is not available yet; restricting it to
   built-ins is what stops the recursion.

2. **Identifies the result by a hash over every package in that environment**, not by the plugin
   archive's own `sha256`. What a plugin reports depends on its dependencies, so its identity has to
   change when they do.

   This has an ordering consequence worth stating plainly: the hash is not known until the solve has
   happened, so a cache hit skips the install and the plugin run, never the solve. Against cached
   repodata the solve is the cheap part.

3. **Installs it** into a prefix of its own keyed by that hash, separate from the user's environment
   and reused across solves.

4. **Runs the entry point** directly from the prefix's binary directory, with the environment's
   binary directories prepended to `PATH` and `CONDA_PREFIX` set -- and nothing more. Running the
   file rather than a shell command avoids quoting surprises and keeps `activate.d` output off the
   stdout the protocol is parsed from; conda packages resolve their own libraries through `RPATH`,
   so skipping activation costs little.

5. **Reads the verdicts from stdout and checks them against the registration**: exactly one verdict
   per registered virtual package and nothing besides. A plugin claiming a name its channel never
   registered it for is rejected outright rather than filtered -- a channel promising one thing and
   shipping another is a bug worth surfacing, not something to paper over.

6. **Caches the verdicts** under the plugin's own cache policy, keyed by the same hash.

7. **Injects the results** into the solver's virtual package set alongside the built-ins, as
   `ChannelVirtualPackage`s:

```rust
pub struct ChannelVirtualPackage {
    pub channel: ChannelUrl,
    pub plugin_sha256: Sha256Hash,
    pub package: GenericVirtualPackage,
}
```

   The channel and the hash travel with the virtual package so provenance survives into whatever
   decides between two channels' claims on the same name. The solver itself still consumes plain
   `GenericVirtualPackage`s: it interns them by name and offers them as candidates, so scoping by
   channel stays the caller's job.

### Plugin Interface

Plugins are simple executables. **The entry point is the plugin package name**: package `cuda-detect`
ships an executable `cuda-detect`. Package names are unique within a channel and a JSON object cannot
repeat a key, so the entry point needs no separate metadata field, and conda already puts executables
in the environment's binary directory so no path needs declaring either.

**Output is JSON Lines**, one object per line, so a plugin can report as it discovers and a malformed
line can be reported with its line number instead of invalidating the whole run:

```text
{"kind": "present", "name": "__cuda", "version": "12.4"}
{"kind": "present", "name": "__cuda_arch", "version": "0", "build_string": "sm_89"}
{"kind": "absent", "name": "__rocm"}
{"kind": "cache", "ttl_seconds": 86400, "watch_paths": ["/sys/module/amdgpu/version"]}
```

`absent` is a line kind of its own rather than a `present` line with a null version. A plugin must
give a verdict on every virtual package its channel registered it for, so "not on this system" has to
be something it can say out loud -- and a null version cannot carry that, because serde maps a
missing key and an explicit `null` to the same value. A distinct kind puts the distinction in the wire
format instead of in a deserializer subtlety. `build_string` is optional and exists because
`__archspec` and `__cuda_arch` carry their information there rather than in the version.

At most one `cache` line per run; a second one is an error, since which policy applies would be
undefined. Unknown line kinds and unknown fields are rejected.

#### The process boundary

The entry point is invoked **directly, not through a shell**. A shell would run the environment's
`activate.d` scripts, and anything those print lands on the same stdout the protocol is parsed from, so a
chatty activation script would corrupt a plugin's output. What that gives up is limited: conda packages
resolve their own libraries through `RPATH`, so only a plugin relying on `activate.d` side effects would
notice.

The plugin therefore sees:

- **stdin** connected to `/dev/null`
- the parent's environment, with the plugin environment's binary directories prepended to `PATH` and
  `CONDA_PREFIX` set to the prefix
- nothing else -- no arguments, no configuration file, no environment variable of its own

Entry-point lookup uses the same directories activation would put on `PATH`
(`rattler_shell::activation::prefix_path_entries`), and on Windows also tries `.exe`, `.bat` and `.cmd`.

The contract:

- **stdin**: empty
- **stdout**: JSON Lines as above
- **stderr**: diagnostic output, logged at debug level
- **exit 0**: the plugin ran and its output is authoritative
- **exit non-zero**: plugin failure; a warning is logged and every virtual package it was registered
  for is treated as absent

This replaces the draft's three-way exit code (`0` present / `1` absent / `2+` failure). With several
virtual packages per plugin, presence is per verdict and cannot be carried by one exit status:
`__cuda` may be present while `__cuda_arch` is not.

**The run is bounded.** A plugin still running after **one second** is killed and reported as an
error: it is meant to read a version file or query a driver, and one that needs longer would stall
every solve that runs it. So is a plugin that produces more output than its registration can need:
**8 KiB per registered virtual package, plus two lines of headroom** -- one for the cache policy, one
of slack -- counted across stdout and stderr together. The 8 KiB per line is not tight. A verdict
line cannot get long, since a package's name, version and build string together fit in an archive
file name, which caps them at under 250 bytes; what can get long is a `cache` line watching a
filesystem path, at most `PATH_MAX` (4096 bytes on Linux), and 8 KiB fits one maximal path with
every byte JSON-escaped.

**Validation is exact.** Every registered name gets exactly one verdict; a duplicate, a name that was
never registered, or silence about one that was, each fail the run. A machine without the hardware is
the ordinary case and passes: every name still gets a verdict, they are simply all `absent`.

Plugins can be compiled binaries, shell scripts, or anything else that fits in a conda package.
Keeping the interface this simple means detection for a new accelerator is a single small package with
a shell script that checks a few paths.

### Data Written to Disk

#### The detection cache

One JSON file per (channel, plugin, plugin environment) under
`$RATTLER_CACHE_DIR/virtual-package-plugins/`. The file name is the plugin package name, so the
directory is readable, followed by a SHA-256 over all three parts of the key:

```
virtual-package-plugins/foobar-detect-037701a2e23b3403ee56053f7e53566e91ee7ecd7043f1f5ffb8f11ca541839a.json
```

All three parts are needed. Two channels may ship a `cuda-detect` that reports different things, a
channel may register several plugins, and what a plugin reports depends on the packages it runs with. The
plugin name is validated as a path component before use, since it comes from channel metadata and a
channel must not be able to place a file outside the cache directory.

```json
{
  "virtual_packages": [
    {
      "channel": "file:///path/to/channels/virtual-package-plugins",
      "plugin_sha256": "72029f5d5cf06962118b1863f7873826e48566014d12d0c6cf7dd7160964cea1",
      "package": "__foobar=1.2.3"
    },
    {
      "channel": "file:///path/to/channels/virtual-package-plugins",
      "plugin_sha256": "72029f5d5cf06962118b1863f7873826e48566014d12d0c6cf7dd7160964cea1",
      "package": "__foobar_arch=0=gen4"
    }
  ],
  "expires_at": 1785501271,
  "watched": [
    { "path": "/sys/module/amdgpu/version", "modified_ms": 1785497600000 }
  ]
}
```

- **`virtual_packages`** -- the verdicts, each carrying provenance. `package` is the
  `name=version=build_string` form, with the build string omitted when empty. `plugin_sha256` identifies
  the plugin environment rather than the plugin archive.
- **`expires_at`** -- seconds since the Unix epoch, derived from the `ttl_seconds` the plugin asked for.
  `null` means no time limit, so only `watched` can invalidate the entry.
- **`watched`** -- one entry per path the plugin asked to have watched, recording its modification time in
  milliseconds since the epoch, or `null` if it did not exist. Either changing invalidates the entry, so a
  driver appearing counts as much as one being upgraded -- the case a TTL cannot catch.

An entry is a miss if it is absent, expired, has a changed watched path, **or fails to parse**. A corrupt
cache file costs one plugin run; failing a solve over it would be worse.

A changed *registration* does not invalidate an entry: the key covers the channel, the plugin and its
environment, but not the set of virtual packages the channel registered the plugin for. A channel that
narrows its registration while the plugin environment stays identical is therefore served the old
verdicts, unchecked against the new registration, until the TTL or a watched path catches up.

The cache stores facts rather than protocol types: the caller turns a plugin's declared policy into an
expiry and a set of watched paths. That is also what lets the cache live in `rattler_cache` without it
depending on the crate that produces those results.

### Gateway Integration

Implemented. The repodata gateway parses `info.virtual_package_plugins` and reports it; it does not
execute plugins -- it doesn't know what hardware the client has -- and it does not resolve conflicts.

There are two ways to read the registrations:

`Gateway::virtual_package_plugins(channel, platform)` returns the map for one subdirectory. It takes
no specs, which is the point: the plugin package names only exist inside the metadata being fetched,
so there is nothing to query for until it has been read. It mirrors `Gateway::channel_relations`,
reusing the internal subdir cache, and yields an empty map for a subdirectory that registers none or
does not exist.

`RepoDataQueryOutput::virtual_package_plugins` returns one entry per channel subdir that declared a
registration, carrying the channel, the subdir platform, and the plugin-to-virtual-packages map,
ordered by resolved channel priority (including any CEP-42 relation-derived ordering). This is the
view a solve sees, so it also covers channels discovered through CEP-42 that the caller never named.

Duplicate claims are preserved verbatim in both: two channels each claiming `__rocm`, or two plugins
within one channel each claiming `__rocm`, all come back, and no warning is raised. Deciding which
plugin wins is the caller's job.

`resolve_channel_relation` is exported so a caller resolving a CEP-42 `base`/`overrides` reference
outside a query resolves it the same way the query path does. That validation stops malicious metadata
from pointing at attacker-controlled URLs, so a second implementation would be a place for the two to
drift apart.

For manual inspection, `rattler virtual-packages -c <channel>` walks the `base` chain and prints every
registration the channel can see, keyed by virtual package, warning where one shadows a name clients
detect themselves or a name an inherited channel already registers. Adding `--plugin <name>
--check-output <path|->` instead parses recorded plugin output and checks it against that channel's
registration -- scaffolding to exercise the protocol and the contract before an executor exists, and
worth deleting once one does.

Local fixtures to point it at, since no channel publishes the field yet:

- `test-data/channels/virtual-package-plugins` registers `foobar-detect` for `__foobar` and
  `__foobar_arch` and ships a `noarch: generic` package providing it, whose entry point prints fixed
  verdicts and exits zero. Synthetic names, so the fixture cannot collide with a virtual package clients
  detect themselves and cannot be caught by a future policy on shadowing. It also registers
  `rocm-detect` and deliberately ships no package for it, which exercises that error path.
- `-base` and `-derived` cover inheritance, and deliberately do register `__cuda` and `__glibc`, since
  provoking the shadowing warnings is what they are for.

All of this is behind the `experimental-virtual-package-plugins` feature. With the feature off the
gateway's public API and its serialized output are unchanged.

### Example: Supporting AMD ROCm

A channel operator shipping packages compiled against ROCm:

1. Creates a `rocm-detect` conda package containing a shell script named `rocm-detect` that checks for
   `/opt/rocm/.info/version` and parses the ROCm version.
2. Registers `rocm-detect -> ["__rocm"]` in their channel config on prefix.dev.
3. Uploads packages with `__rocm >= 6.0` in their run dependencies.
4. When a user with ROCm 6.1.2 installed runs `pixi install`, pixi fetches the plugin, runs it,
   discovers ROCm 6.1.2, and the solver selects the appropriate package variants.
5. A user without ROCm gets packages built for CPU fallback (or an unsatisfiable error if no fallback
   exists).

The same pattern works for Intel oneAPI, custom FPGA toolchains, or any other hardware capability that
packages need to select against.

### Example: One Plugin, Several Virtual Packages

A `cuda-detect` package registered as `cuda-detect -> ["__cuda", "__cuda_arch"]` queries the driver
once and reports both the driver version and the compute capability:

```text
{"kind": "present", "name": "__cuda", "version": "12.4"}
{"kind": "present", "name": "__cuda_arch", "version": "0", "build_string": "sm_89"}
```

On a machine with no NVIDIA driver the same plugin exits 0 and reports both as `absent` -- it still
has to account for every name it was registered for. Under the draft's original
one-plugin-per-virtual-package scheme this needed two packages, or one package with two entry points
repeating the same driver query.

### Settled Decisions

1. **Registration is keyed by plugin package name**, mapping to the list of virtual packages it
   provides.
2. **The entry point is the plugin package name.** No entry-point field in the metadata; uniqueness
   within a channel comes for free.
3. **No package-record changes.** The registration lives entirely in `info`; `PackageRecord` and
   `index.json` are untouched, so a client learns what a plugin provides without fetching the plugin's
   record first.
4. **No version constraints in the registration.** Bare package name, latest version.
5. **The gateway reports, it does not decide.** Registrations come back per subdir in channel-priority
   order with duplicates intact.
6. **Plugin identity is (channel, package name)** for conflict resolution, and a hash over the whole
   solved plugin environment for caching, so it changes when a dependency does.
7. **Output is JSON Lines with `present`/`absent`/`cache` line kinds.** Absence is stated explicitly,
   never implied by omission.
8. **Validation is exact**: one verdict per registered name, nothing else, silence included.
9. **The entry point runs from the prefix's binary directory** with `PATH` and `CONDA_PREFIX` set,
   rather than as a shell command in an activated environment.
10. **Detection is host-only**: the plugin environment is solved for the current platform.
11. **The plugin declares its own cache policy** (`ttl_seconds`, `watch_paths`) as a line of output.
12. **Results carry provenance** as `ChannelVirtualPackage`; the solver still receives plain
    `GenericVirtualPackage`s.
13. **Everything is behind an experimental cargo feature** and invisible when it is off.

### Open Questions

1. **Trust and governance.** Execution runs channel-supplied code during solve. Users should have to
   opt in, and the shape of that opt-in is unsettled: a single global switch, or a per-channel
   allowlist. The nearest precedent in rattler is `run_post_link_scripts`, a two-state setting whose
   opt-in value is named `insecure` and which defaults to off. This blocks the executor.
2. **How long a plugin environment should be kept.** Staleness is settled -- the environment is keyed
   by a hash over every package in it, so a dependency update produces a different key rather than a
   stale hit. What is left is eviction: nothing prunes the prefixes those keys accumulate.
3. **Cross-installing for another platform.** Detection is inherently host-only. Built-in virtual
   packages have `detect_for_platform` with documented cross-compilation defaults; plugins have no
   equivalent, and it is not clear what running a host plugin means when solving for a different
   target.
4. **Overrides and opt-out.** Users should be able to override or disable a specific virtual package
   (e.g. skip detection and assert `__rocm 6.1.2`). Built-ins use `CONDA_OVERRIDE_*`; the naming for
   plugin-provided packages, especially with sub-keys like `__cuda_arch` and with several channels
   registering the same name, is undecided.
5. **Reproducibility and lockfiles.** Whether the plugin version that produced a detection is recorded
   in the lock file, and when plugins are updated. Current leaning: always use the latest available and
   do not lock it, but this is unresolved.
6. **Channel-wide storage.** `info` is per-subdir, so the registration is duplicated across subdirs.
   A channel-wide metadata location would fix this and needs a CEP.
7. **Channel relations and overriding.** Whether a channel may register a plugin for a virtual package
   its base channel already covers (e.g. a private channel overriding `__glibc`), and whether such an
   override should affect the base channel. Shadowing along the `base` chain is now detected and
   reported, including registrations that shadow a virtual package clients detect themselves, but
   nothing acts on it: the policy is still open.
8. **Plugin dependencies.** Detection plugins should be self-contained, but if one needs a shared
   library to query a driver API, those deps are resolved from the same channel. Solving the plugin
   environment with built-in virtual packages only (see above) breaks the bootstrap recursion; the
   remaining risk is ordinary dependency conflict.
9. **Versioning semantics.** Virtual package versions should follow conda version ordering so that
   constraints like `__rocm >= 6.0, < 7` work as expected.
10. **wheelnext.** Worth looking at closely -- they are solving essentially the same problem.
