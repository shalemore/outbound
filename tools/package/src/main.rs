//! Packages the executable that was built for a release: the file to download, its sha256 and the
//! Scoop manifest.
//!
//! Everything about the program comes from the root `Cargo.toml` (name, version, description,
//! license, repository and the `[package.metadata.scoop]` table) and from the build (the hash), so
//! none of it is written down a second time here.
//!
//! Usage: `package <exe> <out-dir>`, run from the root of the repository.

use std::path::Path;
use std::process::Command;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let [exe, out_dir] = args.as_slice() else {
        bail!("usage: package <exe> <out-dir>");
    };
    let package = read_package()?;
    let text = |key: &str| package[key].as_str().unwrap_or_default().to_string();
    let (name, version) = (text("name"), text("version"));

    let repository = text("repository");
    let repository = repository.trim_end_matches('/').trim_end_matches(".git");
    let is_github = repository
        .strip_prefix("https://github.com/")
        .is_some_and(|path| path.split('/').count() == 2);
    if !is_github {
        bail!("package.repository in Cargo.toml must be a GitHub URL, but it is {repository:?}");
    }
    let homepage = Some(text("homepage"))
        .filter(|homepage| !homepage.is_empty())
        .unwrap_or_else(|| repository.to_string());

    // The download keeps its name from one version to the next, and the release tag carries the
    // version.
    let asset = format!("{name}-x86_64-pc-windows-msvc.exe");
    fs::create_dir_all(out_dir)?;
    let asset_path = Path::new(out_dir).join(&asset);
    fs::copy(exe, &asset_path).with_context(|| format!("cannot copy {exe}"))?;
    let hash = Sha256::digest(fs::read(&asset_path)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // The same format as sha256sum, which is what the Scoop manifest reads the hash from.
    fs::write(
        format!("{}.sha256", asset_path.display()),
        format!("{hash}  {asset}\n"),
    )?;

    let download =
        |version: &str| format!("{repository}/releases/download/v{version}/{asset}#/{name}.exe");
    let mut manifest = json!({
        "version": version,
        "description": text("description"),
        "homepage": homepage,
        "license": text("license").replace(" OR ", "|"),
        "architecture": { "64bit": { "url": download(&version), "hash": hash } },
        "bin": format!("{name}.exe"),
    });

    let scoop = &package["metadata"]["scoop"];
    if let Some(shortcut) = scoop["shortcut"].as_str() {
        manifest["shortcuts"] = json!([[format!("{name}.exe"), shortcut]]);
    }
    if let Some(argument) = scoop["cleanup"].as_str() {
        // For a program that puts something on the machine outside its own folder: stop it, then
        // let it remove those things.
        manifest["pre_uninstall"] = json!([
            format!("Get-Process {name} -ErrorAction SilentlyContinue | Stop-Process"),
            format!(
                "Start-Process -FilePath \"$dir\\{name}.exe\" -ArgumentList '{argument}' -Wait"
            ),
        ]);
    }
    if let Some(notes) = scoop["notes"].as_str() {
        manifest["notes"] = json!(notes);
    }
    manifest["checkver"] = json!("github");
    manifest["autoupdate"] = json!({
        "architecture": { "64bit": {
            "url": download("$version"),
            "hash": { "url": "$baseurl.sha256" },
        } },
    });

    let manifest_path = Path::new(out_dir).join(format!("{name}.scoop.json"));
    fs::write(&manifest_path, to_scoop_json(&manifest)?)?;

    println!("Packaged {name} {version} in {out_dir}");
    for entry in fs::read_dir(out_dir)? {
        let entry = entry?;
        println!(
            "  {}  ({} bytes)",
            entry.file_name().to_string_lossy(),
            entry.metadata()?.len()
        );
    }
    Ok(())
}

fn read_package() -> Result<Value> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .context("cannot run cargo")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    metadata["packages"][0]
        .as_object()
        .map(|package| Value::Object(package.clone()))
        .context("cargo metadata lists no package")
}

/// Scoop manifests are indented with four spaces and end with a newline.
fn to_scoop_json(manifest: &Value) -> Result<String> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    manifest.serialize(&mut serde_json::Serializer::with_formatter(
        &mut out, formatter,
    ))?;
    out.push(b'\n');
    Ok(String::from_utf8(out)?)
}
