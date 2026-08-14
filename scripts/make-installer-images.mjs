// 生成 MSI 安装器界面用到的品牌位图（WiX 要求 BMP，尺寸固定），
// 采用 DeepSeek 官方设计风格：白色底 + 主色蓝 #4D6BFE 的科技感元素：
//   - src-tauri/icons/installer-banner.bmp  （493×58，各页顶部横幅）
//   - src-tauri/icons/installer-dialog.bmp  （493×312，同时用作：错误/取消页大图，
//     以及自定义对话框的科技感背景 techBg）
// 注意：这两张图在打包时通过 Tauri 注入的绝对路径（bannerPath / dialogImagePath）
// 引用，因此交叉编译（x86 / arm64 等）也能正确解析，无需相对路径。
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..');
const iconsDir = join(root, 'src-tauri', 'icons');
mkdirSync(iconsDir, { recursive: true });

const clamp = (v, lo, hi) => (v < lo ? lo : v > hi ? hi : v);
const lerp = (a, b, t) => a + (b - a) * t;

// DeepSeek 主色蓝。
const DEEPSEEK_BLUE = [0x4d, 0x6b, 0xfe];
const DEEPSEEK_BLUE_LIGHT = [0x8f, 0xa8, 0xff];

// 圆角矩形覆盖度（约 1px 柔化边缘）。
function roundRectCoverage(px, py, x0, y0, x1, y1, r) {
  const cx = clamp(px, x0 + r, x1 - r);
  const cy = clamp(py, y0 + r, y1 - r);
  const d = Math.hypot(px - cx, py - cy) - r;
  return clamp(0.5 - d, 0, 1);
}

// 把自顶向下的 RGB 像素缓冲编码为 24 位 BMP（自底向上存储，BGR 顺序）。
function encodeBmp24(width, height, rgb) {
  const rowSize = Math.ceil((width * 3) / 4) * 4;
  const pixelDataSize = rowSize * height;
  const fileSize = 54 + pixelDataSize;
  const buf = Buffer.alloc(fileSize);
  buf.write('BM', 0, 'ascii');
  buf.writeUInt32LE(fileSize, 2);
  buf.writeUInt32LE(54, 10);
  buf.writeUInt32LE(40, 14);
  buf.writeInt32LE(width, 18);
  buf.writeInt32LE(height, 22);
  buf.writeUInt16LE(1, 26);
  buf.writeUInt16LE(24, 28);
  buf.writeUInt32LE(0, 30);
  buf.writeUInt32LE(pixelDataSize, 34);
  buf.writeInt32LE(2835, 38);
  buf.writeInt32LE(2835, 42);
  for (let y = height - 1; y >= 0; y--) {
    const rowOff = 54 + (height - 1 - y) * rowSize;
    for (let x = 0; x < width; x++) {
      const s = (y * width + x) * 3;
      const d = rowOff + x * 3;
      buf[d] = rgb[s + 2];
      buf[d + 1] = rgb[s + 1];
      buf[d + 2] = rgb[s];
    }
  }
  return buf;
}

// 顶部横幅（科技风）：白→极浅蓝渐变 + 隐约网格 + 右侧三条渐变竖条 + 底部 3px 主色蓝细线。
// 左侧保持干净：wixlib 系统对话框会把标题文字画在横幅上（深色文字需要浅底）。
function renderBanner() {
  const W = 493;
  const H = 58;
  const rgb = Buffer.alloc(W * H * 3);
  const top = [0xff, 0xff, 0xff];
  const bot = [0xf1, 0xf5, 0xfe];
  for (let y = 0; y < H; y++) {
    const t = y / (H - 1);
    for (let x = 0; x < W; x++) {
      const idx = (y * W + x) * 3;
      let r = lerp(top[0], bot[0], t);
      let g = lerp(top[1], bot[1], t);
      let b = lerp(top[2], bot[2], t);
      // 隐约网格（左半部分更淡，避免干扰 wixlib 对话框的标题文字）
      const gridCov = x % 26 === 0 || y % 14 === 0 ? (x < W * 0.62 ? 0.03 : 0.05) : 0;
      if (gridCov > 0) {
        r = lerp(r, DEEPSEEK_BLUE[0], gridCov);
        g = lerp(g, DEEPSEEK_BLUE[1], gridCov);
        b = lerp(b, DEEPSEEK_BLUE[2], gridCov);
      }
      rgb[idx] = Math.round(r);
      rgb[idx + 1] = Math.round(g);
      rgb[idx + 2] = Math.round(b);
    }
  }
  // 右侧三条渐变竖条（DSH 标识元素）
  const bars = [
    { x: 424, w: 14, h: 14 },
    { x: 444, w: 14, h: 26 },
    { x: 464, w: 14, h: 20 },
  ];
  for (const bar of bars) {
    for (let y = 0; y < H; y++) {
      for (let x = bar.x; x < bar.x + bar.w; x++) {
        const cov = roundRectCoverage(x + 0.5, y + 0.5, bar.x, H - 4 - bar.h, bar.x + bar.w, H - 4, bar.w / 2);
        if (cov <= 0) continue;
        const tx = (x - 424) / 54;
        const idx = (y * W + x) * 3;
        rgb[idx] = Math.round(lerp(rgb[idx], lerp(DEEPSEEK_BLUE[0], DEEPSEEK_BLUE_LIGHT[0], tx), cov * 0.75));
        rgb[idx + 1] = Math.round(lerp(rgb[idx + 1], lerp(DEEPSEEK_BLUE[1], DEEPSEEK_BLUE_LIGHT[1], tx), cov * 0.75));
        rgb[idx + 2] = Math.round(lerp(rgb[idx + 2], lerp(DEEPSEEK_BLUE[2], DEEPSEEK_BLUE_LIGHT[2], tx), cov * 0.75));
      }
    }
  }
  // 底部 3px 主色蓝细线
  for (let y = H - 3; y < H; y++) {
    for (let x = 0; x < W; x++) {
      const idx = (y * W + x) * 3;
      rgb[idx] = DEEPSEEK_BLUE[0];
      rgb[idx + 1] = DEEPSEEK_BLUE[1];
      rgb[idx + 2] = DEEPSEEK_BLUE[2];
    }
  }
  return rgb;
}

// 科技感背景图（493×312）：白底 + 隐约蓝色网格 + 左侧品牌竖线 + 右上/左下两处柔和光晕。
// 它同时用作：错误/取消页的大图、自定义对话框的整页背景（由模板拉伸到 440×290）。
function renderDialog() {
  const W = 493;
  const H = 312;
  const rgb = Buffer.alloc(W * H * 3);
  rgb.fill(0xff);
  const step = 22;
  for (let y = 0; y < H; y++) {
    const gridRow = y % step === 0;
    for (let x = 0; x < W; x++) {
      const idx = (y * W + x) * 3;
      let r = 0xff;
      let g = 0xff;
      let b = 0xff;
      if (gridRow || x % step === 0) {
        const cov = 0.04;
        r = lerp(r, DEEPSEEK_BLUE[0], cov);
        g = lerp(g, DEEPSEEK_BLUE[1], cov);
        b = lerp(b, DEEPSEEK_BLUE[2], cov);
      }
      if (x < 3) {
        const cov = 0.3;
        r = lerp(r, DEEPSEEK_BLUE[0], cov);
        g = lerp(g, DEEPSEEK_BLUE[1], cov);
        b = lerp(b, DEEPSEEK_BLUE[2], cov);
      }
      const d1 = Math.hypot(x - W, y - 0);
      const g1 = clamp(1 - d1 / 200, 0, 1) * 0.07;
      const d2 = Math.hypot(x - 0, y - H);
      const g2 = clamp(1 - d2 / 220, 0, 1) * 0.05;
      const glow = g1 + g2;
      if (glow > 0) {
        r = lerp(r, DEEPSEEK_BLUE[0], glow);
        g = lerp(g, DEEPSEEK_BLUE[1], glow);
        b = lerp(b, DEEPSEEK_BLUE[2], glow);
      }
      rgb[idx] = Math.round(r);
      rgb[idx + 1] = Math.round(g);
      rgb[idx + 2] = Math.round(b);
    }
  }
  return rgb;
}

writeFileSync(join(iconsDir, 'installer-banner.bmp'), encodeBmp24(493, 58, renderBanner()));
writeFileSync(join(iconsDir, 'installer-dialog.bmp'), encodeBmp24(493, 312, renderDialog()));
console.log('安装器位图已写入', iconsDir);
