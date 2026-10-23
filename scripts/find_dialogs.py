import ctypes
from ctypes import wintypes

user32 = ctypes.windll.user32
kernel32 = ctypes.windll.kernel32
found = []

@ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
def cb(hwnd, lp):
    cls = ctypes.create_unicode_buffer(64)
    user32.GetClassNameW(hwnd, cls, 64)
    if cls.value == '#32770' and user32.IsWindowVisible(hwnd):
        title = ctypes.create_unicode_buffer(256)
        user32.GetWindowTextW(hwnd, title, 256)
        pid = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        found.append((hwnd, title.value, pid.value))
    return True

user32.EnumWindows(cb, 0)

PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
for hwnd, title, pid in found:
    # exe path
    h = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    path = ctypes.create_unicode_buffer(1024)
    size = wintypes.DWORD(1024)
    # QueryFullProcessImageNameW
    kernel32.QueryFullProcessImageNameW(h, 0, path, ctypes.byref(size))
    kernel32.CloseHandle(h)
    # is wow64?
    h2 = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    wow = wintypes.BOOL(False)
    kernel32.IsWow64Process(h2, ctypes.byref(wow))
    kernel32.CloseHandle(h2)
    arch = 'x86(32-bit WOW64)' if wow.value else 'x64(64-bit)'
    print(f'hwnd={hwnd} title={title!r} pid={pid} arch={arch} exe={path.value}')
