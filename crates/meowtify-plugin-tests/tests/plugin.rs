//! End-to-end tests: build the test guest, precompile it in-process, load it
//! through the same `Runtime` the app uses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use meowtify_plugin::{
    engine_config, Control, ErrorKind, HostError, HttpResponse, LoadedPlugin, LogLevel, Manifest,
    PluginError, Runtime, Services, Track, TrustedArtifact, Url, MAX_PLUGINS,
};

#[derive(Default)]
struct Recorder {
    logs: Mutex<Vec<String>>,
    controls: Mutex<Vec<(String, Control)>>,
    storage: Mutex<HashMap<(String, String), Vec<u8>>>,
}

impl Recorder {
    fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().clone()
    }
}

impl Services for Recorder {
    fn log(&self, _plugin: &str, _level: LogLevel, message: &str) {
        self.logs.lock().unwrap().push(message.to_string());
    }

    fn control(&self, plugin: &str, action: Control) -> Result<(), HostError> {
        self.controls.lock().unwrap().push((plugin.to_string(), action));
        Ok(())
    }

    fn fetch(&self, _plugin: &str, _url: &Url) -> Result<HttpResponse, HostError> {
        Ok(HttpResponse { status: 200, body: b"hi".to_vec() })
    }

    fn storage_get(&self, plugin: &str, key: &str) -> Result<Option<Vec<u8>>, HostError> {
        Ok(self.storage.lock().unwrap().get(&(plugin.to_string(), key.to_string())).cloned())
    }

    fn storage_set(&self, plugin: &str, key: &str, value: &[u8]) -> Result<(), HostError> {
        self.storage.lock().unwrap().insert((plugin.to_string(), key.to_string()), value.to_vec());
        Ok(())
    }

    fn storage_delete(&self, plugin: &str, key: &str) -> Result<(), HostError> {
        self.storage.lock().unwrap().remove(&(plugin.to_string(), key.to_string()));
        Ok(())
    }
}

/// Builds the guest once and returns the path of the precompiled component.
fn cwasm() -> &'static PathBuf {
    static CWASM: OnceLock<PathBuf> = OnceLock::new();
    CWASM.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("meowtify-plugin");
        let manifest = root.join("test-guest").join("Cargo.toml");
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "--release", "--target", "wasm32-wasip2", "--manifest-path"])
            .arg(&manifest)
            .status()
            .expect("run cargo to build the test guest");
        assert!(status.success(), "test guest build failed (is the wasm32-wasip2 target installed?)");

        let wasm = root.join("test-guest/target/wasm32-wasip2/release/test_guest.wasm");
        let bytes = std::fs::read(&wasm).expect("read guest wasm");
        let engine = wasmtime::Engine::new(&engine_config()).unwrap();
        let compiled = engine.precompile_component(&bytes).expect("precompile");
        let out = std::env::temp_dir().join(format!("meowtify-test-guest-{}.cwasm", std::process::id()));
        std::fs::write(&out, compiled).unwrap();
        out
    })
}

fn trusted() -> TrustedArtifact {
    // SAFETY: the file was just compiled from the test guest by this build.
    unsafe { TrustedArtifact::assume_trusted(cwasm().clone()) }
}

fn manifest(perms: &str) -> Manifest {
    Manifest::parse(&format!("id='t'\nname='T'\nversion='1'\nwasm='t.wasm'\n{perms}")).unwrap()
}

fn track(title: &str) -> Track {
    Track { id: "id".into(), title: title.into(), artist: "Artist".into(), album: "Album".into(), duration_ms: 1000 }
}

fn setup(perms: &str) -> (Runtime, Arc<Recorder>, LoadedPlugin) {
    let rec = Arc::new(Recorder::default());
    let rt = Runtime::new(rec.clone()).unwrap();
    let p = rt.load(manifest(perms), &trusted()).unwrap();
    (rt, rec, p)
}

#[test]
fn events_reach_the_plugin() {
    let (_rt, rec, mut p) = setup("");
    p.on_track_change(&track("hello")).unwrap();
    p.on_progress(1234).unwrap();
    p.on_queue_change(&[track("a"), track("b")]).unwrap();
    assert_eq!(rec.logs(), ["track hello", "progress 1234", "queue 2"]);
}

#[test]
fn lyrics_round_trip_with_unicode() {
    let (_rt, _rec, mut p) = setup("");
    let l = p.get_lyrics(&track("Song")).unwrap().unwrap();
    assert_eq!(l.lines.len(), 2);
    assert_eq!(l.lines[0].text, "Song by Artist");
    assert_eq!(l.lines[1].text, "第二行 שלום");
    assert_eq!(l.source.as_deref(), Some("test"));
    assert!(p.get_lyrics(&track("none")).unwrap().is_none());
}

#[test]
fn everything_denied_without_permissions() {
    let (_rt, rec, mut p) = setup("");
    p.on_track_change(&track("probe")).unwrap();
    let logs = rec.logs();
    for want in [
        "play=Err(ErrorKind::Denied)",
        "volume=Err(ErrorKind::Denied)",
        "storage=Err(ErrorKind::Denied)",
        "storage_get=Err(ErrorKind::Denied)",
        "fetch_ok=Err(ErrorKind::Denied)",
        "fetch_evil=Err(ErrorKind::Denied)",
        "fetch_bad=Err(ErrorKind::Denied)",
    ] {
        assert!(logs.iter().any(|l| l == want), "missing {want} in {logs:?}");
    }
    assert!(rec.controls.lock().unwrap().is_empty());
}

#[test]
fn granted_permissions_work_and_allowlist_holds() {
    let (_rt, rec, mut p) = setup("[permissions]\ncontrol = true\nstorage = true\nfetch = ['a.example']\n");
    p.on_track_change(&track("probe")).unwrap();
    let logs = rec.logs();
    for want in [
        "play=Ok(\"ok\")",
        "volume=Err(ErrorKind::Invalid)",
        "storage=Ok(\"ok\")",
        "storage_get=Ok(Some(1))",
        "fetch_ok=Ok(200)",
        "fetch_evil=Err(ErrorKind::Denied)",
        "fetch_bad=Err(ErrorKind::Invalid)",
    ] {
        assert!(logs.iter().any(|l| l == want), "missing {want} in {logs:?}");
    }
    assert_eq!(rec.controls.lock().unwrap().as_slice(), [("t".to_string(), Control::Play)]);
}

#[test]
fn runaway_plugin_is_stopped_and_marked_failed() {
    let (rt, rec, mut bad) = setup("");
    let mut bystander = rt.load(manifest(""), &trusted()).unwrap();

    let start = Instant::now();
    let err = bad.on_track_change(&track("spin")).unwrap_err();
    let took = start.elapsed();
    assert!(matches!(err, PluginError::Failed(_)), "{err:?}");
    assert!(took < Duration::from_secs(2), "spin took {took:?}");
    assert!(bad.failure().is_some());
    assert!(matches!(bad.on_progress(1), Err(PluginError::Failed(_))));

    bystander.on_track_change(&track("still ok")).unwrap();
    assert!(rec.logs().contains(&"track still ok".to_string()));
}

#[test]
fn memory_hog_is_stopped() {
    let (_rt, _rec, mut p) = setup("");
    let err = p.on_track_change(&track("hog")).unwrap_err();
    assert!(matches!(err, PluginError::Failed(_)), "{err:?}");
}

#[test]
fn plugin_slots_are_limited_and_released() {
    let rec = Arc::new(Recorder::default());
    let rt = Runtime::new(rec).unwrap();
    let mut held: Vec<LoadedPlugin> = (0..MAX_PLUGINS).map(|_| rt.load(manifest(""), &trusted()).unwrap()).collect();
    assert!(matches!(rt.load(manifest(""), &trusted()), Err(PluginError::TooManyPlugins)));
    held.pop();
    held.push(rt.load(manifest(""), &trusted()).expect("slot freed on drop"));
}

#[test]
fn theme_only_package_has_no_code() {
    let rt = Runtime::new(Arc::new(Recorder::default())).unwrap();
    let m = Manifest::parse("id='th'\nname='Theme'\nversion='1'\n").unwrap();
    assert!(matches!(rt.load(m, &trusted()), Err(PluginError::NoCode)));
}

#[test]
fn missing_cwasm_is_an_error_not_a_panic() {
    let rt = Runtime::new(Arc::new(Recorder::default())).unwrap();
    let r = rt.load(manifest(""), &unsafe { TrustedArtifact::assume_trusted(PathBuf::from("does-not-exist.cwasm")) });
    assert!(matches!(r, Err(PluginError::Wasm(_))));
}

#[test]
fn error_kinds_are_exported() {
    // Compile-time check that the public surface names the WIT error kinds.
    let _ = ErrorKind::Denied;
}
