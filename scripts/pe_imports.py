import struct, sys

def parse_imports(path):
    with open(path, 'rb') as f:
        data = f.read()
    pe_off = struct.unpack_from('<I', data, 0x3C)[0]
    machine = struct.unpack_from('<H', data, pe_off + 4)[0]
    opt_off = pe_off + 24
    magic = struct.unpack_from('<H', data, opt_off)[0]
    pe32plus = (magic == 0x20b)
    # data directories: import table is index 1
    dd_off = opt_off + (112 + 8 if pe32plus else 96 + 8)
    imp_rva = struct.unpack_from('<I', data, dd_off)[0]
    # section headers
    sec_off = opt_off + (240 if pe32plus else 224)
    nsec = struct.unpack_from('<H', data, pe_off + 6)[0]
    secs = []
    for i in range(nsec):
        s = sec_off + i * 40
        va = struct.unpack_from('<I', data, s + 12)[0]
        vsize = struct.unpack_from('<I', data, s + 8)[0]
        raw = struct.unpack_from('<I', data, s + 20)[0]
        rsize = struct.unpack_from('<I', data, s + 16)[0]
        secs.append((va, vsize, raw, rsize))
    def rva2off(rva):
        for va, vs, raw, rs in secs:
            if va <= rva < va + max(vs, 1):
                return raw + (rva - va)
        return None
    out = []
    off = rva2off(imp_rva)
    if off is None or imp_rva == 0:
        return out
    while True:
        if off + 20 > len(data):
            break
        name_rva = struct.unpack_from('<I', data, off + 12)[0]
        if name_rva == 0:
            break
        noff = rva2off(name_rva)
        if noff is None or noff >= len(data):
            out.append('BAD_RVA_%x' % name_rva)
            break
        end = data.find(b'\x00', noff)
        out.append(data[noff:end].decode('ascii', errors='replace'))
        off += 20
    return out

for p in sys.argv[1:]:
    print(p)
    print('  imports:', parse_imports(p))
