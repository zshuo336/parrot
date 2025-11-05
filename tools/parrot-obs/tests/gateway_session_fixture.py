import json,os,signal,socket,subprocess,sys,time
from pathlib import Path
signal.signal(signal.SIGTERM,signal.SIG_IGN)
if sys.argv[1]=='worker':
 os.setpgrp();fd=int(sys.argv[3]);s=socket.socket();s.bind(('127.0.0.1',0));s.listen()
 path=Path(sys.argv[2]);tmp=path.with_suffix('.tmp');tmp.write_text(json.dumps(dict(pid=os.getpid(),session=os.getsid(0),group=os.getpgrp(),port=s.getsockname()[1])));tmp.replace(path)
 while os.read(fd,1):pass
 time.sleep(.8)
else:
 read,write=os.pipe()
 subprocess.Popen([sys.executable,__file__,'worker',sys.argv[2],str(read)],pass_fds=(read,))
 os.close(read)
 while not Path(sys.argv[2]).exists():time.sleep(.01)
 print('READY='+str(json.loads(Path(sys.argv[2]).read_text())['port']),flush=True)
 while True:time.sleep(.1)
