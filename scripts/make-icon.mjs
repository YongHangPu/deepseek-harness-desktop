// 在零外部依赖的前提下生成 dsh-desktop 的应用图标：
//   - src-tauri/icons/icon-source.png  （1024x1024，参考源图）
//   - src-tauri/icons/32x32.png
//   - src-tauri/icons/128x128.png
//   - src-tauri/icons/128x128@2x.png   （256x256）
//   - src-tauri/icons/icon.ico         （多尺寸、PNG 压缩条目）
import { deflateSync } from 'node:zlib';
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..');
const iconsDir = join(root, 'src-tauri', 'icons');
mkdirSync(iconsDir, { recursive: true });

// ---- 极简 PNG 编码器（手写 PNG 结构，不依赖第三方库） --------------------
let crcTable = null;
function crc32(buf) {
  if (!crcTable) {
    crcTable = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      crcTable[n] = c;
    }
  }
  let crc = -1;
  for (let i = 0; i < buf.length; i++) crc = (crc >>> 8) ^ crcTable[(crc ^ buf[i]) & 0xff];
  return (crc ^ -1) >>> 0;
}
// 构造一个 PNG 数据块：长度 + 类型 + 数据 + CRC32 校验。
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, 'ascii');
  const crcBuf = Buffer.alloc(4);
  crcBuf.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
  return Buffer.concat([len, typeBuf, data, crcBuf]);
}
// 把 RGBA 像素缓冲编码为完整 PNG 文件（字节流）。
function encodePng(size, rgba) {
  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // 位深
  ihdr[9] = 6; // 颜色类型：RGBA
  const stride = size * 4 + 1;
  const raw = Buffer.alloc(stride * size);
  for (let y = 0; y < size; y++) {
    raw[y * stride] = 0; // 每行前置滤波字节：无滤波
    rgba.copy(raw, y * stride + 1, y * size * 4, (y + 1) * size * 4);
  }
  const idat = deflateSync(raw, { level: 9 });
  return Buffer.concat([sig, chunk('IHDR', ihdr), chunk('IDAT', idat), chunk('IEND', Buffer.alloc(0))]);
}

// ---- 图标绘制 ---------------------------------------------------------------
const clamp = (v, lo, hi) => (v < lo ? lo : v > hi ? hi : v);
const lerp = (a, b, t) => a + (b - a) * t;

// 圆角矩形的带符号覆盖度，边缘做约 1px 的柔化（用于抗锯齿）。
function roundRectCoverage(px, py, x0, y0, x1, y1, r) {
  const cx = clamp(px, x0 + r, x1 - r);
  const cy = clamp(py, y0 + r, y1 - r);
  const d = Math.hypot(px - cx, py - cy) - r;
  return clamp(0.5 - d, 0, 1);
}

// 配色：深色渐变背景 + 青绿到蓝的渐变竖条。
const BG_TOP = [0x11, 0x30, 0x57];
const BG_BOT = [0x06, 0x0e, 0x1b];
const TEAL = [0x2e, 0xe6, 0xc8];
const BLUE = [0x4d, 0x7c, 0xfe];

// 按指定尺寸绘制图标：圆角矩形背景 + 三条高度不一的圆角竖条。
function render(size) {
  const rgba = Buffer.alloc(size * size * 4);
  const radius = size * 0.21;
  const barW = Math.max(2, Math.round(size * 0.117));
  const gap = Math.max(1, Math.round(size * 0.045));
  const totalW = barW * 3 + gap * 2;
  const startX = Math.round((size - totalW) / 2);
  const hs = [size * 0.30, size * 0.47, size * 0.37];
  const barR = barW / 2;

  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const cov = roundRectCoverage(x + 0.5, y + 0.5, 0, 0, size, size, radius);
      const ty = y / (size - 1);
      let r = lerp(BG_TOP[0], BG_BOT[0], ty);
      let g = lerp(BG_TOP[1], BG_BOT[1], ty);
      let b = lerp(BG_TOP[2], BG_BOT[2], ty);

      for (let i = 0; i < 3; i++) {
        const bx0 = startX + i * (barW + gap);
        const bcov = roundRectCoverage(x + 0.5, y + 0.5, bx0, (size - hs[i]) / 2, bx0 + barW, (size + hs[i]) / 2, barR);
        if (bcov > 0) {
          const tx = clamp((x - startX) / totalW, 0, 1);
          const cr = lerp(TEAL[0], BLUE[0], tx);
          const cg = lerp(TEAL[1], BLUE[1], tx);
          const cb = lerp(TEAL[2], BLUE[2], tx);
          r = lerp(r, cr, bcov);
          g = lerp(g, cg, bcov);
          b = lerp(b, cb, bcov);
        }
      }

      const idx = (y * size + x) * 4;
      rgba[idx] = Math.round(r);
      rgba[idx + 1] = Math.round(g);
      rgba[idx + 2] = Math.round(b);
      rgba[idx + 3] = Math.round(cov * 255);
    }
  }
  return rgba;
}

// ---- ICO 封装（PNG 压缩条目，Vista 及以上系统支持） ------------------------
function icoFromPngs(entries) {
  const count = entries.length;
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0); // 保留字段
  header.writeUInt16LE(1, 2); // 类型：图标
  header.writeUInt16LE(count, 4);
  let offset = 6 + 16 * count;
  const dirs = entries.map(({ size, png }) => {
    const e = Buffer.alloc(16);
    e[0] = size >= 256 ? 0 : size;
    e[1] = size >= 256 ? 0 : size;
    e[2] = 0; // 颜色数
    e[3] = 0; // 保留字段
    e.writeUInt16LE(1, 4); // 平面数
    e.writeUInt16LE(32, 6); // 每像素位数
    e.writeUInt32LE(png.length, 8); // 资源字节数
    e.writeUInt32LE(offset, 12); // 图像数据偏移
    offset += png.length;
    return e;
  });
  return Buffer.concat([header, ...dirs, ...entries.map((e) => e.png)]);
}

// ---- 输出各尺寸图标文件 ----------------------------------------------------
const source = encodePng(1024, render(1024));
writeFileSync(join(iconsDir, 'icon-source.png'), source);

for (const [name, size] of [['32x32.png', 32], ['128x128.png', 128], ['128x128@2x.png', 256]]) {
  writeFileSync(join(iconsDir, name), encodePng(size, render(size)));
}

const icoEntries = [16, 24, 32, 48, 64, 128, 256].map((size) => ({
  size,
  png: encodePng(size, render(size)),
}));
writeFileSync(join(iconsDir, 'icon.ico'), icoFromPngs(icoEntries));

// macOS 图标：ICNS（使用 PNG 编码条目：ic07=128、ic08=256、ic09=512、ic10=1024）。
function icnsFromPngs(entries) {
  const chunks = entries.map(({ type, png }) => {
    const head = Buffer.alloc(8);
    head.write(type, 0, 'ascii');
    head.writeUInt32BE(png.length + 8, 4);
    return Buffer.concat([head, png]);
  });
  const body = Buffer.concat(chunks);
  const file = Buffer.alloc(8 + body.length);
  file.write('icns', 0, 'ascii');
  file.writeUInt32BE(file.length, 4);
  body.copy(file, 8);
  return file;
}
writeFileSync(join(iconsDir, 'icon.icns'), icnsFromPngs([
  { type: 'ic07', png: encodePng(128, render(128)) },
  { type: 'ic08', png: encodePng(256, render(256)) },
  { type: 'ic09', png: encodePng(512, render(512)) },
  { type: 'ic10', png: encodePng(1024, render(1024)) },
]));

console.log('图标已写入', iconsDir);
