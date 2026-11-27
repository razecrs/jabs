"""Turn sampler.exe output into a profile: self and inclusive sample counts per function, for
the threads whose stacks contain a marker function (by default the 1.7.10 server's tick loop).
Java methods (jm_<class>__<method>___<descriptor>) print as class.method; frames outside the
program (Windows DLLs) as ?<address>. Needs llvm-nm on PATH, or its path in LLVM_NM.

    python symbolize.py <exe> <samples file> [marker substring] [top N]
"""
import bisect
import collections
import os
import re
import struct
import subprocess
import sys

exe, samples_path = sys.argv[1], sys.argv[2]
marker = sys.argv[3] if len(sys.argv) > 3 else "MinecraftServer.run"
top = int(sys.argv[4]) if len(sys.argv) > 4 else 40

nm = subprocess.run(
    [os.environ.get("LLVM_NM", "llvm-nm"), "--defined-only", "-n", exe],
    capture_output=True, text=True, check=True,
).stdout
addrs, names = [], []
for line in nm.splitlines():
    parts = line.split()
    if len(parts) == 3 and parts[1] in "tT":
        addrs.append(int(parts[0], 16))
        names.append(parts[2])

ESC = re.compile(r"_([0-9a-f]{2})")


def unmangle(s):
    return ESC.sub(lambda m: chr(int(m.group(1), 16)), s)


def pretty(sym):
    """jm_<class>__<method>___<descriptor> -> class.method (descriptor dropped)."""
    if sym.startswith("jm_"):
        body = sym[3:]
        cls, _, rest = body.partition("__")
        method = rest.split("___", 1)[0]
        return unmangle(cls).replace("/", ".") + "." + unmangle(method)
    return sym




def image_base(path):
    """The PE optional header's ImageBase: where llvm-nm's addresses assume the image is loaded."""
    with open(path, "rb") as f:
        data = f.read(4096)
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    magic = struct.unpack_from("<H", data, pe + 24)[0]
    if magic == 0x20B:  # PE32+
        return struct.unpack_from("<Q", data, pe + 24 + 24)[0]
    return struct.unpack_from("<I", data, pe + 24 + 28)[0]


PREFERRED = image_base(exe)
base = None
stacks = []
with open(samples_path, encoding="utf-8", errors="replace") as f:
    for line in f:
        if line.startswith("# base "):
            base = int(line.split()[2], 16)
            continue
        if base is None:
            sys.exit(f"{samples_path}: no '# base' line before the stacks; not sampler.exe output?")
        name, _, frames = line.rstrip("\n").partition("\t")
        stack = []
        for a in frames.split():
            addr = int(a, 16) - base + PREFERRED
            i = bisect.bisect_right(addrs, addr) - 1
            stack.append(pretty(names[i]) if i >= 0 and addr - addrs[i] < 1 << 20 else f"?{a}")
        stacks.append((name, stack))

picked = [s for _, s in stacks if any(marker in fr for fr in s)]
print(f"{len(stacks)} stacks, {len(picked)} on threads with '{marker}'")
self_counts = collections.Counter(s[0] for s in picked)
incl = collections.Counter()
for s in picked:
    for fr in set(s):
        incl[fr] += 1
n = len(picked) or 1
print(f"\n== self (top of stack), % of {len(picked)} samples ==")
for fn, c in self_counts.most_common(top):
    print(f"{100 * c / n:5.1f}%  {fn}")
print(f"\n== inclusive (anywhere on the stack) ==")
for fn, c in incl.most_common(top):
    print(f"{100 * c / n:5.1f}%  {fn}")
