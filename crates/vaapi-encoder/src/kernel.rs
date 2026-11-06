//! Kernel lock: H.264 encode hangs the RX 580's VCE on kernels 7.2.2 and later (seen on 7.2.11,
//! reported as fixed in 7.3-rc2), while linux-lts 6.18 works. Encoding is only allowed on 6.18.

/// Only kernel line where H.264 encode was confirmed not to hang the VCE on this machine.
pub const SAFE_KERNEL_PREFIX: &str = "6.18.";

/// Whether encoding is allowed on a kernel with this release string (`uname -r`).
pub fn encode_allowed(release: &str) -> bool {
    release.trim().starts_with(SAFE_KERNEL_PREFIX)
}

/// Release string of the running kernel.
pub fn running_release() -> std::io::Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/osrelease")?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_allowed_only_on_6_18() {
        assert!(encode_allowed("6.18.60-1-lts"));
        assert!(encode_allowed("6.18.60-1-lts\n"));
        assert!(!encode_allowed("7.2.11-arch1-1"));
        assert!(!encode_allowed("6.1.100-1-lts"));
        // "6.180" must not pass as 6.18
        assert!(!encode_allowed("6.180.1"));
        assert!(!encode_allowed(""));
    }
}
