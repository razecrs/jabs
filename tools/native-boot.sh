#!/usr/bin/env bash
# Compile the 1.7.10 server natively from MinecraftServer.main, build in parallel, run, report.
set -u
# Usage: tools/native-boot.sh WORKDIR  (build output goes to WORKDIR; game files stay in local/)
# JABS_BOOT_OPT sets the C optimization level (default -O1; -O0 builds much faster).
# The program's class path and java.home are its .jab (src/jab.rs), written beside server.exe.
# Without a recorded java.home list (local/profiles/1.7.10-java-home.txt) this is a recording
# build: java.home is the JDK and the run appends every file it touches there to that list; the
# next build copies exactly those files into the .jab. JABS_BOOT_PORTABLE=1 also packs the .jab
# into the executable (jabs pack) and runs that single file, from a directory with no .jab.
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
HOME_LIST=local/profiles/1.7.10-java-home.txt
if [ -f "$HOME_LIST" ]; then HOME_FILES="--java-home-files $HOME_LIST"; else HOME_FILES="--record-java-home $(pwd)/$HOME_LIST"; fi
echo "java.home: $HOME_FILES"
# JABS's runtime classes (the app class loader); no invokedynamic for string concatenation, and
# sun.net.www.ParseUtil for the class path's jar: URLs.
rm -rf "$B/rt" && javac -XDstringConcat=inline --add-exports java.base/sun.net.www=ALL-UNNAMED -nowarn -d "$B/rt" runtime/java/jabs/rt/*.java 2>&1 | grep -v "^Note:"
./target/release/jabs.exe compile local/servers/1.7.10-server.jar $(find "$B/rt" -name "*.class") --jdk java --root net/minecraft/server/MinecraftServer $LOADED $EXECUTED --jab "$B/src/server.jab" $HOME_FILES --out-dir "$B/src" > "$B/report.json" || exit 1
cp "$TOOLS/native-boot-entry.c" "$B/src/entry.c"
cd "$B/src" && start=$(date +%s)
ls *.c | xargs -P 12 -I{} clang --target=x86_64-w64-windows-gnu -std=c11 ${JABS_BOOT_OPT:--O1} -w -c {} -o {}.o 2>&1 | grep -m5 error
clang --target=x86_64-w64-windows-gnu *.o -o server.exe -lws2_32 -liphlpapi 2>&1 | grep -m5 -E "error|undefined"
echo "build: $(( $(date +%s) - start ))s, $(stat -c %s server.exe 2>/dev/null) bytes, .jab $(stat -c %s server.jab 2>/dev/null) bytes"
PROGRAM="$B/src/server.exe"
if [ "${JABS_BOOT_PORTABLE:-0}" = 1 ]; then
    mkdir -p "$B/portable" && rm -f "$B/portable/server.jab"
    /d/Projects/jabs/target/release/jabs.exe pack "$B/src/server.exe" "$B/src/server.jab" --out "$B/portable/server.exe" || exit 1
    PROGRAM="$B/portable/server.exe"
fi
mkdir -p /d/Projects/jabs/local/profiles/run/server-1.7.10 && cd /d/Projects/jabs/local/profiles/run/server-1.7.10
# The same switches a JVM run would take: no JMX for log4j (nobody connects to it, and the
# platform MBean server needs reflection natives), and no Swing console window.
timeout 120 "$PROGRAM" -Dlog4j2.disable.jmx=true -- nogui 2>&1 | tail -40
