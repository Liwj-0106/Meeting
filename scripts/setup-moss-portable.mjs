#!/usr/bin/env node

import { execFileSync, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const SOURCE_REVISION = "cb765f2b0fe6f7a298aa2002e2281ae693d1f3c3";
const SOURCE_REPOSITORY = "OpenMOSS/MOSS-Transcribe-Diarize";
const SOURCE_URL = "https://github.com/OpenMOSS/MOSS-Transcribe-Diarize.git";
const MODEL_REVISION = "902e98bcb3db33ac913d3496127b92a8d81f2daa";
const WINDOWS_AV_WHEEL = {
  version: "15.0.0",
  filename: "av-15.0.0-cp312-cp312-win_amd64.whl",
  sha256: "383f1b57520d790069d85fc75f43cfa32fca07f5fb3fb842be37bd596638602c",
};
const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.resolve(scriptDir, "..");
const portableRoot = path.join(projectRoot, "portable");
const envPrefix = path.join(portableRoot, "conda-envs", "moss-td");
const condaPkgs = path.join(portableRoot, "conda-pkgs");
const modelRoot = path.join(
  portableRoot,
  "app-data",
  "models",
  "moss-transcribe-diarize",
);
const sourceRoot = path.join(portableRoot, "sources", "MOSS-Transcribe-Diarize");
const setupRoot = path.join(portableRoot, "app-data", "cache", "moss-setup");
const requestedSessionTemp = process.env.MEETILY_SESSION_TEMP;
const setupTemp = requestedSessionTemp
  ? path.join(path.resolve(requestedSessionTemp), "moss-setup")
  : path.join(portableRoot, "app-data", "temp", "moss-setup");
const isolatedProfile = path.join(setupRoot, "user-profile");
const isolatedAppData = path.join(isolatedProfile, "AppData", "Roaming");
const isolatedLocalAppData = path.join(isolatedProfile, "AppData", "Local");
const condarc = path.join(setupRoot, "condarc.yml");
const requirements = path.join(scriptDir, "moss-runtime-requirements.txt");
const downloader = path.join(scriptDir, "download-moss-model.py");
const args = new Set(process.argv.slice(2));
const dryRun = args.delete("--dry-run");
const envOnly = args.delete("--env-only");
const modelOnly = args.delete("--model-only");
const trustedSourceFiles = {
  LICENSE:
    "c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4",
  "moss_transcribe_diarize/inference_utils.py":
    "1d97700b83ed95438be2e3a59529b3726d7bf7f7cdd488ad2097180bad15b86b",
  "moss_transcribe_diarize/transcript_parser.py":
    "475c564edc128afde69ae27f1fe8575412b1d6ac8f9d6be38b6c7f4bd16b8412",
};
const sourceManifest = path.join(sourceRoot, "meetily-source-manifest.json");
const verifiedWheelRoot = path.join(setupTemp, "verified-wheels");

if (args.size || (envOnly && modelOnly)) {
  throw new Error("usage: node scripts/setup-moss-portable.mjs [--dry-run] [--env-only|--model-only]");
}

function nearestExisting(candidate) {
  let current = path.resolve(candidate);
  while (!fs.existsSync(current)) {
    const parent = path.dirname(current);
    if (parent === current) throw new Error("path has no existing ancestor");
    current = parent;
  }
  return current;
}

function isWithin(candidate, root) {
  const relative = path.relative(root, candidate);
  return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
}

function assertNonSystemPath(candidate, label, withinPortable = true) {
  const resolved = path.resolve(candidate);
  if (/^(\\\\[?.]\\|\\\\)/.test(resolved)) {
    throw new Error(`${label} cannot use a device or UNC path`);
  }
  if (path.parse(resolved).root.toUpperCase() === "C:\\") {
    throw new Error(`${label} cannot resolve to the system drive`);
  }
  const canonicalAncestor = fs.realpathSync.native(nearestExisting(resolved));
  const canonicalPortable = fs.realpathSync.native(portableRoot);
  if (path.parse(canonicalAncestor).root.toUpperCase() === "C:\\") {
    throw new Error(`${label} has an existing ancestor on the system drive`);
  }
  if (withinPortable && !isWithin(canonicalAncestor, canonicalPortable)) {
    throw new Error(`${label} escapes the canonical portable root`);
  }
  return resolved;
}

for (const [candidate, label] of [
  [portableRoot, "portable root"],
  [envPrefix, "Conda environment"],
  [condaPkgs, "Conda package cache"],
  [modelRoot, "model"],
  [sourceRoot, "official source"],
  [setupRoot, "setup cache"],
]) {
  assertNonSystemPath(candidate, label);
}
assertNonSystemPath(setupTemp, "setup temp", !requestedSessionTemp);

if (!fs.existsSync(requirements) || !fs.existsSync(downloader)) {
  throw new Error("reviewed setup inputs are missing");
}

function sha256(file) {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

function reviewedTextSha256(file) {
  const normalized = fs.readFileSync(file, "utf8").replaceAll("\r\n", "\n");
  if (normalized.includes("\r")) {
    throw new Error("reviewed source contains a non-canonical carriage return");
  }
  return crypto.createHash("sha256").update(normalized, "utf8").digest("hex");
}

function findConda() {
  const candidates = [
    process.env.MEETILY_CONDA_EXE,
    "D:\\ProgramData\\Anaconda3\\Scripts\\conda.exe",
    "D:\\ProgramData\\Anaconda3\\condabin\\conda.bat",
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      return assertNonSystemPath(candidate, "Conda executable", false);
    }
  }
  throw new Error("no D-resident Conda executable was found");
}

const conda = findConda();
const python = path.join(envPrefix, "python.exe");
const cacheRoot = path.join(portableRoot, "app-data", "cache");
const inheritedEnvironment = {};
for (const name of [
  "PATH",
  "Path",
  "SystemRoot",
  "WINDIR",
  "COMSPEC",
  "PATHEXT",
  "CUDA_PATH",
  "CUDA_HOME",
  "HTTP_PROXY",
  "HTTPS_PROXY",
  "NO_PROXY",
  "REQUESTS_CA_BUNDLE",
  "SSL_CERT_FILE",
]) {
  if (process.env[name] !== undefined) inheritedEnvironment[name] = process.env[name];
}
const childEnv = {
  ...inheritedEnvironment,
  CONDARC: condarc,
  CONDA_PKGS_DIRS: condaPkgs,
  CONDA_ENVS_PATH: path.join(portableRoot, "conda-envs"),
  PIP_CACHE_DIR: path.join(cacheRoot, "pip"),
  PIP_CONFIG_FILE: os.devNull,
  PIP_DISABLE_PIP_VERSION_CHECK: "1",
  PYTHONNOUSERSITE: "1",
  PYTHONDONTWRITEBYTECODE: "1",
  PYTHONPYCACHEPREFIX: path.join(cacheRoot, "python-pycache"),
  HF_HOME: path.join(cacheRoot, "huggingface"),
  HF_HUB_CACHE: path.join(cacheRoot, "huggingface", "hub"),
  HUGGINGFACE_HUB_CACHE: path.join(cacheRoot, "huggingface", "hub"),
  HF_XET_CACHE: path.join(cacheRoot, "huggingface", "xet"),
  HF_HUB_DISABLE_XET: "1",
  HF_HUB_DISABLE_TELEMETRY: "1",
  TRANSFORMERS_CACHE: path.join(cacheRoot, "huggingface", "transformers"),
  TORCH_HOME: path.join(cacheRoot, "torch"),
  TORCHINDUCTOR_CACHE_DIR: path.join(cacheRoot, "torch-inductor"),
  TORCH_EXTENSIONS_DIR: path.join(cacheRoot, "torch-extensions"),
  NUMBA_CACHE_DIR: path.join(cacheRoot, "numba"),
  TRITON_CACHE_DIR: path.join(cacheRoot, "triton"),
  CUDA_CACHE_PATH: path.join(cacheRoot, "cuda"),
  UV_CACHE_DIR: path.join(cacheRoot, "uv"),
  XDG_CACHE_HOME: cacheRoot,
  TEMP: setupTemp,
  TMP: setupTemp,
  USERPROFILE: isolatedProfile,
  APPDATA: isolatedAppData,
  LOCALAPPDATA: isolatedLocalAppData,
  MEETILY_MOSS_DOWNLOAD_DIR: path.join(setupTemp, "downloads"),
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_CONFIG_GLOBAL: path.join(setupRoot, "gitconfig"),
};

for (const [name, value] of Object.entries(childEnv)) {
  if (name.endsWith("CACHE") || name.endsWith("CACHE_DIR") || ["TEMP", "TMP", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "MEETILY_MOSS_DOWNLOAD_DIR"].includes(name)) {
    if (typeof value === "string" && path.isAbsolute(value)) {
      const externalSessionTemp = requestedSessionTemp && ["TEMP", "TMP", "MEETILY_MOSS_DOWNLOAD_DIR"].includes(name);
      assertNonSystemPath(value, `environment ${name}`, !externalSessionTemp);
    }
  }
}

function run(executable, commandArgs) {
  const rendered = [executable, ...commandArgs].map((value) => JSON.stringify(value)).join(" ");
  if (dryRun) {
    console.log(`dry_run=${rendered}`);
    return;
  }
  const result = spawnSync(executable, commandArgs, {
    cwd: projectRoot,
    env: childEnv,
    stdio: "inherit",
    windowsHide: true,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`command failed with status ${result.status}`);
}

function capture(executable, commandArgs) {
  const result = execFileSync(executable, commandArgs, {
    cwd: projectRoot,
    env: childEnv,
    encoding: "utf8",
    windowsHide: true,
  });
  return result.trim();
}

function verifyReviewedSource(candidate) {
  const revision = capture("git", ["-C", candidate, "rev-parse", "HEAD"]);
  if (revision !== SOURCE_REVISION) {
    throw new Error("official source revision differs from the reviewed revision");
  }
  const remote = capture("git", ["-C", candidate, "remote", "get-url", "origin"]);
  if (remote.replace(/\\/g, "/").replace(/\/$/, "") !== SOURCE_URL.replace(/\/$/, "")) {
    throw new Error("official source remote differs from the reviewed repository");
  }
  for (const [relative, expected] of Object.entries(trustedSourceFiles)) {
    const sourceFile = path.join(candidate, ...relative.split("/"));
    if (!fs.existsSync(sourceFile) || reviewedTextSha256(sourceFile) !== expected) {
      throw new Error("official source contents differ from the reviewed revision");
    }
  }
}

function writeSourceManifest() {
  fs.writeFileSync(
    sourceManifest,
    JSON.stringify(
      {
        schema: 1,
        repository: SOURCE_REPOSITORY,
        revision: SOURCE_REVISION,
        files: trustedSourceFiles,
      },
      null,
      2,
    ) + "\n",
    "utf8",
  );
}

function provisionReviewedSource() {
  if (fs.existsSync(sourceRoot)) {
    if (!fs.statSync(sourceRoot).isDirectory() || !fs.existsSync(path.join(sourceRoot, ".git"))) {
      throw new Error("portable source target exists but is not the reviewed Git checkout");
    }
    verifyReviewedSource(sourceRoot);
    if (!dryRun) writeSourceManifest();
    return;
  }

  const sourceStage = path.join(setupTemp, `source-clone-${process.pid}`);
  if (dryRun) {
    run("git", ["clone", "--filter=blob:none", "--no-checkout", "--no-tags", SOURCE_URL, sourceStage]);
    run("git", ["-C", sourceStage, "fetch", "--depth", "1", "origin", SOURCE_REVISION]);
    run("git", ["-C", sourceStage, "checkout", "--detach", "FETCH_HEAD"]);
    console.log(`source_install=${sourceRoot}`);
    return;
  }

  fs.mkdirSync(path.dirname(sourceRoot), { recursive: true });
  if (fs.existsSync(sourceStage)) {
    throw new Error("source staging directory already exists; inspect it before retrying");
  }
  try {
    run("git", ["clone", "--filter=blob:none", "--no-checkout", "--no-tags", SOURCE_URL, sourceStage]);
    run("git", ["-C", sourceStage, "fetch", "--depth", "1", "origin", SOURCE_REVISION]);
    run("git", ["-C", sourceStage, "checkout", "--detach", "FETCH_HEAD"]);
    verifyReviewedSource(sourceStage);
    fs.renameSync(sourceStage, sourceRoot);
    writeSourceManifest();
  } catch (error) {
    if (fs.existsSync(sourceStage) && isWithin(path.resolve(sourceStage), path.resolve(setupTemp))) {
      fs.rmSync(sourceStage, { recursive: true, force: true });
    }
    throw error;
  }
}

function parsePinnedRequirements(contents) {
  const pinned = {};
  for (const raw of contents.split(/\r?\n/)) {
    const line = raw.replace(/\s+#.*$/, "").trim();
    if (!line || line.startsWith("#")) continue;
    const match = /^([A-Za-z0-9_.-]+)==([^\s]+)$/.exec(line);
    if (!match) throw new Error(`runtime dependency is not exactly pinned: ${line}`);
    pinned[match[1].toLowerCase().replace(/[_.]+/g, "-")] = match[2];
  }
  return pinned;
}

const directDependencies = {
  ...parsePinnedRequirements(fs.readFileSync(requirements, "utf8")),
  torch: "2.8.0+cu128",
  torchaudio: "2.8.0+cu128",
};

if (dryRun) {
  console.log(`portable_root=${portableRoot}`);
  console.log(`environment_prefix=${envPrefix}`);
  console.log(`conda_package_cache=${condaPkgs}`);
  console.log(`model_root=${modelRoot}`);
  console.log(`model_revision=${MODEL_REVISION}`);
} else {
  for (const directory of [
    path.dirname(envPrefix),
    condaPkgs,
    setupRoot,
    setupTemp,
    isolatedProfile,
    isolatedAppData,
    isolatedLocalAppData,
    childEnv.PIP_CACHE_DIR,
    childEnv.HF_HOME,
    childEnv.HF_XET_CACHE,
    childEnv.TORCH_HOME,
    childEnv.TORCHINDUCTOR_CACHE_DIR,
    childEnv.TORCH_EXTENSIONS_DIR,
    childEnv.NUMBA_CACHE_DIR,
    childEnv.TRITON_CACHE_DIR,
    childEnv.CUDA_CACHE_PATH,
    childEnv.UV_CACHE_DIR,
    childEnv.PYTHONPYCACHEPREFIX,
    childEnv.MEETILY_MOSS_DOWNLOAD_DIR,
    verifiedWheelRoot,
  ]) {
    fs.mkdirSync(directory, { recursive: true });
    const canonicalDirectory = fs.realpathSync.native(directory);
    const isSessionTemporary = requestedSessionTemp
      ? isWithin(canonicalDirectory, fs.realpathSync.native(requestedSessionTemp))
      : false;
    assertNonSystemPath(
      canonicalDirectory,
      "created setup directory",
      !isSessionTemporary,
    );
  }
  fs.writeFileSync(
    condarc,
    [
      "channels:",
      "  - conda-forge",
      "channel_priority: strict",
      "pkgs_dirs:",
      `  - ${condaPkgs.replaceAll("\\", "/")}`,
      "envs_dirs:",
      `  - ${path.join(portableRoot, "conda-envs").replaceAll("\\", "/")}`,
      "auto_activate_base: false",
      "show_channel_urls: true",
      "",
    ].join("\n"),
    "utf8",
  );
}

provisionReviewedSource();

if (!modelOnly) {
  if (!fs.existsSync(python)) {
    run(conda, [
      "create",
      "--yes",
      "--prefix",
      envPrefix,
      "--override-channels",
      "--channel",
      "conda-forge",
      "python=3.12",
      "pip",
    ]);
  }
  run(python, ["-m", "pip", "install", "--index-url", "https://download.pytorch.org/whl/cu128", "torch==2.8.0", "torchaudio==2.8.0"]);
  const verifiedAvWheel = path.join(verifiedWheelRoot, WINDOWS_AV_WHEEL.filename);
  if (dryRun || !fs.existsSync(verifiedAvWheel)) {
    run(python, [
      "-m",
      "pip",
      "download",
      "--index-url",
      "https://pypi.org/simple",
      "--only-binary=:all:",
      "--no-deps",
      "--dest",
      verifiedWheelRoot,
      `av==${WINDOWS_AV_WHEEL.version}`,
    ]);
  }
  if (!dryRun) {
    if (!fs.existsSync(verifiedAvWheel) || sha256(verifiedAvWheel) !== WINDOWS_AV_WHEEL.sha256) {
      throw new Error("downloaded PyAV wheel does not match the reviewed Windows wheel");
    }
  }
  run(python, [
    "-m",
    "pip",
    "install",
    "--no-deps",
    "--force-reinstall",
    verifiedAvWheel,
  ]);
  run(python, [
    "-m",
    "pip",
    "install",
    "--only-binary",
    "av",
    "--requirement",
    requirements,
  ]);
  run(python, ["-m", "pip", "check"]);
  if (!dryRun) {
    const freeze = execFileSync(python, ["-m", "pip", "freeze", "--all"], {
      cwd: projectRoot,
      env: childEnv,
      encoding: "utf8",
      windowsHide: true,
    });
    const directNames = Object.keys(directDependencies);
    const versionProbe = [
      "import importlib.metadata as metadata, json",
      `names = ${JSON.stringify(directNames)}`,
      "print(json.dumps({name: metadata.version(name) for name in names}, sort_keys=True))",
    ].join("; ");
    const installed = JSON.parse(capture(python, ["-c", versionProbe]));
    for (const [name, expectedVersion] of Object.entries(directDependencies)) {
      if (installed[name] !== expectedVersion) {
        throw new Error(`installed direct dependency mismatch: ${name}`);
      }
    }
    const freezePath = path.join(envPrefix, "meetily-runtime-freeze.txt");
    fs.writeFileSync(freezePath, freeze, "utf8");
    fs.writeFileSync(
      path.join(envPrefix, "meetily-runtime-manifest.json"),
      JSON.stringify(
        {
          schema: 2,
          source_revision: SOURCE_REVISION,
          model_revision: MODEL_REVISION,
          direct_dependencies: directDependencies,
          direct_dependencies_verified: true,
          transitive_dependencies_fully_hash_locked: false,
          freeze_sha256: sha256(freezePath),
          windows_av_wheel: WINDOWS_AV_WHEEL,
          windows_av_wheel_verified_before_install: true,
        },
        null,
        2,
      ) + "\n",
      "utf8",
    );
  }
}

if (!envOnly) {
  if (!dryRun && !fs.existsSync(python)) {
    throw new Error("portable MOSS environment is not installed");
  }
  run(python, [downloader, portableRoot, modelRoot]);
}

console.log("moss_portable_setup=complete");
