import { appendFileSync, existsSync } from "node:fs";
import { isAbsolute, resolve as resolvePath } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const opencodeRoot = process.env.CPR_HARNESS_OPENCODE_ROOT;
const piRoot = process.env.CPR_HARNESS_PI_ROOT;
const workerDir = process.env.CPR_HARNESS_WORKER_DIR;
const workerPackage = workerDir ? pathToFileURL(resolvePath(workerDir, "package.json")).href : "";

function filePath(url) {
  if (!url?.startsWith("file:")) return "";
  try {
    return fileURLToPath(url);
  } catch {
    return "";
  }
}

function under(root, file) {
  if (!root || !file) return false;
  const prefix = resolvePath(root).toLowerCase();
  const candidate = resolvePath(file).toLowerCase();
  return candidate === prefix || candidate.startsWith(prefix + "\\") || candidate.startsWith(prefix + "/");
}

function bare(specifier) {
  return (
    !specifier.startsWith(".") &&
    !specifier.startsWith("/") &&
    !specifier.startsWith("file:") &&
    !specifier.startsWith("node:") &&
    !specifier.startsWith("data:") &&
    !isAbsolute(specifier)
  );
}

function opencodeFile(specifier) {
  if (!opencodeRoot) return "";
  if (specifier === "@opencode/schema") return resolvePath(opencodeRoot, "packages/schema/src/index.ts");
  if (specifier.startsWith("@opencode/schema/")) {
    return resolvePath(opencodeRoot, "packages/schema/src", `${specifier.slice("@opencode/schema/".length)}.ts`);
  }
  return "";
}

function telemetryFile(specifier) {
  if (!piRoot) return "";
  if (specifier === "@earendil-works/pi-telemetry") return resolvePath(piRoot, "packages/telemetry/src/index.ts");
  return "";
}

function typescriptSibling(parentUrl, specifier) {
  if (!specifier.startsWith(".") || !specifier.endsWith(".js")) return "";
  const parent = filePath(parentUrl);
  if (!under(opencodeRoot, parent)) return "";
  const candidate = resolvePath(parent, "..", specifier.slice(0, -3) + ".ts");
  return existsSync(candidate) ? candidate : "";
}

export async function resolve(specifier, context, nextResolve) {
  const mapped = opencodeFile(specifier) || telemetryFile(specifier) || typescriptSibling(context.parentURL, specifier);
  if (mapped) return nextResolve(pathToFileURL(mapped).href, context);

  const parent = filePath(context.parentURL);
  const insideInstall = parent.includes(`${"node_modules"}`);
  if (workerPackage && bare(specifier) && !insideInstall) {
    return nextResolve(specifier, { ...context, parentURL: workerPackage });
  }
  return nextResolve(specifier, context);
}

const loadLog = process.env.CPR_HARNESS_LOAD_LOG;

export async function load(url, context, nextLoad) {
  const loaded = await nextLoad(url, context);
  const file = filePath(url.split("?")[0]);
  if (loadLog && (under(opencodeRoot, file) || under(piRoot, file))) {
    appendFileSync(loadLog, `${file}\n`);
  }
  return loaded;
}
