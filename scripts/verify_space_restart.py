"""人工授权的已安装App只读重启验收；仅创建/移除本脚本自建夹具。"""
import json, os, pathlib, shutil, signal, subprocess, time, uuid
APP = pathlib.Path('/Applications/卡若清理.app')
EXE = APP / 'Contents/MacOS/KaruoCleaner'
SUPPORT = pathlib.Path.home() / 'Library/Application Support/KaruoCleaner'
ROOT = pathlib.Path.home() / ('.karuo-space-restart-check-' + uuid.uuid4().hex)
OUT = pathlib.Path(__file__).resolve().parent.parent / '开发文档/build17重启实机验收.json'

def api(command, **kwargs):
    return json.loads(subprocess.check_output([str(EXE), '--api', json.dumps({'cmd': command, **kwargs})], timeout=12))

def event(name, **kwargs):
    print(json.dumps({'phase': name, **kwargs}, ensure_ascii=False), flush=True)

def wait_space(predicate, timeout=20):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        result = api('session_get', module='space')
        if predicate(result):
            return result
        time.sleep(.1)
    raise RuntimeError('观察超时；先查询同一个扫描任务，不自动重启或重复启动')

def owner_pid():
    pids = subprocess.check_output(['lsof', '-t', str(SUPPORT / 'control.sock')], text=True).split()
    assert len(set(pids)) == 1
    pid = int(pids[0])
    command = subprocess.check_output(['ps', '-p', str(pid), '-o', 'command='], text=True).strip()
    assert command == str(EXE), command
    return pid

def restart(allow_read_scan=False):
    held = subprocess.run(['lsof', '-t', str(SUPPORT / 'history-rust.json.lock')], capture_output=True, text=True)
    assert not held.stdout.strip(), '文件动作仍在进行，不中断'
    for module in ['smart', 'junk', 'large', 'uninstall']:
        assert not api('session_get', module=module).get('scanning'), '其他扫描仍在进行'
    if not allow_read_scan:
        assert not api('session_get', module='space').get('scanning')
    pid = owner_pid()
    os.kill(pid, signal.SIGTERM)
    for _ in range(50):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            break
        time.sleep(.05)
    else:
        raise RuntimeError('旧进程尚未退出，未创建第二实例')
    subprocess.check_call(['open', '-n', str(APP)])
    time.sleep(.5)

for module in ['smart', 'junk', 'large', 'space', 'uninstall']:
    assert not api('session_get', module=module).get('scanning'), '应用尚有任务，本次验收不启动'
ROOT.mkdir()
(ROOT / 'Projects').mkdir()
(ROOT / 'Archive').mkdir()
for i in range(126):
    (ROOT / 'Projects' / f'item-{i:04}.bin').write_bytes(b'owned fixture')
(ROOT / 'Archive' / 'note.txt').write_bytes(b'owned fixture')
api('root_set', module='space', path=str(ROOT))
request_id = int(time.time() * 1000)
api('space_start', path=str(ROOT), request_id=request_id)
first = wait_space(lambda r: r.get('data', {}).get('saved_at', 0) > 0 and r.get('data', {}).get('done') and not r.get('data', {}).get('snapshot_notice'))
api('space_browse', module='space', path=str(ROOT / 'Projects'), page=2, query='item-', show_hidden=True, request_id=request_id)
before = api('session_get', module='space')['data']
assert before['page'] == 2 and before['total'] == 126 and len(before['entries']) == 26
assert before['query'] == 'item-' and before['show_hidden']
event('fixture_saved', scanned=before['scanned'], page=before['page'], total=before['total'])
restart()
after = wait_space(lambda r: r.get('data', {}).get('restored') and r.get('data', {}).get('root') == str(ROOT))['data']
for key in ['root', 'path', 'page', 'query', 'show_hidden', 'total', 'scanned', 'size']:
    assert before[key] == after[key], (key, before[key], after[key])
assert [e['id'] for e in before['entries']] == [e['id'] for e in after['entries']]
event('restart_restored', page=after['page'], page_count=after['page_count'], total=after['total'])
volume = next(v for v in api('volumes')['volumes'] if v.get('internal'))
request_id += 1
api('space_start', volume=volume['id'], request_id=request_id)
live = wait_space(lambda r: r.get('request_id') == request_id and r.get('scanning'))
event('read_only_scan_live', request_id=request_id, scanned=live.get('data', {}).get('scanned'))
restart(allow_read_scan=True)
interrupted = wait_space(lambda r: r.get('data', {}).get('restored') and bool(r.get('data', {}).get('snapshot_notice')))['data']
assert interrupted['root'] == str(ROOT) and interrupted['total'] == 126
assert '此前已保存' in interrupted['snapshot_notice']
event('interruption_restored_previous', notice=interrupted['snapshot_notice'])
report = {'restart': {'restored': True, 'total': after['total'], 'page': after['page'], 'page_count': after['page_count'], 'query_preserved': True, 'hidden_preference_preserved': True, 'entry_ids_preserved': True}, 'interruption': {'previous_snapshot_preserved': True, 'notice': interrupted['snapshot_notice']}, 'scope': '只读扫描和重启；未执行个人文件移动、恢复或永久清理'}
# 仅清理本次唯一目录；随后重新扫描真实内置磁盘，避免留下夹具结果。
assert ROOT.parent == pathlib.Path.home() and ROOT.name.startswith('.karuo-space-restart-check-')
shutil.rmtree(ROOT)
volume = next(v for v in api('volumes')['volumes'] if v.get('internal'))
request_id += 1
api('space_start', volume=volume['id'], request_id=request_id)
actual = wait_space(lambda r: r.get('request_id') == request_id)
report['final_disk_scan'] = {'request_id': request_id, 'running': actual.get('scanning')}
event('final_disk_scan_started', request_id=request_id, running=actual.get('scanning'))
wait_space(lambda r: r.get('data', {}).get('scanned', 0) > 0)
time.sleep(.6)
api('cancel', module='space')
saved_partial = wait_space(lambda r: r.get('data', {}).get('done') and r.get('data', {}).get('cancelled') and r.get('data', {}).get('saved_at', 0) > 0 and not r.get('data', {}).get('snapshot_notice'), timeout=60)['data']
event('disk_partial_saved', scanned=saved_partial['scanned'], cancelled=saved_partial['cancelled'])
restart()
real_restored = wait_space(lambda r: r.get('data', {}).get('restored') and r.get('data', {}).get('root') == saved_partial['root'], timeout=60)['data']
assert real_restored['cancelled'] and real_restored['truncated']
assert real_restored['scanned'] >= saved_partial['scanned'] > 0
report['real_disk_restore'] = {'restored': True, 'scanned': real_restored['scanned'], 'cancelled': True, 'partial': True, 'source_volume_preserved': real_restored.get('source_volume') == volume['id']}
OUT.write_text(json.dumps(report, ensure_ascii=False, indent=2))
event('real_disk_partial_restart_restored', **report['real_disk_restore'])
