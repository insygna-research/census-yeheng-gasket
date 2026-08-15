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
        confine_landlock(cmd, cwd)
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

/// Landlock confinement for Linux: enforced in pre_exec so the ruleset
/// applies to the exec'd child and its whole process tree.
#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
fn confine_landlock(
    cmd: &mut tokio::process::Command,
    cwd: &std::path::Path,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let cwd = cwd
        .canonicalize()
        .map_err(|e| format!("sandbox: cwd not accessible: {e}"))?;
    let tmp = std::path::PathBuf::from(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()));
    // pre_exec runs between fork and exec: Landlock is inherited by the
    // exec'd child and its whole process tree. Owned paths (no borrows) so
    // the closure is Send + 'static.
    unsafe {
        cmd.as_std_mut()
            .pre_exec(move || landlock_ruleset(&cwd, &tmp).map_err(std::io::Error::other));
    }
    Ok(())
}

/// Read-only filesystem everywhere except cwd/TMPDIR (/var/tmp via a fourth
/// rule). Errors (unsupported kernel, missing paths) reach pre_exec and fail
/// the spawn -> fail-closed.
#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
fn landlock_ruleset(cwd: &std::path::Path, tmp: &std::path::Path) -> Result<(), String> {
    use landlock::{
        Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, ABI,
    };
    let read = AccessFs::from_read(ABI::V5);
    let read_write = AccessFs::from_all(ABI::V5);

    Ruleset::default()
        // Fail-closed: a kernel without Landlock (or missing the V1 core
        // access set) must error out here, not silently skip confinement.
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .map_err(|e| e.to_string())?
        // Everything beyond V1 (Refer, Truncate, IoctlDev, ...) is enforced
        // opportunistically where the running kernel supports it.
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(read_write)
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|e| e.to_string())?,
            read,
        ))
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new(cwd).map_err(|e| e.to_string())?,
            read_write,
        ))
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new(tmp).map_err(|e| e.to_string())?,
            read_write,
        ))
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new("/var/tmp").map_err(|e| e.to_string())?,
            read_write,
        ))
        .map_err(|e| e.to_string())?
        .no_new_privs(true)
        .restrict_self()
        .map_err(|e| e.to_string())?;
    Ok(())
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

// Landlock tests compile (and run) only on Linux with the feature on. On the
// macOS dev box they are verified via cross-target `cargo check`.
#[cfg(all(test, target_os = "linux", feature = "sandbox-landlock"))]
mod landlock_tests {
    use super::*;

    #[test]
    fn landlock_ruleset_builds_for_existing_paths() {
        let cwd = tempfile::tempdir().unwrap();
        // Enforcing here sandboxes only this test's thread (Landlock is
        // per-thread and libtest spawns one thread per test), and the ruleset
        // grants rw beneath the tempdir, so the test runner is unaffected.
        assert!(landlock_ruleset(cwd.path(), std::path::Path::new("/tmp")).is_ok());
    }
}

#[cfg(all(test, target_os = "linux", not(feature = "sandbox-landlock")))]
mod no_landlock_tests {
    use super::*;

    #[test]
    fn confine_without_feature_fails_closed_with_hint() {
        let mut cmd = tokio::process::Command::new("true");
        let err = confine(&mut cmd, std::path::Path::new("/tmp")).unwrap_err();
        assert!(err.contains("--features sandbox-landlock"), "{err}");
    }
}
