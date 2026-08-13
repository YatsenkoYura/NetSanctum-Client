import fs from "node:fs";

const packagePath = "package.json";
const lockPath = "package-lock.json";
const cargoPath = "src-tauri/Cargo.toml";
const cargoLockPath = "src-tauri/Cargo.lock";
const tauriPath = "src-tauri/tauri.conf.json";

const command = process.argv[2] ?? "check";
const requested = process.argv[3]?.replace(/^v/, "");
const packageJson = readJson(packagePath);
const packageLock = readJson(lockPath);
const tauriConfig = readJson(tauriPath);
const cargoToml = fs.readFileSync(cargoPath, "utf8");
const cargoVersion = cargoToml.match(/^version = "([^"]+)"$/m)?.[1];
const cargoLock = fs.readFileSync(cargoLockPath, "utf8");
const cargoLockPattern = /(\[\[package\]\]\nname = "netsanctum-desktop"\nversion = ")[^"]+("\n)/;
const cargoLockVersion = cargoLock.match(cargoLockPattern)?.[0].match(/version = "([^"]+)"/)?.[1];

if (command === "set") {
  if (!requested || !isSemver(requested)) {
    throw new Error("Usage: npm run version:set -- X.Y.Z");
  }
  packageJson.version = requested;
  packageLock.version = requested;
  packageLock.packages[""].version = requested;
  tauriConfig.version = requested;
  writeJson(packagePath, packageJson);
  writeJson(lockPath, packageLock);
  writeJson(tauriPath, tauriConfig);
  fs.writeFileSync(
    cargoPath,
    cargoToml.replace(/^version = "[^"]+"$/m, `version = "${requested}"`),
  );
  fs.writeFileSync(
    cargoLockPath,
    cargoLock.replace(cargoLockPattern, (_match, prefix, suffix) => `${prefix}${requested}${suffix}`),
  );
  console.log(`Version set to ${requested}`);
} else if (command === "check") {
  const expected = requested ?? packageJson.version;
  if (!isSemver(expected)) throw new Error(`Invalid version: ${expected}`);
  const versions = {
    package: packageJson.version,
    packageLock: packageLock.version,
    packageLockRoot: packageLock.packages[""].version,
    cargo: cargoVersion,
    cargoLock: cargoLockVersion,
    tauri: tauriConfig.version,
  };
  const mismatches = Object.entries(versions).filter(([, value]) => value !== expected);
  if (mismatches.length) {
    console.error({ expected, versions });
    process.exit(1);
  }
  console.log(`Version ${expected} is synchronized`);
} else {
  throw new Error(`Unknown command: ${command}`);
}

function isSemver(value) {
  return /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(value);
}

function readJson(path) {
  return JSON.parse(fs.readFileSync(path, "utf8"));
}

function writeJson(path, value) {
  fs.writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}
