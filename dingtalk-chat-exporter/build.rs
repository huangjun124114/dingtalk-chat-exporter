use std::fs;

fn plist_value<'a>(plist: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("<key>{key}</key>");
    let after_key = plist.split_once(&marker)?.1;
    let after_open = after_key.split_once("<string>")?.1;
    Some(after_open.split_once("</string>")?.0.trim())
}

fn main() {
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=packaging/macos/Info.plist");

    let package_version = env!("CARGO_PKG_VERSION");
    let tauri_config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string("tauri.conf.json").expect("无法读取 tauri.conf.json"),
    )
    .expect("tauri.conf.json 不是有效 JSON");
    let tauri_version = tauri_config
        .get("version")
        .and_then(serde_json::Value::as_str)
        .expect("tauri.conf.json 缺少 version");
    let plist = fs::read_to_string("packaging/macos/Info.plist").expect("无法读取 Info.plist");
    let plist_version = plist_value(&plist, "CFBundleShortVersionString")
        .expect("Info.plist 缺少 CFBundleShortVersionString");

    assert_eq!(
        tauri_version, package_version,
        "版本不一致：tauri.conf.json={tauri_version}，Cargo.toml={package_version}"
    );
    assert_eq!(
        plist_version, package_version,
        "版本不一致：Info.plist={plist_version}，Cargo.toml={package_version}"
    );

    tauri_build::build()
}
