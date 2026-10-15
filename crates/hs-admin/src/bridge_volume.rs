//! The volume of a bridge this server deploys: its default size, the size an administrator may
//! set (`PUT /bridge-offerings/{type}/instances/{user_id}` with `volume_size`), and how full it
//! is, in the words the admin API and the management interface use ([`view`]).
//!
//! The demo's Slack bridge stopped on 2026-10-14 when its 1 GiB volume filled up, and the only
//! sign was "degraded" on the Registrations page. The operator now measures each bridge's
//! volume into its `Bridge`'s `status.volume` (`hs-operator`'s `volume` module); the bridge
//! manager reads it from there into [`BridgeVolume`], which says `nearly_full` from
//! [`NEARLY_FULL`] used, `full` from [`FULL`] or when the bridge says it could not write ("No
//! space left on device"), and what to do.

use crate::model::{BridgeDeployment, BridgeVolume};

/// The size of a deployed bridge's volume unless an administrator sets another. Kept equal to
/// `hs_operator::crds::bridge::DEFAULT_STORAGE_SIZE` (a test in `hs-cli` checks). Why 5 GiB:
/// a mautrix bridge keeps its SQLite database there, which grows with every chat it bridges, and
/// until 2026-10-15 also up to 1.1 GB of its own log files (`logs/bridge.log` and ten 100 MB
/// backups, its default); the demo's Slack bridge, logging at debug, filled 1 GiB in under
/// three days. A Longhorn (or any thin-provisioned) volume takes only what is written.
pub const DEFAULT_SIZE: &str = "5Gi";
/// From this fraction used a volume is nearly full. The same as `hs-operator`'s.
pub const NEARLY_FULL: f64 = 0.85;
/// From this fraction used a volume is full. The same as `hs-operator`'s.
pub const FULL: f64 = 0.95;
/// The smallest size an administrator may set.
pub const MIN_SIZE_BYTES: u64 = 1 << 30;
/// The largest size an administrator may set.
pub const MAX_SIZE_BYTES: u64 = 1 << 40;

/// The bytes a Kubernetes quantity stands for (`5Gi`, `500Mi`, `2G`, `1.5Gi`, `1e9`), rounded
/// up; `None` for anything else. The same grammar as `hs-operator`'s `quantity::bytes`, which
/// compares sizes on the cluster's side.
#[must_use]
pub fn quantity_bytes(quantity: &str) -> Option<u64> {
    let q = quantity.trim();
    let split = q
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+'))
        .unwrap_or(q.len());
    let (number, suffix) = q.split_at(split);
    let number = number.strip_prefix('+').unwrap_or(number);
    if number.is_empty() || number == "." || number.matches('.').count() > 1 {
        return None;
    }
    let (multiplier, divisor): (u128, u128) = match suffix {
        "" => (1, 1),
        "Ki" => (1 << 10, 1),
        "Mi" => (1 << 20, 1),
        "Gi" => (1 << 30, 1),
        "Ti" => (1 << 40, 1),
        "Pi" => (1 << 50, 1),
        "Ei" => (1 << 60, 1),
        "m" => (1, 1000),
        "k" => (1_000, 1),
        "M" => (1_000_000, 1),
        "G" => (1_000_000_000, 1),
        "T" => (1_000_000_000_000, 1),
        "P" => (1_000_000_000_000_000, 1),
        "E" => (1_000_000_000_000_000_000, 1),
        exp if exp.starts_with(['e', 'E']) => (10u128.checked_pow(exp[1..].parse().ok()?)?, 1),
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits = format!("{whole}{fraction}");
    let mantissa: u128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    let scale = 10u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let bytes = mantissa
        .checked_mul(multiplier)?
        .div_ceil(scale.checked_mul(divisor)?);
    u64::try_from(bytes).ok()
}

/// Checks a size an administrator asked for: a whole number of `Mi`, `Gi` or `Ti` between
/// [`MIN_SIZE_BYTES`] and [`MAX_SIZE_BYTES`], and not less than `current` (what the volume has
/// now, when known). Returns it trimmed, with its bytes.
///
/// # Errors
/// A sentence saying what is wrong, for a `400` naming `/volume_size`.
pub fn parse_size(size: &str, current: Option<&str>) -> Result<(String, u64), String> {
    let size = size.trim();
    let whole = size
        .strip_suffix("Gi")
        .or_else(|| size.strip_suffix("Mi"))
        .or_else(|| size.strip_suffix("Ti"))
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    let bytes = quantity_bytes(size).filter(|_| whole).ok_or_else(|| {
        format!("{size:?} is not a size: give a whole number of Mi, Gi or Ti, such as 10Gi")
    })?;
    if bytes < MIN_SIZE_BYTES {
        return Err(format!("{size} is less than the smallest volume, 1Gi"));
    }
    if bytes > MAX_SIZE_BYTES {
        return Err(format!("{size} is more than the largest volume, 1Ti"));
    }
    if let Some(current) = current
        && let Some(current_bytes) = quantity_bytes(current)
        && bytes < current_bytes
    {
        return Err(format!(
            "the volume has {current} now, and a volume cannot shrink: give {current} or more"
        ));
    }
    Ok((size.to_owned(), bytes))
}

/// Whether a bridge's own words (its deployment's message: the last line it printed before it
/// stopped) say it could not write to its volume.
#[must_use]
pub fn says_disk_full(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no space left on device") || lower.contains("database or disk is full")
}

/// `bytes` for a person: `973 MiB`, `0.98 GiB`, `10 GiB`.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // for display
    let b = bytes as f64;
    let gib = b / f64::from(1u32 << 30);
    if gib >= 10.0 {
        format!("{gib:.0} GiB")
    } else if gib >= 1.0 {
        format!("{gib:.1} GiB").replace(".0 ", " ")
    } else {
        format!("{:.0} MiB", b / f64::from(1u32 << 20))
    }
}

/// The [`BridgeVolume`] of a deployed bridge: `size` is what this server asks for (the size set
/// for the instance, `size_set`, or [`DEFAULT_SIZE`]), `deployment` what the cluster says.
#[must_use]
pub fn view(size: &str, size_set: bool, deployment: Option<&BridgeDeployment>) -> BridgeVolume {
    let cluster_size = deployment
        .and_then(|d| d.storage_size.clone())
        .filter(|cluster| {
            matches!(
                (quantity_bytes(cluster), quantity_bytes(size)),
                (Some(c), Some(s)) if c > s
            )
        });
    let capacity_bytes = deployment.and_then(|d| d.volume_capacity_bytes);
    let used_bytes = deployment.and_then(|d| d.volume_used_bytes);
    #[allow(clippy::cast_precision_loss)] // a ratio
    let used_ratio = match (used_bytes, capacity_bytes) {
        (Some(used), Some(capacity)) if capacity > 0 => {
            Some((used as f64 / capacity as f64).clamp(0.0, 1.0))
        }
        _ => None,
    };
    let disk_full = deployment
        .and_then(|d| d.message.as_deref())
        .is_some_and(says_disk_full);
    let state = if disk_full || used_ratio.is_some_and(|r| r >= FULL) {
        "full"
    } else if used_ratio.is_some_and(|r| r >= NEARLY_FULL) {
        "nearly_full"
    } else if used_ratio.is_some() {
        "ok"
    } else {
        "unknown"
    };
    let has = cluster_size.as_deref().unwrap_or(size);
    let used = match (used_bytes, capacity_bytes, used_ratio) {
        (Some(used), Some(capacity), Some(ratio)) => format!(
            " ({:.0}% used: {} of {})",
            (ratio * 100.0 + 1e-9).floor(),
            human_bytes(used),
            human_bytes(capacity)
        ),
        _ => String::new(),
    };
    let message = match state {
        "full" if disk_full => Some(format!(
            "The bridge's volume is full{used}: the bridge stopped because it could not write \
             to it (\"No space left on device\"). Give it a larger volume size than {has}: the \
             volume grows in place, keeping everything on it, and the bridge starts again."
        )),
        "full" => Some(format!(
            "The bridge's volume is full{used}: the bridge will stop when it cannot write to \
             it. Give it a larger volume size than {has}: the volume grows in place, keeping \
             everything on it, and the bridge restarts."
        )),
        "nearly_full" => Some(format!(
            "The bridge's volume is nearly full{used}. Give it a larger volume size than {has} \
             before it fills and the bridge stops: the volume grows in place and the bridge \
             restarts."
        )),
        _ => None,
    };
    BridgeVolume {
        size: size.to_owned(),
        size_set,
        cluster_size,
        capacity_bytes,
        used_bytes,
        used_ratio,
        observed_at: deployment.and_then(|d| d.volume_observed_at.clone()),
        state: state.to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities_read_as_bytes() {
        assert_eq!(quantity_bytes("1Gi"), Some(1 << 30));
        assert_eq!(quantity_bytes("1.5Gi"), Some(3 << 29));
        assert_eq!(quantity_bytes("2G"), Some(2_000_000_000));
        assert_eq!(quantity_bytes("1500m"), Some(2));
        assert_eq!(quantity_bytes("1e3"), Some(1000));
        for bad in ["", "Gi", "1GB", "-1Gi", "1..5Gi", "1 Gi", "64Ei"] {
            assert_eq!(quantity_bytes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_administrators_size_is_whole_bounded_and_never_a_shrink() {
        assert_eq!(
            parse_size(" 10Gi ", None),
            Ok(("10Gi".to_owned(), 10 << 30))
        );
        assert_eq!(parse_size("1536Mi", None).map(|s| s.1), Ok(1536 << 20));
        assert_eq!(parse_size("1Ti", Some("5Gi")).map(|s| s.1), Ok(1 << 40));
        for bad in ["10", "10G", "1.5Gi", "Gi", "ten", "10GiB", "-5Gi"] {
            let e = parse_size(bad, None).unwrap_err();
            assert!(e.contains("not a size"), "{bad}: {e}");
        }
        assert!(parse_size("512Mi", None).unwrap_err().contains("smallest"));
        assert!(parse_size("2Ti", None).unwrap_err().contains("largest"));
        let shrink = parse_size("5Gi", Some("10Gi")).unwrap_err();
        assert!(shrink.contains("cannot shrink"), "{shrink}");
        assert!(shrink.contains("10Gi or more"), "{shrink}");
        // The same size, written another way, is not a shrink.
        assert!(parse_size("10240Mi", Some("10Gi")).is_ok());
    }

    #[test]
    fn sizes_read_for_a_person() {
        assert_eq!(human_bytes(1_020_702_720), "973 MiB");
        assert_eq!(human_bytes(1 << 30), "1 GiB");
        assert_eq!(human_bytes(3 << 29), "1.5 GiB");
        assert_eq!(human_bytes(119_447_552), "114 MiB");
        assert_eq!(human_bytes(10_464_022_528), "9.7 GiB");
        assert_eq!(human_bytes(50 << 30), "50 GiB");
    }

    fn deployment(used: Option<u64>, message: Option<&str>) -> BridgeDeployment {
        BridgeDeployment {
            phase: "Ready".to_owned(),
            message: message.map(str::to_owned),
            storage_size: Some("1Gi".to_owned()),
            volume_capacity_bytes: used.map(|_| 1_000_000_000),
            volume_used_bytes: used,
            volume_observed_at: used.map(|_| "2026-10-14T08:00:00Z".to_owned()),
            ..BridgeDeployment::default()
        }
    }

    #[test]
    fn a_volume_is_ok_nearly_full_or_full_with_what_to_do() {
        let ok = view("1Gi", false, Some(&deployment(Some(90_000_000), None)));
        assert_eq!(ok.state, "ok");
        assert_eq!(ok.message, None);
        assert_eq!(ok.used_ratio, Some(0.09));

        let nearly = view("1Gi", false, Some(&deployment(Some(870_000_000), None)));
        assert_eq!(nearly.state, "nearly_full");
        let text = nearly.message.unwrap();
        assert!(text.contains("nearly full (87% used"), "{text}");
        assert!(text.contains("larger volume size than 1Gi"), "{text}");

        // The demo's Slack bridge from 2026-10-14 08:00: 98% used.
        let full = view("1Gi", false, Some(&deployment(Some(980_000_000), None)));
        assert_eq!(full.state, "full");
        assert!(full.message.unwrap().contains("is full (98% used"));

        // Not measured yet.
        let unknown = view("5Gi", false, Some(&deployment(None, None)));
        assert_eq!(unknown.state, "unknown");
        assert_eq!(unknown.message, None);
        assert_eq!(view("5Gi", false, None).state, "unknown");
    }

    #[test]
    fn a_bridge_that_could_not_write_has_a_full_volume_whatever_was_measured() {
        // The init container's last words through the operator, 2026-10-15; the last
        // measurement was older, or there was none (the pod never ran to be measured).
        let message = "files: CrashLoopBackOff: back-off 5m0s; it last exited saying: cp: \
                       write error: No space left on device";
        for used in [None, Some(500_000_000)] {
            let v = view("1Gi", false, Some(&deployment(used, Some(message))));
            assert_eq!(v.state, "full", "{used:?}");
            let text = v.message.unwrap();
            assert!(text.contains("could not write"), "{text}");
            assert!(text.contains("grows in place"), "{text}");
        }
        assert!(says_disk_full(
            "FTL Failed to start bridge error=\"database or disk is full\""
        ));
        assert!(!says_disk_full("bridge: CrashLoopBackOff"));
    }

    #[test]
    fn a_volume_raised_by_hand_is_reported_as_kept() {
        let mut d = deployment(Some(1), None);
        d.storage_size = Some("10Gi".to_owned());
        let v = view("5Gi", true, Some(&d));
        assert_eq!(v.cluster_size.as_deref(), Some("10Gi"));
        assert!(v.size_set);
        d.storage_size = Some("5Gi".to_owned());
        assert_eq!(view("5Gi", true, Some(&d)).cluster_size, None);
    }
}
