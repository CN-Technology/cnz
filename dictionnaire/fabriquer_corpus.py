import os, sys, json, random, zipfile, glob, shutil
random.seed(1)
OUT = sys.argv[1]; NM = sys.argv[2]; PY = sys.argv[3]; OFF = sys.argv[4]
MAX = 64 << 10
def put(cat, name, data):
    d = os.path.join(OUT, 'samples', cat); os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, f"{len(os.listdir(d)):06d}-{name[-40:].replace('/', '_')}"), 'wb') as f: f.write(data[:MAX])
budget = {}
def add(cat, path, limit):
    try: data = open(path, 'rb').read()
    except: return
    if len(data) < 64 or budget.get(cat, 0) >= limit: return
    budget[cat] = budget.get(cat, 0) + min(len(data), MAX)
    put(cat, path, data)
# npm: packages under a permissive license
OK = ('MIT', 'ISC', 'BSD', 'Apache', '0BSD', 'BlueOak', 'Unlicense', 'CC0')
files = []
for root, dirs, fs in os.walk(NM):
    if 'package.json' in fs:
        try: lic = json.load(open(os.path.join(root, 'package.json'))).get('license', '')
        except: lic = ''
        if not isinstance(lic, str) or not lic.startswith(OK):
            dirs[:] = []; continue
    for f in fs: files.append(os.path.join(root, f))
random.shuffle(files)
ext = {'.js': ('js', 24 << 20), '.mjs': ('js', 24 << 20), '.cjs': ('js', 24 << 20), '.ts': ('ts', 8 << 20), '.json': ('json', 4 << 20),
       '.md': ('md', 4 << 20), '.css': ('css', 2 << 20), '.html': ('html', 2 << 20), '.svg': ('svg', 1 << 20), '.yml': ('yaml', 1 << 20), '.yaml': ('yaml', 1 << 20)}
for p in files:
    e = os.path.splitext(p)[1].lower()
    if e in ext and not p.endswith('.min.js') and not p.endswith('.map'): add(ext[e][0], p, ext[e][1])
for p in glob.glob(PY + '/**/*.py', recursive=True): add('py', p, 8 << 20)
for p in glob.glob(PY + '/**/*.html', recursive=True): add('html', p, 2 << 20)
for p in glob.glob(OUT + '/php/**/*.php', recursive=True): add('php', p, 8 << 20)
for p in glob.glob(OUT + '/php/**/*.xml', recursive=True): add('xml', p, 1 << 20)
# Office: half A (sorted by name, every other one); XML parts decompressed
docs = sorted(glob.glob(OFF + '/**/*.*', recursive=True))
docs = [d for d in docs if d.lower().endswith(('.docx', '.xlsx', '.pptx', '.odt', '.ods', '.odp'))]
test = []
for k, d in enumerate(docs):
    if k % 2: test.append(d); continue
    try:
        z = zipfile.ZipFile(d)
        for n in z.namelist():
            if n.endswith('.xml') or n.endswith('.rels'):
                add('office', d + '/' + n, 12 << 20) if False else None
                data = z.read(n)
                if len(data) >= 64 and budget.get('office', 0) < (12 << 20):
                    budget['office'] = budget.get('office', 0) + min(len(data), MAX); put('office', os.path.basename(d) + '_' + n, data)
    except Exception as ex: pass
open(os.path.join(OUT, 'office-test.txt'), 'w').write('\n'.join(test))
for k, v in sorted(budget.items()): print(f"{k:8} {v/1e6:6.2f} Mo")
print(f"total {sum(budget.values())/1e6:.1f} Mo ; documents Office de test : {len(test)}")
