#!/usr/bin/env python3
"""Crowd benchmark: N offline bot players on the 1.7.10 server, on the JVM and on JABS's server.exe.

Each engine starts in a fresh directory (same seed), then runs two phases:
1. stationary: every bot logs in and stays at spawn;
2. walking: every bot climbs 30 blocks and flies outward in its own direction, so the server keeps
   loading and generating chunks.

Bots answer keepalives and confirm the server's position like a real client. Reported per sample:
Working Set, Private RAM (and the JVM's committed/used heap), TPS from Time Update packets,
connected bots, "Can't keep up!" warnings, kicks with their reasons, and the exit code plus the
log tail if the server dies. Mojang-free: needs a local 1.7.10-server.jar and a built server.exe.
"""
import argparse
import math
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import threading
import time

from bench_1_7_10_server import heap_text, jvm_heap_mb, mc_str, mem_mb, pack, varint


def write_config(dirpath, port, seed, players):
    with open(os.path.join(dirpath, "eula.txt"), "w", encoding="utf-8") as f:
        f.write("eula=true\n")
    with open(os.path.join(dirpath, "server.properties"), "w", encoding="utf-8") as f:
        f.write(
            f"server-port={port}\n"
            "online-mode=false\n"
            f"level-seed={seed}\n"
            f"max-players={players + 10}\n"
            "view-distance=10\n"
            "spawn-protection=0\n"
            "allow-flight=true\n"
            "difficulty=1\n"
            "gamemode=0\n"
            "level-name=world\n"
        )


class Server:
    """A server process whose whole log goes to a file; counts Done and overload warnings."""

    def __init__(self, cmd, cwd, log_path):
        self.log = open(log_path, "w", encoding="utf-8", errors="replace")
        self.proc = subprocess.Popen(
            cmd, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, bufsize=1, errors="replace",
        )
        self.done = threading.Event()
        self.done_line = ""
        self.overloads = 0
        self.tail = []
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            self.log.write(line)
            s = line.rstrip()
            self.tail.append(s)
            if len(self.tail) > 40:
                self.tail.pop(0)
            if "Done (" in s and not self.done.is_set():
                self.done_line = s
                self.done.set()
            if "Can't keep up!" in s:
                self.overloads += 1
        self.log.flush()

    def alive(self):
        return self.proc.poll() is None

    def stop(self):
        if not self.alive():
            return self.proc.returncode
        try:
            self.proc.stdin.write("stop\n")
            self.proc.stdin.flush()
            return self.proc.wait(timeout=120)
        except (OSError, subprocess.TimeoutExpired):
            self.proc.kill()
            return "killed"


class Bot(threading.Thread):
    """One offline player. Reads through its own buffer so a timeout never splits a packet."""

    def __init__(self, index, port, phase, stop_ev, clock):
        super().__init__(daemon=True)
        self.name_ = f"bot{index:03d}"
        self.index = index
        self.port = port
        self.phase = phase  # shared one-element list: "stationary" or "walking"
        self.stop_ev = stop_ev
        self.clock = clock  # Time Update samples [(wall, world_age)] when not None
        self.logged_in = False
        self.joined = False  # Join Game seen: the server reads this connection as PLAY
        self.ended = None  # why the bot left: a kick reason, "eof", or an error
        self.keepalives = 0
        self.chunks = 0  # chunk columns received (Map Chunk Bulk and Chunk Data)
        self.pos = None  # [x, feet_y, z, yaw, pitch]
        self.base_y = None
        self.buf = bytearray()
        self.off = 0  # start of the first unread packet; the buffer is compacted once per recv

    def _packet(self):
        """(packet id, payload start, payload end) of the next whole packet, or None. Payloads are
        not copied: chunk data (most of the traffic) is skipped without touching it."""
        buf, n, i = self.buf, len(self.buf), self.off
        length = shift = 0
        while True:
            if i >= n:
                return None
            b = buf[i]
            i += 1
            length |= (b & 0x7F) << shift
            if not b & 0x80:
                break
            shift += 7
        end = i + length
        if end > n:
            return None
        pid = shift = 0
        while True:
            b = buf[i]
            i += 1
            pid |= (b & 0x7F) << shift
            if not b & 0x80:
                break
            shift += 7
        self.off = end
        return pid, i, end

    def _send_position(self, sock):
        x, y, z, _, _ = self.pos
        # 0x04 Player Position: x, feet y, stance (eye y), z, on ground.
        sock.sendall(pack(0x04, struct.pack(">ddddB", x, y, y + 1.62, z, 0)))

    def run(self):
        try:
            sock = socket.create_connection(("127.0.0.1", self.port), timeout=10)
            sock.sendall(pack(0, varint(5) + mc_str("127.0.0.1") + struct.pack(">H", self.port) + varint(2)))
            sock.sendall(pack(0, mc_str(self.name_)))
            sock.settimeout(0.05)
            angle = 2 * math.pi * self.index / 200.0
            last_move = last_beat = time.time()
            while not self.stop_ev.is_set():
                now = time.time()
                if self.joined and self.pos and self.phase[0] == "walking" and now - last_move >= 0.05:
                    last_move = last_beat = now
                    if self.pos[1] < self.base_y + 30:
                        self.pos[1] += 0.4
                    else:
                        self.pos[0] += 0.25 * math.cos(angle)
                        self.pos[2] += 0.25 * math.sin(angle)
                    self._send_position(sock)
                elif self.joined and now - last_beat >= 1.0:
                    # 0x03 Player (on ground): a real client sends one every tick while standing still.
                    last_beat = now
                    sock.sendall(pack(0x03, b"\x01"))
                try:
                    data = sock.recv(1 << 20)
                    if not data:
                        self.ended = "eof"
                        break
                    if self.off:
                        del self.buf[:self.off]
                        self.off = 0
                    self.buf.extend(data)
                except socket.timeout:
                    continue
                while True:
                    p = self._packet()
                    if p is None:
                        break
                    pid, start, end = p
                    # Chunk data, the bulk of the bytes: only the header is read, to count the chunk
                    # columns the server delivered (the work behind its memory).
                    if pid == 0x26 and end - start >= 2:  # Map Chunk Bulk: short column count first
                        self.chunks += struct.unpack_from(">h", self.buf, start)[0]
                        continue
                    if pid == 0x21:  # Chunk Data: x, z, ground-up, primary bit mask (0 = unload)
                        if end - start >= 11 and struct.unpack_from(">H", self.buf, start + 9)[0]:
                            self.chunks += 1
                        continue
                    pl = bytes(self.buf[start:end])
                    if not self.logged_in:
                        if pid == 0x02:
                            self.logged_in = True
                        elif pid == 0x00:
                            self.ended = "login refused: " + pl[1:].decode("utf-8", "replace")[:120]
                            return
                        continue
                    if pid == 0x01:  # Join Game: a real client sends play packets only after it
                        self.joined = True
                    elif pid == 0x00:
                        sock.sendall(pack(0x00, pl))
                        self.keepalives += 1
                    elif pid == 0x08:
                        x, y, z, yaw, pitch = struct.unpack(">dddff", pl[:32])
                        feet = y - 1.62
                        self.pos = [x, feet, z, yaw, pitch]
                        if self.base_y is None:
                            self.base_y = feet
                        # 0x06 Player Position And Look: confirms the server's placement.
                        sock.sendall(pack(0x06, struct.pack(">ddddffB", x, feet, y, z, yaw, pitch, 0)))
                    elif pid == 0x03 and self.clock is not None:
                        self.clock.append((time.time(), struct.unpack(">q", pl[:8])[0]))
                    elif pid == 0x40:
                        self.ended = "kicked: " + pl[1:].decode("utf-8", "replace")[:120]
                        return
            sock.close()
        except (OSError, EOFError, struct.error) as e:
            if not self.stop_ev.is_set():
                self.ended = f"error: {e.__class__.__name__}: {e}"


def tps(clocks, since):
    """Median over the clock bots of world ticks per wall second since `since`. Time Update packets
    queue behind chunk data, so one bot's view can lag; the median of several does not.
    (Vanilla's debug profiler would count ticks on the server, but it slows the native server
    from 20 to 6-8 TPS while the JVM stays at 20, so it would measure itself.)"""
    rates = []
    for clock in clocks:
        pts = [(w, a) for w, a in clock if w >= since]
        if len(pts) >= 2 and pts[-1][0] > pts[0][0]:
            rates.append((pts[-1][1] - pts[0][1]) / (pts[-1][0] - pts[0][0]))
    if not rates:
        return None
    rates.sort()
    return rates[len(rates) // 2]


def run_engine(label, cmd, workdir, port, args, jcmd, out_dir):
    os.makedirs(workdir)
    write_config(workdir, port, args.seed, args.players)
    print(f"\n=== {label}: {args.players} players ===", flush=True)
    t0 = time.time()
    server = Server(cmd, workdir, os.path.join(out_dir, f"{label}.log"))
    if not server.done.wait(timeout=900):
        rc = server.stop()
        print(f"{label} no Done within 900 s (rc={rc})\n" + "\n".join(server.tail[-15:]), flush=True)
        return
    print(f"{label} boot={time.time() - t0:.2f}s ({server.done_line.split(']: ')[-1]})", flush=True)

    phase = ["stationary"]
    stop_ev = threading.Event()
    clocks = [[] for _ in range(min(10, args.players))]
    bots = [Bot(i, port, phase, stop_ev, clocks[i] if i < len(clocks) else None) for i in range(args.players)]
    for b in bots:
        b.start()
        time.sleep(1.0 / args.login_rate)

    peak = (0.0, 0.0)
    for name, seconds in (("stationary", args.duration), ("walking", args.duration)):
        phase[0] = name
        start = time.time()
        last = start
        while time.time() - start < seconds and server.alive():
            time.sleep(1)
            now = time.time()
            if now - last < args.sample_every:
                continue
            ws, priv = mem_mb(server.proc.pid)
            peak = (max(peak[0], ws), max(peak[1], priv))
            heap = jvm_heap_mb(jcmd, server.proc.pid) if label.startswith("jvm") else None
            rate = tps(clocks, last)
            last = now
            online = sum(1 for b in bots if b.logged_in and b.ended is None)
            chunks = sum(b.chunks for b in bots)
            print(
                f"  {name:10s} t={now - start:5.0f}s ws={ws:7.1f}MB priv={priv:7.1f}MB{heap_text(heap)} "
                f"tps={'?' if rate is None else f'{rate:4.1f}'} online={online}/{args.players} chunks_sent={chunks} "
                f"overloads={server.overloads}",
                flush=True,
            )
        if not server.alive():
            break

    stop_ev.set()
    alive = server.alive()
    rc = server.stop()
    ended = {}
    for b in bots:
        if b.ended:
            ended[b.ended] = ended.get(b.ended, 0) + 1
    print(
        f"{label} RESULT survived={alive} rc={rc} peak_ws={peak[0]:.1f}MB peak_priv={peak[1]:.1f}MB "
        f"overloads={server.overloads} logged_in={sum(1 for b in bots if b.logged_in)}/{args.players} "
        f"chunks_sent={sum(b.chunks for b in bots)}",
        flush=True,
    )
    for reason, count in sorted(ended.items(), key=lambda kv: -kv[1])[:8]:
        print(f"  left early x{count}: {reason}", flush=True)
    if not alive:
        print("  server log tail:\n    " + "\n    ".join(server.tail[-15:]), flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--java", required=True)
    ap.add_argument("--jar", required=True)
    ap.add_argument("--native", required=True)
    ap.add_argument("--engines", choices=["both", "jvm", "native"], default="both")
    ap.add_argument("--players", type=int, default=200)
    ap.add_argument("--duration", type=int, default=300, help="seconds per phase")
    ap.add_argument("--sample-every", type=int, default=15)
    ap.add_argument("--login-rate", type=float, default=20.0, help="bot logins per second")
    ap.add_argument("--seed", default="42")
    ap.add_argument("--jvm-opts", default="-Xmx4G -XX:+UseG1GC")
    ap.add_argument(
        "--native-opts",
        default="-Xmx4G",
        help="server.exe options before --, e.g. the same -Xmx as the JVM (JABS 'dc56675' and later)",
    )
    ap.add_argument("--out", default=".", help="directory for the server logs")
    ap.add_argument("--work-root", default=None)
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    jcmd = os.path.join(os.path.dirname(args.java), "jcmd")
    work = tempfile.mkdtemp(prefix="jabs_crowd_", dir=args.work_root)
    try:
        if args.engines in ("both", "jvm"):
            cmd = [args.java, *args.jvm_opts.split(), "-Dlog4j2.disable.jmx=true",
                   "-Djava.awt.headless=true", "-jar", args.jar, "nogui"]
            run_engine("jvm", cmd, os.path.join(work, "jvm"), 25581, args, jcmd, args.out)
        if args.engines in ("both", "native"):
            cmd = [args.native, *args.native_opts.split(), "-Dlog4j2.disable.jmx=true", "--", "nogui"]
            run_engine("native", cmd, os.path.join(work, "native"), 25582, args, jcmd, args.out)
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
