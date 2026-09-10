#!/usr/bin/env python3
"""
LocalServer Async Stress Test Tool (Phase 9.3 & 9.4).

Simulates `siege -b -c 25 -t 10s` using Python 3 asyncio:
- Concurrently sends thousands of requests across static, CGI, and session endpoints.
- Measures Total transactions and Transactions/sec.
- Measures Availability % (target: >= 99.5%).
- Samples `/proc/$PID/fd` before and after test: asserts 0 fd leaks.
- Checks `ss -tan` for lingering `CLOSE_WAIT` connections.
- Checks `ps --ppid $PID` for zombie processes.
- Exits 0 on success, 1 on failure.
"""

import argparse
import asyncio
import os
import random
import re
import subprocess
import sys
import time

def find_localserver_pid():
    """Find PID of running localserver process."""
    try:
        out = subprocess.check_output(["pgrep", "-f", "localserver.*config.*yaml"]).decode().split()
        if out:
            return int(out[0])
    except subprocess.CalledProcessError:
        pass

    try:
        out = subprocess.check_output(["pgrep", "-x", "localserver"]).decode().split()
        if out:
            return int(out[0])
    except subprocess.CalledProcessError:
        pass
    return None

def count_fds(pid):
    """Count open file descriptors for given PID."""
    fd_dir = f"/proc/{pid}/fd"
    if not os.path.exists(fd_dir):
        return 0
    try:
        return len(os.listdir(fd_dir))
    except (PermissionError, ProcessLookupError):
        return 0

def check_close_wait(ports=(8080, 8081, 8082)):
    """Check if any sockets on specified ports are lingering in CLOSE_WAIT."""
    try:
        out = subprocess.check_output(["ss", "-tan"]).decode()
        lingering = []
        for line in out.splitlines():
            if "CLOSE-WAIT" in line or "CLOSE_WAIT" in line:
                for port in ports:
                    if f":{port}" in line:
                        lingering.append(line.strip())
        return lingering
    except Exception as e:
        print(f"Warning: failed to check ss -tan: {e}", file=sys.stderr)
        return []

def check_zombies(pid):
    """Check if server has any zombie child processes."""
    try:
        out = subprocess.check_output(["ps", "--ppid", str(pid), "-o", "pid,stat,cmd"]).decode()
        zombies = []
        for line in out.splitlines()[1:]:
            parts = line.split()
            if len(parts) >= 2 and "Z" in parts[1]:
                zombies.append(line.strip())
        return zombies
    except Exception:
        return []

ENDPOINTS = [
    # (path, weight)
    ("/empty.html", 70),
    ("/index.html", 15),
    ("/listing/", 5),
    ("/session", 5),
    ("/cgi-bin/hello.py", 5),
]

def pick_endpoint():
    total_weight = sum(w for _, w in ENDPOINTS)
    r = random.randint(1, total_weight)
    cur = 0
    for path, weight in ENDPOINTS:
        cur += weight
        if r <= cur:
            return path
    return "/empty.html"

async def worker(worker_id, host, port, end_time, stats):
    """Single load-generator worker."""
    reader = None
    writer = None
    cookie = None

    while time.time() < end_time:
        path = pick_endpoint()
        req = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}\r\n"
            f"User-Agent: stress-test-worker/{worker_id}\r\n"
            f"Connection: keep-alive\r\n\r\n"
        ).encode("latin1")
        headers = [
            f"GET {path} HTTP/1.1",
            f"Host: {host}",
            f"User-Agent: stress-test-worker/{worker_id}",
            "Connection: keep-alive",
        ]
        if cookie:
            headers.append(f"Cookie: {cookie}")
        req = ("\r\n".join(headers) + "\r\n\r\n").encode("latin1")

        try:
            if reader is None or writer is None or writer.is_closing():
                reader, writer = await asyncio.wait_for(
                    asyncio.open_connection(host, port),
                    timeout=2.0
                )

            writer.write(req)
            await writer.drain()

            # Read status line
            status_line = await asyncio.wait_for(reader.readline(), timeout=3.0)
            if not status_line:
                # Connection closed by peer
                stats["failed"] += 1
                if writer:
                    writer.close()
                reader = writer = None
                continue

            # Parse status code
            parts = status_line.split()
            if len(parts) >= 2 and parts[1].isdigit():
                code = int(parts[1])
            else:
                code = 0

            # Read headers until empty line
            content_length = None
            is_chunked = False
            conn_close = False

            while True:
                line = await asyncio.wait_for(reader.readline(), timeout=3.0)
                if not line or line == b"\r\n" or line == b"\n":
                    break
                line_str = line.decode("latin1", errors="replace").lower()
                if line_str.startswith("content-length:"):
                    try:
                        content_length = int(line_str.split(":", 1)[1].strip())
                    except ValueError:
                        pass
                elif line_str.startswith("transfer-encoding:") and "chunked" in line_str:
                    is_chunked = True
                elif line_str.startswith("connection:") and "close" in line_str:
                    conn_close = True
                elif line_str.startswith("set-cookie:"):
                    cookie_val = line.decode("latin1", errors="replace").split(":", 1)[1].strip().split(";")[0].strip()
                    if cookie_val:
                        cookie = cookie_val

            # Read body
            if content_length is not None and content_length > 0:
                await asyncio.wait_for(reader.readexactly(content_length), timeout=5.0)
            elif is_chunked:
                while True:
                    size_line = await asyncio.wait_for(reader.readline(), timeout=3.0)
                    if not size_line:
                        break
                    chunk_size = int(size_line.split(b";")[0].strip(), 16)
                    if chunk_size == 0:
                        # Consume trailing CRLF
                        await asyncio.wait_for(reader.readline(), timeout=3.0)
                        break
                    await asyncio.wait_for(reader.readexactly(chunk_size + 2), timeout=5.0)

            stats["total"] += 1
            if 200 <= code < 500:
                stats["success"] += 1
            else:
                stats["failed"] += 1
                stats["errors"][f"HTTP {code}"] = stats["errors"].get(f"HTTP {code}", 0) + 1

            if conn_close:
                writer.close()
                reader = writer = None

        except Exception as e:
            stats["total"] += 1
            stats["failed"] += 1
            err_name = f"{type(e).__name__}: {e}"
            stats["errors"][err_name] = stats["errors"].get(err_name, 0) + 1
            if writer:
                try:
                    writer.close()
                except Exception:
                    pass
            reader = writer = None
            await asyncio.sleep(0.01)

    if writer:
        try:
            writer.close()
            await writer.wait_closed()
        except Exception:
            pass

async def run_stress(host, port, concurrency, duration, pid):
    """Orchestrate concurrent workers."""
    stats = {"total": 0, "success": 0, "failed": 0}
    stats = {"total": 0, "success": 0, "failed": 0, "errors": {}}
    start_time = time.time()
    end_time = start_time + duration

    print(f"Starting stress test against http://{host}:{port}/ ...")
    print(f"Parameters: Concurrency={concurrency}, Duration={duration}s")
    if pid:
        baseline_fds = count_fds(pid)
        print(f"Target PID: {pid}, Baseline FDs: {baseline_fds}")
    else:
        baseline_fds = None
        print("Target PID: not detected (FD & zombie tracking disabled)")

    tasks = [
        asyncio.create_task(worker(i, host, port, end_time, stats))
        for i in range(concurrency)
    ]
    await asyncio.gather(*tasks)

    elapsed = time.time() - start_time
    total = stats["total"]
    success = stats["success"]
    failed = stats["failed"]
    avail = (success / total * 100.0) if total > 0 else 0.0
    rps = total / elapsed if elapsed > 0 else 0.0

    print("\n" + "=" * 55)
    print("STRESS TEST RESULTS")
    print("=" * 55)
    print(f"Elapsed Time:       {elapsed:.2f} s")
    print(f"Total Transactions: {total}")
    print(f"Successful:         {success}")
    print(f"Failed:             {failed}")
    if stats["errors"]:
        print(f"Error Details:      {stats['errors']}")
    print(f"Transaction Rate:   {rps:.2f} trans/sec")
    print(f"Availability:       {avail:.2f} %")

    # Cooldown before leak check
    time.sleep(1.5)

    checks_passed = True
    print("-" * 55)
    print("HARDENING & LEAK VERIFICATION")
    print("-" * 55)

    # 1. Availability check >= 99.5%
    if avail >= 99.5:
        print(f"[PASS] Availability is {avail:.2f}% (target: >= 99.5%)")
    else:
        print(f"[FAIL] Availability is {avail:.2f}% (target: >= 99.5%)")
        checks_passed = False

    # 2. FD Leak check
    if pid and baseline_fds is not None:
        final_fds = count_fds(pid)
        diff = final_fds - baseline_fds
        print(f"File Descriptors:   baseline={baseline_fds}, final={final_fds}, diff={diff}")
        if diff <= 0:
            print("[PASS] Open file descriptors returned to baseline (0 FD leak)")
        else:
            print(f"[FAIL] Open file descriptors leaked: {diff} extra FDs open")
            checks_passed = False

    # 3. CLOSE_WAIT check
    cw = check_close_wait([port])
    if not cw:
        print("[PASS] No lingering CLOSE_WAIT socket connections")
    else:
        print(f"[FAIL] Lingering CLOSE_WAIT connections found: {len(cw)}")
        for line in cw[:5]:
            print(f"       {line}")
        checks_passed = False

    # 4. Zombie check
    if pid:
        zombies = check_zombies(pid)
        if not zombies:
            print("[PASS] No zombie child processes found")
        else:
            print(f"[FAIL] Zombie child processes found: {len(zombies)}")
            for z in zombies:
                print(f"       {z}")
            checks_passed = False

    print("=" * 55)
    return checks_passed

def main():
    parser = argparse.ArgumentParser(description="LocalServer Async Stress Test")
    parser.add_argument("--host", default="127.0.0.1", help="Target host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=8080, help="Target port (default: 8080)")
    parser.add_argument("-c", "--concurrency", type=int, default=25, help="Concurrency (default: 25)")
    parser.add_argument("-t", "--duration", type=int, default=10, help="Duration in seconds (default: 10)")
    parser.add_argument("--pid", type=int, help="Target server PID")
    args = parser.parse_args()

    pid = args.pid or find_localserver_pid()
    spawned_proc = None

    if pid is None:
        bin_path = "./target/release/localserver" if os.path.exists("./target/release/localserver") else "./target/debug/localserver"
        if not os.path.exists(bin_path):
            subprocess.run(["cargo", "build", "--release"], check=True)
            bin_path = "./target/release/localserver"
        print(f"Spawning {bin_path} config.yaml in background...")
        spawned_proc = subprocess.Popen([bin_path, "config.yaml"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(0.5)
        pid = spawned_proc.pid

    try:
        passed = asyncio.run(run_stress(args.host, args.port, args.concurrency, args.duration, pid))
    finally:
        if spawned_proc:
            spawned_proc.terminate()
            try:
                spawned_proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                spawned_proc.kill()

    sys.exit(0 if passed else 1)

if __name__ == "__main__":
    main()
