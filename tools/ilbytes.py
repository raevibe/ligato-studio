import sys
sys.path.insert(0, sys.argv[0].rsplit('/',1)[0])
from dnmeta import Meta, u32, u16
m = Meta(sys.argv[1]); want = sys.argv[2]; tname = sys.argv[3] if len(sys.argv)>3 else None
td = m.tables[2]; meths = m.tables[6]
for i,row in enumerate(td):
    if tname and m.s(row[1]) != tname: continue
    mend = td[i+1][5] if i+1 < len(td) else len(meths)+1
    for mi in range(row[5], mend):
        r = meths[mi-1]
        if m.s(r[3]) == want and r[0]:
            o = m.rva(r[0]); h = m.d[o]
            if h & 3 == 2: size, code = h >> 2, o+1
            else: size, code = u32(m.d, o+4), o + (u16(m.d,o)>>12)*4
            print(m.s(row[2])+'.'+m.s(row[1]), want, 'size', size)
            print(m.d[code:code+size].hex())
