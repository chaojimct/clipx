# -*- coding: utf-8 -*-
"""微信输入法中/英态 —— 端到端行为测试（剪贴板回读）。

问题：profile「激活信标 → 反激活 → 切回微信输入法」之后，
微信输入法的中文/英文状态会不会被重置成英文？

方法（只看用户能看见的结果，不读内部状态）：
  1. 聚焦一个编辑器窗口；
  2. SendInput 打 "nihao" + 空格；
  3. Ctrl+A / Ctrl+C → 从剪贴板回读文本；
     中文态 = 「你好」，英文态 = 「nihao」；
  4. 跑 clipx.exe --beacon-probe-cycle 做 profile 切换循环；
  5. 在**同一个窗口**里再打一次，比对两次结果。

剪贴板回读对任何编辑器都通用（Notepad3 / 记事本 / Electron 均可）。

用法：python scripts/ime_behavior_test.py [--no-cycle] [--target <窗口标题片段>]
"""

import ctypes
from ctypes import wintypes
import os
import subprocess
import sys
import time

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004
SW_RESTORE = 9
VK_CONTROL = 0x11
VK_A = 0x41
VK_C = 0x43
VK_SPACE = 0x20
CF_UNICODETEXT = 13
GMEM_MOVEABLE = 0x0002


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                ("dwExtraInfo", ctypes.POINTER(wintypes.ULONG))]


class INPUTunion(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("padding", ctypes.c_byte * 24)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("u", INPUTunion)]


user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.GetForegroundWindow.restype = wintypes.HWND
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.AttachThreadInput.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.BOOL]
user32.GetClassNameW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.IsWindowVisible.argtypes = [wintypes.HWND]
user32.OpenClipboard.argtypes = [wintypes.HWND]
user32.GetClipboardData.restype = wintypes.HANDLE
user32.GetClipboardData.argtypes = [wintypes.UINT]
user32.CloseClipboard.argtypes = []
user32.EmptyClipboard.argtypes = []
kernel32.GlobalLock.restype = ctypes.c_void_p
kernel32.GlobalLock.argtypes = [wintypes.HANDLE]
kernel32.GlobalUnlock.argtypes = [wintypes.HANDLE]


CB = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)


def visible_windows():
    out = []
    def cb(h, l):
        if not user32.IsWindowVisible(h):
            return True
        cls = ctypes.create_unicode_buffer(64)
        user32.GetClassNameW(h, cls, 64)
        t = ctypes.create_unicode_buffer(300)
        user32.GetWindowTextW(h, t, 300)
        if t.value.strip():
            out.append((h, cls.value, t.value))
        return True
    user32.EnumWindows(CB(cb), 0)
    return out


def pick_target(keyword=None):
    cands = []
    for h, cls, title in visible_windows():
        low = (cls + "|" + title).lower()
        if keyword:
            if keyword.lower() in low:
                cands.append((h, cls, title))
        else:
            if cls in ("Notepad", "Notepad3", "AfxWnd", "Chrome_WidgetWin_1",
                       "ApplicationFrameWindow") or "notepad" in low:
                cands.append((h, cls, title))
    return cands


def force_focus(hwnd):
    fg = user32.GetForegroundWindow()
    fg_tid = wintypes.DWORD()
    if fg:
        user32.GetWindowThreadProcessId(fg, ctypes.byref(fg_tid))
    my_tid = kernel32.GetCurrentThreadId()
    att = False
    if fg_tid.value and fg_tid.value != my_tid:
        att = bool(user32.AttachThreadInput(my_tid, fg_tid.value, True))
    try:
        user32.ShowWindow(hwnd, SW_RESTORE)
        user32.BringWindowToTop(hwnd)
        user32.SetForegroundWindow(hwnd)
    finally:
        if att:
            user32.AttachThreadInput(my_tid, fg_tid.value, False)
    time.sleep(0.6)


def send_vk(vk, ctrl=False):
    if ctrl:
        for down in (True, False):
            ki = KEYBDINPUT(); ki.wVk = VK_CONTROL
            ki.dwFlags = 0 if down else KEYEVENTF_KEYUP
            inp = INPUT(); inp.type = INPUT_KEYBOARD; inp.u.ki = ki
            user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
    for down in (True, False):
        ki = KEYBDINPUT(); ki.wVk = vk
        ki.dwFlags = 0 if down else KEYEVENTF_KEYUP
        inp = INPUT(); inp.type = INPUT_KEYBOARD; inp.u.ki = ki
        user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
    if ctrl:
        for down in (True, False):
            ki = KEYBDINPUT(); ki.wVk = VK_CONTROL
            ki.dwFlags = 0 if down else KEYEVENTF_KEYUP
            inp = INPUT(); inp.type = INPUT_KEYBOARD; inp.u.ki = ki
            user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
    time.sleep(0.06)


def send_unicode(text):
    for ch in text:
        for down in (True, False):
            ki = KEYBDINPUT(); ki.wVk = 0; ki.wScan = ord(ch)
            ki.dwFlags = KEYEVENTF_UNICODE | (0 if down else KEYEVENTF_KEYUP)
            inp = INPUT(); inp.type = INPUT_KEYBOARD; inp.u.ki = ki
            user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
            time.sleep(0.04)


def clipboard_read():
    for _ in range(10):
        if user32.OpenClipboard(None):
            try:
                h = user32.GetClipboardData(CF_UNICODETEXT)
                if h:
                    p = kernel32.GlobalLock(h)
                    if p:
                        s = ctypes.c_wchar_p(p).value
                        kernel32.GlobalUnlock(h)
                        return s or ""
            finally:
                user32.CloseClipboard()
        time.sleep(0.15)
    return ""


def clipboard_clear():
    if user32.OpenClipboard(None):
        try:
            user32.EmptyClipboard()
        finally:
            user32.CloseClipboard()


def classify(txt):
    txt = (txt or "").strip()
    if not txt:
        return "空（按键未生效）"
    if txt == "nihao":
        return "英文态"
    if any('一' <= c <= '鿿' for c in txt):
        return "中文态"
    return "其他(%r)" % txt


def type_and_read(label):
    clipboard_clear()
    send_unicode("nihao")
    time.sleep(1.0)
    send_vk(VK_SPACE)
    time.sleep(1.0)
    send_vk(VK_A, ctrl=True)
    time.sleep(0.4)
    send_vk(VK_C, ctrl=True)
    time.sleep(0.8)
    txt = clipboard_read()
    print("  %s → 上屏=%r  判定=%s" % (label, txt, classify(txt)))
    return txt


def main():
    keyword = None
    if "--target" in sys.argv:
        keyword = sys.argv[sys.argv.index("--target") + 1]

    cands = pick_target(keyword)
    if not cands:
        print("没找到目标窗口。当前可见窗口：")
        for h, cls, t in visible_windows():
            print("  %s  %-28s %s" % (hex(h), cls, t[:50]))
        return 2
    hwnd, cls, title = cands[-1]
    print("目标窗口: %s  class=%s  title=%s" % (hex(hwnd), cls, title))
    force_focus(hwnd)

    print("\n[基线] 未做任何 profile 切换：")
    base = type_and_read("基线")

    if "--no-cycle" in sys.argv:
        return 0

    clipx = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                          "..", "target", "release", "clipx.exe"))
    if not os.path.exists(clipx):
        print("找不到 %s" % clipx)
        return 2
    print("\n--- profile 切换循环: %s --beacon-probe-cycle ---" % clipx)
    r = subprocess.run([clipx, "--beacon-probe-cycle"], capture_output=True, text=True)
    print(r.stdout.strip())
    if r.stderr.strip():
        print("stderr:", r.stderr.strip())

    force_focus(hwnd)
    time.sleep(0.6)
    print("\n[切换后] 同一个窗口：")
    after = type_and_read("切换后")

    print("\n===== 结论 =====")
    print("基线  : %r → %s" % (base, classify(base)))
    print("切换后: %r → %s" % (after, classify(after)))
    if classify(base) == classify(after):
        print("=> 中英态**未变**：profile 切换没有重置微信输入法状态。")
    else:
        print("=> 中英态**变了**：profile 切换确实重置了微信输入法状态（你的担心成立）。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
