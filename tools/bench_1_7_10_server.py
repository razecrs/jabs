#!/usr/bin/env python3
"""Reproducible benchmark comparing Vanilla 1.7.10 JVM vs JABS native server.exe.

Runs two fair, matched comparisons on the same Windows machine:
1. Cold boot + fresh world generation with a fixed seed (`level-seed=42`) in empty dirs.
2. Warm boot on identical pre-generated `world/` copies, followed by idle RAM and
   a sustained 1-player session (`benchbot`) receiving chunks (`0x26`) and sending keepalives.

Default JVM flags use `-Xmx2G -XX:+UseG1GC` WITHOUT `-Xms` so initial heap reservation
is not artificially inflated. Requires `bench_1_7_10_server.py` arguments pointing to local
`1.7.10-server.jar` and JABS-built `server.exe`.
"""
import argparse
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import threading
import time
import zlib


def varint(n: int) -> bytes:
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            break
    return bytes(out)


def pack(pid: int, payload: bytes = b"") -> bytes:
    body = varint(pid) + payload
    return varint(len(body)) + body


def mc_str(s: str) -> bytes:
    b = s.encode("utf-8")
    return varint(len(b)) + b


def read_varint(sock: socket.socket) -> int:
    val = 0
    shift = 0
    while True:
        b = sock.recv(1)
        if not b:
            raise EOFError("eof")
        v = b[0]
        val |= (v & 0x7F) << shift
        if not (v & 0x80):
            return val
        shift += 7


def recv_pkt(sock: socket.socket):
    ln = read_varint(sock)
    buf = bytearray()
    while len(buf) < ln:
        chunk = sock.recv(ln - len(buf))
        if not chunk:
            raise EOFError("eof")
        buf.extend(chunk)
    idx = 0
    pid = 0
    shift = 0
    while True:
        b = buf[idx]
        idx += 1
        pid |= (b & 0x7F) << shift
        if not (b & 0x80):
            break
        shift += 7
    return pid, bytes(buf[idx:])


def mem_mb(pid: int):
    ps = (
        f"$p=Get-Process -Id {pid}; "
        "\"{0:F1},{1:F1}\" -f ($p.WorkingSet64/1MB),($p.PrivateMemorySize64/1MB)"
    )
    out = subprocess.check_output(["powershell", "-NoProfile", "-Command", ps], text=True).strip()
    ws, priv = out.split(",")
    return float(ws), float(priv)


def write_config(dirpath: str, port: int, seed: str):
    with open(os.path.join(dirpath, "eula.txt"), "w", encoding="utf-8") as f:
        f.write("eula=true\n")
    with open(os.path.join(dirpath, "server.properties"), "w", encoding="utf-8") as f:
        f.write(
            f"server-port={port}\n"
            "online-mode=false\n"
            f"level-seed={seed}\n"
            "max-players=20\n"
            "view-distance=10\n"
            "spawn-protection=16\n"
            "difficulty=1\n"
            "gamemode=0\n"
            "level-name=world\n"
        )


def start_server(cmd, cwd, timeout_s=120):
    t0 = time.time()
    proc = subprocess.Popen(
        cmd, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1
    )
    done_ev = threading.Event()
    done_line = [""]
    overloads = []
    tail = []

    def reader():
        for line in proc.stdout:
            s = line.rstrip()
            tail.append(s)
            if len(tail) > 60:
                tail.pop(0)
            if "Done (" in s:
                done_line[0] = s
                done_ev.set()
            if "Can't keep up!" in s:
                overloads.append(s)

    threading.Thread(target=reader, daemon=True).start()
    if not done_ev.wait(timeout=timeout_s):
        proc.kill()
        raise RuntimeError(f"Server timed out before Done:\n" + "\n".join(tail))
    boot_s = time.time() - t0
    return proc, boot_s, done_line[0], overloads


def stop_server(proc):
    proc.stdin.write("stop\n")
    proc.stdin.flush()
    return proc.wait(timeout=45)


def run_player_session(pid: int, port: int, duration_s: int, sample_every_s: int):
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    sock.sendall(pack(0, varint(5) + mc_str("127.0.0.1") + struct.pack(">H", port) + varint(2)))
    sock.sendall(pack(0, mc_str("benchbot")))
    sock.settimeout(0.5)

    start = time.time()
    last_sample = start
    logged_in = False
    bulk_ok = chunk_ok = zlib_fail = keepalives = 0
    samples = []

    while time.time() - start < duration_s:
        now = time.time()
        if now - last_sample >= sample_every_s:
            ws, priv = mem_mb(pid)
            samples.append((round(now - start, 1), ws, priv))
            print(
                f"  sample t={now - start:6.1f}s ws={ws:6.1f}MB priv={priv:6.1f}MB "
                f"bulk={bulk_ok} chunk={chunk_ok} fail={zlib_fail} ka={keepalives}",
                flush=True,
            )
            last_sample = now
        try:
            pkt_id, pl = recv_pkt(sock)
            if not logged_in and pkt_id == 0x02:
                logged_in = True
            elif logged_in and pkt_id == 0x00:
                sock.sendall(pack(0x00, pl))
                keepalives += 1
            elif logged_in and pkt_id == 0x26:
                _, dlen, _ = struct.unpack(">hiB", pl[:7])
                try:
                    zlib.decompress(pl[7 : 7 + dlen])
                    bulk_ok += 1
                except Exception:
                    zlib_fail += 1
            elif logged_in and pkt_id == 0x21 and len(pl) >= 17:
                dlen = struct.unpack(">i", pl[13:17])[0]
                if dlen > 0:
                    try:
                        zlib.decompress(pl[17 : 17 + dlen])
                        chunk_ok += 1
                    except Exception:
                        zlib_fail += 1
        except socket.timeout:
            pass
        except EOFError:
            break

    ws_end, priv_end = mem_mb(pid)
    sock.close()
    return {
        "logged_in": logged_in,
        "bulk_ok": bulk_ok,
        "chunk_ok": chunk_ok,
        "zlib_fail": zlib_fail,
        "keepalives": keepalives,
        "end_ws": ws_end,
        "end_priv": priv_end,
        "samples": samples,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--java", required=True)
    ap.add_argument("--jar", required=True)
    ap.add_argument("--native", required=True)
    ap.add_argument("--seed", default="42")
    ap.add_argument("--duration", type=int, default=600)
    ap.add_argument("--sample-every", type=int, default=30)
    ap.add_argument("--work-root", default=None)
    args = ap.parse_args()

    work_base = tempfile.mkdtemp(prefix="jabs_fair_bench_", dir=args.work_root)
    jvm_cmd = [
        args.java,
        "-Xmx2G",
        "-XX:+UseG1GC",
        "-Dlog4j2.disable.jmx=true",
        "-Djava.awt.headless=true",
        "-jar",
        args.jar,
        "nogui",
    ]
    nat_cmd = [args.native, "-Dlog4j2.disable.jmx=true", "--", "nogui"]

    try:
        # 1. Cold boot + fresh fixed-seed world generation on both engines.
        print("=== SCENARIO A: COLD BOOT + FRESH WORLDGEN (seed=" + args.seed + ") ===", flush=True)
        jvm_cold = os.path.join(work_base, "jvm_cold")
        nat_cold = os.path.join(work_base, "nat_cold")
        os.makedirs(jvm_cold)
        os.makedirs(nat_cold)
        write_config(jvm_cold, 25571, args.seed)
        write_config(nat_cold, 25572, args.seed)

        p_jvm, b_jvm, dl_jvm, ov_jvm = start_server(jvm_cmd, jvm_cold)
        ws_jvm_c, priv_jvm_c = mem_mb(p_jvm.pid)
        rc_jvm = stop_server(p_jvm)
        print(f"JVM_COLD boot={b_jvm:.2f}s ({dl_jvm}) idle_ws={ws_jvm_c:.1f}MB idle_priv={priv_jvm_c:.1f}MB overloads={len(ov_jvm)} rc={rc_jvm}", flush=True)

        p_nat, b_nat, dl_nat, ov_nat = start_server(nat_cmd, nat_cold)
        ws_nat_c, priv_nat_c = mem_mb(p_nat.pid)
        rc_nat = stop_server(p_nat)
        print(f"NAT_COLD boot={b_nat:.2f}s ({dl_nat}) idle_ws={ws_nat_c:.1f}MB idle_priv={priv_nat_c:.1f}MB overloads={len(ov_nat)} rc={rc_nat}", flush=True)

        # 2. Warm boot on identical copied pre-generated world + 1-player sustained run.
        print(f"\n=== SCENARIO B: WARM EXISTING WORLD + {args.duration}s 1-PLAYER ===", flush=True)
        seed_world = os.path.join(jvm_cold, "world")
        jvm_warm = os.path.join(work_base, "jvm_warm")
        nat_warm = os.path.join(work_base, "nat_warm")
        os.makedirs(jvm_warm)
        os.makedirs(nat_warm)
        write_config(jvm_warm, 25573, args.seed)
        write_config(nat_warm, 25574, args.seed)
        shutil.copytree(seed_world, os.path.join(jvm_warm, "world"))
        shutil.copytree(seed_world, os.path.join(nat_warm, "world"))

        print("[JVM Warm + 1p]", flush=True)
        p_jw, b_jw, dl_jw, ov_jw = start_server(jvm_cmd, jvm_warm)
        ws_jw_i, priv_jw_i = mem_mb(p_jw.pid)
        time.sleep(2)
        ws_jw_i2, priv_jw_i2 = mem_mb(p_jw.pid)
        res_jw = run_player_session(p_jw.pid, 25573, args.duration, args.sample_every)
        rc_jw = stop_server(p_jw)
        print(
            f"JVM_WARM boot={b_jw:.2f}s ({dl_jw}) idle_ws={ws_jw_i2:.1f}MB idle_priv={priv_jw_i2:.1f}MB "
            f"end_ws={res_jw['end_ws']:.1f}MB end_priv={res_jw['end_priv']:.1f}MB "
            f"bulk={res_jw['bulk_ok']} fail={res_jw['zlib_fail']} overloads={len(ov_jw)} rc={rc_jw}",
            flush=True,
        )

        print("[Native Warm + 1p]", flush=True)
        p_nw, b_nw, dl_nw, ov_nw = start_server(nat_cmd, nat_warm)
        ws_nw_i, priv_nw_i = mem_mb(p_nw.pid)
        time.sleep(2)
        ws_nw_i2, priv_nw_i2 = mem_mb(p_nw.pid)
        res_nw = run_player_session(p_nw.pid, 25574, args.duration, args.sample_every)
        rc_nw = stop_server(p_nw)
        print(
            f"NAT_WARM boot={b_nw:.2f}s ({dl_nw}) idle_ws={ws_nw_i2:.1f}MB idle_priv={priv_nw_i2:.1f}MB "
            f"end_ws={res_nw['end_ws']:.1f}MB end_priv={res_nw['end_priv']:.1f}MB "
            f"bulk={res_nw['bulk_ok']} fail={res_nw['zlib_fail']} overloads={len(ov_nw)} rc={rc_nw}",
            flush=True,
        )
    finally:
        shutil.rmtree(work_base, ignore_errors=True)


if __name__ == "__main__":
    main()
