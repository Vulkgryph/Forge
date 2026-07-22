use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=shaders/egui.vert");
    println!("cargo:rerun-if-changed=shaders/egui.frag");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    compile("shaders/egui.vert", shaderc::ShaderKind::Vertex,   out_dir.join("egui.vert.spv"));
    compile("shaders/egui.frag", shaderc::ShaderKind::Fragment, out_dir.join("egui.frag.spv"));

    emit_forge_server_version();
}

/// `ssh.rs` needs forge-server's version to know whether the copy already on
/// a remote host is current, without depending on the forge-server crate
/// itself (a separate, cross-compiled-to-Linux binary, not something this
/// client links against). Reading its Cargo.toml here — instead of hand-
/// copying the version string into ssh.rs, which then has to be remembered
/// on every forge-server bump — makes forge-server/Cargo.toml the one
/// place that number actually lives.
fn emit_forge_server_version() {
    let path = "forge-server/Cargo.toml";
    println!("cargo:rerun-if-changed={path}");
    let text = fs::read_to_string(path).expect("failed to read forge-server/Cargo.toml");
    let value: toml::Value = text.parse().expect("failed to parse forge-server/Cargo.toml");
    let version = value["package"]["version"]
        .as_str()
        .expect("forge-server/Cargo.toml missing package.version");
    println!("cargo:rustc-env=FORGE_SERVER_VERSION={version}");
}

fn compile(src: &str, kind: shaderc::ShaderKind, out: PathBuf) {
    let source   = fs::read_to_string(src).expect("failed to read shader");
    let compiler = shaderc::Compiler::new().expect("shaderc unavailable");
    let mut opts = shaderc::CompileOptions::new().expect("compile options");
    opts.set_target_env(shaderc::TargetEnv::Vulkan, shaderc::EnvVersion::Vulkan1_3 as u32);
    opts.set_optimization_level(shaderc::OptimizationLevel::Performance);
    let artifact = compiler
        .compile_into_spirv(&source, kind, src, "main", Some(&opts))
        .unwrap_or_else(|e| panic!("compile {src}: {e}"));
    fs::write(out, artifact.as_binary_u8()).expect("write SPIR-V");
}
