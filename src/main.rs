use clap::Parser;
use crc32fast::Hasher as Crc32Hasher;
use lepton_jpeg::{DEFAULT_THREAD_POOL, EnabledFeatures};
use preflate_rs::{PreflateConfig, preflate_whole_deflate_stream, recreate_whole_deflate_stream};
use std::io::{Cursor, Read, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "flimg", about = "Lossless JPEG/PNG preprocessor for zpaq archiving")]
struct Cli {
    #[arg(short, long)]
    mode: String,

    #[arg(short, long)]
    input: PathBuf,

    #[arg(short, long)]
    output: PathBuf,
}

// ── JPEG ──

fn encode_jpeg(data: &[u8], output: &mut impl Write) -> Result<(), String> {
    let mut encoded = Vec::new();
    lepton_jpeg::encode_lepton(
        &mut Cursor::new(data),
        &mut Cursor::new(&mut encoded),
        &EnabledFeatures::compat_lepton_vector_write(),
        &DEFAULT_THREAD_POOL,
    )
    .map_err(|e| format!("Lepton encode: {}", e))?;
    output.write_all(&[0x01]).unwrap();
    output.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
    output.write_all(&encoded).unwrap();
    Ok(())
}

fn decode_jpeg(
    input: &mut Cursor<&[u8]>,
    size: usize,
    output: &mut impl Write,
) -> Result<(), String> {
    let mut lepton = vec![0u8; size];
    input.read_exact(&mut lepton).unwrap();
    lepton_jpeg::decode_lepton(
        &mut Cursor::new(&lepton),
        output,
        &EnabledFeatures::compat_lepton_vector_read(),
        &DEFAULT_THREAD_POOL,
    )
    .map_err(|e| format!("Lepton decode: {}", e))?;
    Ok(())
}

// ── PNG ──

const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

struct PngInfo {
    pre_idat: Vec<u8>,        // all bytes before first IDAT
    zlib_header: [u8; 2],
    deflate_data: Vec<u8>,    // concatenated raw DEFLATE (no zlib header/adler32)
    adler32: [u8; 4],
    idat_sizes: Vec<u32>,     // sizes of each original IDAT chunk's zlib data
}

fn parse_png(data: &[u8]) -> Result<PngInfo, String> {
    if data.len() < 33 || data[..8] != PNG_SIG {
        return Err("无效 PNG 签名".into());
    }
    let mut pos = 8;
    let mut pre_idat = Vec::new();
    let mut all_idat_data = Vec::new();
    let mut idat_sizes = Vec::new();
    let mut found_idat = false;

    while pos + 12 <= data.len() {
        let chunk_len = u32::from_be_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
        let chunk_type = &data[pos+4..pos+8];

        if pos + 12 + chunk_len > data.len() {
            return Err("PNG chunk 数据不完整".into());
        }

        if chunk_type == b"IDAT" {
            let chunk_data = &data[pos+8..pos+8+chunk_len];
            if !found_idat { found_idat = true; }
            idat_sizes.push(chunk_len as u32);
            all_idat_data.extend_from_slice(chunk_data);
            pos += 12 + chunk_len;
        } else if chunk_type == b"IEND" {
            if !found_idat { return Err("未找到 IDAT 块".into()); }
            break;
        } else {
            let chunk_bytes = &data[pos..pos+12+chunk_len];
            if !found_idat {
                pre_idat.extend_from_slice(chunk_bytes);
            }
            pos += 12 + chunk_len;
        }
    }

    if !found_idat { return Err("未找到 IDAT 块".into()); }
    if all_idat_data.len() < 6 { return Err("IDAT 数据过短".into()); }

    // The zlib stream spans all IDAT chunks — strip header + adler32 from the combined data
    let zlib_header = [all_idat_data[0], all_idat_data[1]];
    let len = all_idat_data.len();
    let deflate_data = all_idat_data[2..len - 4].to_vec();
    let adler32 = [all_idat_data[len - 4], all_idat_data[len - 3], all_idat_data[len - 2], all_idat_data[len - 1]];

    Ok(PngInfo { pre_idat, zlib_header, deflate_data, adler32, idat_sizes })
}

fn encode_png(data: &[u8], output: &mut impl Write) -> Result<(), String> {
    let info = parse_png(data)?;
    let config = PreflateConfig::default();
    let (result, plain_text) = preflate_whole_deflate_stream(&info.deflate_data, &config)
        .map_err(|e| format!("preflate: {}", e))?;
    let plain_bytes = plain_text.text();
    let corrections = &result.corrections;

    // Verify adler32
    let computed = adler32(plain_bytes);
    let stored = u32::from_be_bytes(info.adler32);
    if computed != stored {
        return Err(format!("adler32 mismatch: computed 0x{:08x}, stored 0x{:08x}", computed, stored));
    }

    output.write_all(&[0x02]).unwrap();
    output.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
    // pre_idat size + data
    output.write_all(&(info.pre_idat.len() as u32).to_le_bytes()).unwrap();
    output.write_all(&info.pre_idat).unwrap();
    // zlib header
    output.write_all(&info.zlib_header).unwrap();
    // idat_sizes count + sizes
    output.write_all(&(info.idat_sizes.len() as u32).to_le_bytes()).unwrap();
    for &s in &info.idat_sizes {
        output.write_all(&s.to_le_bytes()).unwrap();
    }
    // corrections
    output.write_all(&(corrections.len() as u32).to_le_bytes()).unwrap();
    output.write_all(corrections).unwrap();
    // plaintext
    output.write_all(&(plain_bytes.len() as u32).to_le_bytes()).unwrap();
    output.write_all(plain_bytes).unwrap();
    Ok(())
}

fn decode_png(
    input: &mut Cursor<&[u8]>,
    size: usize,
    output: &mut impl Write,
) -> Result<(), String> {
    let mut raw = vec![0u8; size];
    input.read_exact(&mut raw).unwrap();
    let mut pos = 0;

    // pre_idat
    let pre_idat_len = u32::from_le_bytes([raw[pos], raw[pos+1], raw[pos+2], raw[pos+3]]) as usize;
    pos += 4;
    let pre_idat = &raw[pos..pos+pre_idat_len];
    pos += pre_idat_len;

    // zlib header
    let zlib_header = [raw[pos], raw[pos+1]];
    pos += 2;

    // idat_sizes
    let idat_count = u32::from_le_bytes([raw[pos], raw[pos+1], raw[pos+2], raw[pos+3]]) as usize;
    pos += 4;
    let mut idat_sizes = Vec::with_capacity(idat_count);
    for _ in 0..idat_count {
        let s = u32::from_le_bytes([raw[pos], raw[pos+1], raw[pos+2], raw[pos+3]]);
        idat_sizes.push(s);
        pos += 4;
    }

    // corrections
    let corr_len = u32::from_le_bytes([raw[pos], raw[pos+1], raw[pos+2], raw[pos+3]]) as usize;
    pos += 4;
    let corrections = &raw[pos..pos+corr_len];
    pos += corr_len;

    // plaintext
    let plain_len = u32::from_le_bytes([raw[pos], raw[pos+1], raw[pos+2], raw[pos+3]]) as usize;
    pos += 4;
    let plaintext = &raw[pos..pos+plain_len];

    // Recreate DEFLATE stream
    let deflate_data = recreate_whole_deflate_stream(plaintext, corrections)
        .map_err(|e| format!("preflate recreate: {}", e))?;

    // Build zlib stream
    let adler = adler32(plaintext);
    let mut zlib_data = Vec::with_capacity(2 + deflate_data.len() + 4);
    zlib_data.extend_from_slice(&zlib_header);
    zlib_data.extend_from_slice(&deflate_data);
    zlib_data.extend_from_slice(&adler.to_be_bytes());

    // Write PNG
    output.write_all(&PNG_SIG).unwrap();
    output.write_all(pre_idat).unwrap();
    // Rebuild IDAT chunks with same boundaries as original
    let mut offset = 0;
    for &chunk_size in &idat_sizes {
        let end = (offset + chunk_size as usize).min(zlib_data.len());
        let seg = &zlib_data[offset..end];
        output.write_all(&build_png_chunk(b"IDAT", seg)).unwrap();
        offset = end;
    }
    output.write_all(&build_png_chunk(b"IEND", &[])).unwrap();
    Ok(())
}

fn build_png_chunk(chunk_type: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(12 + data.len());
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(chunk_type);
    chunk.extend_from_slice(data);
    let mut hasher = Crc32Hasher::new();
    hasher.update(chunk_type);
    hasher.update(data);
    chunk.extend_from_slice(&hasher.finalize().to_be_bytes());
    chunk
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

// ── main ──

const JPEG_MAGIC: [u8; 2] = [0xFF, 0xD8];

fn detect_format(cursor: &mut Cursor<&[u8]>) -> &'static str {
    let mut buf = [0u8; 8];
    if cursor.read_exact(&mut buf).is_err() {
        return "unknown";
    }
    if buf[..2] == JPEG_MAGIC {
        "jpeg"
    } else if buf[..8] == PNG_SIG {
        "png"
    } else {
        "unknown"
    }
}

fn main() {
    let cli = Cli::parse();
    let mut input_data = Vec::new();
    std::fs::File::open(&cli.input)
        .unwrap_or_else(|e| {
            eprintln!("无法打开输入: {} ({})", cli.input.display(), e);
            std::process::exit(1);
        })
        .read_to_end(&mut input_data)
        .unwrap();
    let mut output = std::fs::File::create(&cli.output).unwrap_or_else(|e| {
        eprintln!("无法创建输出: {} ({})", cli.output.display(), e);
        std::process::exit(1);
    });

    let result = match cli.mode.as_str() {
        "encode" => {
            let mut cursor = Cursor::new(input_data.as_slice());
            match detect_format(&mut cursor) {
                "jpeg" => encode_jpeg(&input_data, &mut output),
                "png" => encode_png(&input_data, &mut output),
                other => Err(format!("不支持格式: {}", other)),
            }
        }
        "decode" => {
            let mut cursor = Cursor::new(input_data.as_slice());
            let mut buf = [0u8; 1];
            cursor.read_exact(&mut buf).unwrap();
            let mut sz = [0u8; 4];
            cursor.read_exact(&mut sz).unwrap();
            let _original = u32::from_le_bytes(sz) as usize;
            let remaining = input_data.len() - 5;
            match buf[0] {
                0x01 => decode_jpeg(&mut cursor, remaining, &mut output),
                0x02 => decode_png(&mut cursor, remaining, &mut output),
                b => Err(format!("未知类型: 0x{:02x}", b)),
            }
        }
        _ => {
            eprintln!("模式应为 'encode' 或 'decode'");
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("错误: {}", e);
        std::process::exit(1);
    }
}
