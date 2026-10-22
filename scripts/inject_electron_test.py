# -*- coding: utf-8 -*-
"""Electron 实测：把焦点放到指定 pid 的输入框上，跑 clipx --beacon-inject。

用法: python scripts/inject_electron_test.py <pid>
先在目标应用里点一下输入框，再跑本脚本（脚本会抢焦点回该进程）。
"""
import ctypes
import os
import subprocess
import sys
import time

u32 = ctypes.windll.user32

EXE = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "target", "release", "clipx.exe"))


class RECT(ctypes.Structure):
    _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                ("right", ctypes.c_long), ("bottom", ctypes.c_long)]


class GUITHREADINFO(ctypes.Structure):
    _fields_ = [("cbSize", ctypes.c_ulong), ("flags", ctypes.c_ulong),
                ("hwndActive", ctypes.c_void_p), ("hwndFocus", ctypes.c_void_p),
                ("hwndCapture", ctypes.c_void_p), ("hwndMenuOwner", ctypes.c_void_p),
                ("hwndMoveSize", ctypes.c_void_p), ("hwndCaret", ctypes.c_void_p),
                ("rcCaret", RECT)]


class POINT(ctypes.Structure):
    _fields_ = [("x", ctypes.c_long), ("y", ctypes.c_long)]


def top_hwnds(pid):
    found = []
    WNDENUMPROC = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)

    def cb(hwnd, _l):
        wpid = ctypes.c_ulong(0)
        u32.GetWindowThreadProcessId(hwnd, ctypes.byref(wpid))
        if wpid.value == pid and u32.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True

    u32.EnumWindows(WNDENUMPROC(cb), 0)
    return found


def main():
    if len(sys.argv) < 2:
        print("用法: python scripts/inject_electron_test.py <pid>")
        return 1
    pid = int(sys.argv[1])
    hwnds = top_hwnds(pid)
    print("pid=%d 可见顶层窗口: %s" % (pid, [hex(h) for h in hwnds]))
    if not hwnds:
        print("没有可见窗口")
        return 1

    hwnd = hwnds[0]
    u32.keybd_event(0x12, 0, 0, 0)
    u32.keybd_event(0x12, 0, 2, 0)
    u32.SetForegroundWindow(hwnd)
    time.sleep(0.6)

    # 记录前台 pid 供比对
    fg = u32.GetForegroundWindow()
    wpid = ctypes.c_ulong(0)
    u32.GetWindowThreadProcessId(fg, ctypes.byref(wpid))
    print("抢焦点后前台 pid=%d (期望 %d)" % (wpid.value, pid))

    r = subprocess.run([EXE, "--beacon-inject", str(pid)], capture_output=True, text=True, timeout=30)
    print("--- clipx --beacon-inject ---")
    print(r.stdout)
    if r.stderr:
        print(r.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
