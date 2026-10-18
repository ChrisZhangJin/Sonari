# Sonari — 开源音频编辑器（Rust 引擎 + Tauri 2 桌面应用）

纯桌面端音频分析与编辑工具。逆向分析 ocenaudio 后按其架构分层自研：
**引擎零 GUI 依赖**，渲染全部交给前端。

定位：开源、跨平台（先 Windows）、可编辑音频；并计划提供 **CLI 子命令**，
方便 AI agent 以 JSON 命令行方式调用引擎能力（解码 / 分析 / 导出）。

## 架构

```
┌────────────────────────────────────────────────┐
│  前端 UI（Vue 3 + Canvas）                       │
│  波形、频谱、星座图、菜单、对话框                  │
└──────────────────┬─────────────────────────────┘
                   │ Tauri invoke('engine', {cmd, p})
┌──────────────────┴─────────────────────────────┐
│  Tauri 2 桌面壳（sonari.exe）                    │
│  原生窗口 + WebView2，一个 exe 无浏览器依赖        │
└──────────────────┬─────────────────────────────┘
                   │ 同进程调用
┌──────────────────┴─────────────────────────────┐
│  引擎（engine/ crate: sonari-engine）            │
│  纯 Rust，零 GUI 依赖，输出纯数据                 │
│  decode | doc | dsp | analysis | encode        │
│  transform | commands | audio（cpal 播放/录音）   │
└────────────────────────────────────────────────┘
```

| ocenaudio 组件 | Sonari 模块 | 说明 |
|---|---|---|
| `libiaudio.dll`（80 种 FormatFilter） | `engine/src/decode.rs` | symphonia 0.6，WAV/MP3/FLAC/OGG/Opus/AAC…，统一输出交错 f32 |
| `libocen.dll` Document/Undo（902 API） | `engine/src/doc.rs` | Span/Marker/命令式 undo（Cut/Paste/Gain） |
| `libdspb.dll` FFT/窗/响度 | `engine/src/dsp.rs` | realfft + Hann/Hamming/Blackman(Harris) + PSD→dB + Bark/Mel |
| `libocen.spectral.*` + Shazam 式 PoC | `engine/src/analysis.rs` | 频谱图 + **星座图**（landmark 配对哈希） |
| `libqtocenmixer2` | `engine/src/audio.rs` | cpal 播放/录音，设备枚举 |
| FormatFilter 写出侧 | `engine/src/encode.rs` | WAV 内建 + ffmpeg 兜底 |
| SoundTouch 重采样器 | `engine/src/transform.rs` | 加窗 sinc 插值重采样 + 声道转换 |
| `libqtocen*.dll`（Qt GUI） | `ui/`（Vue 3 + Canvas） | 全部 UI 渲染 |
| `ocenaudio.exe`（宿主进程） | `src-tauri/` | Tauri 2 桌面壳 |

## 构建

```bash
cd ui && npm install && npm run build         # 1. 构建前端（编译期嵌入二进制）
cargo build --release -p sonari \
            --features custom-protocol        # 2. 桌面应用
# 产物：target/release/sonari.exe（WebView2 运行时 Win10/11 系统自带）
```

> ⚠️ **坑**：release 打包**必须**开 `--features custom-protocol`。
> 不开的话 exe 以 dev 模式运行——启动后去连 `tauri.conf.json` 里
> `devUrl`（localhost:5174）的 vite dev server，窗口永远空白。
> 等价的规范做法是用 Tauri CLI：`npm run tauri build`（自动带上该 feature）。

### 开发模式

```bash
cd ui && npm install && npm run dev           # 前端热更新 http://localhost:5174
cargo tauri dev                               # 桌面壳 + 引擎
```

## 引擎命令

| cmd | 参数 | 返回 |
|---|---|---|
| `doc_open` | path | docId + info + 4096 桶混合波形峰值 + BS.1770 响度（LUFS/LRA/TP） |
| `waveform` | docId, buckets | **逐声道**峰值（data 按声道顺次平铺 [min,max]） |
| `spectrogram` | fft/hop/maxFrames/**channel?** | 帧数组（dB） |
| `constellation` | fft/hop/peaksPerFrame/fanOut/preEmphasis | stars[] + hashes[] |
| `doc_info` | docId | **信号属性**（容器/编解码器/分辨率/通道/采样率/时长/样本数/文件大小/平均比特率/逐声道峰值/响度） |
| `doc_channels` | docId, channels | **转换声道**（1=单声道 / 2=立体声），返回刷新后的完整文档 |
| `doc_resample` | docId, sampleRate | **转换为采样率**（4 k–384 kHz） |
| `doc_save` | docId, path?, format?, bitDepth?, bitrateKbps?, begin?, end? | **保存 / 导出 / 另存为** |
| `doc_rename` | docId, name | **重命名** |
| `encoder_caps` | — | 探测 ffmpeg：可写格式清单 / 位深 / 码率档位 |
| `pcm` | begin/end | f32le base64 |

### 编码写出

| 路径 | 格式 | 说明 |
|---|---|---|
| **内建 WAV** | WAV | RIFF/WAVE，PCM 16/24/IEEE float 32，零外部依赖 |
| **FFMPEG 兜底** | MP3/FLAC/OGG/Opus/M4A/AIFF/WMA/CAF | ffmpeg 缺席时禁用 |

## 功能特性

- **多文档**：左侧文件列表，同时打开多个音频文件
- **逐声道波形+频谱**：多声道文件每声道一行
- **星座图**：Shazam 式音频指纹（landmark 配对哈希）
- **右键菜单**：ocenaudio 风格的上下文菜单（转换声道/采样率/查看信息/播放/保存/重命名/全选）
- **录音**：cpal 实时录音，实时波形显示
- **保存确认**：文件修改后关闭时弹出保存提示
- **快捷键**：Ctrl+O 打开 / Ctrl+S 保存 / Space 播放 / Ctrl+1~4 切换视图

## 已验证

- `cargo test -p sonari-engine`：**30/30**（21 单元 + 9 集成）
- 单元测试覆盖 FFT、窗函数、Bark/Mel、undo 往返、重采样保频、WAV 编码往返
- 集成测试覆盖转换声道/采样率、另存为改绑、导出选区、ffmpeg MP3/FLAC 往返、磁盘重命名

## 路线图

1. 编辑闭环：选区 Gain/Cut 的 UI（引擎已就绪）
2. **CLI 能力**：`sonari analyze <file>` / `sonari convert <in> <out> --rate 16000 --channels 1`
   等子命令，输出 JSON，面向 AI agent 与脚本化流水线
3. 星座图扩展：哈希库 + 录音匹配（Shazam 式检索）
4. NSIS/MSI 安装包 + 跨平台构建（macOS / Linux）
