#!/usr/bin/env bash
# Compile the 1.7.10 server natively from MinecraftServer.main, build in parallel, run, report.
set -u
# Usage: tools/native-boot.sh WORKDIR  (build output goes to WORKDIR; game files stay in local/)
B="${1:?usage: native-boot.sh WORKDIR}"
mkdir -p "$B"
TOOLS="$(cd "$(dirname "$0")" && pwd)"
cd /d/Projects/jabs || exit 1
rm -rf "$B/src" && mkdir -p "$B/src"
# Classes a JVM run loaded (-Xlog:class+load), if recorded: they may be loaded by name.
LOADED=""; [ -f local/profiles/1.7.10-loaded-classes.txt ] && LOADED="--loaded-classes local/profiles/1.7.10-loaded-classes.txt"
# JABS's runtime classes (the app class loader); no invokedynamic for string concatenation.
rm -rf "$B/rt" && javac -XDstringConcat=inline -nowarn -d "$B/rt" runtime/java/jabs/rt/*.java 2>&1 | grep -v "^Note:"
./target/release/jabs.exe compile local/servers/1.7.10-server.jar $(find "$B/rt" -name "*.class") --jdk java --root net/minecraft/server/MinecraftServer $LOADED --out-dir "$B/src" > "$B/report.json" || exit 1
cp "$TOOLS/native-boot-entry.c" "$B/src/entry.c"
cd "$B/src" && start=$(date +%s)
ls *.c | xargs -P 12 -I{} clang --target=x86_64-w64-windows-gnu -std=c11 -O1 -w -c {} -o {}.o 2>&1 | grep -m5 error
clang --target=x86_64-w64-windows-gnu *.o -o server.exe 2>&1 | grep -m5 -E "error|undefined"
echo "build: $(( $(date +%s) - start ))s, $(stat -c %s server.exe 2>/dev/null) bytes"
mkdir -p /d/Projects/jabs/local/profiles/run/server-1.7.10 && cd /d/Projects/jabs/local/profiles/run/server-1.7.10
timeout 120 "$B/src/server.exe" 2>&1 | tail -6
