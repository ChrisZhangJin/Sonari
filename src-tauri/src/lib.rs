//! Sonari 桌面壳 —— 薄壳层：Tauri 命令直接委托 `sonari_engine`（与 WS 服务端
//! 共用同一份实现，JSON 契约一致，前端代码零改动可用）。
//!
//! 对标 ocenaudio：libqtocen（Qt 表现层宿主）。播放/录音走引擎 cpal
//! （`audio_*` 命令，真实设备选择），其余命令走 `commands` 命令总线。

use std::sync::{Arc, RwLock};

use sonari_engine::audio::{self, AudioState};
use sonari_engine::commands::{self, Docs};
use serde_json::Value;
use tauri::State;

#[derive(Clone, Default)]
pub struct AppState {
    docs: Arc<RwLock<Docs>>,
    audio: Arc<AudioState>,
}

/// 单一命令入口：前端 invoke('engine', { cmd, p })，按命令名路由。
#[tauri::command]
async fn engine(state: State<'_, AppState>, cmd: String, p: Value) -> Result<Value, String> {
    let docs = state.docs.clone();
    let audio = state.audio.clone();
    tokio::task::spawn_blocking(move || {
        // 命令分类由 `sonari_engine` 单点定义 —— 新增命令不必再改宿主
        if sonari_engine::is_audio_cmd(&cmd) {
            let docs = docs.read().unwrap();
            audio::dispatch(&audio, &docs, &cmd, &p)
        } else if sonari_engine::is_write_cmd(&cmd) {
            let mut docs = docs.write().unwrap();
            commands::dispatch_write(&mut docs, &cmd, &p)
        } else {
            let docs = docs.read().unwrap();
            commands::dispatch_read(&docs, &cmd, &p)
        }
    })
    .await
    .map_err(|e| format!("join: {e}"))?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![engine])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
