# JABS: compatible Minecraft execution with incremental native compilation

Status: agreed development plan; capabilities and performance targets below are not claims of completed implementation.

## Goal and development strategy

Build a portable Minecraft launcher, optimization pipeline, and native execution runtime. Preserve original game behavior and existing mod JAR workflows while improving frame consistency, simulation performance, startup, and memory use.

Use two development tracks:

1. **Playable track:** run Minecraft through its existing JVM and mod loader, introducing verified optimizations and native acceleration where measurements justify them.
2. **Native track:** develop a standalone runtime with bytecode interpretation and incremental compilation to C. Move playable profiles onto it only after they pass compatibility and performance gates.

The transitional JVM is an explicitly reported execution backend, not evidence that native compatibility is complete. Native execution becomes the default profile by profile, rather than through a single global cutover.

“All Minecraft features and sensible platforms” is the long-term compatibility objective. Every release publishes the exact versions, loaders, platforms, and features tested. No blanket compatibility or speed claim precedes evidence.

## Architecture and interfaces

### Implementation and ownership

- Use **Rust** for the launcher CLI, JAR/class reader, typed intermediate representation, compiler orchestration, cache, and test tooling.
- Use **C11** for the standalone runtime and generated native code, with OS-specific threading, memory, filesystem, networking, and dynamic-library adapters.
- Use Clang as the initial native compiler. Add other C toolchains through the same build interface after the first backend is validated.
- Use small Java components for integration with existing launchers, mod loaders, diagnostics, and transformation capture.
- Preserve the existing C experiment. Begin implementation in new project modules.
- Keep Minecraft JARs, decompiled sources, assets, and derived game binaries local. Distribute JABS tooling and original optimization transformations.
- Review dependency licenses before incorporation; retain required notices and source obligations. Do not select a Java class-library implementation without resolving its redistribution requirements.

### Playable track

The existing loader remains responsible for mod discovery, dependencies, class-loader boundaries, access changes, and Mixins.

Use supported loader integration where available and a Java instrumentation agent for observation and narrowly scoped transformations. Verify transformation ordering with fixtures before enabling optimizations.

Initially:

- Keep Java objects owned by the JVM.
- Accelerate sufficiently large operations through JNI only when total elapsed time—including crossings and data movement—improves.
- Avoid translating every field access into JNI calls.
- Apply Java/bytecode optimizations when they outperform native offloading.
- Keep each optimization individually disableable and report which ones activated.

Decompile pinned versions for research and performance experiments. Production preparation primarily transforms bytecode or JABS IR. Source patches are allowed only for explicitly pinned versions whose reconstruction and build are reproducible.

### Standalone native track

Build a runtime supporting interpreted and compiled methods within **one object model and heap**:

- Java class identity includes the defining class loader.
- Method dispatch supports interpreted and native entrypoints.
- Preserve class initialization, exception ordering, integer behavior, floating-point requirements, array checks, monitors, and Java memory-model semantics.
- Provide explicit GC roots and safepoints in generated code.
- Start with a precise, nonmoving collector for correctness; evaluate more advanced collectors before calling the runtime performance-ready.
- Implement reflection, method handles, dynamic class definition, `invokedynamic`, JNI, and required Java library/native services through staged conformance milestones.
- Unsupported facilities produce specific diagnostics. They never silently return placeholder results.

Compiled code must not depend on another JVM’s private object layout. The transitional and standalone backends are separate execution environments; do not attempt transparent object sharing between their heaps.

Java standard-library provisioning is an early feasibility gate: pin the required OpenJDK library artifacts, inventory their native/runtime dependencies, and prove a legally distributable arrangement before expanding to game boot.

### Incremental mod compilation

Compile the classes produced by the selected loader’s transformation process, preserving its ordering and class-loader identity.

Cache native units by:

- Transformed class contents and relevant dependency assumptions.
- Runtime/compiler ABI and optimization version.
- Target architecture and enabled CPU features.

Use per-class compilation initially, with stable method dispatch boundaries. Disable cross-class inlining initially to keep changes local.

When adding or updating a mod:

1. Run the loader’s normal discovery and transformation behavior.
2. Compare resulting class contents with cached units.
3. Reuse unchanged units.
4. Interpret changed or newly defined classes in the standalone runtime.
5. Compile eligible units in a bounded background queue.
6. Publish compiled entrypoints at safe boundaries; existing frames finish using their original implementation.

A mod can change Minecraft or another mod, so affected classes also require processing. Arbitrary transformers may force transformation work to repeat; **unchanged resulting classes must still reuse compiled artifacts**.

Mod changes take effect on restart initially. Hot installation/removal during a running world is outside the first release.

Compilation failures retain the correct execution path and generate diagnostics. A native-runtime compatibility failure requires relaunching through the JVM backend; there is no hidden mid-session heap migration.

### User interface

Start with a CLI:

- `jabs inspect`: inventory a profile’s game, libraries, loader, mods, and required facilities.
- `jabs prepare`: produce local analysis and cached compilation artifacts.
- `jabs run`: launch a selected profile/backend.
- `jabs verify`: run compatibility fixtures or replay comparisons.
- `jabs bench`: run a pinned benchmark suite and produce a report.
- `jabs doctor`: explain unsupported features, cache misses, and environment problems.

Profiles identify the Minecraft version, loader, mods, Java requirements, execution backend, and optimization settings. Reports show actual backend usage, compilation time, cache reuse, and fallback reasons.

Authentication uses legitimate account flows and existing version metadata. Store credentials in the platform credential store where available.

## Compatibility and platform rollout

### Minecraft and mod support

Start with **Minecraft 1.21.1, Java 21, and Fabric** as a stable development fixture. Add a separately pinned version matching Horizon for direct comparisons; benchmarks across different Minecraft versions do not count as head-to-head wins.

Expansion order:

1. Vanilla and Fabric on the initial version.
2. NeoForge on the same version.
3. Minecraft 1.12.2 with Forge and Java 8 semantics.
4. Minecraft 1.7.10 with Forge.
5. Additional modern and historical versions through version-specific adapters and regression profiles.

The initial compatibility corpus covers vanilla survival/creative, world generation, saves, entities, redstone, commands, resource/data packs, networking, audio, input, and authentication. Add mod fixtures for Mixins, reflection, dynamic classes, native libraries, serialization, custom rendering, and concurrency.

Include Sodium and Iris as explicit compatibility targets; shader support is not inferred from basic rendering success. Performance mods remain valid dependencies rather than competitors to exclude.

Client and dedicated-server execution share compiler/runtime infrastructure. Server smoke tests precede client rendering to expose runtime failures with fewer dependencies.

### Platforms

- **First release:** Windows x86-64 and Linux x86-64, with automated conformance testing on both.
- **Next:** macOS ARM64, then other desktop architectures with available test machines and dependencies.
- **Later:** Android and FreeBSD; evaluate iOS separately around native-code loading, packaging, and graphics restrictions.
- **Legacy/32-bit:** add named targets only after memory, compiler, OS API, and graphics requirements are demonstrated.
- **Browser:** defer implementation until desktop acceptance gates pass. Keep platform interfaces portable meanwhile.

The compiler may run on a newer build host than the execution target. C generation alone does not establish platform support.

Retain Minecraft’s existing graphics path initially. Introduce replacement rendering or platform backends only as separately tested changes, especially where mods depend on graphics APIs.

## Milestones and acceptance gates

### 1. Establish reproducible evidence

Create pinned profiles, input fixtures, benchmark worlds, replay actions, and JSON reports. Capture stock Minecraft, a performance-mod profile, and Horizon where equivalent configurations are possible.

Deliver a working CLI baseline and compatibility inventory before native game claims.

### 2. Demonstrate a useful optimization

Profile the baseline and select the largest actionable bottleneck supported by traces. Prototype a behavior-preserving change, compare it against the original, and package it as a repeatable transformation.

Gate: a repeatable improvement beyond measurement noise, with passing affected correctness tests. If native offloading loses, retain the better Java path and record the result.

### 3. Prove native execution semantics

Implement class parsing, verification, typed IR, an interpreter, C emission, and the shared runtime incrementally.

Test synthetic Java programs against the reference JVM for arithmetic edge cases, exceptions, inheritance/interfaces, arrays, initialization, reflection, class-loader identity, GC roots, synchronization, and compiled/interpreted calls.

Gate: the supported semantics pass differential tests on Windows and Linux. Establish the class-library provisioning path before proceeding to Minecraft boot.

### 4. Boot and validate standalone Minecraft

Progress through Java library tests, dedicated-server initialization, world generation/ticking, save/reload, client initialization, and gameplay.

Gate: repeatable gameplay and world comparisons pass for the pinned vanilla profile, including saving and reopening. Use isolated world copies for all development tests.

### 5. Preserve actual mod-loader behavior

Run Fabric and the representative mod corpus on the standalone runtime. Exercise transformations, class generation, JNI, and native libraries.

Gate: adding a fixture mod rebuilds only affected native units; repeat launches reuse all eligible cached units. Compilation runs without monopolizing gameplay threads. Loader compatibility and native execution coverage are reported explicitly.

Then expand loaders, versions, and platforms in the stated order.

### 6. Earn the performance claim

Measure cold launch, warm launch, first launch after a mod change, compilation time, memory, frame-time percentiles, tick-time percentiles, and sustained throughput.

Use identical hardware, worlds, camera paths, render/simulation distances, and visual settings. Separate generated-world tests from pregenerated-world tests and report cold versus warmed caches.

Initial release targets:

- At least **10% lower p99 frame time** than stock in the declared gameplay suite.
- At least **10% lower p95 tick time** in the declared simulation suite.
- No unexplained regression above **5%** in protected startup, mod-loading, or throughput scenarios.
- At least five measured repetitions, with variability reported.
- A Horizon-beating label only for comparable scenarios whose measured results support it.

These are release goals, not forecasts. Failure to meet them triggers further profiling or a narrower claim—not reduced simulation distance, omitted features, or altered mechanics.

## Defaults and limits

- Native compilation is a means of improving execution, not an assumption of speed.
- Fast mod loading and incremental cache reuse are required throughout development.
- The JVM remains available for unsupported profiles while native coverage grows; each milestone records which dependencies still prevent standalone execution.
- Correct original behavior takes precedence over an optimization. Disable a transformation when its applicability cannot be established.
- Browser parity, arbitrary native-mod portability, every historical release, and all OSes are expansion goals with individual acceptance gates.
- The initial implementation starts with baseline tooling and conformance fixtures, then proceeds through the milestones above. It does not begin by rewriting Minecraft or claiming a complete JVM replacement.
