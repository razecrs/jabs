#!/usr/bin/env bash
# Compile the 1.7.10 server natively from MinecraftServer.main, build in parallel, run, report.
set -u
# Usage: tools/native-boot.sh WORKDIR  (build output goes to WORKDIR; game files stay in local/)
# JABS_BOOT_OPT sets the C optimization level (default -O1; -O0 builds much faster).
B="${1:?usage: native-boot.sh WORKDIR}"
mkdir -p "$B"
TOOLS="$(cd "$(dirname "$0")" && pwd)"
cd /d/Projects/jabs || exit 1
rm -rf "$B/src" && mkdir -p "$B/src"
# Classes a JVM run loaded (-Xlog:class+load), if recorded: they may be loaded by name.
# Prefer the list from a run that reached Done; the older one stopped at the EULA check.
LOADED=""
for list in local/profiles/1.7.10-loaded-classes-done.txt local/profiles/1.7.10-loaded-classes.txt; do
    [ -f "$list" ] && { LOADED="--loaded-classes $list"; break; }
done
echo "class list: ${LOADED:-none}"
# Methods a JVM run executed (tools/method-trace), if recorded: log4j calls its plugins' factory
# methods (DatePatternConverter.newInstance, ...) by name through Method.invoke, which no
# instruction shows, so they are kept only because the run executed them.
EXECUTED=""
trace=local/profiles/1.7.10-method-trace.txt
[ -f "$trace" ] && EXECUTED="--executed-methods $trace"
echo "method trace: ${EXECUTED:-none}"
# JABS's runtime classes (the app class loader); no invokedynamic for string concatenation.
rm -rf "$B/rt" && javac -XDstringConcat=inline -nowarn -d "$B/rt" runtime/java/jabs/rt/*.java 2>&1 | grep -v "^Note:"
./target/release/jabs.exe compile local/servers/1.7.10-server.jar $(find "$B/rt" -name "*.class") --jdk java --root net/minecraft/server/MinecraftServer $LOADED $EXECUTED --out-dir "$B/src" > "$B/report.json" || exit 1
cp "$TOOLS/native-boot-entry.c" "$B/src/entry.c"
cd "$B/src" && start=$(date +%s)
ls *.c | xargs -P 12 -I{} clang --target=x86_64-w64-windows-gnu -std=c11 ${JABS_BOOT_OPT:--O1} -w -c {} -o {}.o 2>&1 | grep -m5 error
clang --target=x86_64-w64-windows-gnu *.o -o server.exe -lws2_32 -liphlpapi 2>&1 | grep -m5 -E "error|undefined"
echo "build: $(( $(date +%s) - start ))s, $(stat -c %s server.exe 2>/dev/null) bytes"
mkdir -p /d/Projects/jabs/local/profiles/run/server-1.7.10 && cd /d/Projects/jabs/local/profiles/run/server-1.7.10
# The same switches a JVM run would take: no JMX for log4j (nobody connects to it, and the
# platform MBean server needs reflection natives), and no Swing console window.
timeout 120 "$B/src/server.exe" -Dlog4j2.disable.jmx=true -- nogui 2>&1 | tail -40
