# -*- coding: utf-8 -*-
"""微信输入法中/英态 —— 进程内行为测试（唯一可行路径）。

为什么不能读状态：微信输入法是**纯 TSF** 输入法，不实现 IMM32 兼容层。
实测 `ImmGetContext(edit_hwnd)` 在**本进程内**也返回 INVALID (0xFFFF...)，
所以 `ImmGetConversionStatus` 根本不可用（跨进程更是恒为 NULL）。
→ 只能看**用户可见的结果**：在文本框里打「nihao」+空格，
  中文态上屏「你好」，英文态上屏「nihao」。

为什么在本进程自建 EDIT 控件：
  - 跨进程 SendInput/焦点不可靠；
  - 自建 EDIT 让本进程成为 TSF 宿主，当前活动 profile（微信输入法）
    的 TIP 会装进本进程，行为与用户在记事本里打字完全一致；
  - 读回上屏用 WM_GETTEXT，精确无歧义。

切换动作：子进程跑 `clipx.exe --beacon-probe-cycle`
  激活信标 → 反激活信标 → 切回微信输入法（ActivateProfile 会重建 TIP 实例）。

用法：python scripts/ime_state_probe.py [--no-cycle]
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
WS_CHILD = 0x40000000
WS_VISIBLE = 0x10000000
WS_OVERLAPPEDWINDOW = 0x00CF0000
ES_MULTILINE = 0x0004
SW_SHOW = 5
SW_RESTORE = 9
WM_DESTROY = 0x0002
WM_SETTEXT = 0x000C
WM_GETTEXT = 0x000D
WM_GETTEXTLENGTH = 0x000E
WS_EX_TOOLWINDOW = 0x00000080
LRESULT = ctypes.c_ssize_t

WNDPROC = ctypes.WINFUNCTYPE(LRESULT, wintypes.HWND, wintypes.UINT,
                              wintypes.WPARAM, wintypes.LPARAM)


class WNDCLASS(ctypes.Structure):
    _fields_ = [
        ("style", wintypes.UINT), ("lpfnWndProc", WNDPROC),
        ("cbClsExtra", ctypes.c_int), ("cbWndExtra", ctypes.c_int),
        ("hInstance", wintypes.HINSTANCE), ("hIcon", wintypes.HICON),
        ("hCursor", wintypes.HANDLE), ("hbrBackground", wintypes.HBRUSH),
        ("lpszMenuName", wintypes.LPCWSTR), ("lpszClassName", wintypes.LPCWSTR),
    ]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                ("dwExtraInfo", ctypes.POINTER(wintypes.ULONG))]


class INPUTunion(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("padding", ctypes.c_byte * 24)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("u", INPUTunion)]


user32.RegisterClassW.argtypes = [ctypes.POINTER(WNDCLASS)]
user32.CreateWindowExW.restype = wintypes.HWND
user32.CreateWindowExW.argtypes = [wintypes.DWORD, wintypes.LPCWSTR, wintypes.LPCWSTR,
                                   wintypes.DWORD, ctypes.c_int, ctypes.c_int,
                                   ctypes.c_int, ctypes.c_int, wintypes.HWND,
                                   wintypes.HMENU, wintypes.HINSTANCE, ctypes.c_void_p]
user32.DefWindowProcW.restype = LRESULT
user32.DefWindowProcW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
user32.SendMessageW.restype = LRESULT
user32.SendMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
user32.GetForegroundWindow.restype = wintypes.HWND
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.AttachThreadInput.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.BOOL]
user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.PostQuitMessage.argtypes = [ctypes.c_int]


def def_wndproc(hwnd, msg, wparam, lparam):
    if msg == WM_DESTROY:
        user32.PostQuitMessage(0)
        return 0
    return user32.DefWindowProcW(hwnd, msg, wparam, lparam)


PROC = WNDPROC(def_wndproc)


def make_edit():
    hinst = kernel32.GetModuleHandleW(None)
    wc = WNDCLASS()
    wc.lpfnWndProc = PROC
    wc.hInstance = hinst
    wc.lpszClassName = "ClipxImeProbe"
    if not user32.RegisterClassW(ctypes.byref(wc)):
        err = ctypes.get_last_error()
        if err != 1410:
            raise OSError("RegisterClassW failed %d" % err)
    hwnd = user32.CreateWindowExW(WS_EX_TOOLWINDOW, "ClipxImeProbe", "ime probe",
                                  WS_OVERLAPPEDWINDOW, 100, 100, 520, 260,
                                  None, None, hinst, None)
    edit = user32.CreateWindowExW(0, "EDIT", "",
                                  WS_CHILD | WS_VISIBLE | ES_MULTILINE,
                                  0, 0, 500, 210, hwnd, None, hinst, None)
    user32.ShowWindow(hwnd, SW_SHOW)
    return hwnd, edit


def pump(seconds):
    end = time.time() + seconds
    msg = wintypes.MSG()
    while time.time() < end:
        while user32.PeekMessageW(ctypes.byref(msg), None, 0, 0, 1):
            user32.TranslateMessage(ctypes.byref(msg))
            user32.DispatchMessageW(ctypes.byref(msg))
        time.sleep(0.01)


def focus_edit(hwnd, edit):
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
        user32.SetActiveWindow(hwnd)
        user32.SetFocus(edit)
    finally:
        if att:
            user32.AttachThreadInput(my_tid, fg_tid.value, False)
    pump(0.5)
    return user32.GetForegroundWindow() == hwnd


# VK -> 扫描码表（n i h a o），IME 组字只认真实键盘 scan code，
# KEYEVENTF_UNICODE 注入会被 IME 忽略/乱码。
VK_SCAN = {
    "n": (0x4E, 0x2E),   # N
    "i": (0x49, 0x17),   # I
    "h": (0x48, 0x26),   # H
    "a": (0x41, 0x1E),   # A
    "o": (0x4F, 0x18),   # O
}


def send_unicode(text):
    """用真实 VK + scan code 注入（等价于物理键盘）。"""
    for ch in text:
        ch = ch.lower()
        if ch not in VK_SCAN:
            continue
        vk, scan = VK_SCAN[ch]
        for down in (True, False):
            ki = KEYBDINPUT()
            ki.wVk = vk
            ki.wScan = scan
            ki.dwFlags = (0 if down else KEYEVENTF_KEYUP)
            inp = INPUT()
            inp.type = INPUT_KEYBOARD
            inp.u.ki = ki
            user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
            time.sleep(0.06)


def send_vk(vk):
    for down in (True, False):
        ki = KEYBDINPUT()
        ki.wVk = vk
        ki.wScan = 0
        ki.dwFlags = 0 if down else KEYEVENTF_KEYUP
        inp = INPUT()
        inp.type = INPUT_KEYBOARD
        inp.u.ki = ki
        user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
        time.sleep(0.05)


def edit_text(edit):
    n = user32.SendMessageW(edit, WM_GETTEXTLENGTH, 0, 0)
    buf = ctypes.create_unicode_buffer(n + 2)
    user32.SendMessageW(edit, WM_GETTEXT, n + 2, ctypes.cast(buf, ctypes.c_void_p).value)
    return buf.value


def classify(txt):
    txt = (txt or "").strip()
    if not txt:
        return "空（按键未生效）"
    if txt == "nihao":
        return "英文态"
    if any('一' <= c <= '鿿' for c in txt):
        return "中文态"
    return "其他(%r)" % txt


def probe(edit, label):
    user32.SendMessageW(edit, WM_SETTEXT, 0, ctypes.cast(ctypes.c_wchar_p(""), ctypes.c_void_p).value)
    send_unicode("nihao")
    pump(0.8)
    send_vk(0x20)  # space 选词
    pump(0.8)
    txt = edit_text(edit)
    print("  %s → 上屏=%r  判定=%s" % (label, txt, classify(txt)))
    return txt


def main():
    hwnd, edit = make_edit()
    got = focus_edit(hwnd, edit)
    print("前台接管: %s (hwnd=%s)" % (got, hex(hwnd)))
    print("\n[基线] 未做任何 profile 切换：")
    base = probe(edit, "基线")

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

    focus_edit(hwnd, edit)
    print("\n[切换后] 同一个 EDIT 控件：")
    after = probe(edit, "切换后")

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
