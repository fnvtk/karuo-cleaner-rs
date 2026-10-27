"""Verify stopping the real smart-clean pipeline and restoring only its own generated cache fixtures."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

exe=str(Path(sys.argv[1] if len(sys.argv)>1 else 'dist/轻理AI.app/Contents/MacOS/KaruoCleaner').resolve())
root=Path(tempfile.mkdtemp(prefix='cm-stop-',dir=str(Path.home()))).resolve()
cache=root/'Library/Caches/com.cleanmac.acceptance'
cache.mkdir(parents=True)
payload=b'cleanmac disposable acceptance cache\n'*120
files=[]
for n in range(4000):
    path=cache/f'cm-stop-fixture-{n:04d}.cache'
    path.write_bytes(payload)
    old=time.time()-30*86400
    os.utime(path,(old,old))
    files.append(path)
env=dict(os.environ,HOME=str(root));log=(root/'app.log').open('w')
p=subprocess.Popen([exe],env=env,stdout=log,stderr=log)
def api(request):
    r=subprocess.run([exe,'--api','-'],input=json.dumps(request),env=env,text=True,capture_output=True,timeout=30)
    if r.returncode:raise AssertionError(r.stderr)
    v=json.loads(r.stdout)
    if v.get('error'):raise AssertionError(v)
    return v

def apply(action):
    preview=api({'cmd':'preview','action':action})
    return api({'cmd':'apply','preview_id':preview['preview_id'],'confirmed':True})

success=False
try:
    for _ in range(100):
        if p.poll() is not None:raise AssertionError((root/'app.log').read_text())
        if (root/'Library/Application Support/KaruoCleaner/control.sock').exists():break
        time.sleep(.05)
    api({'cmd':'ready'})
    apply({'cmd':'smart_clean'})
    stopped=False
    for _ in range(400):
        status=api({'cmd':'smart_clean_status'})
        if status.get('running') and status.get('phase')=='move' and status.get('moved_bytes',0)>0:
            api({'cmd':'cancel','module':'smart'});stopped=True;break
        if status.get('running') is False:break
        time.sleep(.02)
    assert stopped,('No partial moving boundary observed',status,root)
    for _ in range(400):
        status=api({'cmd':'smart_clean_status'})
        if not status.get('running'):break
        time.sleep(.02)
    assert status['phase']=='cancelled',status
    records=api({'cmd':'history'})['records']
    assert 0<len(records)<len(files),(len(records),status)
    assert len(records)==status['moved_count']==status['processed']
    assert sum(not f.exists() for f in files)==len(records)
    assert all(Path(r['original_path']).is_relative_to(cache) for r in records)
    apply({'cmd':'restore','ids':[r['id'] for r in records]})
    assert all(f.read_bytes()==payload for f in files)
    assert not api({'cmd':'history'})['records']
    evidence={'scenario':'native-smart-stop-and-restore','fixture_files':len(files),'moved_before_stop':len(records),'unmoved_files_untouched':True,'all_original_bytes_restored':True,'phase':status['phase'],'processed':status['processed'],'moved_bytes':status['moved_bytes'],'user_files_touched':False}
    out=Path('release-assets/acceptance/smart-stop.json');out.parent.mkdir(parents=True,exist_ok=True);out.write_text(json.dumps(evidence,ensure_ascii=False,indent=2))
    print('PASS',json.dumps(evidence,ensure_ascii=False),flush=True)
    success=True
finally:
    # If verification fails, preserve the journal and fixture directory for recovery.
    if p.poll() is None:p.terminate();p.wait(10)
    log.close()
    if success:shutil.rmtree(root)
    else:print('Recovery evidence retained at',root,flush=True)
