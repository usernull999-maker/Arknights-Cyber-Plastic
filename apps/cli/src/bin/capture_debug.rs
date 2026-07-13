use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use arknights_super_ruler_bridge::legacy::capture::{create_backend, CapturedFrame};
use arknights_super_ruler_bridge::legacy::{PixelFormat, RulerConfig};

fn main() -> Result<(), String> {
    let (config_path, output_path) = parse_args()?;
    let config = RulerConfig::load_from_path(&config_path).map_err(|e| e.to_string())?;
    let capture_config = config.to_capture_config().map_err(|e| e.to_string())?;

    let mut backend =
        create_backend(capture_config).map_err(|e| format!("backend init failed: {e}"))?;
    backend
        .connect()
        .map_err(|e| format!("backend connect failed: {e}"))?;
    let frame = backend
        .capture_frame()
        .map_err(|e| format!("capture failed: {e}"))?;
    backend.disconnect();

    write_frame_bitmap(&output_path, &frame)?;
    println!(
        "wrote {} ({}x{} {:?})",
        output_path.display(),
        frame.width,
        frame.height,
        frame.format
    );
    Ok(())
}

fn parse_args() -> Result<(PathBuf, PathBuf), String> {
    let mut config_path = PathBuf::from("config.json");
    let mut output_path = PathBuf::from("logs").join("capture_debug_full.bmp");

    let args: Vec<String> = std::env::args().collect();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--config" | "-c" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "missing value after --config".to_string())?;
                config_path = PathBuf::from(value);
            }
            "--output" | "-o" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "missing value after --output".to_string())?;
                output_path = PathBuf::from(value);
            }
            "--help" | "-h" => {
                println!("Usage: capture_debug [--config PATH] [--output PATH]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }

    Ok((config_path, output_path))
}

fn write_frame_bitmap(path: &Path, frame: &CapturedFrame) -> Result<(), String> {
    let mut data_bgr = Vec::with_capacity(frame.width as usize * frame.height as usize * 3);
    let bpp = bytes_per_pixel(frame.format);
    for y in 0..frame.height {
        for x in 0..frame.width {
            let offset = ((y * frame.width + x) as usize) * bpp;
            if offset + bpp > frame.data.len() {
                return Err("captured frame buffer is shorter than expected".to_string());
            }
            match frame.format {
                PixelFormat::Bgr => data_bgr.extend_from_slice(&frame.data[offset..offset + 3]),
                PixelFormat::Bgra => data_bgr.extend_from_slice(&frame.data[offset..offset + 3]),
                PixelFormat::Rgba => {
                    data_bgr.push(frame.data[offset + 2]);
                    data_bgr.push(frame.data[offset + 1]);
                    data_bgr.push(frame.data[offset]);
                }
            }
        }
    }

    write_bgr_bitmap(path, frame.width, frame.height, &data_bgr)
}

fn bytes_per_pixel(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Bgr => 3,
        PixelFormat::Bgra | PixelFormat::Rgba => 4,
    }
}

fn write_bgr_bitmap(path: &Path, width: u32, height: u32, data_bgr: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| {
        format!(
            "failed to create output directory '{}': {e}",
            parent.display()
        )
    })?;

    let expected_len = width as usize * height as usize * 3;
    if data_bgr.len() != expected_len {
        return Err(format!(
            "invalid buffer size: expected {} bytes, got {}",
            expected_len,
            data_bgr.len()
        ));
    }

    let row_stride = width as usize * 3;
    let padded_row_stride = (row_stride + 3) & !3;
    let pixel_array_size = padded_row_stride * height as usize;
    let file_size = 14usize + 40usize + pixel_array_size;
    let mut file =
        File::create(path).map_err(|e| format!("failed to create '{}': {e}", path.display()))?;

    file.write_all(b"BM")
        .and_then(|_| file.write_all(&(file_size as u32).to_le_bytes()))
        .and_then(|_| file.write_all(&0u16.to_le_bytes()))
        .and_then(|_| file.write_all(&0u16.to_le_bytes()))
        .and_then(|_| file.write_all(&(54u32).to_le_bytes()))
        .map_err(|e| {
            format!(
                "failed to write bitmap file header '{}': {e}",
                path.display()
            )
        })?;

    file.write_all(&(40u32).to_le_bytes())
        .and_then(|_| file.write_all(&(width as i32).to_le_bytes()))
        .and_then(|_| file.write_all(&(height as i32).to_le_bytes()))
        .and_then(|_| file.write_all(&(1u16).to_le_bytes()))
        .and_then(|_| file.write_all(&(24u16).to_le_bytes()))
        .and_then(|_| file.write_all(&(0u32).to_le_bytes()))
        .and_then(|_| file.write_all(&(pixel_array_size as u32).to_le_bytes()))
        .and_then(|_| file.write_all(&(0i32).to_le_bytes()))
        .and_then(|_| file.write_all(&(0i32).to_le_bytes()))
        .and_then(|_| file.write_all(&(0u32).to_le_bytes()))
        .and_then(|_| file.write_all(&(0u32).to_le_bytes()))
        .map_err(|e| {
            format!(
                "failed to write bitmap info header '{}': {e}",
                path.display()
            )
        })?;

    let padding = [0u8; 3];
    for row in (0..height as usize).rev() {
        let start = row * row_stride;
        let end = start + row_stride;
        file.write_all(&data_bgr[start..end])
            .map_err(|e| format!("failed to write bitmap pixel row '{}': {e}", path.display()))?;
        if padded_row_stride > row_stride {
            file.write_all(&padding[..padded_row_stride - row_stride])
                .map_err(|e| {
                    format!(
                        "failed to write bitmap row padding '{}': {e}",
                        path.display()
                    )
                })?;
        }
    }

    Ok(())
}
