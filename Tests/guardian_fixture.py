"""只在独立HOME验证管家；不执行真实文件清理、不启用launchd。"""
import json, os, pathlib, subprocess, tempfile
root = pathlib.Path(tempfile.mkdtemp(prefix='guardian-fixture-', dir='/Users/Shared'))
support = root / 'Library/Application Support/KaruoCleaner'
support.mkdir(parents=True)
exe = '/Applications/卡若清理.app/Contents/MacOS/KaruoCleaner'
env = dict(os.environ, HOME=str(root))
def put(name, value):
    p = support / name
    p.write_text(json.dumps(value))
    p.chmod(0o600)
def check():
    return json.loads(subprocess.check_output([exe, '--guardian-check'], env=env))
base = check()
assert base['preferences']['cache_days'] == 30 and not base['binding']['verified']
put('guardian.json', dict(cache_days=60, log_days=180, clean_cache=False, clean_logs=True))
a = check()
assert a['preferences']['cache_days'] == 60 and not a['preferences']['clean_cache']
assert a['preferences'] == check()['preferences']
put('history-rust.json', dict(records=[dict(id='fixture', date=1, original_path=str(root/'keep'), trash_path=str(root/'trash'), size=1, restored=True, inode=1, device=1)], intents=[]))
assert check()['recovered_count'] == 1
put('schedule.json', dict(enabled=True, frequency='weekly', hour=10, minute=0, mode='scan'))
result = json.loads(subprocess.check_output([exe, '--scheduled-run'], env=env))
assert result['moved'] == 0 and result['mode'] == 'scan'
assert json.loads((support/'schedule-last-run.json').read_text())['status'] == 'scanned'
put('guardian.json', dict(cache_days=1, log_days=90, clean_cache=True, clean_logs=True))
assert check()['error']
put('schedule.json', dict(enabled=True, frequency='weekly', hour=10, minute=0, mode='trash'))
paused = json.loads(subprocess.check_output([exe, '--scheduled-run'], env=env))
assert paused['status'] == 'paused' and paused['moved'] == 0
print(json.dumps(dict(checks=7, result='passed', fixture=str(root), real_files_deleted=0), ensure_ascii=False))
