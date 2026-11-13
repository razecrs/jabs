# JABS

Foundations for compatible Minecraft execution and incremental native compilation.
The architecture and rollout are in [PLAN.md](PLAN.md).

## Current capabilities

- Strict versioned JSON profiles with pinned SHA-256 artifact identities.
- Streaming file hashing and profile-relative artifact paths.
- Content-addressed integrity metadata with cache reuse and tamper detection.
- Host/tool diagnostics and JSON output.
- Per-run nearest-rank frame/tick timing summaries from supplied measurements.
- `inspect-jar`: class-file versions and required Java, loader metadata (Fabric,
  Quilt, NeoForge, `mods.toml`, legacy `mcmod.info`, coremods, LaunchWrapper
  tweakers), mixin configs, native libraries and `invokedynamic` usage.
- `compile-class`: compiles a class's methods to C11 with Java semantics:
  primitive arithmetic, branches, loops, switches, static fields and class
  initialization (JVMS 5.5), and objects of that same class (constructors,
  fields, virtual calls via a vtable, null checks raising
  `NullPointerException`). A small bytecode verifier proves every path
  type-correct before any pointer code is emitted. Everything else is listed
  per method with the reason it was not compiled. There is no garbage
  collector yet: objects live until the process exits. A differential test runs
  2,873 cases on the reference JVM and as native code and requires
  bit-identical results.
- `run`: launches a vanilla **server** profile on the JVM after rehashing every
  artifact, with the main class from the jar manifest and a required exact Java
  version. `--until-ready` reports wall-clock time to the server's `Done` line and
  stops it cleanly. JABS never accepts the Minecraft EULA for you: the server's
  `eula.txt` must already say `eula=true`.

Launch optimizations (each opt-in per profile under `launch.optimizations`, and
reported per run):

- `unbundle`: runs a Mojang bundler's nested jars (each verified against the
  bundler's listed SHA-256) directly on the classpath, in the bundler's order.
- `strip_signatures`: runs local unsigned copies of the game jars, because the
  JVM AOT cache skips classes from signed jars. The originals stay verified;
  the copies never leave your machine.
- `aot_cache`: Java 25+ AOT cache (JEP 483/514). `jabs prepare` performs the
  one-time training start; `jabs run` only uses an existing cache and never
  trains implicitly.

Measured on one Windows machine, 26.3 dedicated server, existing world, time to
the `Done` line: stock `java -jar` 8.6-9.0 s (3 runs); all three optimizations
4.4-6.0 s (10 runs, median about 4.6 s). The training start took 38 s. These
are startup numbers only; gameplay and tick performance are unchanged and not
yet measured. See `examples/server-profile.json`.

JABS **does not launch clients or mod loaders yet, verify bytecode, compile object-using Java,
or execute mods natively**. `prepare` caches integrity
metadata only. It reports zero compiled units. `run` requires a `launch` section.
No performance advantage has been measured.

## Getting started

Install a current stable Rust toolchain, then:

```text
cargo build --locked
cargo run -- doctor
cargo run -- hash local/1.21.1-client.jar
```

Supply your own game files in the ignored `local/` folder. Copy
`examples/profile.json`, set its artifact paths, and replace the placeholder
with the digest from `hash`. The example intentionally fails until configured.
Paths are relative to the profile file, not the process working directory.
Artifacts may also use absolute paths. No files are downloaded or executed.

```text
cargo run -- inspect examples/profile.json
cargo run -- prepare examples/profile.json
cargo run -- verify examples/profile.json
cargo run -- bench examples/measurements.json
```

`prepare` and `verify` accept `--cache PATH`; the default is `.jabs/cache`
relative to the current working directory. Cache records contain hashes and
sizes, not game contents or credentials. Sources are rehashed on every call.
Missing/corrupt records fail verification; remove a corrupt record and prepare
again. Interrupted/concurrent preparation can require a retry or record removal.
The profile key tracks the declared configuration; integrity metadata is reused
across profiles by file content. This is not yet a compiler invalidation system.

Profiles require schema 1, one game artifact, unique artifact IDs, a positive
Java major, and a lowercase SHA-256 for every artifact. Supported *labels* are
`vanilla`, `fabric`, `forge`, `neoforge` and `jvm`, `native`; labels do not imply
implemented runtime support. Non-vanilla loaders require a version. Artifact
roles are `game`, `library`, `loader`, `mod`. Dependency completeness and actual
Java-version compatibility are not checked yet. Unknown JSON fields are rejected.

## Measurements

`bench` summarizes user-supplied JSON; it does not perform or authenticate a
benchmark. Supply nonempty positive finite millisecond arrays for `frame_ms`
and `tick_ms` in each run, plus scenario, backend, Minecraft version, hardware,
and settings. The example contains explicitly synthetic data.

Runs remain separate. Output includes median, p95, p99 and maximum timings,
sample counts, and whether at least five repetitions were provided. Five runs
alone do not establish statistical significance. There is no baseline comparison
or speedup claim. Server-only measurements and automated capture are future work.

## Development

```text
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests use temporary synthetic artifacts, not Minecraft files. They cover pinned
identity, profile validation, cache reuse/invalidation, corrupted records,
percentile definitions, invalid timings and CLI failure behavior.

Next: typed IR from method bytecode, wiring `inspect-jar` into profile
inspection, then baseline capture and reference-JVM conformance testing.
