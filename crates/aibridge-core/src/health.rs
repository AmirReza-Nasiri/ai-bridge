//! Installation health checks (executable discovery via the platform layer).

use aibridge_platform::{platform_name, DefaultPlatform, Platform};

fn find_line(name: &str) -> String {
    match DefaultPlatform::find_executable(name) {
        Ok(path) => format!("  [ok]      {name}: {}", path.display()),
        Err(_) => format!("  [missing] {name}: not found"),
    }
}

/// Human-readable health report (CLI discovery + platform).
pub fn report() -> String {
    let mut s = format!(
        "AI Bridge v{} health ({}):\n",
        crate::version(),
        platform_name()
    );
    for tool in ["claude", "codex", "rtk"] {
        s.push_str(&find_line(tool));
        s.push('\n');
    }
    s.push_str("Note: warm Codex peer + review gate are wired in a later increment.");
    s
}

/// Optional-capability report.
pub fn capability_report() -> String {
    let rtk = match DefaultPlatform::find_executable("rtk") {
        Ok(_) => "available",
        Err(_) => "not installed",
    };
    format!(
        "AI Bridge capabilities:\n  output_optimizer (rtk): {rtk}\n  profile-scoping: schema-only (v1)"
    )
}
