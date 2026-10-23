# -*- coding: utf-8 -*-
"""Probe Everything IPC variants: QUERYA(1)/QUERYW(2), all windows of ev processes."""
import ctypes, struct, time, sys
from ctypes import wintypes

user32 = ctypes.windll.user32
kernel32 = ctypes.windll.kernel32
user32.DefWindowProcW.restype = ctypes.c_ssize_t
user32.DefWindowProcW.argtypes = [wintypes.HWND, ctypes.c_uint, ctypes.c_size_t, ctypes.c_ssize_t]
user32.SendMessageW.argtypes = [wintypes.HWND, ctypes.c_uint, ctypes.c_size_t, ctypes.c_ssize_t]
user32.SendMessageW.restype = ctypes.c_ssize_t

WM_COPYDATA = 0x4A
query = (sys.argv[1] if len(sys.argv) > 1 else 'skills').encode('mbcs')

class COPYDATASTRUCT(ctypes.Structure):
    _fields_ = [('dwData', ctypes.c_size_t), ('cbData', ctypes.c_uint), ('lpData', ctypes.c_void_p)]

replies = []
WNDPROC = ctypes.WINFUNCTYPE(ctypes.c_ssize_t, wintypes.HWND, ctypes.c_uint, ctypes.c_size_t, ctypes.c_ssize_t)

def proc(h, msg, wp, lp):
    if msg == WM_COPYDATA:
        cds = ctypes.cast(lp, ctypes.POINTER(COPYDATASTRUCT)).contents
        data = ctypes.string_at(cds.lpData, cds.cbData)
        head = data[:8].hex()
        print(f'REPLY! dwData={cds.dwData} bytes={cds.cbData} head={head}')
        replies.append(data)
    return user32.DefWindowProcW(h, msg, wp, lp)

wndproc = WNDPROC(proc)

class WNDCLASSW(ctypes.Structure):
    _fields_ = [('style', ctypes.c_uint), ('lpfnWndProc', WNDPROC), ('cbClsExtra', ctypes.c_int),
                ('cbWndExtra', ctypes.c_int), ('hInstance', wintypes.HINSTANCE), ('hIcon', wintypes.HICON),
                ('hCursor', ctypes.c_void_p), ('hbrBackground', wintypes.HBRUSH), ('lpszMenuName', wintypes.LPCWSTR),
                ('lpszClassName', wintypes.LPCWSTR)]

hinst = kernel32.GetModuleHandleW(None)
wc = WNDCLASSW()
wc.lpfnWndProc = wndproc
wc.lpszClassName = 'clipx_ev_probe'
user32.RegisterClassW(ctypes.byref(wc))
recv = user32.CreateWindowExW(0, 'clipx_ev_probe', '', 0, 0, 0, 0, 0, None, None, hinst, None)

# enumerate ALL windows whose class is EVERYTHING_TASKBAR_NOTIFICATION, any pid
hits = []
@ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
def enum_cb(h, lp):
    cls = ctypes.create_unicode_buffer(64)
    user32.GetClassNameW(h, cls, 64)
    if cls.value == 'EVERYTHING_TASKBAR_NOTIFICATION':
        pid = wintypes.DWORD()
        user32.GetWindowThreadProcessId(h, ctypes.byref(pid))
        title = ctypes.create_unicode_buffer(128)
        user32.GetWindowTextW(h, title, 128)
        hits.append((h, pid.value, title.value))
    return True
user32.EnumWindows(enum_cb, 0)
print('all taskbar-notification windows:', hits)

# payload with UTF-8: EVERYTHING_IPC_QUERY2W (id=12) supports utf8 query + reply flags
# try classic QUERYA (id=1): DWORD max; DWORD offset; CHAR query[]
payload_a = struct.pack('<II', 100, 0) + query + b'\x00'

for hwnd, pid, title in hits:
    for id_, payload in ((1, payload_a),):
        buf = ctypes.create_string_buffer(payload)
        cds = COPYDATASTRUCT()
        cds.dwData = id_
        cds.cbData = len(payload)
        cds.lpData = ctypes.cast(buf, ctypes.c_void_p)
        r = user32.SendMessageW(hwnd, WM_COPYDATA, recv, ctypes.addressof(cds))
        print(f'hwnd={hwnd} pid={pid} id={id_} -> ret={r}')
        deadline = time.time() + 2
        msg = wintypes.MSG()
        while time.time() < deadline and not replies:
            while user32.PeekMessageW(ctypes.byref(msg), 0, 0, 0, 1):
                user32.TranslateMessage(ctypes.byref(msg))
                user32.DispatchMessageW(ctypes.byref(msg))
            time.sleep(0.03)
        if replies:
            print('  => GOT REPLY, totalitems=', replies[0])
            break
        print('  => no reply in 2s')
    if replies:
        break

if not replies:
    print('NO REPLY from any window/variant')
