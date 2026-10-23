import ctypes
from ctypes import wintypes

user32 = ctypes.windll.user32
kernel32 = ctypes.windll.kernel32

found = []

@ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
def cb(hwnd, lp):
    buf = ctypes.create_unicode_buffer(128)
    n = user32.GetClassNameW(hwnd, buf, 128)
    name = buf.value
    if name.startswith('EVERYTHING_TASKBAR_NOTIFICATION'):
        pid = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        # visible?
        vis = user32.IsWindowVisible(hwnd)
        # thread id
        tid = user32.GetWindowThreadProcessId(hwnd, None)
        found.append((hwnd, name, pid.value, vis, tid))
    return True

user32.EnumWindows(cb, 0)

for hwnd, name, pid, vis, tid in found:
    # exe path
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    h = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    path = ctypes.create_unicode_buffer(1024)
    size = wintypes.DWORD(1024)
    kernel32.QueryFullProcessImageNameW(h, 0, path, ctypes.byref(size))
    kernel32.CloseHandle(h)
    print(f'hwnd={hwnd} class={name!r} pid={pid} tid={tid} visible={bool(vis)}')
    print(f'  exe={path.value}')
