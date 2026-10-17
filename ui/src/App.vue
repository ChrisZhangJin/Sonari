<script setup lang="ts">
import { ref, computed, watch, nextTick, onMounted, onUnmounted } from 'vue';
import {
  Engine,
  IS_TAURI,
  errText,
  type DocOpened,
  type ConstellationResult,
  type DocDetail,
  type EncoderCaps,
} from './api';

// dev 模式（vite 5174）直连引擎端口；打包单 exe 后同源同端口
const wsUrl =
  location.port === '5174' || !location.protocol.startsWith('http')
    ? 'ws://127.0.0.1:9712'
    : `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}`;
const eng = new Engine(wsUrl);

const status = ref(IS_TAURI ? '桌面模式 · 引擎内嵌' : '未连接');
const busy = ref(false);

// ---- 多文档（左侧"已打开的文件"） ----
const docs = ref<DocOpened[]>([]);
const activeId = ref('');
const activeDoc = computed(() => docs.value.find((d) => d.docId === activeId.value) ?? null);
const chList = computed(() => {
  const n = activeDoc.value?.info.channels ?? 1;
  return Array.from({ length: n }, (_, i) => i + 1); // 1-based 便于 v-for
});
function chText(n: number): string {
  return n === 1 ? '单声道' : n === 2 ? '双声道' : `${n} 声道`;
}

// ---- 视图 ----
type ViewMode = 'wave' | 'spec' | 'both' | 'const';
const view = ref<ViewMode>('both');

// ---- 菜单 ----
const openMenu = ref<'' | 'file' | 'edit' | 'view'>('');
function toggleMenu(m: 'file' | 'edit' | 'view') {
  openMenu.value = openMenu.value === m ? '' : m;
}
function setView(v: ViewMode) {
  view.value = v;
  openMenu.value = '';
}

// ---- 星座图参数 ----
const fftLen = ref(1024);
const hop = ref(64);
const peaksPerFrame = ref(5);
const fanOut = ref(10);
const dynRange = ref(90);
const preEmphasis = ref(false);
const specHop = ref(256);

// ---- 数据缓存 ----
const waveCache = new Map<string, number[][]>(); // docId -> number[][]（每声道平铺 [min,max]）
const specCache = new Map<string, number[][]>(); // docId:ch -> frames[][]

// ---- 画布引用 ----
const waveCanvases: (HTMLCanvasElement | null)[] = [];
const specCanvases: (HTMLCanvasElement | null)[] = [];
const overviewCanvas = ref<HTMLCanvasElement | null>(null);
const constCanvas = ref<HTMLCanvasElement | null>(null);
const rulerCanvas = ref<HTMLCanvasElement | null>(null);
const rowsEl = ref<HTMLElement | null>(null);
const viewareaEl = ref<HTMLElement | null>(null);
function setCanvas(arr: (HTMLCanvasElement | null)[], i: number, el: unknown) {
  arr[i] = el instanceof HTMLCanvasElement ? el : null;
}

const infoText = computed(() => {
  const d = activeDoc.value;
  if (!d) return '';
  const i = d.info;
  const s = d.stats;
  const bits = i.bitsPerSample ? `${i.bitsPerSample}bit` : '32f';
  return `${i.sampleRate} Hz · ${bits} · ${i.channels}ch · ${fmtSamples(i.frames)} 样本 · ${fmtTime(i.durationSec)}` +
    (s ? ` · ${s.integratedLufs.toFixed(1)} LUFS · TP ${s.truePeakDbtp.toFixed(1)} dBTP` : '');
});

function fmtTime(sec: number): string {
  if (sec >= 60) {
    const m = Math.floor(sec / 60);
    return `${m}:${(sec % 60).toFixed(1).padStart(4, '0')}`;
  }
  return `${sec.toFixed(2)}s`;
}
function fmtSamples(n: number): string {
  return n >= 1e6 ? `${(n / 1e6).toFixed(2)}M` : n >= 1e3 ? `${(n / 1e3).toFixed(1)}k` : String(n);
}
function fmtMeta(d: DocOpened): string {
  const khz = (d.info.sampleRate / 1000).toFixed(1);
  const bits = d.info.bitsPerSample ? `${d.info.bitsPerSample}bit` : '32f';
  return `${bits} · ${khz} kHz · ${d.info.channels}ch · ${fmtTime(d.info.durationSec)}`;
}

// ---- 最近打开 ----
const RECENTS_KEY = 'sonari.recents';
const recents = ref<string[]>(loadRecents());
function loadRecents(): string[] {
  try {
    const v = JSON.parse(localStorage.getItem(RECENTS_KEY) ?? '[]');
    return Array.isArray(v) ? v.filter((x) => typeof x === 'string') : [];
  } catch {
    return [];
  }
}
function pushRecent(path: string) {
  if (!path) return;
  recents.value = [path, ...recents.value.filter((p) => p !== path)].slice(0, 10);
  localStorage.setItem(RECENTS_KEY, JSON.stringify(recents.value));
}
function clearRecents() {
  recents.value = [];
  localStorage.removeItem(RECENTS_KEY);
}
async function openRecent(p: string) {
  openMenu.value = '';
  await openPath(p);
}
function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

// ---- 文件打开：桌面走系统文件管理器，浏览器退回输入 ----
const AUDIO_EXTS = ['wav', 'mp3', 'flac', 'ogg', 'opus', 'm4a', 'aac', 'aif', 'aiff', 'wma'];
async function openFileDialog(): Promise<string | null> {
  if (!IS_TAURI) {
    // 浏览器模式：引擎按服务端本地路径解码
    return window.prompt('输入音频文件完整路径（服务端本地）：') ?? null;
  }
  const { open } = await import('@tauri-apps/plugin-dialog');
  const sel = await open({
    multiple: false,
    title: '打开音频文件',
    filters: [
      { name: '音频文件', extensions: AUDIO_EXTS },
      { name: '所有文件', extensions: ['*'] },
    ],
  });
  return typeof sel === 'string' ? sel : null;
}

async function triggerOpen() {
  openMenu.value = '';
  const path = await openFileDialog();
  if (path) await openPath(path);
}

async function openPath(path: string) {
  busy.value = true;
  status.value = '解码中…';
  try {
    const doc = await eng.docOpen(path);
    await addDoc(doc);
    status.value = `已打开 ${doc.name}`;
    pushRecent(path);
  } catch (e: any) {
    status.value = `打开失败: ${errText(e)}`;
  } finally {
    busy.value = false;
  }
}

async function addDoc(doc: DocOpened) {
  const existed = docs.value.find((d) => d.name === doc.name && d.info.frames === doc.info.frames);
  if (existed) {
    activeId.value = existed.docId;
  } else {
    docs.value.push(doc);
    activeId.value = doc.docId;
  }
  if (!waveCache.has(doc.docId)) {
    const w = await eng.waveform(doc.docId, 2000);
    const perCh: number[][] = [];
    for (let c = 0; c < w.channels; c++) {
      perCh.push(w.data.slice(c * w.buckets * 2, (c + 1) * w.buckets * 2));
    }
    waveCache.set(doc.docId, perCh);
  }
  await nextTick();
  redrawAll();
}

async function closeDoc(docId: string) {
  openMenu.value = '';
  try {
    await eng.docClose(docId);
  } catch {
    /* 引擎侧已不存在也无妨 */
  }
  docs.value = docs.value.filter((d) => d.docId !== docId);
  waveCache.delete(docId);
  for (const k of [...specCache.keys()]) if (k.startsWith(docId + ':')) specCache.delete(k);
  if (activeId.value === docId) activeId.value = docs.value.at(-1)?.docId ?? '';
}

async function closeAll() {
  for (const d of [...docs.value]) await closeDoc(d.docId);
}

function selectDoc(id: string) {
  activeId.value = id;
}

async function quit() {
  if (IS_TAURI) {
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    getCurrentWindow().close();
  } else {
    window.close();
  }
}

// ---- 首选项（音频设备） ----
const showPrefs = ref(false);
type DevInfo = { name: string; meta: string; isDefault: boolean };
const devices = ref<{ host: string; outputs: DevInfo[]; inputs: DevInfo[] }>({
  host: '',
  outputs: [],
  inputs: [],
});
const AUDIO_PREFS_KEY = 'sonari.audio';
const audioPrefs = ref<{ output: string; input: string }>(loadAudioPrefs());
function loadAudioPrefs(): { output: string; input: string } {
  try {
    const v = JSON.parse(localStorage.getItem(AUDIO_PREFS_KEY) ?? '{}');
    return {
      output: typeof v.output === 'string' ? v.output : '',
      input: typeof v.input === 'string' ? v.input : '',
    };
  } catch {
    return { output: '', input: '' };
  }
}
watch(audioPrefs, (v) => localStorage.setItem(AUDIO_PREFS_KEY, JSON.stringify(v)), { deep: true });

async function openPrefs() {
  openMenu.value = '';
  showPrefs.value = true;
  await loadDevices();
}
async function loadDevices() {
  try {
    devices.value = await eng.audioDevices();
    // 从未选择过时默认选中系统默认设备
    if (!audioPrefs.value.output) {
      audioPrefs.value.output = devices.value.outputs.find((d) => d.isDefault)?.name ?? '';
    }
    if (!audioPrefs.value.input) {
      audioPrefs.value.input = devices.value.inputs.find((d) => d.isDefault)?.name ?? '';
    }
  } catch (e: any) {
    status.value = `枚举设备失败: ${errText(e)}`;
  }
}

// ================= 绘制 =================

function inferno(t: number): string {
  const stops: [number, [number, number, number]][] = [
    [0.0, [0, 0, 4]],
    [0.13, [27, 12, 65]],
    [0.25, [74, 12, 107]],
    [0.38, [120, 28, 109]],
    [0.5, [165, 44, 96]],
    [0.63, [207, 68, 70]],
    [0.75, [237, 105, 37]],
    [0.88, [251, 155, 6]],
    [1.0, [252, 255, 164]],
  ];
  const x = Math.max(0, Math.min(1, t));
  for (let i = 1; i < stops.length; i++) {
    if (x <= stops[i][0]) {
      const [t0, c0] = stops[i - 1];
      const [t1, c1] = stops[i];
      const k = (x - t0) / (t1 - t0);
      const r = Math.round(c0[0] + (c1[0] - c0[0]) * k);
      const g = Math.round(c0[1] + (c1[1] - c0[1]) * k);
      const b = Math.round(c0[2] + (c1[2] - c0[2]) * k);
      return `rgb(${r},${g},${b})`;
    }
  }
  return 'rgb(252,255,164)';
}

/** 画布物理尺寸对齐 CSS 尺寸（铺满容器，不留空） */
function fitCanvas(c: HTMLCanvasElement) {
  const w = Math.max(64, c.clientWidth);
  const h = Math.max(24, c.clientHeight);
  if (c.width !== w || c.height !== h) {
    c.width = w;
    c.height = h;
  }
}

function drawOverview() {
  const c = overviewCanvas.value;
  const d = activeDoc.value;
  if (!c || !d) return;
  fitCanvas(c);
  const W = c.width;
  const H = c.height;
  const g = c.getContext('2d')!;
  g.fillStyle = '#0d1117';
  g.fillRect(0, 0, W, H);
  const n = d.peaks.length / 2;
  if (!n) return;
  const mid = H / 2;
  const colW = W / n;
  g.fillStyle = '#17705a';
  for (let i = 0; i < n; i++) {
    const mn = d.peaks[i * 2];
    const mx = d.peaks[i * 2 + 1];
    const y0 = mid - mx * (mid - 3);
    const y1 = mid - mn * (mid - 3);
    g.fillRect(i * colW, y0, Math.max(colW, 0.5), Math.max(y1 - y0, 1));
  }
}

function drawWaveRow(canvas: HTMLCanvasElement | null, flat: number[]) {
  if (!canvas) return;
  fitCanvas(canvas);
  const g = canvas.getContext('2d')!;
  const W = canvas.width;
  const H = canvas.height;
  g.fillStyle = '#0d1117';
  g.fillRect(0, 0, W, H);
  const mid = H / 2;
  // dB 网格线（0.5=-6dB, 0.25=-12dB, 0.125=-18dB）
  g.strokeStyle = '#1d2630';
  g.fillStyle = '#5a6a7a';
  g.font = '10px Consolas, monospace';
  g.textAlign = 'right';
  for (const a of [0.5, 0.25, 0.125]) {
    const off = a * (mid - 4);
    for (const y of [mid - off, mid + off]) {
      g.beginPath();
      g.moveTo(0, y);
      g.lineTo(W, y);
      g.stroke();
    }
    const db = Math.round(20 * Math.log10(a));
    g.fillText(`${db}`, W - 4, mid - off - 2);
  }
  g.strokeStyle = '#2a3440';
  g.beginPath();
  g.moveTo(0, mid);
  g.lineTo(W, mid);
  g.stroke();
  const n = flat.length / 2;
  if (!n) return;
  const colW = W / n;
  g.fillStyle = '#1fd6a4';
  for (let i = 0; i < n; i++) {
    const mn = flat[i * 2];
    const mx = flat[i * 2 + 1];
    const y0 = mid - mx * (mid - 4);
    const y1 = mid - mn * (mid - 4);
    g.fillRect(i * colW, y0, Math.max(colW, 0.6), Math.max(y1 - y0, 1));
  }
}

async function fetchSpec(docId: string, ch: number): Promise<number[][]> {
  const key = `${docId}:${ch}`;
  const hit = specCache.get(key);
  if (hit) return hit;
  const r = await eng.spectrogram(docId, { fft: 1024, hop: specHop.value, maxFrames: 1200, channel: ch });
  specCache.set(key, r.frames);
  return r.frames;
}

function drawSpecRow(canvas: HTMLCanvasElement | null, frames: number[][], sampleRate: number) {
  if (!canvas) return;
  fitCanvas(canvas);
  const g = canvas.getContext('2d')!;
  const W = canvas.width;
  const H = canvas.height;
  g.fillStyle = '#000';
  g.fillRect(0, 0, W, H);
  const nf = frames.length;
  if (!nf) return;
  const nb = frames[0].length;
  const colW = W / nf;
  const rowH = H / nb;
  for (let f = 0; f < nf; f++) {
    const db = frames[f];
    for (let b = 0; b < nb; b++) {
      const t = 1 + db[b] / dynRange.value;
      if (t <= 0.02) continue;
      g.fillStyle = inferno(Math.pow(t, 0.7));
      g.fillRect(f * colW, H - (b + 1) * rowH, Math.max(colW, 1), Math.max(rowH, 1));
    }
  }
  // 频率刻度（右缘）
  g.font = '10px Consolas, monospace';
  g.textAlign = 'right';
  const nyquist = sampleRate / 2;
  const step = nyquist > 12000 ? 4000 : nyquist > 6000 ? 2000 : 1000;
  for (let hz = step; hz < nyquist; hz += step) {
    const y = H - (hz / nyquist) * H;
    g.fillStyle = 'rgba(255,255,255,0.10)';
    g.fillRect(0, y, W, 1);
    g.fillStyle = 'rgba(220,230,240,0.85)';
    g.fillText(`${hz / 1000}k`, W - 4, y - 2);
  }
}

function drawRuler() {
  const c = rulerCanvas.value;
  const d = activeDoc.value;
  if (!c) return;
  fitCanvas(c);
  const W = c.width;
  const H = c.height;
  const g = c.getContext('2d')!;
  g.fillStyle = '#10151a';
  g.fillRect(0, 0, W, H);
  g.fillStyle = '#8fa1b3';
  g.fillRect(0, 0, W, 1);
  if (!d) return;
  const dur = d.info.durationSec;
  if (dur <= 0) return;
  const steps = [0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1200, 1800, 3600];
  const step = steps.find((s) => dur / s <= 16) ?? 3600;
  for (let t = 0; t <= dur + 1e-9; t += step) {
    const x = Math.round((t / dur) * W);
    g.fillStyle = 'rgba(255,255,255,0.06)';
    g.fillRect(x, 0, 1, H);
    g.fillStyle = '#8fa1b3';
    g.fillRect(x, 0, 1, 6);
    g.font = '10px Consolas, monospace';
    g.textAlign = 'left';
    const label = step < 1 ? t.toFixed(1) : String(Math.round(t));
    g.fillText(label, x + 3, 17);
  }
}

async function drawConst() {
  const c = constCanvas.value;
  const d = activeDoc.value;
  if (!c || !d) return;
  fitCanvas(c);
  busy.value = true;
  try {
    const r: ConstellationResult = await eng.constellation(d.docId, {
      fft: fftLen.value,
      hop: hop.value,
      peaksPerFrame: peaksPerFrame.value,
      fanOut: fanOut.value,
      preEmphasis: preEmphasis.value,
    });
    const g = c.getContext('2d')!;
    const W = c.width;
    const H = c.height;
    g.fillStyle = '#0b0e12';
    g.fillRect(0, 0, W, H);
    const nf = Math.max(r.frames_analyzed, 1);
    const nb = r.fft_len / 2 + 1;
    g.strokeStyle = 'rgba(255,170,60,0.22)';
    g.lineWidth = 0.7;
    g.beginPath();
    for (const h of r.hashes) {
      const x1 = (h.t1 / nf) * W;
      const y1 = H - (h.f1 / nb) * H;
      const x2 = ((h.t1 + h.dt) / nf) * W;
      const y2 = H - (h.f2 / nb) * H;
      g.moveTo(x1, y1);
      g.lineTo(x2, y2);
    }
    g.stroke();
    for (const s of r.stars) {
      const t = 1 + s.db / dynRange.value;
      const alpha = Math.max(0.25, Math.min(1, t));
      const x = (s.frame / nf) * W;
      const y = H - (s.bin / nb) * H;
      g.fillStyle = `rgba(120,220,255,${alpha})`;
      g.beginPath();
      g.arc(x, y, 1.6, 0, Math.PI * 2);
      g.fill();
    }
    status.value = `星座图 ${r.stars.length} 星 · ${r.hashes.length} 哈希 · ${r.frames_analyzed} 帧`;
  } catch (e: any) {
    status.value = `星座失败: ${errText(e)}`;
  } finally {
    busy.value = false;
  }
}

async function redrawAll() {
  await nextTick();
  drawOverview();
  drawRuler();
  const d = activeDoc.value;
  if (!d) return;
  const perCh = waveCache.get(d.docId);
  if (perCh) {
    for (let c = 0; c < d.info.channels; c++) {
      drawWaveRow(waveCanvases[c], perCh[c] ?? []);
    }
  }
  if (view.value === 'spec' || view.value === 'both') {
    for (let c = 0; c < d.info.channels; c++) {
      try {
        const frames = await fetchSpec(d.docId, c);
        drawSpecRow(specCanvases[c], frames, d.info.sampleRate);
      } catch (e: any) {
        status.value = `频谱失败(ch${c}): ${errText(e)}`;
      }
    }
  }
  if (view.value === 'const') await drawConst();
}

watch([view, activeId], () => {
  redrawAll();
});

// 自适应窗口/布局变化 → 重画
let redrawTimer: number | null = null;
let resizeRo: ResizeObserver | null = null;
onMounted(() => {
  const va = viewareaEl.value;
  if (va && 'ResizeObserver' in window) {
    resizeRo = new ResizeObserver(() => {
      if (redrawTimer != null) clearTimeout(redrawTimer);
      redrawTimer = window.setTimeout(() => redrawAll(), 80);
    });
    resizeRo.observe(va);
  }
});
onUnmounted(() => {
  resizeRo?.disconnect();
  if (playTimer != null) clearInterval(playTimer);
  if (recTimer != null) clearInterval(recTimer);
});

// ============ 播放 / 录音（引擎 cpal，支持设备选择） ============
const playing = ref(false);
const playheadSec = ref(0);
const recording = ref(false);
const recSec = ref(0);
let playTimer: number | null = null;
let recTimer: number | null = null;

async function startPlayback(a: number, b: number) {
  const d = activeDoc.value;
  if (!d) return;
  const begin = Math.max(0, Math.floor(a * d.info.sampleRate));
  const end = Math.floor(b * d.info.sampleRate);
  try {
    await eng.playbackStop();
    try {
      await eng.playbackStart(d.docId, begin, end, audioPrefs.value.output);
    } catch (e: any) {
      // 首选项里的设备已失效（拔掉/改名）→ 回退系统默认设备重试一次
      if (audioPrefs.value.output && String(errText(e)).includes('未找到设备')) {
        audioPrefs.value.output = '';
        await eng.playbackStart(d.docId, begin, end, '');
        status.value = '已回退系统默认输出设备播放';
      } else {
        throw e;
      }
    }
    playing.value = true;
    playheadSec.value = a;
    if (playTimer == null) playTimer = window.setInterval(pollPlayback, 100);
  } catch (e: any) {
    status.value = `播放失败: ${errText(e)}`;
  }
}

async function pollPlayback() {
  try {
    const st = await eng.playbackStatus();
    playheadSec.value = st.positionSec;
    if (!st.playing) {
      playing.value = false;
      if (playTimer != null) {
        clearInterval(playTimer);
        playTimer = null;
      }
      // 播完释放流：否则设备句柄一直占着，下次播放可能失败
      try {
        await eng.playbackStop();
      } catch {
        /* 忽略 */
      }
    }
  } catch {
    /* 状态轮询失败静默 */
  }
}

async function stopPlayback() {
  try {
    await eng.playbackStop();
  } catch {
    /* 忽略 */
  }
  playing.value = false;
  if (playTimer != null) {
    clearInterval(playTimer);
    playTimer = null;
  }
}

function playSelection() {
  const d = activeDoc.value;
  if (!d) return;
  if (sel.value && sel.value.b - sel.value.a > 0.001) {
    startPlayback(sel.value.a, sel.value.b);
  } else {
    startPlayback(cursorSec.value, d.info.durationSec);
  }
}

async function toggleRecord() {
  if (recording.value) {
    recording.value = false;
    if (recTimer != null) {
      clearInterval(recTimer);
      recTimer = null;
    }
    try {
      const r = await eng.recordStop();
      const now = new Date();
      const p2 = (n: number) => String(n).padStart(2, '0');
      const name = `录音 ${now.getFullYear()}${p2(now.getMonth() + 1)}${p2(now.getDate())} ${p2(now.getHours())}${p2(now.getMinutes())}${p2(now.getSeconds())}`;
      busy.value = true;
      const doc = await eng.docOpenData({
        name,
        sampleRate: r.sampleRate,
        channels: r.channels,
        f32leBase64: r.f32leBase64,
      });
      await addDoc(doc);
      status.value = `录音完成：${r.durationSec.toFixed(1)}s · ${r.sampleRate} Hz · ${r.channels}ch`;
    } catch (e: any) {
      status.value = `录音保存失败: ${errText(e)}`;
    } finally {
      busy.value = false;
    }
  } else {
    try {
      await eng.recordStart(audioPrefs.value.input);
      recording.value = true;
      recSec.value = 0;
      recTimer = window.setInterval(async () => {
        try {
          const s = await eng.recordStatus();
          recSec.value = s.seconds;
        } catch {
          /* 忽略 */
        }
      }, 100);
      status.value = '录音中…（再次点击停止并生成文档）';
    } catch (e: any) {
      status.value = `录音启动失败: ${errText(e)}`;
    }
  }
}

async function stopAll() {
  if (recording.value) await toggleRecord();
  else await stopPlayback();
}

// ================= 光标 / 拖选 =================
const sel = ref<{ a: number; b: number } | null>(null); // 秒
const cursorSec = ref(0);
const hoverSec = ref<number | null>(null);
let dragFrom: number | null = null;

/** 客户端 X → 秒（相对行画布区域：左 60px / 右 8px） */
function xToTime(clientX: number): number {
  const el = rowsEl.value;
  const d = activeDoc.value;
  if (!el || !d) return 0;
  const rect = el.getBoundingClientRect();
  const cw = Math.max(1, rect.width - 60 - 8);
  const t = ((clientX - rect.left - 60) / cw) * d.info.durationSec;
  return Math.max(0, Math.min(d.info.durationSec, t));
}

function ovDown(e: MouseEvent) {
  // 只响应左键：右键留给上下文菜单，别顺手起一个选区
  if (e.button !== 0) return;
  dragFrom = xToTime(e.clientX);
  sel.value = null;
}
function ovMove(e: MouseEvent) {
  const t = xToTime(e.clientX);
  hoverSec.value = t;
  if (dragFrom != null) {
    sel.value = { a: Math.min(dragFrom, t), b: Math.max(dragFrom, t) };
  }
}
function ovUp(e: MouseEvent) {
  if (e.button !== 0) return;
  const t = xToTime(e.clientX);
  if (dragFrom != null && Math.abs(t - dragFrom) < 0.005) {
    // 单击 → 只设光标
    sel.value = null;
    cursorSec.value = t;
  }
  dragFrom = null;
}
function ovLeave() {
  dragFrom = null;
  hoverSec.value = null;
}
function pct(t: number): string {
  const d = activeDoc.value;
  if (!d || d.info.durationSec <= 0) return '0%';
  return `${(t / d.info.durationSec) * 100}%`;
}
const selStyle = computed(() => {
  if (!sel.value || !activeDoc.value) return {};
  const dur = activeDoc.value.info.durationSec;
  return {
    left: `${(sel.value.a / dur) * 100}%`,
    width: `${((sel.value.b - sel.value.a) / dur) * 100}%`,
  };
});

// ---- LCD 时间 ----
const lcdTime = computed(() => {
  if (recording.value) return recSec.value;
  if (playing.value) return playheadSec.value;
  return hoverSec.value ?? cursorSec.value;
});

// ---- 快捷键 ----
function onKey(e: KeyboardEvent) {
  const tag = (e.target as HTMLElement | null)?.tagName ?? '';
  const typing = tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA';
  if (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === 's') {
    e.preventDefault();
    openSaveAs('saveas');
  } else if (e.ctrlKey && e.key.toLowerCase() === 's') {
    e.preventDefault();
    ctxSave();
  } else if (e.ctrlKey && e.key.toLowerCase() === 'a' && !typing) {
    e.preventDefault();
    ctxSelectAll();
  } else if (e.ctrlKey && e.key.toLowerCase() === 'o') {
    e.preventDefault();
    triggerOpen();
  } else if (e.ctrlKey && e.key === '1') {
    e.preventDefault();
    setView('wave');
  } else if (e.ctrlKey && e.key === '2') {
    e.preventDefault();
    setView('spec');
  } else if (e.ctrlKey && e.key === '3') {
    e.preventDefault();
    setView('both');
  } else if (e.ctrlKey && e.key === '4') {
    e.preventDefault();
    setView('const');
  } else if (e.ctrlKey && e.key.toLowerCase() === 'w' && activeId.value) {
    e.preventDefault();
    closeDoc(activeId.value);
  } else if (e.code === 'Space' && !typing) {
    e.preventDefault();
    if (playing.value) stopPlayback();
    else playSelection();
  } else if (e.key === 'Escape') {
    openMenu.value = '';
    closeCtx();
    showInfo.value = false;
    showPrefs.value = false;
  }
}
onMounted(() => window.addEventListener('keydown', onKey));
onUnmounted(() => window.removeEventListener('keydown', onKey));

// 频谱 hop 变化后清缓存重画
watch(specHop, () => {
  specCache.clear();
  if (view.value === 'spec' || view.value === 'both') redrawAll();
});

// ================= 右键菜单（ocenaudio 式：播放 / 查看信息） =================
const ctx = ref<{ x: number; y: number; docId: string } | null>(null);
const ctxEl = ref<HTMLElement | null>(null);
const ctxDoc = computed(() => docs.value.find((d) => d.docId === ctx.value?.docId) ?? null);

/** 全局接管右键：屏蔽 WebView2 自带的浏览器菜单（返回/刷新/另存为…） */
function onCtx(e: MouseEvent, docId?: string) {
  const tag = (e.target as HTMLElement | null)?.tagName ?? '';
  // 输入框内保留原生菜单（复制/粘贴）
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
  e.preventDefault();
  const d = (docId ? docs.value.find((x) => x.docId === docId) : activeDoc.value) ?? null;
  openMenu.value = '';
  // 先按估算值定位，挂载后再用真实尺寸兜一次（菜单项随文档状态增减，估不准）
  ctx.value = {
    x: Math.max(6, Math.min(e.clientX, window.innerWidth - 210)),
    y: Math.max(6, Math.min(e.clientY, window.innerHeight - 470)),
    docId: d?.docId ?? '',
  };
  nextTick(() => {
    const el = ctxEl.value;
    if (!el || !ctx.value) return;
    const r = el.getBoundingClientRect();
    ctx.value = {
      ...ctx.value,
      x: Math.max(6, Math.min(ctx.value.x, window.innerWidth - r.width - 8)),
      y: Math.max(6, Math.min(ctx.value.y, window.innerHeight - r.height - 8)),
    };
  });
}
function closeCtx() {
  ctx.value = null;
}
function onAppClick() {
  openMenu.value = '';
  closeCtx();
}

/** 右键 → 播放（有选区播选区，否则从光标播到结尾） */
async function ctxPlay() {
  const id = ctx.value?.docId;
  closeCtx();
  if (!id) return;
  if (id !== activeId.value) {
    activeId.value = id;
    await nextTick();
    await redrawAll();
  }
  const d = activeDoc.value;
  if (!d) return;
  if (sel.value && sel.value.b - sel.value.a > 0.001) startPlayback(sel.value.a, sel.value.b);
  else startPlayback(cursorSec.value, d.info.durationSec);
}

// ---- 查看信息（信号属性对话框） ----
const showInfo = ref(false);
const info = ref<DocDetail | null>(null);
async function openInfo(docId?: string) {
  const id = docId ?? activeId.value;
  closeCtx();
  if (!id) return;
  showInfo.value = true;
  info.value = null;
  try {
    info.value = await eng.docInfo(id);
  } catch (e) {
    status.value = `读取信息失败: ${errText(e)}`;
    showInfo.value = false;
  }
}
async function ctxInfo() {
  await openInfo(ctx.value?.docId);
}

// ================= 「转换为」：声道 / 采样率 =================
const RATES: { hz: number; label: string }[] = [
  { hz: 8000, label: '8 kHz' },
  { hz: 16000, label: '16 kHz' },
  { hz: 22050, label: '22.05 kHz' },
  { hz: 32000, label: '32 kHz' },
  { hz: 44100, label: '44.1 kHz' },
  { hz: 48000, label: '48 kHz' },
];

/** 模态框的操作目标 —— 右键侧栏里「非当前」文档时，必须锁住被点的那一个，
 *  否则确认动作会落到 activeDoc 上（改错文件）。 */
const dlgDocId = ref('');
const dlgDoc = computed(
  () => docs.value.find((d) => d.docId === dlgDocId.value) ?? activeDoc.value,
);

/** 转换结果是一份完整的新文档（peaks/stats 都刷新过）→ 原地替换并重绘 */
async function applyDocMutation(p: Promise<DocOpened>) {
  const id = ctx.value?.docId ?? activeId.value;
  closeCtx();
  if (!id) return;
  const keepPath = docs.value.find((d) => d.docId === id)?.path;
  busy.value = true;
  status.value = '转换中…';
  try {
    await stopPlayback();
    const updated = await p;
    if (!updated.path) updated.path = keepPath;
    const i = docs.value.findIndex((d) => d.docId === updated.docId);
    if (i >= 0) docs.value[i] = updated;
    else docs.value.push(updated);
    waveCache.delete(updated.docId);
    for (const k of [...specCache.keys()]) if (k.startsWith(updated.docId + ':')) specCache.delete(k);
    sel.value = null;
    await addDoc(updated);
    status.value = `已转换：${updated.info.sampleRate} Hz · ${chText(updated.info.channels)} · ${fmtTime(updated.info.durationSec)}`;
  } catch (e) {
    status.value = `转换失败: ${errText(e)}`;
  } finally {
    busy.value = false;
  }
}

function ctxConvertChannels() {
  const d = ctxDoc.value;
  if (!d) return;
  const target = d.info.channels === 1 ? 2 : 1;
  applyDocMutation(eng.docChannels(d.docId, target));
}

function ctxResample(hz: number) {
  const d = ctxDoc.value;
  if (!d) return;
  applyDocMutation(eng.docResample(d.docId, hz));
}

// 「其他…」采样率
const showRateAsk = ref(false);
const rateAsk = ref(44100);
function ctxResampleCustom() {
  const d = ctxDoc.value ?? activeDoc.value;
  if (!d) return;
  dlgDocId.value = d.docId;
  rateAsk.value = d.info.sampleRate;
  closeCtx();
  showRateAsk.value = true;
}
function doRateAsk() {
  const d = dlgDoc.value;
  showRateAsk.value = false;
  if (!d) return;
  const hz = Math.round(Number(rateAsk.value));
  if (!Number.isFinite(hz) || hz < 4000 || hz > 384000) {
    status.value = '采样率需在 4000–384000 Hz 之间';
    return;
  }
  applyDocMutation(eng.docResample(d.docId, hz));
}

function ctxSelectAll() {
  const d = ctxDoc.value ?? activeDoc.value;
  closeCtx();
  if (!d) return;
  if (d.docId !== activeId.value) activeId.value = d.docId;
  sel.value = { a: 0, b: d.info.durationSec };
  cursorSec.value = 0;
}

function ctxDeselect() {
  closeCtx();
  sel.value = null;
}

// ================= 「重命名…」 =================
const showRename = ref(false);
const renameTo = ref('');
function ctxRename() {
  const d = ctxDoc.value ?? activeDoc.value;
  closeCtx();
  if (!d) return;
  dlgDocId.value = d.docId;
  renameTo.value = d.name;
  showRename.value = true;
}
async function doRename() {
  const d = dlgDoc.value;
  const name = renameTo.value.trim();
  if (!d || !name) return;
  showRename.value = false;
  try {
    const r = await eng.docRename(d.docId, name);
    const doc = docs.value.find((x) => x.docId === d.docId);
    if (doc) {
      doc.name = r.name;
      doc.path = r.path;
    }
    status.value = `已重命名为 ${r.name}`;
  } catch (e) {
    status.value = `重命名失败: ${errText(e)}`;
  }
}

// ================= 保存 / 导出… / 另存为… =================
const caps = ref<EncoderCaps | null>(null);
const showSave = ref(false);
/** 'save' = 覆盖原文件；'export' = 导出选区；'saveas' = 另存为新文件 */
const saveMode = ref<'save' | 'export' | 'saveas'>('saveas');
const saveForm = ref({
  path: '',
  format: 'wav',
  bitDepth: 16,
  bitrateKbps: 192,
  selectionOnly: false,
});

async function loadCaps() {
  if (caps.value) return caps.value;
  try {
    caps.value = await eng.encoderCaps();
  } catch (e) {
    status.value = `读取编码器能力失败: ${errText(e)}`;
  }
  return caps.value;
}

function extOf(path: string): string {
  const m = /\.([a-z0-9]+)$/i.exec(path);
  return m ? m[1].toLowerCase() : '';
}
function withoutExt(path: string): string {
  return path.replace(/\.[a-z0-9]+$/i, '');
}
function joinPath(dir: string, name: string): string {
  const sep = dir.includes('\\') ? '\\' : '/';
  return dir.replace(/[\\/]+$/, '') + sep + name;
}

async function openSaveAs(mode: 'export' | 'saveas') {
  const d = ctxDoc.value ?? activeDoc.value;
  closeCtx();
  if (!d) return;
  const c = await loadCaps();
  saveMode.value = mode;
  dlgDocId.value = d.docId;
  const src = d.path ?? '';
  const base = src ? withoutExt(src) : d.name.replace(/\.[a-z0-9]+$/i, '');
  const guessExt = extOf(src) || 'wav';
  const available = (c?.formats ?? []).filter((f) => f.available !== false);
  const format = available.some((f) => f.ext === guessExt)
    ? guessExt
    : (available[0]?.ext ?? 'wav');
  const isExport = mode === 'export';
  saveForm.value = {
    // 磁盘文档 → 预填同目录的「-副本 / -导出」；内存文档（录音）留空强制选位置，
    // 否则相对路径会写进进程 CWD，用户根本找不到文件
    path: src ? `${base}${isExport ? '-导出' : '-副本'}.${format}` : '',
    format,
    bitDepth: 16,
    bitrateKbps: 192,
    selectionOnly: isExport && !!sel.value,
  };
  showSave.value = true;
}

/** 绝对路径校验：Windows 盘符 / UNC，或 POSIX 根路径 */
const savePathOk = computed(() => {
  const p = saveForm.value.path.trim();
  if (!p) return false;
  if (!IS_TAURI) return true; // 浏览器模式由服务端解析
  return /^[a-zA-Z]:[\\/]/.test(p) || p.startsWith('\\\\') || p.startsWith('/');
});

/** 改格式时同步后缀 + 切换可用参数 */
function onFormatChange() {
  const f = saveForm.value;
  f.path = `${withoutExt(f.path)}.${f.format}`;
}

const saveFmt = computed(() => caps.value?.formats.find((f) => f.ext === saveForm.value.format) ?? null);
const saveIsPcm = computed(() => !!saveFmt.value && !saveFmt.value.lossy);
const saveIsLossy = computed(() => !!saveFmt.value?.lossy);

function pickSavePath() {
  const f = saveForm.value;
  const doPick = async () => {
    const { save } = await import('@tauri-apps/plugin-dialog');
    const sel2 = await save({
      title: saveMode.value === 'export' ? '导出音频' : '另存为',
      defaultPath: f.path,
      filters: [{ name: saveFmt.value?.label ?? '音频', extensions: [f.format] }],
    });
    if (typeof sel2 === 'string' && sel2) {
      f.path = sel2;
      const e = extOf(sel2);
      if (e && (caps.value?.formats ?? []).some((x) => x.ext === e)) f.format = e;
    }
  };
  if (!IS_TAURI) {
    const p = window.prompt('输出路径（服务端本地）：', f.path);
    if (p) f.path = p;
    return;
  }
  doPick();
}

async function doSave() {
  const d = dlgDoc.value;
  const f = saveForm.value;
  if (!d || !savePathOk.value) return;
  const begin = f.selectionOnly && sel.value ? Math.floor(sel.value.a * d.info.sampleRate) : 0;
  const end = f.selectionOnly && sel.value
    ? Math.floor(sel.value.b * d.info.sampleRate)
    : d.info.frames;
  showSave.value = false;
  busy.value = true;
  status.value = '写出中…';
  try {
    const r = await eng.docSave({
      docId: d.docId,
      path: f.path.trim(),
      format: f.format,
      bitDepth: f.bitDepth,
      bitrateKbps: f.bitrateKbps,
      begin,
      end,
    });
    const doc = docs.value.find((x) => x.docId === d.docId);
    if (doc && r.rebound) {
      doc.path = r.path;
      doc.name = r.path.split(/[\\/]/).pop() ?? doc.name;
    }
    status.value =
      `已写出 ${r.path} · ${fmtBytes(r.bytes)} · ${r.format.toUpperCase()}` +
      (r.viaFfmpeg ? ' · ffmpeg' : '') +
      (r.region ? ` · 选区 ${fmtTime((end - begin) / d.info.sampleRate)}` : '');
  } catch (e) {
    status.value = `写出失败: ${errText(e)}`;
  } finally {
    busy.value = false;
  }
}

/** 「保存」：直接覆盖原文件；内存文档（录音）则退化为「另存为…」 */
async function ctxSave() {
  const d = ctxDoc.value ?? activeDoc.value;
  closeCtx();
  if (!d) return;
  if (!d.path) {
    await openSaveAs('saveas');
    return;
  }
  busy.value = true;
  status.value = '保存中…';
  try {
    const r = await eng.docSave({ docId: d.docId });
    status.value = `已保存 ${r.path} · ${fmtBytes(r.bytes)} · ${r.format.toUpperCase()}`;
  } catch (e) {
    status.value = `保存失败: ${errText(e)}`;
  } finally {
    busy.value = false;
  }
}

function fmtBytes(b: number | null): string {
  if (b == null) return '—';
  if (b >= 1024 ** 3) return `${(b / 1024 ** 3).toFixed(2)} GB`;
  if (b >= 1024 ** 2) return `${(b / 1024 ** 2).toFixed(2)} MB`;
  if (b >= 1024) return `${(b / 1024).toFixed(1)} KB`;
  return `${b} B`;
}
function fmtDuration(s: number): string {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = (s - h * 3600 - m * 60).toFixed(3).padStart(6, '0');
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}
function fmtHz(sr: number): string {
  return `${sr.toLocaleString('en-US')} Hz`;
}
function fmtDb(v: number, unit: string): string {
  return Number.isFinite(v) ? `${v.toFixed(1)} ${unit}` : '—';
}

/** 信号属性分组表（ocenaudio「查看信息」版式） */
const infoSections = computed(() => {
  const d = info.value;
  if (!d) return [] as { title: string; rows: [string, string][] }[];
  const res: [string, string][] =
    d.bitsPerSample && d.sampleRate && d.channels
      ? [
          [
            '数据速率',
            `${((d.sampleRate * d.channels * d.bitsPerSample) / 1000).toFixed(0)} kbps（未压缩）`,
          ],
        ]
      : [];
  const peaks = d.channelPeaksDbfs
    .map((v, i) => `ch${i} ${Number.isFinite(v) ? v.toFixed(1) : '-∞'}`)
    .join('   ');
  return [
    {
      title: '文件',
      rows: [
        ['文件名', d.name],
        ['位置', d.path || '（内存 · 录音产物）'],
        ['容器格式', d.container ?? '—'],
        ['文件大小', fmtBytes(d.fileBytes)],
        ['平均比特率', d.bitrateKbps ? `${d.bitrateKbps.toFixed(0)} kbps` : '—'],
      ],
    },
    {
      title: '信号属性',
      rows: [
        ['格式', d.codecLabel],
        ['分辨率', d.bitsPerSample ? `${d.bitsPerSample} 位` : '32 位浮点（引擎内部表示）'],
        ['通道', d.channelsText],
        ['采样率', fmtHz(d.sampleRate)],
        ['时长', fmtDuration(d.durationSec)],
        ['每声道样本数', d.frames.toLocaleString('en-US')],
        ['总样本数', (d.frames * d.channels).toLocaleString('en-US')],
        ...res,
      ],
    },
    {
      title: '电平',
      rows: [
        ['整体响度', d.stats ? fmtDb(d.stats.integratedLufs, 'LUFS') : '—'],
        ['响度范围', d.stats ? fmtDb(d.stats.loudnessRangeLu, 'LU') : '—'],
        ['真峰值', d.stats ? fmtDb(d.stats.truePeakDbtp, 'dBTP') : '—'],
        ['样本峰值', d.stats ? fmtDb(d.stats.samplePeakDbfs, 'dBFS') : '—'],
        ['各声道峰值', peaks || '—'],
      ],
    },
  ];
});
</script>

<template>
  <div class="app" @click="onAppClick" @contextmenu="onCtx">
    <!-- 菜单栏 -->
    <div class="menubar">
      <div class="menu" :class="{ open: openMenu === 'file' }" @click.stop="toggleMenu('file')">文件(F)
        <div class="dropdown" v-if="openMenu === 'file'">
          <div class="mi" @click="triggerOpen"><span>打开(O)…</span><span class="sc">Ctrl+O</span></div>
          <div class="mi has-sub" :class="{ dis: !recents.length }">
            <span>最近打开(R)</span><span class="sc">▸</span>
            <div class="submenu" v-if="recents.length">
              <div class="mi" v-for="p in recents" :key="p" :title="p" @click="openRecent(p)">
                <span class="ri-name">{{ baseName(p) }}</span>
                <span class="ri-path">{{ p }}</span>
              </div>
              <div class="sep"></div>
              <div class="mi" @click="clearRecents"><span>清空历史</span></div>
            </div>
          </div>
          <div class="sep"></div>
          <div class="mi" :class="{ dis: !activeDoc }" @click="ctxSave">
            <span>保存(S)</span><span class="sc">Ctrl+S</span>
          </div>
          <div class="mi" :class="{ dis: !activeDoc }" @click="openSaveAs('saveas')">
            <span>另存为(A)…</span><span class="sc">Ctrl+Shift+S</span>
          </div>
          <div class="mi" :class="{ dis: !activeDoc }" @click="openSaveAs('export')">
            <span>导出(E)…</span>
          </div>
          <div class="sep"></div>
          <div class="mi" :class="{ dis: !activeDoc }" @click="activeDoc && closeDoc(activeDoc.docId)">
            <span>关闭当前(C)</span><span class="sc">Ctrl+W</span>
          </div>
          <div class="mi" :class="{ dis: !docs.length }" @click="closeAll"><span>全部关闭</span></div>
          <div class="sep"></div>
          <div class="mi" @click="quit"><span>退出(X)</span></div>
        </div>
      </div>
      <div class="menu" :class="{ open: openMenu === 'edit' }" @click.stop="toggleMenu('edit')">编辑(E)
        <div class="dropdown" v-if="openMenu === 'edit'">
          <div class="mi" @click="openPrefs"><span>首选项(P)…</span></div>
        </div>
      </div>
      <div class="menu" :class="{ open: openMenu === 'view' }" @click.stop="toggleMenu('view')">视图(V)
        <div class="dropdown" v-if="openMenu === 'view'">
          <div class="mi" @click="setView('wave')">
            <span class="chk">{{ view === 'wave' ? '✓' : '' }}</span><span>波形视图(W)</span><span class="sc">Ctrl+1</span>
          </div>
          <div class="mi" @click="setView('spec')">
            <span class="chk">{{ view === 'spec' ? '✓' : '' }}</span><span>频谱视图(S)</span><span class="sc">Ctrl+2</span>
          </div>
          <div class="mi" @click="setView('both')">
            <span class="chk">{{ view === 'both' ? '✓' : '' }}</span><span>波形和频谱视图(B)</span><span class="sc">Ctrl+3</span>
          </div>
          <div class="sep"></div>
          <div class="mi" @click="setView('const')">
            <span class="chk">{{ view === 'const' ? '✓' : '' }}</span><span>星座图</span><span class="sc">Ctrl+4</span>
          </div>
        </div>
      </div>
      <span class="appname">Sonari</span>
    </div>

    <!-- 操作栏：图标 + LCD -->
    <div class="toolbar">
      <button class="tbtn" @click.stop="triggerOpen" title="打开文件 (Ctrl+O)">
        <svg viewBox="0 0 16 16" width="15" height="15"><path d="M1.5 3.5h4.2l1.4 1.6h7.4v7.4H1.5z" fill="#e8b34b" stroke="#b98d33" stroke-width="1"/></svg>
      </button>
      <button class="tbtn" :class="{ rec: recording }" @click.stop="toggleRecord" :title="recording ? '停止录音' : '录音'">
        <svg viewBox="0 0 16 16" width="15" height="15"><circle cx="8" cy="8" r="5.5" :fill="recording ? '#ff5a5a' : '#e05252'" stroke="#a83838" stroke-width="1"/></svg>
      </button>
      <button class="tbtn" :disabled="!activeDoc || playing" @click.stop="playSelection" title="播放 (空格)">
        <svg viewBox="0 0 16 16" width="15" height="15"><path d="M4 2.5l9 5.5-9 5.5z" fill="#41d17f"/></svg>
      </button>
      <button class="tbtn" :disabled="!playing && !recording" @click.stop="stopAll" title="停止">
        <svg viewBox="0 0 16 16" width="15" height="15"><rect x="3.5" y="3.5" width="9" height="9" rx="1" fill="#d8dee5"/></svg>
      </button>

      <div class="lcd">
        <div class="lcd-top">
          <span>{{ activeDoc ? activeDoc.info.sampleRate + ' Hz' : '— Hz' }}</span>
          <span>{{ activeDoc ? chText(activeDoc.info.channels) : '—' }}</span>
          <span v-if="recording" class="lcd-rec">● REC</span>
        </div>
        <div class="lcd-time">{{ lcdTime.toFixed(3) }}<span class="lcd-unit">秒</span></div>
      </div>

      <span v-if="busy" class="busy">⏳ 处理中…</span>
      <span class="filename">{{ activeDoc ? activeDoc.name : '未打开文件' }}</span>
    </div>

    <!-- 主体：侧栏 + 视图 -->
    <div class="body">
      <aside>
        <div class="side-title">已打开的文件</div>
        <div v-for="d in docs" :key="d.docId" class="fileitem" :class="{ on: d.docId === activeId }"
          @click.stop="selectDoc(d.docId)" @contextmenu.stop="onCtx($event, d.docId)">
          <div class="fi-icon">🎵</div>
          <div class="fi-text">
            <div class="fi-name" :title="d.name">{{ d.name }}</div>
            <div class="fi-meta">{{ fmtMeta(d) }}</div>
          </div>
          <button class="fi-close" title="关闭" @click.stop="closeDoc(d.docId)">✕</button>
        </div>
        <div v-if="!docs.length" class="side-empty">尚无打开的文件</div>
      </aside>

      <main ref="viewareaEl" class="viewarea">
        <template v-if="activeDoc">
          <div class="trow overview-row">
            <div class="chlabel">总览</div>
            <div class="cwrap ovwrap"><canvas ref="overviewCanvas"></canvas></div>
          </div>

          <div class="rows" ref="rowsEl">
            <template v-if="view === 'wave' || view === 'both'">
              <div v-for="c in chList" :key="'w' + c" class="trow">
                <div class="chlabel">#{{ c - 1 }}</div>
                <div class="cwrap"><canvas :ref="(el) => setCanvas(waveCanvases, c - 1, el)"></canvas></div>
              </div>
            </template>

            <template v-if="view === 'spec' || view === 'both'">
              <div v-for="c in chList" :key="'s' + c" class="trow">
                <div class="chlabel dim">spec {{ c - 1 }}</div>
                <div class="cwrap"><canvas :ref="(el) => setCanvas(specCanvases, c - 1, el)"></canvas></div>
              </div>
            </template>

            <template v-if="view === 'const'">
              <div class="const-params">
                FFT
                <select v-model.number="fftLen">
                  <option :value="256">256</option>
                  <option :value="512">512</option>
                  <option :value="1024">1024</option>
                  <option :value="2048">2048</option>
                  <option :value="4096">4096</option>
                </select>
                hop <input class="num" v-model.number="hop" />
                峰/帧 <input class="num" v-model.number="peaksPerFrame" />
                fan-out <input class="num" v-model.number="fanOut" />
                动态范围 <input class="num" v-model.number="dynRange" /> dB
                <label><input type="checkbox" v-model="preEmphasis" /> 预加重</label>
                <button :disabled="busy" @click.stop="drawConst">重新计算</button>
              </div>
              <div class="trow">
                <div class="chlabel">星座</div>
                <div class="cwrap"><canvas ref="constCanvas"></canvas></div>
              </div>
            </template>

            <!-- 光标 / 选区 / 播放头 覆盖层 -->
            <div class="overlay" v-if="view !== 'const'" @mousedown="ovDown" @mousemove="ovMove" @mouseup="ovUp"
              @mouseleave="ovLeave">
              <div class="sel-box" v-if="sel" :style="selStyle"></div>
              <div class="cursor-line" v-if="hoverSec != null" :style="{ left: pct(hoverSec) }"></div>
              <div class="playhead" v-if="playing" :style="{ left: pct(playheadSec) }"></div>
            </div>
          </div>

          <div class="trow ruler-row">
            <div class="chlabel"></div>
            <div class="cwrap rulerwrap"><canvas ref="rulerCanvas"></canvas></div>
          </div>
        </template>
        <div v-else class="empty">
          <div class="empty-icon">🎵</div>
          <div>菜单"文件 → 打开…（Ctrl+O）"选择音频文件，或点击左上角 📂 按钮</div>
          <div class="empty-sub">支持 WAV / MP3 / FLAC / OGG / Opus · 双声道按声道分行显示 · 空格播放 / 拖动选择</div>
        </div>
      </main>
    </div>

    <!-- 状态栏 -->
    <footer>
      <span class="status">{{ status }}</span>
      <span class="info">{{ infoText }}</span>
      <span class="mode">{{ IS_TAURI ? '桌面模式 · 引擎内嵌' : '浏览器模式 · ws://127.0.0.1:9712' }}</span>
    </footer>

    <!-- 右键菜单（ocenaudio 式，屏蔽 WebView2 自带浏览器菜单） -->
    <div class="ctx-mask" v-if="ctx" @click="closeCtx" @contextmenu.prevent="closeCtx"></div>
    <div class="ctx-menu" v-if="ctx" ref="ctxEl" :style="{ left: ctx.x + 'px', top: ctx.y + 'px' }"
      @click="closeCtx" @contextmenu.prevent>
      <div class="ctx-title" :title="ctxDoc ? ctxDoc.path || ctxDoc.name : ''">
        <div class="ctx-name">{{ ctxDoc ? ctxDoc.name : '无文档' }}</div>
        <div class="ctx-meta" v-if="ctxDoc">
          {{ chText(ctxDoc.info.channels) }} · {{ ctxDoc.info.sampleRate }} Hz ·
          {{ fmtTime(ctxDoc.info.durationSec) }}
        </div>
      </div>
      <div class="sep"></div>

      <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxConvertChannels">
        <span class="ci">⇄</span>
        <span>{{ ctxDoc && ctxDoc.info.channels === 1 ? '转换为立体声' : '转换为单声道' }}</span>
      </div>

      <div class="mi has-sub" :class="{ dis: !ctxDoc }">
        <span class="ci">◴</span><span>转换为</span><span class="sc">▸</span>
        <div class="submenu submenu-rates">
          <div v-for="r in RATES" :key="r.hz" class="mi"
            :class="{ dis: ctxDoc && ctxDoc.info.sampleRate === r.hz }" @click.stop="ctxResample(r.hz)">
            <span class="chk">{{ ctxDoc && ctxDoc.info.sampleRate === r.hz ? '✓' : '' }}</span>
            <span>{{ r.label }}</span>
          </div>
          <div class="sep"></div>
          <div class="mi" @click.stop="ctxResampleCustom">
            <span class="chk"></span><span>其他…</span>
          </div>
        </div>
      </div>

      <div class="sep"></div>
      <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxInfo">
        <span class="ci">ⓘ</span><span>查看信息…</span>
      </div>
      <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxPlay">
        <span class="ci">▶</span><span>播放</span><span class="sc">Space</span>
      </div>

      <div class="sep"></div>
      <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxSave">
        <span class="ci">💾</span><span>保存</span><span class="sc">Ctrl+S</span>
      </div>
      <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="openSaveAs('export')">
        <span class="ci">⇱</span><span>导出…</span>
      </div>
      <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="openSaveAs('saveas')">
        <span class="ci">⇱</span><span>另存为…</span><span class="sc">Ctrl+Shift+S</span>
      </div>
      <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxRename">
        <span class="ci">✎</span><span>重命名…</span>
      </div>

      <div class="sep"></div>
      <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxSelectAll">
        <span class="ci">▭</span><span>全选</span><span class="sc">Ctrl+A</span>
      </div>
      <div class="mi" :class="{ dis: !sel }" @click.stop="ctxDeselect">
        <span class="ci"></span><span>取消选择</span>
      </div>
    </div>

    <!-- 其他采样率 -->
    <div class="modal-mask" v-if="showRateAsk" @click.self="showRateAsk = false">
      <div class="modal modal-sm">
        <div class="modal-title">转换为采样率</div>
        <div class="form-row">
          <label>目标采样率</label>
          <input class="num" v-model.number="rateAsk" type="number" min="4000" max="384000" />
          <span class="unit">Hz</span>
        </div>
        <div class="form-row">
          <label>当前</label>
          <span class="ro">{{ dlgDoc ? dlgDoc.info.sampleRate + ' Hz' : '—' }}</span>
        </div>
        <div class="modal-foot">
          <span class="pref-note">允许 4000–384000 Hz，时长不变、帧数按比例缩放</span>
          <button @click="doRateAsk">转换</button>
        </div>
      </div>
    </div>

    <!-- 重命名 -->
    <div class="modal-mask" v-if="showRename" @click.self="showRename = false">
      <div class="modal modal-sm">
        <div class="modal-title">重命名</div>
        <div class="form-row">
          <label>新名称</label>
          <input v-model="renameTo" style="flex: 1" @keydown.enter="doRename" />
        </div>
        <div class="form-row">
          <label>位置</label>
          <span class="ro">{{ dlgDoc?.path || '（内存 · 录音产物，仅改显示名）' }}</span>
        </div>
        <div class="modal-foot">
          <span class="pref-note">含扩展名；磁盘文件会被真实改名</span>
          <button @click="doRename">确定</button>
        </div>
      </div>
    </div>

    <!-- 保存 / 导出 / 另存为 -->
    <div class="modal-mask" v-if="showSave" @click.self="showSave = false">
      <div class="modal">
        <div class="modal-title">
          {{ saveMode === 'export' ? '导出音频' : saveMode === 'saveas' ? '另存为' : '保存' }}
        </div>

        <div class="form-row">
          <label>输出路径</label>
          <input v-model="saveForm.path" style="flex: 1" :placeholder="dlgDoc?.path || '点「浏览…」选择位置'" />
          <button @click.stop="pickSavePath">浏览…</button>
        </div>

        <div class="form-row">
          <label>格式</label>
          <select v-model="saveForm.format" @change="onFormatChange">
            <option v-for="f in caps?.formats ?? []" :key="f.ext" :value="f.ext"
              :disabled="f.available === false">
              {{ f.label }}{{ f.available === false ? ' —— 需要 ffmpeg' : '' }}
            </option>
          </select>
          <span class="ro" v-if="caps">
            {{ caps.ffmpeg ? `ffmpeg 可用` : '未检测到 ffmpeg：仅 WAV 可写' }}
          </span>
        </div>

        <template v-if="saveIsPcm">
          <div class="form-row">
            <label>分辨率</label>
            <select v-model.number="saveForm.bitDepth">
              <option v-for="b in caps?.bitDepths ?? [16, 24, 32]" :key="b" :value="b">
                {{ b === 32 ? '32 位浮点' : b + ' 位 PCM' }}
              </option>
            </select>
          </div>
        </template>
        <template v-else-if="saveIsLossy">
          <div class="form-row">
            <label>码率</label>
            <select v-model.number="saveForm.bitrateKbps">
              <option v-for="b in caps?.bitrates ?? [128, 192, 256, 320]" :key="b" :value="b">
                {{ b }} kbps
              </option>
            </select>
          </div>
        </template>

        <div class="form-row">
          <label>范围</label>
          <label class="chk-inline">
            <input type="checkbox" v-model="saveForm.selectionOnly" :disabled="!sel" />
            仅导出选区{{ sel ? `（${fmtTime(sel.b - sel.a)}）` : '（当前无选区）' }}
          </label>
        </div>

        <div class="save-preview">
          <div><span class="pk">源</span>{{ dlgDoc?.info.sampleRate }} Hz ·
            {{ dlgDoc ? chText(dlgDoc.info.channels) : '' }} ·
            {{ dlgDoc ? fmtTime(dlgDoc.info.durationSec) : '' }}</div>
          <div><span class="pk">目标</span>{{ saveForm.format.toUpperCase() }} ·
            {{ saveIsPcm ? (saveForm.bitDepth === 32 ? '32 位浮点' : saveForm.bitDepth + ' 位') : saveForm.bitrateKbps + ' kbps' }}</div>
        </div>

        <div class="modal-foot">
          <span class="pref-note">
            <template v-if="!savePathOk">请先点「浏览…」选择输出位置（需要绝对路径）</template>
            <template v-else>{{ saveIsPcm ? '无损写出' : '有损编码，经 FFMPEG 兜底' }}</template>
          </span>
          <button @click="doSave" :disabled="busy || !savePathOk">写出</button>
        </div>
      </div>
    </div>

    <!-- 查看信息 · 信号属性 -->
    <div class="modal-mask" v-if="showInfo" @click.self="showInfo = false">
      <div class="modal info-modal">
        <div class="modal-title">查看信息 — 信号属性</div>
        <div v-if="!info" class="info-loading">读取中…</div>
        <template v-else>
          <div v-for="s in infoSections" :key="s.title" class="info-sec">
            <div class="info-sec-title">{{ s.title }}</div>
            <table class="info-tab">
              <tr v-for="r in s.rows" :key="r[0]">
                <td class="info-k">{{ r[0] }}</td>
                <td class="info-v" :title="r[1]">{{ r[1] }}</td>
              </tr>
            </table>
          </div>
        </template>
        <div class="modal-foot">
          <span class="pref-note">{{ info ? `文档 ID ${info.docId}` : '' }}</span>
          <button @click="showInfo = false">确定</button>
        </div>
      </div>
    </div>

    <!-- 首选项 -->
    <div class="modal-mask" v-if="showPrefs" @click.self="showPrefs = false">
      <div class="modal">
        <div class="modal-title">首选项 — 音频</div>
        <div class="pref-host">
          <span class="host-badge">{{ devices.host || '音频后端' }}</span>
          <button :disabled="busy" @click.stop="loadDevices">⟳ 刷新设备</button>
        </div>

        <div class="pref-sec">播放设备（输出）</div>
        <div class="dev-list">
          <label v-for="dv in devices.outputs" :key="'o' + dv.name" class="dev"
            :class="{ sel: audioPrefs.output === dv.name }">
            <input type="radio" name="outdev" :value="dv.name" v-model="audioPrefs.output" />
            <span class="dev-icon">🔊</span>
            <span class="dev-name">{{ dv.name }}<b v-if="dv.isDefault" class="dev-def">系统默认</b></span>
            <span class="dev-meta">{{ dv.meta }}</span>
          </label>
          <div v-if="!devices.outputs.length" class="dev-empty">未枚举到输出设备</div>
        </div>

        <div class="pref-sec">录音设备（输入）</div>
        <div class="dev-list">
          <label v-for="dv in devices.inputs" :key="'i' + dv.name" class="dev"
            :class="{ sel: audioPrefs.input === dv.name }">
            <input type="radio" name="indev" :value="dv.name" v-model="audioPrefs.input" />
            <span class="dev-icon">🎤</span>
            <span class="dev-name">{{ dv.name }}<b v-if="dv.isDefault" class="dev-def">系统默认</b></span>
            <span class="dev-meta">{{ dv.meta }}</span>
          </label>
          <div v-if="!devices.inputs.length" class="dev-empty">未枚举到输入设备</div>
        </div>

        <div class="modal-foot">
          <span class="pref-note">选择即时生效：播放与录音按此处的设备执行</span>
          <button @click="showPrefs = false">关闭</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style>
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; font: 13px/1.5 'Segoe UI', system-ui, sans-serif; background: #14171c; color: #d8dee5; overflow: hidden; }

.app { height: 100vh; display: flex; flex-direction: column; }

/* 菜单栏 */
.menubar { display: flex; align-items: center; gap: 2px; background: #1b2027; border-bottom: 1px solid #262d36; padding: 0 8px; height: 30px; position: relative; z-index: 100; }
.menu { position: relative; padding: 3px 12px; cursor: pointer; border-radius: 4px; user-select: none; }
.menu:hover, .menu.open { background: #2a3340; }
.dropdown { position: absolute; top: 100%; left: 0; min-width: 250px; background: #232a33; border: 1px solid #35414d; border-radius: 6px; padding: 4px; box-shadow: 0 8px 24px rgba(0,0,0,0.5); }
.mi { display: flex; align-items: center; gap: 6px; padding: 6px 10px; border-radius: 4px; cursor: pointer; white-space: nowrap; }
.mi:hover { background: #2f5a78; }
.mi.dis { opacity: 0.4; pointer-events: none; }
.mi .sc { margin-left: auto; color: #6b7a8a; font-size: 11px; padding-left: 18px; }
.mi .chk { width: 14px; color: #1fd6a4; }
.mi.has-sub { position: relative; }
.submenu { display: none; position: absolute; left: 100%; top: -5px; min-width: 340px; max-width: 620px; background: #232a33; border: 1px solid #35414d; border-radius: 6px; padding: 4px; box-shadow: 0 8px 24px rgba(0,0,0,0.5); }
.mi.has-sub:hover .submenu { display: block; }
.submenu .ri-name { max-width: 200px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.submenu .ri-path { margin-left: auto; color: #56657a; font-size: 10px; max-width: 260px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sep { height: 1px; background: #35414d; margin: 4px 6px; }
.appname { margin-left: auto; color: #1fd6a4; font-weight: 600; letter-spacing: 0.5px; }

/* 操作栏 */
.toolbar { display: flex; gap: 4px; align-items: center; background: #181d23; border-bottom: 1px solid #232b33; padding: 5px 8px; }
.tbtn { width: 32px; height: 28px; padding: 0; display: inline-flex; align-items: center; justify-content: center; background: #20262e; border: 1px solid #2c343d; border-radius: 5px; cursor: pointer; }
.tbtn:hover:not(:disabled) { background: #2c3540; }
.tbtn:disabled { opacity: 0.4; cursor: default; }
.tbtn.rec { border-color: #ff5a5a; box-shadow: 0 0 8px rgba(255,90,90,0.5); animation: recPulse 1s infinite; }
@keyframes recPulse { 50% { box-shadow: 0 0 2px rgba(255,90,90,0.2); } }

/* LCD 电子显示屏 */
.lcd { margin-left: 8px; background: #04120a; border: 1px solid #0f3320; border-radius: 5px; padding: 2px 12px 3px; min-width: 220px; box-shadow: inset 0 0 8px rgba(0,0,0,0.8); }
.lcd-top { display: flex; gap: 12px; color: #1f9e4d; font-size: 10px; font-family: Consolas, monospace; }
.lcd-rec { color: #ff5a5a; animation: recBlink 1s infinite; }
@keyframes recBlink { 50% { opacity: 0.25; } }
.lcd-time { color: #35e86b; font: bold 20px/1.15 Consolas, 'Courier New', monospace; letter-spacing: 1px; text-shadow: 0 0 6px rgba(53,232,107,0.55); }
.lcd-unit { font-size: 11px; font-weight: normal; margin-left: 6px; opacity: 0.8; }

.toolbar .busy { color: #e8b34b; font-size: 12px; }
.toolbar .filename { color: #8fa1b3; margin-left: auto; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* 主体 */
.body { flex: 1; display: flex; min-height: 0; }
aside { width: 220px; background: #171c22; border-right: 1px solid #232b33; overflow-y: auto; padding: 6px; }
.side-title { color: #8fa1b3; font-size: 12px; padding: 4px 6px; border-bottom: 1px solid #232b33; margin-bottom: 6px; }
.side-empty { color: #56657a; font-size: 12px; padding: 10px 6px; }
.fileitem { display: flex; gap: 8px; align-items: center; padding: 8px; border-radius: 6px; cursor: pointer; border: 1px solid transparent; }
.fileitem:hover { background: #1f262e; }
.fileitem.on { background: #1c3a4a; border-color: #1fd6a4; }
.fi-icon { font-size: 18px; }
.fi-text { flex: 1; min-width: 0; }
.fi-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; }
.fi-meta { color: #6b7a8a; font-size: 11px; }
.fi-close { padding: 0 6px; font-size: 11px; background: transparent; border: none; color: #6b7a8a; }
.fi-close:hover { color: #ff7b7b; background: transparent; }

/* 视图区：行高自适应铺满，画布随容器尺寸重建 */
.viewarea { flex: 1; min-width: 0; display: flex; flex-direction: column; overflow: hidden; }
.trow { display: flex; gap: 6px; padding: 3px 8px; align-items: stretch; }
.overview-row { height: 54px; flex: none; }
.rows { flex: 1; position: relative; display: flex; flex-direction: column; min-height: 0; }
.rows .trow { flex: 1 1 0; min-height: 48px; }
.chlabel { width: 46px; flex: none; display: flex; align-items: center; justify-content: flex-end; padding-right: 4px; color: #8fa1b3; font-size: 11px; font-family: Consolas, monospace; }
.chlabel.dim { color: #56657a; }
.cwrap { flex: 1; position: relative; min-width: 0; }
.cwrap canvas { position: absolute; inset: 0; width: 100%; height: 100%; display: block; border: 1px solid #232b33; border-radius: 4px; background: #0d1117; }
.overview-row .cwrap canvas, .rulerwrap canvas { border-color: #1d2630; }
.ruler-row { height: 30px; flex: none; }

/* 覆盖层：光标/选区/播放头（画布列区域 = 左60px，右8px） */
.overlay { position: absolute; left: 60px; right: 8px; top: 0; bottom: 0; cursor: text; user-select: none; }
.sel-box { position: absolute; top: 0; bottom: 0; background: rgba(31, 214, 164, 0.14); border-left: 1px solid rgba(31,214,164,0.7); border-right: 1px solid rgba(31,214,164,0.7); pointer-events: none; }
.cursor-line { position: absolute; top: 0; bottom: 0; width: 1px; background: rgba(255,255,255,0.35); pointer-events: none; }
.playhead { position: absolute; top: 0; bottom: 0; width: 2px; background: #ff6b6b; box-shadow: 0 0 6px rgba(255,107,107,0.8); pointer-events: none; }

.const-params { display: flex; gap: 8px; align-items: center; color: #8fa1b3; margin: 4px 8px 0 54px; flex-wrap: wrap; font-size: 12px; }
input, select { background: #1d232a; border: 1px solid #2c343d; color: inherit; padding: 4px 8px; border-radius: 4px; font: inherit; }
input.num { width: 52px; }
button { background: #26303a; border: 1px solid #35414d; color: inherit; padding: 4px 14px; border-radius: 4px; cursor: pointer; font: inherit; }
button:hover:not(:disabled) { background: #31404d; }
button:disabled { opacity: 0.45; cursor: default; }

.empty { height: 100%; display: flex; flex-direction: column; align-items: center; justify-content: center; color: #56657a; gap: 8px; }
.empty-icon { font-size: 42px; }
.empty-sub { font-size: 12px; color: #45536a; }

/* 状态栏 */
footer { display: flex; gap: 16px; align-items: center; background: #1b2027; border-top: 1px solid #262d36; padding: 4px 12px; font-size: 12px; }
footer .status { color: #8fa1b3; }
footer .info { color: #6b7a8a; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
footer .mode { margin-left: auto; color: #45536a; font-size: 11px; }

/* 首选项模态 */
.modal-mask { position: fixed; inset: 0; background: rgba(0,0,0,0.55); display: flex; align-items: center; justify-content: center; z-index: 300; }
.modal { width: 560px; max-height: 80vh; overflow-y: auto; background: #1d232b; border: 1px solid #35414d; border-radius: 10px; padding: 14px 16px; box-shadow: 0 16px 48px rgba(0,0,0,0.6); }
.modal-title { font-size: 15px; font-weight: 600; color: #d8dee5; margin-bottom: 10px; }
.pref-host { display: flex; align-items: center; gap: 10px; margin-bottom: 10px; }
.host-badge { background: #1c4a63; border: 1px solid #4fc3f7; color: #bfe9ff; padding: 2px 10px; border-radius: 4px; font-size: 12px; }
.pref-sec { color: #8fa1b3; font-size: 12px; margin: 10px 0 6px; }
.dev-list { display: flex; flex-direction: column; gap: 4px; }
.dev { display: flex; gap: 8px; align-items: center; padding: 7px 10px; border: 1px solid #2c343d; border-radius: 6px; cursor: pointer; }
.dev:hover { background: #232b34; }
.dev.sel { border-color: #1fd6a4; background: #16303a; }
.dev-icon { font-size: 14px; }
.dev-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.dev-def { margin-left: 8px; color: #1fd6a4; font-size: 10px; border: 1px solid #1fd6a4; border-radius: 3px; padding: 0 4px; font-weight: normal; }
.dev-meta { color: #6b7a8a; font-size: 11px; white-space: nowrap; }
.dev-empty { color: #56657a; font-size: 12px; padding: 8px; }
.modal-foot { display: flex; align-items: center; gap: 10px; margin-top: 14px; }
.pref-note { color: #56657a; font-size: 11px; margin-right: auto; }

/* 右键菜单 */
.ctx-mask { position: fixed; inset: 0; z-index: 400; }
.ctx-menu { position: fixed; z-index: 401; min-width: 190px; background: #232a33; border: 1px solid #35414d; border-radius: 6px; padding: 4px; box-shadow: 0 10px 30px rgba(0,0,0,0.6); }
.ctx-title { padding: 4px 10px 6px; border-bottom: 1px solid #2c343d; margin-bottom: 4px; }
.ctx-name { color: #d8dee5; font-size: 12px; max-width: 280px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ctx-meta { color: #6b7a8a; font-size: 11px; font-family: Consolas, monospace; }
.ctx-menu .mi { padding: 6px 10px; }
.ctx-menu .ci { width: 16px; text-align: center; color: #1fd6a4; font-size: 11px; }
.submenu-rates { min-width: 150px; max-width: none; }
.ctx-menu .chk { width: 14px; color: #1fd6a4; }

/* 表单行（重命名 / 采样率 / 另存为） */
.modal-sm { width: 420px; }
.form-row { display: flex; align-items: center; gap: 10px; margin: 8px 0; }
.form-row > label { width: 76px; flex: none; color: #8fa1b3; font-size: 12px; }
.form-row .ro { color: #6b7a8a; font-size: 12px; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.form-row .unit { color: #6b7a8a; font-size: 12px; }
.chk-inline { display: flex; align-items: center; gap: 6px; color: #d8dee5; font-size: 12px; cursor: pointer; }
.save-preview { margin-top: 12px; padding: 8px 10px; background: #161c23; border: 1px solid #2c343d; border-radius: 6px; display: flex; flex-direction: column; gap: 4px; color: #8fa1b3; font-size: 12px; font-family: Consolas, monospace; }
.save-preview .pk { display: inline-block; width: 34px; color: #1fd6a4; }

/* 查看信息 · 信号属性 */
.info-modal { width: 520px; }
.info-loading { color: #8fa1b3; padding: 20px 4px; }
.info-sec { margin-bottom: 12px; }
.info-sec-title { color: #1fd6a4; font-size: 12px; font-weight: 600; padding: 4px 0 6px; border-bottom: 1px solid #2c343d; margin-bottom: 4px; }
.info-tab { width: 100%; border-collapse: collapse; table-layout: fixed; }
.info-tab td { padding: 3px 6px; vertical-align: top; font-size: 12px; }
.info-k { width: 130px; color: #8fa1b3; }
.info-v { color: #d8dee5; word-break: break-all; font-family: Consolas, 'Courier New', monospace; }
</style>
