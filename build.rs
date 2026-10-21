use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

const ZSTD_BASE: &str = "third_party/zstd-1.5.7/lib";
const C_FILES: &[&str] = &[
    "common/debug.c",
    "common/entropy_common.c",
    "common/error_private.c",
    "common/fse_decompress.c",
    "common/pool.c",
    "common/threading.c",
    "common/xxhash.c",
    "common/zstd_common.c",
    "decompress/huf_decompress.c",
    "decompress/zstd_ddict.c",
    "decompress/zstd_decompress.c",
    "decompress/zstd_decompress_block.c",
];

fn run(command: &mut Command, description: &str) {
    let output = command.output().unwrap_or_else(|error| {
        panic!("Failed to start {description}: {error}. A C compiler is required to build the embedded Zstandard decoder.")
    });
    if !output.status.success() {
        panic!(
            "{description} failed ({}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

// MSVC discovery is delegated to the same MSVC tool locator
// used by cc-rs, instead of manually parsing `vcvars64.bat` output.
// In particular, do not require cmd.exe /C shell quoting to work with
// Visual Studio installation paths or the user's Unicode working directory.
#[cfg(windows)]
fn msvc_command(target: &str, tool: &str) -> Command {
    find_msvc_tools::find(target, tool).unwrap_or_else(|| {
        panic!(
            "Could not locate {tool} for {target} with find-msvc-tools. \
             Verify Visual Studio Installer > Desktop development with C++ \
             (MSVC x64 toolset and Windows SDK). \
             You can also run this build from an x64 Native Tools Command Prompt."
        )
    })
}

fn build_zstd() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = env::var("TARGET").expect("TARGET");
    println!("cargo:rerun-if-changed={ZSTD_BASE}");
    println!("cargo:rerun-if-changed=third_party/zstd-1.5.7/SOURCE-PROVENANCE.txt");

    let windows = target.contains("windows-msvc");
    // Rust's x86_64-pc-windows-msvc default uses the DLL CRT (/MD).
    // If Rust explicitly enables +crt-static, match it with /MT instead.
    // Without an explicit option MSVC cl.exe defaults to /MT and the linker
    // warns LNK4098 (LIBCMT vs MSVCRT). Do NOT suppress this with NODEFAULTLIB.
    let rust_crt_static = env::var("CARGO_CFG_TARGET_FEATURE")
        .unwrap_or_default()
        .split(',')
        .any(|feature| feature == "crt-static");
    let crt_option = if rust_crt_static { "/MT" } else { "/MD" };
    if target.contains("windows") && !windows {
        panic!("The bundled decoder supports Windows MSVC only (requested target: {target})");
    }
    let mut objects = Vec::new();
    for (index, source) in C_FILES.iter().enumerate() {
        let file = Path::new(ZSTD_BASE).join(source);
        assert!(
            file.is_file(),
            "Bundled Zstandard C source missing: {}",
            file.display()
        );
        let object = out_dir.join(format!(
            "zstd_{index:02}.{}",
            if windows { "obj" } else { "o" }
        ));
        let mut command = if windows {
            #[cfg(windows)]
            {
                msvc_command(&target, "cl.exe")
            }
            #[cfg(not(windows))]
            {
                panic!("MSVC cross-compilation is unsupported on a non-Windows host")
            }
        } else {
            Command::new(env::var_os("CC").unwrap_or_else(|| OsStr::new("cc").to_os_string()))
        };
        if windows {
            command
                .arg("/nologo")
                .arg("/c")
                .arg("/O2")
                .arg(crt_option)
                .arg("/DZSTD_DISABLE_ASM=1")
                .arg("/DZSTD_LEGACY_SUPPORT=0")
                .arg(format!("/I{ZSTD_BASE}"))
                .arg(format!("/I{ZSTD_BASE}/common"))
                .arg(format!("/I{ZSTD_BASE}/decompress"))
                .arg(format!("/Fo{}", object.display()))
                .arg(&file);
        } else {
            command
                .arg("-O2")
                .arg("-fPIC")
                .arg("-DZSTD_DISABLE_ASM=1")
                .arg("-DZSTD_LEGACY_SUPPORT=0")
                .arg(format!("-I{ZSTD_BASE}"))
                .arg(format!("-I{ZSTD_BASE}/common"))
                .arg(format!("-I{ZSTD_BASE}/decompress"))
                .arg("-c")
                .arg(&file)
                .arg("-o")
                .arg(&object);
        }
        run(
            &mut command,
            &format!("building embedded Zstandard {source}"),
        );
        objects.push(object);
    }
    let lib = out_dir.join(if windows { "zstd.lib" } else { "libzstd.a" });
    let mut archive = if windows {
        #[cfg(windows)]
        {
            msvc_command(&target, "lib.exe")
        }
        #[cfg(not(windows))]
        {
            panic!("MSVC cross-compilation is unsupported on a non-Windows host")
        }
    } else {
        Command::new(env::var_os("AR").unwrap_or_else(|| OsStr::new("ar").to_os_string()))
    };
    if windows {
        archive
            .arg("/nologo")
            .arg(format!("/OUT:{}", lib.display()));
    } else {
        archive.arg("rcs").arg(&lib);
    }
    archive.args(&objects);
    run(
        &mut archive,
        "archiving the embedded Zstandard static library",
    );
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=zstd");
}

#[cfg(windows)]
fn build_folder_picker() {
    let target = env::var("TARGET").expect("TARGET");
    assert!(
        target.contains("windows-msvc"),
        "Windows UX requires the MSVC target"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("folder_picker.obj");
    let library = out.join("codex_folder_picker.lib");
    let features = env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let crt = if features.split(',').any(|f| f == "crt-static") {
        "/MT"
    } else {
        "/MD"
    };
    run(
        msvc_command(&target, "cl.exe")
            .args(["/nologo", "/c", "/O2", "/EHsc", "/std:c++17", "/utf-8", crt])
            .arg(format!("/Fo{}", object.display()))
            .arg("native/folder_picker.cpp"),
        "building Windows Shell folder picker",
    );
    run(
        msvc_command(&target, "lib.exe")
            .arg("/nologo")
            .arg(format!("/OUT:{}", library.display()))
            .arg(&object),
        "archiving Windows Shell folder picker",
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=codex_folder_picker");
    for lib in ["ole32", "shell32", "user32", "uuid", "winsqlite3"] {
        println!("cargo:rustc-link-lib={lib}");
    }
}

fn main() {
    let target = env::var("TARGET").expect("TARGET");
    if target.contains("windows") && target != "x86_64-pc-windows-msvc" {
        panic!("The supported Windows target is x86_64-pc-windows-msvc, not {target}");
    }
    // Non-Windows compilation is retained solely for internal core tests; it
    // does not provide a supported desktop application on those platforms.
    build_zstd();
    println!("cargo:rerun-if-changed=native/folder_picker.cpp");
    #[cfg(windows)]
    build_folder_picker();
    println!("cargo:rerun-if-changed=assets/emoji_u1f4dd.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/emoji_u1f4dd.ico")
            .set("FileDescription", "Codex Conversation Handoff Exporter")
            .set("ProductName", "Codex Conversation Handoff Exporter")
            .set(
                "OriginalFilename",
                "Codex-Conversation-Handoff-Exporter.exe",
            )
            .set("InternalName", "Codex-Conversation-Handoff-Exporter")
            .set("FileVersion", env!("CARGO_PKG_VERSION"))
            .set("ProductVersion", env!("CARGO_PKG_VERSION"))
            .set(
                "LegalCopyright",
                "Copyright (c) 2026 SunnyLeu; icon Copyright 2013 Google, Inc.",
            );
        // Native folder picker must use per-monitor DPI scaling. This is a
        // process-level Windows manifest, not a .NET/Windows Forms dependency.
        resource.set_manifest_file("native/app.manifest");
        println!("cargo:rerun-if-changed=native/app.manifest");
        resource
            .compile()
            .expect("failed to compile Windows icon/version resources");
    }
}
