#!/usr/bin/env python3
"""Minimal ECMA-335 metadata dumper: types, fields (+enum constants), methods, properties, user strings."""
import struct, sys

def u16(b, o): return struct.unpack_from('<H', b, o)[0]
def u32(b, o): return struct.unpack_from('<I', b, o)[0]

S, G, B = 's', 'g', 'b'
def T(n): return ('t', n)
def C(n): return ('c', n)

CODED = {
    'TypeDefOrRef': (2, [2, 1, 27]),
    'HasConstant': (2, [4, 8, 23]),
    'HasCustomAttribute': (5, [6, 4, 1, 2, 8, 9, 10, 0, 14, 23, 20, 17, 26, 27, 32, 35, 38, 39, 40, 42, 44, 43]),
    'HasFieldMarshal': (1, [4, 8]),
    'HasDeclSecurity': (2, [2, 6, 32]),
    'MemberRefParent': (3, [2, 1, 26, 6, 27]),
    'HasSemantics': (1, [20, 23]),
    'MethodDefOrRef': (1, [6, 10]),
    'MemberForwarded': (1, [4, 6]),
    'Implementation': (2, [38, 35, 39]),
    'CustomAttributeType': (3, [None, None, 6, 10, None]),
    'ResolutionScope': (2, [0, 26, 35, 1]),
    'TypeOrMethodDef': (1, [2, 6]),
}
SCHEMA = {
    0: [2, S, G, G, G], 1: [C('ResolutionScope'), S, S],
    2: [4, S, S, C('TypeDefOrRef'), T(4), T(6)], 3: [T(4)], 4: [2, S, B], 5: [T(6)],
    6: [4, 2, 2, S, B, T(8)], 7: [T(8)], 8: [2, 2, S], 9: [T(2), C('TypeDefOrRef')],
    10: [C('MemberRefParent'), S, B], 11: [2, C('HasConstant'), B],
    12: [C('HasCustomAttribute'), C('CustomAttributeType'), B], 13: [C('HasFieldMarshal'), B],
    14: [2, C('HasDeclSecurity'), B], 15: [2, 4, T(2)], 16: [4, T(4)], 17: [B],
    18: [T(2), T(20)], 19: [T(20)], 20: [2, S, C('TypeDefOrRef')], 21: [T(2), T(23)],
    22: [T(23)], 23: [2, S, B], 24: [2, T(6), C('HasSemantics')],
    25: [T(2), C('MethodDefOrRef'), C('MethodDefOrRef')], 26: [S], 27: [B],
    28: [2, C('MemberForwarded'), S, T(26)], 29: [4, T(4)], 30: [4, 4], 31: [4],
    32: [4, 2, 2, 2, 2, 4, B, S, S], 33: [4], 34: [4, 4, 4],
    35: [2, 2, 2, 2, 4, B, S, S, B], 36: [4, T(35)], 37: [4, 4, 4, T(35)],
    38: [4, S, B], 39: [4, 4, S, S, C('Implementation')], 40: [4, 4, S, C('Implementation')],
    41: [T(2), T(2)], 42: [2, 2, C('TypeOrMethodDef'), S], 43: [C('MethodDefOrRef'), B],
    44: [T(42), C('TypeDefOrRef')],
}

class Meta:
    def __init__(self, path):
        d = self.d = open(path, 'rb').read()
        pe = u32(d, 0x3c)
        nsec = u16(d, pe + 6); opt = u16(d, pe + 20)
        oh = pe + 24
        magic = u16(d, oh)
        ddir = oh + (96 if magic == 0x10b else 112)
        cli_rva = u32(d, ddir + 14 * 8)
        self.secs = []
        so = oh + opt
        for i in range(nsec):
            o = so + i * 40
            self.secs.append((u32(d, o + 12), u32(d, o + 8), u32(d, o + 20)))
        cli = self.rva(cli_rva)
        md = self.rva(u32(d, cli + 8))
        vlen = u32(d, md + 12)
        p = md + 16 + vlen
        nstreams = u16(d, p + 2); p += 4
        self.streams = {}
        for _ in range(nstreams):
            off, size = u32(d, p), u32(d, p + 4); p += 8
            e = d.index(b'\0', p); name = d[p:e].decode()
            p = (e + 4) & ~3
            self.streams[name] = (md + off, size)
        self.parse_tables()

    def rva(self, r):
        for va, vs, raw in self.secs:
            if va <= r < va + max(vs, 1):
                return r - va + raw
        raise ValueError(hex(r))

    def parse_tables(self):
        d = self.d
        o, _ = self.streams.get('#~') or self.streams['#-']
        hs = d[o + 6]
        self.sw = 4 if hs & 1 else 2; self.gw = 4 if hs & 2 else 2; self.bw = 4 if hs & 4 else 2
        valid = struct.unpack_from('<Q', d, o + 8)[0]
        p = o + 24
        self.rows = {}
        for i in range(64):
            if valid >> i & 1:
                self.rows[i] = u32(d, p); p += 4
        if hs & 0x40: p += 4
        def tw(n): return 4 if self.rows.get(n, 0) > 0xffff else 2
        def cw(name):
            bits, tabs = CODED[name]
            m = max(self.rows.get(t, 0) for t in tabs if t is not None)
            return 2 if m < (1 << (16 - bits)) else 4
        self.tables = {}
        for i in sorted(self.rows):
            cols = []
            for c in SCHEMA[i]:
                if c == S: cols.append(('s', self.sw))
                elif c == G: cols.append(('g', self.gw))
                elif c == B: cols.append(('b', self.bw))
                elif isinstance(c, int): cols.append(('i', c))
                elif c[0] == 't': cols.append(('t', tw(c[1])))
                else: cols.append(('c:' + c[1], cw(c[1])))
            rs = sum(w for _, w in cols)
            rows = []
            for r in range(self.rows[i]):
                q = p + r * rs; row = []
                for kind, w in cols:
                    row.append(u16(d, q) if w == 2 else u32(d, q)); q += w
                rows.append(row)
            self.tables[i] = rows
            p += rs * self.rows[i]

    def s(self, i):
        o, _ = self.streams['#Strings']
        e = self.d.index(b'\0', o + i)
        return self.d[o + i:e].decode('utf-8', 'replace')

    def blob(self, i):
        o, _ = self.streams['#Blob']; p = o + i; b0 = self.d[p]
        if b0 & 0x80 == 0: n, p = b0, p + 1
        elif b0 & 0xc0 == 0x80: n, p = ((b0 & 0x3f) << 8) | self.d[p + 1], p + 2
        else: n, p = ((b0 & 0x1f) << 24) | (self.d[p + 1] << 16) | (self.d[p + 2] << 8) | self.d[p + 3], p + 4
        return self.d[p:p + n]

    def user_strings(self):
        if '#US' not in self.streams: return []
        o, size = self.streams['#US']; p = o + 1; out = []
        while p < o + size:
            b0 = self.d[p]
            if b0 & 0x80 == 0: n, p = b0, p + 1
            elif b0 & 0xc0 == 0x80: n, p = ((b0 & 0x3f) << 8) | self.d[p + 1], p + 2
            else: n, p = ((b0 & 0x1f) << 24) | (self.d[p + 1] << 16) | (self.d[p + 2] << 8) | self.d[p + 3], p + 4
            if n > 1: out.append(self.d[p:p + n - 1].decode('utf-16-le', 'replace'))
            p += n
        return out

CONST_FMT = {0x02: '?', 0x03: '<H', 0x04: '<b', 0x05: '<B', 0x06: '<h', 0x07: '<H', 0x08: '<i',
             0x09: '<I', 0x0a: '<q', 0x0b: '<Q', 0x0c: '<f', 0x0d: '<d'}

def dump(path, want_us=False):
    m = Meta(path)
    td = m.tables.get(2, []); fields = m.tables.get(4, []); meths = m.tables.get(6, [])
    consts = {}
    for typ, parent, val in m.tables.get(11, []):
        tag, idx = parent & 3, parent >> 2
        if tag == 0:
            b = m.blob(val); t = typ & 0xff
            if t == 0x0e: v = repr(b.decode('utf-16-le', 'replace'))
            elif t in CONST_FMT and len(b) >= struct.calcsize(CONST_FMT[t]): v = struct.unpack_from(CONST_FMT[t], b)[0]
            else: v = b.hex()
            consts[idx] = v
    props = {}
    pm = m.tables.get(21, []); pr = m.tables.get(23, [])
    for i, (parent, start) in enumerate(pm):
        end = pm[i + 1][1] if i + 1 < len(pm) else len(pr) + 1
        props[parent] = [m.s(pr[j - 1][1]) for j in range(start, end)]
    for i, row in enumerate(td):
        flags, name, ns, ext, fstart, mstart = row
        fend = td[i + 1][4] if i + 1 < len(td) else len(fields) + 1
        mend = td[i + 1][5] if i + 1 < len(td) else len(meths) + 1
        print(f'TYPE {m.s(ns)}.{m.s(name)}')
        for f in range(fstart, fend):
            fl, fn, _ = fields[f - 1]
            c = f' = {consts[f]}' if f in consts else ''
            print(f'  F {fn and m.s(fn)}{c}')
        for p in props.get(i + 1, []):
            print(f'  P {p}')
        for mi in range(mstart, mend):
            print(f'  M {m.s(meths[mi - 1][3])}')
    if want_us:
        for s in m.user_strings():
            print('US', s.replace('\n', '\\n'))

if __name__ == '__main__':
    dump(sys.argv[1], '--us' in sys.argv)
