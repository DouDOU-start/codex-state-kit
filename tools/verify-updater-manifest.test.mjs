import { test } from 'node:test';
import assert from 'node:assert/strict';
import { verifyManifest, prepareManifest } from './verify-updater-manifest.mjs';

const repo = 'DouDOU-start/codex-state-kit';
function fixture() {
  const platforms = {};
  const assets = [];
  for (const [platform, name] of Object.entries({
    'windows-x86_64': 'app-windows-x64-setup.exe',
    'darwin-x86_64': 'app-darwin-x64.app.tar.gz',
    'darwin-aarch64': 'app-darwin-aarch64.app.tar.gz',
    'linux-x86_64': 'app-linux-amd64.AppImage',
  })) {
    platforms[platform] = { signature: 'test-signature', url: `https://github.com/${repo}/releases/download/v0.0.5/${name}` };
    assets.push({ name, size: 100 }, { name: `${name}.sig`, size: 100 });
  }
  return { manifest: { version: '0.0.5', platforms }, assets };
}
test('accepts complete signed four-platform release', () => {
  const { manifest, assets } = fixture();
  assert.doesNotThrow(() => verifyManifest(manifest, assets, 'v0.0.5', repo));
});
test('rejects missing platform, signature or artifact', () => {
  for (const mutate of [
    (m) => delete m.platforms['darwin-aarch64'],
    (m) => m.platforms['windows-x86_64'].signature = '',
    (_, a) => a.pop(),
  ]) {
    const { manifest, assets } = fixture(); mutate(manifest, assets);
    assert.throws(() => verifyManifest(manifest, assets, 'v0.0.5', repo));
  }
});
test('rejects mismatched versions and foreign downloads', () => {
  const { manifest, assets } = fixture();
  assert.throws(() => verifyManifest(manifest, assets, 'v0.0.6', repo));
  manifest.platforms['windows-x86_64'].url = 'https://evil.example/setup.exe';
  assert.throws(() => verifyManifest(manifest, assets, 'v0.0.5', repo));
});

test('converts draft API URLs using only assets belonging to this release', () => {
  const { manifest, assets } = fixture();
  const original = manifest.platforms['windows-x86_64'].url;
  assets[0].apiUrl = `https://api.github.com/repos/${repo}/releases/assets/123`;
  manifest.platforms['windows-x86_64'].url = assets[0].apiUrl;
  prepareManifest(manifest, assets, 'v0.0.5', repo);
  assert.equal(manifest.platforms['windows-x86_64'].url, original);
  manifest.platforms['windows-x86_64'].url = `https://api.github.com/repos/${repo}/releases/assets/999`;
  assert.throws(() => prepareManifest(manifest, assets, 'v0.0.5', repo));
});
