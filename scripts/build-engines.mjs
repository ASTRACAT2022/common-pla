import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { chmod, copyFile, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const engines = join(root, "src-tauri/resources/engines");
const toolsVersion = "v3.1.20260812";
const goVersion = "v3.1.20260828";
const windowsClientVersion = "3.1.0";

await mkdir(engines, { recursive: true });

function run(binary, args, cwd, env = {}) {
  const result = spawnSync(binary, args, { cwd, env: { ...process.env, ...env }, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${binary} exited with status ${result.status}`);
}

async function clone(repository, reference, destination) {
  run("git", ["clone", "--depth", "1", "--branch", reference, repository, destination], root);
}

async function buildUnixAwg(temp) {
  const tools = join(temp, "amneziawg-tools");
  const goSource = join(temp, "amneziawg-go");
  await clone("https://github.com/amnezia-vpn/amneziawg-tools.git", toolsVersion, tools);
  await clone("https://github.com/amnezia-vpn/amneziawg-go.git", goVersion, goSource);
  run("make", ["WITH_WGQUICK=yes", "WITH_BASHCOMPLETION=no", "WITH_SYSTEMDUNITS=no", "-j2"], join(tools, "src"));

  const quickName = process.platform === "darwin" ? "darwin.bash" : "linux.bash";
  await copyFile(join(tools, "src/wg"), join(engines, "awg"));
  await copyFile(join(tools, `src/wg-quick/${quickName}`), join(engines, "awg-quick"));
  await chmod(join(engines, "awg"), 0o755);
  await chmod(join(engines, "awg-quick"), 0o755);

  run("go", ["build", "-trimpath", "-buildvcs=false", "-o", join(engines, "amneziawg-go"), "."], goSource, { CGO_ENABLED: "0" });
  await chmod(join(engines, "amneziawg-go"), 0o755);
  await copyFile(join(tools, "COPYING"), join(engines, "AMNEZIAWG-TOOLS-COPYING"));
  await copyFile(join(goSource, "LICENSE"), join(engines, "AMNEZIAWG-GO-LICENSE"));

  if (process.platform === "darwin") await buildMacBash(temp);
}

async function buildMacBash(temp) {
  const archivePath = join(temp, "bash-5.3.tar.gz");
  const response = await fetch("https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz");
  if (!response.ok) throw new Error(`GNU Bash download failed: HTTP ${response.status}`);
  const archive = new Uint8Array(await response.arrayBuffer());
  const expected = "0d5cd86965f869a26cf64f4b71be7b96f90a3ba8b3d74e27e8e9d9d5550f31ba";
  const actual = createHash("sha256").update(archive).digest("hex");
  if (actual !== expected) throw new Error("GNU Bash SHA-256 mismatch; refusing to build it");
  await writeFile(archivePath, archive);
  run("tar", ["-xzf", archivePath, "-C", temp], root);
  const source = join(temp, "bash-5.3");
  run("./configure", ["--disable-readline", "--disable-history", "--without-bash-malloc", `--prefix=${temp}/bash-install`], source);
  run("make", ["-j2"], source);
  await copyFile(join(source, "bash"), join(engines, "bash"));
  await copyFile(join(source, "COPYING"), join(engines, "BASH-COPYING"));
  await chmod(join(engines, "bash"), 0o755);
}

async function buildWindowsAwg(temp) {
  if (process.platform !== "win32") throw new Error("Build the Windows AmneziaWG engine on a Windows runner");
  const client = join(temp, "amneziawg-windows-client");
  await clone("https://github.com/amnezia-vpn/amneziawg-windows-client.git", windowsClientVersion, client);
  run("cmd.exe", ["/c", "build.bat"], client);
  const platform = process.arch === "arm64" ? "arm64" : "amd64";
  for (const filename of ["amneziawg.exe", "awg.exe", "wintun.dll"]) {
    await copyFile(join(client, platform, filename), join(engines, filename));
  }
  await copyFile(join(client, "COPYING"), join(engines, "AMNEZIAWG-WINDOWS-COPYING"));
  await copyFile(join(client, ".deps/src/COPYING"), join(engines, "AMNEZIAWG-TOOLS-COPYING"));
  const goSource = join(temp, "amneziawg-go-license");
  await clone("https://github.com/amnezia-vpn/amneziawg-go.git", goVersion, goSource);
  await copyFile(join(goSource, "LICENSE"), join(engines, "AMNEZIAWG-GO-LICENSE"));
}

const temp = await mkdtemp(join(tmpdir(), "commonnet-engines-"));
try {
  if (process.platform === "win32") {
    await buildWindowsAwg(temp);
  } else if (process.platform === "linux" || process.platform === "darwin") {
    await buildUnixAwg(temp);
  } else {
    throw new Error(`No AmneziaWG engine build configured for ${process.platform}`);
  }
} finally {
  try {
    await rm(temp, { recursive: true, force: true, maxRetries: 8, retryDelay: 1000 });
  } catch (error) {
    if (
      process.platform !== "win32" ||
      !["EPERM", "EBUSY", "ENOTEMPTY", "EACCES"].includes(error.code)
    ) {
      throw error;
    }
    console.warn("Windows still holds a temporary AmneziaWG build file; the runner will clean it up");
  }
}

console.log(`Built AmneziaWG engines for ${process.platform}-${process.arch}`);
