//! Screenshotting one window: ask the server's control socket for the
//! whole output and crop it here.
//!
//! The server screenshots an *output*, because that is what it owns a
//! framebuffer for. A client that wants a picture of itself therefore
//! needs two things it already has: the control socket path (the same
//! resolution rule `nitro-shot` uses) and its own position on the
//! output, which the server tells it in every `Configure`.
//!
//! The alternative — a `shot <window>` control request — would put the
//! window→client mapping and a crop in the compositor for the benefit of
//! one caller. Cropping in the client keeps the server's screenshot path
//! the single thing it already is.

use std::io::{ErrorKind, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use nitro_core::{Point, Size};

/// A screenshot: `XRGB8888` rows, `stride` bytes each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    /// `stride * height` bytes.
    pub data: Vec<u8>,
}

/// Where the server's control socket is, given the environment.
///
/// Taken as arguments rather than read here so the rule can be tested
/// without mutating the process's environment — which is `unsafe` and
/// races every other test in the binary.
#[must_use]
pub fn resolve_control(
    override_path: Option<&std::path::Path>,
    runtime_dir: Option<&std::path::Path>,
    uid: u32,
) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    match runtime_dir {
        Some(d) if d.is_absolute() => d.join("nitro").join("control.sock"),
        _ => PathBuf::from(format!("/tmp/nitro-{uid}")).join("control.sock"),
    }
}

/// Where the server's control socket is: `$NITRO_CONTROL`, else
/// `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/…`.
#[must_use]
pub fn control_path() -> PathBuf {
    let over = std::env::var_os("NITRO_CONTROL").map(PathBuf::from);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    resolve_control(
        over.as_deref(),
        runtime.as_deref(),
        rustix::process::getuid().as_raw(),
    )
}

/// A screenshot of the whole first output.
///
/// # Errors
/// If the control socket cannot be reached or answers with an error.
pub fn output_shot() -> std::io::Result<Shot> {
    output_shot_at(&control_path())
}

/// A screenshot of the whole first output, from an explicit control
/// socket. What the test harness uses.
///
/// # Errors
/// If the control socket cannot be reached or answers with an error.
pub fn output_shot_at(path: &std::path::Path) -> std::io::Result<Shot> {
    let mut sock = UnixStream::connect(path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    sock.write_all(b"shot\n")?;
    let header = read_line(&mut sock)?;
    let rest = header
        .strip_prefix("ok ")
        .ok_or_else(|| std::io::Error::other(format!("server: {}", header.trim())))?;
    let fields: Vec<u32> = rest
        .split_whitespace()
        .map(|f| f.parse::<u32>().ok())
        .collect::<Option<_>>()
        .ok_or_else(|| std::io::Error::other(format!("bad shot header {header:?}")))?;
    let [width, height, stride] = fields[..] else {
        return Err(std::io::Error::other(format!("bad shot header {header:?}")));
    };
    let mut data = vec![0u8; stride as usize * height as usize];
    read_exact(&mut sock, &mut data)?;
    Ok(Shot {
        width,
        height,
        stride,
        data,
    })
}

/// A screenshot of the output, cropped to a window at `origin` of `size`
/// logical pixels on an output of `scale`.
///
/// # Errors
/// As [`output_shot`], plus an empty crop (the window is off-screen).
pub fn window_shot(origin: Point, size: Size, scale: f32) -> std::io::Result<Shot> {
    window_shot_at(&control_path(), origin, size, scale)
}

/// As [`window_shot`], from an explicit control socket.
///
/// # Errors
/// As [`output_shot`].
pub fn window_shot_at(
    control: &std::path::Path,
    origin: Point,
    size: Size,
    scale: f32,
) -> std::io::Result<Shot> {
    let full = output_shot_at(control)?;
    Ok(crop(
        &full,
        (origin.x * scale).round() as i64,
        (origin.y * scale).round() as i64,
        (size.w * scale).round() as i64,
        (size.h * scale).round() as i64,
    ))
}

/// Crop `img` to `(x, y, w, h)` device pixels, clamped to the image.
///
/// A crop that falls outside the image entirely is a 0×0 shot rather
/// than an error: a window can legitimately be off-screen, and the
/// caller learns that from the size.
#[must_use]
pub fn crop(img: &Shot, x: i64, y: i64, w: i64, h: i64) -> Shot {
    let iw = i64::from(img.width);
    let ih = i64::from(img.height);
    let x0 = x.clamp(0, iw);
    let y0 = y.clamp(0, ih);
    let x1 = (x + w).clamp(x0, iw);
    let y1 = (y + h).clamp(y0, ih);
    let (cw, ch) = ((x1 - x0) as u32, (y1 - y0) as u32);
    let stride = cw * 4;
    let mut data = Vec::with_capacity(stride as usize * ch as usize);
    for row in 0..ch {
        let src = (y0 as usize + row as usize) * img.stride as usize + x0 as usize * 4;
        let end = src + stride as usize;
        if end <= img.data.len() {
            data.extend_from_slice(&img.data[src..end]);
        } else {
            data.resize(data.len() + stride as usize, 0);
        }
    }
    Shot {
        width: cw,
        height: ch,
        stride,
        data,
    }
}

fn read_line(sock: &mut UnixStream) -> std::io::Result<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match sock.read(&mut b) {
            Ok(0) => return Err(std::io::Error::other("server closed the connection")),
            Ok(_) => {
                if b[0] == b'\n' {
                    return Ok(String::from_utf8_lossy(&out).into_owned());
                }
                out.push(b[0]);
                if out.len() > 256 {
                    return Err(std::io::Error::other("reply line too long"));
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn read_exact(sock: &mut UnixStream, buf: &mut [u8]) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match sock.read(&mut buf[done..]) {
            Ok(0) => return Err(std::io::Error::other("short screenshot")),
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32) -> Shot {
        let stride = w * 4;
        let mut data = vec![0u8; (stride * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = (y * stride + x * 4) as usize;
                data[off] = x as u8;
                data[off + 1] = y as u8;
            }
        }
        Shot {
            width: w,
            height: h,
            stride,
            data,
        }
    }

    #[test]
    fn a_crop_takes_the_right_pixels() {
        let img = image(8, 8);
        let c = crop(&img, 2, 3, 4, 2);
        assert_eq!((c.width, c.height, c.stride), (4, 2, 16));
        assert_eq!(c.data.len(), 32);
        // First pixel of the crop is (2, 3) of the source.
        assert_eq!(c.data[0], 2);
        assert_eq!(c.data[1], 3);
        // First pixel of the second row is (2, 4).
        assert_eq!(c.data[16], 2);
        assert_eq!(c.data[17], 4);
    }

    #[test]
    fn a_crop_is_clamped_to_the_image() {
        let img = image(8, 8);
        let c = crop(&img, 6, 6, 10, 10);
        assert_eq!((c.width, c.height), (2, 2));
        let off = crop(&img, 20, 20, 4, 4);
        assert_eq!((off.width, off.height), (0, 0));
        assert!(off.data.is_empty());
        let negative = crop(&img, -4, -4, 6, 6);
        assert_eq!((negative.width, negative.height), (2, 2));
    }

    #[test]
    fn the_control_path_follows_the_environment() {
        let over = std::path::Path::new("/tmp/nitro-shot-test.sock");
        assert_eq!(
            resolve_control(Some(over), Some(std::path::Path::new("/run/user/1")), 7),
            over
        );
        assert_eq!(
            resolve_control(None, Some(std::path::Path::new("/run/user/1")), 7),
            PathBuf::from("/run/user/1/nitro/control.sock")
        );
        assert_eq!(
            resolve_control(None, None, 7),
            PathBuf::from("/tmp/nitro-7/control.sock")
        );
    }
}
