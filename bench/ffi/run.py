# What the worker process costs against the C library (crates/fagus-ffi), from PHP, on the modern
# invoices of tools/gen.py with 1, 10 and 100 pages.
#
#   cargo build --release -p fagus-cli -p fagus-ffi && composer install   # in the repository
#   python3 run.py times [rounds]     render times (7 rounds by default)
#   python3 run.py memory [rounds]    memory of PHP and the worker (macOS; PAGES=1 for 1 page)
#
# times: in each round, for each invoice, `fagus render --repeat` (Rust, one warm engine), PHP with
# the package and its worker, PHP with the library through FFI (both warm: the median of 200, 50
# or 10 renders after one), and a new engine with its first render, as each PHP-FPM request pays
# it (the median of 30, 15 or 5), each in a process of its own, the order turned every other
# round; printed are the medians of the rounds. memory: 21 renders of one invoice, then the peak
# physical footprint and the resident size of PHP and of the worker (../memory/procmon.py), the
# medians of the rounds.
import json, os, statistics, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
FAGUS = os.environ.get('FAGUS', os.path.join(REPO, 'target', 'release', 'fagus'))
DOCS = os.path.join(HERE, 'docs')
LINES = {1: 3, 10: 150, 100: 1680}
REPEATS = {1: 200, 10: 50, 100: 10}
COLD = {1: 30, 10: 15, 100: 5}
MB = 2**20
med = statistics.median


def documents():
    os.makedirs(os.path.join(DOCS, 'fonts'), exist_ok=True)
    for name in ('Inter-Regular.ttf', 'Inter-Bold.ttf'):
        with open(os.path.join(REPO, 'tests', 'fonts', name), 'rb') as source, \
                open(os.path.join(DOCS, 'fonts', name), 'wb') as target:
            target.write(source.read())
    for lines in LINES.values():
        with open(document_of(lines), 'w') as f:
            subprocess.run([sys.executable, os.path.join(REPO, 'tools', 'gen.py'), str(lines), 'fonts'],
                           stdout=f, check=True)


def document_of(lines):
    return os.path.join(DOCS, f'modern-{lines}.html')


def command(path, mode, pages, repeats):
    return ['php', '-d', 'memory_limit=2G', os.path.join(HERE, 'engine.php'), path, mode,
            document_of(LINES[pages]), str(repeats)]


def php(path, mode, pages, repeats):
    out = subprocess.run(command(path, mode, pages, repeats), capture_output=True, text=True, check=True)
    return json.loads(out.stdout.strip().splitlines()[-1])['median']


def rust(pages, repeats):
    os.makedirs(os.path.join(HERE, 'out'), exist_ok=True)
    p = subprocess.run([FAGUS, 'render', document_of(LINES[pages]), '-o', os.path.join(HERE, 'out', 'rust.pdf'),
                        '--timings', '--repeat', str(repeats)], capture_output=True, text=True, check=True)
    return float(p.stderr.strip().splitlines()[-1].split('median ')[1].split()[0])


def times(rounds):
    results, loads = {}, []
    for r in range(rounds):
        loads.append(os.getloadavg()[0])
        for pages in LINES:
            order = ['rust', 'worker', 'ffi', 'worker-cold', 'ffi-cold']
            for what in (list(reversed(order)) if r % 2 else order):
                path, _, cold = what.partition('-')
                if path == 'rust':
                    value = rust(pages, REPEATS[pages])
                elif cold:
                    value = php(path, 'cold', pages, COLD[pages])
                else:
                    value = php(path, 'warm', pages, REPEATS[pages])
                results.setdefault((pages, what), []).append(value)
        print(f'round {r + 1} of {rounds}, load {loads[-1]:.1f}', file=sys.stderr)
    print(f'Load average of the last minute: {min(loads):.1f} to {max(loads):.1f}\n')
    print('| Pages | Rust, in-process | PHP, worker | PHP, library | New worker and its first render | New library engine and its first render |')
    print('|--:|--:|--:|--:|--:|--:|')
    for pages in LINES:
        v = {what: med(results[(pages, what)]) for what in ('rust', 'worker', 'ffi', 'worker-cold', 'ffi-cold')}
        print(f"| {pages} | {v['rust']:.2f} | {v['worker']:.2f} | {v['ffi']:.2f} | {v['worker-cold']:.2f} | {v['ffi-cold']:.2f} |")
    print('\n' + json.dumps({f'{k[0]}:{k[1]}': v for k, v in results.items()}))


def memory(rounds):
    sys.path.insert(0, os.path.join(REPO, 'bench', 'memory'))
    import procmon
    pages = int(os.environ.get('PAGES', '100'))
    runs = {}
    for r in range(rounds):
        for path in (['worker', 'ffi'] if r % 2 == 0 else ['ffi', 'worker']):
            p = subprocess.Popen(command(path, 'memory', pages, 20), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 text=True)
            while p.stdout.readline().strip() != '@@done':
                pass
            processes = {}
            for pid in procmon.tree(p.pid):
                ri = procmon.rusage(pid)
                if ri is not None:
                    processes['PHP' if pid == p.pid else 'worker'] = (ri.ri_lifetime_max_phys_footprint / MB,
                                                                     ri.ri_resident_size / MB)
            p.communicate('\n')
            runs.setdefault(path, []).append(processes)
    print(f'{pages} pages, 21 renders; peak footprint, and resident size after the renders, in MB\n')
    print('| Path | Process | Peak footprint | RSS |')
    print('|---|---|--:|--:|')
    for path, results in runs.items():
        for name in sorted({name for run in results for name in run}):
            print(f'| {path} | {name} | {med(run[name][0] for run in results):.1f} | {med(run[name][1] for run in results):.1f} |')
        print(f'| {path} | all | {med(sum(v[0] for v in run.values()) for run in results):.1f} '
              f'| {med(sum(v[1] for v in run.values()) for run in results):.1f} |')


if __name__ == '__main__':
    what = sys.argv[1] if len(sys.argv) > 1 else 'times'
    rounds = int(sys.argv[2]) if len(sys.argv) > 2 else (7 if what == 'times' else 5)
    documents()
    (times if what == 'times' else memory)(rounds)
