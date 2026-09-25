import { readFileSync, writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

export function verifyManifest(manifest, assets, tag, repository) {
  if (manifest.version?.replace(/^v/, '') !== tag.replace(/^v/, '')) throw new Error('更新清单版本不匹配');
  const required = {
    'windows-x86_64': '.exe',
    'darwin-x86_64': '.app.tar.gz',
    'darwin-aarch64': '.app.tar.gz',
    'linux-x86_64': '.AppImage.tar.gz',
  };
  for (const [platform, extension] of Object.entries(required)) {
    const entry = manifest.platforms?.[platform];
    if (!entry?.signature?.trim()) throw new Error(`${platform} 缺少签名`);
    const url = new URL(entry.url);
    const prefix = `/${repository}/releases/download/${tag}/`;
    if (url.origin !== 'https://github.com' || !url.pathname.startsWith(prefix) || url.search || url.hash) throw new Error(`${platform} 更新地址不属于当前发布`);
    const name = decodeURIComponent(url.pathname.slice(prefix.length));
    if (!name.endsWith(extension)) throw new Error(`${platform} 更新包类型错误`);
    for (const assetName of [name, `${name}.sig`]) {
      if (!assets.some((asset) => asset.name === assetName && asset.size > 0)) throw new Error(`缺少更新资源 ${assetName}`);
    }
  }
}

// tauri-action uses API asset URLs while the release is still a draft.
// Resolve only IDs belonging to this release before exposing a public updater feed.
export function prepareManifest(manifest, assets, tag, repository) {
  const prefix = `https://api.github.com/repos/${repository}/releases/assets/`;
  for (const entry of Object.values(manifest.platforms ?? {})) {
    if (!entry.url?.startsWith(prefix)) continue;
    const assetId = entry.url.slice(prefix.length);
    if (!/^\d+$/.test(assetId)) throw new Error('无效的草稿资源地址');
    const asset = assets.find((asset) => asset.apiUrl === entry.url || String(asset.id) === assetId);
    if (!asset) throw new Error('草稿资源不属于当前发布');
    entry.url = `https://github.com/${repository}/releases/download/${tag}/${encodeURIComponent(asset.name)}`;
  }
  verifyManifest(manifest, assets, tag, repository);
  return manifest;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [manifestFile, assetsFile, tag, repository] = process.argv.slice(2);
  const manifest = prepareManifest(JSON.parse(readFileSync(manifestFile, 'utf8')), JSON.parse(readFileSync(assetsFile, 'utf8')).assets, tag, repository);
  writeFileSync(manifestFile, `${JSON.stringify(manifest, null, 2)}\n`);
  console.log('Windows x64、macOS Intel/Apple Silicon、Linux x64 更新清单验证通过');
}
