use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use flate2::read::ZlibDecoder;
use gimage_core::{EngineError, RgbaImage};

use crate::RustImageEngine;
use crate::pipeline::fit_edge;

struct Header {
    wide: bool,
    width: u32,
    height: u32,
    channels: usize,
    colors: usize,
    sample_bytes: usize,
}

fn bad(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_int<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn header(input: &mut impl Read) -> io::Result<Header> {
    let bytes: [u8; 26] = read_int(input)?;
    if &bytes[..4] != b"8BPS" || bytes[4] != 0 || !matches!(bytes[5], 1 | 2) {
        return Err(bad("无效的 PSD / PSB 文件头"));
    }
    let channels = u16::from_be_bytes([bytes[12], bytes[13]]) as usize;
    let height = u32::from_be_bytes(bytes[14..18].try_into().unwrap());
    let width = u32::from_be_bytes(bytes[18..22].try_into().unwrap());
    let sample_bytes = match u16::from_be_bytes([bytes[22], bytes[23]]) {
        8 => 1,
        16 => 2,
        _ => return Err(bad("PSD 合成预览需要 8 位或 16 位通道")),
    };
    let colors = match u16::from_be_bytes([bytes[24], bytes[25]]) {
        1 => 1,
        3 => 3,
        4 => 4,
        _ => return Err(bad("PSD 合成预览支持灰度、RGB 和 CMYK")),
    };
    if !(colors..=56).contains(&channels)
        || !(1..=300_000).contains(&width)
        || !(1..=300_000).contains(&height)
    {
        return Err(bad("无效的 PSD 尺寸或通道数"));
    }
    Ok(Header {
        wide: bytes[5] == 2,
        width,
        height,
        channels,
        colors,
        sample_bytes,
    })
}

pub fn probe(path: &Path) -> Result<(u32, u32, u16, bool), EngineError> {
    let mut file = File::open(path).map_err(|e| EngineError::io(path, e))?;
    let header = header(&mut file).map_err(|e| EngineError::decode(path, e.to_string()))?;
    Ok((
        header.width,
        header.height,
        1,
        header.channels > header.colors,
    ))
}

fn read_length(input: &mut impl Read, wide: bool) -> io::Result<u64> {
    if wide {
        Ok(u64::from_be_bytes(read_int(input)?))
    } else {
        Ok(u64::from(u32::from_be_bytes(read_int(input)?)))
    }
}

fn skip_section(input: &mut (impl Read + Seek)) -> io::Result<()> {
    let length = read_length(input, false)?;
    input.seek_relative(length as i64)
}

pub fn decode(
    engine: &RustImageEngine,
    path: &Path,
    edge: Option<u32>,
) -> Result<RgbaImage, EngineError> {
    let mut input = BufReader::new(File::open(path).map_err(|e| EngineError::io(path, e))?);
    let header = header(&mut input).map_err(|e| EngineError::decode(path, e.to_string()))?;
    engine.guard_pixels(path, header.width, header.height)?;
    decode_composite(input, header, edge)
        .map_err(|e| EngineError::decode(path, format!("{e}。请确认文件已保存兼容的合成图")))
}

fn decode_composite(
    mut input: BufReader<File>,
    header: Header,
    edge: Option<u32>,
) -> io::Result<RgbaImage> {
    skip_section(&mut input)?;
    skip_section(&mut input)?;
    let layers_length = read_length(&mut input, header.wide)?;
    let layers_start = input.stream_position()?;
    let layers_end = layers_start
        .checked_add(layers_length)
        .ok_or_else(|| bad("PSD 图层区无效"))?;
    let mut has_alpha = header.channels == header.colors + 1;
    if layers_length >= if header.wide { 8 } else { 4 } {
        let info_length = read_length(&mut input, header.wide)?;
        if info_length >= 2 && input.stream_position()? <= layers_end - 2 {
            has_alpha = i16::from_be_bytes(read_int(&mut input)?) < 0;
        }
    }
    has_alpha &= header.channels > header.colors;
    input.seek(SeekFrom::Start(layers_end))?;

    let compression = u16::from_be_bytes(read_int(&mut input)?);
    let used_channels = header.colors + usize::from(has_alpha);
    let row_count = header.height as usize;
    let mut rle_lengths = Vec::new();
    if compression == 1 {
        for _ in 0..used_channels * row_count {
            let length = if header.wide {
                u32::from_be_bytes(read_int(&mut input)?) as usize
            } else {
                usize::from(u16::from_be_bytes(read_int(&mut input)?))
            };
            rle_lengths.push(length);
        }
        let unused = (header.channels - used_channels) * row_count;
        input.seek_relative((unused * if header.wide { 4 } else { 2 }) as i64)?;
    } else if compression > 3 {
        return Err(bad("PSD 合成图的压缩方式不受支持"));
    }
    let mut pixels_input: Box<dyn Read> = if compression >= 2 {
        Box::new(ZlibDecoder::new(input))
    } else {
        Box::new(input)
    };

    let (width, height) = fit_edge(header.width, header.height, edge.unwrap_or(u32::MAX));
    let source_width = header.width as usize;
    let row_bytes = source_width * header.sample_bytes;
    let mut row = vec![0; row_bytes];
    let mut compressed = Vec::new();
    let pixel_count = (u64::from(width) * u64::from(height) * 4) as usize;
    let mut output = vec![255; pixel_count];
    for channel in 0..used_channels {
        let mut target_y = 0usize;
        for source_y in 0..row_count {
            if compression == 1 {
                let size = rle_lengths[channel * row_count + source_y];
                if size > row_bytes * 2 + 1024 {
                    return Err(bad("PSD 的 RLE 行超出长度"));
                }
                compressed.resize(size, 0);
                pixels_input.read_exact(&mut compressed)?;
                unpack_bits(&compressed, &mut row)?;
            } else {
                pixels_input.read_exact(&mut row)?;
                if compression == 3 {
                    undo_prediction(&mut row, header.sample_bytes);
                }
            }
            if target_y < height as usize && target_y * row_count / height as usize == source_y {
                let target_start = target_y * width as usize * 4;
                for (x, pixel) in output[target_start..target_start + width as usize * 4]
                    .chunks_exact_mut(4)
                    .enumerate()
                {
                    let value = row[(x * source_width / width as usize) * header.sample_bytes];
                    match channel {
                        c if c == header.colors => pixel[3] = value,
                        0 if header.colors == 1 => pixel[..3].fill(value),
                        3 => {
                            for color in &mut pixel[..3] {
                                *color = ((u16::from(*color) * u16::from(value) + 127) / 255) as u8;
                            }
                        }
                        c => pixel[c] = value,
                    }
                }
                target_y += 1;
            }
        }
    }
    Ok(RgbaImage::new(width, height, output))
}

fn undo_prediction(row: &mut [u8], sample_bytes: usize) {
    if sample_bytes == 1 {
        for i in 1..row.len() {
            row[i] = row[i].wrapping_add(row[i - 1]);
        }
    } else {
        let mut previous = 0u16;
        for pair in row.chunks_exact_mut(2) {
            previous = previous.wrapping_add(u16::from_be_bytes([pair[0], pair[1]]));
            pair.copy_from_slice(&previous.to_be_bytes());
        }
    }
}

fn unpack_bits(mut bytes: &[u8], mut row: &mut [u8]) -> io::Result<()> {
    while !row.is_empty() {
        let control = read_int::<1>(&mut bytes)?[0] as i8;
        if control == -128 {
            continue;
        }
        let count = 1 + usize::from(control.unsigned_abs());
        if count > row.len() {
            return Err(bad("PSD 的 RLE 行超出宽度"));
        }
        let (target, remaining) = row.split_at_mut(count);
        if control < 0 {
            target.fill(read_int::<1>(&mut bytes)?[0]);
        } else {
            bytes.read_exact(target)?;
        }
        row = remaining;
    }
    Ok(())
}
