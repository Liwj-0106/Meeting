import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.resolve(scriptDir, "..");
const portableRoot = path.join(projectRoot, "portable");
const stageRoot = path.resolve(process.env.MEETILY_SESSION_TEMP ?? "");
if (!process.env.MEETILY_SESSION_TEMP || path.parse(stageRoot).root.toUpperCase() === "C:\\") {
  throw new Error("MEETILY_SESSION_TEMP must be an absolute non-C-drive validation directory");
}
const audioRoot = path.join(portableRoot, "app-data", "cache", "moss-smoke");
const audioPath = path.join(audioRoot, "sapi-two-speaker-48k.wav");
const silencePath = path.join(audioRoot, "silence-48k.wav");

function wavMetadata(file) {
  const bytes = fs.readFileSync(file);
  if (bytes.toString("ascii", 0, 4) !== "RIFF" || bytes.toString("ascii", 8, 12) !== "WAVE") {
    throw new Error("smoke input must be a RIFF/WAVE file");
  }
  let offset = 12;
  let sampleRate = null;
  let blockAlign = null;
  let dataBytes = null;
  while (offset + 8 <= bytes.length) {
    const name = bytes.toString("ascii", offset, offset + 4);
    const size = bytes.readUInt32LE(offset + 4);
    if (name === "fmt ") {
      sampleRate = bytes.readUInt32LE(offset + 12);
      blockAlign = bytes.readUInt16LE(offset + 20);
    } else if (name === "data") {
      dataBytes = size;
    }
    offset += 8 + size + (size % 2);
  }
  if (sampleRate !== 48_000 || !blockAlign || dataBytes === null || dataBytes % blockAlign !== 0) {
    throw new Error("smoke input must be canonical 48 kHz PCM");
  }
  const frames = dataBytes / blockAlign;
  return { frames, duration_seconds: frames / sampleRate };
}

const metadata = wavMetadata(audioPath);
const silenceMetadata = wavMetadata(silencePath);
const canonicalPortable = fs.realpathSync.native(portableRoot);
for (const file of [audioPath, silencePath]) {
  const canonical = fs.realpathSync.native(file);
  const relative = path.relative(canonicalPortable, canonical);
  if (relative.startsWith("..") || path.isAbsolute(relative)) {
    throw new Error("smoke audio must remain inside the portable root");
  }
}
let existingStageAncestor = stageRoot;
while (!fs.existsSync(existingStageAncestor)) {
  const parent = path.dirname(existingStageAncestor);
  if (parent === existingStageAncestor) throw new Error("validation directory has no existing ancestor");
  existingStageAncestor = parent;
}
if (path.parse(fs.realpathSync.native(existingStageAncestor)).root.toUpperCase() === "C:\\") {
  throw new Error("validation directory resolves to the system drive");
}
metadata.turns = [0, Math.floor(metadata.frames / 3), Math.floor((metadata.frames * 2) / 3)].map(
  (start_frame) => ({ start_frame }),
);
const python = path.join(portableRoot, "conda-envs", "moss-td", "python.exe");
const worker = path.join(projectRoot, "frontend", "src-tauri", "workers", "moss_worker.py");
const modelRoot = path.join(portableRoot, "app-data", "models", "moss-transcribe-diarize");
const modelRevision = "902e98bcb3db33ac913d3496127b92a8d81f2daa";
const outputPath = path.join(stageRoot, "moss-real-smoke.json");
const evaluationInputPath = path.join(stageRoot, "moss-real-evaluation-input.json");
const evaluationReportPath = path.join(stageRoot, "moss-real-evaluation-report.json");
const runtimeProfile = path.join(portableRoot, "app-data", "cache", "moss-runtime", "user-profile");
const runtimeCache = path.join(portableRoot, "app-data", "cache", "moss-runtime");

fs.mkdirSync(audioRoot, { recursive: true });
fs.mkdirSync(path.join(runtimeProfile, "AppData", "Roaming"), { recursive: true });
fs.mkdirSync(path.join(runtimeProfile, "AppData", "Local"), { recursive: true });
fs.mkdirSync(stageRoot, { recursive: true });

const cases = [
  { id: "speech", job: "smoke-speech", audio: audioPath, frames: metadata.frames },
  { id: "silence", job: "smoke-silence", audio: silencePath, frames: silenceMetadata.frames },
];

const env = {
  SystemRoot: process.env.SystemRoot,
  WINDIR: process.env.WINDIR,
  COMSPEC: process.env.COMSPEC,
  PATH: `${path.dirname(python)};${path.join(path.dirname(python), "Library", "bin")};${path.join(process.env.SystemRoot, "System32")}`,
  TEMP: path.join(runtimeCache, "temp"),
  TMP: path.join(runtimeCache, "temp"),
  USERPROFILE: runtimeProfile,
  HOME: runtimeProfile,
  APPDATA: path.join(runtimeProfile, "AppData", "Roaming"),
  LOCALAPPDATA: path.join(runtimeProfile, "AppData", "Local"),
  HF_HOME: path.join(runtimeCache, "huggingface"),
  HF_HUB_CACHE: path.join(runtimeCache, "huggingface", "hub"),
  HF_XET_CACHE: path.join(runtimeCache, "huggingface", "xet"),
  TRANSFORMERS_CACHE: path.join(runtimeCache, "transformers"),
  TORCH_HOME: path.join(runtimeCache, "torch"),
  NUMBA_CACHE_DIR: path.join(runtimeCache, "numba"),
  TRITON_CACHE_DIR: path.join(runtimeCache, "triton"),
  CUDA_CACHE_PATH: path.join(runtimeCache, "cuda"),
  XDG_CACHE_HOME: path.join(runtimeCache, "xdg"),
  MPLCONFIGDIR: path.join(runtimeCache, "matplotlib"),
  PYTHONPYCACHEPREFIX: path.join(runtimeCache, "pycache"),
  PYTHONNOUSERSITE: "1",
  PYTHONUTF8: "1",
  PYTHONIOENCODING: "utf-8",
  TOKENIZERS_PARALLELISM: "false",
  HF_HUB_OFFLINE: "1",
  HF_DATASETS_OFFLINE: "1",
  TRANSFORMERS_OFFLINE: "1",
  MEETILY_MOSS_BENCHMARK_METRICS: "1",
};
for (const value of Object.values(env)) {
  if (typeof value === "string" && /^D:\\/.test(value) && !value.includes(";")) {
    fs.mkdirSync(path.extname(value) ? path.dirname(value) : value, { recursive: true });
  }
}

const start = performance.now();
const child = spawn(
  python,
  [
    "-I",
    "-u",
    worker,
    "--portable-root",
    portableRoot,
    "--model-root",
    modelRoot,
    "--audio-root",
    audioRoot,
    "--model-revision",
    modelRevision,
  ],
  { cwd: path.dirname(worker), env, stdio: ["pipe", "pipe", "pipe"], windowsHide: true },
);

let handshakeMs = null;
let handshakeRuntime = null;
let caseIndex = -1;
const caseResults = [];
let stderr = "";
let stdoutBuffer = "";
let peakRamBytes = 0;
let sampledProcessVramBytes = 0;
let sentExecuteAt = null;
let finished = false;

function send(value) {
  child.stdin.write(`${JSON.stringify(value)}\n`);
}

function parseCsv(line) {
  const fields = [];
  let field = "";
  let quoted = false;
  for (let i = 0; i < line.length; i += 1) {
    const character = line[i];
    if (character === '"') quoted = !quoted;
    else if (character === "," && !quoted) {
      fields.push(field);
      field = "";
    } else field += character;
  }
  fields.push(field);
  return fields;
}

function sampleResources() {
  const task = spawnSync(
    path.join(process.env.SystemRoot, "System32", "tasklist.exe"),
    ["/fi", `PID eq ${child.pid}`, "/fo", "csv", "/nh"],
    { encoding: "utf8", windowsHide: true },
  );
  const taskLine = task.stdout?.trim();
  if (taskLine?.startsWith('"')) {
    const fields = parseCsv(taskLine);
    const kib = Number((fields[4] ?? "").replace(/[^0-9]/g, ""));
    if (Number.isFinite(kib)) peakRamBytes = Math.max(peakRamBytes, kib * 1024);
  }
  const gpu = spawnSync(
    "nvidia-smi.exe",
    ["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"],
    { encoding: "utf8", windowsHide: true },
  );
  for (const line of (gpu.stdout ?? "").split(/\r?\n/)) {
    const [pid, mib] = line.split(",").map((item) => item.trim());
    if (Number(pid) === child.pid && Number.isFinite(Number(mib))) {
      sampledProcessVramBytes = Math.max(sampledProcessVramBytes, Number(mib) * 1024 * 1024);
    }
  }
}

const resourceTimer = setInterval(sampleResources, 500);
const timeoutTimer = setTimeout(() => {
  if (!finished) {
    child.kill();
    finish(new Error("smoke_timeout"));
  }
}, 20 * 60 * 1000);

function validateResponse(response, testCase) {
  if (
    response?.schema !== 1 ||
    response?.job !== testCase.job ||
    response?.session !== "smoke-session" ||
    response?.window_start_frame !== 0 ||
    response?.window_end_frame !== testCase.frames ||
    !Array.isArray(response?.segments)
  ) {
    throw new Error("invalid_response_envelope");
  }
  let previousStart = -1;
  const speakers = new Set();
  for (const segment of response.segments) {
    if (
      !Number.isInteger(segment.start_frame) ||
      !Number.isInteger(segment.end_frame) ||
      segment.start_frame < 0 ||
      segment.end_frame <= segment.start_frame ||
      segment.end_frame > testCase.frames ||
      segment.start_frame < previousStart ||
      !/^S\d{2}$/.test(segment.speaker) ||
      typeof segment.text !== "string" ||
      !segment.text.trim()
    ) {
      throw new Error("invalid_segment_schema");
    }
    previousStart = segment.start_frame;
    speakers.add(segment.speaker);
  }
  return {
    id: testCase.id,
    frames: testCase.frames,
    segments: response.segments,
    segment_count: response.segments.length,
    speaker_count: speakers.size,
  };
}

function executeCase(index) {
  caseIndex = index;
  const testCase = cases[index];
  sentExecuteAt = performance.now();
  send({
    type: "execute",
    request: {
      schema: 1,
      job: testCase.job,
      session: "smoke-session",
      window_start_frame: 0,
      window_end_frame: testCase.frames,
      audio_path: testCase.audio,
      model_revision: modelRevision,
    },
  });
}

function onMessage(message) {
  if (message.type === "handshake") {
    handshakeMs = performance.now() - start;
    if (message.status !== "ready") throw new Error(`handshake_${message.status}`);
    if (
      message.backend !== "transformers" ||
      !/^(cuda:[0-9]+|cpu)$/.test(message.device) ||
      !/^(bfloat16|float16|float32)$/.test(message.dtype)
    ) {
      throw new Error("invalid_runtime_metadata");
    }
    handshakeRuntime = {
      backend: message.backend,
      device: message.device,
      dtype: message.dtype,
    };
    executeCase(0);
  } else if (message.type === "result") {
    const result = validateResponse(message.response, cases[caseIndex]);
    result.inference_ms = performance.now() - sentExecuteAt;
    caseResults.push(result);
    if (caseIndex + 1 < cases.length) executeCase(caseIndex + 1);
    else send({ type: "shutdown" });
  } else if (message.type === "error") {
    if (cases[caseIndex]?.id === "silence") {
      caseResults.push({
        id: "silence",
        frames: cases[caseIndex].frames,
        segments: [],
        segment_count: 0,
        speaker_count: 0,
        inference_ms: performance.now() - sentExecuteAt,
        worker_error: message.code,
      });
      send({ type: "shutdown" });
    } else {
      throw new Error(`worker_${message.code}`);
    }
  } else if (message.type === "shutdown" && message.status === "ok") {
    child.stdin.end();
  }
}

child.stdout.on("data", (chunk) => {
  stdoutBuffer += chunk.toString("utf8");
  const lines = stdoutBuffer.split(/\r?\n/);
  stdoutBuffer = lines.pop() ?? "";
  try {
    for (const line of lines) if (line) onMessage(JSON.parse(line));
  } catch (error) {
    child.kill();
    finish(error);
  }
});
child.stderr.on("data", (chunk) => {
  stderr += chunk.toString("utf8");
});

function finish(error) {
  if (finished) return;
  finished = true;
  clearInterval(resourceTimer);
  clearTimeout(timeoutTimer);
  sampleResources();
  const privateMetrics = {};
  for (const line of stderr.split(/\r?\n/)) {
    const match = /^metric_([a-z0-9_]+)=([0-9]+)$/.exec(line);
    if (match) privateMetrics[match[1]] = Math.max(privateMetrics[match[1]] ?? 0, Number(match[2]));
  }
  const speech = caseResults.find((item) => item.id === "speech");
  const silence = caseResults.find((item) => item.id === "silence");
  let evaluation = null;
  if (speech && silence) {
    const hash = (file) => crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
    const toHypothesis = (item) => item.segments.map((segment) => ({
      speaker: segment.speaker,
      text: segment.text,
      start_ms: segment.start_frame / 48,
    }));
    const evaluationInput = {
      schema: 1,
      run_metadata: {
        run_id: "moss-sapi-smoke",
        engine: "moss-transcribe-diarize-production-worker",
        model_revision: modelRevision,
        device: `${handshakeRuntime?.device ?? "unknown"}/${handshakeRuntime?.dtype ?? "unknown"}`,
        clock: "performance.now monotonic",
      },
      samples: [
        {
          id: "sapi-two-speaker",
          scenario: "synthetic-two-speaker-functional-smoke",
          language: "en",
          audio_sha256: hash(audioPath),
          reference_revision: "sapi-script-v1",
          reference: [
            { speaker: "VOICE-A", text: "Welcome to the product review meeting. Today we will discuss the launch plan.", start_ms: metadata.turns[0].start_frame / 48 },
            { speaker: "VOICE-B", text: "The engineering team will finish the reliability test before Friday.", start_ms: metadata.turns[1].start_frame / 48 },
            { speaker: "VOICE-A", text: "The final decision is to release after the test report is approved.", start_ms: metadata.turns[2].start_frame / 48 },
          ],
          hypothesis: toHypothesis(speech),
          audio_duration_ms: metadata.duration_seconds * 1000,
          processing_duration_ms: speech.inference_ms,
          final_latency_ms: speech.inference_ms,
          peak_ram_mb: peakRamBytes / 1_048_576,
          peak_vram_mb: (privateMetrics.cuda_peak_reserved_bytes ?? 0) / 1_048_576,
        },
        {
          id: "synthetic-silence",
          scenario: "synthetic-silence-functional-smoke",
          language: "en",
          audio_sha256: hash(silencePath),
          reference_revision: "generated-silence-v1",
          reference: "",
          hypothesis: toHypothesis(silence),
          audio_duration_ms: silenceMetadata.duration_seconds * 1000,
          processing_duration_ms: silence.inference_ms,
          final_latency_ms: silence.inference_ms,
          peak_ram_mb: peakRamBytes / 1_048_576,
          peak_vram_mb: (privateMetrics.cuda_peak_reserved_bytes ?? 0) / 1_048_576,
        },
      ],
    };
    fs.writeFileSync(evaluationInputPath, `${JSON.stringify(evaluationInput, null, 2)}\n`, "utf8");
    const evaluated = spawnSync(
      process.execPath,
      [path.join(projectRoot, "scripts", "evaluation", "asr_metrics.mjs"), evaluationInputPath, "--output", evaluationReportPath],
      { cwd: projectRoot, env, encoding: "utf8", windowsHide: true },
    );
    if (evaluated.status !== 0) throw new Error("evaluation_failed");
    const report = JSON.parse(fs.readFileSync(evaluationReportPath, "utf8"));
    evaluation = {
      cer: report.summary.cer,
      wer: report.summary.wer,
      cp_cer: report.summary.cp_cer,
      speaker_mapping: report.samples[0].speaker_mapping,
      silence_hallucinated_characters: report.summary.silence_stress.hallucinated_characters,
    };
  }
  const silenceHallucinated = (silence?.segment_count ?? 0) !== 0;
  const workerCaseFailed = caseResults.some((item) => item.worker_error);
  const record = {
    ok: !error && child.exitCode === 0 && !silenceHallucinated && !workerCaseFailed,
    error: error?.message ?? (silenceHallucinated ? "silence_hallucination" : (workerCaseFailed ? "silence_inference_failed" : null)),
    worker_exit_code: child.exitCode,
    model_revision: modelRevision,
    audio_source: "Windows SAPI synthetic two-voice alternating speech",
    accuracy_benchmark: false,
    audio_frames: metadata.frames,
    audio_seconds: metadata.duration_seconds,
    handshake_ms: handshakeMs,
    runtime: handshakeRuntime,
    speech_inference_ms: speech?.inference_ms ?? null,
    speech_rtf: speech ? speech.inference_ms / 1000 / metadata.duration_seconds : null,
    silence_inference_ms: silence?.inference_ms ?? null,
    peak_ram_bytes: peakRamBytes,
    peak_vram_bytes: privateMetrics.cuda_peak_reserved_bytes ?? null,
    peak_vram_allocated_bytes: privateMetrics.cuda_peak_allocated_bytes ?? null,
    gpu_total_bytes: privateMetrics.cuda_total_bytes ?? null,
    nvidia_smi_process_peak_bytes: sampledProcessVramBytes || null,
    speech_segment_count: speech?.segment_count ?? null,
    speech_speaker_count: speech?.speaker_count ?? null,
    silence_segment_count: silence?.segment_count ?? null,
    silence_worker_error: silence?.worker_error ?? null,
    evaluation,
    stderr_codes: stderr
      .split(/\r?\n/)
      .filter((line) => /^(warning|error)=[a-z0-9_]+$/.test(line)),
  };
  fs.writeFileSync(outputPath, `${JSON.stringify(record, null, 2)}\n`, "utf8");
  console.log(JSON.stringify(record));
  process.exitCode = record.ok ? 0 : 1;
}

child.on("exit", (code) => {
  if (!finished) {
    child.exitCode = code;
    finish(code === 0 && caseResults.length === cases.length ? null : new Error(`worker_exit_${code}`));
  }
});

send({ type: "handshake", schema: 1 });
