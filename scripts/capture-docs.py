"""Capture fictional web UI and actual export fixtures. Requires websocket-client and Edge."""
import argparse
import base64
import json
import pathlib
import subprocess
import tempfile
import time
import urllib.request
import websocket

parser = argparse.ArgumentParser()
parser.add_argument('--edge', required=True)
parser.add_argument('--fixture', required=True)
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parents[1]
output = root / 'docs/screenshots'
output.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='qza-docs-') as profile:
    process = subprocess.Popen([args.edge, '--headless=new', '--no-first-run',
        '--remote-debugging-port=19329', '--remote-allow-origins=*',
        '--allow-file-access-from-files', '--user-data-dir=' + profile,
        'http://127.0.0.1:1420/#/archives'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                tabs = json.load(urllib.request.urlopen('http://127.0.0.1:19329/json'))
                target = next(t for t in tabs if '1420' in t['url'])
                break
            except Exception:
                time.sleep(.2)
        ws = websocket.create_connection(target['webSocketDebuggerUrl'], timeout=30)
        seq = 0
        def call(method, params=None):
            global seq
            seq += 1
            ws.send(json.dumps({'id': seq, 'method': method, 'params': params or {}}))
            while True:
                result = json.loads(ws.recv())
                if result.get('id') == seq:
                    if 'error' in result:
                        raise RuntimeError(result['error'])
                    return result.get('result', {})
        def evaluate(expression):
            return call('Runtime.evaluate', {'expression': expression, 'returnByValue': True}).get('result', {}).get('value')
        def screenshot(name):
            time.sleep(1)
            data = call('Page.captureScreenshot', {'format': 'png'})['data']
            (output / name).write_bytes(base64.b64decode(data))
        call('Emulation.setDeviceMetricsOverride', {'width': 1440, 'height': 1000, 'deviceScaleFactor': 1, 'mobile': False})
        for _ in range(100):
            if evaluate("document.body.innerText.includes('分享 ZIP')"):
                break
            time.sleep(.2)
        time.sleep(2)
        screenshot('01-archives.png')
        assert evaluate("(()=>{const b=[...document.querySelectorAll('button')].find(x=>x.innerText.includes('分享 ZIP'));if(!b)return false;b.click();return true})()")
        screenshot('02-export-options.png')
        call('Page.navigate', {'url': pathlib.Path(args.fixture).resolve().as_uri()})
        time.sleep(2)
        assert evaluate("document.body.innerText.includes('虚构演示数据')")
        screenshot('03-offline-reader.png')
        ws.close()
    finally:
        process.terminate()
        process.wait(timeout=20)
print('Captured three fictional documentation screenshots.')
