# User-Specified Virtual Packages

## Proposal for Conda Channel-Defined Virtual Package Plugins

### Status

Working end to end behind the `experimental-virtual-package-plugins` cargo feature: a channel's
registration is read from repodata, the plugin it names is installed and run, its verdicts are checked
against that registration and cached, and `rattler virtual-packages --detect` reports them. Nothing is
visible unless the feature is enabled, and a default build can neither fetch nor execute a plugin.

What is missing is what happens next: nothing feeds the results into a solve, and there is no trust model
beyond the feature flag itself.

| Part | State |
| --- | --- |
| `info.virtual_package_plugins` parsing (`repodata.json` and sharded index) | Implemented |
| `Gateway::virtual_package_plugins(channel, platform)` accessor | Implemented |
| Registrations on `RepoDataQueryOutput`, per channel subdir | Implemented |
| Inherited and shadowed registrations along the CEP-42 `base` chain | Implemented and resolved |
| Plugin report protocol (one JSON object) | Implemented |
| Contract check of output against the registration | Implemented |
| `SourcedVirtualPackage` result type, carrying a `BuiltIn` or `Plugin` source | Implemented |
| `rattler virtual-packages -c <channel> [--detect]` | Implemented |
| Resolution per view (channel + its CEP-42 relations) | Implemented -- CEP-42's relation graph, topologically sorted; cycles, depth and contradictions are errors |
| Running a plugin out of an existing environment, activated | Implemented |
| Detection result cache | Implemented |
| Plugin environment creation | Implemented |
| Detection end to end (orchestration) | Implemented |
| Solver injection, `CONDA_OVERRIDE_*`, lockfile representation | Not implemented |
| Trust / opt-in model | Open, blocks execution |
| prefix.dev upload validation | Proposed, server side; nothing here depends on it |

### Crate Layout

| Piece | Crate | Status |
| --- | --- | --- |
| `info.virtual_package_plugins` type and parsing | `rattler_conda_types` | done |
| `SourcedVirtualPackage` | `rattler_conda_types` | done |
| Registration accessors and query output | `rattler_repodata_gateway` | done |
| Report protocol, contract check | `rattler_virtual_package_plugins` | done |
| Activating a plugin's prefix (`activation`) | `rattler_virtual_package_plugins` | done |
| Running a plugin (`runner`) | `rattler_virtual_package_plugins` | done |
| Environment creation (`environment`) | `rattler_virtual_package_plugins` | done |
| Orchestration (`detect`) | `rattler_virtual_package_plugins` | done |
| `VirtualPackageFactory` and the built-in source (`factory`) | `rattler_virtual_package_plugins` | done |
| Detection result cache | `rattler_cache` | done |

`SourcedVirtualPackage` sits in `rattler_conda_types` rather than next to the code that produces it
because the result cache belongs in `rattler_cache`, beside `package_cache` and `run_exports_cache`,
and `rattler_cache` cannot depend on `rattler_virtual_package_plugins` without a cycle.

`rattler_virtual_package_plugins` gates its own contents on the feature and compiles to an empty
crate without it. That is not vanity: cargo features are additive, so a workspace member that enabled
`rattler_conda_types/experimental-virtual-package-plugins` unconditionally would switch the field on
for every build in the workspace.

`rattler_index` does not yet propagate the field: with the feature off it drops
`info.virtual_package_plugins` on a repodata round-trip, and with the feature on it writes an empty
map. Only a channel server publishing the field directly exercises the path today.

### What the CEPs Decide

Several accepted CEPs constrain this design. They are listed here with the decision each forced,
because more than one of them overturned a choice made before it was read.

| CEP | Status | Bears on |
| --- | --- | --- |
| [30](../../../../ceps/cep-0030.md) Virtual packages | Accepted | Which virtual packages exist and who owes them |
| [46](../../../../ceps/cep-0046.md) `__cuda_arch` | Accepted | Where a virtual package's information lives |
| [42](../../../../ceps/cep-0042.md) Channel relations | Accepted | Which channels a view spans, and who outranks whom |
| [26](../../../../ceps/cep-0026.md) Identifying packages and channels | Approved | Legal virtual package names |
| [33](../../../../ceps/cep-0033.md) Version literals and ordering | Accepted | Version comparison for `__rocm >=6.0` |
| [36](../../../../ceps/cep-0036.md) Package metadata files | Accepted | Whether the registration may live in `info` |
| [38](../../../../ceps/cep-0038.md) Channel-wide metadata | Accepted | Considered for the registration, rejected |
| [16](../../../../ceps/cep-0016.md) Sharded repodata | Accepted | The second place the registration is published |

**CEP 30 -- a plugin may replace a name, never remove it.** `combine` merges a view's plugin
results with the built-ins, and a built-in survives unless a plugin *produced* the same name --
which is not the same as a plugin having *claimed* it. A plugin registered for `__archspec` that
finds nothing has claimed the name and produced nothing, and dropping the built-in there would leave
the set without a name the CEP says MUST always be present, because a channel got its detection
wrong. The rule covers every built-in rather than only the always-present ones: CEP 30 pins when
each of its names must and must not appear, so a client that detected one is already meeting the
CEP, and a plugin contradicting that is asserting something the CEP does not let it assert.

**CEP 30 -- the built-ins belong to the client, not to a channel.** It requires every client to
support `__archspec`, `__cuda`, `__glibc`, `__linux`, `__osx`, `__unix` and `__win`, with
`__archspec` present *always* and `__linux`/`__unix` on matching platforms, and it names rattler as
a reference implementation. So the built-ins carry a `BuiltIn` source with no channel, are visible
in every view, and are never made conditional on which channels are configured. An earlier draft
attributed them to conda-forge and dropped them when conda-forge was absent; that would have broken
compliance outright. What the CEP does *not* say is that the client's own detection must be what
fills a name, which is why a plugin may still override one.

**CEP 46 -- the build string is not a place to put identity.** `__cuda_arch` puts the compute
capability in the *version* and requires its build string to be `0`; using the build string for the
device name was explicitly rejected so that nobody constrains on it. `build_string` in the report
protocol therefore exists for `__archspec`, which CEP 30 does require to carry a microarchitecture
there, and not as a general side channel. The examples in this document were wrong on this point
until the CEP was read.

**CEP 42 -- what a view spans, and which side wins.** Two relations, `base` and `overrides`, both
of which pull a channel into scope whether or not the user listed it, and whose directions are
opposite: a `base` is *higher* priority than the channel declaring it, a channel is *higher* than
what it `overrides`. An earlier attempt followed only `base` and had its direction reversed, which
let a channel that declared itself built upon another contradict that other's account of the system
it was built for.

The CEP's algorithm is implemented, not approximated: relations form a directed graph of priority
edges, and the chain is a topological sort of it. That matters beyond tidiness. Relations compose in
ways a linear walk cannot see -- a channel's base may itself override something, and that something
belongs in the order -- and three of the CEP's requirements only become checkable once the graph
exists:

- a **cycle** MUST be an error, and is `ViewError::Cycle`. A walk that stops at a channel it has
  already visited cannot tell a cycle from a diamond; the sort can.
- exceeding the **depth limit** SHOULD be an error, and is `ViewError::TooDeep`.
- declaring **both `base` and `overrides` for the same channel** MUST be an error, and is
  `ViewError::ContradictoryRelations` -- the channel has asked to be both above and below another.

All three previously stopped the walk silently and carried on with whatever had been collected.

Because these are the CEP's "MUST"s, each is tested rather than merely written. The graph walk is
`relation_chain`, split from `channel_view` so it takes a lookup from channel to relations instead of
a gateway: the crafted metadata that produces a cycle, a contradiction, or a chain past the depth cap
is then a table in a test rather than a set of fixture channels that would have to be served. Each
check was also verified by breaking it and confirming the matching test fails -- the depth cap and
the contradiction have no other coverage, and the cycle *algorithm* was already tested through
`topological_order` while the wiring that turns `None` into `ViewError::Cycle` was not.

**CEP 26 -- what a virtual package may be called.** Names must match
`^__[a-z0-9][._-]?([a-z0-9]+(\.|-|_|$))*$` and stay under 64 characters. Registrations are still
parsed leniently, so one malformed name cannot make a whole `repodata.json` unusable -- but that is
a parsing decision, not a licence: a name that does not meet CEP 26 will fail later, when it is used
as a package spec. Validating at parse time is the open item here.

**CEP 33 -- versions compare the conda way.** CEP 30 requires a virtual package's version to follow
CEP 33 whatever produced it, so a plugin's version string goes through the same `Version` type as
any package's, and `__rocm >=6.0,<7` orders as expected. Nothing special was needed; this is why
`Detected::version` is a `Version` and not a string.

**CEP 36 and 38 -- where the registration lives.** CEP 36 describes the `repodata.json` schema and
says of the `info` dictionary that "additional keys SHOULD NOT be present and SHOULD be ignored",
while *top-level* additional keys "MUST be allowed" and ignored when unrecognised. So
`info.virtual_package_plugins` is an extension CEP 36 discourages, and legitimising it needs a CEP
of its own -- exactly the path CEP 42 took to put `channel_relations` in the same dictionary. That
precedent is why `info` was chosen anyway: it is where channel-level metadata now lives, and older
clients ignore what they do not recognise.

CEP 38's `channeldata.json` was considered as the channel-wide home that would end the per-subdir
duplication, and does not fit. Its schema is an aggregation of *per-package* metadata with fixed
required keys, it is optional and documented as potentially unreliable, and reading it costs an
extra HTTP request -- which is the very thing CEP 42 gives as its reason for choosing `repodata.json`
instead. The channel-wide location this design wants does not exist yet.

**CEP 16 -- sharded channels.** The sharded index carries an `info` dictionary too, so the
registration is published there as well and a sharded channel is not a second-class citizen.

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

**Proposed, and a server-side policy question rather than part of this design:** that prefix.dev
validate at upload time that any virtual package a package depends on has a plugin registered in the
channel, and reject uploads that reference undefined ones. The argument for it is that a server
should not serve what it knows to be broken. The argument against is that conda already lets you
upload a package depending on something that does not exist, and this would be the one place that
rule is tightened.

Nothing in the client depends on the answer. A registration naming a package the channel does not
ship is reported as exactly that, and a dependency on an unregistered virtual package is simply
unsatisfiable -- the same as any other missing dependency.

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
needs a CEP: CEP 38's `channeldata.json` is the only channel-wide file that exists and does not fit
(see *What the CEPs Decide*).

**Lenient parsing.** Plugin and virtual package names are parsed without validation, so a channel
publishing a malformed name does not make the whole `repodata.json` unusable. CEP 26 does constrain
what a virtual package may be called -- `^__[a-z0-9][._-]?([a-z0-9]+(\.|-|_|$))*$`, under 64
characters -- and a name that breaks it fails later, when it is used as a package spec, rather than
at parse time.

### 2. Client-Side: Plugin Execution and Caching

*Implemented by `detect::detect_virtual_packages`, which composes the steps below.*

When a client resolves an environment and encounters a virtual package provided by a registered
plugin, it:

1. **Solves the plugin's environment** from the same channel, for the **host** platform. Detection
   inspects the running machine, so a plugin is never solved for a cross-compilation target.

   That solve uses **built-in virtual packages only**. Resolving a plugin's own dependencies is
   itself a solve against a channel whose plugin data is not available yet; restricting it to
   built-ins is what stops the recursion.

   The repodata is read **without the dependency closure** first. A detection plugin should be
   self-contained, and the common one is: a script with no dependencies at all. Fetching the closure
   to find that out costs a round trip the answer never needed, so it is only fetched when the
   plugin's own record names dependencies. `EnvironmentTimings::refetched_for_dependencies` says
   when that happened.

2. **Identifies the result by a hash over every package in that environment**, not by the plugin
   archive's own `sha256`. What a plugin reports depends on its dependencies, so its identity has to
   change when they do.

   This has an ordering consequence worth stating plainly: the hash is not known until the solve has
   happened, so a cache hit skips the install and the plugin run, never the solve. Measured against a
   local channel, that solve is under a millisecond -- it resolves one package, not an environment.

3. **Installs it** into a prefix of its own keyed by that hash, separate from the user's environment
   and reused across solves.

4. **Activates the prefix and runs the entry point** with the environment that produced. The
   activation happens in a shell of its own; the plugin does not. Running the file rather than a
   shell command avoids quoting surprises and keeps `activate.d` output off the stdout the report is
   parsed from, while the plugin still gets everything activation sets.

5. **Reads the report from stdout and checks it against the registration**: a verdict for every
   registered virtual package and nothing besides. A plugin claiming a name its channel never
   registered it for is rejected outright rather than filtered -- a channel promising one thing and
   shipping another is a bug worth surfacing, not something to paper over.

6. **Caches the verdicts** under the plugin's own cache policy, keyed by the same hash.

7. **Injects the results** into the solver's virtual package set alongside the built-ins, as
   `SourcedVirtualPackage`s -- but only the ones this plugin won (see *Conflict Resolution*):

```rust
pub struct SourcedVirtualPackage {
    pub source: VirtualPackageSource,
    pub package: GenericVirtualPackage,
}

pub enum VirtualPackageSource {
    BuiltIn,
    Plugin { channel: ChannelUrl, plugin: PackageName, environment: Sha256Hash },
}
```

   **The source is not decoration.** It says which channels a virtual package is visible to, which
   is what allows two independent channels to each answer for a name without one of them having to
   lose. A `BuiltIn` belongs to no channel and is visible everywhere -- CEP 30 makes the standard
   virtual packages an obligation of the *client*, not of any channel, so one cannot be missing
   however the channels are configured. A `Plugin` value is visible to the channel that registered
   it and to any channel reaching that one through a CEP-42 `base` chain.

   Carrying it on the value is the precondition for keeping those sets apart: without it there is
   nowhere to record which channel a given `__rocm` answers for, and two of them collapse into one
   the moment they are put in a list together.

   The solver itself still consumes plain `GenericVirtualPackage`s: it interns them by name and
   offers them as candidates, so scoping stays the caller's job for now.

### Factories

A caller assembling the virtual packages for a solve deals with two kinds of source: the ones this
client detects itself, which CEP 30 obliges it to offer, and the ones a channel's plugin reports.
They behave nothing alike -- one is a synchronous read of the running system, the other installs an
environment and starts a process -- but a caller should not have to know which it is holding.

`VirtualPackageFactory` is that common shape, and it splits the cheap question from the expensive
one:

```rust
#[async_trait]
pub trait VirtualPackageFactory {
    /// The names this source speaks for. Costs nothing.
    fn provides(&self) -> &BTreeSet<PackageName>;

    /// What is actually on this system. May be slow.
    async fn resolve(&self) -> Result<Vec<SourcedVirtualPackage>, FactoryError>;
}
```

That split is the whole point of calling it a factory rather than a list. A caller can see what a
factory *would* answer for and skip resolving one whose names nothing needs, instead of paying for
every plugin a channel happens to register. In both specializations `provides` is what the source
claims and `resolve` is what turned out to be there; a name reported absent simply does not come
back.

**`BuiltinVirtualPackages`** wraps `VirtualPackage::detect`. Its results carry `BuiltIn`, so they
belong to no channel and appear in every view.

Its `provides` is a fixed list, `STANDARDIZED_VIRTUAL_PACKAGES`, rather than the result of
detecting. That keeps the cheap contract honest, and it is also the right answer: `provides` means
"names this source speaks for", not "names it will find". `__cuda` is a name this client speaks for
on a machine with no GPU -- it looks, and reports absence. Deriving the list by detecting would
conflate the two and make `provides` cost exactly what it exists to avoid.

The list is written out because `rattler_virtual_packages` exposes no enumeration of the names it
detects; they live in its `From<VirtualPackage> for GenericVirtualPackage` impls. It now lives with
the factory, and `rattler virtual-packages` imports it from there rather than keeping its own copy.

**`PluginVirtualPackages`** is the other specialization: one per plugin a view resolved to, wrapping
`detect_virtual_packages`. Everything expensive about detection sits behind its `resolve`, so a
caller that does not need any of the names it offers never solves for the plugin, installs it or
runs it.

Its `provides` is what the plugin **won**, not everything its channel registered it for. Those
differ when another channel in the same view already speaks for one of its names: the plugin is
still held to reporting a verdict on that name -- the contract is between the plugin and its
channel, and losing a name does not excuse it -- but the verdict is dropped rather than offered.
`declared` and `provides` on `ResolvedPlugin` keep the two apart.

What every plugin in a run shares -- the gateway, the caches, the prefix root, the platform, the
timeout, and the single `now` they must all agree on -- is a `PluginContext`, passed once rather
than repeated per plugin.

The trait lives in `rattler_virtual_package_plugins` rather than in `rattler_virtual_packages`.
That crate is light, stable and entirely synchronous, and resolving a plugin is neither; defining
the trait there would make it async for the sake of an experiment. Everything here stays behind
`experimental-virtual-package-plugins`, and moving the trait down later is mechanical.

### Plugin Interface

Plugins are simple executables. **The entry point is the plugin package name**: package `cuda-detect`
ships an executable `cuda-detect`. Package names are unique within a channel and a JSON object cannot
repeat a key, so the entry point needs no separate metadata field, and conda already puts executables
in the environment's binary directory so no path needs declaring either.

**The report is one JSON object**, keyed by virtual package name:

```json
{
  "virtual_packages": {
    "__cuda": { "version": "12.4" },
    "__cuda_arch": { "version": "8.9" },
    "__rocm": null
  },
  "cache": {
    "ttl_seconds": 86400,
    "watch_paths": ["/sys/module/amdgpu/version"],
    "watch_env": ["CUDA_VISIBLE_DEVICES"]
  }
}
```

`null` is how a plugin says "not on this system". A plugin must give a verdict on every virtual
package its channel registered it for, so absence has to be something it can state -- and keying by
name is what lets a `null` carry it: the contract checks which *keys* are present, so a missing key
is silence and an explicit `null` is a verdict, with no deserializer subtlety in between.
`build_string` is optional and exists because `__archspec` carries its information there rather than
in the version: CEP 30 requires its build string to name a CPU microarchitecture, with the version
fixed at `1`. It is *not* how `__cuda_arch` works -- CEP 46 requires that one's build string to be
`0` and puts the compute capability in the version, having explicitly rejected using the build
string for device identity so that nobody constrains on it.

Keying by name also makes a duplicate verdict impossible to write down, and an object cannot repeat
its `cache` key, so neither needs detecting.

**Unknown keys are ignored**, at every level of the report, and logged at debug level. A plugin
written against a newer protocol than the client understands stays usable for the part the two
agree on. Rejecting the unknown would buy no safety here: the plugin is arbitrary code the client
has just run, and a protocol that cannot be extended without breaking older clients is worse than
one that can.

#### The process boundary

**The prefix is activated, and the plugin is invoked directly.** These are two separate things, and
keeping them separate is what makes both possible.

Activation runs in a shell of its own, via `Activator::run_activation`, which brackets the activation
script between two dumps of the environment and diffs them. What comes back is the set of variables
activation changed; what an activation script *printed* stays on that shell's stdout and is
discarded. The plugin is then started as a plain process with those variables applied, so nothing an
activation script says can reach the stream the report is read from, and no quoting question arises
about the plugin's own invocation.

The alternative -- running the plugin as a command inside an activated shell -- would put both on the
same stdout, which is what the earlier draft avoided by not activating at all. Not activating is the
wrong trade: a package may ship `activate.d` scripts or `state.json` variables that its programs
expect, and a plugin that alone does not get them behaves unlike every other program in a conda
environment.

The plugin therefore sees:

- **stdin** connected to `/dev/null`
- the parent's environment, plus everything activating the prefix changed -- which includes the
  prefix's binary directories at the front of `PATH` and `CONDA_PREFIX` pointing at it
- nothing else -- no arguments, no configuration file, no environment variable of its own

A failing activation script fails the detection rather than being skipped: a plugin run with a
half-applied environment would report something that depends on how far the script got.

Entry-point lookup happens *before* activation, since a registration naming a package that ships no
executable is worth saying at once. It uses the same directories activation puts on `PATH`
(`rattler_shell::activation::prefix_path_entries`), and on Windows also tries `.exe`, `.bat` and `.cmd`.

The contract:

- **stdin**: empty
- **stdout**: one JSON report as above
- **stderr**: diagnostic output, logged at debug level
- **exit 0**: the plugin ran and its output is authoritative
- **exit non-zero**: plugin failure, reported as a distinct error rather than an empty result. Every
  detection failure means the same thing for a solve -- none of this plugin's virtual packages can be
  used -- but they are kept apart so a caller can tell a broken plugin from a broken channel. Downgrading
  that to "treat them as absent" is the caller's decision, since a system without the hardware looks the
  same from here

This replaces the draft's three-way exit code (`0` present / `1` absent / `2+` failure). With several
virtual packages per plugin, presence is per verdict and cannot be carried by one exit status:
`__cuda` may be present while `__cuda_arch` is not.

**The run is bounded.** A plugin still running when its timeout elapses is killed and reported as an
error: detection happens on the way into a solve, and a plugin that hangs would hang the solve with
it.

**How long that is, is the caller's to decide, within a ceiling.** A plugin reading a version file is
done in microseconds; one connecting to a GPU has been measured at over a second on Windows. No
single number fits both, so the bound is a `RunTimeout` the caller passes in, and nothing a plugin or
a channel says can raise it. `rattler virtual-packages --detect --plugin-timeout <SECONDS>` is where
that surfaces today.

- **Default: five seconds.** One second was provably too short -- `__cuda` on Windows misses it by
  half a second, because it has to connect to the GPU. Five leaves room on cold hardware while still
  being short enough that a hung plugin is noticed rather than waited out.
- **Maximum: sixty seconds**, and a caller cannot pass it. `RunTimeout::new` clamps, and it is the
  only way to construct one, so there is no path in the API to an unbounded plugin run -- not by
  configuration, not by a caller's arithmetic. Asking for more is clamped and logged rather than
  refused: a caller asking for ten minutes has misjudged how long detection takes, and running with
  a minute is a better answer than an error about a number.

The asymmetry is what sets the ceiling: a timeout that is too short only skips one plugin, while an
unbounded one stalls every solve on the machine.

**One bound covers activation and the run together.** A caller allowing five seconds is allowing five
seconds to get an answer, not five for each half, so the timeout becomes a deadline before the shell
starts and the plugin gets what is left of it. A slow activation and a slow plugin are still told
apart in the error. An activation that runs out of time leaves its shell running: a blocking call
cannot be cancelled, and killing a half-finished activation script would be worse than letting it
finish into a result nobody reads.

A plugin producing more output than its registration can need is killed the same way:
**8 KiB per registered virtual package, plus two of headroom** -- one for the cache policy, one of
slack -- counted across stdout and stderr together. That is not tight. A verdict cannot get long,
since a package's name, version and build string together fit in an archive file name, which caps
them at under 250 bytes; what can get long is a watched filesystem path, at most `PATH_MAX` (4096
bytes on Linux), and 8 KiB fits one maximal path with every byte JSON-escaped. Unlike the timeout,
this is not configurable -- nothing has asked for it, and a plugin needing more is misbehaving rather
than unlucky.

**Every failure carries what the plugin said.** A killed plugin's stderr is collected as it arrives
rather than at the end, so a plugin that explains itself and *then* hangs is reported with the
explanation. It is kept out of the error message and offered as `DetectError::plugin_stderr`, since
it is the plugin's text rather than rattler's and can run to several lines.

**Validation is exact.** Every registered name gets a verdict; a name that was never registered, or
silence about one that was, each fail the run. A machine without the hardware is the ordinary case
and passes: every name still gets a verdict, they are simply all `null`.

Plugins can be compiled binaries, shell scripts, or anything else that fits in a conda package.
Keeping the interface this simple means detection for a new accelerator is a single small package with
a shell script that checks a few paths.

### Data Written to Disk

#### The plugin environment

An ordinary conda environment in a directory named after a hash of every package in it, under a root the
caller chooses:

```
exec/plugins/72029f5d5cf06962118b1863f7873826e48566014d12d0c6cf7dd7160964cea1/
├── .plugin-ready          <- sentinel: this prefix finished installing
├── CACHEDIR.TAG
├── bin/foobar-detect      <- the entry point that gets run
├── Scripts/               <- the Windows equivalent
└── conda-meta/
```

`.plugin-ready` is written last, and is what distinguishes a finished prefix from one left behind by an
interrupted install. It is empty; only its existence means anything.

The directory name is not the plugin archive's own hash. It covers every resolved package, because what a
plugin reports depends on what it runs with: a dependency upgrade, a downgrade, a dropped dependency or a
rebuild of the same version each have to produce a different environment rather than a stale one. The
input is sorted, so solver ordering does not affect it.

That has a consequence worth stating plainly: the hash is unknown until the solve has happened, so a cache
hit skips the install and the plugin run, never the solve. Against cached repodata the solve is the cheap
half.

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
      "source": {
        "plugin": {
          "channel": "file:///path/to/channels/virtual-package-plugins",
          "plugin": "foobar-detect",
          "environment": "72029f5d5cf06962118b1863f7873826e48566014d12d0c6cf7dd7160964cea1"
        }
      },
      "package": "__foobar=1.2.3"
    },
    {
      "source": {
        "plugin": {
          "channel": "file:///path/to/channels/virtual-package-plugins",
          "plugin": "foobar-detect",
          "environment": "72029f5d5cf06962118b1863f7873826e48566014d12d0c6cf7dd7160964cea1"
        }
      },
      "package": "__foobar_arch=0=gen4"
    }
  ],
  "expires_at": 1785501271,
  "watched": [
    { "path": "/sys/module/amdgpu/version", "modified_ms": 1785497600000 }
  ],
  "watched_env": [
    { "name": "CUDA_VISIBLE_DEVICES", "value": null }
  ]
}
```

- **`virtual_packages`** -- the verdicts, each carrying provenance. `package` is the
  `name=version=build_string` form, with the build string omitted when empty. `environment` identifies
  the plugin environment rather than the plugin archive, so it changes when a dependency of the plugin
  does. A built-in would be stored as `{"source": "built_in"}`, though nothing caches those: they are
  cheap to redetect and belong to no channel.
- **`expires_at`** -- seconds since the Unix epoch, derived from the `ttl_seconds` the plugin asked
  for. Every entry has one: **a plugin cannot ask to be cached forever.** What it can do is ask for
  longer or shorter, within a maximum, and add watches that expire the entry *sooner*.

  - A plugin that declares no policy, or declares one without `ttl_seconds`, gets **one hour**. The
    plugin that thought least about caching must not be the one whose answers are kept longest, and
    an hour costs at most one plugin run per hour -- against a prefix that already exists, that is
    milliseconds.
  - A plugin asking for more than **thirty days** gets thirty days. Without a ceiling a channel could
    pin a verdict on a machine for as long as it liked, and a driver upgrade would go unnoticed until
    someone cleared the cache by hand.
  - `ttl_seconds: 0` is honoured as written: a plugin saying "do not reuse this" is not overridden
    into the default.

  The field is nullable in the cache format because `rattler_cache` is a general store that knows
  nothing about the plugin protocol. The client never writes a null.
- **`watched`** -- one entry per path the plugin asked to have watched, recording its modification time in
  milliseconds since the epoch, or `null` if it did not exist. Either changing invalidates the entry, so a
  driver appearing counts as much as one being upgraded -- the case a TTL cannot catch.
- **`watched_env`** -- one entry per environment variable the plugin asked to have watched, recording its
  value or `null` if it was not set. This catches what no path can: the hardware still being there while
  the user has hidden it, as `CUDA_VISIBLE_DEVICES` does.

  These are read from the process that runs the plugin, not from the plugin's activated environment.
  The activated one is already covered by the environment hash the entry is keyed on; the process's own
  is what a user changes between two solves.

An entry is a miss if it is absent, expired, has a changed watch, **or fails to parse**. A corrupt
cache file costs one plugin run; failing a solve over it would be worse.

A changed *registration* does not invalidate an entry: the key covers the channel, the plugin and its
environment, but not the set of virtual packages the channel registered the plugin for. A channel that
narrows its registration while the plugin environment stays identical is therefore served the old
verdicts, unchecked against the new registration, until the TTL or a watched path catches up.

An absent result is cached like any other. "None of these are present on this system" is a real answer and
costs exactly as much to recompute.

The cache stores facts rather than protocol types: the caller turns a plugin's declared policy into an
expiry and a set of watched paths. That is also what lets the cache live in `rattler_cache` without it
depending on the crate that produces those results.

### Failure Modes

Every way detection can fail, and what each means. They all mean the same thing for a solve -- none of
this plugin's virtual packages can be used -- but they are kept distinct so a caller can say which
happened, and tell a broken plugin from a broken channel.

**Reading the registration**

| Condition | Outcome |
| --- | --- |
| No `virtual_package_plugins` in `info`, or an empty map | Not a failure: the channel registers no plugins |
| A malformed plugin or virtual package name | Not a failure: names are parsed unvalidated, so one bad entry cannot make a channel unusable |
| A registered name that cannot be used as a package spec | Error, naming the offending registration |

**Preparing the environment**

| Condition | Outcome |
| --- | --- |
| The channel's repodata cannot be fetched | Error, wrapping the gateway's own |
| This system's *built-in* virtual packages cannot be determined | Error. The plugin solve needs them, so a machine whose libc or CPU cannot be identified cannot run plugins either |
| The channel registers the plugin but ships no such package | Error saying exactly that, checked before the solve rather than inferred from its failure |
| The plugin's dependencies cannot be resolved | Error. This is also where a plugin depending on a *plugin-provided* virtual package lands, since the solve sees built-in ones only -- deliberately, as that is what stops the recursion |
| The environment cannot be installed | Error, wrapping the installer's own |
| The `.plugin-ready` sentinel cannot be written | Error. The prefix is left in place but is not treated as ready, so the next attempt reinstalls rather than running a half-installed plugin |

**Running the plugin**

| Condition | Outcome |
| --- | --- |
| No executable named after the plugin package in the prefix | Error naming what was looked for and where, raised before anything is activated |
| An activation script fails | Error, carrying the script's own output. The plugin is not run: what it reported would depend on how far the script got |
| Activation is still running when the deadline passes | Error naming the prefix and the budget, distinct from the plugin timing out. Its shell is left to finish |
| The executable exists but cannot be started | Error, wrapping the OS error |
| Exit code other than `0`, or death by signal | Error carrying the code and the plugin's stderr. The plugin ran and said no; whether to downgrade that to "these virtual packages are absent" is the caller's decision, since a machine without the hardware looks the same from here |
| Still running when the caller's timeout elapses | Error carrying the timeout and the stderr it wrote before it hung. The plugin is killed; a hanging plugin must not hang the solve with it |
| More output than the registration can need | Error carrying the budget and the stderr written so far. The plugin is killed once it exceeds one verdict's worth per registered virtual package plus headroom (see the contract) |

**Reading the report**

| Condition | Outcome |
| --- | --- |
| Not a JSON object, or a verdict without a version | Error carrying serde's line and column, so it can be found in a log |
| An unknown key, at any level | Not a failure: ignored, and logged at debug level so a misspelled key is findable |
| Nothing on stdout | Error of its own. A plugin that exits zero having said nothing has usually written its report to the wrong stream, and saying that beats reporting it as silence about every registered name |

**Checking the contract**

| Condition | Outcome |
| --- | --- |
| A virtual package the channel did not register | Error listing every offending name |
| Silence about one the channel did register | Error listing every missing name |

**Caching the result**

| Condition | Outcome |
| --- | --- |
| The key cannot be used as a path component | Error. The plugin name comes from channel metadata, so this is the check that stops a channel writing outside the cache directory |
| The cache cannot be read or written | Error |
| An entry exists but does not parse | Not a failure: treated as a miss. The cost is one plugin run, where failing a solve over a corrupt cache file would be worse |

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
plugin wins happens a layer up, in `resolve`.

### Views, and Resolution Within Them

A **view** is one channel together with every channel CEP-42 relates it to. It is the scope a
virtual package lives in, and it is what `resolve::resolve_views` resolves: one result per view,
never one global answer.

**Unrelated channels do not compete.** Two channels with no relation between them may each register
a plugin for `__rocm`, and both answer -- each within its own view. A channel outside the chain
cannot say anything about the packages this one serves, so by definition it contributes nothing
here. There is no contest, no loser, and nothing consults the order the channels were listed in.
This is what carrying the source on each virtual package buys: two `__rocm`s can coexist because
each records which channel it answers for.

**Inside a view, CEP-42's priority decides**, and both of its relations are followed:

| Relation | Brings into scope | Who wins a contested name |
| --- | --- | --- |
| `base: X` | X and its chain | **X** -- a base is higher priority than the channel declaring it |
| `overrides: X` | X and its chain | **the declaring channel** -- it supersedes what it overrides |

Both pull a channel in whether or not the user listed it. CEP-42's own example makes that explicit
for `overrides`: `conda install -c conda-forge/label/rc some-package` resolves to the label channel
*and* `conda-forge`, because "for packages that the label does not provide, the main channel serves
as a fallback". A channel whose packages a solve will see must be a channel whose virtual packages
it can see too, or a package from the fallback would depend on a name nothing provides.

The chain is therefore built in the order CEP-42 spells out -- bases ahead of the channel declaring
them, overridden channels behind it -- and the first claimant along it wins. A channel declaring
`base: conda-forge` and `overrides: my-hotfixes` yields `[conda-forge, itself, my-hotfixes]`.

**Overriding an upstream virtual package means declaring `overrides`, not `base`.** These say
opposite things about priority, and the difference is the whole point of having two relations: a
channel that builds *on* conda-forge defers to it, while one that supersedes conda-forge outranks
it. Virtual packages follow the same direction as packages rather than inventing their own, so
there is one rule to learn.

Three further things, each deliberate:

**Subdirs of one channel are folded, not compared.** A channel repeats its registration in every
subdir, so the same plugin seen twice is one plugin, registered for the union of what its subdirs
said.

**Two plugins in one channel claiming one name is an error**, not a contest. Nothing breaks that
tie, and a channel registering both `a-detect` and `b-detect` for `__rocm` is contradicting itself.
Detection fails rather than guessing, before any of that channel's plugins runs.

**A plugin can lose one name and keep another.** It still runs, for what it won, and it is still
held to *everything* its channel registered it for: the contract is between the plugin and its
channel, and losing `__cuda` to a higher-priority channel in the view does not excuse the plugin
from giving a verdict. The verdict is discarded rather than never asked for. A plugin that loses every name is not
run at all, but is still returned in `ResolvedView::shadowed`, so a caller can say a registration was
skipped and which channel took it.

**Built-ins are the weakest source.** A plugin claiming a name the client also detects overrides it.
CEP 30 requires such a name to be *present* and does not dictate that the client's own detection is
what fills it, so a channel that knows better about `__cuda` may say so.

`resolve_channel_relation` is exported so a caller resolving a CEP-42 `base`/`overrides` reference
outside a query resolves it the same way the query path does. That validation stops malicious metadata
from pointing at attacker-controlled URLs, so a second implementation would be a place for the two to
drift apart.

For manual inspection, `rattler virtual-packages -c <channel>` walks the `base` chain and prints every
registration the channel can see, keyed by virtual package, warning where one shadows a name clients
detect themselves or a name an inherited channel already registers.

Adding `--detect` instead runs each registered plugin and reports what it found, marking whether the
answer was cached or freshly produced:

```
🔌 file:///.../virtual-package-plugins/ [linux-64]
  ✔ foobar-detect (from cache)
      __foobar=1.2.3
      __foobar_arch=0=gen4
  ✖ rocm-detect (skipped)
      the channel registers the plugin 'rocm-detect' but provides no such package
  • __unix=0=0 (built in)
  • __linux=7.0.11=0 (built in)
  • __glibc=2.43=0 (built in)
  • __archspec=1=zen5 (built in)
```

The built-ins come last because which of them survive is only known once the plugins have run: a
plugin that *claimed* a name but found nothing has not replaced it. Each view reports its whole set,
built-ins included, because that is what a view *is* -- the virtual
packages a solve against that channel would see. The built-ins come from `BuiltinVirtualPackages`,
resolved once for the run since they do not vary by channel and detecting them can mean a driver
query. A built-in whose name a plugin in that view claims is not printed: the plugin overrode it.

A plugin that fails is reported and skipped rather than aborting the run: one broken plugin should not
hide what the others found. Where a plugin fails after starting, its own stderr is printed beneath the
error -- usually the more specific account of the two. `--detect` takes a single platform, since
detection inspects the running machine. Unlike the listing mode it does not walk the `base` chain:
only plugins the named channels register themselves are run.

Conflicts are resolved across every named channel before anything runs, in the order the channels
were given, so a registration a higher-priority channel already speaks for is reported rather than
run:

```
🔌 file:///.../virtual-package-plugins-base/ [linux-64]
  ✔ cuda-detect (ran the plugin)
      __cuda=12.4
🔌 file:///.../virtual-package-plugins-derived/ [linux-64]
  ○ vendor-cuda-detect (not run)
      __cuda is provided by file:///.../virtual-package-plugins-base/
```

A registration naming a package the channel does not have is reported as exactly that, rather than as a
failure to resolve dependencies -- it is a disagreement between a channel's metadata and its packages, and
no amount of dependency resolution will fix it. Where a failure does come from further down, the whole
chain of causes is printed, not just the outermost message.

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

```json
{
  "virtual_packages": {
    "__cuda": { "version": "12.4" },
    "__cuda_arch": { "version": "8.9" }
  }
}
```

(Both as their CEPs require: `__cuda` the driver's CUDA version, `__cuda_arch` the compute
capability in the *version*, with no build string.)

On a machine with no NVIDIA driver the same plugin exits 0 and reports both as `null` -- it still has
to account for every name it was registered for. Under the draft's original
one-plugin-per-virtual-package scheme this needed two packages, or one package with two entry points
repeating the same driver query.

**A plugin is never asked for a subset.** `__cuda_arch` costs a driver connection, so it is fair to
ask whether a client that only needs `__cuda` could say so and save the work. It cannot, and
deliberately: grouping names under one plugin *is* the statement that they share their expensive
work, and splitting the cost is what separate plugin packages are for. A per-run subset would also
make the caching worse rather than better -- the cache key would have to cover the requested subset,
so asking for `__cuda` and later for both would query the driver twice over.

A channel that would rather pay twice can ship two plugins. `__cuda` and `__cuda_arch` under one is
the arrangement that motivated grouping in the first place: one driver connection, two answers.

### Settled Decisions

1. **Registration is keyed by plugin package name**, mapping to the list of virtual packages it
   provides.
2. **The entry point is the plugin package name.** No entry-point field in the metadata; uniqueness
   within a channel comes for free.
3. **No package-record changes.** The registration lives entirely in `info`; `PackageRecord` and
   `index.json` are untouched, so a client learns what a plugin provides without fetching the plugin's
   record first.
4. **No version constraints in the registration.** Bare package name, latest version.
5. **The gateway reports, `resolve` decides.** Registrations come back per subdir with duplicates
   intact. Resolution is per view -- a channel plus everything CEP-42 relates it to -- and within a
   view the CEP's own priority decides: a `base` outranks the channel declaring it, a channel
   outranks what it `overrides`. Unrelated channels never compete, being separate views.
6. **Plugin identity is (channel, package name)** for conflict resolution, and a hash over the whole
   solved plugin environment for caching, so it changes when a dependency does.
7. **The report is one JSON object keyed by virtual package name**, with `null` for absent. Absence
   is stated explicitly, never implied by omission, and a duplicate verdict cannot be expressed.
8. **Unknown keys are ignored, not rejected**, so the protocol can grow without breaking older
   clients.
9. **Validation is exact**: a verdict per registered name, nothing else, silence included.
10. **The prefix is activated in a shell of its own, and the plugin then runs directly** with the
    environment that produced -- rather than as a command inside that shell.
11. **Detection is host-only**: the plugin environment is solved for the current platform.
12. **The plugin declares its own cache policy** (`ttl_seconds`, `watch_paths`, `watch_env`) in its
    report, within bounds the client sets: every entry expires, in an hour by default and thirty
    days at the most.
13. **Results carry their source** as `SourcedVirtualPackage` -- `BuiltIn`, or `Plugin` with the
    channel, plugin package and environment hash. The source is what scopes a virtual package to the
    channels that can see it; the solver still receives plain `GenericVirtualPackage`s.
14. **A plugin is never asked for a subset of what it was registered for.** Grouping names under one
    plugin is the statement that they share their expensive work; splitting the cost is what
    separate plugin packages are for.
15. **Everything is behind an experimental cargo feature** and invisible when it is off.

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
   A channel-wide location would fix it, and none suitable exists: CEP 38's `channeldata.json`
   aggregates per-package metadata, is optional, is documented as unreliable, and costs an extra
   request. This needs a CEP, which would also legitimise the `info` key under CEP 36.
7. **Channel relations and overriding.** Whether a channel may register a plugin for a virtual package
   its base channel already covers (e.g. a private channel overriding `__glibc`), and whether such an
   override should affect the base channel. Shadowing along the `base` chain is now detected and
   reported, including registrations that shadow a virtual package clients detect themselves, but
   nothing acts on it: the policy is still open.
8. **Plugin dependencies.** Detection plugins should be self-contained, but if one needs a shared
   library to query a driver API, those deps are resolved from the same channel. Solving the plugin
   environment with built-in virtual packages only (see above) breaks the bootstrap recursion; the
   remaining risk is ordinary dependency conflict.
9. ~~**Versioning semantics.**~~ Settled by CEP 33 via CEP 30: a virtual package's version follows
   conda version ordering whatever produced it, so `__rocm >=6.0,<7` works because the value goes
   through the same `Version` type as any package's.
10. **wheelnext.** Worth looking at closely -- they are solving essentially the same problem.
11. **Concurrent detections.** Nothing serializes two processes preparing the same plugin
    environment: the sentinel keeps a half-installed prefix from being *used*, not two installers
    from interleaving. The package cache below it locks; the prefix itself does not.

---

## Review Comments

Comments left on this document during the review of 2026-07-31, grouped into threads, with the
resolution of each. Quotes from Bas Zalmstra were left as replies quoting his inline comments, and
are attributed to him here. Each thread says what changed and where; the body above describes the
state after everything marked **done**.

### 1. Output format: JSON Lines or one JSON object -- done

> **Wolf Vollprecht** (on *malformed*) -- Why not just normal JSON? JSON object or array would be
> much easier IMO.
>
> **Tobias Hunger** -- Because I can parse an object on each line break and do not need to wait for
> the entire thing to finish. But yes: an object works as well. Should be easy to change.
> I originally did not have a timeout for the plugin and thought it might be better to get results
> as they come in instead of having to wait for the entire thing to be written. I added a timeout of
> 1s later, so that is a non-issue now.

**Done: switched to one JSON object.** Streaming was the only argument for JSON Lines and the
timeout removed it. The replacement keys the verdicts by virtual package name, which makes a
duplicate verdict impossible to express rather than something the contract has to catch:

```json
{
  "virtual_packages": {
    "__cuda": { "version": "12.4" },
    "__cuda_arch": { "version": "8.9" },
    "__rocm": null
  },
  "cache": { "ttl_seconds": 86400, "watch_paths": ["/sys/module/amdgpu/version"] }
}
```

`null` is `absent`. It works here where it did not work per line: the contract checks the *key set*
against the registration, so a missing key is still silence and an explicit `null` is still a
verdict -- the distinction serde could not carry when it was a missing field versus a null field.
The `present`/`absent`/`cache` line kinds all disappear with it, and so does the duplicate-cache-line
error, since an object cannot have two `cache` keys.

`PluginLine`, `PluginOutput`, `Verdict` and `parse_output` collapse into `PluginReport` and
`parse_report`; `ContractViolation::Duplicated` is gone, since nothing can express a duplicate any
more. One case is new: a plugin that exits zero having written nothing is now its own error rather
than silence about every registered name, because writing the report to stderr by mistake is a
mistake worth naming.

The fixture plugin package is rebuilt from
`test-data/channels/virtual-package-plugins/regenerate.py`, which also updates the hashes in
`info/paths.json` and `repodata.json`. It was hand-built before, and getting the protocol wrong in a
binary fixture is expensive to notice.

### 2. Rejecting unknown line kinds and unknown fields -- done

> **Wolf Vollprecht** (on *known line kinds and unknown*) -- This seems a bit counter to what we do
> in Pixi. I don't really understand the reasoning. We do this all the time and should be able to
> work around this if it is really an issue.
>
> **Tobias Hunger** -- I asked Claude to be paranoid when doing this. A plugin might be malicious
> after all. We can lift that restriction if we can all agree that is not a problem.

**Done: lifted.** Strictness buys nothing against a malicious plugin -- it already runs arbitrary
code -- and it costs forward compatibility: an older client would reject a plugin written for a
newer protocol outright rather than ignoring the part it does not understand. Unknown keys are now
ignored and logged at debug level, at both levels of the report, so a misspelled `ttl_seconds` is
still findable instead of silently meaning "no expiry".

What stays strict is the contract: a verdict about a virtual package the channel never registered the
plugin for is still an error, because that is a channel disagreeing with itself rather than a version
skew.

### 3. Conflict resolution across channels -- done, then superseded

> **Wolf Vollprecht** (on *Deliberately not done -- reported as declared, caller decides*) --
> I think we said "highest priority channel wins"?
>
> **Tobias Hunger** -- Yes, we did and that is the implementation I will add next. I wanted to see
> the plugins running and caching their results (so we do not rerun them all the time!) first.

**Done.** `RepoDataQueryOutput::virtual_package_plugins` already returned registrations in resolved
channel-priority order, so the new `resolve` module walks them in that order and gives each virtual
package to the first channel that claims it. Two plugins *within one channel* claiming the same name
are an error -- there is no priority to break that tie, and it is a mistake in the channel.

Two cases the thread did not cover, decided while building it:

- A plugin that loses *some* of its names still runs for the rest, and is still held to everything
  its channel registered it for. The contract is between the plugin and its channel; the verdicts
  for lost names are simply discarded.
- A plugin that loses *all* of them is not run, but is still returned in `Resolution::shadowed`, so
  `--detect` can say the registration was skipped and which channel took it. Dropping it silently
  would leave a user wondering where their plugin went.

`rattler virtual-packages --detect` now resolves across all the channels it was given before running
anything, rather than running each channel's registrations independently.

**Superseded since.** Highest-priority-channel-wins was the wrong frame: it made two channels with
no relationship to each other fight over a name, with list order picking the winner. Channels are
independent, and each sees its own plugins, so resolution is now per *view* -- a channel plus its
CEP-42 relations. Inside a view the CEP's priority decides -- a `base` outranks the channel
declaring it, and a channel outranks what it `overrides` -- while across views nothing competes.
An earlier attempt had the `base` direction backwards; see *Views, and Resolution Within Them*. Whether a plugin-provided
name may shadow a *built-in* one is a separate question and stays open (see below).

### 4. The one-second timeout -- done

> **Tobias Hunger** -- I reject a plugin now if it reaches a timeout of 1s or produces too much
> output (8k per expected line + 16k). We should probably discuss these limits. 1s might be a bit
> short on slow HW. Maybe we should make that configurable (up to a certain limit)?
>
> **Bas Zalmstra** -- On Windows I have seen the `__cuda` virtual package take 1.5 seconds to load.
> This is because it has to connect to the GPU.
>
> **Tobias Hunger** -- Yes, we need to discuss this: either make it configurable within certain
> limits or just increase the default value.

**Both.** One second is provably too short: the flagship use case misses it by half a second on
Windows.

Done: the bound is now a `RunTimeout` the caller passes in, through `DetectOptions` and on to
`run_plugin`, rather than the `RUN_TIMEOUT` constant it used to be. `rattler virtual-packages
--detect --plugin-timeout <SECONDS>` exposes it; on the pixi side it would be a config key. Nothing
a plugin or a channel writes can influence it -- only the caller.

The numbers are settled too: **five seconds by default, sixty at most**. The default had to clear
the case Bas measured, and five seconds leaves a GPU query room on cold hardware. The maximum is
enforced by the type -- `RunTimeout::new` clamps and is the only constructor -- so no configuration,
and no caller's arithmetic, can produce a longer one.

Activation did not get a budget of its own in the end (see thread 7): it runs inside the same
deadline, so five seconds means five seconds to an answer rather than five per half.

### 5. Output limits -- done

Same thread as above: **8 KiB per registered virtual package plus two lines of headroom**, counted
across stdout and stderr together.

**Kept, and not made configurable.** With one JSON document the "per line" framing goes away, but
the size does not need to change: the reasoning behind 8 KiB was one maximal `PATH_MAX` watch path
with every byte JSON-escaped, and the format does not affect that. Unlike the timeout it gets no
knob -- nobody has asked for one, and a plugin that needs more is misbehaving rather than unlucky.
`MAX_LINE_BYTES` is renamed `MAX_BYTES_PER_VIRTUAL_PACKAGE`, which is what it always meant.

The defect in the same thread is fixed: `runner.rs` used to drop the captured stderr on the timeout,
over-budget and read-error paths, so exactly the failures a plugin author most needs to debug were
the ones that said nothing. The output buffers now outlive the collecting, so a plugin that
complains and *then* hangs is reported with the complaint. Nothing is truncated: the budget already
caps how much there can be.

### 6. Who consumes the provenance -- done

> **Bas Zalmstra** -- Then who consumes this provenance?
>
> **Tobias Hunger** -- We can end up with several channels having the "same" plugin. I added this to
> be able to distinguish between those.

**No change, and the consumer now exists.** Conflict resolution (thread 3) is what reads it: which
channel a verdict came from is exactly what decides whether it is used or discarded, and
`Resolution::shadowed_by` names the winner so a skipped registration can be explained rather than
just omitted.

### 7. Activation scripts -- done

> **Bas Zalmstra** -- I don't really see the downside of running the activation scripts. We always
> do this and have more than enough experience with quoting in other places. I think it would be
> confusing if they are not run.
>
> **Bas Zalmstra** -- We could first run the activation scripts, then print a marker, and then
> directly execute the plugin executable. It should then be easy to distinguish one from the other.
>
> **Tobias Hunger** -- You are right: we should. Activation scripts should not slow down the overall
> plugin discovery: they get validated for that. We can also add a timeout when running the script,
> just to be sure.
>
> **Tobias Hunger** (on the marker) -- That would probably be safer: who knows what people will use
> to write plugins with.

**Done: they are run.** The marker approach already existed in rattler and did not even need the
plugin's own stdout to be involved: `Activator::run_activation`
(`crates/rattler_shell/src/activation.rs:617`) brackets the activation script with a separator, diffs
the environment before against after, and hands back the changed variables. The plugin is still
spawned directly, now with that environment applied -- so activation output lands on the activation
shell's stdout, never on the stream the report is read from, and the plugin still never runs under a
shell.

The new `activation` module holds it. A failing activation script fails the detection rather than
being skipped, since a plugin run with a half-applied environment reports something that depends on
how far the script got.

Bounding it needed a decision the thread did not settle: whether activation gets a budget of its
own. It does not -- it runs inside the run's, which becomes a deadline before the shell starts. A
caller allowing five seconds is allowing five seconds to get an answer, and two separate five-second
bounds would quietly mean ten. The error still says which half ran out.

The cost is one extra shell per plugin run, on a cache miss only. `run_activation` is blocking and
cannot be cancelled, so an activation that overruns leaves its shell to finish into a result nobody
reads.

### 8. Watching environment variables -- done

> **Bas Zalmstra** -- We should also allow watching environment variables probably.
>
> **Tobias Hunger** -- Yes, we should.

**Done.** `watch_env` sits alongside `watch_paths` in the cache policy, and `watched_env` in the
cache entry records each variable's value -- or its absence, the same way `WatchedPath` records a
missing file. Setting, changing or unsetting one invalidates the entry.

The variables watched are the ones in the process that runs the plugin, not in its activated
environment: those are what a user changes between two solves, and the activated ones are already
covered by the environment hash the entry is keyed on.

`CachedDetection::record` now takes a `WatchList` rather than a list of paths, so the next kind of
watch does not change every caller again.

One thing worth knowing for anyone extending this: `WatchedEnv` reads through an injectable lookup,
and the tests use their own rather than the real environment. Setting a variable to test it would
mutate state every other thread in the test binary shares, which is how an unrelated test starts
failing once a week.

### 9. The cost of solving and installing a plugin environment -- measured, partly addressed

> **Bas Zalmstra** -- This part worries me. Because it will involve extra roundtrips to the server
> and a complete solve. That adds significant overhead, but we should measure that.
>
> **Tobias Hunger** -- How would you expect to solve this? It's a normal package, can we do a
> fastpath install? That would require us to limit the features the package is allowed to use,
> wouldn't it?

**Measured first.** The four stages -- repodata, solve, install, run -- are timed and travel with the
result as `DetectionTimings`. `rattler virtual-packages --detect --timings` prints them.

Against the local fixture channel, with the package cache warm and the prefix already installed:

```
repodata 1.5ms, solve 0.8ms, install 0ns, run 10.7ms      (verdicts computed)
repodata 1.5ms, solve 0.6ms, install 0ns, run 0ns         (verdicts from cache)
```

Two things that says. The solve Bas worried about is **sub-millisecond** here -- it resolves one
package against one channel, which is nothing like a real environment solve. And the plugin *run*,
at ~11ms, is the largest stage, most of it the activation shell added in thread 7. That is the
honest local number; a remote channel would move the weight into repodata, which is where the
measurement needs repeating.

**Fixed: the second repodata round trip for the common plugin.** `ensure_plugin_environment` used to
ask for the plugin's whole dependency closure. It now asks for the plugin alone, and only goes back
for the closure when the plugin's own record names dependencies -- which a self-contained detection
plugin does not. Nothing about plugin packages is restricted: one with dependencies still works, it
just costs the extra query, and says so in its timings.

**Not done, and now clearly not urgent:**

- *Passing the caller's already-fetched `RepoData` in* rather than querying at all. This only pays
  off once a solve integration exists to pass it, and the measurement says the query is not what
  hurts.
- *A pre-solve cache level* keyed on (channel, plugin, repodata revision), so a warm run skips the
  solve too. At sub-millisecond, the solve does not justify it yet.

### 10. Upload-time validation of virtual package dependencies -- documented, still theirs to decide

> **Bas Zalmstra** -- I don't know if we need to verify this explicitly. You can also upload packages
> that reference nonexisting packages.
>
> **Tobias Hunger** -- The server side should IMHO verify that: a server should not serve things it
> knows to be broken. The client handles this fine already: it errors out on the plugin (and all the
> virtual packages it is supposed to provide).

**Unresolved, and nothing in this branch depends on it.** It is a prefix.dev policy question, not a
client one. The client behaviour is already the conservative one either way: a registration naming a
package the channel does not ship is reported as exactly that
(`EnvironmentError::PluginPackageMissing`), and a dependency on an unregistered virtual package is
simply unsatisfiable. The section above now describes the validation as a proposed server-side
policy, with both arguments, rather than as settled design.

### 11. Capturing stderr -- done

> **Bas Zalmstra** -- Do we capture stderr, so we can pass that along to the user?
>
> **Tobias Hunger** -- I need to check but I think it gets captured. It is counted towards the
> output limit for sure :-)

**It was captured but not passed along.** It lives in `PluginRun::stderr`, is logged at debug level
on a successful run, and is carried in `DetectError::PluginFailed` on a non-zero exit -- but it was
dropped on the timeout, over-budget and read-error paths, and no caller ever showed it.

Both halves are fixed now. Every runner error carries the stderr collected before the failure, and
`DetectError::plugin_stderr` offers it whatever the failure was, so `--detect` can print a plugin's
own account of itself under the error rather than only rattler's summary of it.

### 12. Telling a plugin which virtual packages to resolve -- rejected, documented

> **Bas Zalmstra** -- Should we instruct the plugin which plugins we want it to resolve? There is
> overhead in resolving virtual packages. For instance `__cuda_arch` requires connecting to the GPU
> which is expensive.
>
> **Tobias Hunger** -- I expect that to be two plugins then. I added the "a plugin can have several
> virtual packages" mechanism so we do not end up having plugin A do some expensive operation and
> report one of two facts that depend on that operation, and then have to run plugin B, which
> repeats that expensive operation to report the other fact.

**Rejected, deliberately.** Splitting cost is what separate plugin packages are for; grouping names
under one plugin is the statement that they share their expensive work. A per-run subset would make
the caching worse, not better: the cache key would have to cover the requested subset, so asking for
`__cuda` and then for both would run the plugin twice over the same driver query.

`__cuda` and `__cuda_arch` specifically are the case Bas names, and they are also the case that
motivated grouping -- one driver connection answering both. Splitting them into two packages is
available to a channel that would rather pay twice. The reasoning is written into the body, under
*One Plugin, Several Virtual Packages*, since it is a question that will come back.

### 13. How the cache policy is determined -- done

> **Bas Zalmstra** -- How is the plugin's cache policy determined?
>
> **Tobias Hunger** -- Plugins can pass the cache policy along with their results.

**Already the design, and the hole in it is fixed.** A plugin that declared no policy used to get
`CachePolicy::default()` -- no TTL and no watches, so cached forever, for the plugin that thought
least about caching.

Every entry now has an expiry, and "cache these forever" is not something a plugin can ask for:

- no policy, or a policy without `ttl_seconds`, means **one hour**
- more than **thirty days** is clamped to thirty days, so a channel cannot pin a stale verdict on a
  machine and leave a driver upgrade unnoticed until someone clears the cache by hand
- `ttl_seconds: 0` is honoured as written; a plugin saying "do not reuse this" is not overridden

`watch_paths` and `watch_env` still only make an entry expire *sooner*, never later.

### Numbers chosen, and open to argument

Four numbers were picked while implementing this rather than agreed in the review. Each is a
one-line change if the answer should be different:

| Number | Value | Why |
| --- | --- | --- |
| Default plugin timeout | 5s | Clears the 1.5s `__cuda` case Bas measured, with room on cold hardware |
| Maximum plugin timeout | 60s | Past anything detection should need, so hitting it means hung |
| Default cache TTL | 1 hour | Bounds staleness at roughly one plugin run per hour |
| Maximum cache TTL | 30 days | Long enough for something that never changes, short enough to self-heal |

### Still open

Whether a plugin-provided virtual package may shadow one clients detect themselves (`__cuda`,
`__glibc`). Shadowing between *channels* is now resolved by priority, but shadowing a **built-in** is
a different question: nothing acts on it, and `rattler virtual-packages` only warns. This is open
question 7 above.

### Order of work

| Step | Change | Threads | State |
| --- | --- | --- | --- |
| 1 | One JSON object, lenient about unknown keys | 1, 2 | Done |
| 2 | Caller-set timeout; stderr on every failure path | 4, 5, 11 | Done |
| 3 | Run activation scripts, bounded, via `run_activation` | 7 | Done |
| 4 | `watch_env` | 8 | Done |
| 5 | Conflict resolution: highest-priority channel wins | 3, 6 | Done |
| 6 | Measure the four stages, then the no-dependency fast path | 9 | Done |
| 7 | Document the rejected and deferred decisions in the body | 10, 12 | Done |
| 8 | Raise the default timeout, with a ceiling no caller can pass | 4 | Done |
| 9 | A default cache TTL for a plugin that declares none | 13 | Done |
