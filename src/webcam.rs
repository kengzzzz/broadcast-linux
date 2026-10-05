use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::InputFormat;

/// Picks the ffmpeg input format for the webcam at `size` (e.g. "1920x1080").
pub fn choose(input: &str, wanted: InputFormat, size: &str) -> Result<&'static str> {
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-f",
            "v4l2",
            "-list_formats",
            "all",
            "-i",
            input,
        ])
        .output()
        .context("running ffmpeg (is it installed?)")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let formats = parse(&stderr);
    if formats.is_empty() {
        let why = if stderr.contains("No such file") {
            "does not exist"
        } else if stderr.contains("Permission denied") {
            "permission denied; join the video group, then log in again"
        } else {
            "lists no formats"
        };
        bail!("{input} {why}");
    }
    pick(&formats, wanted, size).with_context(|| input.to_owned())
}

fn pick(formats: &[(String, String)], wanted: InputFormat, size: &str) -> Result<&'static str> {
    let candidates = match wanted {
        InputFormat::Auto => vec![InputFormat::Mjpeg, InputFormat::Yuyv, InputFormat::Nv12],
        format => vec![format],
    };
    let offers = |name: &str| {
        formats
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, sizes)| sizes.as_str())
    };
    // Stepwise devices list a range like "{32-4096, 2}x{32-2160, 2}"; assume it fits.
    let fits = |sizes: &str| sizes.contains('{') || sizes.split_whitespace().any(|s| s == size);
    if let Some(format) = candidates
        .iter()
        .map(|f| f.ffmpeg_name())
        .find(|name| offers(name).is_some_and(fits))
    {
        return Ok(format);
    }
    let listed: Vec<String> = formats
        .iter()
        .map(|(name, sizes)| format!("{name}: {sizes}"))
        .collect();
    bail!(
        "no {} capture at {size}; set [camera] width/height or input_format to one of: {}",
        wanted.label(),
        listed.join("; ")
    )
}

/// Reads `ffmpeg -list_formats` lines like
/// `[in#0 @ 0x1] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720`.
fn parse(stderr: &str) -> Vec<(String, String)> {
    stderr
        .lines()
        .filter_map(|line| {
            let (kind, rest) = line.split_once("] ")?.1.split_once(':')?;
            if !matches!(kind.trim(), "Raw" | "Compressed") {
                return None;
            }
            let mut parts = rest.split(" : ");
            let name = parts.next()?.trim().to_owned();
            let sizes = parts.last()?.trim().to_owned();
            Some((name, sizes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIO: &str = "[in#0 @ 0x5587] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720 1920x1080 340x340
[in#0 @ 0x5587] Compressed:       mjpeg :          Motion-JPEG : 640x480 1280x720 1920x1080
Error opening input file /dev/video0.";

    #[test]
    fn parses_formats() {
        let formats = parse(BRIO);
        assert_eq!(formats.len(), 2);
        assert_eq!(formats[0].0, "yuyv422");
        assert_eq!(
            formats[1],
            ("mjpeg".into(), "640x480 1280x720 1920x1080".into())
        );
        assert_eq!(
            parse("Error opening input files: No such file or directory"),
            []
        );
    }

    #[test]
    fn picks_formats() {
        let formats = parse(BRIO);
        assert_eq!(
            pick(&formats, InputFormat::Auto, "1920x1080").unwrap(),
            "mjpeg"
        );
        assert_eq!(
            pick(&formats, InputFormat::Auto, "340x340").unwrap(),
            "yuyv422"
        );
        assert_eq!(
            pick(&formats, InputFormat::Yuyv, "1920x1080").unwrap(),
            "yuyv422"
        );
        assert!(pick(&formats, InputFormat::Nv12, "1920x1080").is_err());
        assert!(pick(&formats, InputFormat::Auto, "1234x567").is_err());
        let capture_card = parse(
            "[in#0 @ 0x1] Raw       :        nv12 :         Y/UV 4:2:0 : {32-4096, 2}x{32-2160, 2}",
        );
        assert_eq!(
            pick(&capture_card, InputFormat::Auto, "1920x1080").unwrap(),
            "nv12"
        );
    }
}
