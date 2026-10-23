import ctypes

kernel32 = ctypes.windll.kernel32
for name in ('findx2', 'FindX', 'findx'):
    path = '\\\\.\\pipe\\' + name
    h = kernel32.CreateFileW(path, 0x80000000, 0, None, 3, 0, None)
    if h != -1:
        print(f'{path}: EXISTS')
        kernel32.CloseHandle(h)
    else:
        err = ctypes.GetLastError()
        print(f'{path}: err={err} (2=not found, 5=access denied)')
