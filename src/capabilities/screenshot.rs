use std::future::Future;

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router, Spec};

use super::consent::require_risk_consent;

const SCREENSHOT_BUDGET: usize = 1500 * 1024;

pub fn register(r: &mut Router) {
    let specs: Vec<Spec> = serde_json::from_str(include_str!("specs/screenshot.json"))
        .expect("embedded capability specs are valid JSON");
    for spec in specs {
        r.register_spec(spec, handler(screenshot_handler));
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScreenshotOptions {
    pub display: i64,
    pub format: String,
    pub quality: i64,
    pub max_width: i64,
}

impl ScreenshotOptions {
    fn from_args(args: &Args) -> ScreenshotOptions {
        let mut opts = ScreenshotOptions {
            display: 0,
            format: "jpeg".to_string(),
            quality: 70,
            max_width: 1600,
        };
        if let Some(n) = args.get("display").and_then(Value::as_i64) {
            opts.display = n;
        }
        if let Some(s) = args.get("format").and_then(Value::as_str) {
            opts.format = s.to_string();
        }
        if let Some(n) = args.get("quality").and_then(Value::as_i64) {
            opts.quality = n;
        }
        if let Some(n) = args.get("max_width").and_then(Value::as_i64) {
            opts.max_width = n;
        }
        opts
    }
}

#[derive(Debug, Clone, Default)]
pub struct Screenshot {
    pub data: Vec<u8>,
    pub width: i64,
    pub height: i64,
    pub scaled: bool,
}

async fn screenshot_handler(ctx: Ctx, args: Args) -> CapResult {
    let opts = ScreenshotOptions::from_args(&args);
    require_risk_consent(
        &ctx,
        "sys.screenshot",
        "Takes a picture of what is currently on your screen and sends it to your helper.",
    )
    .await?;
    let cancel = ctx.cancel.clone();
    capture_within_budget(opts, move |o| {
        let cancel = cancel.clone();
        async move { capture_screen(&cancel, &o).await }
    })
    .await
}

pub async fn capture_within_budget<F, Fut>(mut opts: ScreenshotOptions, mut capture: F) -> CapResult
where
    F: FnMut(ScreenshotOptions) -> Fut,
    Fut: Future<Output = Result<Screenshot, HandlerError>>,
{
    for attempt in 0..2 {
        let shot = capture(opts.clone()).await?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&shot.data);
        if encoded.len() <= SCREENSHOT_BUDGET {
            let sum = Sha256::digest(&shot.data);
            let hex: String = sum.iter().map(|b| format!("{b:02x}")).collect();
            return Ok(json!({
                "width": shot.width,
                "height": shot.height,
                "format": opts.format,
                "bytes": shot.data.len(),
                "scaled": shot.scaled,
                "sha256": hex,
                "image_base64": encoded,
            }));
        }
        if attempt == 0 {
            opts.max_width /= 2;
            if opts.max_width < 320 {
                opts.max_width = 320;
            }
            continue;
        }
        return Err(HandlerError::plain(format!(
            "sys.screenshot: image is {} bytes encoded, over the {} byte limit; lower max_width or quality",
            encoded.len(),
            SCREENSHOT_BUDGET
        )));
    }
    Err(HandlerError::plain(
        "sys.screenshot: could not produce an image within the size limit",
    ))
}

pub fn png_to_jpeg(png_data: &[u8], quality: i64) -> Result<Vec<u8>, HandlerError> {
    let decode_err =
        |e: String| HandlerError::plain(format!("sys.screenshot: decode capture: {e}"));
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| decode_err(e.to_string()))?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| decode_err(e.to_string()))?;
    buf.truncate(info.buffer_size());
    let (width, height) = (info.width, info.height);
    if width > u16::MAX as u32 || height > u16::MAX as u32 {
        return Err(HandlerError::plain(
            "sys.screenshot: encode jpeg: image dimensions exceed the JPEG limit",
        ));
    }
    let (pixels, color) = match info.color_type {
        png::ColorType::Grayscale => (buf, jpeg_encoder::ColorType::Luma),
        png::ColorType::GrayscaleAlpha => (
            buf.chunks_exact(2).map(|p| p[0]).collect(),
            jpeg_encoder::ColorType::Luma,
        ),
        png::ColorType::Rgb => (buf, jpeg_encoder::ColorType::Rgb),
        png::ColorType::Rgba => (buf, jpeg_encoder::ColorType::Rgba),
        png::ColorType::Indexed => {
            return Err(decode_err("unexpected indexed color".to_string()));
        }
    };
    let quality = if (1..=100).contains(&quality) {
        quality as u8
    } else {
        70
    };
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality)
        .encode(&pixels, width as u16, height as u16, color)
        .map_err(|e| HandlerError::plain(format!("sys.screenshot: encode jpeg: {e}")))?;
    Ok(out)
}

pub fn parse_shot_dimensions(line: &str) -> Option<(i64, i64, bool)> {
    let fields: Vec<&str> = line.split('x').collect();
    if fields.len() != 3 {
        return None;
    }
    let w = fields[0].parse::<i64>().ok()?;
    let h = fields[1].parse::<i64>().ok()?;
    Some((w, h, fields[2] == "1"))
}

#[cfg(not(windows))]
pub async fn capture_screen(
    _cancel: &tokio_util::sync::CancellationToken,
    _opts: &ScreenshotOptions,
) -> Result<Screenshot, HandlerError> {
    Err(HandlerError::plain(
        "sys.screenshot: only supported on Windows",
    ))
}

#[cfg(windows)]
struct TempDir(std::path::PathBuf);

#[cfg(windows)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(windows)]
pub async fn capture_screen(
    cancel: &tokio_util::sync::CancellationToken,
    opts: &ScreenshotOptions,
) -> Result<Screenshot, HandlerError> {
    use super::pwsh::run_pwsh;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("handoff-shot-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| HandlerError::plain(e.to_string()))?;
    let dir = TempDir(dir);
    let out = dir.0.join("capture.png");

    let stdout = run_pwsh(cancel, &screenshot_script(&out.to_string_lossy(), opts)).await?;

    let mut data = read_captured_file(&out)?;
    if opts.format == "jpeg" {
        data = png_to_jpeg(&data, opts.quality)?;
    }

    let mut shot = Screenshot {
        data,
        ..Screenshot::default()
    };
    for line in String::from_utf8_lossy(&stdout).trim().split('\n') {
        if let Some((w, h, scaled)) = parse_shot_dimensions(line.trim()) {
            shot.width = w;
            shot.height = h;
            shot.scaled = scaled;
        }
    }
    Ok(shot)
}

#[cfg(windows)]
fn read_captured_file(path: &std::path::Path) -> Result<Vec<u8>, HandlerError> {
    let data = std::fs::read(path)
        .map_err(|e| HandlerError::plain(format!("sys.screenshot: read capture: {e}")))?;
    if data.is_empty() {
        return Err(HandlerError::plain(
            "sys.screenshot: capture produced no image",
        ));
    }
    Ok(data)
}

#[cfg(windows)]
const SCRIPT: &str = r##"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

$display = @@DISPLAY@@
if ($display -lt 0) {
    $bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
} else {
    $screens = [System.Windows.Forms.Screen]::AllScreens
    if ($display -ge $screens.Count) { $display = 0 }
    $bounds = $screens[$display].Bounds
}

$bmp = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$gfx = [System.Drawing.Graphics]::FromImage($bmp)
$gfx.CopyFromScreen($bounds.X, $bounds.Y, 0, 0, $bounds.Size)
$gfx.Dispose()

$maxWidth = @@MAXWIDTH@@
$scaled = 0
if ($bmp.Width -gt $maxWidth) {
    $ratio = $maxWidth / $bmp.Width
    $newW = [int]$maxWidth
    $newH = [int][math]::Round($bmp.Height * $ratio)
    $resized = New-Object System.Drawing.Bitmap($newW, $newH)
    $rg = [System.Drawing.Graphics]::FromImage($resized)
    $rg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $rg.DrawImage($bmp, 0, 0, $newW, $newH)
    $rg.Dispose()
    $bmp.Dispose()
    $bmp = $resized
    $scaled = 1
}

$bmp.Save(@@OUT@@, [System.Drawing.Imaging.ImageFormat]::Png)

Write-Output ("" + $bmp.Width + "x" + $bmp.Height + "x" + $scaled)
$bmp.Dispose()
"##;

#[cfg(windows)]
pub fn screenshot_script(out_path: &str, opts: &ScreenshotOptions) -> String {
    SCRIPT
        .replace("@@DISPLAY@@", &opts.display.to_string())
        .replace("@@MAXWIDTH@@", &opts.max_width.to_string())
        .replace("@@OUT@@", &super::pwsh::ps_single_quote(out_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn opts() -> ScreenshotOptions {
        ScreenshotOptions {
            display: 0,
            format: "jpeg".into(),
            quality: 70,
            max_width: 1600,
        }
    }

    fn sample_png(w: u32, h: u32, color: png::ColorType) -> Vec<u8> {
        let channels = match color {
            png::ColorType::Rgba => 4,
            png::ColorType::Rgb => 3,
            png::ColorType::GrayscaleAlpha => 2,
            _ => 1,
        };
        let mut data = Vec::new();
        for i in 0..(w * h * channels) {
            data.push((i % 251) as u8);
        }
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&data).unwrap();
        out
    }

    #[test]
    fn parse_shot_dimensions_reads_width_height_and_scaled() {
        assert_eq!(parse_shot_dimensions("1280x720x1"), Some((1280, 720, true)));
        assert_eq!(
            parse_shot_dimensions("1280x720x0"),
            Some((1280, 720, false))
        );
        assert_eq!(parse_shot_dimensions("garbage"), None);
        assert_eq!(parse_shot_dimensions("axbxc"), None);
    }

    #[test]
    fn options_default_and_override() {
        let none = ScreenshotOptions::from_args(&Args::new());
        assert_eq!(none, opts());
        let args =
            serde_json::json!({"display": -1, "format": "png", "quality": 5, "max_width": 640});
        let got = ScreenshotOptions::from_args(args.as_object().unwrap());
        assert_eq!(
            got,
            ScreenshotOptions {
                display: -1,
                format: "png".into(),
                quality: 5,
                max_width: 640
            }
        );
    }

    #[test]
    fn png_converts_to_jpeg_for_every_color_type() {
        for color in [
            png::ColorType::Rgba,
            png::ColorType::Rgb,
            png::ColorType::Grayscale,
            png::ColorType::GrayscaleAlpha,
        ] {
            let jpg = png_to_jpeg(&sample_png(40, 30, color), 70).unwrap();
            assert_eq!(&jpg[..2], &[0xFF, 0xD8], "{color:?}");
        }
    }

    #[test]
    fn out_of_range_quality_falls_back() {
        let png = sample_png(16, 16, png::ColorType::Rgba);
        assert_eq!(
            png_to_jpeg(&png, 0).unwrap(),
            png_to_jpeg(&png, 70).unwrap()
        );
        assert_eq!(
            png_to_jpeg(&png, 101).unwrap(),
            png_to_jpeg(&png, 70).unwrap()
        );
    }

    #[test]
    fn garbage_is_not_a_png() {
        let err = png_to_jpeg(b"nope", 70).unwrap_err();
        assert!(
            err.message.starts_with("sys.screenshot: decode capture:"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn small_capture_returns_all_result_fields() {
        let out = capture_within_budget(opts(), |_| async {
            Ok(Screenshot {
                data: vec![1, 2, 3],
                width: 10,
                height: 5,
                scaled: true,
            })
        })
        .await
        .unwrap();
        assert_eq!(out["width"], 10);
        assert_eq!(out["height"], 5);
        assert_eq!(out["format"], "jpeg");
        assert_eq!(out["bytes"], 3);
        assert_eq!(out["scaled"], true);
        assert_eq!(out["image_base64"], "AQID");
        assert_eq!(
            out["sha256"],
            "039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81"
        );
    }

    #[tokio::test]
    async fn oversize_capture_retries_once_at_half_width() {
        let widths = Arc::new(std::sync::Mutex::new(Vec::new()));
        let w = widths.clone();
        let out = capture_within_budget(opts(), move |o| {
            let w = w.clone();
            async move {
                w.lock().unwrap().push(o.max_width);
                let size = if o.max_width == 1600 {
                    2 * 1024 * 1024
                } else {
                    10
                };
                Ok(Screenshot {
                    data: vec![0; size],
                    width: o.max_width,
                    height: 1,
                    scaled: true,
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(*widths.lock().unwrap(), vec![1600, 800]);
        assert_eq!(out["width"], 800);
    }

    #[tokio::test]
    async fn still_oversize_after_retry_is_an_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let err = capture_within_budget(opts(), move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            async {
                Ok(Screenshot {
                    data: vec![0; 2 * 1024 * 1024],
                    ..Screenshot::default()
                })
            }
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            err.message.contains("over the 1536000 byte limit"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn retry_width_never_drops_below_the_minimum() {
        let widths = Arc::new(std::sync::Mutex::new(Vec::new()));
        let w = widths.clone();
        let mut o = opts();
        o.max_width = 400;
        let _ = capture_within_budget(o, move |o| {
            let w = w.clone();
            async move {
                w.lock().unwrap().push(o.max_width);
                Ok(Screenshot {
                    data: vec![0; 2 * 1024 * 1024],
                    ..Screenshot::default()
                })
            }
        })
        .await;
        assert_eq!(*widths.lock().unwrap(), vec![400, 320]);
    }

    #[tokio::test]
    async fn capture_errors_pass_through() {
        let err = capture_within_budget(opts(), |_| async {
            Err::<Screenshot, _>(HandlerError::plain("boom"))
        })
        .await
        .unwrap_err();
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn registers_sys_screenshot() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["sys.screenshot"]);
        assert!(r.spec_for("sys.screenshot").unwrap().risky);
    }

    #[cfg(windows)]
    #[test]
    fn script_embeds_options_and_quoted_path() {
        let s = screenshot_script(
            "C:\\a'b\\c.png",
            &ScreenshotOptions {
                display: -1,
                max_width: 800,
                ..opts()
            },
        );
        assert!(s.contains("$display = -1"));
        assert!(s.contains("$maxWidth = 800"));
        assert!(s.contains("'C:\\a''b\\c.png'"));
        assert!(!s.contains("@@"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn real_screen_capture_is_not_blocked_and_produces_a_jpeg() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let shot = match capture_screen(
            &cancel,
            &ScreenshotOptions {
                max_width: 800,
                ..opts()
            },
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                assert!(
                    !e.message.contains("malicious"),
                    "blocked by antivirus: {}",
                    e.message
                );
                return;
            }
        };
        assert!(
            shot.data.len() >= 1000,
            "image is only {} bytes",
            shot.data.len()
        );
        assert_eq!(&shot.data[..2], &[0xFF, 0xD8]);
        assert_eq!(shot.width, 800);
        assert!(shot.scaled);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn handler_end_to_end_with_consent_returns_a_jpeg_result() {
        use crate::capabilities::consent::testing::SessionGuard;
        use crate::consent::Prompt;
        let prompt: Prompt =
            Arc::new(|_, _| Box::pin(async { Ok(crate::consent::Answer::ForGrant) }));
        let _guard = SessionGuard::with_prompt(prompt);
        let args = serde_json::json!({"max_width": 640, "quality": 50});
        let out = match screenshot_handler(Ctx::default(), args.as_object().unwrap().clone()).await
        {
            Ok(v) => v,
            Err(e) => {
                assert!(!e.message.contains("malicious"), "{}", e.message);
                return;
            }
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(out["image_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        assert_eq!(out["bytes"].as_u64().unwrap() as usize, bytes.len());
        assert_eq!(out["format"], "jpeg");
        assert_eq!(out["width"], 640);
        assert!(out["height"].as_i64().unwrap() > 0);
        assert_eq!(out["sha256"].as_str().unwrap().len(), 64);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn real_screen_capture_png() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let shot = match capture_screen(
            &cancel,
            &ScreenshotOptions {
                format: "png".into(),
                max_width: 640,
                ..opts()
            },
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                assert!(
                    !e.message.contains("malicious"),
                    "blocked by antivirus: {}",
                    e.message
                );
                return;
            }
        };
        assert_eq!(&shot.data[1..4], b"PNG");
    }
}
