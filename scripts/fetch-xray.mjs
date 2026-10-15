import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { unzipSync } from "fflate";

const version = "v26.9.30";
const assets = {
  "linux-x64": ["Xray-linux-64.zip", "f851110beaff16e78d643f0ccfd9524b4a44dfd59bae3e34bb52bba378f7690e"],
  "linux-arm64": ["Xray-linux-arm64-v8a.zip", "9886f077f9fd8e6713b84c377c1c7db4e53b9bfa8c276a5bd12561139522b473"],
  "darwin-x64": ["Xray-macos-64.zip", "1f366aaf21d3c3003d1556d066c4e08f9dd62d1f96555ecedde85b397df0f684"],
  "darwin-arm64": ["Xray-macos-arm64-v8a.zip", "4b363bd924df5bf09f87bd445755ff8e5742b9a8a0480cda261061416c3c7dce"],
  "win32-x64": ["Xray-windows-64.zip", "b17a619343c11b89d8faf36278298856749b15c52277d875d32051b33dcda617"],
  "win32-arm64": ["Xray-windows-arm64-v8a.zip", "5cf7ad4fd9aac5aa248ae7e598f3f0a0d0f79a6195089d611ddef9653830f1b0"],
};

const platformKey = `${process.platform}-${process.arch}`;
const asset = assets[platformKey];
if (!asset) throw new Error(`Xray ${platformKey} is not available in the pinned release`);

const [filename, expectedHash] = asset;
const downloadUrl = `https://github.com/XTLS/Xray-core/releases/download/${version}/${filename}`;
console.log(`Downloading official Xray core ${version} for ${platformKey}`);
const response = await fetch(downloadUrl, { redirect: "follow" });
if (!response.ok) throw new Error(`Xray download failed: HTTP ${response.status}`);
const archive = new Uint8Array(await response.arrayBuffer());
const actualHash = createHash("sha256").update(archive).digest("hex");
if (actualHash !== expectedHash) throw new Error("Xray archive SHA-256 mismatch; refusing to install it");

const contents = unzipSync(archive);
const binaryName = process.platform === "win32" ? "xray.exe" : "xray";
const binary = contents[binaryName];
if (!binary) throw new Error(`The official archive did not contain ${binaryName}`);

const scriptDir = fileURLToPath(new URL("../src-tauri/resources/engines/", import.meta.url));
await mkdir(scriptDir, { recursive: true });
await writeFile(join(scriptDir, binaryName), binary, { mode: 0o755 });
for (const dataFile of ["geoip.dat", "geosite.dat", "LICENSE"]) {
  if (contents[dataFile]) await writeFile(join(scriptDir, dataFile), contents[dataFile]);
}
console.log(`Verified and installed ${binaryName} (${actualHash})`);
