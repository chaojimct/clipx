# -*- coding: utf-8 -*-
"""读 clipx TSF 光标信标共享内存，打印当前可用结果。

协议 v2（clipx-tsf）：槽 0 = 控制块（BC2），槽 1..63 = 结果环（BC1）。
协议 v1（spike 旧 DLL）：槽 0 起即结果环（BC1）。

用法：
    python scripts/beacon_probe.py
    python scripts/beacon_probe.py --pid 12345
"""
import argparse
import ctypes
import ctypes.wintypes as wt
import struct
import time

k32 = ctypes.windll.kernel32
k32.OpenFileMappingW.restype = wt.HANDLE
k32.MapViewOfFile.restype = ctypes.c_void_p
k32.GetTickCount64.restype = ctypes.c_ulonglong

FILE_MAP_READ = 0x0004
FILE_MAP_WRITE = 0x0008
SLOT_COUNT = 64
SLOT_SIZE = 64
NAME = "Local\\clipx-tsf-caret"
MAGIC_CTRL = 0x43424332  # "BC2"
MAGIC_SLOT = 0x43424331  # "BC1"

# 控制块字段下标
C_MAGIC, C_VERSION, C_REQ_SEQ, C_TICK_LO, C_TICK_HI, C_ACK_SEQ = 0, 1, 2, 3, 4, 5


def open_map(write=False):
    access = FILE_MAP_READ | (FILE_MAP_WRITE if write else 0)
    h = k32.OpenFileMappingW(access, False, NAME)
    if not h:
        return None, None
    view = k32.MapViewOfFile(h, access, 0, 0, 0)
    if not view:
        k32.CloseHandle(h)
        return None, None
    return h, view


def words_at(view, slot):
    buf = (ctypes.c_ubyte * SLOT_SIZE)()
    ctypes.memmove(buf, view + slot * SLOT_SIZE, SLOT_SIZE)
    return struct.unpack_from("<16I", bytes(buf))


def detect_version(view):
    w = words_at(view, 0)
    if w[C_MAGIC] == MAGIC_CTRL:
        return 2, w
    if w[C_MAGIC] == MAGIC_SLOT:
        return 1, w
    return 0, w


def parse_slot_words(w):
    if w[0] != MAGIC_SLOT:
        return None
    pid, tick_lo, tick_hi = w[1], w[2], w[3]
    rect = (w[4], w[5], w[6], w[7])
    dpi_aware, seq, source = w[8], w[9], w[10]
    tick = (tick_hi << 32) | tick_lo
    return {
        "pid": pid,
        "tick": tick,
        "rect": rect,
        "dpi_aware": dpi_aware,
        "seq": seq,
        "source": source,
    }


def read_all(view, version):
    out = []
    start = 1 if version == 2 else 0
    for slot in range(start, SLOT_COUNT):
        w = words_at(view, slot)
        info = parse_slot_words(w)
        if info:
            info["slot"] = slot
            out.append(info)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pid", type=int, default=0, help="只看这个宿主进程的结果")
    ap.add_argument("--watch", action="store_true", help="持续刷新")
    args = ap.parse_args()

    h, view = open_map()
    if view is None:
        print("映射不存在：信标 DLL 尚未发布任何数据")
        print("（需要先安装/激活信标，且宿主进程已启动）")
        return 1

    while True:
        version, ctrl = detect_version(view)
        if version == 0:
            print("槽 0 魔数无效，等待信标初始化…")
            time.sleep(0.5)
            if not args.watch:
                return 1
            continue

        if version == 2:
            print(f"协议 v2  request_seq={ctrl[C_REQ_SEQ]} ack_seq={ctrl[C_ACK_SEQ]}")
        else:
            print("协议 v1（旧 spike DLL，无控制块）")

        rows = read_all(view, version)
        now = k32.GetTickCount64()
        shown = 0
        for r in sorted(rows, key=lambda x: x["tick"], reverse=True):
            if args.pid and r["pid"] != args.pid:
                continue
            age = now - r["tick"]
            print(
                f"  slot={r['slot']:2d} pid={r['pid']:6d} seq={r['seq']:5d} "
                f"age={age:6d}ms rect={r['rect']} dpi_aware={r['dpi_aware']} "
                f"source={r['source']}"
            )
            shown += 1
        if shown == 0:
            print("  （无匹配结果）")
        if not args.watch:
            return 0
        time.sleep(0.5)


if __name__ == "__main__":
    raise SystemExit(main())
