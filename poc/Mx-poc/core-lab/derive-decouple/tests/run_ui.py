"""Compile contract fixtures against Cargo-reported artifacts, never guessed rlibs."""
from pathlib import Path
import argparse,json,os,subprocess,tempfile

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--target-dir',required=True);parser.add_argument('--output-dir',required=True);args=parser.parse_args()
    root=Path(__file__).resolve().parents[2];out=Path(args.output_dir).resolve();out.mkdir(parents=True,exist_ok=True)
    cmd=['cargo','build','--locked','--manifest-path',str(root/'Cargo.toml'),'--target-dir',str(Path(args.target_dir).resolve()),'-p','derive-decouple','-p','api-neutral','--message-format=json']
    result=subprocess.run(cmd,text=True,capture_output=True,timeout=180);(out/'build.jsonl').write_text(result.stdout);(out/'build-stderr.txt').write_text(result.stderr)
    if result.returncode:raise RuntimeError('UI prerequisite build failed')
    artifacts={}
    for line in result.stdout.splitlines():
        row=json.loads(line)
        if row.get('reason')!='compiler-artifact':continue
        name=row['target']['name']
        if name=='api_neutral': artifacts[name]=next(p for p in row['filenames'] if p.endswith('.rlib'))
        if name=='derive_decouple': artifacts[name]=next(p for p in row['filenames'] if Path(p).suffix in {'.so','.dylib','.dll'})
    assert set(artifacts)=={'api_neutral','derive_decouple'},artifacts
    cases=json.loads((Path(__file__).parent/'ui/cases.json').read_text());rows=[]
    with tempfile.TemporaryDirectory(prefix='parrot-ui-') as tmp:
        for case in cases:
            source=Path(__file__).parent/'ui'/case['file'];binary=Path(tmp)/source.stem
            cmd=[os.environ.get('RUSTC','rustc'),'--edition=2021','--crate-name',source.stem,str(source),'-L','dependency='+str(Path(artifacts['api_neutral']).parent),'--extern','derive_decouple='+artifacts['derive_decouple'],'--extern',case.get('api_name','api_neutral')+'='+artifacts['api_neutral'],'-o',str(binary)]
            compile_result=subprocess.run(cmd,capture_output=True,text=True,timeout=60);(out/(source.stem+'.txt')).write_text(compile_result.stdout+compile_result.stderr)
            run_exit=None
            if case['pass']:
                passed=compile_result.returncode==0
                if passed:
                    run=subprocess.run([str(binary)],capture_output=True,text=True,timeout=30);(out/(source.stem+'-run.txt')).write_text(run.stdout+run.stderr);run_exit=run.returncode;passed=run_exit==0
            else:
                passed=compile_result.returncode!=0 and all(s in compile_result.stderr for s in case['diagnostics'])
            rows.append(dict(file=case['file'],expected_pass=case['pass'],compile_exit=compile_result.returncode,run_exit=run_exit,passed=passed,command=cmd));print({k:v for k,v in rows[-1].items() if k!='command'},flush=True)
    (out/'results.json').write_text(json.dumps(rows,indent=2)+'\n');return 0 if all(r['passed'] for r in rows) else 1
if __name__=='__main__':raise SystemExit(main())
