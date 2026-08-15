//! Filesystem confinement for the `bash` tool, enabled by GASKET_SANDBOX=1.
//! Fail-closed: if confinement cannot be applied, the command is refused.

/// Generate a Seatbelt (sandbox-exec) SBPL profile: allow everything broadly,
/// deny file writes everywhere except cwd / tmp / var/tmp. Pure function.
// Not referenced outside tests yet; `confine()` (next task) will call it.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
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
