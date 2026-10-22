# -*- coding: utf-8 -*-
"""微信输入法中/英态 —— 真实 TSF 宿主上的行为测试（Edge 地址栏 + 截图取证）。

背景与踩过的坑：
- 微信输入法是**纯 TSF** 输入法，不实现 IMM32 兼容层：
  `ImmGetContext(edit_hwnd)` 在**本进程内**也返回 INVALID(0xFFFF...)，
  跨进程更是恒 NULL → 所有「读状态」的路子都走不通。
- 自建 Win32 EDIT 控件走 IMM32 老路径，微信输入法不接管，打出来是乱码
  → 探针控件不能当 TSF 宿主。
- x64 下 ctypes 的 INPUT union 必须是 32 字节（sizeof(INPUT)=40），
  否则 SendInput 直接返回 0 / ERROR_INVALID_PARAMETER(87)，键根本没发出去。
- 剪贴板回读在 Edge 上不可靠 → 改用**截图**取证：地址栏里是
  「你好」还是「nihao」，肉眼即可判定。

用法：
  python scripts/ime_state_test.py            # 基线 + probe-cycle 后
  python scripts/ime_state_test.py --force    # 额外测「强制把信标切成活动 profile」
"""

import ctypes
from ctypes import wintypes
import os
import subprocess
import sys
import time

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)

INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
SW_RESTORE = 9
VK_CONTROL = 0x11
VK_A = 0x41
VK_C = 0x43
VK_L = 0x4C
VK_SPACE = 0x20
SRCCOPY = 0x00CC0020
DIB_RGB_COLORS = 0
PW_RENDERFULLCONTENT = 0x00000002

# VK -> 扫描码（IME 组字只认真实键盘 scan code）
VK_SCAN = {
    "n": (0x4E, 0x2E), "i": (0x49, 0x17), "h": (0x48, 0x26),
    "a": (0x41, 0x1E), "o": (0x4F, 0x18),
}


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                ("dwExtraInfo", ctypes.POINTER(wintypes.ULONG))]


class INPUTunion(ctypes.Union):
    # x64 下 union 必须 32 字节（MOUSEINPUT 24 + 8 对齐），
    # 否则 sizeof(INPUT)=28，SendInput 参数校验失败 → 返回 0 / err 87
    _fields_ = [("ki", KEYBDINPUT), ("padding", ctypes.c_byte * 32)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("u", INPUTunion)]


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [("biSize", wintypes.DWORD), ("biWidth", ctypes.c_long),
                ("biHeight", ctypes.c_long), ("biPlanes", wintypes.WORD),
                ("biBitCount", wintypes.WORD), ("biCompression", wintypes.DWORD),
                ("biSizeImage", wintypes.DWORD), ("biXPelsPerMeter", ctypes.c_long),
                ("biYPelsPerMeter", ctypes.c_long), ("biClrUsed", wintypes.DWORD),
                ("biClrImportant", wintypes.DWORD)]


class BITMAPINFO(ctypes.Structure):
    _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wintypes.DWORD * 3)]


user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.GetForegroundWindow.restype = wintypes.HWND
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.BringWindowToTop.argtypes = [wintypes.HWND]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.AttachThreadInput.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.BOOL]
user32.GetClassNameW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.IsWindowVisible.argtypes = [wintypes.HWND]
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.PrintWindow.argtypes = [wintypes.HWND, wintypes.HDC, wintypes.UINT]
user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
gdi32.CreateCompatibleDC.restype = wintypes.HDC
gdi32.CreateCompatibleDC.argtypes = [wintypes.HDC]
gdi32.CreateDIBSection.restype = wintypes.HBITMAP
gdi32.CreateDIBSection.argtypes = [wintypes.HDC, ctypes.POINTER(BITMAPINFO),
                                   wintypes.UINT, ctypes.POINTER(ctypes.c_void_p),
                                   wintypes.HANDLE, wintypes.DWORD]
gdi32.SelectObject.argtypes = [wintypes.HDC, wintypes.HGDIOBJ]
gdi32.BitBlt.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                         ctypes.c_int, wintypes.HDC, ctypes.c_int, ctypes.c_int, wintypes.DWORD]
gdi32.DeleteObject.argtypes = [wintypes.HGDIOBJ]
gdi32.DeleteDC.argtypes = [wintypes.HDC]

CB = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)


def edge_windows():
    out = []
    def cb(h, l):
        if not user32.IsWindowVisible(h):
            return True
        cls = ctypes.create_unicode_buffer(64)
        user32.GetClassNameW(h, cls, 64)
        if cls.value != "Chrome_WidgetWin_1":
            return True
        t = ctypes.create_unicode_buffer(300)
        user32.GetWindowTextW(h, t, 300)
        out.append((h, t.value))
        return True
    user32.EnumWindows(CB(cb), 0)
    return out


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
    time.sleep(0.8)
    return user32.GetForegroundWindow() == hwnd


def one(vk, scan=0):
    for down in (True, False):
        ki = KEYBDINPUT()
        ki.wVk = vk
        ki.wScan = scan
        ki.dwFlags = 0 if down else KEYEVENTF_KEYUP
        inp = INPUT()
        inp.type = INPUT_KEYBOARD
        inp.u.ki = ki
        n = user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
        if n != 1:
            print("    [SendInput 失败 err=%d]" % ctypes.get_last_error())
        time.sleep(0.05)


def combo(vk):
    one(VK_CONTROL)
    one(vk)
    one(VK_CONTROL)
    time.sleep(0.15)


def send_text(text):
    for ch in text.lower():
        if ch in VK_SCAN:
            vk, scan = VK_SCAN[ch]
            one(vk, scan)
            time.sleep(0.05)


def write_png(path, bgra, w, h):
    import struct
    import zlib
    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)
        row = bgra[y * stride:(y + 1) * stride]
        for x in range(w):
            b, g, r = row[x * 4], row[x * 4 + 1], row[x * 4 + 2]
            raw += bytes((r, g, b))

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 6))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def capture(hwnd, path):
    """PrintWindow 抓窗口，存 PNG（纯 ctypes + zlib，无第三方依赖）。"""
    rc = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rc))
    w, h = rc.right - rc.left, rc.bottom - rc.top
    if w <= 0 or h <= 0:
        return None
    hdc = user32.GetDC(hwnd)
    memdc = gdi32.CreateCompatibleDC(hdc)
    bmi = BITMAPINFO()
    bmi.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bmi.bmiHeader.biWidth = w
    bmi.bmiHeader.biHeight = -h          # 负值 = 自上而下
    bmi.bmiHeader.biPlanes = 1
    bmi.bmiHeader.biBitCount = 32
    bmi.bmiHeader.biCompression = 0
    bmi.bmiHeader.biSizeImage = w * h * 4
    ppv = ctypes.c_void_p()
    hbmp = gdi32.CreateDIBSection(memdc, ctypes.byref(bmi), DIB_RGB_COLORS,
                                  ctypes.byref(ppv), None, 0)
    old = gdi32.SelectObject(memdc, hbmp)
    gdi32.BitBlt(memdc, 0, 0, w, h, hdc, 0, 0, SRCCOPY)
    user32.PrintWindow(hwnd, memdc, PW_RENDERFULLCONTENT)
    buf = ctypes.string_at(ppv, w * h * 4)
    gdi32.SelectObject(memdc, old)
    gdi32.DeleteObject(hbmp)
    gdi32.DeleteDC(memdc)
    user32.ReleaseDC(hwnd, hdc)
    write_png(path, buf, w, h)
    return path


PROBE_HTML = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                           "..", "target", "ime-probe.html"))


def open_edge():
    """开一个本地 HTML 的 contenteditable 大输入框（真正的 TSF 文本域）。"""
    before = {h for h, _ in edge_windows()}
    exe = None
    for c in (r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
              r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"):
        if os.path.exists(c):
            exe = c
            break
    if not exe:
        return None
    url = "file:///" + PROBE_HTML.replace("\\", "/")
    subprocess.Popen([exe, "--new-window", url])
    for _ in range(30):
        time.sleep(0.5)
        new = [w for w in edge_windows() if w[0] not in before]
        if new:
            time.sleep(1.5)
            return new[-1][0]
    return None


def run_clipx(args):
    clipx = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                          "..", "target", "release", "clipx.exe"))
    if not os.path.exists(clipx):
        print("找不到 %s" % clipx)
        return ""
    r = subprocess.run([clipx] + args, capture_output=True, text=True)
    print(r.stdout.strip())
    if r.stderr.strip():
        print("stderr:", r.stderr.strip())
    return r.stdout


def round_probe(hwnd, outdir, label, shift_first=False, marker="|"):
    """在 contenteditable 里追加 "[marker]nihao" + 空格，并截图。

    - marker 用来在同一个框里分段，便于一张图看出三段结果；
    - 不做 Ctrl+A 全选删除，避免选区/组合键干扰 IME 组词；
    - shift_first=True：先按 Shift（微信输入法中/英切换键）切到中文态。
    """
    force_focus(hwnd)
    time.sleep(0.6)
    if shift_first:
        one(0x10)          # VK_SHIFT：微信输入法中/英切换
        time.sleep(0.9)
    send_text(marker)
    time.sleep(0.3)
    send_text("nihao")
    time.sleep(1.5)
    one(VK_SPACE)        # 空格选词
    time.sleep(1.5)
    path = os.path.join(outdir, "ime_%s.png" % label)
    capture(hwnd, path)
    print("  [%s] 截图: %s" % (label, path))
    return path


def main():
    outdir = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                           "..", "target", "ime-shots"))
    os.makedirs(outdir, exist_ok=True)
    hwnd = open_edge()
    if not hwnd:
        print("拉不起 Edge 窗口")
        return 2
    print("Edge 窗口: %s" % hex(hwnd))
    try:
        p1 = round_probe(hwnd, outdir, "before", shift_first=True, marker="1")
        print("\n--- profile 切换循环: clipx --beacon-probe-cycle ---")
        run_clipx(["--beacon-probe-cycle"])
        # 切换后**不按 Shift**，直接打 —— 若上屏变裸字母，
        # 说明微信输入法的中文态被 profile 切换重置回了它自己的默认(英文态)。
        p2 = round_probe(hwnd, outdir, "after", shift_first=False, marker="2")
        p3 = round_probe(hwnd, outdir, "after_shift", shift_first=True, marker="3")
        if "--force" in sys.argv:
            print("\n--- 强制把信标切成活动 profile ---")
            run_clipx(["--beacon-activate"])
        print("\n===== 请比对两张截图 =====")
        print("切换前: %s" % p1)
        print("切换后: %s" % p2)
        print("地址栏是「你好」= 中文态；是「nihao」= 英文态。")
        return 0
    finally:
        user32.PostMessageW(hwnd, 0x0010, 0, 0)
        time.sleep(1.2)


if __name__ == "__main__":
    sys.exit(main())
