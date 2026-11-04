fn main() {
    // ---------------------------------------------------------------- llama.cpp
    //
    // 用 CMake 把 vendor/llama.cpp 与 native/llama_shim.cpp 一起编成静态库。
    // 只在开启了 `llama` feature 时才编译。
    let llama_enabled = std::env::var("CARGO_FEATURE_LLAMA").is_ok();
    if llama_enabled {
        let cuda_mode = std::env::var("DEEPAISEE_CUDA")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let cuda_enabled = match cuda_mode.as_str() {
            "0" | "false" | "off" | "no" => false,
            "1" | "true" | "on" | "yes" => true,
            _ => cuda_toolkit_root().is_some(),
        };
        let vulkan_mode = std::env::var("DEEPAISEE_VULKAN")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let vulkan_enabled = match vulkan_mode.as_str() {
            "0" | "false" | "off" | "no" => false,
            "1" | "true" | "on" | "yes" => true,
            _ => vulkan_sdk_root().is_some(),
        };
        let windows_gnu =
            cfg!(windows) && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
        if cuda_enabled && windows_gnu {
            panic!("DEEPAISEE_CUDA requires the x86_64-pc-windows-msvc target; NVIDIA CUDA does not support this project's Windows GNU toolchain");
        }
        let mut cfg = cmake::Config::new("native");
        if vulkan_enabled {
            if let Some(out_dir) = windows_ascii_cmake_out_dir() {
                // MSBuild's FileTracker cannot create logs below this workspace's
                // non-ASCII path. Keep CMake intermediates on the same volume at
                // an ASCII path; Cargo/Rust outputs remain in the configured target dir.
                cfg.out_dir(out_dir);
            }
        }
        cfg.define("CMAKE_BUILD_TYPE", "Release")
            .define("LLAMA_BUILD_TESTS", "OFF")
            .define("LLAMA_BUILD_EXAMPLES", "OFF")
            .define("LLAMA_BUILD_SERVER", "OFF")
            .define("LLAMA_BUILD_TOOLS", "OFF")
            // GGUF multimodal support is provided by the mtmd projector
            // library. Keep this explicit in the Rust build as well as in
            // native/CMakeLists.txt so stale/default CMake cache values cannot
            // silently produce a text-only llama.cpp binary.
            .define("LLAMA_BUILD_MTMD", "ON")
            .define("LLAMA_CURL", "OFF")
            .define("BUILD_SHARED_LIBS", "OFF")
            .define("DEEPAISEE_CUDA", if cuda_enabled { "ON" } else { "OFF" })
            .define(
                "DEEPAISEE_VULKAN",
                if vulkan_enabled { "ON" } else { "OFF" },
            );

        if vulkan_enabled {
            let sdk = vulkan_sdk_root().expect(
                "DEEPAISEE_VULKAN=1 requires a Vulkan SDK with Include/vulkan/vulkan.h and Bin/glslc.exe; place it in .tools/vulkan or set VULKAN_SDK",
            );
            cfg.define("Vulkan_ROOT", sdk.to_string_lossy().to_string())
                .define("VULKAN_SDK", sdk.to_string_lossy().to_string());
            if let Some(spirv_headers) = spirv_headers_dir(&sdk) {
                cfg.define(
                    "SPIRV-Headers_DIR",
                    spirv_headers.to_string_lossy().to_string(),
                );
            }
        }

        // Pass the toolkit root to CMake and MSBuild. Visual Studio's CUDA
        // targets need CudaToolkitDir at build time even when CMake found nvcc.
        if cuda_enabled {
            if let Some(cuda_root) = cuda_toolkit_root() {
                let root = std::path::PathBuf::from(cuda_root);
                let nvcc = root
                    .join("bin")
                    .join(if cfg!(windows) { "nvcc.exe" } else { "nvcc" });
                cfg.define("CUDAToolkit_ROOT", root.to_string_lossy().to_string())
                    .define("CMAKE_CUDA_COMPILER", nvcc.to_string_lossy().to_string());
                if cfg!(windows) {
                    // CUDA .targets concatenate path components directly.
                    // Forward slash avoids MSBuild escaping issues.
                    cfg.build_arg(format!("/p:CudaToolkitDir={}/", root.display()));
                }
            }
        }

        println!("cargo:rerun-if-env-changed=DEEPAISEE_CUDA");
        println!("cargo:rerun-if-env-changed=DEEPAISEE_VULKAN");
        println!("cargo:rerun-if-env-changed=VULKAN_SDK");
        println!("cargo:rerun-if-env-changed=DEEPAISEE_VULKAN_ROOT");
        println!("cargo:rerun-if-env-changed=CUDA_PATH");
        println!("cargo:rerun-if-env-changed=CUDA_HOME");
        println!("cargo:rerun-if-env-changed=DEEPAISEE_CUDA_ROOT");
        println!("cargo:rerun-if-env-changed=CUDA_PATH_V12_0");
        println!("cargo:rerun-if-env-changed=CUDA_PATH_V13_1");

        // Windows + GNU 工具链：让 cmake 用 Ninja，并把 ucrt64 的 gcc/g++ 指给它。
        //
        // 编译器目录按优先级找：
        //   1. 环境变量 `DEEPAISEE_UCRT64_BIN`（想自定义就设这个）
        //   2. PATH 里已经有 gcc.exe —— 说明工具链已配好，不干预
        //   3. 常见安装位置（MSYS2 默认路径）
        // 找不到就跳过，让 cmake 自己按 PATH 找编译器（在已激活的 MSYS2 shell 里也能编）。
        if cfg!(windows) && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            let path = std::env::var("PATH").unwrap_or_default();
            let in_path = path
                .split(';')
                .any(|p| std::path::Path::new(p).join("gcc.exe").exists());

            let tools = if let Ok(p) = std::env::var("DEEPAISEE_UCRT64_BIN") {
                Some(std::path::PathBuf::from(p))
            } else if in_path {
                None
            } else {
                [
                    r"C:\msys64\ucrt64\bin",
                    r"C:\msys64\mingw64\bin",
                    r"C:\tools\msys64\ucrt64\bin",
                ]
                .iter()
                .map(std::path::PathBuf::from)
                .find(|p| p.join("gcc.exe").exists())
            };

            if let Some(tools) = tools {
                if tools.join("gcc.exe").exists() {
                    let cc = tools.join("gcc.exe");
                    let cxx = tools.join("g++.exe");
                    cfg.define("CMAKE_C_COMPILER", cc.to_string_lossy().to_string())
                        .define("CMAKE_CXX_COMPILER", cxx.to_string_lossy().to_string());
                    std::env::set_var("PATH", format!("{};{}", tools.to_string_lossy(), path));
                }
            }
            if std::env::var("CMAKE_GENERATOR").is_err() {
                std::env::set_var("CMAKE_GENERATOR", "Ninja");
            }
        }

        let dst = cfg.build();
        // ggml 的 CMake 目标在 Windows 上产出的静态库叫 `ggml.a` / `ggml-base.a`
        // （没有 lib 前缀），而 rustc 的 `-l static=ggml` 只会去找 `libggml.a`。
        // 这里把它们复制一份带前缀的副本到 out/native-libs，再把这个目录加进搜索路径。
        let compat = dst.join("native-libs");
        let _ = std::fs::create_dir_all(&compat);
        for name in [
            "ggml",
            "ggml-base",
            "ggml-cpu",
            "ggml-cuda",
            "ggml-vulkan",
            "llama",
            "mtmd",
            "llama_shim",
        ] {
            let src = dst.join("lib").join(format!("{name}.a"));
            let alt = dst
                .join("build")
                .join("llama.cpp")
                .join("ggml")
                .join("src")
                .join(format!("{name}.a"));
            let cuda_alt = dst
                .join("build")
                .join("llama.cpp")
                .join("ggml")
                .join("src")
                .join("ggml-cuda")
                .join(format!("{name}.a"));
            let vulkan_alt = dst
                .join("build")
                .join("llama.cpp")
                .join("ggml")
                .join("src")
                .join("ggml-vulkan")
                .join(format!("{name}.a"));
            let from = if src.exists() {
                src
            } else if alt.exists() {
                alt
            } else if vulkan_alt.exists() {
                vulkan_alt
            } else {
                cuda_alt
            };
            if from.exists() {
                let _ = std::fs::copy(&from, compat.join(format!("lib{name}.a")));
            }
        }
        for name in ["libmtmd.a", "mtmd.lib"] {
            let candidate = dst.join("build").join("lib").join(name);
            if candidate.exists() {
                let target_name = if name.ends_with(".a") { "libmtmd.a" } else { "mtmd.lib" };
                let _ = std::fs::copy(candidate, compat.join(target_name));
            }
        }
        for sub in [
            "native-libs",
            "lib",
            "build",
            "build/lib",
            "build/llama.cpp/src",
            "build/llama.cpp/tools/mtmd",
            "build/llama.cpp/ggml/src",
            "build/llama.cpp/ggml/src/ggml-cuda",
            "build/llama.cpp/ggml/src/ggml-vulkan",
            "build/llama.cpp/vendor/hash",
        ] {
            let p = dst.join(sub);
            println!("cargo:rustc-link-search=native={}", p.display());
        }
        println!("cargo:rustc-link-lib=static=llama_shim");
        println!("cargo:rustc-link-lib=static=mtmd");
        println!("cargo:rustc-link-lib=static=llama");
        // Link the static backend libraries in dependency order.
        println!("cargo:rustc-link-lib=static=ggml");
        if cuda_enabled {
            println!("cargo:rustc-link-lib=static=ggml-cuda");
        }
        if vulkan_enabled {
            println!("cargo:rustc-link-lib=static=ggml-vulkan");
            if let Some(sdk) = vulkan_sdk_root() {
                let lib_dir = sdk.join("Lib");
                println!("cargo:rustc-link-search=native={}", lib_dir.display());
            }
            println!("cargo:rustc-link-lib=dylib=vulkan-1");
        }
        println!("cargo:rustc-link-lib=static=ggml-base");
        println!("cargo:rustc-link-lib=static=ggml-cpu");
        if cuda_enabled {
            if let Some(cuda_root) = cuda_toolkit_root() {
                let lib_dir = std::path::PathBuf::from(cuda_root).join(if cfg!(windows) {
                    "lib/x64"
                } else {
                    "lib64"
                });
                println!("cargo:rustc-link-search=native={}", lib_dir.display());
            }
            for lib in ["cuda", "cudart", "cublas", "cublasLt"] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
        }
        // GNU 工具链需要 libstdc++，MSVC 自带 C++ 运行时。
        //
        // 这里刻意用**静态**方式链入 GNU 运行时：否则最终 exe 会依赖
        // libstdc++-6.dll / libgcc_s_seh-1.dll / libwinpthread-1.dll 三个
        // MinGW DLL，安装包里少任何一个都会让程序双击后直接弹「系统错误」
        // 退出（退出码 0xC0000135），而这些 DLL 又是"构建机本地"的版本，
        // 打包分发时极易漏拷或版本错配。静态链入后 exe 自带运行时，
        // 安装目录里不再需要这三个文件。
        //
        // 注意：这里**不能**用 `cargo:rustc-link-lib=dylib=stdc++`。那样 rustc 会
        // 直接生成 `-lstdc++`，而 GNU ld 在默认的 -Bdynamic 状态下会命中
        // `libstdc++.dll.a`（导入库），最终 exe 依然写着 DLL Name: libstdc++-6.dll。
        // 必须在链接参数里显式切换成 -Bstatic 再取 -lstdc++，ld 才会挑 `libstdc++.a`。
        // -lwinpthread 同理（否则会带 libwinpthread-1.dll）。
        //
        // 还有个坑：rustc 会传 `-nodefaultlibs`，默认的 msvcrt / mingwex / mingw32
        // 在 `-lstdc++` **之前**就已经出现在链接行里，而 ld 是单遍的——
        // 静态库 libstdc++.a 里对 `fileno` / `fdopen` / `__imp__wstat64` 的引用
        // 没人再解析，报一屏 `undefined reference`。所以静态库之后必须把这几轮
        // 默认库再补一遍。
        // 而且系统导入库之间也有互相依赖（libmingwex 的 dirent 用 ucrt 的
        // `_findclose`，libstdc++ 的 filesystem 用 kernel32 的
        // `__imp_GetDiskFreeSpaceExW`），单遍扫描怎么排都有落下。所以用
        // `--start-group/--end-group` 让 ld 在这组库里反复扫描到收敛。
        // -static-libgcc 用 GCC 驱动自己的开关，静态拿下 libgcc_s_seh-1.dll。
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            println!("cargo:rustc-link-arg=-static-libgcc");
            println!("cargo:rustc-link-arg=-Wl,-Bstatic");
            println!("cargo:rustc-link-arg=-lstdc++");
            println!("cargo:rustc-link-arg=-lwinpthread");
            println!("cargo:rustc-link-arg=-Wl,-Bdynamic");
            println!("cargo:rustc-link-arg=-Wl,--start-group");
            for lib in [
                "ucrt",
                "mingwex",
                "mingw32",
                "ssp",
                "kernel32",
                "user32",
                "advapi32",
                "shell32",
                "ole32",
                "ws2_32",
            ] {
                println!("cargo:rustc-link-arg=-l{lib}");
            }
            println!("cargo:rustc-link-arg=-Wl,--end-group");
        }
        println!("cargo:rerun-if-changed=native/llama_shim.cpp");
        println!("cargo:rerun-if-changed=native/CMakeLists.txt");
        println!("cargo:rerun-if-changed=../vendor/llama.cpp/tools/mtmd/mtmd.h");
        println!("cargo:rerun-if-changed=../vendor/llama.cpp/src/models/gemma-embedding2.cpp");
    }

    // Windows + GNU 工具链下，windres 无法打开含非 ASCII 字符路径的 .ico
    // （报 `can't open icon file ...: Invalid argument`）。这里在需要时把图标
    // 复制到 OUT_DIR 下的纯 ASCII 路径再交给 tauri-build；路径本身是 ASCII 时
    // 走默认逻辑，行为完全不变。
    let mut attrs = tauri_build::Attributes::new();
    if let Some(icon) = ascii_window_icon() {
        attrs = attrs.windows_attributes(
            tauri_build::WindowsAttributes::new().window_icon_path(icon),
        );
    }
    tauri_build::try_build(attrs).expect("failed to run tauri-build")
}

/// 当 `icons/icon.ico` 的绝对路径含非 ASCII 字符时，复制一份到 OUT_DIR（ASCII）
/// 并返回该副本路径；否则返回 None，让 tauri-build 用默认图标路径。
fn ascii_window_icon() -> Option<std::path::PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    let icon = manifest.join("icons").join("icon.ico");
    if !icon.exists() {
        return None;
    }
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR")?);
    if !out_dir.to_string_lossy().is_ascii() {
        return None;
    }
    let dst = out_dir.join("icon-ascii.ico");
    if std::fs::copy(&icon, &dst).is_ok() {
        Some(dst)
    } else {
        None
    }
}

/// Resolve the CUDA toolkit root consistently for CMake configuration and
/// Rust link search paths. Prefer the canonical CUDA_PATH, then versioned
/// variables used by side-by-side toolkit installations.
fn cuda_toolkit_root() -> Option<std::path::PathBuf> {
    [
        "CUDA_PATH",
        "CUDA_HOME",
        "DEEPAISEE_CUDA_ROOT",
        "CUDA_PATH_V13_1",
        "CUDA_PATH_V12_0",
    ]
    .iter()
    .filter_map(|name| std::env::var_os(name))
    .map(std::path::PathBuf::from)
    .chain(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .into_iter()
            .flat_map(|manifest| {
                let repo = std::path::PathBuf::from(manifest).join("..");
                [repo.join(".tools/cuda"), repo.join(".tools/cuda-toolkit")]
            }),
    )
    .find(|root| {
        root.join("bin").join("nvcc.exe").exists() || root.join("bin").join("nvcc").exists()
    })
}

fn vulkan_sdk_root() -> Option<std::path::PathBuf> {
    [
        "VULKAN_SDK",
        "DEEPAISEE_VULKAN_ROOT",
        // MSYS2 的 vulkan-devel 装在 ucrt64 下，没有独立 SDK 目录（REQ-093）。
        "MSYS2_ROOT",
    ]
    .iter()
    .filter_map(|name| std::env::var_os(name))
    .map(std::path::PathBuf::from)
    .chain(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .into_iter()
            .flat_map(|manifest| {
                let repo = std::path::PathBuf::from(manifest).join("..");
                [
                    repo.join(".tools/vulkan"),
                    repo.join(".tools/vulkan-sdk"),
                    // 常见外部工具链位置：有就自动启用 Vulkan，不需要手动设环境变量。
                    std::path::PathBuf::from(r"D:\dsh-tools\msys64\ucrt64"),
                    std::path::PathBuf::from(r"C:\msys64\ucrt64"),
                    std::path::PathBuf::from(r"D:\msys64\ucrt64"),
                ]
            }),
    )
    .find(|root| {
        let header = root.join("include").join("vulkan").join("vulkan.h").exists()
            || root
                .join("Include")
                .join("vulkan")
                .join("vulkan.h")
                .exists();
        header
            && (root.join("bin").join("glslc.exe").exists()
                || root.join("Bin").join("glslc.exe").exists())
    })
}

/// Locate the CMake config for SPIRV-Headers.
///
/// llama.cpp's Vulkan backend does `find_package(SPIRV-Headers CONFIG REQUIRED)`
/// and appends `$ENV{VULKAN_SDK}` to `CMAKE_PREFIX_PATH`, so a config under
/// `$VULKAN_SDK/share/cmake` resolves on its own. The Khronos SDK ships it in
/// `Lib/cmake/SPIRV-Headers` while MSYS2 puts it in `share/cmake/SPIRV-Headers`
/// and `lib/cmake/...` — check every layout and pass the directory explicitly
/// so neither package needs a hand-edited environment.
fn spirv_headers_dir(sdk: &std::path::Path) -> Option<std::path::PathBuf> {
    let candidates = [
        sdk.join("Lib/cmake/SPIRV-Headers"),
        sdk.join("share/cmake/SPIRV-Headers"),
        sdk.join("lib/cmake/SPIRV-Headers"),
    ];
    candidates
        .into_iter()
        .find(|dir| dir.join("SPIRV-HeadersConfig.cmake").exists())
}

/// Return a unique ASCII-only CMake output directory on the Cargo target volume.
/// This is needed for the Vulkan shader-generator subproject on Windows/MSVC.
fn windows_ascii_cmake_out_dir() -> Option<std::path::PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let cargo_out = std::path::PathBuf::from(std::env::var_os("OUT_DIR")?);
    if cargo_out.to_string_lossy().is_ascii() {
        return None;
    }
    let build_id = cargo_out.parent()?.file_name()?;
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("CARGO_MANIFEST_DIR")
                .map(std::path::PathBuf::from)
                .map(|manifest| manifest.join("target"))
        })?;
    let mut volume_root = std::path::PathBuf::new();
    for component in target_dir.components().take(2) {
        volume_root.push(component.as_os_str());
    }
    if volume_root.as_os_str().is_empty() {
        return None;
    }
    Some(volume_root.join(".deepdark-oracle-cmake").join(build_id))
}
