//! Builds the privileged-only BPF object without affecting default workspace builds.

use std::{env, error::Error, io, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn Error>> {
    if env::var_os("CARGO_FEATURE_PRIVILEGED").is_none() {
        return Ok(());
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing manifest dir")?);
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    let source = manifest.join("src");
    for file in ["marsh.bpf.c", "marsh.h", "classify.c"] {
        println!("cargo:rerun-if-changed={}", source.join(file).display());
    }
    println!("cargo:rerun-if-env-changed=MARSH_BPF_CLANG");
    let clang = env::var_os("MARSH_BPF_CLANG").unwrap_or_else(|| "clang".into());
    let version = Command::new(&clang).arg("--version").output().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("clang 15 or later (MARSH_BPF_CLANG, default clang) is required to build the BPF LSM object: {error}"),
        )
    })?;
    let version = String::from_utf8_lossy(&version.stdout);
    // BTF-defined maps, CO-RE relocations and BPF v3 atomics need a recent clang.
    let major = version
        .split("clang version ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .and_then(|major| major.parse::<u32>().ok());
    if major.is_none_or(|major| major < 15) {
        return Err(format!("the BPF LSM object needs clang 15 or later, found: {version}").into());
    }
    let target = match env::var("CARGO_CFG_TARGET_ENDIAN")?.as_str() {
        "little" => "bpfel",
        "big" => "bpfeb",
        _ => return Err("unsupported target endianness".into()),
    };
    let status = Command::new(&clang)
        .args(["-target", target, "-mcpu=v3", "-O2", "-g", "-Wall", "-Werror", "-c"])
        .arg(format!("-ffile-prefix-map={}=/marsh-bpf", manifest.display()))
        .arg(source.join("marsh.bpf.c"))
        .arg("-o")
        .arg(out.join("marsh.bpf.o"))
        .status()?;
    if !status.success() {
        return Err("compiling the BPF LSM programs failed; no unprivileged fallback is built".into());
    }
    // Linked only by the classifier test's `#[link]`, never into production binaries.
    #[cfg(feature = "privileged")]
    cc::Build::new()
        .file(source.join("classify.c"))
        .warnings_into_errors(true)
        .cargo_metadata(false)
        .out_dir(&out)
        .compile("marsh_classify");
    println!("cargo:rustc-link-search=native={}", out.display());
    Ok(())
}
