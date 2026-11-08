//! Install-time helper: turns a plugin `.wasm` into a `.cwasm` for the host's
//! exact engine config. Cranelift lives here so the app never loads it.
//!
//! Usage: `meowtify-plugin-compile <in.wasm> <out.cwasm>`

use std::process::ExitCode;

fn run(input: &str, output: &str) -> Result<(), String> {
    let wasm = std::fs::read(input).map_err(|e| format!("read {input}: {e}"))?;
    let engine = wasmtime::Engine::new(&meowtify_plugin::engine_config()).map_err(|e| e.to_string())?;
    let compiled = engine.precompile_component(&wasm).map_err(|e| format!("compile {input}: {e}"))?;
    // Write beside the target, then rename, so a crash never leaves a half file.
    let tmp = format!("{output}.tmp");
    std::fs::write(&tmp, compiled).map_err(|e| format!("write {tmp}: {e}"))?;
    std::fs::rename(&tmp, output).map_err(|e| format!("rename to {output}: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        eprintln!("usage: meowtify-plugin-compile <in.wasm> <out.cwasm>");
        return ExitCode::from(2);
    };
    match run(input, output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
