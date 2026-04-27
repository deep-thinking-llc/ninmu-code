#!/usr/bin/env node
// Ninmu Code npm installer — downloads the pre-built binary for the current platform.
// This runs during `npm install -g ninmu` (or bun add -g ninmu).

const { createWriteStream, existsSync, mkdirSync } = require("fs");
const { chmod, rename, unlink } = require("fs/promises");
const https = require("https");
const path = require("path");
const os = require("os");

const REPO = "deep-thinking-llc/claw-code";
const VERSION = process.env.NINMU_VERSION || "latest";

function detectPlatform() {
  const platform = os.platform();
  const arch = os.arch();

  let osName;
  switch (platform) {
    case "darwin":
      osName = "macos";
      break;
    case "linux":
      osName = "linux";
      break;
    default:
      throw new Error(`Unsupported platform: ${platform}`);
  }

  let archName;
  switch (arch) {
    case "x64":
      archName = "x64";
      break;
    case "arm64":
      archName = "arm64";
      break;
    default:
      throw new Error(`Unsupported architecture: ${arch}`);
  }

  return `ninmu-${osName}-${archName}`;
}

async function getLatestTag() {
  return new Promise((resolve, reject) => {
    https
      .get(
        `https://api.github.com/repos/${REPO}/releases/latest`,
        { headers: { "User-Agent": "ninmu-installer" } },
        (res) => {
          let data = "";
          res.on("data", (chunk) => (data += chunk));
          res.on("end", () => {
            try {
              const json = JSON.parse(data);
              resolve(json.tag_name);
            } catch {
              reject(new Error("Failed to parse latest release response"));
            }
          });
        }
      )
      .on("error", reject);
  });
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    const file = createWriteStream(dest);
    https
      .get(url, { headers: { "User-Agent": "ninmu-installer" } }, (res) => {
        if (res.statusCode !== 200) {
          reject(new Error(`HTTP ${res.statusCode}: ${res.statusMessage}`));
          return;
        }
        res.pipe(file);
        file.on("finish", () => file.close(resolve));
      })
      .on("error", (err) => {
        file.close();
        reject(err);
      });
  });
}

async function main() {
  const artifact = detectPlatform();
  const binDir = path.join(__dirname, "bin");
  const binPath = path.join(binDir, "ninmu");

  if (!existsSync(binDir)) {
    mkdirSync(binDir, { recursive: true });
  }

  const tag = VERSION === "latest" ? await getLatestTag() : VERSION;
  const url = `https://github.com/${REPO}/releases/download/${tag}/${artifact}`;

  console.log(`Downloading ninmu ${tag} for ${os.platform()}/${os.arch()}...`);
  const tmpPath = binPath + ".tmp";
  await download(url, tmpPath);
  await chmod(tmpPath, 0o755);
  await rename(tmpPath, binPath);
  console.log(`Installed ninmu to ${binPath}`);
}

main().catch((err) => {
  console.error("Failed to install ninmu:", err.message);
  process.exit(1);
});
