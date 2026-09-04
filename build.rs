// 把产品图标编译进 exe 的资源段, 让资源管理器 / 快捷方式 / Alt-Tab 显示它,
// 而不是通用白框图标。
//
// 图标在**编译期**从 src/icon.rs 现生成(写进 OUT_DIR), 不入库 —— 与 --write-ico、
// 托盘图标共用同一份光栅化代码, 三者不可能画得不一样。icon.rs 是纯 std(不碰
// windows crate), 所以 build.rs 能直接 `#[path]` include 它。

#[path = "src/icon.rs"]
#[allow(dead_code)] // build.rs 只用到 product_ico, 其余 paint/Look 等在这里未使用
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");

    // 只在 Windows 目标下嵌入资源
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR 未设置");
    let ico_path = std::path::Path::new(&out_dir).join("stayawake.ico");
    std::fs::write(&ico_path, icon::product_ico()).expect("生成 .ico 失败");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("ico 路径非 UTF-8"));
    if let Err(e) = res.compile() {
        // rc.exe 缺失等情况: 不因为嵌不了图标就让整个构建失败, 只警告。
        // exe 照常能用, 只是在资源管理器里显示通用图标。
        println!("cargo:warning=图标未能嵌入(exe 仍可用): {e}");
    }
}
