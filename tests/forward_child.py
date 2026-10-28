#!/usr/bin/env python3
import os
import sys
import time


def read_request_head(buf):
    head = bytearray()
    while b"\r\n\r\n" not in bytes(head):
        ch = buf.read(1)
        if not ch:
            raise EOFError("truncated")
        head.extend(ch)
    return bytes(head)


def parse_content_length(head):
    try:
        text = head.decode("iso-8859-1")
    except Exception:
        text = head.decode("utf-8", errors="replace")
    for line in text.split("\r\n"):
        if line.lower().startswith("content-length:"):
            try:
                return int(line.split(":", 1)[1].strip())
            except Exception:
                return 0
    return 0


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "echo"
    buf = sys.stdin.buffer
    out = sys.stdout.buffer

    if mode == "exit-silent":
        return
    if mode == "hang":
        if len(sys.argv) > 2:
            try:
                with open(sys.argv[2], "w", encoding="utf-8") as f:
                    f.write(str(os.getpid()))
            except Exception:
                pass
        time.sleep(30)
        return
    if mode == "linger":
        head = read_request_head(buf)
        cl = parse_content_length(head)
        if cl > 0:
            buf.read(cl)
        try:
            out.write(b"HTTP/1.1 200 ok\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            out.flush()
        except Exception:
            pass
        if len(sys.argv) > 2:
            try:
                with open(sys.argv[2], "w", encoding="utf-8") as f:
                    f.write(str(os.getpid()))
            except Exception:
                pass
        time.sleep(30)
        return
    if mode == "env-report":
        head = read_request_head(buf)
        cl = parse_content_length(head)
        if cl > 0:
            buf.read(cl)
        names = sys.argv[2:]
        lines = []
        for n in names:
            lines.append(f"{n}={1 if n in os.environ else 0}")
        leaked = 0
        for v in os.environ.values():
            if "upstream-secret" in str(v):
                leaked = 1
                break
        lines.append(f"token_in_env={leaked}")
        body = ("\n".join(lines) + "\n").encode("utf-8")
        resp = b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body
        out.write(resp)
        out.flush()
        return
    if mode == "big":
        head = read_request_head(buf)
        cl = parse_content_length(head)
        if cl > 0:
            buf.read(cl)
        body = b"x" * (1024 * 1024)
        resp = b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body
        out.write(resp)
        out.flush()
        return

    head = read_request_head(buf)
    cl = parse_content_length(head)
    body = b""
    if cl > 0:
        body = buf.read(cl)
    try:
        text = head.decode("iso-8859-1")
    except Exception:
        text = head.decode("utf-8", errors="replace")
    method = ""
    target = ""
    host = ""
    auth = ""
    xfp = ""
    # parse first line
    for line in text.split("\r\n"):
        if method == "" and line and "HTTP/" in line:
            parts = line.split(" ")
            if len(parts) >= 3:
                method = parts[0]
                target = parts[1]
            continue
        if line.lower().startswith("host:"):
            host = line.split(":", 1)[1].strip()
        elif line.lower().startswith("authorization:"):
            auth = line.split(":", 1)[1].strip()
        elif line.lower().startswith("x-forwarded-proto:"):
            xfp = line.split(":", 1)[1].strip()
    out.write(b"HTTP/1.1 200 OK\r\n")
    out.write(b"x-saw-method: " + method.encode() + b"\r\n")
    out.write(b"x-saw-target: " + target.encode() + b"\r\n")
    out.write(b"x-saw-host: " + host.encode() + b"\r\n")
    out.write(b"x-saw-authorization: " + auth.encode() + b"\r\n")
    out.write(b"x-saw-xfp: " + xfp.encode() + b"\r\n")
    out.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n")
    out.write(b"Connection: close\r\n\r\n")
    out.write(body)
    out.flush()


if __name__ == "__main__":
    main()
