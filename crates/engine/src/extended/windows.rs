use std::fs::File;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::{path::Path, ptr};

use gimage_core::{EngineError, ImageFormat, RgbaImage};
use windows::Data::Pdf::{PdfDocument, PdfPage, PdfPageRenderOptions};
use windows::Storage::Streams::{DataReader, IRandomAccessStream, InMemoryRandomAccessStream};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::WinRT::{
    CreateRandomAccessStreamOnFile, RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
};
use windows::core::{GUID, Interface, PCWSTR};

use crate::RustImageEngine;
use crate::codec::{Decoded, read_orientation};
use crate::pipeline::fit_edge;

struct Apartment(bool);

impl Apartment {
    fn new() -> windows::core::Result<Self> {
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self(true)),
            Err(error) if error.code().0 as u32 == 0x8001_0106 => Ok(Self(false)),
            Err(error) => Err(error),
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { RoUninitialize() };
        }
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn open_wic(path: &Path) -> windows::core::Result<(IWICImagingFactory, IWICBitmapDecoder)> {
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
    let name = wide_path(path);
    let decoder = unsafe {
        factory.CreateDecoderFromFilename(
            PCWSTR(name.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?
    };
    Ok((factory, decoder))
}

pub fn has_decoder(class: &GUID) -> bool {
    let Ok(_apartment) = Apartment::new() else {
        return false;
    };
    let result: windows::core::Result<IWICBitmapDecoder> =
        unsafe { CoCreateInstance(class, None, CLSCTX_INPROC_SERVER) };
    result.is_ok()
}

pub fn available_formats() -> Vec<ImageFormat> {
    let mut formats = Vec::new();
    if has_decoder(&CLSID_WICHeifDecoder) {
        formats.push(ImageFormat::Heic);
    }
    if has_decoder(&CLSID_WICRAWDecoder) || has_decoder(&CLSID_WICAdngDecoder) {
        formats.push(ImageFormat::Raw);
    }
    formats
}

fn wic_error(path: &Path, format: &ImageFormat, error: windows::core::Error) -> EngineError {
    let hint = match format {
        ImageFormat::Heic => {
            "请检查 Windows HEIF 和 HEVC 图像扩展，部分相机的 HIF 封装取决于系统解码器"
        }
        ImageFormat::Raw => "请安装或更新 Windows Raw Image Extension 并确认它支持该相机型号",
        ImageFormat::Avif => "需要支持 AVIF 的 Windows 图像解码器",
        _ => "Windows 图像解码器无法读取此文件",
    };
    EngineError::decode(path, format!("{hint}（{error}）"))
}

fn orientation(path: &Path, format: &ImageFormat) -> u16 {
    if matches!(format, ImageFormat::Raw | ImageFormat::Jpeg) {
        read_orientation(path)
    } else {
        1
    }
}

fn dimensions(source: &IWICBitmapSource) -> windows::core::Result<(u32, u32)> {
    let (mut width, mut height) = (0, 0);
    unsafe { source.GetSize(&mut width, &mut height)? };
    if width == 0 || height == 0 {
        return Err(windows::core::Error::from(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    }
    Ok((width, height))
}

pub fn wic_probe(path: &Path, format: &ImageFormat) -> Result<(u32, u32, u16, bool), EngineError> {
    let result = (|| {
        let _apartment = Apartment::new()?;
        let (_, decoder) = open_wic(path)?;
        let frame = unsafe { decoder.GetFrame(0)? };
        dimensions(&frame)
    })();
    let (width, height) = result.map_err(|error| wic_error(path, format, error))?;
    Ok((
        width,
        height,
        orientation(path, format),
        *format == ImageFormat::Avif,
    ))
}

pub fn scaled_jpeg(
    engine: &RustImageEngine,
    path: &Path,
    edge: Option<u32>,
) -> Result<Decoded, EngineError> {
    wic_decode(engine, path, &ImageFormat::Jpeg, edge, false)
}

pub fn wic_decode(
    engine: &RustImageEngine,
    path: &Path,
    format: &ImageFormat,
    edge: Option<u32>,
    thumbnail: bool,
) -> Result<Decoded, EngineError> {
    let err = |error| wic_error(path, format, error);
    let _apartment = Apartment::new().map_err(err)?;
    let (factory, decoder) = open_wic(path).map_err(err)?;
    if *format == ImageFormat::Jpeg
        && unsafe { decoder.GetContainerFormat() }.ok() != Some(GUID_ContainerFormatJpeg)
    {
        return Err(EngineError::decode(path, "文件内容并非 JPEG"));
    }
    let frame = unsafe { decoder.GetFrame(0) }.map_err(err)?;
    let (width, height) = dimensions(&frame).map_err(err)?;
    engine.guard_pixels(path, width, height)?;
    let alignment = orientation(path, format);

    if *format == ImageFormat::Raw
        && let Some(edge) = edge
    {
        let preview = unsafe { decoder.GetPreview() }.or_else(|_| unsafe { frame.GetThumbnail() });
        if let Ok(preview) = preview
            && let Ok((w, h)) = dimensions(&preview)
            && w.max(h)
                >= if thumbnail {
                    edge.min(width.max(height)) / 2
                } else {
                    edge.min(width.max(height))
                }
            && let Ok(image) = read_pixels(&factory, &preview, Some(edge))
        {
            return Ok(Decoded {
                image,
                orientation: alignment,
            });
        }
    }

    if matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Heic | ImageFormat::Raw
    ) && let Some(edge) = edge
        && width.max(height) > edge
        && let Ok(transform) = frame.cast::<IWICBitmapSourceTransform>()
        && let Ok(image) = decode_native_scale(&factory, &transform, (width, height), edge)
    {
        return Ok(Decoded {
            image,
            orientation: alignment,
        });
    }
    let image = read_pixels(&factory, &frame, edge).map_err(err)?;
    Ok(Decoded {
        image,
        orientation: alignment,
    })
}

fn decode_native_scale(
    factory: &IWICImagingFactory,
    transform: &IWICBitmapSourceTransform,
    source_size: (u32, u32),
    edge: u32,
) -> windows::core::Result<RgbaImage> {
    let (mut width, mut height) = fit_edge(source_size.0, source_size.1, edge);
    unsafe { transform.GetClosestSize(&mut width, &mut height)? };
    if width == 0
        || height == 0
        || (width, height) == source_size
        || width.max(height) > edge.saturating_mul(3)
    {
        return Err(windows::core::Error::from(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    }
    let mut pixel_format = GUID_WICPixelFormat32bppRGBA;
    unsafe { transform.GetClosestPixelFormat(&mut pixel_format)? };
    let pixel_size = if pixel_format == GUID_WICPixelFormat32bppRGBA
        || pixel_format == GUID_WICPixelFormat32bppBGRA
    {
        4
    } else if pixel_format == GUID_WICPixelFormat24bppBGR {
        3
    } else {
        return Err(windows::core::Error::from(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    };
    let stride = width
        .checked_mul(pixel_size)
        .filter(|stride| u64::from(*stride) * u64::from(height) <= u64::from(u32::MAX))
        .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG))?;
    let mut buffer = vec![0; stride as usize * height as usize];
    unsafe {
        transform.CopyPixels(
            ptr::null(),
            width,
            height,
            &pixel_format,
            WICBitmapTransformRotate0,
            stride,
            &mut buffer,
        )?;
        let bitmap =
            factory.CreateBitmapFromMemory(width, height, &pixel_format, stride, &buffer)?;
        read_pixels(factory, &bitmap, Some(edge))
    }
}

fn read_pixels(
    factory: &IWICImagingFactory,
    source: &IWICBitmapSource,
    edge: Option<u32>,
) -> windows::core::Result<RgbaImage> {
    let (width, height) = dimensions(source)?;
    let (out_w, out_h) = fit_edge(width, height, edge.unwrap_or(u32::MAX));
    let stride = out_w
        .checked_mul(4)
        .filter(|stride| u64::from(*stride) * u64::from(out_h) <= u64::from(u32::MAX))
        .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG))?;
    unsafe {
        let converter = factory.CreateFormatConverter()?;
        if (width, height) == (out_w, out_h) {
            converter.Initialize(
                source,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.,
                WICBitmapPaletteTypeCustom,
            )?;
        } else {
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(source, out_w, out_h, WICBitmapInterpolationModeFant)?;
            converter.Initialize(
                &scaler,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.,
                WICBitmapPaletteTypeCustom,
            )?;
        }
        let mut pixels = vec![0; stride as usize * out_h as usize];
        converter.CopyPixels(ptr::null(), stride, &mut pixels)?;
        Ok(RgbaImage::new(out_w, out_h, pixels))
    }
}

struct AiPage {
    page: PdfPage,
    width: u32,
    height: u32,
    _document: PdfDocument,
    _stream: IRandomAccessStream,
}

fn ai_page(path: &Path) -> Result<AiPage, EngineError> {
    let mut header = Vec::new();
    File::open(path)
        .and_then(|file| file.take(1024).read_to_end(&mut header))
        .map_err(|error| EngineError::io(path, error))?;
    if !header.windows(5).any(|bytes| bytes == b"%PDF-") {
        return Err(EngineError::decode(
            path,
            "请在 Illustrator 中以「创建 PDF 兼容文件」重新保存",
        ));
    }
    let result = (|| {
        let wide = wide_path(path);
        let stream: IRandomAccessStream =
            unsafe { CreateRandomAccessStreamOnFile(PCWSTR(wide.as_ptr()), 0)? };
        let doc = PdfDocument::LoadFromStreamAsync(&stream)?.join()?;
        let page = doc.GetPage(0)?;
        let size = page.Size()?;
        Ok::<_, windows::core::Error>(AiPage {
            page,
            width: size.Width.ceil() as u32,
            height: size.Height.ceil() as u32,
            _document: doc,
            _stream: stream,
        })
    })();
    result
        .map_err(|error| EngineError::decode(path, format!("无法读取 AI 中的 PDF 画板（{error}）")))
}

pub fn ai_probe(path: &Path) -> Result<(u32, u32, u16, bool), EngineError> {
    let _apartment = Apartment::new().map_err(|e| EngineError::decode(path, e.to_string()))?;
    let page = ai_page(path)?;
    Ok((page.width, page.height, 1, false))
}

pub fn ai_decode(
    engine: &RustImageEngine,
    path: &Path,
    edge: Option<u32>,
) -> Result<RgbaImage, EngineError> {
    let err = |e: windows::core::Error| EngineError::decode(path, e.to_string());
    let _apartment = Apartment::new().map_err(err)?;
    let page = ai_page(path)?;
    engine.guard_pixels(path, page.width, page.height)?;
    if page.width == 0 || page.height == 0 {
        return Err(EngineError::decode(path, "AI 画板为空"));
    }
    let (width, height) = fit_edge(page.width, page.height, edge.unwrap_or(u32::MAX));
    let options = PdfPageRenderOptions::new().map_err(err)?;
    options.SetDestinationWidth(width).map_err(err)?;
    options.SetDestinationHeight(height).map_err(err)?;
    let output = InMemoryRandomAccessStream::new().map_err(err)?;
    page.page
        .RenderWithOptionsToStreamAsync(&output, &options)
        .and_then(|operation| operation.join())
        .map_err(err)?;
    let size = output.Size().map_err(err)?;
    if size > u64::from(width) * u64::from(height) * 5 + 1024 * 1024 || size > u64::from(u32::MAX) {
        return Err(EngineError::decode(path, "AI 画板渲染数据超出大小限制"));
    }
    let stream = output.GetInputStreamAt(0).map_err(err)?;
    let reader = DataReader::CreateDataReader(&stream).map_err(err)?;
    let loaded = reader
        .LoadAsync(size as u32)
        .and_then(|operation| operation.join())
        .map_err(err)?;
    if u64::from(loaded) != size {
        return Err(EngineError::decode(path, "AI 画板渲染不完整"));
    }
    let mut bytes = vec![0; size as usize];
    reader.ReadBytes(&mut bytes).map_err(err)?;
    let image = image::load_from_memory(&bytes)
        .map_err(|e| EngineError::decode(path, e.to_string()))?
        .into_rgba8();
    Ok(RgbaImage::new(
        image.width(),
        image.height(),
        image.into_raw(),
    ))
}
