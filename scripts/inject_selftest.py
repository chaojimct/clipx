# -*- coding: utf-8 -*-
"""路线乙自检：把焦点放到一个 TSF 宿主上，跑 clipx --beacon-inject，看能否拿到 caret。

用法: python scripts/inject_selftest.py [pid]
不带 pid 时自动开一个记事本 + 输入框。
"""
import ctypes
import os
import subprocess
import sys
import time

u32 = ctypes.windll.user32
k32 = ctypes.windll.kernel32

EXE = os.path.join(os.path.dirname(__file__), "..", "target", "release", "clipx.exe")
EXE = os.path.abspath(EXE)


def set_foreground(hwnd):
    # ALT-tap 抢焦点（Windows 只允许前台切换的进程才 SetForegroundWindow）
    u32.keybd_event(0x12, 0, 0, 0)
    u32.keybd_event(0x12, 0, 2, 0)
    u32.SetForegroundWindow(hwnd)
    time.sleep(0.4)


def foreground_pid():
    hwnd = u32.GetForegroundWindow()
    wpid = ctypes.c_ulong(0)
    u32.GetWindowThreadProcessId(hwnd, ctypes.byref(wpid))
    return wpid.value


def focus_until_confirmed(hwnd, want_pid, tries=8):
    """反复抢焦点直到前台确实是目标 —— 测量本身要求目标持有 TSF 焦点。"""
    for i in range(tries):
        set_foreground(hwnd)
        got = foreground_pid()
        if got == want_pid:
            return True
        print("  焦点未到位 (第%d次): 前台=%d 期望=%d" % (i + 1, got, want_pid))
        time.sleep(0.3)
    return False


def main():
    target_pid = None
    if len(sys.argv) > 1:
        target_pid = int(sys.argv[1])
        # 找该进程任一可见顶层窗口
        EnumWindows = u32.EnumWindows
        found = []

        WNDENUMPROC = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)

        def cb(hwnd, _l):
            pid = ctypes.c_ulong(0)
            u32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
            if pid.value == target_pid and u32.IsWindowVisible(hwnd):
                found.append(hwnd)
                return False
            return True

        EnumWindows(WNDENUMPROC(cb), 0)
        if not found:
            print("pid %d 没有可见顶层窗口" % target_pid)
            return 1
        if not focus_until_confirmed(found[0], target_pid):
            print("警告: 未能把焦点交给目标窗口，测量可能失败")
    else:
        # 开记事本，往里打几个字，让 caret 真实存在
        p = subprocess.Popen(["notepad.exe"])
        time.sleep(1.5)
        target_pid = p.pid
        # 按 pid 找顶层窗口（Win11 新记事本 class 不是 "Notepad"）
        found = []
        WNDENUMPROC = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)

        def cb(hwnd, _l):
            wpid = ctypes.c_ulong(0)
            u32.GetWindowThreadProcessId(hwnd, ctypes.byref(wpid))
            if wpid.value == target_pid and u32.IsWindowVisible(hwnd):
                found.append(hwnd)
                return False
            return True

        u32.EnumWindows(WNDENUMPROC(cb), 0)
        hwnd = found[0] if found else 0
        if hwnd:
            if not focus_until_confirmed(hwnd, target_pid):
                print("警告: 未能把焦点交给记事本，测量可能失败")
            # 往编辑框打字，制造真 caret
            for ch in "hello caret":
                u32.keybd_event(ord(ch.upper()), 0, 0, 0)
                u32.keybd_event(ord(ch.upper()), 0, 2, 0)
                time.sleep(0.03)
            time.sleep(0.5)
            focus_until_confirmed(hwnd, target_pid, tries=3)
        print("notepad pid=%d hwnd=%s 前台pid=%d" % (target_pid, hex(hwnd or 0), foreground_pid()))

    print("clipx exe:", EXE)
    # 显式传 pid，避免抢焦点失败时测到别的进程。
    r = subprocess.run(
        [EXE, "--beacon-inject", str(target_pid)],
        capture_output=True,
        text=True,
        timeout=30,
    )
    print("--- stdout ---")
    print(r.stdout)
    if r.stderr:
        print("--- stderr ---")
        print(r.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
