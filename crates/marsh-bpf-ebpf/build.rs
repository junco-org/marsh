use std::{env, error::Error, io, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=src/peer.c");
    println!("cargo:rerun-if-env-changed=MARSH_BPF_CLANG");
    let clang = env::var_os("MARSH_BPF_CLANG").unwrap_or_else(|| "clang-21".into());
    let version = Command::new(&clang)
        .arg("--version")
        .output()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("clang-21 is required for CO-RE accessors: {error}"),
            )
        })?;
    if !version.status.success()
        || !String::from_utf8_lossy(&version.stdout).contains("clang version 21.")
    {
        return Err(
            "CO-RE bitcode must use clang 21, matching the pinned Rust LLVM and bpf-linker".into(),
        );
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing manifest dir")?);
    let target = if env::var("CARGO_CFG_TARGET_ENDIAN")? == "little" {
        "bpfel"
    } else {
        "bpfeb"
    };
    let object = out.join("peer.bc");
    let status = Command::new(clang)
        .args([
            "-target",
            target,
            "-O2",
            "-g",
            "-emit-llvm",
            "-c",
            "-fno-stack-protector",
            "-Wall",
            "-Werror",
            "-fdebug-compilation-dir=/marsh-bpf-ebpf",
        ])
        .arg(format!(
            "-ffile-prefix-map={}=/marsh-bpf-ebpf",
            manifest.display()
        ))
        .arg(manifest.join("src/peer.c"))
        .arg("-o")
        .arg(&object)
        .status()?;
    if !status.success() {
        return Err("compiling the BTF/CO-RE socket peer accessors failed".into());
    }
    println!("cargo:rustc-link-arg={}", object.display());
    Ok(())
}
