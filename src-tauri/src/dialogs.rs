//! 文件打开 / 保存对话框。
//!
//! Linux：不走 tauri-plugin-dialog。其底层 rfd 0.16 的 GTK3 后端会另起一条
//! 「全局 GTK 线程」再 `gtk_init` + `gtk_main_iteration`，与 Tauri（tao）主线程
//! 的 GTK 主循环争抢同一个默认 GMainContext；且 Linux 下插件不设父窗口，对话框
//! 不是 transient，可能弹在主窗口背后。表现为「打开」只能点一次。
//! 这里直接在 Tauri 主线程上建 GtkFileChooserDialog，并挂到主窗口下（模态）。
//!
//! 其它平台：沿用插件（带父窗口）。

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Filter {
    pub name: String,
    pub extensions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DialogOpts {
    /// "open" | "save"
    pub mode: String,
    pub title: Option<String>,
    pub default_path: Option<String>,
    #[serde(default)]
    pub filters: Vec<Filter>,
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub async fn file_dialog(
    window: tauri::WebviewWindow,
    opts: DialogOpts,
) -> Result<Option<String>, String> {
    use gtk::prelude::*;
    use gtk::{FileChooserAction, FileChooserDialog, FileFilter, ResponseType};
    use std::path::Path;
    use std::sync::Mutex;

    let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
    let w = window.clone();
    window
        .run_on_main_thread(move || {
            let save = opts.mode == "save";
            let action = if save { FileChooserAction::Save } else { FileChooserAction::Open };
            let parent = w.gtk_window().ok();
            let title = opts.title.unwrap_or_default();
            let dlg = FileChooserDialog::with_buttons(
                Some(title.as_str()),
                parent.as_ref(),
                action,
                &[
                    ("取消", ResponseType::Cancel),
                    (if save { "保存" } else { "打开" }, ResponseType::Accept),
                ],
            );
            dlg.set_modal(true);
            dlg.set_default_response(ResponseType::Accept);
            if save {
                dlg.set_do_overwrite_confirmation(true);
            }
            if let Some(p) = opts.default_path.as_deref().filter(|s| !s.is_empty()) {
                let p = Path::new(p);
                if save {
                    if let Some(dir) = p.parent().filter(|d| d.is_dir()) {
                        dlg.set_current_folder(dir);
                    }
                    if let Some(name) = p.file_name() {
                        dlg.set_current_name(&name.to_string_lossy());
                    }
                } else if p.is_dir() {
                    dlg.set_current_folder(p);
                } else {
                    dlg.set_filename(p);
                }
            }
            for f in &opts.filters {
                let ff = FileFilter::new();
                ff.set_name(Some(&f.name));
                for e in &f.extensions {
                    if e == "*" {
                        ff.add_pattern("*");
                    } else {
                        ff.add_pattern(&format!("*.{}", e.to_lowercase()));
                        ff.add_pattern(&format!("*.{}", e.to_uppercase()));
                    }
                }
                dlg.add_filter(ff);
            }

            let tx = Mutex::new(Some(tx));
            dlg.connect_response(move |d, resp| {
                let path = if resp == ResponseType::Accept {
                    d.filename().map(|p| p.to_string_lossy().into_owned())
                } else {
                    None
                };
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(path);
                }
                // SAFETY: 对话框只在此回调里销毁一次，之后不再使用
                unsafe { d.destroy() };
            });
            dlg.show_all();
            dlg.present();
        })
        .map_err(|e| e.to_string())?;
    // 对话框被销毁但没发出响应（理论上不会）→ 视作取消
    Ok(rx.await.unwrap_or(None))
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
pub async fn file_dialog(
    window: tauri::WebviewWindow,
    opts: DialogOpts,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
    let mut b = window.dialog().file().set_parent(&window);
    if let Some(t) = opts.title {
        b = b.set_title(t);
    }
    if let Some(p) = opts.default_path.filter(|s| !s.is_empty()) {
        let p = std::path::PathBuf::from(p);
        if opts.mode == "save" {
            if let Some(dir) = p.parent().filter(|d| d.is_dir()) {
                b = b.set_directory(dir);
            }
            if let Some(name) = p.file_name() {
                b = b.set_file_name(name.to_string_lossy());
            }
        } else if p.is_dir() {
            b = b.set_directory(p);
        }
    }
    for f in &opts.filters {
        let exts: Vec<&str> = f.extensions.iter().map(|s| s.as_str()).collect();
        b = b.add_filter(&f.name, &exts);
    }
    let cb = move |p: Option<tauri_plugin_dialog::FilePath>| {
        let _ = tx.send(p.and_then(|p| p.into_path().ok()).map(|p| p.to_string_lossy().into_owned()));
    };
    if opts.mode == "save" {
        b.save_file(cb);
    } else {
        b.pick_file(cb);
    }
    Ok(rx.await.unwrap_or(None))
}
