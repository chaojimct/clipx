# -*- coding: utf-8 -*-
"""坐标真值校验：用 GetGUIThreadInfo.rcCaret 当 ground truth，比对注入测出的点。"""
import ctypes
import os
import subprocess
import sys
import time

u32 = ctypes.windll.user32

class RECT(ctypes.Structure):
    _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                ("right", ctypes.c_long), ("bottom", ctypes.c_long)]

class GUITHREADINFO(ctypes.Structure):
    _fields_ = [("cbSize", ctypes.c_ulong), ("flags", ctypes.c_ulong),
                ("hwndActive", ctypes.c_void_p), ("hwndFocus", ctypes.c_void_p),
                ("hwndCapture", ctypes.c_void_p), ("hwndMenuOwner", ctypes.c_void_p),
                ("hwndMoveSize", ctypes.c_void_p), ("hwndCaret", ctypes.c_void_p),
                ("rcCaret", RECT)]

def ground_truth():
    gti = GUITHREADINFO()
    gti.cbSize = ctypes.sizeof(GUITHREADINFO)
    u32.GetGUIThreadInfo(0, ctypes.byref(gti))
    if not gti.hwndCaret:
        return None
    r = RECT()
    ctypes.memmove(ctypes.byref(r), ctypes.byref(gti.rcCaret), ctypes.sizeof(RECT))
    pt = ctypes.c_long(0)
    # rcCaret 是 hwndCaret 的客户区坐标
    pt_struct = ctypes.wintypes.POINT(r.left, r.top) if hasattr(ctypes, "wintypes") else None
    class POINT(ctypes.Structure):
        _fields_ = [("x", ctypes.c_long), ("y", ctypes.c_long)]
    p = POINT(r.left, r.top)
    u32.ClientToScreen(gti.hwndCaret, ctypes.byref(p))
    return (p.x, p.y, r.right - r.left, r.bottom - r.top)

def main():
    p = subprocess.Popen(["notepad.exe"])
    time.sleep(1.5)
    found = []
    WNDENUMPROC = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)

    def cb(hwnd, _l):
        wpid = ctypes.c_ulong(0)
        u32.GetWindowThreadProcessId(hwnd, ctypes.byref(wpid))
        if wpid.value == p.pid and u32.IsWindowVisible(hwnd):
            found.append(hwnd)
            return False
        return True

    u32.EnumWindows(WNDENUMPROC(cb), 0)
    hwnd = found[0] if found else 0
    if hwnd:
        u32.keybd_event(0x12, 0, 0, 0)
        u32.keybd_event(0x12, 0, 2, 0)
        u32.SetForegroundWindow(hwnd)
        time.sleep(0.5)
        for ch in "hello caret":
            u32.keybd_event(ord(ch.upper()), 0, 0, 0)
            u32.keybd_event(ord(ch.upper()), 0, 2, 0)
            time.sleep(0.03)
        time.sleep(0.6)

    gt = ground_truth()
    print("ground-truth caret (GetGUIThreadInfo.rcCaret -> screen):", gt)

    exe = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "target", "release", "clipx.exe"))
    r = subprocess.run([exe, "--beacon-inject", str(p.pid)], capture_output=True, text=True, timeout=30)
    print("--- clipx --beacon-inject ---")
    print(r.stdout)
    if gt:
        # 与 ground truth 比矩形四边（不是中心点），避免"中心 vs 左上角"的假偏差。
        for line in r.stdout.splitlines():
            if line.startswith("注入测量成功") and "rect=" in line:
                seg = line.split("rect=")[1].split(")")[0] + ")"
                nums = seg.strip("()").split(",")
                l, t, r_, b = (int(x.strip()) for x in nums)
                gl, gt_top, gw, gh = gt
                gr, gb = gl + gw, gt_top + gh
                dl, dt, dr, db = abs(l - gl), abs(t - gt_top), abs(r_ - gr), abs(b - gb)
                worst = max(dl, dt, dr, db)
                print("矩形四边偏差: dL=%d dT=%d dR=%d dB=%d  (<=2px 即像素级精确)" % (dl, dt, dr, db))
                print("结论:", "✅ 像素级精确" if worst <= 2 else "❌ 有偏差")
    return 0

if __name__ == "__main__":
    sys.exit(main())
