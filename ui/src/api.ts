/** 引擎命令总线客户端 —— 双模式：
 *  - 桌面（Tauri 2）：invoke() 直调 Rust 命令（同进程，无网络）
 *  - 浏览器 / dev：WebSocket JSON 帧连 sonari-server
 *  两种模式共用 `sonari_engine::commands`，契约一致。 */
import { invoke } from '@tauri-apps/api/core';

export const IS_TAURI =
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

export interface DocInfo {
  sampleRate: number;
  channels: number;
  channelsText: string;
  frames: number;
  durationSec: number;
  bitsPerSample: number | null;
  codec: string;
  codecLabel: string;
}

/** 「查看信息」—— 对标 ocenaudio「信号属性」 */
export interface DocDetail {
  docId: string;
  name: string;
  path: string;
  container: string | null;
  codec: string;
  codecLabel: string;
  sampleRate: number;
  channels: number;
  channelsText: string;
  frames: number;
  durationSec: number;
  bitsPerSample: number | null;
  bitsPerCodedSample: number | null;
  fileBytes: number | null;
  bitrateKbps: number | null;
  channelPeaksDbfs: number[];
  stats: LevelStats | null;
}

export interface LevelStats {
  integratedLufs: number;
  loudnessRangeLu: number;
  truePeakDbtp: number;
  samplePeakDbfs: number;
}

export interface DocOpened {
  docId: string;
  name: string;
  /** 来源路径；录音产物为空（另存为后会改绑） */
  path?: string;
  info: DocInfo;
  /** 4096 bucket × [min,max] 展平 */
  peaks: number[];
  stats: LevelStats | null;
}

/** 编码器能力（ffmpeg 是否可用 / 支持哪些格式） */
export interface EncoderCaps {
  ffmpeg: boolean;
  ffmpegPath: string | null;
  bitDepths: number[];
  bitrates: number[];
  formats: {
    ext: string;
    label: string;
    builtin: boolean;
    lossy: boolean;
    available?: boolean;
  }[];
}

/** 写出结果 */
export interface SaveResult {
  docId: string;
  path: string;
  bytes: number;
  format: string;
  bitDepth: number;
  region: boolean;
  frames: number;
  viaFfmpeg: boolean;
  rebound: boolean;
}

export interface Star {
  frame: number;
  bin: number;
  db: number;
}
export interface Hash {
  f1: number;
  f2: number;
  dt: number;
  t1: number;
}
export interface ConstellationResult {
  stars: Star[];
  hashes: Hash[];
  fft_len: number;
  hop: number;
  frames_analyzed: number;
}

type Pending = { resolve: (v: any) => void; reject: (e: any) => void };

export class Engine {
  private ws: WebSocket | null = null;
  private seq = 1;
  private pending = new Map<number, Pending>();
  url: string;

  constructor(url = 'ws://127.0.0.1:9712') {
    this.url = url;
  }

  connect(): Promise<void> {
    if (IS_TAURI) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url);
      ws.onopen = () => {
        this.ws = ws;
        resolve();
      };
      ws.onerror = () => reject(new Error(`无法连接 ${this.url}`));
      ws.onmessage = (ev) => {
        try {
          const m = JSON.parse(ev.data);
          const p = this.pending.get(m.id);
          if (p) {
            this.pending.delete(m.id);
            if (m.ok) p.resolve(m.data);
            else p.reject(new Error(m.error ?? 'engine error'));
          }
        } catch {
          /* 忽略坏帧 */
        }
      };
    });
  }

  private call<T>(cmd: string, params: Record<string, unknown> = {}): Promise<T> {
    if (IS_TAURI) {
      // 单一命令入口：Tauri 侧按 cmd 前缀路由（commands / audio）
      // Tauri 命令 Err(String) 到前端可能是裸字符串，统一包成 Error 以便 e.message 可用
      return invoke<T>('engine', {
        cmd,
        p: params as unknown as Record<string, unknown>,
      }).catch((e: unknown) => {
        throw new Error(errText(e));
      });
    }
    const ws = this.ws;
    if (!ws) return Promise.reject(new Error('not connected'));
    const id = this.seq++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      ws.send(JSON.stringify({ id, cmd, params }));
      setTimeout(() => {
        if (this.pending.delete(id)) reject(new Error(`${cmd} timeout`));
      }, 120_000);
    });
  }

  docOpen(path: string): Promise<DocOpened> {
    return this.call('doc_open', { path });
  }
  /** 逐声道波形峰值：data 按声道顺次拼接的 [min,max] 平铺数组 */
  waveform(
    docId: string,
    buckets = 2000,
  ): Promise<{ channels: number; buckets: number; data: number[] }> {
    return this.call('waveform', { docId, buckets });
  }
  docClose(docId: string): Promise<void> {
    return this.call('doc_close', { docId });
  }
  /** 信号属性（右键「查看信息」） */
  docInfo(docId: string): Promise<DocDetail> {
    return this.call('doc_info', { docId });
  }
  /** 「转换为」采样率：原地改文档，返回刷新后的完整文档（含新 peaks/stats） */
  docResample(docId: string, sampleRate: number): Promise<DocOpened> {
    return this.call('doc_resample', { docId, sampleRate });
  }
  /** 「转换声道」：1 = 单声道，2 = 立体声 */
  docChannels(docId: string, channels: number): Promise<DocOpened> {
    return this.call('doc_channels', { docId, channels });
  }
  /** 「重命名…」：改磁盘文件名（内存文档只改显示名） */
  docRename(docId: string, name: string): Promise<{ docId: string; name: string; path: string }> {
    return this.call('doc_rename', { docId, name });
  }
  /** 「保存 / 导出… / 另存为…」：path 缺省 = 覆盖原文件；给 begin/end = 只写选区 */
  docSave(o: {
    docId: string;
    path?: string;
    format?: string;
    bitDepth?: number;
    bitrateKbps?: number;
    begin?: number;
    end?: number;
  }): Promise<SaveResult> {
    return this.call('doc_save', o);
  }
  /** 可用输出格式（ffmpeg 探测结果） */
  encoderCaps(): Promise<EncoderCaps> {
    return this.call('encoder_caps', {});
  }
  spectrogram(
    docId: string,
    o: { fft: number; hop: number; begin?: number; end?: number; maxFrames?: number; channel?: number },
  ): Promise<{ fft: number; hop: number; sampleRate: number; begin: number; frames: number[][] }> {
    return this.call('spectrogram', { docId, ...o });
  }
  constellation(
    docId: string,
    o: {
      fft?: number;
      hop?: number;
      peaksPerFrame?: number;
      minBin?: number;
      fanOut?: number;
      maxPairs?: number;
      preEmphasis?: boolean;
      begin?: number;
      end?: number;
    },
  ): Promise<ConstellationResult> {
    return this.call('constellation', { docId, ...o });
  }
  pcm(
    docId: string,
    begin: number,
    end: number,
  ): Promise<{ sampleRate: number; channels: number; frames: number; f32leBase64: string }> {
    return this.call('pcm', { docId, begin, end });
  }
  /** 从内存打开文档（录音产物）：f32le 交错样本 */
  docOpenData(o: {
    name: string;
    sampleRate: number;
    channels: number;
    f32leBase64: string;
  }): Promise<DocOpened> {
    return this.call('doc_open_data', o);
  }

  // ---- 音频设备（cpal） ----
  audioDevices(): Promise<{
    host: string;
    outputs: { name: string; meta: string; isDefault: boolean }[];
    inputs: { name: string; meta: string; isDefault: boolean }[];
  }> {
    return this.call('audio_devices', {});
  }
  playbackStart(docId: string, begin: number, end: number, device?: string): Promise<unknown> {
    return this.call('playback_start', { docId, begin, end, device: device ?? '' });
  }
  playbackStop(): Promise<unknown> {
    return this.call('playback_stop', {});
  }
  playbackStatus(): Promise<{ playing: boolean; positionSec: number }> {
    return this.call('playback_status', {});
  }
  recordStart(device?: string): Promise<unknown> {
    return this.call('record_start', { device: device ?? '' });
  }
  recordStatus(): Promise<{ recording: boolean; seconds: number; sampleRate?: number; channels?: number }> {
    return this.call('record_status', {});
  }
  recordStop(): Promise<{
    sampleRate: number;
    channels: number;
    frames: number;
    durationSec: number;
    f32leBase64: string;
  }> {
    return this.call('record_stop', {});
  }
}

/** 错误文本兜底 —— Tauri 的 Err(String) 到前端常是裸字符串，没有 .message */
export function errText(e: unknown): string {
  if (e == null) return '未知错误';
  if (typeof e === 'string') return e;
  if (e instanceof Error) return e.message || String(e);
  const o = e as Record<string, unknown>;
  return String(o.message ?? o.error ?? o.msg ?? e);
}

/** base64 → Float32Array（PCM 播放用） */
export function b64ToF32(b64: string): Float32Array {
  const bin = atob(b64);
  const n = bin.length >> 2;
  const out = new Float32Array(n);
  const view = new DataView(new ArrayBuffer(bin.length));
  for (let i = 0; i < bin.length; i++) view.setUint8(i, bin.charCodeAt(i));
  for (let i = 0; i < n; i++) out[i] = view.getFloat32(i * 4, true);
  return out;
}
