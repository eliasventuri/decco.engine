use decco_core::{config::EngineConfig, DeccoEngine};
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring, JNI_TRUE};
use jni::JNIEnv;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use std::path::PathBuf;
use tokio::runtime::Runtime;
use tracing::info;

struct AndroidEngineRuntime {
    rt: Runtime,
    engine: Mutex<Option<DeccoEngine>>,
}

static ANDROID_RUNTIME: OnceCell<AndroidEngineRuntime> = OnceCell::new();

fn get_runtime() -> &'static AndroidEngineRuntime {
    ANDROID_RUNTIME.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to initialize Tokio runtime for Android");

        AndroidEngineRuntime {
            rt,
            engine: Mutex::new(None),
        }
    })
}

/// JNI Entrypoint: `com.decco.core.engine.RustEngineBridge.startEngine(baseDir: String, port: Int, isTvMode: Boolean): Int`
#[no_mangle]
pub extern "system" fn Java_com_decco_core_engine_RustEngineBridge_startEngine<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    base_dir: JString<'local>,
    port: jint,
    is_tv_mode: jboolean,
) -> jint {
    let base_dir_str: String = match env.get_string(&base_dir) {
        Ok(s) => s.into(),
        Err(_) => return -1,
    };

    let tv_mode = is_tv_mode == JNI_TRUE;
    let runtime = get_runtime();

    {
        let existing = runtime.engine.lock();
        if let Some(ref eng) = *existing {
            let p = *eng.state.torrent_mgr.primary_port.read();
            return p as jint;
        }
    }

    let mut config = EngineConfig::new(PathBuf::from(base_dir_str), tv_mode);
    if port > 0 {
        config.ports = vec![port as u16, 18889, 18888];
    }

    let engine = DeccoEngine::new(config);
    let engine_clone = engine.clone();

    let bound_port = runtime.rt.block_on(async move {
        match engine_clone.start().await {
            Ok(ports) => ports.first().copied().unwrap_or(8888) as jint,
            Err(_) => -1,
        }
    });

    if bound_port > 0 {
        info!(
            "[Android-JNI] Decco Rust Engine started on port {} (tvMode={})",
            bound_port, tv_mode
        );
        *runtime.engine.lock() = Some(engine);
    }

    bound_port
}

/// JNI Entrypoint: `com.decco.core.engine.RustEngineBridge.setMeteredMode(isMetered: Boolean)`
#[no_mangle]
pub extern "system" fn Java_com_decco_core_engine_RustEngineBridge_setMeteredMode<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    is_metered: jboolean,
) {
    let runtime = get_runtime();
    let guard = runtime.engine.lock();
    if let Some(ref engine) = *guard {
        engine
            .state
            .torrent_mgr
            .set_metered_mode(is_metered == JNI_TRUE);
    }
}

/// JNI Entrypoint: `com.decco.core.engine.RustEngineBridge.prewarmTorrent(hash: String)`
#[no_mangle]
pub extern "system" fn Java_com_decco_core_engine_RustEngineBridge_prewarmTorrent<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    hash: JString<'local>,
) {
    let Ok(hash_str) = env.get_string(&hash) else {
        return;
    };
    let clean_hash: String = hash_str.into();
    let runtime = get_runtime();
    let guard = runtime.engine.lock();
    if let Some(ref engine) = *guard {
        let mgr = engine.state.torrent_mgr.clone();
        runtime.rt.spawn(async move {
            mgr.start_or_get_engine(&clean_hash, None, None, None, None)
                .await;
        });
    }
}

/// JNI Entrypoint: `com.decco.core.engine.RustEngineBridge.clearCache(): Boolean`
#[no_mangle]
pub extern "system" fn Java_com_decco_core_engine_RustEngineBridge_clearCache<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jboolean {
    let runtime = get_runtime();
    let guard = runtime.engine.lock();
    if let Some(ref engine) = *guard {
        if engine.clear_cache().is_ok() {
            return JNI_TRUE;
        }
    }
    0
}

/// JNI Entrypoint: `com.decco.core.engine.RustEngineBridge.hasActiveDownloads(): Boolean`
#[no_mangle]
pub extern "system" fn Java_com_decco_core_engine_RustEngineBridge_hasActiveDownloads<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jboolean {
    let runtime = get_runtime();
    let guard = runtime.engine.lock();
    if let Some(ref engine) = *guard {
        let has_active = engine
            .state
            .downloads_mgr
            .active_downloads
            .read()
            .values()
            .any(|dl| dl.meta.status == "downloading" || dl.meta.status == "loading");
        if has_active {
            return JNI_TRUE;
        }
    }
    0
}

/// JNI Entrypoint: `com.decco.core.engine.RustEngineBridge.pauseAllTorrents()`
#[no_mangle]
pub extern "system" fn Java_com_decco_core_engine_RustEngineBridge_pauseAllTorrents<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) {
    let runtime = get_runtime();
    let guard = runtime.engine.lock();
    if let Some(ref engine) = *guard {
        engine.state.torrent_mgr.active_engines.write().clear();
        engine.state.downloads_mgr.pause_all_downloads();
    }
}

/// JNI Entrypoint: `com.decco.core.engine.RustEngineBridge.getDownloadsPayloadJson(): String`
#[no_mangle]
pub extern "system" fn Java_com_decco_core_engine_RustEngineBridge_getDownloadsPayloadJson<
    'local,
>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let runtime = get_runtime();
    let guard = runtime.engine.lock();
    let json_str = if let Some(ref engine) = *guard {
        serde_json::to_string(&engine.state.downloads_mgr.get_downloads_payload())
            .unwrap_or_else(|_| "{\"downloads\":[]}".to_string())
    } else {
        "{\"downloads\":[]}".to_string()
    };

    env.new_string(json_str)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
