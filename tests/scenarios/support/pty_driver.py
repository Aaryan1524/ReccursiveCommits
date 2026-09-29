#!/usr/bin/env python3
"""Drives an interactive program on a real pseudo-terminal.

`reccursive schedule` refuses to prompt at all unless standard input is a terminal - that is
deliberate product behaviour, not an accident of testing, so proving the multi-batch wizard works
means running it against one. This is a minimal, dependency-free driver (stdlib `pty` only, no
`pexpect`) built for that one purpose: wait for a pattern to appear in the program's output, send a
line or a raw key, repeat, and print the full transcript at the end so a scenario failure is
readable.

Usage: pty_driver.py <script.json> -- <command> [args...]

`script.json` is a list of steps, each with any of:
  - `expect`: a regex to wait for in the output collected since the previous `expect`.
  - `send`: a line of text, submitted with a trailing newline - for answering a `Text` prompt.
  - `raw`: bytes written exactly as given, with no newline added - for the keys a `Select` or
    `MultiSelect` reads directly: an escape-prefixed arrow key, a bare space to toggle, or a bare
    carriage return to confirm.
"""
import json
import os
import pty
import re
import select
import sys
import time

TIMEOUT_SECONDS = 20.0


def run(steps, command):
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(command[0], command)
        os._exit(127)

    transcript = []
    buffer = ""

    def read_until(pattern):
        nonlocal buffer
        deadline = time.time() + TIMEOUT_SECONDS
        compiled = re.compile(pattern)
        while True:
            if compiled.search(buffer):
                return
            remaining = deadline - time.time()
            if remaining <= 0:
                sys.stderr.write("---- transcript so far ----\n" + "".join(transcript) + "\n")
                raise TimeoutError(f"timed out waiting for pattern: {pattern!r}")
            ready, _, _ = select.select([fd], [], [], remaining)
            if not ready:
                continue
            try:
                chunk = os.read(fd, 4096).decode("utf-8", errors="replace")
            except OSError:
                sys.stderr.write("---- transcript so far ----\n" + "".join(transcript) + "\n")
                raise TimeoutError(f"process exited waiting for pattern: {pattern!r}")
            if not chunk:
                sys.stderr.write("---- transcript so far ----\n" + "".join(transcript) + "\n")
                raise TimeoutError(f"process exited waiting for pattern: {pattern!r}")
            transcript.append(chunk)
            buffer += chunk

    for step in steps:
        if "expect" in step:
            read_until(step["expect"])
            buffer = ""
        if "send" in step:
            os.write(fd, (step["send"] + "\n").encode("utf-8"))
        if "raw" in step:
            os.write(fd, step["raw"].encode("utf-8"))

    # Drain whatever the process still has to say before it exits.
    deadline = time.time() + TIMEOUT_SECONDS
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.5)
        if not ready:
            if os.waitpid(pid, os.WNOHANG) != (0, 0):
                break
            continue
        try:
            chunk = os.read(fd, 4096).decode("utf-8", errors="replace")
        except OSError:
            break
        if not chunk:
            break
        transcript.append(chunk)

    _, status = os.waitpid(pid, 0)
    exit_code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else 1
    full_transcript = "".join(transcript)
    print(full_transcript)
    return exit_code


def main():
    if "--" not in sys.argv:
        sys.exit("usage: pty_driver.py <script.json> -- <command> [args...]")
    split = sys.argv.index("--")
    script_path = sys.argv[1]
    command = sys.argv[split + 1 :]
    with open(script_path, encoding="utf-8") as handle:
        steps = json.load(handle)
    sys.exit(run(steps, command))


if __name__ == "__main__":
    main()
