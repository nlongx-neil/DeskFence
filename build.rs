//! 把 `assets/deskkeeper.ico` 作为 ID 1 图标资源链接进 exe，
//! 使资源管理器「属性 → 图标」显示新图标（窗口/托盘图标另由运行期加载）。
//!
//! 资源编译使用 GNU `windres`（MinGW/binutils 自带，需在 PATH 中）；
//! 未安装时只给出 warning，不阻断构建（此时仅 exe 文件图标缺失）。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/deskkeeper.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = manifest.join("assets").join("deskkeeper.ico");
    if !ico.exists() {
        println!("cargo:warning=缺少 {}, 跳过 exe 图标资源", ico.display());
        return;
    }

    let rc = out.join("deskkeeper_res.rc");
    // .rc 里反斜杠是转义符，统一改用正斜杠（windres 在 Windows 上可识别）。
    let ico_slashes = ico.display().to_string().replace('\\', "/");
    let rc_content = format!("1 ICON \"{ico_slashes}\"\n");
    if let Err(e) = std::fs::write(&rc, rc_content) {
        println!("cargo:warning=写入 rc 失败: {e}");
        return;
    }

    let obj = out.join("deskkeeper_res.o");
    let windres = std::env::var_os("WINDRES").unwrap_or_else(|| "windres".into());
    let mut cmd = Command::new(&windres);
    // windres 是控制台程序：构建终端已带控制台时无影响，但在无控制台的构建宿主
    // （IDE 后台构建等）里会短暂闪出一个黑框。构建期统一用 CREATE_NO_WINDOW。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let status = cmd
        .arg("-O")
        .arg("coff")
        .arg(format!("-I{}", manifest.display()))
        .arg("-o")
        .arg(&obj)
        .arg(&rc)
        .status();
    match status {
        Ok(s) if s.success() && obj.exists() => {
            println!("cargo:rustc-link-arg={}", obj.display());
            println!("cargo:warning=已链接 exe 图标资源（windres）");
        }
        Ok(s) => println!(
            "cargo:warning=windres 退出码 {:?}，exe 图标未嵌入",
            s.code()
        ),
        Err(e) => {
            println!("cargo:warning=未找到 windres（{e}），exe 图标未嵌入；可设置 WINDRES 环境变量")
        }
    }
}
