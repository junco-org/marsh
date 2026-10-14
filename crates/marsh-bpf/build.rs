//! Builds the privileged-only artifact without affecting default workspace builds.

use std::{env, error::Error, fs, io, path::PathBuf, process::Command};

const NIGHTLY: &str = "nightly-2025-12-01";

fn main() -> Result<(), Box<dyn Error>> {
    if env::var_os("CARGO_FEATURE_PRIVILEGED").is_none() {
        return Ok(());
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing manifest dir")?);
    let source = manifest.join("../marsh-bpf-ebpf");
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "rust-toolchain.toml",
        "src/main.rs",
        "src/peer.c",
    ] {
        println!("cargo:rerun-if-changed={}", source.join(path).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("../marsh-peer-class/src").display()
    );
    println!("cargo:rerun-if-env-changed=MARSH_BPF_CLANG");
    let version = Command::new("bpf-linker")
        .arg("--version")
        .output()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("bpf-linker 0.9.15 is required: {error}"),
            )
        })?;
    if !version.status.success()
        || String::from_utf8_lossy(&version.stdout).trim() != "bpf-linker 0.9.15"
    {
        return Err("privileged enforcement requires bpf-linker =0.9.15 (LLVM 21)".into());
    }
    let target = match env::var("CARGO_CFG_TARGET_ENDIAN")?.as_str() {
        "little" => "bpfel-unknown-none",
        "big" => "bpfeb-unknown-none",
        _ => return Err("unsupported target endianness".into()),
    };
    let arch = env::var("CARGO_CFG_TARGET_ARCH")?;
    let target_dir = out.join("ebpf-target");
    let root = manifest.join("../..").canonicalize()?;
    let rustflags = format!(
        "-C\u{1f}linker=bpf-linker\u{1f}-C\u{1f}debuginfo=2\u{1f}-C\u{1f}link-arg=--btf\u{1f}-C\u{1f}target-cpu=v3\u{1f}--cfg\u{1f}bpf_target_arch=\"{arch}\"\u{1f}--remap-path-prefix={}=/marsh-build\u{1f}--remap-path-prefix={}=/marsh",
        out.display(),
        root.display()
    );
    let status = Command::new("rustup")
        .args([
            "run",
            NIGHTLY,
            "cargo",
            "build",
            "--locked",
            "--release",
            "--ignore-rust-version",
            "--target",
            target,
            "-Z",
            "build-std=core",
            "--manifest-path",
        ])
        .arg(source.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        // Cargo's host compiler and flags must not leak into the nested BPF build.
        .env_remove("RUSTC")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTFLAGS")
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags)
        .env_remove("CARGO_CFG_BPF_TARGET_ARCH")
        .status()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("install {NIGHTLY} with rust-src: {error}"),
            )
        })?;
    if !status.success() {
        return Err(format!("BPF build failed; requires {NIGHTLY} + rust-src, clang-21, bpf-linker =0.9.15; no unprivileged fallback").into());
    }
    fs::copy(
        target_dir.join(target).join("release/marsh-bpf-ebpf"),
        out.join("marsh-bpf.o"),
    )?;
    Ok(())
}
