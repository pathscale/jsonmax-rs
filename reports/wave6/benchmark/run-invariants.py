from run_stage5 import run,OUT,ROOT
import subprocess,shutil
subprocess.run(['python3',str(OUT/'prepare-invariant-probe.py')],check=True)
shutil.copy2(OUT/'invariant-main.rs',OUT/'invariant-probe/src/main.rs')
run('build-invariants',['cargo','+1.98.1','build','--release','--manifest-path',str(OUT/'invariant-probe/Cargo.toml'),'--offline'],{'CARGO_TARGET_DIR':str(ROOT/'target/wave6-invariant-probe')})
run('invariants',[str(ROOT/'target/wave6-invariant-probe/release/wave6-invariant-probe')],{'WAVE6_CORPUS_DIR':str(OUT/'generated-corpora')})
