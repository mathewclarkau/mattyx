use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // The ghostty submodule at the mattyx repo root is the default source.
    // MTYX_GHOSTTY_SRC overrides it for out-of-tree builds.
    let ghostty_dir = match env::var("MTYX_GHOSTTY_SRC") {
        Ok(p) => PathBuf::from(p),
        Err(_) => manifest_dir.join("../../../ghostty"),
    };
    let ghostty_dir = ghostty_dir.canonicalize().unwrap_or_else(|e| {
        panic!(
            "ghostty source not found at {} ({}). Run `git submodule update --init` \
             or set MTYX_GHOSTTY_SRC.",
            ghostty_dir.display(),
            e
        )
    });
    let ghostty_dir = strip_windows_verbatim(ghostty_dir);

    println!("cargo:rerun-if-env-changed=MTYX_GHOSTTY_SRC");
    println!("cargo:rerun-if-env-changed=ZIG");
    println!("cargo:rerun-if-env-changed=MTYX_GHOSTTY_VT_ZIG_CPU");
    println!("cargo:rerun-if-changed={}", ghostty_dir.join("include").display());
    println!("cargo:rerun-if-changed={}", ghostty_dir.join("build.zig").display());
    println!("cargo:rerun-if-changed={}", ghostty_dir.join("src").display());

    // Build libghostty-vt.a with zig. ReleaseFast regardless of the cargo
    // profile: the VT parser is on the PTY hot path and a debug zig build
    // is an order of magnitude slower.
    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".to_string());
    let prefix = out_dir.join("ghostty-vt");
    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();
    // Args are collected (not chained onto a Command) so the retry loop
    // below can rebuild an identical invocation — Command is not Clone.
    let mut zig_args: Vec<String> = vec![
        "build".to_string(),
        "-Demit-lib-vt=true".to_string(),
        "-Demit-xcframework=false".to_string(),
        "-Doptimize=ReleaseFast".to_string(),
    ];
    if target != host {
        if let Some(zig_target) = zig_target_for_rust_target(&target) {
            zig_args.push(format!("-Dtarget={zig_target}"));
        }
    }
    // Valgrind's instruction emulation doesn't cover every CPU-native SIMD
    // extension zig's default target detection can select (e.g. some AVX-512
    // variants), which SIGILLs under valgrind. CI's valgrind job sets this to
    // "baseline" to match the same workaround ghostty's own build.zig uses
    // for its valgrind step (see `Config.baselineTarget()`).
    if let Ok(cpu) = env::var("MTYX_GHOSTTY_VT_ZIG_CPU") {
        zig_args.push(format!("-Dcpu={cpu}"));
    }
    zig_args.push("--prefix".to_string());
    zig_args.push(prefix.display().to_string());

    // Windows cache placement (PR #101 run 35314291149): zig 0.15.2 on
    // windows-latest died with "unable to read results of configure phase
    // from '.zig-cache\tmp\<hex>': FileNotFound". Keep the local cache
    // short, flat and off the repo path (C:\mtyx-zig-cache, TEMP
    // fallback), which caps every cache-internal path regardless of
    // checkout depth. Combined with the one-shot retry below for the
    // known-transient signature (configure-phase tmp entries vanishing
    // under AV/Defender scans on fresh caches), both plausible root
    // causes are covered. Non-Windows hosts keep zig's defaults.
    let cache_dir_for_retry = if cfg!(windows) {
        let (local_cache, global_cache) = windows_zig_cache_dirs();
        zig_args.push("--cache-dir".to_string());
        zig_args.push(local_cache.display().to_string());
        zig_args.push("--global-cache-dir".to_string());
        zig_args.push(global_cache.display().to_string());
        local_cache
    } else {
        ghostty_dir.join(".zig-cache")
    };

    // Run zig build, capturing output so a transient failure can be
    // recognised and retried. Output is echoed either way so CI logs
    // look identical to the streamed version.
    let mut attempt = 1;
    loop {
        // Command is not Clone and output() consumes it; rebuild the
        // identical invocation per attempt from the collected args.
        let mut run = Command::new(&zig);
        run.current_dir(&ghostty_dir).args(&zig_args);
        let output = run.output().unwrap_or_else(|e| {
            panic!("failed to run `{zig} build` in {}: {e}", ghostty_dir.display())
        });
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(&output.stdout);
        let _ = stdout.write_all(&output.stderr);
        let _ = stdout.flush();
        if output.status.success() {
            break;
        }
        let stderr_text = String::from_utf8_lossy(&output.stderr).into_owned();
        // "unable to read results of configure phase" is the known
        // transient signature: a configure-phase tmp directory entry
        // that vanished between write and read (Defender scan races on
        // fresh Windows caches, parallel-step tmp cleanup). Clear the
        // tmp area and retry exactly once; a real failure fails hard on
        // the second attempt.
        if attempt == 1 && stderr_text.contains("unable to read results of configure phase") {
            eprintln!(
                "mtyx: transient zig configure-phase failure; clearing {} and retrying once",
                cache_dir_for_retry.join("tmp").display()
            );
            let _ = std::fs::remove_dir_all(cache_dir_for_retry.join("tmp"));
            attempt += 1;
            continue;
        }
        panic!("zig build of libghostty-vt failed with {}", output.status);
    }

    println!("cargo:rustc-link-search=native={}", prefix.join("lib").display());
    if target.contains("windows") {
        println!("cargo:rustc-link-lib=static=ghostty-vt-static");
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }

    // Generate bindings from the public C header.
    let include_dir = ghostty_dir.join("include");
    let mut builder = bindgen::Builder::default()
        .header(include_dir.join("ghostty/vt.h").to_str().unwrap().to_string())
        .clang_arg(format!("-I{}", include_dir.display()))
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false);

    // Feed system include paths to clang to help find headers (like limits.h)
    // when libclang doesn't have its own resource directory headers.
    //
    // Try the project's own toolchain first (cc -> clang), then gcc as a
    // last resort. CI uses `apt install clang libclang-dev` (no gcc) so the
    // gcc-only probe would silently no-op there.
    for cc in ["cc", "clang", "gcc"] {
        if let Some(paths) = probe_system_includes(cc) {
            for path in paths {
                builder = builder.clang_arg(format!("-isystem{}", path.display()));
            }
            break;
        }
    }

    // If clang is on PATH, ask it for its resource dir and feed it to
    // bindgen via -resource-dir. This is the surest way to find clang's
    // bundled limits.h/stddef.h on stripped-down clang packages.
    if let Some(resdir) = clang_resource_dir() {
        builder = builder.clang_arg(format!("-resource-dir={}", resdir.display()));
    }

    let bindings = builder.generate().expect("bindgen failed for ghostty/vt.h");
    bindings.write_to_file(out_dir.join("bindings.rs")).expect("failed to write bindings.rs");
}

fn zig_target_for_rust_target(target: &str) -> Option<&'static str> {
    match target {
        "x86_64-pc-windows-gnu" => Some("x86_64-windows-gnu"),
        "x86_64-pc-windows-msvc" => Some("x86_64-windows-msvc"),
        "aarch64-pc-windows-msvc" => Some("aarch64-windows-msvc"),
        _ => None,
    }
}

/// `std::fs::canonicalize` returns a verbatim (`\\?\`-prefixed) path on
/// Windows. clang/libclang cannot resolve `#include <ghostty/vt/types.h>`
/// against a verbatim `-I` dir: header search string-concats the include
/// name with forward slashes onto the dir, and forward slashes are not
/// translated inside the NT namespace the verbatim prefix opts into, so
/// the lookup fails while the main header (all backslashes, from
/// `Path::join`) opens fine. This is exactly how the zig build succeeded
/// and bindgen failed with "'ghostty/vt/types.h' file not found" on
/// windows-latest (PR #101 run 35317130765). Strip both verbatim forms
/// (drive-local `\\?\D:\...` and `\\?\UNC\server\...`); compiled only
/// on Windows, so Linux/macOS paths and bindgen args are unchanged.
fn strip_windows_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(text) = path.to_str() {
            if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{}", rest));
            }
            if let Some(rest) = text.strip_prefix(r"\\?\") {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// Short, stable, off-repo zig cache dirs for Windows hosts (see the
/// comment at the call site). Local and global must be distinct
/// directories — they share a namespace (tmp/, o/, h/, p/) and zig
/// documents them as separate caches.
fn windows_zig_cache_dirs() -> (PathBuf, PathBuf) {
    // Default Windows ACLs let interactive users create directories
    // directly at drive roots (runners and dev boxes alike); fall back
    // to %TEMP% for locked-down environments.
    let root = PathBuf::from(r"C:\mtyx-zig-cache");
    if std::fs::create_dir_all(&root).is_err() {
        let fallback = std::env::temp_dir().join("mtyx-zig-cache");
        let _ = std::fs::create_dir_all(&fallback);
        let (local, global) = (fallback.join("local"), fallback.join("global"));
        let _ = std::fs::create_dir_all(&local);
        let _ = std::fs::create_dir_all(&global);
        return (local, global);
    }
    let (local, global) = (root.join("local"), root.join("global"));
    let _ = std::fs::create_dir_all(&local);
    let _ = std::fs::create_dir_all(&global);
    (local, global)
}

/// Run `<cc> -E -Wp,-v -` and return the include paths listed between
/// `#include <...>` and `End of search list.` on stderr. Returns None if
/// the probe couldn't be run (binary missing or non-zero exit).
fn probe_system_includes(cc: &str) -> Option<Vec<std::path::PathBuf>> {
    let output = Command::new(cc)
        .args(["-E", "-Wp,-v", "-"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut paths = Vec::new();
    let mut in_search_list = false;
    for line in stderr.lines() {
        if line.contains("#include <...>") {
            in_search_list = true;
            continue;
        }
        if line.contains("End of search list.") {
            break;
        }
        if in_search_list {
            let path = line.trim();
            if !path.is_empty() && std::path::Path::new(path).exists() {
                paths.push(std::path::PathBuf::from(path));
            }
        }
    }
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// Ask `clang` for its resource directory (the path containing clang's
/// bundled `include/limits.h` etc.). Returns None if clang isn't on PATH
/// or the probe failed. We feed this to bindgen via `-resource-dir` so
/// the build works on systems where the system `limits.h` is missing or
/// libclang's resource-dir detection is broken (thin Arch `clang`,
/// distroless images, some Nix shells).
fn clang_resource_dir() -> Option<std::path::PathBuf> {
    let output = Command::new("clang").arg("-print-resource-dir").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if dir.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(dir))
    }
}
