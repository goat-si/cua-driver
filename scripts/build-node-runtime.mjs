#!/usr/bin/env node

/**
 * Build the pinned UBRN N-API runtime with a copy-based RustBuffer boundary.
 *
 * Electron 20+ rejects external ArrayBuffers. UBRN 0.31.0-3 uses them as a
 * zero-copy optimization for every RustBuffer argument and return. The
 * generated SDK already copies RustBuffer contents while lowering/lifting, so
 * this compatibility build keeps the same values and ownership while using
 * JavaScript-owned Uint8Arrays at the N-API boundary.
 *
 * The patched source remains MPL-2.0 and comes from the exact, pinned
 * uniffi-bindgen-react-native development dependency.
 *
 * The runtime is its own Cargo project, outside rust/. Its dependency graph is
 * locked by scripts/node-runtime/Cargo.lock and built with --locked, so every
 * build resolves the same crates and CI can check them with cargo-deny
 * (goat-si/cua-driver#4).
 *
 *   --output <path> [--target <triple>]  build the runtime into <path>
 *   --prepare <directory>                write the patched, locked project only
 *   --update-lockfile                    re-resolve and rewrite the lockfile
 */
import { spawnSync } from "node:child_process"
import {
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs"
import { tmpdir } from "node:os"
import { basename, dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const scriptDirectory = dirname(fileURLToPath(import.meta.url))
const driverRoot = resolve(scriptDirectory, "..")
const typescriptRoot = join(driverRoot, "typescript")
const lockfile = join(scriptDirectory, "node-runtime", "Cargo.lock")
const upstreamRoot = join(
  typescriptRoot,
  "node_modules",
  "uniffi-bindgen-react-native",
)
function option(name) {
  const index = process.argv.indexOf(name)
  if (index < 0) return undefined
  const value = process.argv[index + 1]
  if (!value || value.startsWith("--")) throw new Error(`missing ${name} value`)
  return value
}
const usage =
  "usage: build-node-runtime.mjs --output <path> [--target <triple>] | --prepare <directory> | --update-lockfile"
const outputOption = option("--output")
const prepareOption = option("--prepare")
const updateLockfile = process.argv.includes("--update-lockfile")
if ([outputOption, prepareOption, updateLockfile || undefined].filter(Boolean).length !== 1) {
  throw new Error(usage)
}
const target = option("--target")
if (target && !outputOption) throw new Error(usage)

function cargoEnvironment() {
  const environment = { ...process.env }
  const buildsWindowsMsvc = target
    ? target.endsWith("-pc-windows-msvc")
    : process.platform === "win32"
  if (!buildsWindowsMsvc) return environment

  // Native npm packages must load on a clean Windows installation. The
  // upstream N-API runtime includes C++ objects, so MSVC's default dynamic
  // CRT linkage otherwise leaves VCRUNTIME140.dll as an undeclared system
  // prerequisite before any Cua code can run.
  environment.RUSTFLAGS = [
    environment.RUSTFLAGS?.trim(),
    "-C target-feature=+crt-static",
  ].filter(Boolean).join(" ")
  return environment
}

const manifest = JSON.parse(readFileSync(join(typescriptRoot, "package.json"), "utf8"))
const expectedVersion = manifest.devDependencies?.["uniffi-bindgen-react-native"]
const upstreamManifestPath = join(upstreamRoot, "package.json")
if (!existsSync(upstreamManifestPath)) {
  throw new Error("missing pinned UBRN source; run npm ci in libs/cua-driver/typescript")
}
const actualVersion = JSON.parse(readFileSync(upstreamManifestPath, "utf8")).version
if (actualVersion !== expectedVersion) {
  throw new Error(`UBRN source mismatch: expected ${expectedVersion}, found ${actualVersion}`)
}

function replaceOnce(source, needle, replacement, description) {
  const matches = source.split(needle).length - 1
  if (matches !== 1) throw new Error(`expected one ${description}, found ${matches}`)
  return source.replace(needle, replacement)
}

function patchRegister(source) {
  const allocStart = source.indexOf(
    "    let alloc_module = Arc::clone(&module);\n    let alloc_fn =",
  )
  const allocEndNeedle = '    result.set_named_property("rustbuffer_alloc", alloc_fn)?;'
  const allocEnd = source.indexOf(allocEndNeedle, allocStart)
  if (allocStart < 0 || allocEnd < 0) throw new Error("UBRN alloc block changed")
  source = `${source.slice(0, allocStart)}    let alloc_fn = env.create_function_from_closure("rustbuffer_alloc", move |ctx| {
        let size_arg: i32 = ctx.get(0)?;
        if size_arg < 0 {
            return Err(napi::Error::from_reason(
                "rustbuffer_alloc size must be non-negative".to_string(),
            ));
        }
        let len = usize::try_from(size_arg).map_err(|_| {
            napi::Error::from_reason("RustBuffer size exceeds addressable memory".to_string())
        })?;
        let typedarray = unsafe {
            napi_utils::create_uint8array(ctx.env.raw(), std::ptr::null(), len)?
        };
        unsafe { JsUnknown::from_raw(ctx.env.raw(), typedarray) }
    })?;
${source.slice(allocEnd)}`

  const freeStart = source.indexOf("    let free_module = Arc::clone(&module);")
  const freeEndNeedle = '    result.set_named_property("rustbuffer_free", free_fn)?;'
  const freeEnd = source.indexOf(freeEndNeedle, freeStart)
  if (freeStart < 0 || freeEnd < 0) throw new Error("UBRN free block changed")
  return `${source.slice(0, freeStart)}    let free_fn = env.create_function_from_closure("rustbuffer_free", move |ctx| {
        // Copy-mode buffers are JavaScript-owned. RustBuffer arguments are
        // separately allocated by rustbuffer_from_bytes inside call dispatch.
        ctx.env.get_undefined().map(|u| u.into_unknown())
    })?;
${source.slice(freeEnd)}`
}

function patchCall(source) {
  source = replaceOnce(
    source,
    "rust_buffer_to_js_uint8array_handoff(env, *rb, capacity_symbol)",
    "rust_buffer_to_js_uint8array_copy(env, *rb, module.rb_ops().free_ptr)",
    "RustBuffer return dispatch",
  )
  source = replaceOnce(
    source,
    "fn rust_buffer_to_js_uint8array_handoff(\n    env: &napi::Env,\n    rb: RustBufferC,\n    capacity_symbol: &CapacitySymbol,\n)",
    "fn rust_buffer_to_js_uint8array_copy(\n    env: &napi::Env,\n    rb: RustBufferC,\n    free_ptr: *const c_void,\n)",
    "RustBuffer return function",
  )
  const bodyStart = source.indexOf(
    "    // Empty RustBuffer (capacity == 0 or null data): no allocation to alias,",
  )
  const bodyEndNeedle = "    Ok(unsafe { JsUnknown::from_raw(raw_env, typedarray)? })\n}"
  const bodyEnd = source.indexOf(bodyEndNeedle, bodyStart)
  if (bodyStart < 0 || bodyEnd < 0) throw new Error("UBRN RustBuffer return body changed")
  const replacement = `    // Electron disallows external ArrayBuffers. Copy into a V8-owned
    // Uint8Array, then release the Rust allocation before returning to JS.
    let typedarray = unsafe { napi_utils::create_uint8array(raw_env, rb.data, len) };
    unsafe { napi_utils::free_rustbuffer(rb, free_ptr) };
    let typedarray = typedarray?;
    Ok(unsafe { JsUnknown::from_raw(raw_env, typedarray)? })
}`
  return `${source.slice(0, bodyStart)}${replacement}${source.slice(bodyEnd + bodyEndNeedle.length)}`
}

function patchManifest(source) {
  // UBRN's napi crate carries no license field; its sources are MPL-2.0 like
  // the core crate's, and cargo-deny needs the field to check the graph. It is
  // never published, which lets cargo-deny accept its path dependency.
  return replaceOnce(
    source,
    'edition = "2021"\n',
    'edition = "2021"\nlicense = "MPL-2.0"\npublish = false\n',
    "N-API runtime edition line",
  )
}

function cargo(args) {
  const result = spawnSync("cargo", args, {
    env: cargoEnvironment(),
    stdio: "inherit",
  })
  if (result.error) throw result.error
  if (result.status !== 0) throw new Error(`cargo exited with status ${result.status}`)
}

/** Write the patched runtime project to runtimeRoot; returns its manifest path. */
function prepareRuntime(runtimeRoot, { locked = true } = {}) {
  if (existsSync(runtimeRoot)) throw new Error(`${runtimeRoot} already exists`)
  cpSync(join(upstreamRoot, "runtimes", "core"), join(runtimeRoot, "core"), {
    recursive: true,
  })
  cpSync(join(upstreamRoot, "runtimes", "napi"), join(runtimeRoot, "napi"), {
    recursive: true,
  })
  const registerPath = join(runtimeRoot, "napi", "src", "register", "mod.rs")
  writeFileSync(registerPath, patchRegister(readFileSync(registerPath, "utf8")))
  const callPath = join(runtimeRoot, "napi", "src", "call", "mod.rs")
  writeFileSync(callPath, patchCall(readFileSync(callPath, "utf8")))
  const manifestPath = join(runtimeRoot, "napi", "Cargo.toml")
  writeFileSync(manifestPath, patchManifest(readFileSync(manifestPath, "utf8")))
  if (locked) {
    if (!existsSync(lockfile)) {
      throw new Error(`missing ${lockfile}; run build-node-runtime.mjs --update-lockfile`)
    }
    copyFileSync(lockfile, join(runtimeRoot, "napi", "Cargo.lock"))
  }
  return manifestPath
}

function writeLockfile(runtimeRoot) {
  const manifestPath = prepareRuntime(runtimeRoot, { locked: false })
  cargo(["generate-lockfile", "--manifest-path", manifestPath])
  mkdirSync(dirname(lockfile), { recursive: true })
  copyFileSync(join(runtimeRoot, "napi", "Cargo.lock"), lockfile)
  console.log(`wrote ${lockfile} for UBRN ${actualVersion}`)
}

function buildRuntime(runtimeRoot) {
  const manifestPath = prepareRuntime(runtimeRoot)
  const args = ["build", "--release", "--locked", "--manifest-path", manifestPath]
  if (target) args.push("--target", target)
  cargo(args)

  const targetDirectory = target
    ? join(runtimeRoot, "napi", "target", target, "release")
    : join(runtimeRoot, "napi", "target", "release")
  const candidates = [
    "libuniffi_runtime_napi.dylib",
    "libuniffi_runtime_napi.so",
    "uniffi_runtime_napi.dll",
  ]
  const built = candidates.map((name) => join(targetDirectory, name)).find(existsSync)
  if (!built) throw new Error(`missing built N-API runtime under ${targetDirectory}`)
  const output = resolve(outputOption)
  mkdirSync(dirname(output), { recursive: true })
  copyFileSync(built, output)
  console.log(`built ${basename(output)} from UBRN ${actualVersion}`)
}

if (prepareOption) {
  const manifestPath = prepareRuntime(resolve(prepareOption))
  console.log(`prepared ${manifestPath} from UBRN ${actualVersion}`)
} else {
  const temporaryRoot = mkdtempSync(join(tmpdir(), "cua-driver-node-runtime-"))
  try {
    const runtimeRoot = join(temporaryRoot, "runtime")
    if (updateLockfile) writeLockfile(runtimeRoot)
    else buildRuntime(runtimeRoot)
  } finally {
    rmSync(temporaryRoot, { recursive: true, force: true })
  }
}
