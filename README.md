# Sonari — 开源音频编辑器（ocenaudio 架构启示，Rust + Vue 自研实现）

独立音频分析与编辑工具。逆向分析 ocenaudio（`D:\Program Files\ocenaudio`，报告见
`output/ocenaudio-analysis.md`）后按其架构分层自研：**引擎零 GUI 依赖**，
渲染全部交给前端 —— 继承 libocen 的 `OCENCANVAS_*` 抽象决策。

定位：开源、跨平台（先 Windows）、可编辑音频；并计划提供 **CLI 子命令**，
方便 AI agent 以 JSON 命令行方式调用引擎能力（解码 / 分析 / 导出）。

## 架构

| ocenaudio | 本项目 | 说明 |
|---|---|---|
| `libiaudio.dll`（80 种 FormatFilter） | `engine/src/decode.rs` | symphonia 0.6，WAV/MP3/FLAC/OGG/Opus/AAC…，统一输出交错 f32 |
| `libocen.dll` Document/Undo（902 API） | `engine/src/doc.rs` | Span/Marker/命令式 undo（Cut/Paste/Gain） |
| `libdspb.dll` FFT/窗/响度 | `engine/src/dsp.rs` | realfft + Hann/Hamming/Blackman(Harris) + PSD→dB + Bark/Mel |
| `libocen.spectral.*` + Shazam 式 PoC | `engine/src/analysis.rs` | 频谱图 + **星座图**（landmark 配对哈希） |
| `libqtocen*.dll`（Qt GUI） | `ui/`（Vue3 + Canvas） | 菜单栏/工具栏/文件列表/**逐声道波形+频谱行**/时间标尺/星座图/Web Audio 播放 |
| ocenaudio.exe（宿主进程） | `server/`（WS JSON 命令总线）+ `src-tauri/`（Tauri 2 桌面壳） | |

星座图参数默认值（fft=1024 hop=64 peaks=5 fan=10）来自 `ocen_const.log` PoC 实测。

## 运行

### ① 桌面 exe（Tauri 2，推荐）—— `target/release/sonari.exe`

原生窗口 + WebView2 渲染 UI + 引擎，**一个 exe，无浏览器依赖**：

```bash
cd ui && npm run build                          # 1. 构建前端（编译期嵌入二进制）
cargo build --release -p sonari \
            --features custom-protocol          # 2. 桌面应用
# 产物：target/release/sonari.exe（WebView2 运行时 Win10/11 系统自带）
```

> ⚠️ **坑**：release 打包**必须**开 `--features custom-protocol`。
> 不开的话 exe 以 dev 模式运行——启动后去连 `tauri.conf.json` 里
> `devUrl`（localhost:5174）的 vite dev server，而它没在跑，窗口就会
> 永远空白（about:blank），且前端资源完全不会嵌入 exe。
> 等价的规范做法是用 Tauri CLI：`npm run tauri build`（自动带上该 feature）。

命令层与 WS 服务端共用 `sonari_engine::commands`（`src-tauri/src/lib.rs`
的 `engine_cmd!` 宏，每个命令约 20 行薄壳），前端 `api.ts` 按
`__TAURI_INTERNALS__` 自动切换 invoke() / WebSocket，UI 代码零分叉。

### ② 浏览器单文件 exe —— `target/release/sonari-server.exe`（**已停止功能维护，仅作调试参考**）

> ⚠️ 项目主产物是上面的桌面 exe（`sonari.exe`）。server 仅保留用于**调试参考**：
> 需要无头排查引擎命令（`/波形/频谱/星座/设备/播放` 的返回）时用它跑 WS 冒烟，
> 不再随新功能同步更新，也不作为交付形态。

```bash
cargo build --release -p sonari-server   # 双击自动开浏览器 http://127.0.0.1:9712
node smoke.mjs                           # 端到端冒烟（12 项断言）
```

### ③ 开发模式

```bash
cargo run -p sonari-server -- --listen 127.0.0.1:9712   # 引擎服务
cd ui && npm install && npm run dev                    # 前端热更新 http://localhost:5174
node smoke.mjs                                         # 端到端冒烟（12 项断言）
```

## WS 命令总线

| cmd | 参数 | 返回 |
|---|---|---|
| `doc_open` | path | docId + info + 4096 桶混合波形峰值 + BS.1770 响度（LUFS/LRA/TP） |
| `waveform` | docId, buckets | **逐声道**峰值（data 按声道顺次平铺 [min,max]，UI 每声道一行） |
| `spectrogram` | fft/hop/maxFrames/**channel?** | 帧数组（dB）；channel=N 取单声道，缺省为正确混音（帧平均） |
| `constellation` | fft/hop/peaksPerFrame/fanOut/preEmphasis | stars[] + hashes[]（f1,f2,Δt） |
| `doc_info` | docId | **信号属性**（右键「查看信息」）：容器/编解码器/分辨率/通道/采样率/时长/样本数/文件大小/平均比特率/逐声道峰值/响度 |
| `doc_channels` | docId, channels | **转换声道**（1=单声道下混 / 2=立体声上混），返回刷新后的完整文档 |
| `doc_resample` | docId, sampleRate | **转换为采样率**（4 k–384 kHz），帧数按比例缩放、时长不变，返回刷新后的完整文档 |
| `doc_save` | docId, path?, format?, bitDepth?, bitrateKbps?, begin?, end? | **保存 / 导出… / 另存为…**：不带 path = 覆盖写回；带 path = 改绑文档；带 begin/end = 只写选区 |
| `doc_rename` | docId, name | **重命名…**：改磁盘文件名（内存文档只改显示名） |
| `encoder_caps` | — | 探测 ffmpeg：可写格式清单 / 位深 / 码率档位 |
| `pcm` | begin/end | f32le base64（前端 Web Audio 播放） |

### 命令路由

分类由 **`sonari_engine::{AUDIO_CMDS, WRITE_CMDS, is_audio_cmd, is_write_cmd}` 单点定义**：
`audio_*` 走 `AudioState`，写命令走写锁，其余走读锁并发。
新增命令只改 engine 一处，server 与 Tauri 两个宿主自动同步。

### 编码写出（对标 ocenaudio 的 FormatFilter 写出侧）

| 路径 | 格式 | 说明 |
|---|---|---|
| **内建 WAV**（`engine/src/encode.rs`） | WAV | RIFF/WAVE，PCM 16 / PCM 24 / IEEE float 32，零外部依赖 |
| **FFMPEG 兜底** | MP3 / FLAC / OGG / Opus / M4A(AAC) / AIFF / WMA / CAF | 与 ocenaudio 一致：有损编码交给外挂 FFMPEG。探测顺序：`SONARI_FFMPEG` 环境变量 → `PATH` 里的 ffmpeg |

ffmpeg 缺席时，非 WAV 格式在「另存为」下拉里显示为「需要 ffmpeg」并禁用，**不会静默失败**。
转码走 32 位浮点临时 WAV 中转（保留全部动态范围），完成后即删；Windows 下以
`CREATE_NO_WINDOW` 调用，不弹控制台。

### 信号变换（`engine/src/transform.rs`）

「转换为」菜单的后端：**加窗 sinc 插值**（Hann 窗，核半宽随截止频率自适应，
降采样时按 1/ratio 放大到最多 48 taps 做抗混叠），下混为帧内平均、上混为声道复制。

命令并发处理（慢 FFT 不阻塞），样本走 `spawn_blocking`。

## 右键菜单（ocenaudio 式）

WebView2 自带的浏览器菜单（返回 / 刷新 / 另存为 / 打印）被 `@contextmenu` 全量拦截
（输入框内放行，保留复制粘贴），换成自定义菜单：

```
文件名 / 双声道 · 44100 Hz · 2.000s
──────────────────────────
⇄  转换为立体声            ← 单声道时显示此条；立体声时显示「转换为单声道」
◴  转换为            ▸   8 kHz / 16 kHz / 22.05 kHz / 32 kHz / 48 kHz
                          ✓ 标记当前采样率 · 「其他…」可填 4 k–384 kHz
──────────────────────────
ⓘ  查看信息…               ← 信号属性对话框
▶  播放                    Space
──────────────────────────
💾 保存                    Ctrl+S        （覆盖写回原文件；录音文档退化为另存为）
⇱  导出…                                 （可勾选「仅导出选区」）
⇱  另存为…                 Ctrl+Shift+S  （格式转换 + 改绑文档）
✎  重命名…                               （真实改磁盘文件名）
──────────────────────────
▭  全选                    Ctrl+A
   取消选择
```

「转换为」是**原地改文档**（不改文件名、不改路径），时长不变；转换后波形/频谱/星座缓存
即时失效重算，文档信息区同步刷新。「另存为」会把文档改绑到新文件（后续「保存」写新文件），
并回读实际码流元数据，所以「查看信息」里的格式会从 `PCM 16 位有符号` 变成 `MPEG-1/2 Layer III（MP3）`。

## 已验证

- `cargo test -p sonari-engine`：**30/30**（21 单元 + 9 集成）
  - 单元：FFT 单频定位、星座星点聚集、undo 往返、Bark/Mel、重采样保频（过零率法，
    44.1k↔16k↔8k 主频误差 < 12 Hz）、WAV 16/24/32f 往返（32f 逐位相等）
  - 集成（`engine/tests/menu.rs`，走真实命令总线）：转换声道保帧数、转换采样率帧数缩放、
    另存为改绑文档 + `doc_info` 跟着变、导出选区帧数精确、**ffmpeg MP3/FLAC 真实往返
    （自家解码器读回，时长 ±0.15 s、FLAC 峰值保持 0.5）**、磁盘重命名、内存文档拒绝「保存」
- 冒烟 12/12：解码正确性、峰值、LUFS=-12.4、频谱 1.2kHz 能量集中（bin28=0dB vs 均值 -78.8dB）、
  星座三频段星点分布（440Hz:1492 / 1.2k:1806 / 3k:1504）、PCM 导出
- `doc_info`（stereo_lr.wav）：容器 WAV · 编解码器「PCM 16 位有符号（小端）」· 分辨率 16 位 ·
  双声道 · 44100 Hz · 2.000s · 88200 帧 · 352844 B · 1411 kbps（= sr×ch×bits，自洽）

## 路线图

1. cpal 播放下沉到 Rust（现用 Web Audio，浏览器端零延迟足够一期）
2. 编辑闭环：选区 Gain/Cut 的 UI（引擎已就绪）
3. **CLI 能力**：`sonari analyze <file>` / `sonari convert <in> <out> --rate 16000 --channels 1`
   等子命令，输出 JSON，面向 AI agent 与脚本化流水线（复用 `sonari_engine::commands`，零新增逻辑）
4. 星座图扩展：哈希库 + 录音匹配（Shazam 式检索，ocen_const.log PoC 的完整形态）
5. NSIS/MSI 安装包 + 跨平台构建（macOS / Linux）
