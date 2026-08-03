// build.rs — 编译时注入 git commit hash + 构建时间到环境变量
// 让 ion --export 的 HTML 能显示精确版本（如 "0.4.0+5668a9b 2026-08-04"），
// 每次代码改动 commit 后版本号自动变，用户能确认新代码生效了。

use std::process::Command;

fn main() {
    // git short hash
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // build date (YYYY-MM-DD)
    let build_date = Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=ION_GIT_HASH={}", git_hash);
    println!("cargo:rustc-env=ION_BUILD_DATE={}", build_date);

    // 让 build.rs 在 git HEAD 变化时重新运行
    println!("cargo:rerun-if-changed=.git/HEAD");
}
