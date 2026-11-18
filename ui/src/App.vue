<script setup lang="ts">
import { ref, computed, watch, nextTick, onMounted, onUnmounted } from 'vue';
import {
  Engine,
  errText,
  fileDialog,
  type DocOpened,
  type ClipInfo,
  type MssConstellation,
  type PacketLoss,
  type ConstellationResult,
  type DocDetail,
  type EncoderCaps,
} from './api';

const eng = new Engine();
/** 快捷键修饰键的显示：macOS 为 ⌘，其它为 Ctrl+ */
const IS_MAC = /Mac|iPhone|iPad/i.test(navigator.platform || navigator.userAgent);
const MOD = IS_MAC ? '⌘' : 'Ctrl+';

const status = ref('桌面模式 · 引擎内嵌');
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

// ---- 文档脏状态跟踪 ----
const dirtyDocs = ref(new Set<string>());
function markDirty(docId: string) { dirtyDocs.value.add(docId); }
function markClean(docId: string) { dirtyDocs.value.delete(docId); }
function isDirty(docId: string): boolean { return dirtyDocs.value.has(docId); }

// ---- 关闭前保存确认 ----
const showCloseConfirm = ref(false);
const closeConfirmDocId = ref('');
const closeConfirmDoc = computed(() => docs.value.find((d) => d.docId === closeConfirmDocId.value) ?? null);

// ---- 视图 ----
type ViewMode = 'wave' | 'spec' | 'both' | 'const' | 'waveconst';
const view = ref<ViewMode>('both');

// ---- 菜单 ----
const openMenu = ref<'' | 'file' | 'edit' | 'view'>('');
function toggleMenu(m: 'file' | 'edit' | 'view') {
  openMenu.value = openMenu.value === m ? '' : m;
}
/** 下拉菜单项点击后收起菜单（并阻止冒泡到标题再把菜单切回打开）；
 *  点在带子菜单的父项本身上则保持打开 */
function onDropdownClick(e: MouseEvent) {
  const mi = (e.target as HTMLElement).closest('.mi');
  if (mi && mi.classList.contains('has-sub') && !(e.target as HTMLElement).closest('.submenu')) return;
  openMenu.value = '';
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
/** 动态范围（dB）：频谱图请求与着色共用同一个值（星座图视图的「动态范围」输入也绑定它） */
const dynRange = ref(80);
/** 频谱图实际使用的动态范围（输入框清空/非法时回退 80 dB） */
function specDr(): number {
  const v = Number(dynRange.value);
  return Number.isFinite(v) && v >= 1 ? v : 80;
}
const preEmphasis = ref(false);
const specHop = ref(256);

// ---- 数据缓存 ----
const waveCache = new Map<string, number[][]>(); // docId -> number[][]（每声道平铺 [min,max]）
/** frames 为绝对电平（dBFS），maxDb = 该请求范围内最大值 */
type SpecData = { frames: number[][]; hop: number; begin: number; fft: number; maxDb: number };
const specCache = new Map<string, SpecData>(); // docId:ch:fft:dr -> 全文件频谱
const zoomSpecCache = new Map<string, SpecData>(); // docId:ch:fft:dr:begin:end:hop -> 缩放窗口频谱（小 LRU）
/** 按采样率选 FFT：最接近 25 ms 的 2 的幂，限制在 128…4096
 *  （8 kHz → 256，16 kHz → 512，44.1/48 kHz → 1024），让 20–40 ms 的丢包缺口不被长窗抹平 */
function specFftFor(sampleRate: number): number {
  const n = Math.pow(2, Math.round(Math.log2(Math.max(1, sampleRate * 0.025))));
  return Math.max(128, Math.min(4096, n));
}

// ---- 缩放视图：可见时间窗 [viewA, viewB]（秒）；viewB <= viewA 表示整段 ----
const viewA = ref(0);
const viewB = ref(0);
/** 当前可见窗口（秒），始终落在 [0, dur] 内 */
function win(): { a: number; b: number } {
  const dur = activeDoc.value?.info.durationSec ?? 0;
  if (viewB.value > viewA.value && viewA.value >= 0 && viewB.value <= dur + 1e-9) {
    return { a: viewA.value, b: viewB.value };
  }
  return { a: 0, b: dur };
}
const zoomed = computed(() => {
  const dur = activeDoc.value?.info.durationSec ?? 0;
  const { a, b } = win();
  return dur > 0 && b - a < dur - 1e-9;
});
function resetView() {
  viewA.value = 0;
  viewB.value = 0;
}

// ---- 画布引用 ----
const waveCanvases: (HTMLCanvasElement | null)[] = [];
const specCanvases: (HTMLCanvasElement | null)[] = [];
const specLegends: (HTMLCanvasElement | null)[] = []; // 频谱行左侧 dB 色标
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
  openMenu.value = '';
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
  return fileDialog({
    mode: 'open',
    title: '打开音频文件',
    filters: [
      { name: '音频文件', extensions: AUDIO_EXTS },
      { name: '所有文件', extensions: ['*'] },
    ],
  });
}

async function triggerOpen() {
  openMenu.value = '';
  try {
    const path = await openFileDialog();
    if (path) await openPath(path);
  } catch (e: any) {
    status.value = `打开对话框失败: ${errText(e)}`;
  }
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
  // 如果文档有修改，弹出确认框
  if (isDirty(docId)) {
    closeConfirmDocId.value = docId;
    showCloseConfirm.value = true;
    return;
  }
  await doCloseDoc(docId);
}

/** 真正执行关闭（跳过脏检查） */
async function doCloseDoc(docId: string) {
  try {
    await eng.docClose(docId);
  } catch {
    /* 引擎侧已不存在也无妨 */
  }
  docs.value = docs.value.filter((d) => d.docId !== docId);
  waveCache.delete(docId);
  dirtyDocs.value.delete(docId);
  for (const k of [...specCache.keys()]) if (k.startsWith(docId + ':')) specCache.delete(k);
  zoomSpecCache.clear();
  if (activeId.value === docId) activeId.value = docs.value.at(-1)?.docId ?? '';
}

/** 关闭确认框 → 保存并关闭 */
async function closeConfirmSave() {
  const docId = closeConfirmDocId.value;
  showCloseConfirm.value = false;
  const d = docs.value.find((x) => x.docId === docId);
  if (d?.path) {
    // 有路径，直接保存
    try {
      await eng.docSave({ docId });
      markClean(docId);
      await doCloseDoc(docId);
    } catch (e: any) {
      status.value = `保存失败: ${errText(e)}`;
    }
  } else {
    // 无路径（录音产物），需另存为
    dlgDocId.value = docId;
    openSaveAs('saveas');
  }
}

/** 关闭确认框 → 不保存直接关闭 */
async function closeConfirmDiscard() {
  const docId = closeConfirmDocId.value;
  showCloseConfirm.value = false;
  await doCloseDoc(docId);
}

/** 关闭确认框 → 取消 */
function closeConfirmCancel() {
  showCloseConfirm.value = false;
  closeConfirmDocId.value = '';
}

async function closeAll() {
  openMenu.value = '';
  for (const d of [...docs.value]) {
    if (isDirty(d.docId)) {
      // 遇到脏文档时弹出确认框，中断批量关闭
      closeConfirmDocId.value = d.docId;
      showCloseConfirm.value = true;
      return;
    }
    await doCloseDoc(d.docId);
  }
}

function selectDoc(id: string) {
  activeId.value = id;
}

async function quit() {
  openMenu.value = '';
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  getCurrentWindow().close();
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

// ---- 界面首选项 ----
const UI_PREFS_KEY = 'sonari.ui';
const uiPrefs = ref<{ showHistory: boolean }>(loadUiPrefs());
function loadUiPrefs(): { showHistory: boolean } {
  try {
    const v = JSON.parse(localStorage.getItem(UI_PREFS_KEY) ?? '{}');
    return { showHistory: typeof v.showHistory === 'boolean' ? v.showHistory : true };
  } catch {
    return { showHistory: true };
  }
}
watch(uiPrefs, (v) => localStorage.setItem(UI_PREFS_KEY, JSON.stringify(v)), { deep: true });

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

let _infernoLut: Uint8ClampedArray | null = null;
/** 256 级 inferno 色表（RGB 平铺） */
function infernoLut(): Uint8ClampedArray {
  if (_infernoLut) return _infernoLut;
  const lut = new Uint8ClampedArray(256 * 3);
  for (let i = 0; i < 256; i++) {
    const m = /rgb\((\d+),(\d+),(\d+)\)/.exec(inferno(i / 255))!;
    lut[i * 3] = +m[1];
    lut[i * 3 + 1] = +m[2];
    lut[i * 3 + 2] = +m[3];
  }
  return (_infernoLut = lut);
}

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
  // 缩放时：窗口外压暗 + 窗口框
  const dur = d.info.durationSec;
  if (zoomed.value && dur > 0) {
    const { a, b } = win();
    const x0 = (a / dur) * W;
    const x1 = Math.max(x0 + 2, (b / dur) * W);
    g.fillStyle = 'rgba(0,0,0,0.55)';
    g.fillRect(0, 0, x0, H);
    g.fillRect(x1, 0, W - x1, H);
    g.strokeStyle = '#e8b34b';
    g.lineWidth = 1;
    g.strokeRect(x0 + 0.5, 0.5, x1 - x0 - 1, H - 1);
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

/** 全文件频谱（单声道），按 docId:ch:fft:dr 缓存；hop 不超过 fft/4，保证帧间重叠 */
async function fullSpec(d: DocOpened, ch: number): Promise<SpecData> {
  const fft = specFftFor(d.info.sampleRate);
  const dr = specDr();
  const key = `${d.docId}:${ch}:${fft}:${dr}`;
  const hit = specCache.get(key);
  if (hit) return hit;
  const hop = Math.max(1, Math.min(specHop.value, fft / 4));
  const r = await eng.spectrogram(d.docId, { fft, hop, maxFrames: 1200, channel: ch, dynRange: dr });
  const v = { frames: r.frames, hop: r.hop, begin: r.begin, fft: r.fft, maxDb: r.maxDb };
  specCache.set(key, v);
  return v;
}

/** 文件级配色参考：全文件各声道频谱的最大电平（dBFS）。
 *  同一电平在时间、缩放、声道之间同色；安静文件也以自身最大值为顶，不会整片发黑。 */
async function specRefDb(d: DocOpened): Promise<number> {
  let ref = -Infinity;
  for (let c = 0; c < d.info.channels; c++) ref = Math.max(ref, (await fullSpec(d, c)).maxDb);
  return Number.isFinite(ref) ? ref : 0;
}

async function fetchSpec(d: DocOpened, ch: number, widthPx: number, refDb: number): Promise<SpecData> {
  if (!zoomed.value) return fullSpec(d, ch);
  // 缩放：只算可见窗口；左右各扩半个 FFT，让帧中心正好铺满窗口
  const docId = d.docId;
  const fft = specFftFor(d.info.sampleRate);
  const dr = specDr();
  const sr = d.info.sampleRate;
  const { a, b } = win();
  const f0 = Math.floor(a * sr);
  const f1 = Math.ceil(b * sr);
  const begin = Math.max(0, f0 - fft / 2);
  const end = Math.min(d.info.frames, f1 + fft / 2);
  const hop = Math.max(1, Math.min(specHop.value, fft / 4, Math.floor((f1 - f0) / Math.max(64, widthPx))));
  const key = `${docId}:${ch}:${fft}:${dr}:${begin}:${end}:${hop}:${refDb}`;
  const hit = zoomSpecCache.get(key);
  if (hit) return hit;
  const r = await eng.spectrogram(docId, { fft, hop, begin, end, maxFrames: 4000, channel: ch, dynRange: dr, refDb });
  const v = { frames: r.frames, hop: r.hop, begin: r.begin, fft: r.fft, maxDb: r.maxDb };
  zoomSpecCache.set(key, v);
  if (zoomSpecCache.size > 16) zoomSpecCache.delete(zoomSpecCache.keys().next().value!);
  return v;
}

/** 电平 → 色表下标：t = 1 + (dB − ref)/dr，线性于 dB；色表取 inferno(t^0.7)；0 = 纯黑（≤ 底 + 2%） */
function specColorIndex(db: number, refDb: number, dr: number): number {
  const t = 1 + (db - refDb) / dr;
  return t <= 0.02 ? 0 : Math.max(1, Math.min(255, Math.round(Math.pow(Math.min(t, 1), 0.7) * 255)));
}

/** 频谱行左侧的 dB 色标：竖向渐变 + 若干 dBFS 刻度（与 drawSpecRow 同一映射） */
function drawSpecLegend(canvas: HTMLCanvasElement | null, refDb: number) {
  if (!canvas) return;
  const w = Math.max(8, canvas.clientWidth);
  const h = Math.max(8, canvas.clientHeight);
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  const g = canvas.getContext('2d')!;
  g.clearRect(0, 0, w, h);
  const dr = specDr();
  const lut = infernoLut();
  const barW = 6;
  const bx = w - barW;
  const pad = 4; // 上下留出半行字高
  const bh = Math.max(1, h - 2 * pad);
  for (let y = 0; y < bh; y++) {
    const db = refDb - (y / Math.max(1, bh - 1)) * dr;
    const ci = specColorIndex(db, refDb, dr);
    g.fillStyle = ci === 0 ? '#000' : `rgb(${lut[ci * 3]},${lut[ci * 3 + 1]},${lut[ci * 3 + 2]})`;
    g.fillRect(bx, pad + y, barW, 1);
  }
  g.strokeStyle = 'rgba(143,161,179,0.5)';
  g.strokeRect(bx - 0.5, pad - 0.5, barW + 1, bh + 1);
  // 刻度：dBFS 取整到步长，步长按可用高度自适应（标签间距 ≥ 11px）
  const step = [10, 20, 30, 40, 60, 80, 120].find((s) => (s / dr) * bh >= 11) ?? dr;
  g.font = '9px Consolas, monospace';
  g.textAlign = 'right';
  g.textBaseline = 'middle';
  g.fillStyle = '#8fa1b3';
  const yOf = (db: number) => pad + ((refDb - db) / dr) * (bh - 1);
  for (let db = Math.floor(refDb / step) * step; db >= refDb - dr; db -= step) {
    const y = yOf(db);
    if (y < pad - 0.5) continue;
    g.fillRect(bx - 3, Math.round(y), 2, 1);
    g.fillText(`${db}`, bx - 4, Math.min(h - 5, Math.max(5, y)));
  }
}

/** spec.frames 为绝对 dBFS；颜色相对 refDb（文件级参考），dynRange 以下为黑 */
function drawSpecRow(canvas: HTMLCanvasElement | null, spec: SpecData, sampleRate: number, refDb: number) {
  if (!canvas) return;
  fitCanvas(canvas);
  const g = canvas.getContext('2d')!;
  const W = canvas.width;
  const H = canvas.height;
  g.fillStyle = '#000';
  g.fillRect(0, 0, W, H);
  const { frames } = spec;
  const nf = frames.length;
  if (!nf) return;
  const nb = frames[0].length;
  const { a, b } = win();
  const span = Math.max(b - a, 1e-9);
  // 逐像素着色（ImageData + 色表），比逐 bin fillRect 快一两个数量级
  // 列 x → 帧：第 f 帧中心 = begin + f*hop + fft/2（帧）；越界取首/末帧铺满边缘
  const colFrame = new Int32Array(W);
  for (let x = 0; x < W; x++) {
    const fr = ((a + ((x + 0.5) / W) * span) * sampleRate - spec.begin - spec.fft / 2) / spec.hop;
    colFrame[x] = Math.max(0, Math.min(nf - 1, Math.round(fr)));
  }
  // 行 y → bin 区间，取区间内最大值（bin 比像素多时不丢峰）
  const rowB0 = new Int32Array(H);
  const rowB1 = new Int32Array(H);
  for (let y = 0; y < H; y++) {
    const yy = H - 1 - y;
    rowB0[y] = Math.min(nb - 1, Math.floor((yy / H) * nb));
    rowB1[y] = Math.max(rowB0[y] + 1, Math.min(nb, Math.floor(((yy + 1) / H) * nb)));
  }
  const lut = infernoLut();
  const dr = specDr();
  const img = g.createImageData(W, H);
  const px = img.data;
  let prevF = -1;
  const colIdx = new Uint8Array(H);
  for (let x = 0; x < W; x++) {
    const f = colFrame[x];
    if (f !== prevF) {
      const db = frames[f];
      for (let y = 0; y < H; y++) {
        let m = -Infinity;
        for (let k = rowB0[y]; k < rowB1[y]; k++) if (db[k] > m) m = db[k];
        colIdx[y] = specColorIndex(m, refDb, dr);
      }
      prevF = f;
    }
    for (let y = 0; y < H; y++) {
      const o = (y * W + x) * 4;
      const ci = colIdx[y];
      if (ci === 0) {
        px[o + 3] = 255; // 黑底
      } else {
        px[o] = lut[ci * 3];
        px[o + 1] = lut[ci * 3 + 1];
        px[o + 2] = lut[ci * 3 + 2];
        px[o + 3] = 255;
      }
    }
  }
  g.putImageData(img, 0, 0);
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
  const { a, b } = win();
  const span = b - a;
  if (span <= 0) return;
  const steps = [
    0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5,
    1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1200, 1800, 3600,
  ];
  const step = steps.find((s) => span / s <= 16) ?? 3600;
  const decimals = step >= 1 ? 0 : Math.ceil(-Math.log10(step) - 1e-9);
  for (let i = Math.ceil(a / step - 1e-9); i * step <= b + 1e-9; i++) {
    const t = i * step;
    const x = Math.round(((t - a) / span) * W);
    g.fillStyle = 'rgba(255,255,255,0.06)';
    g.fillRect(x, 0, 1, H);
    g.fillStyle = '#8fa1b3';
    g.fillRect(x, 0, 1, 6);
    g.font = '10px Consolas, monospace';
    g.textAlign = 'left';
    const label = t.toFixed(decimals);
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

// ---- 「波形和星座图」：rust_mss emd-core 0.3.0 default 指纹 ----
const mssCanvases: (HTMLCanvasElement | null)[] = []; // 每声道一行，与频谱行一致
/** 按文档对象缓存：每次编辑 / 撤销都会换成新的文档对象，缓存自然失效。
 *  键 = 时间窗（秒），长文件按窗口分块计算。 */
const mssCache = new WeakMap<DocOpened, Map<string, MssConstellation>>();
/** 引擎单次指纹计算的时间窗上限（秒），与后端 MAX_SECONDS 对齐 */
const MSS_MAX_WINDOW = 600;
/** 窗口量化粒度（秒）—— 同一窗口内复用缓存，避免平移时反复重算 */
const MSS_WINDOW_STEP = 30;
async function drawMss(gen: number) {
  const d = activeDoc.value;
  if (!d) return;
  for (let ch = 0; ch < d.info.channels; ch++) {
    await drawMssRow(gen, d, ch, mssCanvases[ch] ?? null);
    if (gen !== redrawGen) return;
  }
}

/** 一个声道的星座行（每声道单独指纹，缓存键带声道号） */
async function drawMssRow(gen: number, d: DocOpened, ch: number, c: HTMLCanvasElement | null) {
  if (!c) return;
  // 短文件：整段一次算完（缓存键 = "all"）
  // 长文件：按量化窗口分块算，避免后端 O(全长) 的重采样+STFT 打爆内存
  const dur = d.info.durationSec;
  const whole = dur <= MSS_MAX_WINDOW;
  let wb = 0;
  let we = dur;
  let key = 'all';
  if (!whole) {
    const { a, b } = win();
    wb = Math.max(0, Math.floor(a / MSS_WINDOW_STEP) * MSS_WINDOW_STEP);
    we = Math.min(dur, Math.ceil(b / MSS_WINDOW_STEP) * MSS_WINDOW_STEP);
    if (we - wb > MSS_MAX_WINDOW) we = wb + MSS_MAX_WINDOW;
    key = `${wb.toFixed(1)}-${we.toFixed(1)}`;
  }
  let buckets = mssCache.get(d);
  if (!buckets) {
    buckets = new Map<string, MssConstellation>();
    mssCache.set(d, buckets);
  }
  key = `${ch}:${key}`;
  let r = buckets.get(key);
  if (!r) {
    try {
      r = whole
        ? await eng.mssConstellation(d.docId, undefined, undefined, ch)
        : await eng.mssConstellation(d.docId, wb, we, ch);
    } catch (e: any) {
      status.value = `星座图失败: ${errText(e)}`;
      return;
    }
    buckets.set(key, r);
    if (gen !== redrawGen) return;
  }
  const base = r.beginSec ?? 0; // 指纹的帧 0 对应的绝对秒数
  fitCanvas(c);
  const g = c.getContext('2d')!;
  const W = c.width;
  const H = c.height;
  g.fillStyle = '#0b0e12';
  g.fillRect(0, 0, W, H);
  const { a, b } = win();
  const span = Math.max(b - a, 1e-9);
  const sec = r.hop / r.sampleRate; // 每帧秒数
  const nb = r.binHi - r.binLo;
  // 帧号 t 是相对指纹窗口起点的，需加 base 换算成绝对秒
  const xOf = (t: number) => ((base + t * sec - a) / span) * W;
  const yOf = (f: number) => H - 4 - ((f + 0.5) / nb) * (H - 8);
  const t0 = Math.floor((a - base) / sec) - 21;
  const t1 = Math.ceil((b - base) / sec) + 1;
  // 频率刻度
  g.font = '10px Consolas, monospace';
  g.textAlign = 'right';
  const hzPerBin = r.sampleRate / r.nFft;
  for (const hz of [500, 1000, 1500, 2000]) {
    const y = yOf(hz / hzPerBin - r.binLo);
    g.fillStyle = 'rgba(255,255,255,0.07)';
    g.fillRect(0, y, W, 1);
    g.fillStyle = 'rgba(220,230,240,0.7)';
    g.fillText(`${hz / 1000}k`, W - 4, y - 2);
  }
  // 哈希连线（窗口内太多时不画，避免糊成一片）
  const H4 = r.hashes;
  let inWin = 0;
  for (let i = 0; i < H4.length; i += 4) if (H4[i] >= t0 && H4[i] <= t1) inWin++;
  if (inWin <= 30000) {
    g.strokeStyle = 'rgba(255,170,60,0.18)';
    g.lineWidth = 0.7;
    g.beginPath();
    for (let i = 0; i < H4.length; i += 4) {
      const t = H4[i];
      if (t < t0 || t > t1) continue;
      g.moveTo(xOf(t), yOf(H4[i + 1]));
      g.lineTo(xOf(t + H4[i + 3]), yOf(H4[i + 2]));
    }
    g.stroke();
  }
  // 峰（星）：亮度按 dB
  const P = r.peaks;
  let maxDb = -Infinity;
  for (let i = 2; i < P.length; i += 3) maxDb = Math.max(maxDb, P[i]);
  for (let i = 0; i < P.length; i += 3) {
    const t = P[i];
    if (t < t0 || t > t1) continue;
    const k = Math.max(0, Math.min(1, 1 - (maxDb - P[i + 2]) / 40));
    g.fillStyle = `rgba(120,220,255,${0.3 + 0.7 * k})`;
    g.beginPath();
    g.arc(xOf(t), yOf(P[i + 1]), 1.4 + 1.2 * k, 0, Math.PI * 2);
    g.fill();
  }
  // 说明
  g.textAlign = 'left';
  g.fillStyle = 'rgba(220,230,240,0.85)';
  const capped = r.hashesUncapped > H4.length / 4 ? `（上限前 ${r.hashesUncapped}）` : '';
  const trunc = r.hashesTruncated || r.peaksTruncated ? ' · 载荷已截断' : '';
  const winTxt = base > 0 ? ` · 窗口 ${base.toFixed(0)}–${(base + (r.frames * sec)).toFixed(0)}s` : '';
  g.fillText(
    `${d.info.channels > 1 ? chName(ch, d.info.channels) + ' · ' : ''}rust_mss 0.3.0 default · fast peaks · 峰 ${P.length / 3} · 哈希 ${H4.length / 4}${capped}${trunc}${winTxt}` +
      (inWin > 30000 ? ` · 窗口内 ${inWin} 条连线过多未画，放大查看` : ''),
    6,
    12,
  );
}

let redrawGen = 0;
async function redrawAll() {
  const gen = ++redrawGen;
  await nextTick();
  drawOverview();
  drawRuler();
  const d = activeDoc.value;
  if (!d) return;
  if (view.value === 'wave' || view.value === 'both' || view.value === 'waveconst') {
    let perCh = waveCache.get(d.docId);
    if (zoomed.value) {
      // 缩放：按可见窗口 + 画布像素宽重新取峰值
      const { a, b } = win();
      const sr = d.info.sampleRate;
      const px = Math.max(64, waveCanvases[0]?.clientWidth ?? 1000);
      try {
        const w = await eng.waveform(d.docId, px, {
          begin: Math.floor(a * sr),
          end: Math.min(d.info.frames, Math.ceil(b * sr)),
        });
        if (gen !== redrawGen) return;
        perCh = [];
        for (let c = 0; c < w.channels; c++) perCh.push(w.data.slice(c * w.buckets * 2, (c + 1) * w.buckets * 2));
      } catch (e: any) {
        status.value = `波形失败: ${errText(e)}`;
      }
    }
    if (perCh) {
      for (let c = 0; c < d.info.channels; c++) drawWaveRow(waveCanvases[c], perCh[c] ?? []);
    }
  }
  if (view.value === 'spec' || view.value === 'both') {
    // 先取文件级参考电平（全文件各声道频谱，已缓存则免算），所有行同一配色
    let refDb = 0;
    try {
      refDb = await specRefDb(d);
      if (gen !== redrawGen) return;
    } catch (e: any) {
      status.value = `频谱失败: ${errText(e)}`;
    }
    for (let c = 0; c < d.info.channels; c++) {
      try {
        const spec = await fetchSpec(d, c, specCanvases[c]?.clientWidth ?? 1000, refDb);
        if (gen !== redrawGen) return;
        drawSpecRow(specCanvases[c], spec, d.info.sampleRate, refDb);
        drawSpecLegend(specLegends[c], refDb);
      } catch (e: any) {
        status.value = `频谱失败(ch${c}): ${errText(e)}`;
      }
    }
  }
  if (view.value === 'const') await drawConst();
  if (view.value === 'waveconst') await drawMss(gen);
}

/** 缩放/平移时：总览、标尺立即重画，取数据的行防抖 */
let zoomTimer: number | null = null;
function setZoomWindow(a: number, b: number) {
  const dur = activeDoc.value?.info.durationSec ?? 0;
  if (b - a >= dur - 1e-9) resetView();
  else {
    viewA.value = a;
    viewB.value = b;
  }
  drawOverview();
  drawRuler();
  if (zoomTimer != null) clearTimeout(zoomTimer);
  zoomTimer = window.setTimeout(() => redrawAll(), 60);
}

/** 总览条：滚轮上=放大 / 下=缩小，以鼠标所指时刻为中心 */
function overviewWheel(e: WheelEvent) {
  const d = activeDoc.value;
  if (!d || d.info.durationSec <= 0) return;
  e.preventDefault();
  const dur = d.info.durationSec;
  const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
  const pivot = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width)) * dur;
  const { a, b } = win();
  const dy = e.deltaMode === 1 ? e.deltaY * 33 : e.deltaMode === 2 ? e.deltaY * 400 : e.deltaY;
  const factor = Math.pow(1.25, Math.max(-4, Math.min(4, dy / 100)));
  const minSpan = Math.min(dur, Math.max(0.002, 32 / d.info.sampleRate));
  const span = Math.max(minSpan, Math.min(dur, (b - a) * factor));
  // 鼠标在窗口内：保持它在窗口里的相对位置；在窗口外：以它为中心
  const rel = pivot >= a && pivot <= b && b > a ? (pivot - a) / (b - a) : 0.5;
  const na = Math.max(0, Math.min(dur - span, pivot - rel * span));
  setZoomWindow(na, na + span);
}

/** 总览条：按下/拖动 = 把可见窗口平移到鼠标处 */
let ovPanning = false;
function overviewPan(e: MouseEvent) {
  const d = activeDoc.value;
  if (!d || !zoomed.value) return;
  const dur = d.info.durationSec;
  const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
  const t = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width)) * dur;
  const { a, b } = win();
  const span = b - a;
  const na = Math.max(0, Math.min(dur - span, t - span / 2));
  setZoomWindow(na, na + span);
}
function overviewDown(e: MouseEvent) {
  if (e.button !== 0) return;
  ovPanning = true;
  overviewPan(e);
}
function overviewMove(e: MouseEvent) {
  if (ovPanning) overviewPan(e);
}
function overviewUp() {
  ovPanning = false;
}

watch(activeId, () => resetView());
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

// ---- 声道启用状态（按文档）：仅播放某声道时其余置灰并静音 ----
const chOn = ref<Record<string, boolean[]>>({});
function chEnabled(c: number): boolean {
  const d = activeDoc.value;
  const arr = d ? chOn.value[d.docId] : undefined;
  return !arr || arr.length !== d!.info.channels || arr[c] !== false;
}
/** 需要传给引擎的声道列表；全部启用时返回 undefined */
function playChannels(d: DocOpened): number[] | undefined {
  const arr = chOn.value[d.docId];
  if (!arr || arr.length !== d.info.channels || arr.every(Boolean)) return undefined;
  return arr.flatMap((on, i) => (on ? [i] : []));
}
const allChOn = computed(() => {
  const d = activeDoc.value;
  return !d || !playChannels(d);
});
function chName(c: number, n: number): string {
  if (n === 2) return c === 0 ? '左声道' : '右声道';
  return `声道 #${c}`;
}
async function setChannels(on: boolean[] | null) {
  closeCtx();
  const d = activeDoc.value;
  if (!d) return;
  if (on) chOn.value = { ...chOn.value, [d.docId]: on };
  else {
    const { [d.docId]: _, ...rest } = chOn.value;
    chOn.value = rest;
  }
  const list = playChannels(d);
  status.value = list ? `仅播放：${list.map((c) => chName(c, d.info.channels)).join('、')}` : '播放全部声道';
  // 正在播放：从当前位置按新声道设置重新开始
  if (playing.value) await startPlayback(playheadSec.value, lastPlayEnd);
}
function ctxSoloChannel() {
  const d = activeDoc.value;
  const c = ctx.value?.ch;
  if (!d || c == null) return;
  setChannels(Array.from({ length: d.info.channels }, (_, i) => i === c));
}

let lastPlayEnd = 0;
async function startPlayback(a: number, b: number) {
  const d = activeDoc.value;
  if (!d) return;
  lastPlayEnd = b;
  const chs = playChannels(d);
  const begin = Math.max(0, Math.floor(a * d.info.sampleRate));
  const end = Math.floor(b * d.info.sampleRate);
  try {
    await eng.playbackStop();
    try {
      await eng.playbackStart(d.docId, begin, end, audioPrefs.value.output, chs);
    } catch (e: any) {
      // 首选项里的设备已失效（拔掉/改名）→ 回退系统默认设备重试一次
      if (audioPrefs.value.output && String(errText(e)).includes('未找到设备')) {
        audioPrefs.value.output = '';
        await eng.playbackStart(d.docId, begin, end, '', chs);
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

/** 录音目标：当前是否要插入到已有文档（而非新建） */
const recInsertDocId = ref('');
const recInsertFrame = ref(0);
/** 实时录音电平（用于可视化） */
const recLevel = ref(0);

async function toggleRecord() {
  if (recording.value) {
    // ---- 停止录音 ----
    recording.value = false;
    if (recTimer != null) {
      clearInterval(recTimer);
      recTimer = null;
    }
    try {
      const r = await eng.recordStop();
      busy.value = true;

      if (recInsertDocId.value) {
        // 插入模式：把录音数据粘贴到已有文档的光标位置
        try {
          const updated = await eng.docPaste(recInsertDocId.value, recInsertFrame.value, r.f32leBase64);
          // 原地更新文档
          const i = docs.value.findIndex((d) => d.docId === updated.docId);
          if (i >= 0) docs.value[i] = updated;
          waveCache.delete(updated.docId);
          for (const k of [...specCache.keys()]) if (k.startsWith(updated.docId + ':')) specCache.delete(k);
          zoomSpecCache.clear();
          markDirty(updated.docId);
          await addDoc(updated);
          status.value = `已插入录音 ${r.durationSec.toFixed(1)}s 到光标位置`;
        } catch (e: any) {
          status.value = `插入录音失败: ${errText(e)}`;
        }
      } else {
        // 新建模式：录音产物作为新文档
        const now = new Date();
        const p2 = (n: number) => String(n).padStart(2, '0');
        const name = `录音 ${now.getFullYear()}${p2(now.getMonth() + 1)}${p2(now.getDate())} ${p2(now.getHours())}${p2(now.getMinutes())}${p2(now.getSeconds())}`;
        const doc = await eng.docOpenData({
          name,
          sampleRate: r.sampleRate,
          channels: r.channels,
          f32leBase64: r.f32leBase64,
        });
        markDirty(doc.docId);
        await addDoc(doc);
        status.value = `录音完成：${r.durationSec.toFixed(1)}s · ${r.sampleRate} Hz · ${r.channels}ch`;
      }
    } catch (e: any) {
      status.value = `录音保存失败: ${errText(e)}`;
    } finally {
      busy.value = false;
      recInsertDocId.value = '';
      recInsertFrame.value = 0;
    }
  } else {
    // ---- 开始录音 ----
    try {
      // 决定录音模式：有选中文档且光标在波形中 → 插入模式
      const d = activeDoc.value;
      if (d && cursorSec.value >= 0) {
        recInsertDocId.value = d.docId;
        recInsertFrame.value = Math.floor(cursorSec.value * d.info.sampleRate);
      } else {
        recInsertDocId.value = '';
        recInsertFrame.value = 0;
      }

      await eng.recordStart(audioPrefs.value.input);
      recording.value = true;
      recSec.value = 0;
      recLevel.value = 0;
      recTimer = window.setInterval(async () => {
        try {
          const s = await eng.recordStatus();
          recSec.value = s.seconds;
          // 模拟电平变化用于实时波形可视化
          recLevel.value = Math.random() * 0.3 + 0.1;
        } catch {
          /* 忽略 */
        }
      }, 100);
      status.value = recInsertDocId.value
        ? `录音中…插入到 ${d?.name ?? '文档'} 的 ${cursorSec.value.toFixed(2)}s 位置`
        : '录音中…新建音频文件（再次点击停止）';
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
  const { a, b } = win();
  const t = a + ((clientX - rect.left - 60) / cw) * (b - a);
  return Math.max(a, Math.min(b, t));
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
    cursorSec.value = sel.value.a; // 播放标记跟到选区起点
  }
}
function ovUp(e: MouseEvent) {
  if (e.button !== 0) return;
  const t = xToTime(e.clientX);
  if (dragFrom != null && Math.abs(t - dragFrom) < 0.005) {
    // 单击 → 只设光标
    sel.value = null;
    cursorSec.value = t;
  } else if (sel.value) {
    cursorSec.value = sel.value.a;
  }
  dragFrom = null;
}
function ovLeave() {
  dragFrom = null;
  hoverSec.value = null;
}
function pct(t: number): string {
  const d = activeDoc.value;
  const { a, b } = win();
  if (!d || b - a <= 0) return '0%';
  return `${((t - a) / (b - a)) * 100}%`;
}
const selStyle = computed(() => {
  if (!sel.value || !activeDoc.value) return {};
  const { a, b } = win();
  const span = Math.max(b - a, 1e-9);
  return {
    left: `${((sel.value.a - a) / span) * 100}%`,
    width: `${((sel.value.b - sel.value.a) / span) * 100}%`,
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
  if (fadeDlg.value) {
    if (e.key === 'Escape') fadeDlg.value = null;
    else if (e.key === 'Enter') applyFade();
    else return;
    e.preventDefault();
    return;
  }
  const tag = (e.target as HTMLElement | null)?.tagName ?? '';
  const typing = tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA';
  const mod = e.ctrlKey || e.metaKey; // Windows / Linux 用 Ctrl，macOS 用 ⌘
  if (mod && e.shiftKey && e.key.toLowerCase() === 's') {
    e.preventDefault();
    openSaveAs('saveas');
  } else if (mod && e.key.toLowerCase() === 's') {
    e.preventDefault();
    ctxSave();
  } else if (mod && e.key.toLowerCase() === 'a' && !typing) {
    e.preventDefault();
    ctxSelectAll();
  } else if (mod && !typing && (e.key.toLowerCase() === 'y' || (e.shiftKey && e.key.toLowerCase() === 'z'))) {
    e.preventDefault();
    editRedo();
  } else if (mod && !typing && e.key.toLowerCase() === 'z') {
    e.preventDefault();
    editUndo();
  } else if (mod && !typing && e.key.toLowerCase() === 'x') {
    e.preventDefault();
    lastClipKey = Date.now();
    editCut();
  } else if (mod && !typing && e.key.toLowerCase() === 'c') {
    e.preventDefault();
    lastClipKey = Date.now();
    editCopy();
  } else if (mod && !typing && e.key.toLowerCase() === 'v') {
    e.preventDefault();
    lastClipKey = Date.now();
    editPaste();
  } else if (mod && e.key.toLowerCase() === 'o') {
    e.preventDefault();
    triggerOpen();
  } else if (mod && e.key === '1') {
    e.preventDefault();
    setView('wave');
  } else if (mod && e.key === '2') {
    e.preventDefault();
    setView('spec');
  } else if (mod && e.key === '3') {
    e.preventDefault();
    setView('both');
  } else if (mod && e.key === '4') {
    e.preventDefault();
    setView('const');
  } else if (mod && e.key === '5') {
    e.preventDefault();
    setView('waveconst');
  } else if (mod && e.key.toLowerCase() === 'w' && activeId.value) {
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

/** macOS：原生「编辑」菜单可能先截走 ⌘C/⌘X/⌘V，页面收不到 keydown，只收到 copy/cut/paste
 *  事件 —— 这里兜底。keydown 已经处理过的（0.5 s 内）不再重复执行。 */
let lastClipKey = 0;
function onClipEvent(e: Event) {
  const el = document.activeElement as HTMLElement | null;
  const tag = el?.tagName ?? '';
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || el?.isContentEditable) return;
  e.preventDefault();
  if (Date.now() - lastClipKey < 500) return;
  if (e.type === 'copy') editCopy();
  else if (e.type === 'cut') editCut();
  else if (e.type === 'paste') editPaste();
}
onMounted(() => {
  for (const t of ['copy', 'cut', 'paste']) document.addEventListener(t, onClipEvent);
});
onUnmounted(() => {
  for (const t of ['copy', 'cut', 'paste']) document.removeEventListener(t, onClipEvent);
});
onUnmounted(() => window.removeEventListener('keydown', onKey));

// 频谱 hop / 动态范围变化后清缓存重画（缓存键也含 dr，这里顺带释放旧数据）
watch([specHop, dynRange], () => {
  specCache.clear();
  zoomSpecCache.clear();
  if (view.value === 'spec' || view.value === 'both') redrawAll();
});

// ================= 右键菜单（ocenaudio 式：播放 / 查看信息） =================
/** 右键菜单：diagram = 图表区（编辑 / 播放 / 声道），file = 文件列表（信息 / 转换 / 保存 / 重命名） */
const ctx = ref<{ x: number; y: number; docId: string; ch?: number; kind: 'diagram' | 'file' } | null>(null);
const ctxEl = ref<HTMLElement | null>(null);
const ctxDoc = computed(() => docs.value.find((d) => d.docId === ctx.value?.docId) ?? null);

/** 全局右键：只屏蔽 WebView 自带的浏览器菜单（返回/刷新/另存为…），不弹我们的菜单 */
function onAppCtx(e: MouseEvent) {
  const tag = (e.target as HTMLElement | null)?.tagName ?? '';
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
  e.preventDefault();
}

/** 图表区域右键：弹出菜单；落在某声道行上则带上声道号 */
function onDiagramCtx(e: MouseEvent) {
  if (!activeDoc.value) return;
  let ch: number | undefined;
  for (const el of document.elementsFromPoint(e.clientX, e.clientY)) {
    const v = (el as HTMLElement).dataset?.ch;
    if (v != null) {
      ch = Number(v);
      break;
    }
  }
  onCtx(e);
  if (ctx.value && ch != null) ctx.value.ch = ch;
}

/** 文件列表右键：文件级菜单 */
function onFileCtx(e: MouseEvent, docId: string) {
  onCtx(e, docId, 'file');
}

function onCtx(e: MouseEvent, docId?: string, kind: 'diagram' | 'file' = 'diagram') {
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
    kind,
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
    zoomSpecCache.clear();
    resetView();
    sel.value = null;
    await addDoc(updated);
    markDirty(updated.docId);
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

// ---- 「查看丢包率」 ----
const plDlg = ref<{ name: string; scope: string; r: PacketLoss } | null>(null);
async function ctxPacketLoss() {
  const d = ctxDoc.value;
  closeCtx();
  if (!d) return;
  try {
    plDlg.value = { name: d.name, scope: '整段 · 所有声道', r: await eng.docPacketLoss(d.docId) };
  } catch (e: any) {
    status.value = `丢包率计算失败: ${errText(e)}`;
  }
}
/** 图表区：选区的丢包率；右键落在某声道行上则只算该声道 */
async function selPacketLoss() {
  const d = activeDoc.value;
  const r = d && selFrames(d);
  const c = ctx.value?.ch;
  closeCtx();
  if (!d || !r) return;
  const one = c != null && d.info.channels > 1;
  try {
    const res = await eng.docPacketLoss(d.docId, { ...r, ...(one ? { channel: c } : {}) });
    const scope = `选区 ${res.beginSec.toFixed(3)}–${res.endSec.toFixed(3)}s · ${one ? chName(c!, d.info.channels) : '所有声道'}`;
    plDlg.value = { name: d.name, scope, r: res };
  } catch (e: any) {
    status.value = `丢包率计算失败: ${errText(e)}`;
  }
}

// ---- 抽取声道 → 新文件 ----
async function extractChannel(docId: string, ch: number, range?: { begin: number; end: number }) {
  closeCtx();
  const src = docs.value.find((x) => x.docId === docId);
  if (!src) return;
  try {
    const doc = await eng.docExtractChannel(docId, ch, range);
    const dup = docs.value.find((x) => x.name === doc.name && x.info.frames === doc.info.frames);
    if (dup) {
      // 同名同长度的已经抽过：丢掉新建的这份，直接切过去
      await eng.docClose(doc.docId).catch(() => {});
      activeId.value = dup.docId;
      status.value = `已存在 ${dup.name}`;
      return;
    }
    await addDoc(doc);
    markDirty(doc.docId); // 内存文档，关闭时提示保存
    status.value = `已抽取${chName(ch, src.info.channels)} → ${doc.name}（未保存）`;
  } catch (e: any) {
    status.value = `抽取声道失败: ${errText(e)}`;
  }
}
function selExtractChannel() {
  const d = activeDoc.value;
  const c = ctx.value?.ch;
  const r = d && selFrames(d);
  if (!d || !r || c == null) return closeCtx();
  extractChannel(d.docId, c, r);
}

function ctxCloseFile() {
  const id = ctx.value?.docId;
  closeCtx();
  if (id) closeDoc(id);
}

function ctxDeselect() {
  closeCtx();
  sel.value = null;
}

// ================= 编辑：剪切 / 复制 / 粘贴 / 静音 + 撤销 / 重做 =================
const clip = ref<ClipInfo | null>(null);
onMounted(async () => {
  try {
    clip.value = await eng.clipboardInfo();
  } catch {
    /* 剪贴板查询失败不影响其它功能 */
  }
});
const hist = computed(() => activeDoc.value?.history ?? { undo: [] as string[], redo: [] as string[] });
const canUndo = computed(() => hist.value.undo.length > 0);
const canRedo = computed(() => hist.value.redo.length > 0);
const hasSel = computed(() => !!sel.value && sel.value.b - sel.value.a > 1e-6);

/** 选区（秒）→ 帧区间 */
function selFrames(d: DocOpened): { begin: number; end: number } | null {
  if (!sel.value) return null;
  const sr = d.info.sampleRate;
  const begin = Math.max(0, Math.round(sel.value.a * sr));
  const end = Math.min(d.info.frames, Math.round(sel.value.b * sr));
  return end > begin ? { begin, end } : null;
}

let editing = false;
/** 执行一次改动文档的编辑：替换文档 + 刷新缓存/画面 + 标脏 */
async function runEdit(
  op: (d: DocOpened) => Promise<DocOpened>,
  done: (before: DocOpened, after: DocOpened) => string,
) {
  closeCtx();
  openMenu.value = '';
  const d = activeDoc.value;
  if (!d || editing || recording.value) return;
  editing = true;
  busy.value = true;
  try {
    await stopPlayback();
    const updated = await op(d);
    if (!updated.path) updated.path = d.path;
    const i = docs.value.findIndex((x) => x.docId === updated.docId);
    if (i >= 0) docs.value[i] = updated;
    waveCache.delete(updated.docId);
    for (const k of [...specCache.keys()]) if (k.startsWith(updated.docId + ':')) specCache.delete(k);
    zoomSpecCache.clear();
    markDirty(updated.docId);
    // 时长变了：选区 / 光标 / 缩放窗口钳回文档内
    const dur = updated.info.durationSec;
    if (sel.value && sel.value.a >= dur) sel.value = null;
    else if (sel.value) sel.value = { a: sel.value.a, b: Math.min(sel.value.b, dur) };
    cursorSec.value = Math.min(cursorSec.value, dur);
    if (viewB.value > dur) resetView();
    status.value = done(d, updated);
    await addDoc(updated);
  } catch (e: any) {
    status.value = `操作失败: ${errText(e)}`;
  } finally {
    busy.value = false;
    editing = false;
  }
}

function editCut() {
  const d0 = activeDoc.value;
  const r = d0 && selFrames(d0);
  if (!r) return;
  const a = sel.value!.a;
  runEdit(
    (d) => eng.docCut(d.docId, r.begin, r.end),
    (b, u) => {
      sel.value = null;
      cursorSec.value = a;
      eng.clipboardInfo().then((c) => (clip.value = c)).catch(() => {});
      return `已剪切 ${(b.info.durationSec - u.info.durationSec).toFixed(3)}s`;
    },
  );
}

async function editCopy() {
  closeCtx();
  openMenu.value = '';
  const d = activeDoc.value;
  const r = d && selFrames(d);
  if (!d || !r) return;
  try {
    clip.value = await eng.docCopy(d.docId, r.begin, r.end);
    status.value = `已复制 ${clip.value.durationSec.toFixed(3)}s`;
  } catch (e: any) {
    status.value = `复制失败: ${errText(e)}`;
  }
}

/** 粘贴：插入到播放标记处（拖选后播放标记在选区起点），不替换选区。
 *  粘贴后选中粘进来的部分。 */
function editPaste() {
  const d0 = activeDoc.value;
  if (!d0 || !clip.value) return;
  const at = Math.round(Math.min(cursorSec.value, d0.info.durationSec) * d0.info.sampleRate);
  runEdit(
    (d) => eng.docPasteClip(d.docId, { frame: at }),
    (b, u) => {
      const sr = u.info.sampleRate;
      const added = u.info.durationSec - b.info.durationSec;
      sel.value = { a: at / sr, b: Math.min(u.info.durationSec, at / sr + added) };
      cursorSec.value = at / sr;
      return `已粘贴 ${added.toFixed(3)}s`;
    },
  );
}

function editSilence() {
  const d0 = activeDoc.value;
  const r = d0 && selFrames(d0);
  if (!d0 || !r) return;
  const secs = (r.end - r.begin) / d0.info.sampleRate;
  runEdit(
    (d) => eng.docSilence(d.docId, r.begin, r.end),
    () => `已静音 ${secs.toFixed(3)}s`,
  );
}

// ================= 淡入 / 淡出：增益曲线编辑器 =================
type CurvePt = { x: number; y: number };
/** 预设（按淡入定义：x = 选区内位置 0..1，y = 增益 0..1）；淡出取镜像 */
const FADE_PRESETS: { key: string; label: string; pts: CurvePt[] }[] = [
  { key: 'linear', label: '线性', pts: [{ x: 0, y: 0 }, { x: 1, y: 1 }] },
  {
    key: 'fast',
    label: '快速（对数）',
    pts: [{ x: 0, y: 0 }, { x: 0.15, y: 0.55 }, { x: 0.4, y: 0.85 }, { x: 1, y: 1 }],
  },
  {
    key: 'slow',
    label: '缓慢（指数）',
    pts: [{ x: 0, y: 0 }, { x: 0.6, y: 0.15 }, { x: 0.85, y: 0.45 }, { x: 1, y: 1 }],
  },
  {
    key: 's',
    label: 'S 形',
    pts: [{ x: 0, y: 0 }, { x: 0.25, y: 0.08 }, { x: 0.5, y: 0.5 }, { x: 0.75, y: 0.92 }, { x: 1, y: 1 }],
  },
  {
    key: 'power',
    label: '等功率',
    pts: [0, 0.25, 0.5, 0.75, 1].map((x) => ({ x, y: Math.sin((Math.PI / 2) * x) })),
  },
];
function presetPts(dir: 'in' | 'out', key: string): CurvePt[] {
  const p = (FADE_PRESETS.find((q) => q.key === key) ?? FADE_PRESETS[0]).pts.map((q) => ({ ...q }));
  return dir === 'in' ? p : p.map((q) => ({ x: 1 - q.x, y: q.y })).reverse();
}

/** 上次用过的曲线（按方向）存在 localStorage */
const FADE_KEY = 'sonari.fade';
function loadFadeCurves(): Partial<Record<'in' | 'out', CurvePt[]>> {
  try {
    const v = JSON.parse(localStorage.getItem(FADE_KEY) ?? '{}');
    const ok = (a: unknown): a is CurvePt[] =>
      Array.isArray(a) && a.length >= 2 && a.every((p) => typeof p?.x === 'number' && typeof p?.y === 'number');
    return { in: ok(v.in) ? v.in : undefined, out: ok(v.out) ? v.out : undefined };
  } catch {
    return {};
  }
}

/** 单调三次 Hermite 插值（Fritsch–Carlson）：过所有控制点、不过冲 */
function monotoneCurve(pts: CurvePt[]): (x: number) => number {
  const n = pts.length;
  const xs = pts.map((p) => p.x);
  const ys = pts.map((p) => p.y);
  const d: number[] = [];
  for (let i = 0; i < n - 1; i++) d.push((ys[i + 1] - ys[i]) / Math.max(xs[i + 1] - xs[i], 1e-9));
  const m: number[] = new Array(n);
  m[0] = d[0];
  m[n - 1] = d[n - 2];
  for (let i = 1; i < n - 1; i++) m[i] = d[i - 1] * d[i] <= 0 ? 0 : (d[i - 1] + d[i]) / 2;
  for (let i = 0; i < n - 1; i++) {
    if (d[i] === 0) {
      m[i] = 0;
      m[i + 1] = 0;
      continue;
    }
    const a = m[i] / d[i];
    const b = m[i + 1] / d[i];
    const s = a * a + b * b;
    if (s > 9) {
      const t = 3 / Math.sqrt(s);
      m[i] = t * a * d[i];
      m[i + 1] = t * b * d[i];
    }
  }
  return (x: number) => {
    if (x <= xs[0]) return ys[0];
    if (x >= xs[n - 1]) return ys[n - 1];
    let i = 0;
    while (i < n - 2 && x > xs[i + 1]) i++;
    const h = xs[i + 1] - xs[i];
    const t = (x - xs[i]) / h;
    const t2 = t * t;
    const t3 = t2 * t;
    const y =
      (2 * t3 - 3 * t2 + 1) * ys[i] +
      (t3 - 2 * t2 + t) * h * m[i] +
      (-2 * t3 + 3 * t2) * ys[i + 1] +
      (t3 - t2) * h * m[i + 1];
    return Math.max(0, Math.min(1, y));
  };
}
function sampleCurve(pts: CurvePt[], n: number): number[] {
  const f = monotoneCurve(pts);
  return Array.from({ length: n }, (_, i) => f(i / (n - 1)));
}

const fadeDlg = ref<{
  dir: 'in' | 'out';
  pts: CurvePt[];
  begin: number;
  end: number;
  secs: number;
  env: number[]; // 选区波形包络（0..1）
} | null>(null);
const fadeCanvas = ref<HTMLCanvasElement | null>(null);
const fadeHover = ref<CurvePt | null>(null);
let fadeDrag = -1;
const FADE_PAD = 14;

/** 菜单「淡入… / 淡出…」：打开曲线编辑器 */
async function editFade(dir: 'in' | 'out') {
  closeCtx();
  openMenu.value = '';
  const d = activeDoc.value;
  const r = d && selFrames(d);
  if (!d || !r) return;
  fadeDlg.value = {
    dir,
    pts: (loadFadeCurves()[dir] ?? presetPts(dir, 'linear')).map((p) => ({ ...p })),
    begin: r.begin,
    end: r.end,
    secs: (r.end - r.begin) / d.info.sampleRate,
    env: [],
  };
  fadeHover.value = null;
  await nextTick();
  drawFade();
  try {
    const w = await eng.waveform(d.docId, 520, r);
    const env: number[] = [];
    for (let b = 0; b < w.buckets; b++) {
      let a = 0;
      for (let c = 0; c < w.channels; c++) {
        const o = (c * w.buckets + b) * 2;
        a = Math.max(a, Math.abs(w.data[o]), Math.abs(w.data[o + 1]));
      }
      env.push(Math.min(1, a));
    }
    if (fadeDlg.value) {
      fadeDlg.value.env = env;
      drawFade();
    }
  } catch {
    /* 没有包络也能编辑曲线 */
  }
}

function setFadePreset(key: string) {
  const f = fadeDlg.value;
  if (!f) return;
  f.pts = presetPts(f.dir, key);
  drawFade();
}

function applyFade() {
  const f = fadeDlg.value;
  if (!f) return;
  try {
    localStorage.setItem(FADE_KEY, JSON.stringify({ ...loadFadeCurves(), [f.dir]: f.pts }));
  } catch {
    /* 存不下也不影响应用 */
  }
  fadeDlg.value = null;
  const curve = sampleCurve(f.pts, 1024);
  runEdit(
    (d) => eng.docFade(d.docId, f.begin, f.end, f.dir, curve),
    () => `已${f.dir === 'in' ? '淡入' : '淡出'} ${f.secs.toFixed(3)}s`,
  );
}

/** 画布坐标 ↔ 曲线坐标 */
function fadeToPx(c: HTMLCanvasElement, p: CurvePt) {
  return {
    x: FADE_PAD + p.x * (c.width - 2 * FADE_PAD),
    y: FADE_PAD + (1 - p.y) * (c.height - 2 * FADE_PAD),
  };
}
function fadeFromEvent(e: MouseEvent): CurvePt | null {
  const c = fadeCanvas.value;
  if (!c) return null;
  const r = c.getBoundingClientRect();
  const px = ((e.clientX - r.left) * c.width) / r.width;
  const py = ((e.clientY - r.top) * c.height) / r.height;
  return {
    x: Math.max(0, Math.min(1, (px - FADE_PAD) / (c.width - 2 * FADE_PAD))),
    y: Math.max(0, Math.min(1, 1 - (py - FADE_PAD) / (c.height - 2 * FADE_PAD))),
  };
}
/** 鼠标附近（9px 内）的控制点下标，没有则 -1 */
function fadeHit(e: MouseEvent): number {
  const c = fadeCanvas.value;
  const f = fadeDlg.value;
  if (!c || !f) return -1;
  const r = c.getBoundingClientRect();
  const px = ((e.clientX - r.left) * c.width) / r.width;
  const py = ((e.clientY - r.top) * c.height) / r.height;
  let best = -1;
  let bd = 9 * 9;
  f.pts.forEach((p, i) => {
    const q = fadeToPx(c, p);
    const dd = (q.x - px) ** 2 + (q.y - py) ** 2;
    if (dd <= bd) {
      bd = dd;
      best = i;
    }
  });
  return best;
}
function fadeMoveTo(i: number, p: CurvePt) {
  const f = fadeDlg.value!;
  const pts = f.pts;
  const last = pts.length - 1;
  // 端点只能上下动；中间点夹在左右邻点之间
  const x = i === 0 ? 0 : i === last ? 1 : Math.max(pts[i - 1].x + 0.01, Math.min(pts[i + 1].x - 0.01, p.x));
  pts[i] = { x, y: p.y };
}
function fadeDown(e: MouseEvent) {
  if (e.button !== 0) return;
  const f = fadeDlg.value;
  const p = fadeFromEvent(e);
  if (!f || !p) return;
  let i = fadeHit(e);
  if (i < 0) {
    // 空白处：在该 x 处插一个新点并开始拖动
    if (p.x <= 0.01 || p.x >= 0.99) return;
    i = f.pts.findIndex((q) => q.x > p.x);
    f.pts.splice(i, 0, { ...p });
  }
  fadeDrag = i;
  fadeMoveTo(i, p);
  fadeHover.value = { ...f.pts[i] };
  drawFade();
  window.addEventListener('mousemove', fadeWinMove);
  window.addEventListener('mouseup', fadeWinUp, { once: true });
}
function fadeWinMove(e: MouseEvent) {
  const p = fadeFromEvent(e);
  if (fadeDrag < 0 || !p || !fadeDlg.value) return;
  fadeMoveTo(fadeDrag, p);
  fadeHover.value = { ...fadeDlg.value.pts[fadeDrag] };
  drawFade();
}
function fadeWinUp() {
  fadeDrag = -1;
  window.removeEventListener('mousemove', fadeWinMove);
  drawFade();
}
function fadeHoverMove(e: MouseEvent) {
  if (fadeDrag >= 0) return;
  const f = fadeDlg.value;
  if (!f) return;
  const i = fadeHit(e);
  const p = fadeFromEvent(e);
  fadeHover.value = i >= 0 ? { ...f.pts[i] } : p ? { x: p.x, y: monotoneCurve(f.pts)(p.x) } : null;
  if (fadeCanvas.value) fadeCanvas.value.style.cursor = i >= 0 ? 'grab' : 'crosshair';
}
/** 双击 / 右键：删除中间控制点 */
function fadeDelete(e: MouseEvent) {
  const f = fadeDlg.value;
  const i = fadeHit(e);
  if (!f || i <= 0 || i >= f.pts.length - 1) return;
  f.pts.splice(i, 1);
  drawFade();
}
const fadeReadout = computed(() => {
  const f = fadeDlg.value;
  const h = fadeHover.value;
  if (!f || !h) return '';
  const db = h.y <= 0 ? '−∞' : (20 * Math.log10(h.y)).toFixed(1);
  return `${(h.x * f.secs).toFixed(3)}s · 增益 ${(h.y * 100).toFixed(0)}%（${db} dB）`;
});

function drawFade() {
  const c = fadeCanvas.value;
  const f = fadeDlg.value;
  if (!c || !f) return;
  const g = c.getContext('2d')!;
  const W = c.width;
  const H = c.height;
  const iw = W - 2 * FADE_PAD;
  const ih = H - 2 * FADE_PAD;
  g.fillStyle = '#0d1117';
  g.fillRect(0, 0, W, H);
  // 网格（25% 一格）
  g.strokeStyle = '#1d2630';
  g.lineWidth = 1;
  for (let k = 0; k <= 4; k++) {
    const x = Math.round(FADE_PAD + (k / 4) * iw) + 0.5;
    const y = Math.round(FADE_PAD + (k / 4) * ih) + 0.5;
    g.beginPath();
    g.moveTo(x, FADE_PAD);
    g.lineTo(x, H - FADE_PAD);
    g.moveTo(FADE_PAD, y);
    g.lineTo(W - FADE_PAD, y);
    g.stroke();
  }
  // 波形预览：灰 = 原始，青 = 应用曲线后
  const curve = monotoneCurve(f.pts);
  const mid = FADE_PAD + ih / 2;
  const n = f.env.length;
  if (n) {
    const bw = iw / n;
    for (let i = 0; i < n; i++) {
      const x = FADE_PAD + i * bw;
      const a = f.env[i] * (ih / 2);
      g.fillStyle = 'rgba(143,161,179,0.28)';
      g.fillRect(x, mid - a, Math.max(bw, 1), Math.max(2 * a, 1));
      const ga = a * curve((i + 0.5) / n);
      g.fillStyle = 'rgba(31,214,164,0.75)';
      g.fillRect(x, mid - ga, Math.max(bw, 1), Math.max(2 * ga, 1));
    }
  }
  // 曲线
  g.strokeStyle = '#e8b34b';
  g.lineWidth = 2;
  g.beginPath();
  for (let k = 0; k <= iw; k++) {
    const x = k / iw;
    const p = fadeToPx(c, { x, y: curve(x) });
    if (k === 0) g.moveTo(p.x, p.y);
    else g.lineTo(p.x, p.y);
  }
  g.stroke();
  // 控制点
  f.pts.forEach((pt, i) => {
    const p = fadeToPx(c, pt);
    g.beginPath();
    g.arc(p.x, p.y, 5, 0, Math.PI * 2);
    g.fillStyle = i === fadeDrag ? '#ffffff' : '#e8b34b';
    g.fill();
    g.strokeStyle = '#0d1117';
    g.lineWidth = 1.5;
    g.stroke();
  });
  // 轴标签
  g.font = '10px Consolas, monospace';
  g.fillStyle = '#6b7a8a';
  g.textAlign = 'left';
  g.fillText('100%', FADE_PAD + 3, FADE_PAD + 10);
  g.fillText('0', FADE_PAD + 3, H - FADE_PAD - 3);
  g.textAlign = 'right';
  g.fillText(`${f.secs.toFixed(3)}s`, W - FADE_PAD - 3, H - FADE_PAD - 3);
}

function editUndo(steps = 1) {
  if (!canUndo.value) return;
  const labels = hist.value.undo.slice(-steps).reverse();
  runEdit((d) => eng.docUndo(d.docId, steps), () => {
    sel.value = null; // 撤销/重做后旧选区已不对应原内容
    return `已撤销：${labels.join('、')}`;
  });
}
function editRedo(steps = 1) {
  if (!canRedo.value) return;
  const labels = hist.value.redo.slice(0, steps);
  runEdit((d) => eng.docRedo(d.docId, steps), () => {
    sel.value = null; // 撤销/重做后旧选区已不对应原内容
    return `已重做：${labels.join('、')}`;
  });
}

/** 历史面板：点第 i 项 = 回到「做完该项之后」的状态（0 = 原始） */
const histItems = computed(() => [
  { label: '原始', done: true },
  ...hist.value.undo.map((l) => ({ label: l, done: true })),
  ...hist.value.redo.map((l) => ({ label: l, done: false })),
]);
function gotoHistory(i: number) {
  const cur = hist.value.undo.length;
  if (i < cur) editUndo(cur - i);
  else if (i > cur) editRedo(i - cur);
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
    markDirty(d.docId);
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
  sampleRate: 44100,
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
  openMenu.value = '';
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
    sampleRate: d.info.sampleRate,
    bitDepth: d.info.bitsPerSample ?? 16,
    bitrateKbps: 192,
    selectionOnly: isExport && !!sel.value,
  };
  showSave.value = true;
}

/** 绝对路径校验：Windows 盘符 / UNC，或 POSIX 根路径 */
const savePathOk = computed(() => {
  const p = saveForm.value.path.trim();
  if (!p) return false;
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

/** 导出对话框可选采样率列表 */
const saveSampleRates = computed(() => {
  const src = dlgDoc.value?.info.sampleRate ?? 44100;
  const common = [8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000];
  const set = new Set(common);
  set.add(src); // 确保源采样率总在列表中
  return [...set].sort((a, b) => a - b);
});

/** 根据设置自动估算码率 */
const estimatedBitrate = computed(() => {
  const f = saveForm.value;
  const ch = dlgDoc.value?.info.channels ?? 1;
  if (saveIsLossy.value) {
    // 有损格式：用选择的码率
    return `${f.bitrateKbps} kbps（有损压缩）`;
  }
  // 无损 PCM：采样率 × 声道 × 位深
  const bps = f.sampleRate * ch * f.bitDepth;
  if (bps >= 1_000_000) {
    return `${(bps / 1000).toFixed(0)} kbps（${(bps / 1_000_000).toFixed(2)} Mbps）`;
  }
  return `${(bps / 1000).toFixed(0)} kbps`;
});

async function pickSavePath() {
  const f = saveForm.value;
  const sel2 = await fileDialog({
    mode: 'save',
    title: saveMode.value === 'export' ? '导出音频' : '另存为',
    defaultPath: f.path,
    filters: [{ name: saveFmt.value?.label ?? '音频', extensions: [f.format] }],
  });
  if (typeof sel2 === 'string' && sel2) {
    f.path = sel2;
    const e = extOf(sel2);
    if (e && (caps.value?.formats ?? []).some((x) => x.ext === e)) f.format = e;
  }
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
  status.value = '导出中…';
  try {
    const r = await eng.docSave({
      docId: d.docId,
      path: f.path.trim(),
      format: f.format,
      sampleRate: f.sampleRate,
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
    markClean(d.docId);
    status.value =
      `已导出 ${r.path} · ${fmtBytes(r.bytes)} · ${r.format.toUpperCase()}` +
      (r.viaFfmpeg ? ' · ffmpeg' : '') +
      (r.region ? ` · 选区 ${fmtTime((end - begin) / d.info.sampleRate)}` : '');
  } catch (e) {
    status.value = `导出失败: ${errText(e)}`;
  } finally {
    busy.value = false;
  }
}

/** 「保存」：直接覆盖原文件；内存文档（录音）则退化为「另存为…」 */
async function ctxSave() {
  const d = ctxDoc.value ?? activeDoc.value;
  openMenu.value = '';
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
    markClean(d.docId);
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
  <div class="app" @click="onAppClick" @contextmenu="onAppCtx">
    <!-- 菜单栏 -->
    <div class="menubar">
      <div class="menu" :class="{ open: openMenu === 'file' }" @click.stop="toggleMenu('file')">文件(F)
        <div class="dropdown" v-if="openMenu === 'file'" @click.stop="onDropdownClick">
          <div class="mi" @click="triggerOpen"><span>打开(O)…</span><span class="sc">{{ MOD }}O</span></div>
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
            <span>保存(S)</span><span class="sc">{{ MOD }}S</span>
          </div>
          <div class="mi" :class="{ dis: !activeDoc }" @click="openSaveAs('saveas')">
            <span>另存为(A)…</span><span class="sc">{{ MOD }}Shift+S</span>
          </div>
          <div class="mi" :class="{ dis: !activeDoc }" @click="openSaveAs('export')">
            <span>导出(E)…</span>
          </div>
          <div class="sep"></div>
          <div class="mi" :class="{ dis: !activeDoc }" @click="activeDoc && closeDoc(activeDoc.docId)">
            <span>关闭当前(C)</span><span class="sc">{{ MOD }}W</span>
          </div>
          <div class="mi" :class="{ dis: !docs.length }" @click="closeAll"><span>全部关闭</span></div>
          <div class="sep"></div>
          <div class="mi" @click="quit"><span>退出(X)</span></div>
        </div>
      </div>
      <div class="menu" :class="{ open: openMenu === 'edit' }" @click.stop="toggleMenu('edit')">编辑(E)
        <div class="dropdown" v-if="openMenu === 'edit'" @click.stop="onDropdownClick">
          <div class="mi" :class="{ dis: !canUndo }" @click="editUndo()"><span>撤销</span><span class="sc">{{ MOD }}Z</span></div>
          <div class="mi" :class="{ dis: !canRedo }" @click="editRedo()"><span>重做</span><span class="sc">{{ MOD }}Y</span></div>
          <div class="sep"></div>
          <div class="mi" :class="{ dis: !hasSel }" @click="editCut"><span>剪切</span><span class="sc">{{ MOD }}X</span></div>
          <div class="mi" :class="{ dis: !hasSel }" @click="editCopy"><span>复制</span><span class="sc">{{ MOD }}C</span></div>
          <div class="mi" :class="{ dis: !clip || !activeDoc }" @click="editPaste"><span>粘贴</span><span class="sc">{{ MOD }}V</span></div>
          <div class="mi" :class="{ dis: !hasSel }" @click="editSilence"><span>静音</span></div>
          <div class="mi" :class="{ dis: !hasSel }" @click="editFade('in')"><span>淡入…</span></div>
          <div class="mi" :class="{ dis: !hasSel }" @click="editFade('out')"><span>淡出…</span></div>
          <div class="sep"></div>
          <div class="mi" @click="openPrefs"><span>首选项(P)…</span></div>
        </div>
      </div>
      <div class="menu" :class="{ open: openMenu === 'view' }" @click.stop="toggleMenu('view')">视图(V)
        <div class="dropdown" v-if="openMenu === 'view'" @click.stop="onDropdownClick">
          <div class="mi" @click="setView('wave')">
            <span class="chk">{{ view === 'wave' ? '✓' : '' }}</span><span>波形视图(W)</span><span class="sc">{{ MOD }}1</span>
          </div>
          <div class="mi" @click="setView('spec')">
            <span class="chk">{{ view === 'spec' ? '✓' : '' }}</span><span>频谱视图(S)</span><span class="sc">{{ MOD }}2</span>
          </div>
          <div class="mi" @click="setView('both')">
            <span class="chk">{{ view === 'both' ? '✓' : '' }}</span><span>波形和频谱视图(B)</span><span class="sc">{{ MOD }}3</span>
          </div>
          <div class="sep"></div>
          <div class="mi" @click="setView('const')">
            <span class="chk">{{ view === 'const' ? '✓' : '' }}</span><span>星座图</span><span class="sc">{{ MOD }}4</span>
          </div>
          <div class="mi" @click="setView('waveconst')" title="rust_mss emd-core 0.3.0 default：fast peaks + 哈希上限">
            <span class="chk">{{ view === 'waveconst' ? '✓' : '' }}</span><span>波形和星座图</span><span class="sc">{{ MOD }}5</span>
          </div>
        </div>
      </div>
      <span class="appname">Sonari</span>
    </div>

    <!-- 操作栏：图标 + LCD -->
    <div class="toolbar">
      <div class="tb-left">
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
      </div>

      <div class="lcd">
        <div class="lcd-top">
          <span>{{ activeDoc ? activeDoc.info.sampleRate + ' Hz' : '— Hz' }}</span>
          <span>{{ activeDoc ? chText(activeDoc.info.channels) : '—' }}</span>
          <span v-if="recording" class="lcd-rec">● REC</span>
        </div>
        <div class="lcd-time">{{ lcdTime.toFixed(3) }}<span class="lcd-unit">秒</span></div>
      </div>

      <div class="tb-right">
        <span v-if="busy" class="busy">⏳ 处理中…</span>
        <span class="filename">{{ activeDoc ? activeDoc.name : '未打开文件' }}</span>
      </div>
    </div>

    <!-- 主体：侧栏 + 视图 -->
    <div class="body">
      <aside>
        <div class="side-title">已打开的文件</div>
        <div v-for="d in docs" :key="d.docId" class="fileitem" :class="{ on: d.docId === activeId }"
          @click.stop="selectDoc(d.docId)" @contextmenu.prevent.stop="onFileCtx($event, d.docId)">
          <div class="fi-icon">🎵</div>
          <div class="fi-text">
            <div class="fi-name" :title="d.name"><span v-if="isDirty(d.docId)" class="fi-dirty">●</span>{{ d.name }}</div>
            <div class="fi-meta">{{ fmtMeta(d) }}</div>
          </div>
          <button class="fi-close" title="关闭" @click.stop="closeDoc(d.docId)">✕</button>
        </div>
        <div v-if="!docs.length" class="side-empty">尚无打开的文件</div>

        <template v-if="activeDoc && uiPrefs.showHistory">
          <div class="side-title hist-title">
            <span>操作历史</span>
            <span class="hist-btns">
              <button class="hbtn" :disabled="!canUndo || busy" title="撤销 (Ctrl+Z)" @click.stop="editUndo()">↶</button>
              <button class="hbtn" :disabled="!canRedo || busy" title="重做 (Ctrl+Y)" @click.stop="editRedo()">↷</button>
            </span>
          </div>
          <div class="hist-list">
            <div v-for="(h, i) in histItems" :key="i" class="hist-item"
              :class="{ cur: i === hist.undo.length, undone: !h.done }"
              :title="i === hist.undo.length ? '当前状态' : '点击回到这一步'" @click.stop="gotoHistory(i)">
              <span class="hist-no">{{ i === 0 ? '·' : i }}</span><span class="hist-label">{{ h.label }}</span>
            </div>
          </div>
        </template>
      </aside>

      <main ref="viewareaEl" class="viewarea" @contextmenu.prevent.stop="onDiagramCtx">
        <template v-if="activeDoc">
          <div class="trow overview-row">
            <div class="chlabel">总览</div>
            <div class="cwrap ovwrap" :class="{ zoomed }" title="滚轮：放大 / 缩小 · 拖动：平移"
              @wheel="overviewWheel" @mousedown="overviewDown" @mousemove="overviewMove"
              @mouseup="overviewUp" @mouseleave="overviewUp"><canvas ref="overviewCanvas"></canvas></div>
          </div>

          <div class="rows" ref="rowsEl">
            <template v-if="view === 'wave' || view === 'both' || view === 'waveconst'">
              <div v-for="c in chList" :key="'w' + c" class="trow" :data-ch="c - 1" :class="{ 'ch-off': !chEnabled(c - 1) }">
                <div class="chlabel">#{{ c - 1 }}</div>
                <div class="cwrap"><canvas :ref="(el) => setCanvas(waveCanvases, c - 1, el)"></canvas></div>
              </div>
            </template>

            <template v-if="view === 'spec' || view === 'both'">
              <div v-for="c in chList" :key="'s' + c" class="trow" :data-ch="c - 1" :class="{ 'ch-off': !chEnabled(c - 1) }">
                <div class="chlabel dim spec-label">
                  <span>spec {{ c - 1 }}</span>
                  <canvas class="spec-legend" :ref="(el) => setCanvas(specLegends, c - 1, el)" title="电平 dBFS（颜色相对文件最大电平，动态范围以下为黑）"></canvas>
                </div>
                <div class="cwrap"><canvas :ref="(el) => setCanvas(specCanvases, c - 1, el)"></canvas></div>
              </div>
            </template>

            <template v-if="view === 'waveconst'">
              <div v-for="c in chList" :key="'m' + c" class="trow mss-row" :data-ch="c - 1" :class="{ 'ch-off': !chEnabled(c - 1) }">
                <div class="chlabel dim">星座 {{ c - 1 }}</div>
                <div class="cwrap"><canvas :ref="(el) => setCanvas(mssCanvases, c - 1, el)"></canvas></div>
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
              <div class="cursor-fixed" v-if="cursorSec > 0 || sel" :style="{ left: pct(cursorSec) }"></div>
              <div class="cursor-line" v-if="hoverSec != null" :style="{ left: pct(hoverSec) }"></div>
              <div class="playhead" v-if="playing" :style="{ left: pct(playheadSec) }"></div>
              <!-- 录音插入指示器 -->
              <div class="rec-insert-marker" v-if="recording && recInsertDocId" :style="{ left: pct(cursorSec) }">
                <div class="rec-insert-line"></div>
                <div class="rec-insert-badge">● REC {{ recSec.toFixed(1) }}s</div>
              </div>
            </div>
          </div>

          <div class="trow ruler-row">
            <div class="chlabel"></div>
            <div class="cwrap rulerwrap"><canvas ref="rulerCanvas"></canvas></div>
          </div>
        </template>
        <!-- 录音实时波形可视化 -->
        <div v-else-if="recording && !recInsertDocId" class="rec-live">
          <div class="rec-live-title">● 录音中 — 新建音频文件</div>
          <div class="rec-live-wave">
            <div class="rec-bar" v-for="i in 40" :key="i"
              :style="{ height: Math.max(4, recLevel * (60 + Math.sin(Date.now() / 200 + i) * 30)) + 'px' }"></div>
          </div>
          <div class="rec-live-time">{{ recSec.toFixed(1) }}s</div>
        </div>
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
      <span class="mode">桌面模式 · 引擎内嵌</span>
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

      <!-- ===== 文件菜单（文件列表右键） ===== -->
      <template v-if="ctx.kind === 'file'">
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxInfo">
          <span class="ci">ⓘ</span><span>查看信息…</span>
        </div>
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxPacketLoss">
          <span class="ci">📉</span><span>查看丢包率…</span>
        </div>
        <template v-if="ctxDoc && ctxDoc.info.channels > 1">
          <div class="sep"></div>
          <div v-for="c in ctxDoc.info.channels" :key="'x' + c" class="mi" @click.stop="extractChannel(ctxDoc.docId, c - 1)">
            <span class="ci">⇲</span><span>抽取{{ chName(c - 1, ctxDoc.info.channels) }}</span>
          </div>
        </template>
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
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxSave">
          <span class="ci">💾</span><span>保存</span><span class="sc">{{ MOD }}S</span>
        </div>
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="openSaveAs('export')">
          <span class="ci">⇱</span><span>导出…</span>
        </div>
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="openSaveAs('saveas')">
          <span class="ci">⇱</span><span>另存为…</span><span class="sc">{{ MOD }}Shift+S</span>
        </div>
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxRename">
          <span class="ci">✎</span><span>重命名…</span>
        </div>
        <div class="sep"></div>
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxCloseFile">
          <span class="ci">✕</span><span>关闭</span><span class="sc">{{ MOD }}W</span>
        </div>
      </template>

      <!-- ===== 图表菜单（波形 / 频谱 / 星座区右键） ===== -->
      <template v-else>
        <template v-if="ctxDoc && ctxDoc.info.channels > 1">
          <div class="mi" v-if="ctx.ch != null" @click.stop="ctxSoloChannel">
            <span class="ci">🎧</span><span>仅播放此声道（{{ chName(ctx.ch, ctxDoc.info.channels) }}）</span>
          </div>
          <div class="mi" :class="{ dis: allChOn }" @click.stop="setChannels(null)">
            <span class="ci">🔈</span><span>播放全部声道</span>
          </div>
          <div class="sep"></div>
        </template>

        <div class="mi" :class="{ dis: !hasSel }" @click.stop="editCut">
          <span class="ci">✂</span><span>剪切</span><span class="sc">{{ MOD }}X</span>
        </div>
        <div class="mi" :class="{ dis: !hasSel }" @click.stop="editCopy">
          <span class="ci">⧉</span><span>复制</span><span class="sc">{{ MOD }}C</span>
        </div>
        <div class="mi" :class="{ dis: !clip }" @click.stop="editPaste"
          :title="clip ? `剪贴板：${clip.durationSec.toFixed(3)}s` + '（插入到播放标记处）' : '剪贴板为空'">
          <span class="ci">📋</span><span>粘贴</span><span class="sc">{{ MOD }}V</span>
        </div>
        <div class="mi" :class="{ dis: !hasSel }" @click.stop="editSilence">
          <span class="ci">🔇</span><span>静音</span>
        </div>
        <div class="mi" :class="{ dis: !hasSel }" @click.stop="editFade('in')">
          <span class="ci">◢</span><span>淡入…</span>
        </div>
        <div class="mi" :class="{ dis: !hasSel }" @click.stop="editFade('out')">
          <span class="ci">◣</span><span>淡出…</span>
        </div>
        <template v-if="hasSel">
          <div class="sep"></div>
          <div class="mi" @click.stop="selPacketLoss">
            <span class="ci">📉</span><span>计算丢包率{{ ctx.ch != null && ctxDoc && ctxDoc.info.channels > 1 ? '（' + chName(ctx.ch, ctxDoc.info.channels) + '）' : '' }}…</span>
          </div>
          <div class="mi" v-if="ctx.ch != null && ctxDoc" @click.stop="selExtractChannel">
            <span class="ci">⇲</span><span>抽取此声道{{ ctxDoc.info.channels > 1 ? '（' + chName(ctx.ch, ctxDoc.info.channels) + '）' : '' }}</span>
          </div>
        </template>
        <div class="sep"></div>
        <div class="mi" :class="{ dis: !canUndo }" @click.stop="editUndo()">
          <span class="ci">↶</span><span>撤销{{ canUndo ? '：' + hist.undo[hist.undo.length - 1] : '' }}</span><span class="sc">{{ MOD }}Z</span>
        </div>
        <div class="mi" :class="{ dis: !canRedo }" @click.stop="editRedo()">
          <span class="ci">↷</span><span>重做{{ canRedo ? '：' + hist.redo[0] : '' }}</span><span class="sc">{{ MOD }}Y</span>
        </div>
        <div class="sep"></div>
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxPlay">
          <span class="ci">▶</span><span>播放</span><span class="sc">Space</span>
        </div>
        <div class="mi" :class="{ dis: !ctxDoc }" @click.stop="ctxSelectAll">
          <span class="ci">▭</span><span>全选</span><span class="sc">{{ MOD }}A</span>
        </div>
        <div class="mi" :class="{ dis: !sel }" @click.stop="ctxDeselect">
          <span class="ci"></span><span>取消选择</span>
        </div>
      </template>
    </div>

    <!-- 淡入 / 淡出：增益曲线编辑器 -->
    <div class="modal-mask" v-if="fadeDlg">
      <div class="modal fade-modal">
        <div class="modal-title">{{ fadeDlg.dir === 'in' ? '淡入' : '淡出' }} — 增益曲线
          <span class="fade-sub">选区 {{ fadeDlg.secs.toFixed(3) }}s</span></div>
        <div class="fade-presets">
          <span class="fade-lbl">预设</span>
          <button v-for="p in FADE_PRESETS" :key="p.key" @click="setFadePreset(p.key)">{{ p.label }}</button>
        </div>
        <canvas ref="fadeCanvas" class="fade-canvas" width="600" height="300"
          @mousedown="fadeDown" @mousemove="fadeHoverMove" @mouseleave="fadeHover = null"
          @dblclick="fadeDelete" @contextmenu.prevent.stop="fadeDelete"></canvas>
        <div class="fade-help">拖动控制点调整曲线 · 在空白处按下添加控制点 · 双击或右键删除控制点 ·
          灰色为原波形，青色为应用后的预览</div>
        <div class="modal-foot">
          <span class="pref-note fade-readout">{{ fadeReadout }}</span>
          <button @click="fadeDlg = null">取消</button>
          <button class="btn-pri" @click="applyFade">应用</button>
        </div>
      </div>
    </div>

    <!-- 查看丢包率 -->
    <div class="modal-mask" v-if="plDlg" @click.self="plDlg = null">
      <div class="modal modal-sm">
        <div class="modal-title">丢包率 — {{ plDlg.name }}</div>
        <div class="pl-line">{{ plDlg.scope }}</div>
        <div class="pl-rate">{{ (plDlg.r.lossRate * 100).toFixed(2) }}%</div>
        <div class="pl-line">丢包 {{ plDlg.r.lostPackets }} / {{ plDlg.r.totalPackets }} 包（每包 {{ plDlg.r.packetMs }} ms）</div>
        <div class="pl-line">缺口 {{ plDlg.r.gapCount }} 处，最长 {{ plDlg.r.longestGapMs.toFixed(0) }} ms</div>
        <div class="pl-gaps" v-if="plDlg.r.gaps.length">
          <div v-for="(g, i) in plDlg.r.gaps.slice(0, 20)" :key="i">
            {{ g.startSec.toFixed(3) }}s · {{ g.durationMs.toFixed(0) }} ms · {{ g.packets }} 包 · 低 {{ g.depthDb.toFixed(0) }} dB
          </div>
          <div v-if="plDlg.r.gaps.length > 20">… 另有 {{ plDlg.r.gaps.length - 20 }} 处</div>
        </div>
        <div class="pl-note" v-if="plDlg.r.estimated">
          ⚠ 估算值：文件中没有真实的包信息（如 RTP 序号）。这里把语音中突然出现、比前后语音低
          ≥ {{ plDlg.r.depthDb }} dB、长 15–{{ plDlg.r.maxGapMs }} ms 的短缺口当作丢包（按 {{ plDlg.r.packetMs }} ms 一包计）；
          更长的静音、或紧挨着自然停顿的丢包无法区分，不计入。若设备做了丢包补偿（PLC）会偏低。
        </div>
        <div class="modal-foot">
          <span class="pref-note"></span>
          <button @click="plDlg = null">关闭</button>
        </div>
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

        <div class="form-row">
          <label>采样率</label>
          <select v-model.number="saveForm.sampleRate">
            <option v-for="r in saveSampleRates" :key="r" :value="r">{{ r }} Hz</option>
          </select>
        </div>

        <div class="form-row">
          <label>位深</label>
          <select v-model.number="saveForm.bitDepth">
            <option v-for="b in caps?.bitDepths ?? [16, 24, 32]" :key="b" :value="b">
              {{ b === 32 ? '32 位浮点 (IEEE Float)' : b + ' 位 PCM' }}
            </option>
          </select>
        </div>

        <template v-if="saveIsLossy">
          <div class="form-row">
            <label>压缩算法</label>
            <span class="ro-codec">{{ saveFmt?.label ?? saveForm.format.toUpperCase() }}</span>
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
          <div><span class="pk">目标</span>{{ saveForm.format.toUpperCase() }} · {{ saveForm.sampleRate }} Hz ·
            {{ saveForm.bitDepth === 32 ? '32 位浮点' : saveForm.bitDepth + ' 位' }}</div>
          <div class="bitrate-info">预估码率　{{ estimatedBitrate }}</div>
        </div>

        <div class="modal-foot">
          <span class="pref-note">
            <template v-if="!savePathOk">请先点「浏览…」选择输出位置（需要绝对路径）</template>
            <template v-else>{{ saveIsPcm ? '无损导出' : '有损压缩（经 FFMPEG 编码）' }}</template>
          </span>
          <button @click="doSave" :disabled="busy || !savePathOk">导出</button>
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
        <div class="modal-title">首选项</div>
        <div class="pref-sec">界面</div>
        <label class="chk-inline pref-chk">
          <input type="checkbox" v-model="uiPrefs.showHistory" />
          <span>在侧栏显示「操作历史」面板</span>
        </label>

        <div class="pref-sec">音频</div>
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

    <!-- 关闭前保存确认 -->
    <div class="modal-mask" v-if="showCloseConfirm" @click.self="closeConfirmCancel">
      <div class="modal modal-sm close-confirm">
        <div class="modal-title">保存更改？</div>
        <div class="close-confirm-msg">
          文件「<strong>{{ closeConfirmDoc?.name ?? '未知' }}</strong>」已被修改。<br />
          关闭前是否保存更改？
        </div>
        <div class="modal-foot close-confirm-btns">
          <button class="btn-discard" @click="closeConfirmDiscard">不保存</button>
          <span style="flex: 1"></span>
          <button @click="closeConfirmCancel">取消</button>
          <button class="btn-save" @click="closeConfirmSave">保存</button>
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
/* 三栏：左按钮 / 中 LCD（始终居中）/ 右文件名 */
.toolbar { display: grid; grid-template-columns: 1fr auto 1fr; gap: 8px; align-items: center; background: #181d23; border-bottom: 1px solid #232b33; padding: 5px 8px; }
.tbtn { width: 32px; height: 28px; padding: 0; display: inline-flex; align-items: center; justify-content: center; background: #20262e; border: 1px solid #2c343d; border-radius: 5px; cursor: pointer; }
.tbtn:hover:not(:disabled) { background: #2c3540; }
.tbtn:disabled { opacity: 0.4; cursor: default; }
.tbtn.rec { border-color: #ff5a5a; box-shadow: 0 0 8px rgba(255,90,90,0.5); animation: recPulse 1s infinite; }
@keyframes recPulse { 50% { box-shadow: 0 0 2px rgba(255,90,90,0.2); } }

/* LCD 电子显示屏 */
.tb-left { display: flex; gap: 4px; align-items: center; }
.tb-right { display: flex; gap: 8px; align-items: center; justify-content: flex-end; min-width: 0; }
.lcd { background: #04120a; border: 1px solid #0f3320; border-radius: 5px; padding: 2px 12px 3px; min-width: 220px; box-shadow: inset 0 0 8px rgba(0,0,0,0.8); }
.lcd-top { display: flex; gap: 12px; color: #1f9e4d; font-size: 10px; font-family: Consolas, monospace; }
.lcd-rec { color: #ff5a5a; animation: recBlink 1s infinite; }
@keyframes recBlink { 50% { opacity: 0.25; } }
.lcd-time { color: #35e86b; font: bold 20px/1.15 Consolas, 'Courier New', monospace; letter-spacing: 1px; text-shadow: 0 0 6px rgba(53,232,107,0.55); }
.lcd-unit { font-size: 11px; font-weight: normal; margin-left: 6px; opacity: 0.8; }

.toolbar .busy { color: #e8b34b; font-size: 12px; }
.toolbar .filename { color: #8fa1b3; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* 主体 */
.body { flex: 1; display: flex; min-height: 0; }
aside { width: 220px; background: #171c22; border-right: 1px solid #232b33; overflow-y: auto; padding: 6px; }
.side-title { color: #8fa1b3; font-size: 12px; padding: 4px 6px; border-bottom: 1px solid #232b33; margin-bottom: 6px; }
.trow.ch-off canvas { filter: grayscale(1) brightness(0.55); }
.trow.ch-off .chlabel { color: #4a5563; text-decoration: line-through; }
.mss-row { flex: 1.4; }
.side-empty { color: #56657a; font-size: 12px; padding: 10px 6px; }
.pref-chk { margin: 2px 0 10px; }
.hist-title { display: flex; align-items: center; justify-content: space-between; margin-top: 14px; }
.hist-btns { display: inline-flex; gap: 4px; }
.hbtn { padding: 0 8px; height: 20px; line-height: 18px; font-size: 13px; }
.hist-list { display: flex; flex-direction: column; gap: 1px; max-height: 40vh; overflow-y: auto; }
.hist-item { display: flex; gap: 8px; padding: 4px 8px; border-radius: 4px; cursor: pointer; font-size: 12px; color: #c3ccd6; border: 1px solid transparent; }
.hist-item:hover { background: #222a33; }
.hist-item.cur { background: #1b3a35; border-color: #1fd6a4; color: #e6fff7; }
.hist-item.undone { color: #56657a; font-style: italic; }
.hist-no { width: 18px; text-align: right; color: #6b7a8a; font-family: Consolas, monospace; }
.fileitem { display: flex; gap: 8px; align-items: center; padding: 8px; border-radius: 6px; cursor: pointer; border: 1px solid transparent; }
.fileitem:hover { background: #1f262e; }
.fileitem.on { background: #1c3a4a; border-color: #1fd6a4; }
.fi-icon { font-size: 18px; }
.fi-text { flex: 1; min-width: 0; }
.fi-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; }
.fi-meta { color: #6b7a8a; font-size: 11px; }
.fi-dirty { color: #e8b34b; margin-right: 3px; font-size: 10px; }
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
.chlabel.spec-label { flex-direction: column; align-items: stretch; justify-content: flex-start; text-align: right; padding-right: 2px; }
.spec-legend { flex: 1; min-height: 0; width: 100%; display: block; }
.trow.ch-off .spec-legend { opacity: 0.4; }
.cwrap { flex: 1; position: relative; min-width: 0; }
.cwrap canvas { position: absolute; inset: 0; width: 100%; height: 100%; display: block; border: 1px solid #232b33; border-radius: 4px; background: #0d1117; }
.overview-row .cwrap canvas, .rulerwrap canvas { border-color: #1d2630; }
.ruler-row { height: 30px; flex: none; }

/* 覆盖层：光标/选区/播放头（画布列区域 = 左60px，右8px） */
.overlay { position: absolute; left: 60px; right: 8px; top: 0; bottom: 0; cursor: text; user-select: none; overflow: hidden; }
.ovwrap { user-select: none; }
.ovwrap.zoomed { cursor: grab; }
.sel-box { position: absolute; top: 0; bottom: 0; background: rgba(31, 214, 164, 0.14); border-left: 1px solid rgba(31,214,164,0.7); border-right: 1px solid rgba(31,214,164,0.7); pointer-events: none; }
.cursor-fixed { position: absolute; top: 0; bottom: 0; width: 1px; background: rgba(31,214,164,0.85); pointer-events: none; z-index: 1; }
.cursor-line { position: absolute; top: 0; bottom: 0; width: 1px; background: rgba(255,255,255,0.35); pointer-events: none; }
.playhead { position: absolute; top: 0; bottom: 0; width: 2px; background: #ff6b6b; box-shadow: 0 0 6px rgba(255,107,107,0.8); pointer-events: none; }
.rec-insert-marker { position: absolute; top: 0; bottom: 0; pointer-events: none; z-index: 2; }
.rec-insert-line { position: absolute; top: 0; bottom: 0; width: 2px; background: #ff5a5a; box-shadow: 0 0 8px rgba(255,90,90,0.8); animation: recPulse 1s infinite; }
.rec-insert-badge { position: absolute; top: 4px; left: 6px; background: rgba(255,90,90,0.9); color: white; font-size: 10px; padding: 1px 6px; border-radius: 3px; white-space: nowrap; animation: recBlink 1s infinite; }

.const-params { display: flex; gap: 8px; align-items: center; color: #8fa1b3; margin: 4px 8px 0 54px; flex-wrap: wrap; font-size: 12px; }
input, select { background: #1d232a; border: 1px solid #2c343d; color: inherit; padding: 4px 8px; border-radius: 4px; font: inherit; }
input.num { width: 52px; }
button { background: #26303a; border: 1px solid #35414d; color: inherit; padding: 4px 14px; border-radius: 4px; cursor: pointer; font: inherit; }
button:hover:not(:disabled) { background: #31404d; }
button:disabled { opacity: 0.45; cursor: default; }

/* 录音实时波形 */
.rec-live { height: 100%; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 16px; }
.rec-live-title { color: #ff5a5a; font-size: 16px; font-weight: 600; animation: recBlink 1s infinite; }
.rec-live-wave { display: flex; gap: 3px; align-items: center; height: 80px; }
.rec-bar { width: 6px; background: linear-gradient(to top, #1fd6a4, #35e86b); border-radius: 3px; transition: height 0.15s ease; min-height: 4px; }
.rec-live-time { color: #35e86b; font: bold 28px/1 Consolas, 'Courier New', monospace; text-shadow: 0 0 8px rgba(53,232,107,0.5); }

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
.pl-rate { font: bold 28px Consolas, monospace; color: #e8b34b; margin: 4px 0 6px; }
.pl-line { color: #c3ccd6; font-size: 12px; margin: 2px 0; }
.pl-gaps { margin-top: 8px; max-height: 160px; overflow-y: auto; font: 11px Consolas, monospace; color: #8fa1b3; border: 1px solid #2c343d; border-radius: 4px; padding: 4px 8px; }
.pl-note { margin-top: 10px; font-size: 11px; color: #b98d33; line-height: 1.5; }
.fade-modal { width: 640px; }
.fade-sub { margin-left: 10px; font-size: 12px; font-weight: normal; color: #6b7a8a; }
.fade-presets { display: flex; flex-wrap: wrap; gap: 6px; align-items: center; margin-bottom: 10px; }
.fade-lbl { color: #8fa1b3; font-size: 12px; margin-right: 2px; }
.fade-canvas { display: block; width: 600px; height: 300px; border: 1px solid #2c343d; border-radius: 6px; cursor: crosshair; }
.fade-help { margin-top: 8px; color: #6b7a8a; font-size: 11px; }
.fade-readout { flex: 1; font-family: Consolas, monospace; color: #e8b34b; }
.btn-pri { background: #1b4d40; border-color: #1fd6a4; color: #e6fff7; }
.btn-pri:hover { background: #22604f; }
.form-row { display: flex; align-items: center; gap: 10px; margin: 8px 0; }
.form-row > label { width: 76px; flex: none; color: #8fa1b3; font-size: 12px; }
.form-row .ro { color: #6b7a8a; font-size: 12px; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.form-row .unit { color: #6b7a8a; font-size: 12px; }
.chk-inline { display: flex; align-items: center; gap: 6px; color: #d8dee5; font-size: 12px; cursor: pointer; }
.save-preview { margin-top: 12px; padding: 8px 10px; background: #161c23; border: 1px solid #2c343d; border-radius: 6px; display: flex; flex-direction: column; gap: 4px; color: #8fa1b3; font-size: 12px; font-family: Consolas, monospace; }
.save-preview .pk { display: inline-block; width: 34px; color: #1fd6a4; }
.save-preview .bitrate-info { color: #6b7a8a; font-size: 11px; margin-top: 2px; }
.ro-codec { color: #d8dee5; font-size: 13px; background: #2a3340; padding: 3px 10px; border-radius: 4px; border: 1px solid #35414d; }

/* 关闭前保存确认 */
.close-confirm-msg { color: #d8dee5; font-size: 13px; line-height: 1.7; padding: 8px 0 4px; }
.close-confirm-msg strong { color: #1fd6a4; }
.close-confirm-btns { gap: 8px !important; }
.btn-discard { background: #3a2020 !important; border-color: #5a3030 !important; color: #ff8888 !important; }
.btn-discard:hover { background: #4a2828 !important; }
.btn-save { background: #1a3a2a !important; border-color: #1fd6a4 !important; color: #1fd6a4 !important; }
.btn-save:hover { background: #244a38 !important; }

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
