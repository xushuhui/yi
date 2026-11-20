#!/usr/bin/env python3
"""在伪终端里跑 yi，发键，把屏幕打印出来。

为什么有它：界面改动只能在真终端里看出来，而「起假模型服务器 + 开伪终端 +
发按键 + 还原屏幕」这套东西每次手写一遍，既费时又容易写错。这里固定下来，
一次命令拿到屏幕。

用法：
    scripts/tui.py --keys 'abc\\x1b[Z|d\\r'
    scripts/tui.py --keys '你好\\r' --reply '收到。' --shots 1

按键里的 `|` 是「停下来截一次屏」的位置；`\\r`、`\\n`、`\\t`、`\\x1b[Z`
这些转义会被还原，中文原样发送。默认按当前目录、跑 ~/.local/bin/yi。

依赖 pyte（把 ANSI 转义还原成二维屏幕）；没装会提示安装命令。
"""

import argparse
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


def load_pyte():
    """拿到 pyte；没有就在缓存目录里建一个独立环境，不碰系统 Python。"""
    try:
        import pyte

        return pyte
    except ImportError:
        pass
    if os.environ.get("YI_TUI_BOOTSTRAPPED"):
        sys.exit("pyte 装不上；看上面的 pip 输出")
    cache = os.path.join(
        os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")), "yi-tui"
    )
    venv = os.path.join(cache, "venv")
    python = os.path.join(venv, "bin", "python")
    if not os.path.exists(python):
        os.makedirs(cache, exist_ok=True)
        subprocess.run([sys.executable, "-m", "venv", venv], check=True)
        subprocess.run([python, "-m", "pip", "install", "-q", "pyte"], check=True)
    env = dict(os.environ, YI_TUI_BOOTSTRAPPED="1")
    os.execve(python, [python, os.path.abspath(__file__), *sys.argv[1:]], env)


def fake_llm(reply, port_holder):
    """本地假装 OpenAI 兼容端点：流式回一句文本，附一份用量。"""
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(8)
    port_holder.append(server.getsockname()[1])

    def frame(obj):
        return ("data: " + json.dumps(obj, ensure_ascii=False) + "\n\n").encode()

    body = (
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
        + frame({"choices": [{"index": 0, "delta": {"content": reply}}]})
        + frame(
            {
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 1,
                    "prompt_cache_hit_tokens": 0,
                    "prompt_cache_miss_tokens": 10,
                },
            }
        )
        + b"data: [DONE]\n\n"
    )

    def handle(client):
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = client.recv(65536)
            if not chunk:
                return
            data += chunk
        head, rest = data.split(b"\r\n\r\n", 1)
        length = 0
        for line in head.split(b"\r\n"):
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":", 1)[1])
        while len(rest) < length:
            rest += client.recv(65536)
        client.sendall(body)
        client.close()

    while True:
        client, _ = server.accept()
        threading.Thread(target=handle, args=(client,), daemon=True).start()


def decode_keys(text):
    """把命令行里写的 \\r、\\x1b 这类转义还原成字节；其余（含中文）原样保留。"""
    out = []
    index = 0
    while index < len(text):
        char = text[index]
        if char == "\\" and index + 1 < len(text):
            nxt = text[index + 1]
            simple = {"r": "\r", "n": "\n", "t": "\t", "e": "\x1b", "\\": "\\"}
            if nxt in simple:
                out.append(simple[nxt])
                index += 2
                continue
            if nxt == "x" and index + 3 < len(text):
                try:
                    out.append(chr(int(text[index + 2 : index + 4], 16)))
                    index += 4
                    continue
                except ValueError:
                    pass
        out.append(char)
        index += 1
    return "".join(out)


def main():
    parser = argparse.ArgumentParser(description="在伪终端里跑 yi 并打印屏幕")
    parser.add_argument("--bin", default=None, help="yi 可执行文件，默认 ~/.local/bin/yi")
    parser.add_argument("--cwd", default=".", help="工作目录")
    parser.add_argument("--keys", default="", help="要发送的按键；| 表示停一次截屏")
    parser.add_argument("--reply", default="收到。", help="假模型返回的文本")
    parser.add_argument("--rows", type=int, default=24)
    parser.add_argument("--cols", type=int, default=100)
    parser.add_argument("--delay", type=float, default=0.8, help="每段按键后等多久")
    parser.add_argument("--shots", type=int, default=0, help="只打印前 N 段之后的屏幕；0 表示全打")
    args = parser.parse_args()

    pyte = load_pyte()

    binary = args.bin or os.path.expanduser("~/.local/bin/yi")
    if not os.path.exists(binary):
        found = shutil.which("yi")
        if not found:
            sys.exit(f"找不到 yi：{binary}")
        binary = found

    home = tempfile.mkdtemp(prefix="yi-tui-")
    port_holder = []
    threading.Thread(target=fake_llm, args=(args.reply, port_holder), daemon=True).start()
    while not port_holder:
        time.sleep(0.01)
    with open(os.path.join(home, "config.json"), "w", encoding="utf-8") as fh:
        json.dump(
            {"provider": "opencode-go", "base_url": f"http://127.0.0.1:{port_holder[0]}"},
            fh,
        )

    screen = pyte.Screen(args.cols, args.rows)
    stream = pyte.Stream(screen)
    pid, fd = pty.fork()
    if pid == 0:
        env = {k: v for k, v in os.environ.items() if k != "NO_COLOR"}
        env.update(
            YI_HOME=home,
            OPENCODE_API_KEY="dummy",
            TERM="xterm-256color",
        )
        os.environ.clear()
        os.environ.update(env)
        os.chdir(args.cwd)
        os.execv(binary, ["yi"])
        os._exit(1)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", args.rows, args.cols, 0, 0))

    def pump(seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([fd], [], [], 0.05)
            if not ready:
                continue
            try:
                data = os.read(fd, 262144)
            except OSError:
                return
            if not data:
                return
            text = data.decode("utf-8", "replace")
            stream.feed(text)
            # reedline 会问光标在哪；不回它，补全菜单就不画。
            if "\x1b[6n" in text:
                os.write(fd, f"\x1b[{screen.cursor.y + 1};{screen.cursor.x + 1}R".encode())

    def show(tag):
        print(f"=== {tag}")
        for index, row in enumerate(screen.display):
            if row.strip():
                print(f"{index:>2} |{row.rstrip()}")

    try:
        pump(2.0)
        segments = [segment for segment in args.keys.split("|")] if args.keys else []
        for number, segment in enumerate(segments, start=1):
            os.write(fd, decode_keys(segment).encode())
            pump(args.delay)
            if args.shots == 0 or number <= args.shots:
                show(f"第 {number} 段")
        if not segments:
            show("启动")
        os.write(fd, b"\x04")
        pump(0.3)
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except OSError:
            pass
        shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
