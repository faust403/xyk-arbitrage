#!/usr/bin/env python3
import http.server, socketserver, subprocess, json, re, urllib.request, threading, time, datetime, socket

PORT = 8081
WALLET = '7MuyonxJKBKqtgwhhYmLU11WZTNFiDeaNn9JBHnk4DWJ'
RPC = 'https://mainnet.helius-rpc.com/?api-key=e0805d12-5ab6-4d17-a854-a28837344689'
POLL_SECONDS = 30

ANSI = re.compile(r'\x1b\[[0-9;]*m')

_cache = {'html': '<html><body><h1>Warming up…</h1></body></html>', 'ts': 0}
_lock = threading.Lock()

def rpc(method, params):
    body = json.dumps({'jsonrpc':'2.0','id':1,'method':method,'params':params}).encode()
    req = urllib.request.Request(RPC, data=body, headers={'Content-Type':'application/json'})
    with urllib.request.urlopen(req, timeout=8) as r:
        return json.load(r)

def gather():
    r = subprocess.run(['systemctl', 'is-active', 'arbbot'], capture_output=True, text=True)
    svc = r.stdout.strip()
    sol = rpc('getBalance',[WALLET])['result']['value'] / 1e9
    toks = rpc('getTokenAccountsByOwner',[WALLET,{'programId':'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'},{'encoding':'jsonParsed'}])['result']['value']
    usdc=usdt=wbtc=0.0
    for a in toks:
        i = a['account']['data']['parsed']['info']; m = i['mint']
        amt = float(i['tokenAmount']['uiAmountString'] or 0)
        if m == 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v': usdc = amt
        elif m == 'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB': usdt = amt
        elif m == '3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh': wbtc = amt
    total = sol*95 + usdc + usdt + wbtc*79587
    rr = subprocess.run('sudo grep -cE "LIQUIDATION SENT|LIQUIDATION LANDED|reverted \\(likely stale|LIQUIDATION CANDIDATE" /var/log/arbbot.log', shell=True, capture_output=True, text=True)
    counts = {}
    for label, pat in [('sent','LIQUIDATION SENT'),('landed','LIQUIDATION LANDED'),('reverted','reverted (likely stale'),('candidates','LIQUIDATION CANDIDATE')]:
        rr = subprocess.run(['sudo','grep','-c',pat,'/var/log/arbbot.log'], capture_output=True, text=True)
        counts[label] = int(rr.stdout.strip() or 0)
    rr = subprocess.run('sudo grep -E "LIQUIDATION SENT|LANDED|reverted" /var/log/arbbot.log | tail -10', shell=True, capture_output=True, text=True)
    recent = [ANSI.sub('', l) for l in rr.stdout.split('\n') if l.strip()]
    sigs = rpc('getSignaturesForAddress',[WALLET,{'limit':5}])['result']
    return {'svc':svc,'sol':sol,'usdc':usdc,'usdt':usdt,'wbtc':wbtc,'total':total,'counts':counts,'recent':recent,'sigs':sigs}

def render(d, age_s):
    color = '#4CAF50' if d['svc']=='active' else '#f44336'
    sigs_html = ''
    for s in d['sigs'][:5]:
        bt = s.get('blockTime',0)
        dt = datetime.datetime.fromtimestamp(bt,datetime.timezone.utc).strftime('%H:%M:%S') if bt else '?'
        err = 'ERR' if s.get('err') else 'OK'
        ec = '#f44336' if err=='ERR' else '#4CAF50'
        sig = s.get('signature','')
        sigs_html += f'<li><span style="color:{ec};">{err}</span> {dt} <a href="https://solscan.io/tx/{sig}" target="_blank" style="color:#9cf;">{sig[:32]}...</a></li>'
    log_html = ''
    for l in d['recent'][-10:]:
        cls = '#4CAF50' if 'LANDED' in l else ('#FFC107' if 'reverted' in l else '#ddd')
        log_html += f'<li style="color:{cls};"><pre>{l[:240]}</pre></li>'
    return f'''<!DOCTYPE html><html><head><meta charset="UTF-8"><title>arbbot</title><meta http-equiv="refresh" content="30"><style>
body{{font-family:monospace;background:#0a0a0a;color:#ddd;padding:20px;max-width:1200px;margin:auto;}}
h1{{color:#4CAF50;}}.box{{background:#1a1a1a;padding:15px;margin-bottom:15px;border-radius:8px;border-left:3px solid #4CAF50;}}
.big{{font-size:2.5em;font-weight:bold;}}table{{width:100%;}}td,th{{padding:8px;border-bottom:1px solid #333;text-align:left;}}
pre{{font-size:0.75em;white-space:pre-wrap;word-wrap:break-word;margin:0;}}li{{list-style:none;padding:8px;border-bottom:1px solid #222;}}
.green{{color:#4CAF50;}}.red{{color:#f44336;}}.yellow{{color:#FFC107;}}a{{text-decoration:none;}}
</style></head><body>
<h1>arbbot dashboard</h1>
<div class="box"><h2>Service: <span style="color:{color};">{d['svc']}</span></h2></div>
<div class="box"><h2>Wallet</h2><table>
<tr><td>SOL</td><td>{d['sol']:.4f}</td><td>${d['sol']*95:.2f}</td></tr>
<tr><td>USDC</td><td>{d['usdc']:.2f}</td><td>${d['usdc']:.2f}</td></tr>
<tr><td>USDT</td><td>{d['usdt']:.2f}</td><td>${d['usdt']:.2f}</td></tr>
<tr><td>WBTC</td><td>{d['wbtc']:.8f}</td><td>${d['wbtc']*79587:.2f}</td></tr>
<tr><th>TOTAL</th><th></th><th class="green big">${d['total']:.2f}</th></tr></table></div>
<div class="box"><h2>Liquidator (cumulative)</h2><table>
<tr><td>CANDIDATEs</td><td class="big">{d['counts']['candidates']}</td></tr>
<tr><td>SENT</td><td class="big">{d['counts']['sent']}</td></tr>
<tr><td class="yellow">Reverted</td><td class="big yellow">{d['counts']['reverted']}</td></tr>
<tr><td class="green">LANDED</td><td class="big green">{d['counts']['landed']}</td></tr></table></div>
<div class="box"><h2>Recent wallet txs</h2><ul>{sigs_html}</ul></div>
<div class="box"><h2>Recent log events</h2><ul>{log_html}</ul></div>
<p style="color:#666;">data age: {age_s}s • cache refresh every {POLL_SECONDS}s • <a href="/logs" style="color:#9cf;">live logs ↗</a> • <a href="https://solscan.io/account/{WALLET}" target="_blank" style="color:#9cf;">view wallet on solscan</a></p>
</body></html>'''

def poller():
    while True:
        try:
            d = gather()
            html = render(d, 0)
            with _lock:
                _cache['html'] = html
                _cache['ts']   = time.time()
        except Exception as e:
            with _lock:
                _cache['html'] = f'<html><body><h1>Poller error</h1><pre>{e}</pre></body></html>'
                _cache['ts']   = time.time()
        time.sleep(POLL_SECONDS)

LOG_FILE = '/var/log/arbbot.log'

LOGS_PAGE = '''<!DOCTYPE html><html><head><meta charset="UTF-8"><title>arbbot logs</title>
<style>
body{font-family:monospace;background:#0a0a0a;color:#ddd;margin:0;padding:0;}
header{background:#1a1a1a;padding:10px 16px;border-bottom:1px solid #333;display:flex;gap:12px;align-items:center;}
header h1{margin:0;color:#4CAF50;font-size:16px;}
header input,header select,header button{background:#0a0a0a;color:#ddd;border:1px solid #333;padding:4px 8px;}
header button{cursor:pointer;}header button:hover{background:#222;}
#log{padding:8px 16px;font-size:12px;white-space:pre-wrap;line-height:1.4;height:calc(100vh - 60px);overflow:auto;}
.bundle{color:#4CAF50;}.revert{color:#FFC107;}.error{color:#f44336;}.cleared{color:#9cf;}
</style></head><body>
<header>
<h1>arbbot logs</h1>
<label>filter: <input id="filter" type="text" placeholder="BUNDLE|LIQUID|cleared" value=""></label>
<label>lines: <select id="lines"><option>100</option><option selected>200</option><option>500</option><option>1000</option></select></label>
<label>refresh: <select id="rate"><option value="2">2s</option><option value="5" selected>5s</option><option value="10">10s</option><option value="0">paused</option></select></label>
<button onclick="bottom()">↓ bottom</button>
<span id="status" style="color:#666;margin-left:auto;">loading…</span>
</header>
<pre id="log"></pre>
<script>
const lg=document.getElementById('log'),st=document.getElementById('status');
let userScrolled=false;
lg.addEventListener('scroll',()=>{userScrolled=(lg.scrollTop+lg.clientHeight<lg.scrollHeight-50);});
function bottom(){lg.scrollTop=lg.scrollHeight;userScrolled=false;}
async function tick(){
  const n=document.getElementById('lines').value;
  const f=document.getElementById('filter').value;
  try{
    const r=await fetch('/logs/raw?n='+n+(f?'&f='+encodeURIComponent(f):''));
    const text=await r.text();
    const lines=text.split('\\n').map(l=>{
      if(l.includes('BUNDLE LANDED')||l.includes('LIQUIDATION LANDED'))return '<span class="bundle">'+l+'</span>';
      if(l.includes('reverted')||l.includes('DROPPED'))return '<span class="revert">'+l+'</span>';
      if(l.includes('ERROR')||l.includes('WARN'))return '<span class="error">'+l+'</span>';
      if(l.includes('cleared')||l.includes('🎯')||l.includes('📈'))return '<span class="cleared">'+l+'</span>';
      return l;
    });
    lg.innerHTML=lines.join('\\n');
    st.textContent=new Date().toLocaleTimeString()+' '+lines.length+' lines';
    if(!userScrolled)bottom();
  }catch(e){st.textContent='err: '+e;}
}
let timer=null;
function reschedule(){
  if(timer)clearInterval(timer);
  const r=parseInt(document.getElementById('rate').value);
  if(r>0)timer=setInterval(tick,r*1000);
}
document.getElementById('rate').onchange=reschedule;
document.getElementById('filter').onchange=tick;
document.getElementById('lines').onchange=tick;
tick();reschedule();
</script></body></html>'''

def tail_log(n=200, filter_re=None):
    try:
        r = subprocess.run(['sudo','tail','-n',str(n),LOG_FILE], capture_output=True, text=True, timeout=5)
        out = ANSI.sub('', r.stdout)
        if filter_re:
            try:
                import re as _re
                pat = _re.compile(filter_re)
                out = '\n'.join(l for l in out.split('\n') if pat.search(l))
            except Exception:
                pass
        return out
    except Exception as e:
        return f'(tail failed: {e})'

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        from urllib.parse import urlparse, parse_qs
        u = urlparse(self.path)
        if u.path == '/logs':
            self.send_response(200)
            self.send_header('Content-Type','text/html; charset=utf-8')
            self.end_headers()
            self.wfile.write(LOGS_PAGE.encode())
            return
        if u.path == '/logs/raw':
            q = parse_qs(u.query)
            n = int(q.get('n',['200'])[0])
            f = q.get('f',[None])[0]
            text = tail_log(n, f)
            self.send_response(200)
            self.send_header('Content-Type','text/plain; charset=utf-8')
            self.end_headers()
            self.wfile.write(text.encode())
            return
        with _lock:
            html = _cache['html']
            age  = int(time.time() - _cache['ts']) if _cache['ts'] else -1
        if age >= 0 and '<meta' in html and '%</p>' not in html:
            html = html.replace('data age: 0s', f'data age: {age}s')
        self.send_response(200)
        self.send_header('Content-Type','text/html; charset=utf-8')
        self.end_headers()
        self.wfile.write(html.encode())
    def log_message(self,*a): pass

class ReusePortServer(socketserver.TCPServer):
    allow_reuse_address = True
    def server_bind(self):
        if hasattr(socket, 'SO_REUSEPORT'):
            self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        super().server_bind()

if __name__ == '__main__':
    threading.Thread(target=poller, daemon=True).start()
    print(f'Dashboard listening on :{PORT}, polling every {POLL_SECONDS}s')
    ReusePortServer(('0.0.0.0',PORT), H).serve_forever()
