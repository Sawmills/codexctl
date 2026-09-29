#!/usr/bin/env python3
"""Real native Codex TUI, remote use command, and a local project file."""
import argparse,os,pathlib,tempfile,subprocess,json,pty,fcntl,termios,struct,select,time,signal,re,uuid
p=argparse.ArgumentParser();p.add_argument('--bin-dir',required=True);p.add_argument('--server',required=True);p.add_argument('--device',required=True);p.add_argument('--receipt',required=True);a=p.parse_args()
bins=pathlib.Path(a.bin_dir).resolve();child=None;master=None
with tempfile.TemporaryDirectory(prefix='codexctl-native-machine-') as d:
 root=pathlib.Path(d);home=root/'.codex';home.mkdir(mode=0o700);nonce='NATIVE_'+uuid.uuid4().hex[:12].upper();(root/'proof.txt').write_text(nonce)
 env=os.environ.copy();env['HOME']=str(root);env.pop('CODEX_HOME',None);env.pop('CODEXCTL_PINNED_ALIAS',None);env['TERM']='xterm-256color'
 base='model = "gpt-6.1-sol"\nmodel_reasoning_effort = "low"\napproval_policy = "never"\nsandbox_mode = "read-only"\n[projects.'+json.dumps(str(root))+']\ntrust_level = "trusted"\n'
 (home/'config.toml').write_text(base)
 def check(args):
  r=subprocess.run(list(map(str,args)),env=env,cwd=root,capture_output=True,text=True,timeout=120)
  if r.returncode:raise RuntimeError('prototype client failed: '+r.stderr.strip())
  return r.stdout
 try:
  check([bins/'codexctl-central','connect','--alias','personal','--server',a.server,'--token-file',a.device])
  switched=check([bins/'codexctl','use'])
  master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',40,160,0,0))
  prompt='Read proof.txt in this project with a tool, then reply with only its exact contents.'
  child=subprocess.Popen(['codex','--no-alt-screen','-C',str(root),prompt],stdin=slave,stdout=slave,stderr=slave,env=env,start_new_session=True);os.close(slave)
  output=b'';deadline=time.monotonic()+120;rendered=b''
  while time.monotonic()<deadline and child.poll() is None:
   if select.select([master],[],[],.2)[0]:
    try:chunk=os.read(master,65536)
    except OSError:break
    output+=chunk
    if b'\x1b[6n' in chunk:os.write(master,b'\x1b[1;1R')
    if b'\x1b[c' in chunk:os.write(master,b'\x1b[?1;2c')
    if b'\x1b]10;?' in chunk:os.write(master,b'\x1b]10;rgb:ffff/ffff/ffff\x1b\\')
    if b'\x1b]11;?' in chunk:os.write(master,b'\x1b]11;rgb:0000/0000/0000\x1b\\')
   rendered=re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]',b'',output);rendered=re.sub(rb'\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)',b'',rendered)
   if b'\xe2\x80\xa2 '+nonce.encode() in rendered:break
  passed=b'\xe2\x80\xa2 '+nonce.encode() in rendered
  has_refresh=any('refresh_token' in path.read_text(errors='replace') for path in (root/'.codexctl').rglob('*.json'))
  receipt={'native_tui_passed':passed,'codex_version':subprocess.check_output(['codex','--version'],text=True).strip(),'use_command_passed':'switched to remote account personal' in switched,'local_file_nonce':nonce,'client_auth_file_absent':not(home/'auth.json').exists(),'refresh_token_on_client':has_refresh}
  pathlib.Path(a.receipt).write_text(json.dumps(receipt,indent=2));pathlib.Path(a.receipt+'.terminal.txt').write_bytes(rendered)
  print(json.dumps(receipt),flush=True)
  if not (passed and receipt['use_command_passed'] and receipt['client_auth_file_absent'] and not has_refresh):raise RuntimeError('native TUI acceptance failed; see receipt')
 finally:
  if child and child.poll() is None:
   os.killpg(child.pid,signal.SIGTERM)
   try:child.wait(timeout=5)
   except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait()
  subprocess.run(['codex','app-server','daemon','stop'],env=env,capture_output=True,timeout=20)
  if master is not None:os.close(master)
