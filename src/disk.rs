//! Free-space probe and byte-size parsing for the disk guardrail.
//!
//! ## Why this module contains the crate's only `unsafe`
//!
//! The guardrail exists to stop the store wedging on a full volume — a known failure class
//! in embedded trace stores, where ingest runs until `ENOSPC` hits mid-write.
//! Preventing that means asking the OS how much room is left, and the standard library has
//! no stable API for it. So this module calls `statvfs(3)` through `libc`, in one function,
//! behind one `unsafe` block with a safety argument.
//!
//! Two alternatives were rejected. `rustix` offers a safe wrapper, but it reaches the tree
//! only through `tempfile`, a **dev**-dependency — taking it would add a crate to the
//! shipped binary. `libc` is already compiled into the release build (DataFusion pulls it),
//! so this costs **zero new crates**. Measuring evald's *own* directory size against a
//! budget instead would need neither, but it cannot see a co-tenant filling the volume —
//! which is the common way this failure actually happens — so it would not prevent the
//! failure the guardrail is named for.
//!
//! ## Failing open
//!
//! Every probe failure returns `None`, and the caller treats `None` as "no guardrail".
//! Blocking ingest because free space could not be *read* would invent an outage; the
//! honest posture is to degrade to the pre-guardrail behaviour and say so in a log line.
//! Non-Unix targets take that path unconditionally.

use std::path::Path;

/// Bytes available on the filesystem holding `path`, or `None` if that cannot be determined
/// (a failed syscall, or a platform without one).
///
/// Reports space available to an **unprivileged** process: `statvfs` also exposes `f_bfree`,
/// which includes the root reserve (typically 5% on ext4) that evald cannot actually write
/// into. Using `f_bfree` would let the guardrail believe it had room that `ENOSPC` would
/// then deny.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();

    // SAFETY: `c_path` is a valid NUL-terminated C string that outlives the call, and
    // `stat` is a correctly sized and aligned allocation for `statvfs`, which is the
    // callee's to initialize. The return value is checked before the struct is read, so
    // `assume_init` only runs on the success path where the kernel has written it.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };

    // f_frsize is the fragment size the block counts are expressed in. Both are widened
    // before multiplying because their types differ by platform — `fsblkcnt_t` and
    // `c_ulong` are 64-bit on linux-gnu but 32-bit on some targets evald builds for.
    //
    // The allow is load-bearing, not noise-suppression: on the platform CI happens to lint,
    // these are already u64 and clippy calls the conversion useless — but deleting it would
    // stop the code compiling on the targets where it is not. `try_from` over `as` so a
    // hypothetical wider type fails the probe (→ guardrail off) instead of truncating into
    // a small free-space figure, which would read as "disk full" and shed all ingest.
    #[allow(clippy::useless_conversion)]
    let avail = u64::try_from(stat.f_bavail).ok()?;
    #[allow(clippy::useless_conversion)]
    let frag = u64::try_from(stat.f_frsize).ok()?;
    Some(avail.saturating_mul(frag))
}

/// Non-Unix: no probe, so the guardrail stays disabled rather than guessing.
#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Parse a byte size: a bare number of bytes, or one suffixed `k`/`m`/`g`/`t` (binary —
/// `1k` is 1024). Case-insensitive, and a trailing `b`/`ib` is accepted so `512MiB`, `512MB`
/// and `512m` all mean the same thing.
///
/// Deliberately binary-only: an operator sizing a guardrail against `df` output is thinking
/// in the same units `df -h` prints, and silently treating `1g` as 10^9 would leave the
/// reserve 7% smaller than they asked for.
pub fn parse_bytes(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Err("empty byte size (use e.g. 512MiB, 2g, 1048576)".to_string());
    }
    // Strip an optional trailing "ib" or "b" so 512MiB / 512MB / 512M all parse.
    let t = t
        .strip_suffix("ib")
        .or_else(|| t.strip_suffix('b'))
        .unwrap_or(&t);
    let (digits, unit_shift) = match t.chars().last() {
        Some('k') => (&t[..t.len() - 1], 10),
        Some('m') => (&t[..t.len() - 1], 20),
        Some('g') => (&t[..t.len() - 1], 30),
        Some('t') => (&t[..t.len() - 1], 40),
        _ => (t, 0),
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid byte size {s:?} (use e.g. 512MiB, 2g, 1048576)"))?;
    value
        .checked_shl(unit_shift)
        .filter(|v| unit_shift == 0 || v >> unit_shift == value)
        .ok_or_else(|| format!("byte size {s:?} is too large"))
}

/// Render a byte count for a log line / CLI message (binary units, one decimal).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes_parse_in_binary_units() {
        assert_eq!(parse_bytes("0"), Ok(0));
        assert_eq!(parse_bytes("1048576"), Ok(1024 * 1024));
        assert_eq!(parse_bytes("1k"), Ok(1024));
        // The three spellings an operator might reasonably type must agree.
        assert_eq!(parse_bytes("512m"), Ok(512 * 1024 * 1024));
        assert_eq!(parse_bytes("512MB"), Ok(512 * 1024 * 1024));
        assert_eq!(parse_bytes("512MiB"), Ok(512 * 1024 * 1024));
        assert_eq!(parse_bytes("2G"), Ok(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes(" 4t "), Ok(4u64 << 40));
    }

    #[test]
    fn bad_byte_sizes_are_rejected_not_defaulted() {
        // A guardrail that silently reads a typo as 0 would disable itself, so every
        // malformed value must be an error the operator sees at startup.
        for bad in [
            "",
            "  ",
            "abc",
            "12x",
            "-5",
            "1.5g",
            "99999999999999999999t",
        ] {
            assert!(parse_bytes(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn free_bytes_reports_something_for_a_real_directory() {
        // Not asserting a value — this is a real filesystem — only that the probe works on
        // the platform running the tests, so a broken FFI signature fails here rather than
        // silently disabling the guardrail in production.
        let dir = tempfile::tempdir().unwrap();
        let free = free_bytes(dir.path());
        if cfg!(unix) {
            let free = free.expect("statvfs should succeed on a temp dir");
            assert!(free > 0, "a writable temp dir should report free space");
        }
    }

    #[test]
    fn free_bytes_on_a_missing_path_is_none_not_a_panic() {
        // The probe runs on a timer; a data-dir that vanishes must degrade to "no
        // guardrail", never take the process down.
        assert_eq!(free_bytes(Path::new("/nonexistent/evald/path/xyz")), None);
    }

    #[test]
    fn human_bytes_is_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }
}
