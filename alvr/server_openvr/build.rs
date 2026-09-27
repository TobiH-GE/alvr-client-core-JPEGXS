use std::{env, path::PathBuf};

fn get_ffmpeg_path() -> PathBuf {
    let ffmpeg_path = alvr_filesystem::deps_dir()
        .join(if cfg!(target_os = "linux") {
            "linux"
        } else {
            "windows"
        })
        .join("ffmpeg");

    if cfg!(target_os = "linux") {
        ffmpeg_path.join("alvr_build")
    } else {
        ffmpeg_path
    }
}

#[cfg(all(target_os = "linux", feature = "gpl"))]
fn get_linux_x264_path() -> PathBuf {
    alvr_filesystem::deps_dir().join("linux/x264/alvr_build")
}

// Optional dependency for the JPEG XS codec (see
// cpp/platform/win32/VideoEncoderJpegXs.h). Not part of any normal ALVR
// build: this is only picked up if the user has built Intel's open source
// SVT-JPEG-XS (https://github.com/OpenVisualCloud/SVT-JPEG-XS) and placed
// the headers/lib here. See deps/windows/svt-jpegxs/README.md.
fn get_svt_jpegxs_path() -> PathBuf {
    alvr_filesystem::deps_dir().join("windows/svt-jpegxs")
}

fn main() {
    let platform_name = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let platform_subpath = match platform_name.as_str() {
        "windows" => "cpp/platform/win32",
        "linux" => "cpp/platform/linux",
        "macos" => "cpp/platform/macos",
        _ => panic!(),
    };

    let common_iter = walkdir::WalkDir::new("cpp")
        .into_iter()
        .filter_entry(|entry| {
            entry.file_name() != "tools"
                && entry.file_name() != "platform"
                && (platform_name != "macos" || entry.file_name() != "amf")
                && (platform_name != "linux" || entry.file_name() != "amf")
        });

    let platform_iter = walkdir::WalkDir::new(platform_subpath).into_iter();

    let cpp_paths = common_iter
        .chain(platform_iter)
        .filter_map(|maybe_entry| maybe_entry.ok())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();

    let source_files_paths = cpp_paths.iter().filter(|path| {
        path.extension()
            .filter(|ext| {
                let ext_str = ext.to_string_lossy();
                ext_str == "c" || ext_str == "cpp"
            })
            .is_some()
    });

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .files(source_files_paths)
        .include(alvr_filesystem::workspace_dir().join("openvr/headers"))
        .include("cpp");

    if platform_name == "windows" {
        build
            .debug(false) // This is because we cannot link to msvcrtd (see below)
            .flag("/permissive-")
            .define("NOMINMAX", None)
            .define("_WINSOCKAPI_", None)
            .define("_MBCS", None)
            .define("_MT", None);
    } else if platform_name == "macos" {
        build.define("__APPLE__", None);
    }

    #[cfg(debug_assertions)]
    build.define("ALVR_DEBUG_LOG", None);

    let gpl_or_linux = cfg!(feature = "gpl") || cfg!(target_os = "linux");

    if gpl_or_linux {
        let ffmpeg_path = get_ffmpeg_path();

        assert!(ffmpeg_path.join("include").exists());
        build.include(ffmpeg_path.join("include"));
    }

    #[cfg(all(target_os = "linux", feature = "gpl"))]
    {
        let x264_path = get_linux_x264_path();

        assert!(x264_path.join("include").exists());
        build.include(x264_path.join("include"));
    }

    #[cfg(feature = "gpl")]
    build.define("ALVR_GPL", None);

    // JPEG XS side-by-side test encoder: auto-enabled only when the
    // (manually built) SVT-JPEG-XS headers are present, so this never
    // affects a normal build/checkout that doesn't have that dependency.
    let jpegxs_path = get_svt_jpegxs_path();
    let has_jpegxs = platform_name == "windows" && jpegxs_path.join("include").exists();
    if has_jpegxs {
        println!(
            "cargo:warning=Building with JPEG XS test encoder enabled ({})",
            jpegxs_path.to_string_lossy()
        );
        build.include(jpegxs_path.join("include"));
        build.define("ALVR_JPEGXS", None);
    }

    build.compile("bindings");

    #[cfg(all(target_os = "linux", feature = "gpl"))]
    {
        let x264_path = get_linux_x264_path();
        let x264_lib_path = x264_path.join("lib");

        println!(
            "cargo:rustc-link-search=native={}",
            x264_lib_path.to_string_lossy()
        );

        let x264_pkg_path = x264_lib_path.join("pkgconfig");
        assert!(x264_pkg_path.exists());

        let x264_pkg_path = x264_pkg_path.to_string_lossy().to_string();
        env::set_var(
            "PKG_CONFIG_PATH",
            env::var("PKG_CONFIG_PATH").map_or(x264_pkg_path.clone(), |old| {
                format!("{x264_pkg_path}:{old}")
            }),
        );
        println!("cargo:rustc-link-lib=static=x264");

        pkg_config::Config::new()
            .statik(true)
            .probe("x264")
            .unwrap();
    }

    // ffmpeg
    if gpl_or_linux {
        let ffmpeg_path = get_ffmpeg_path();
        let ffmpeg_lib_path = ffmpeg_path.join("lib");

        assert!(ffmpeg_lib_path.exists());

        println!(
            "cargo:rustc-link-search=native={}",
            ffmpeg_lib_path.to_string_lossy()
        );

        #[cfg(target_os = "linux")]
        {
            let ffmpeg_pkg_path = ffmpeg_lib_path.join("pkgconfig");
            assert!(ffmpeg_pkg_path.exists());

            let ffmpeg_pkg_path = ffmpeg_pkg_path.to_string_lossy().to_string();
            env::set_var(
                "PKG_CONFIG_PATH",
                env::var("PKG_CONFIG_PATH").map_or(ffmpeg_pkg_path.clone(), |old| {
                    format!("{ffmpeg_pkg_path}:{old}")
                }),
            );

            let pkg = pkg_config::Config::new().statik(true).to_owned();

            for lib in ["libavutil", "libavfilter", "libavcodec"] {
                pkg.probe(lib).unwrap();
            }
        }
        #[cfg(windows)]
        for lib in ["avutil", "avfilter", "avcodec", "swscale"] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }

    // svt-jpegxs (see the `has_jpegxs` block above)
    if has_jpegxs {
        let jpegxs_lib_path = jpegxs_path.join("lib");
        assert!(
            jpegxs_lib_path.exists(),
            "ALVR_JPEGXS enabled but {} is missing. See deps/windows/svt-jpegxs/README.md.",
            jpegxs_lib_path.to_string_lossy()
        );

        println!(
            "cargo:rustc-link-search=native={}",
            jpegxs_lib_path.to_string_lossy()
        );
        println!("cargo:rustc-link-lib=SvtJpegxs");
    }

    bindgen::builder()
        .clang_arg("-xc++")
        .header("cpp/alvr_server/bindings.h")
        .derive_default(true)
        .generate()
        .unwrap()
        .write_to_file(out_dir.join("bindings.rs"))
        .unwrap();

    if platform_name == "linux" {
        println!(
            "cargo:rustc-link-search=native={}",
            alvr_filesystem::workspace_dir()
                .join("openvr/lib/linux64")
                .to_string_lossy()
        );
        println!("cargo:rustc-link-lib=openvr_api");
    } else if platform_name == "windows" {
        println!(
            "cargo:rustc-link-search=native={}",
            alvr_filesystem::workspace_dir()
                .join("openvr/lib/win64")
                .to_string_lossy()
        );
        println!("cargo:rustc-link-lib=openvr_api");
    }

    #[cfg(target_os = "linux")]
    {
        pkg_config::Config::new().probe("vulkan").unwrap();

        #[cfg(not(feature = "gpl"))]
        {
            pkg_config::Config::new().probe("x264").unwrap();
        }

        // fail build if there are undefined symbols in final library
        println!("cargo:rustc-cdylib-link-arg=-Wl,--no-undefined");
    }

    for path in cpp_paths {
        println!("cargo:rerun-if-changed={}", path.to_string_lossy());
    }
}
