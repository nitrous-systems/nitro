//! Vendor-ICD selection.
//!
//! With default discovery the Vulkan loader opens every installed driver,
//! including llvmpipe, which maps libLLVM: measured in #3903 at 32–63 MB
//! against 8.7–10.9 MB with the vendor ICD alone. So the helper finds the
//! render node's kernel driver in sysfs, maps it to that vendor's ICD
//! manifests, and points the loader at exactly one (`VK_DRIVER_FILES`)
//! before loading it. llvmpipe (`lvp`) is never a candidate. Debian
//! installs multiarch-suffixed manifests (`broadcom_icd.armv8l.json`,
//! `intel_icd.x86_64.json`); those match too, the helper's own arch first.

use std::path::{Path, PathBuf};

/// Environment override: a path to one ICD manifest.
pub const ICD_ENV: &str = "NITRO_GPU_ICD";

/// ICD manifest file names for a kernel driver, best first.
#[must_use]
pub fn manifests_for(kernel_driver: &str) -> &'static [&'static str] {
    match kernel_driver {
        // anv first; hasvk covers Gen7/7.5/8 (anv enumerates no device there).
        "i915" | "xe" => &["intel_icd.json", "intel_hasvk_icd.json"],
        "amdgpu" | "radeon" => &["radeon_icd.json"],
        "nouveau" => &["nouveau_icd.json"],
        "msm" => &["freedreno_icd.json"],
        "panfrost" | "panthor" => &["panfrost_icd.json"],
        "v3d" | "vc4" => &["broadcom_icd.json"],
        "asahi" => &["asahi_icd.json"],
        "virtio_gpu" => &["virtio_icd.json"],
        _ => &[],
    }
}

/// Directories searched for manifests, in the loader's order.
#[must_use]
pub fn search_dirs(xdg_data_dirs: Option<&str>) -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/etc/vulkan/icd.d"),
        PathBuf::from("/usr/local/share/vulkan/icd.d"),
    ];
    for d in xdg_data_dirs
        .unwrap_or("/usr/local/share:/usr/share")
        .split(':')
        .filter(|d| !d.is_empty())
    {
        let p = Path::new(d).join("vulkan/icd.d");
        if !v.contains(&p) {
            v.push(p);
        }
    }
    let usr = PathBuf::from("/usr/share/vulkan/icd.d");
    if !v.contains(&usr) {
        v.push(usr);
    }
    v
}

/// Debian multiarch suffixes (`<name>.<arch>.json`) for the helper's own
/// build arch, best first. The userland arch matters, not the kernel's:
/// a 32-bit armhf helper on an aarch64 kernel wants `armv8l`.
#[must_use]
pub fn arch_suffixes() -> &'static [&'static str] {
    if cfg!(target_arch = "arm") {
        &["armv8l", "armv7l", "armhf"]
    } else if cfg!(target_arch = "aarch64") {
        &["aarch64"]
    } else if cfg!(target_arch = "x86_64") {
        &["x86_64"]
    } else if cfg!(target_arch = "x86") {
        &["i686", "i386"]
    } else if cfg!(target_arch = "riscv64") {
        &["riscv64"]
    } else {
        &[]
    }
}

/// How well `file_name` matches the manifest `base` (`foo_icd.json`):
/// `0` for the exact name, `1 + i` for `foo_icd.<arches[i]>.json`,
/// `1 + arches.len()` for any other `foo_icd.<suffix>.json`, `None` if it
/// is a different manifest. The stem must match exactly, so
/// `intel_hasvk_icd.x86_64.json` is not an `intel_icd.json`.
#[must_use]
pub fn match_rank(file_name: &str, base: &str, arches: &[&str]) -> Option<usize> {
    if file_name == base {
        return Some(0);
    }
    let stem = base.strip_suffix(".json")?;
    let arch = file_name
        .strip_prefix(stem)?
        .strip_prefix('.')?
        .strip_suffix(".json")?;
    if arch.is_empty() || arch.contains('.') || arch.contains('/') {
        return None;
    }
    Some(
        1 + arches
            .iter()
            .position(|a| *a == arch)
            .unwrap_or(arches.len()),
    )
}

/// Existing manifests for `kernel_driver` under `dirs`, best first. For
/// each manifest name the first directory holding the exact or a suffixed
/// variant wins (as with the loader); within it the exact name comes
/// first, then the running arch's suffix, then any other (sorted). A
/// foreign-arch manifest just fails to load and the next is tried.
#[must_use]
pub fn candidates(kernel_driver: &str, dirs: &[PathBuf]) -> Vec<PathBuf> {
    candidates_for_arch(kernel_driver, dirs, arch_suffixes())
}

/// [`candidates`] with explicit arch suffixes.
#[must_use]
pub fn candidates_for_arch(kernel_driver: &str, dirs: &[PathBuf], arches: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in manifests_for(kernel_driver) {
        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut found: Vec<(usize, String, PathBuf)> = rd
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    let rank = match_rank(&name, base, arches)?;
                    let p = e.path();
                    p.is_file().then_some((rank, name, p))
                })
                .collect();
            if found.is_empty() {
                continue;
            }
            found.sort();
            out.extend(found.into_iter().map(|(_, _, p)| p));
            break;
        }
    }
    out
}

/// The kernel driver behind a DRM device node, from
/// `<sysfs>/dev/char/<major>:<minor>/device/driver`.
#[must_use]
pub fn kernel_driver(sys: &Path, major: u32, minor: u32) -> Option<String> {
    let link =
        std::fs::read_link(sys.join(format!("dev/char/{major}:{minor}/device/driver"))).ok()?;
    link.file_name()?.to_str().map(str::to_owned)
}

/// The render node to use: `$NITRO_GPU_RENDER_NODE`, else the first
/// `/dev/dri/renderD*`.
#[must_use]
pub fn render_node() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NITRO_GPU_RENDER_NODE") {
        return Some(PathBuf::from(p));
    }
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

/// `(major, minor)` of a device node.
///
/// # Errors
/// `stat` failed.
pub fn dev_numbers(node: &Path) -> Result<(u32, u32), rustix::io::Errno> {
    let st = rustix::fs::stat(node)?;
    Ok((rustix::fs::major(st.st_rdev), rustix::fs::minor(st.st_rdev)))
}

/// Candidate manifests for `node`: the override alone if set, else the
/// vendor's manifests.
#[must_use]
pub fn for_node(node: &Path) -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os(ICD_ENV) {
        return vec![PathBuf::from(p)];
    }
    let Ok((maj, min)) = dev_numbers(node) else {
        return Vec::new();
    };
    let Some(drv) = kernel_driver(Path::new("/sys"), maj, min) else {
        return Vec::new();
    };
    let xdg = std::env::var("XDG_DATA_DIRS").ok();
    candidates(&drv, &search_dirs(xdg.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping() {
        assert_eq!(manifests_for("i915")[0], "intel_icd.json");
        assert_eq!(manifests_for("i915")[1], "intel_hasvk_icd.json");
        assert!(manifests_for("vgem").is_empty());
        for d in [
            "i915",
            "xe",
            "amdgpu",
            "nouveau",
            "msm",
            "panfrost",
            "v3d",
            "asahi",
            "virtio_gpu",
        ] {
            assert!(manifests_for(d).iter().all(|m| !m.contains("lvp")));
        }
    }

    #[test]
    fn suffix_rank() {
        let a = ["armv8l", "armv7l"];
        assert_eq!(
            match_rank("broadcom_icd.json", "broadcom_icd.json", &a),
            Some(0)
        );
        assert_eq!(
            match_rank("broadcom_icd.armv8l.json", "broadcom_icd.json", &a),
            Some(1)
        );
        assert_eq!(
            match_rank("broadcom_icd.armv7l.json", "broadcom_icd.json", &a),
            Some(2)
        );
        assert_eq!(
            match_rank("broadcom_icd.x86_64.json", "broadcom_icd.json", &a),
            Some(3)
        );
        assert_eq!(
            match_rank("lvp_icd.armv8l.json", "broadcom_icd.json", &a),
            None
        );
        assert_eq!(
            match_rank("intel_hasvk_icd.x86_64.json", "intel_icd.json", &a),
            None
        );
        assert_eq!(match_rank("intel_icd..json", "intel_icd.json", &a), None);
        assert_eq!(match_rank("intel_icd.a.b.json", "intel_icd.json", &a), None);
        assert_eq!(match_rank("intel_icdx.json", "intel_icd.json", &a), None);
    }

    #[test]
    fn search_in_a_temp_tree() {
        let root = std::env::temp_dir().join(format!("nitro-icd-{}", std::process::id()));
        let a = root.join("a/vulkan/icd.d");
        let b = root.join("b/vulkan/icd.d");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for f in ["intel_hasvk_icd.json", "lvp_icd.json"] {
            std::fs::write(a.join(f), "{}").unwrap();
        }
        for f in ["intel_icd.json", "intel_hasvk_icd.json"] {
            std::fs::write(b.join(f), "{}").unwrap();
        }
        let xdg = format!("{}:{}", root.join("a").display(), root.join("b").display());
        let dirs = search_dirs(Some(&xdg));
        assert!(dirs.contains(&a) && dirs.contains(&b));
        let c = candidates("i915", &[a.clone(), b.clone()]);
        assert_eq!(
            c,
            vec![b.join("intel_icd.json"), a.join("intel_hasvk_icd.json")]
        );
        assert!(candidates("amdgpu", &[a.clone(), b.clone()]).is_empty());
        // Debian multiarch-suffixed manifests (Pi OS armhf).
        let c = root.join("c/vulkan/icd.d");
        std::fs::create_dir_all(&c).unwrap();
        for f in [
            "broadcom_icd.aarch64.json",
            "lvp_icd.armv8l.json",
            "broadcom_icd.armv8l.json",
            "intel_hasvk_icd.x86_64.json",
        ] {
            std::fs::write(c.join(f), "{}").unwrap();
        }
        let arm = ["armv8l", "armv7l", "armhf"];
        assert_eq!(
            candidates_for_arch("v3d", std::slice::from_ref(&c), &arm),
            vec![
                c.join("broadcom_icd.armv8l.json"),
                c.join("broadcom_icd.aarch64.json")
            ]
        );
        assert_eq!(
            candidates_for_arch("v3d", std::slice::from_ref(&c), &["aarch64"]),
            vec![
                c.join("broadcom_icd.aarch64.json"),
                c.join("broadcom_icd.armv8l.json")
            ]
        );
        // intel_hasvk.x86_64 is not intel_icd; exact name beats suffixed.
        std::fs::write(c.join("intel_icd.x86_64.json"), "{}").unwrap();
        std::fs::write(c.join("intel_icd.json"), "{}").unwrap();
        assert_eq!(
            candidates_for_arch("i915", std::slice::from_ref(&c), &["x86_64"]),
            vec![
                c.join("intel_icd.json"),
                c.join("intel_icd.x86_64.json"),
                c.join("intel_hasvk_icd.x86_64.json"),
            ]
        );
        for d in ["v3d", "i915", "amdgpu", "msm"] {
            assert!(
                candidates_for_arch(d, &[a.clone(), b.clone(), c.clone()], &arm)
                    .iter()
                    .all(|p| !p.to_string_lossy().contains("lvp"))
            );
        }
        // First dir with any variant wins.
        assert_eq!(
            candidates_for_arch("i915", &[b.clone(), c.clone()], &["x86_64"]),
            vec![b.join("intel_icd.json"), b.join("intel_hasvk_icd.json")]
        );
        // sysfs lookup
        let sys = root.join("sys");
        let dev = sys.join("dev/char/226:128/device");
        std::fs::create_dir_all(&dev).unwrap();
        std::os::unix::fs::symlink("../../../bus/pci/drivers/i915", dev.join("driver")).unwrap();
        assert_eq!(kernel_driver(&sys, 226, 128).as_deref(), Some("i915"));
        assert_eq!(kernel_driver(&sys, 226, 129), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
