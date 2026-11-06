# Decco Engine (Rust Cross-Platform Edition)

Motor unificado en **Rust** para **Windows Desktop** y **Android / Android TV**, con paridad 100% de características respecto al motor original de Node.js/Electron (`decco-engine/main.js`) y `core-engine` de Android (`EngineServer.kt` + `TorrentManager.kt`).

---

## Arquitectura del Workspace

```text
decco-engine-rust/
├── Cargo.toml                  # Workspace raíz
├── crates/
│   ├── decco-core/             # Motor compartido: BitTorrent (librqbit), Axum HTTP Server, Descargas, Caché 72h, FFmpeg/HLS, Live TV Proxy
│   ├── decco-desktop/          # Binario nativo Windows (.exe): System Tray, Single Instance, protocolo decco://, Auto-Start, Auto-Updater
│   └── decco-android/          # Biblioteca nativa Android (.so): Bindings JNI para Mobile y Android TV
```

---

## Compilación

### 1. Requisitos previos
Instala el toolchain oficial de Rust desde [rustup.rs](https://rustup.rs/):
```powershell
winget install Rustlang.Rustup
```

### 2. Compilar para Windows Desktop (`decco-engine.exe`)
```powershell
cd decco-engine/decco-engine-rust
cargo build --release -p decco-desktop
```
El ejecutable optimizado se generará en:
`target/release/decco-engine.exe`

### 3. Compilar para Android / Android TV (`libdecco_engine.so`)
1. Instala los targets de Android y `cargo-ndk`:
   ```powershell
   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android
   cargo install cargo-ndk
   ```
2. Compila y exporta directamente las librerías `.so` hacia `decco-android-v2/core-engine`:
   ```powershell
   cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -t x86 -o ../../decco-android-v2/core-engine/src/main/jniLibs build --release -p decco-android
   ```

---

## Uso desde Kotlin en Android (`RustEngineBridge.kt`)

```kotlin
package com.decco.core.engine

object RustEngineBridge {
    init {
        System.loadLibrary("decco_engine")
    }

    external fun startEngine(baseDir: String, port: Int, isTvMode: Boolean): Int
    external fun setMeteredMode(isMetered: Boolean)
    external fun prewarmTorrent(hash: String)
    external fun clearCache(): Boolean
}
```

