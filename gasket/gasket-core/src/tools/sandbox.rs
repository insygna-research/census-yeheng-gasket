//! Filesystem confinement for the `bash` tool, enabled by GASKET_SANDBOX=1.
//! Fail-closed: if confinement cannot be applied, the command is refused.

/// Generate a Seatbelt (sandbox-exec) SBPL profile: allow everything broadly,
/// deny file writes everywhere except cwd / tmp / var/tmp. Pure function.
#[cfg(target_os = "macos")]
fn seatbelt_profile(cwd: &str, tmp: &str) -> String {
    format!(
        "(version 1)\n\
         (allow default)\n\
         (deny file-write*)\n\
         (allow file-write* (subpath \"{cwd}\"))\n\
         (allow file-write* (subpath \"{tmp}\"))\n\
         (allow file-write* (subpath \"/var/tmp\"))\n"
    )
}

/// Read the sandbox flag from the ToolContext env map (host-populated from
/// the process env). Exact "1" only — no truthy-string guessing.
pub(crate) fn sandbox_enabled(env: &std::collections::HashMap<String, String>) -> bool {
    env.get("GASKET_SANDBOX").map(String::as_str) == Some("1")
}

/// Apply filesystem confinement to `cmd`. MUST be called before cwd/env are
/// set on `cmd` (the macOS branch rewrites program+args wholesale).
/// Err = fail-closed: the caller must refuse to run the command.
pub(crate) fn confine(
    cmd: &mut tokio::process::Command,
    cwd: &std::path::Path,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let cwd_c = cwd
            .canonicalize()
            .map_err(|e| format!("sandbox: cwd not accessible: {e}"))?;
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        let profile = seatbelt_profile(&cwd_c.display().to_string(), &tmp);
        let std_cmd = cmd.as_std_mut();
        let program = std_cmd.get_program().to_os_string();
        let args: Vec<_> = std_cmd.get_args().map(std::ffi::OsString::from).collect();
        *cmd = tokio::process::Command::new("sandbox-exec");
        cmd.arg("-p").arg(&profile).arg(program).args(args);
        Ok(())
    }
    #[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
    {
        let _ = (cmd, cwd);
        Err("sandbox: landlock support not yet built".to_string())
    }
    #[cfg(all(target_os = "linux", not(feature = "sandbox-landlock")))]
    {
        let _ = (cmd, cwd);
        Err("GASKET_SANDBOX=1 but this build lacks the landlock backend; rebuild gasket-core with --features sandbox-landlock".to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (cmd, cwd);
        Err("sandbox unsupported on this platform".into())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn profile_allows_read_execute_and_denies_write_by_default() {
        let p = seatbelt_profile("/tmp/cwd", "/tmp/dir");
        assert!(p.contains("(version 1)"), "{p}");
        assert!(p.contains("(allow default)"), "read/exec broadly: {p}");
        assert!(p.contains("(deny file-write*)"), "deny writes: {p}");
        assert!(
            p.contains("(allow file-write* (subpath \"/tmp/cwd\"))"),
            "{p}"
        );
        assert!(
            p.contains("(allow file-write* (subpath \"/tmp/dir\"))"),
            "{p}"
        );
    }

    #[test]
    fn profile_includes_var_tmp_unconditionally() {
        let p = seatbelt_profile("/x", "/y");
        assert!(
            p.contains("(allow file-write* (subpath \"/var/tmp\"))"),
            "{p}"
        );
    }
}

// The env-flag logic is cross-platform, so it gets its own non-gated test
// module (the `tests` module above is macOS-only for the seatbelt profile).
#[cfg(test)]
mod flag_tests {
    use super::*;

    #[test]
    fn sandbox_enabled_only_on_exact_flag() {
        let mut env = std::collections::HashMap::new();
        assert!(!sandbox_enabled(&env));
        env.insert("GASKET_SANDBOX".to_string(), "0".to_string());
        assert!(!sandbox_enabled(&env));
        env.insert("GASKET_SANDBOX".to_string(), "1".to_string());
        assert!(sandbox_enabled(&env));
    }
}
