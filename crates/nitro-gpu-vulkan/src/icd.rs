//! Vendor-ICD selection.
//!
//! With default discovery the Vulkan loader opens every installed driver,
//! including llvmpipe, which maps libLLVM: measured in #3903 at 32–63 MB
//! against 8.7–10.9 MB with the vendor ICD alone. So the helper finds the
//! render node's kernel driver in sysfs, maps it to that vendor's ICD
//! manifests, and points the loader at exactly one (`VK_DRIVER_FILES`)
//! before loading it. llvmpipe (`lvp`) is never a candidate.

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

/// Existing manifests for `kernel_driver` under `dirs`, best first, one
/// per file name (the first directory wins, as with the loader).
#[must_use]
pub fn candidates(kernel_driver: &str, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for name in manifests_for(kernel_driver) {
        if let Some(p) = dirs.iter().map(|d| d.join(name)).find(|p| p.is_file()) {
            out.push(p);
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
        assert!(candidates("amdgpu", &[a, b]).is_empty());
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
