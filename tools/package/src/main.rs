use std::path::Path;
use std::process::Command;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let (out_dir, exes) = args
        .split_last()
        .context("usage: package <exe>... <out-dir>")?;
    if exes.is_empty() {
        bail!("usage: package <exe>... <out-dir>");
    }
    let pkg = read_package()?;
    let text = |k| pkg[k].as_str().unwrap_or_default();
    let (name, version) = (text("name"), text("version"));
    let repo = text("repository")
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let homepage = match text("homepage") {
        "" => repo,
        h => h,
    };

    fs::create_dir_all(out_dir)?;
    let (mut architecture, mut autoupdate) = (json!({}), json!({}));

    for exe in exes {
        let (target, arch) = if exe.contains("aarch64") {
            ("aarch64-pc-windows-msvc", "arm64")
        } else {
            ("x86_64-pc-windows-msvc", "64bit")
        };
        let asset = format!("{name}-{target}.exe");
        let dest = Path::new(out_dir).join(&asset);
        fs::copy(exe, &dest).with_context(|| format!("cannot copy {exe}"))?;
        let hash = Sha256::digest(fs::read(&dest)?)
            .into_iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        fs::write(
            format!("{}.sha256", dest.display()),
            format!("{hash}  {asset}\n"),
        )?;

        let url = |v: &str| format!("{repo}/releases/download/v{v}/{asset}#/{name}.exe");
        architecture[arch] = json!({ "url": url(version), "hash": hash });
        autoupdate[arch] = json!({ "url": url("$version"), "hash": { "url": "$baseurl.sha256" } });
    }

    let mut manifest = json!({
        "version": version,
        "description": text("description"),
        "homepage": homepage,
        "license": text("license").replace(" OR ", "|"),
        "architecture": architecture,
        "bin": format!("{name}.exe"),
    });

    let scoop = &pkg["metadata"]["scoop"];
    if let Some(s) = scoop["shortcut"].as_str() {
        manifest["shortcuts"] = json!([[format!("{name}.exe"), s]]);
    }
    if let Some(arg) = scoop["cleanup"].as_str() {
        manifest["pre_uninstall"] = json!([
            format!("Get-Process {name} -ErrorAction SilentlyContinue | Stop-Process"),
            format!(
                "if ($cmd -eq 'uninstall') {{ Start-Process -FilePath \"$dir\\{name}.exe\" -ArgumentList '{arg}' -Wait }}"
            ),
        ]);
    }
    if let Some(notes) = scoop["notes"].as_str() {
        manifest["notes"] = json!(notes);
    }
    manifest["checkver"] = json!("github");
    manifest["autoupdate"] = json!({ "architecture": autoupdate });

    fs::write(
        Path::new(out_dir).join(format!("{name}.scoop.json")),
        to_scoop_json(&manifest)?,
    )?;
    println!("Packaged {name} {version} in {out_dir}");
    Ok(())
}

fn read_package() -> Result<Value> {
    let out = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    let mut val: Value = serde_json::from_slice(&out.stdout)?;
    val["packages"]
        .as_array_mut()
        .and_then(|p| p.pop())
        .context("no package found")
}

fn to_scoop_json(manifest: &Value) -> Result<String> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(
        &mut out,
        serde_json::ser::PrettyFormatter::with_indent(b"    "),
    );
    manifest.serialize(&mut ser)?;
    out.push(b'\n');
    Ok(String::from_utf8(out)?)
}
