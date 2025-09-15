use anyhow::*;
use clap::{Parser, Subcommand};
use oxido_core::runtime::{run, Cartridge};
use serde::Deserialize;
use std::{fs, path::{Path, PathBuf}, process::Command};

#[derive(Parser)]
#[command(name = "oxido")]
#[command(about = "OxidoBoy CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Executes a game: accepts path to .wasm or .cart folder (with manifest.toml)
    Run {
        /// Route to .wasm or .cart folder
        #[arg(value_name = "PATH")]
        path: String,
        /// Width of framebuffer (used only if PATH is .wasm)
        #[arg(long, default_value_t = 160)]
        width: u32,
        /// Height of framebuffer (used only if PATH is .wasm)
        #[arg(long, default_value_t = 144)]
        height: u32,
        /// Window scale factor (pixel-perfect)
        #[arg(short, long, default_value_t = 3)]
        scale: u32,
    },
    /// Creates a new game (template) in a folder
    New {
        /// Game name and destination folder
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Package a game into a .cart folder (builds WASM and copies manifest/assets)
    Pack {
        /// Root folder of the game (where its Cargo.toml is)
        #[arg(value_name = "GAME_DIR")]
        game_dir: String,
        /// Output folder (.cart). Default: <GAME_DIR>/build/cart
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Deserialize)]
struct Manifest {
    title: Option<String>,
    version: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    /// binary name of the wasm inside the .cart (default "game.wasm")
    wasm: Option<String>,
    /// Optional window scale (pixel-perfect)
    scale: Option<u32>,                  
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { path, width, height,scale } => cmd_run(path, width, height,scale),
        Cmd::New { name } => cmd_new(name),
        Cmd::Pack { game_dir, out } => cmd_pack(game_dir, out),
    }
}

fn cmd_run(path: String, width: u32, height: u32, scale: u32) -> Result<()> {
    let p = Path::new(&path);

    if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("wasm") {
        // Run directly a wasm file
        return run(Cartridge { wasm_path: p.to_path_buf(), w: width, h: height,scale });
    }

    if p.is_dir() {
        // Upload .cart folder manifest
        let manifest_path = p.join("manifest.toml");
        let s = fs::read_to_string(&manifest_path)
            .with_context(|| format!("Could not be read {}", manifest_path.display()))?;
        let man: Manifest = toml::from_str(&s)
            .context("manifest.toml invalid")?;

        let w = man.width.unwrap_or(width);
        let h = man.height.unwrap_or(height);
        let s = man.scale.unwrap_or(scale);  
        let wasm_name = man.wasm.unwrap_or_else(|| "game.wasm".to_string());
        let wasm_path = p.join(wasm_name);

        return run(Cartridge { wasm_path, w, h , scale: s});
    }

    bail!("PATH must be a .wasm or a folder .cart");
}

fn cmd_new(name: String) -> Result<()> {
    let root = PathBuf::from(&name);
    let src_dir = root.join("src");
    let cart_dir = root.join("cart").join("assets");

    fs::create_dir_all(&src_dir)?;
    fs::create_dir_all(&cart_dir)?;

    // Cargo.toml of the game
    let cargo_toml = format!(r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
oxido_sdk = {{ path = "../../oxido_sdk" }}
"#);
    fs::write(root.join("Cargo.toml"), cargo_toml)?;

    // src/lib.rs (minimal game)
    let lib_rs = r#"use oxido_sdk::*;

static mut FB: [u8; DEFAULT_W * DEFAULT_H * 4] = [0; DEFAULT_W * DEFAULT_H * 4];
static mut INPUT_BITS: u32 = 0;
static mut X: f32 = 10.0;

const SPEED: f32 = 60.0;

#[no_mangle]
pub extern "C" fn oxido_init() {}

#[no_mangle]
pub extern "C" fn oxido_update(dt_ms: f32) {
    let dt = dt_ms / 1000.0;
    unsafe {
        if INPUT_BITS & key_bit(Key::Right) != 0 { X += SPEED * dt; }
        if INPUT_BITS & key_bit(Key::Left)  != 0 { X -= SPEED * dt; }
    }
}

#[no_mangle]
pub extern "C" fn oxido_draw_ptr() -> *const u8 {
    unsafe {
        let mut f = Frame { data: &mut FB, w: DEFAULT_W, h: DEFAULT_H };
        f.clear(P0);
        f.rect(X as i32, 60, 16, 16, P3);
        FB.as_ptr()
    }
}

#[no_mangle] pub extern "C" fn oxido_draw_len() -> usize { DEFAULT_W * DEFAULT_H * 4 }
#[no_mangle] pub extern "C" fn oxido_input_set(bits: u32) { unsafe { INPUT_BITS = bits; } }
"#;
    fs::write(src_dir.join("lib.rs"), lib_rs)?;

    // default manifest.toml
    
    let manifest = r#"title = "My Oxido Game"
version = "0.1.0"
width = 160
height = 144
scale = 3
wasm = "game.wasm"
"#;
    fs::write(root.join("cart").join("manifest.toml"), manifest)?;

    println!("✅ Game created in ./{name}");
    println!("Next:");
    println!("  1) cd {name}");
    println!("  2) rustup target add wasm32-unknown-unknown  # if you don't have it");
    println!("  3) cargo build --release --target wasm32-unknown-unknown");
    println!("  4) oxido pack .");
    println!("  5) oxido run build/cart   # run the folder .cart");
    Ok(())
}

fn cmd_pack(game_dir: String, out: Option<String>) -> Result<()> {
    let game = PathBuf::from(&game_dir);
    let cargo_toml = game.join("Cargo.toml");
    ensure!(cargo_toml.exists(), "Not found {}", cargo_toml.display());

    // Read the package name to locate the generated .wasm
    let cargo_str = fs::read_to_string(&cargo_toml)?;
    let pkg_name = parse_package_name(&cargo_str)
        .context("Could not find [package].name in Cargo.toml")?;

    // Compile to wasm (release)
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--target").arg("wasm32-unknown-unknown")
        .current_dir(&game)
        .status()?;
    ensure!(status.success(), "Game compilation failed");

    // Paths: in workspace, the artifacts go to <workspace>/target; outside, to <game>/target
    let ws_root = find_workspace_root(&game);
    let target_base = ws_root.unwrap_or_else(|| game.clone()).join("target");

    // Try first in workspace target, then in game's local target
    let candidate_a = target_base.join("wasm32-unknown-unknown/release")
        .join(format!("{pkg_name}.wasm"));
    let candidate_b = game.join("target/wasm32-unknown-unknown/release")
        .join(format!("{pkg_name}.wasm"));

    let wasm_src = if candidate_a.exists() {
        candidate_a
    } else if candidate_b.exists() {
        candidate_b
    } else {
        bail!(
            "Could not find wasm.\nSearched:\n  - {}\n  - {}",
            candidate_a.display(),
            candidate_b.display()
        );
    };

    // .cart output
    let out_dir = out.map(PathBuf::from)
        .unwrap_or_else(|| game.join("build/cart"));
    if out_dir.exists() {
        fs::remove_dir_all(&out_dir)?;
    }
    fs::create_dir_all(&out_dir)?;

    // manifest: use the one in <game>/cart/manifest.toml if exists, otherwise a default one
    let manifest_src = game.join("cart/manifest.toml");
    let manifest = if manifest_src.exists() {
        fs::read_to_string(&manifest_src)?
    } else {
        format!(r#"title = "{pkg}"
version = "0.1.0"
width = 160
height = 144
scale = 3
wasm = "game.wasm"
"#, pkg=pkg_name)
    };
    fs::write(out_dir.join("manifest.toml"), manifest)?;

    // copy the wasm as game.wasm
    fs::copy(&wasm_src, out_dir.join("game.wasm"))?;

    // copy assets if they exist
    let assets_src = game.join("cart/assets");
    let assets_dst = out_dir.join("assets");
    if assets_src.exists() {
        copy_dir_recursive(&assets_src, &assets_dst)?;
    }
    
    println!("✅ Cartridge generated in {}", out_dir.display());
    println!("To run: oxido run {}", out_dir.display());
    Ok(())
}

fn parse_package_name(cargo_toml: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Pkg { name: String }
    #[derive(Deserialize)]
    struct Root { package: Pkg }
    toml::from_str::<Root>(cargo_toml).ok().map(|r| r.package.name)
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    // Scroll up looking for a Cargo.toml with [workspace]
    for dir in start.ancestors() {
        let toml = dir.join("Cargo.toml");
        if toml.exists() {
            match std::fs::read_to_string(&toml) {
                std::result::Result::Ok(s) => {
                    if s.contains("[workspace]") {
                        return Some(dir.to_path_buf());
                    }
                }
                _ => { /* ignore reading errors and continue climbing */ }
            }
        }
    }
    None
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
