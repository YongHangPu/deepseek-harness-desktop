// 统一更新各配置文件中的版本号：
//   package.json、package-lock.json、src-tauri/tauri.conf.json、
//   src-tauri/Cargo.toml、src-tauri/Cargo.lock
//
// 用法：npm run bump-version -- <新版本号>
//      pnpm bump-version <新版本号>     （pnpm 也支持带 -- 的写法）
// 例如：npm run bump-version -- 0.2.0 / pnpm bump-version 0.2.0
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..');

// npm 会剥掉 "--"，pnpm 会原样传入；统一过滤掉，兼容两种包管理器。
const args = process.argv.slice(2).filter((arg) => arg !== '--');
const version = args[0];
if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error('用法：npm run bump-version -- <版本号>  或  pnpm bump-version <版本号>');
  console.error('例如：npm run bump-version -- 0.2.0 / pnpm bump-version 0.2.0');
  process.exit(1);
}

const updated = [];

// JSON 文件：直接改根级 version 字段（保持 2 空格缩进与现有键顺序）。
for (const file of ['package.json', 'package-lock.json', 'src-tauri/tauri.conf.json']) {
  const path = join(root, file);
  const json = JSON.parse(readFileSync(path, 'utf8'));
  if (typeof json.version !== 'string') throw new Error(`${file} 中没有 version 字段`);
  json.version = version;
  writeFileSync(path, JSON.stringify(json, null, 2) + '\n');
  updated.push(file);
}

// Cargo.toml：[package] 段的 version。
{
  const path = join(root, 'src-tauri', 'Cargo.toml');
  const text = readFileSync(path, 'utf8');
  if (!/^version\s*=\s*"[^"]*"/m.test(text)) throw new Error('Cargo.toml 中未找到 version 字段');
  writeFileSync(path, text.replace(/^version\s*=\s*"[^"]*"/m, `version = "${version}"`));
  updated.push('src-tauri/Cargo.toml');
}

// Cargo.lock：dsh-desktop 包条目的 version（下一次 cargo build 也会自动同步，这里先手动对齐）。
{
  const path = join(root, 'src-tauri', 'Cargo.lock');
  const text = readFileSync(path, 'utf8');
  const pattern = /(name\s*=\s*"dsh-desktop"\r?\nversion\s*=\s*")[^"]*(")/;
  if (!pattern.test(text)) throw new Error('Cargo.lock 中未找到 dsh-desktop 包条目');
  writeFileSync(path, text.replace(pattern, `$1${version}$2`));
  updated.push('src-tauri/Cargo.lock');
}

console.log(`版本号已更新为 ${version}：`);
for (const file of updated) console.log(`  - ${file}`);
console.log('');
console.log('发布提示（推送到 GitHub 后由 Actions 自动构建）：');
console.log('  git add .');
console.log(`  git commit -m "chore: release v${version}"`);
console.log(`  git tag v${version}`);
console.log('  git push');
console.log(`  git push origin v${version}`);
