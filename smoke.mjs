// sonari 端到端冒烟：doc_open → peaks/stats → spectrogram → constellation → pcm
import WebSocket from 'ws';

const url = process.argv[2] ?? 'ws://127.0.0.1:9712';
const PATH = process.argv[3] ?? 'D:\\tools\\sonari\\test\\tone3.wav';

let ws;
let seq = 1;
const pending = new Map();

function connect() {
  return new Promise((res, rej) => {
    ws = new WebSocket(url);
    ws.on('open', res);
    ws.on('error', rej);
    ws.on('message', (d) => {
      const m = JSON.parse(d.toString());
      const p = pending.get(m.id);
      if (p) {
        pending.delete(m.id);
        m.ok ? p.res(m.data) : p.rej(new Error(m.error));
      }
    });
  });
}

function call(cmd, params = {}) {
  const id = seq++;
  return new Promise((res, rej) => {
    pending.set(id, { res, rej });
    ws.send(JSON.stringify({ id, cmd, params }));
    setTimeout(() => {
      if (pending.delete(id)) rej(new Error(`${cmd} timeout`));
    }, 60_000);
  });
}

let pass = 0;
let fail = 0;
function check(name, cond, detail = '') {
  if (cond) {
    pass++;
    console.log(`  PASS  ${name}${detail ? ' — ' + detail : ''}`);
  } else {
    fail++;
    console.log(`  FAIL  ${name}${detail ? ' — ' + detail : ''}`);
  }
}

const t0 = Date.now();
const T = () => `(+${((Date.now() - t0) / 1000).toFixed(1)}s)`;

try {
  await connect();
  console.log(`connected ${url}`);

  // 1) 打开文档
  const doc = await call('doc_open', { path: PATH });
  check('doc_open', !!doc.docId, `${doc.name} sr=${doc.info.sampleRate} ch=${doc.info.channels} frames=${doc.info.frames}`);
  check('decode 正确性', doc.info.sampleRate === 44100 && doc.info.frames === 264600, `frames=${doc.info.frames}`);
  check('波形峰值缓存', doc.peaks.length === 4096 * 2, `len=${doc.peaks.length}`);
  const mx = Math.max(...doc.peaks);
  check('峰值非零', mx > 0.3, `max=${mx.toFixed(3)}`);
  check('响度统计', doc.stats && doc.stats.integratedLufs > -40 && doc.stats.integratedLufs < 0,
    `LUFS=${doc.stats?.integratedLufs?.toFixed(1)}`);

  // 2) 频谱图
  const spec = await call('spectrogram', { docId: doc.docId, fft: 1024, hop: 512, maxFrames: 400 });
  check('spectrogram', spec.frames.length > 200, `${spec.frames.length} 帧 × ${spec.frames[0]?.length} bin`);
  // 三段频率应出现在对应时间：2-4s @1200Hz → bin 28 (1200/44100*1024≈27.9)
  const midFrame = spec.frames[Math.floor(spec.frames.length * 0.5)];
  const binAvg = midFrame.reduce((a, b) => a + b, 0) / midFrame.length;
  check('频谱能量集中于 1.2kHz 段', midFrame[28] > binAvg + 15, `bin28=${midFrame[28].toFixed(1)}dB avg=${binAvg.toFixed(1)}dB`);

  // 3) 星座图
  const con = await call('constellation', {
    docId: doc.docId, fft: 1024, hop: 64, peaksPerFrame: 5, fanOut: 10,
  });
  check('constellation 星点', con.stars.length > 500, `${con.stars.length} stars`);
  check('constellation 哈希', con.hashes.length > 500, `${con.hashes.length} hashes`);
  // 星点应分布在三个频带：bin 10(440Hz), 28(1.2k), 70(3k)
  const bands = [10, 28, 70].map((b) => con.stars.filter((s) => Math.abs(s.bin - b) <= 4).length);
  check('三个频段均有星点', bands.every((c) => c > 20), `440Hz:${bands[0]} 1.2k:${bands[1]} 3k:${bands[2]}`);

  // 4) PCM 导出（播放链路）
  const pcm = await call('pcm', { docId: doc.docId, begin: 0, end: 44100 });
  const expected = Math.floor(44100 * 4 * (4 / 3)); // base64 长度 ≈ bytes*4/3
  check('pcm 导出', pcm.frames === 44100 && pcm.f32leBase64.length > expected * 0.9,
    `${pcm.frames} 帧 b64=${(pcm.f32leBase64.length / 1024).toFixed(0)}KB`);
  // 解码第一个样本应为 0（t=0 正弦起点）
  const bin = Buffer.from(pcm.f32leBase64, 'base64');
  const first = bin.readFloatLE(0);
  check('pcm 幅值范围', Math.abs(first) <= 1.0, `first=${first.toFixed(4)}`);

  console.log(`\n${T()} ${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
} catch (e) {
  console.error(`ERROR: ${e.message}`);
  process.exit(2);
} finally {
  ws?.close();
}
