//! Optional native FFmpeg decoder for HEVC-compressed SDPC tiles.
//!
//! The DLLs are loaded at runtime so JPEG-only installations do not need an
//! FFmpeg link-time dependency.  The loader accepts the bundled PyAV native
//! runtime as well as a standalone `FFMPEG_HOME`/`ffmpeg` directory.

use anyhow::{bail, Context, Result};
use image::{Rgb, RgbImage};
use libloading::Library;
use std::env;
use std::ffi::CString;
use std::fs;
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

type Codec = c_void;
type CodecContext = c_void;
type Packet = c_void;
type Dictionary = c_void;
type SwsContext = c_void;

#[repr(C)]
struct FramePrefix {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
}

type FindDecoder = unsafe extern "C" fn(*const c_char) -> *mut Codec;
type AllocContext = unsafe extern "C" fn(*const Codec) -> *mut CodecContext;
type OpenContext =
    unsafe extern "C" fn(*mut CodecContext, *const Codec, *mut *mut Dictionary) -> c_int;
type FreeContext = unsafe extern "C" fn(*mut *mut CodecContext);
type PacketAlloc = unsafe extern "C" fn() -> *mut Packet;
type PacketFree = unsafe extern "C" fn(*mut *mut Packet);
type PacketFromData = unsafe extern "C" fn(*mut Packet, *mut u8, c_int) -> c_int;
type PacketUnref = unsafe extern "C" fn(*mut Packet);
type AvMalloc = unsafe extern "C" fn(usize) -> *mut u8;
type AvFree = unsafe extern "C" fn(*mut c_void);
type SendPacket = unsafe extern "C" fn(*mut CodecContext, *const Packet) -> c_int;
type ReceiveFrame = unsafe extern "C" fn(*mut CodecContext, *mut FramePrefix) -> c_int;
type FlushBuffers = unsafe extern "C" fn(*mut CodecContext);
type FrameAlloc = unsafe extern "C" fn() -> *mut FramePrefix;
type FrameFree = unsafe extern "C" fn(*mut *mut FramePrefix);
type FrameUnref = unsafe extern "C" fn(*mut FramePrefix);
type SwsGetContext = unsafe extern "C" fn(
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    *const c_void,
    *const c_void,
    *const f64,
) -> *mut SwsContext;
type SwsScale = unsafe extern "C" fn(
    *mut SwsContext,
    *const *const u8,
    *const c_int,
    c_int,
    c_int,
    *const *mut u8,
    *const c_int,
) -> c_int;
type SwsFreeContext = unsafe extern "C" fn(*mut SwsContext);

const PIX_FMT_RGB24: c_int = 2;
const SWS_BILINEAR: c_int = 2;

pub struct Decoder {
    _avutil: Library,
    _avcodec: Library,
    _swscale: Library,
    find_decoder: FindDecoder,
    alloc_context: AllocContext,
    open_context: OpenContext,
    free_context: FreeContext,
    packet_alloc: PacketAlloc,
    packet_free: PacketFree,
    packet_from_data: PacketFromData,
    packet_unref: PacketUnref,
    av_malloc: AvMalloc,
    av_free: AvFree,
    send_packet: SendPacket,
    receive_frame: ReceiveFrame,
    flush_buffers: FlushBuffers,
    frame_alloc: FrameAlloc,
    frame_free: FrameFree,
    frame_unref: FrameUnref,
    sws_get_context: SwsGetContext,
    sws_scale: SwsScale,
    sws_free_context: SwsFreeContext,
    codec_context: *mut CodecContext,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        let directory = locate_ffmpeg_dir().context(
            "HEVC SDPC requires FFmpeg runtime DLLs; set FFMPEG_HOME or bundle an av.libs directory",
        )?;
        prepend_path(&directory)?;

        // Load avutil first because avcodec and swscale depend on it.  The
        // filenames contain PyAV's wheel hashes, so resolve by prefix.
        let avutil = unsafe { Library::new(find_dll(&directory, "avutil")?) }?;
        let avcodec = unsafe { Library::new(find_dll(&directory, "avcodec")?) }?;
        let swscale = unsafe { Library::new(find_dll(&directory, "swscale")?) }?;
        unsafe {
            let mut decoder = Self {
                find_decoder: load(&avcodec, b"avcodec_find_decoder_by_name\0")?,
                alloc_context: load(&avcodec, b"avcodec_alloc_context3\0")?,
                open_context: load(&avcodec, b"avcodec_open2\0")?,
                free_context: load(&avcodec, b"avcodec_free_context\0")?,
                packet_alloc: load(&avcodec, b"av_packet_alloc\0")?,
                packet_free: load(&avcodec, b"av_packet_free\0")?,
                packet_from_data: load(&avcodec, b"av_packet_from_data\0")?,
                packet_unref: load(&avcodec, b"av_packet_unref\0")?,
                send_packet: load(&avcodec, b"avcodec_send_packet\0")?,
                receive_frame: load(&avcodec, b"avcodec_receive_frame\0")?,
                flush_buffers: load(&avcodec, b"avcodec_flush_buffers\0")?,
                av_malloc: load(&avutil, b"av_malloc\0")?,
                av_free: load(&avutil, b"av_free\0")?,
                frame_alloc: load(&avutil, b"av_frame_alloc\0")?,
                frame_free: load(&avutil, b"av_frame_free\0")?,
                frame_unref: load(&avutil, b"av_frame_unref\0")?,
                sws_get_context: load(&swscale, b"sws_getContext\0")?,
                sws_scale: load(&swscale, b"sws_scale\0")?,
                sws_free_context: load(&swscale, b"sws_freeContext\0")?,
                _avutil: avutil,
                _avcodec: avcodec,
                _swscale: swscale,
                codec_context: ptr::null_mut(),
            };
            let name = CString::new("hevc")?;
            let codec = (decoder.find_decoder)(name.as_ptr());
            if codec.is_null() {
                bail!("FFmpeg runtime has no HEVC decoder");
            }
            let context = (decoder.alloc_context)(codec);
            if context.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            let mut context = context;
            let result = (decoder.open_context)(context, codec, ptr::null_mut());
            if result < 0 {
                (decoder.free_context)(&mut context);
                bail!("avcodec_open2(hevc) failed with error {result}");
            }
            decoder.codec_context = context;
            Ok(decoder)
        }
    }

    pub fn decode(&mut self, data: &[u8], width: u32, height: u32) -> Result<RgbImage> {
        if data.is_empty() {
            bail!("empty HEVC tile");
        }
        let size = c_int::try_from(data.len()).context("HEVC tile exceeds FFmpeg packet size")?;
        unsafe {
            let packet = (self.packet_alloc)();
            let frame = (self.frame_alloc)();
            if packet.is_null() || frame.is_null() {
                let mut packet = packet;
                let mut frame = frame;
                if !packet.is_null() {
                    (self.packet_free)(&mut packet);
                }
                if !frame.is_null() {
                    (self.frame_free)(&mut frame);
                }
                bail!("FFmpeg packet/frame allocation failed");
            }
            let raw = (self.av_malloc)(data.len());
            if raw.is_null() {
                let mut packet = packet;
                let mut frame = frame;
                (self.packet_free)(&mut packet);
                (self.frame_free)(&mut frame);
                bail!("FFmpeg packet allocation failed");
            }
            ptr::copy_nonoverlapping(data.as_ptr(), raw, data.len());
            let result = (self.packet_from_data)(packet, raw, size);
            if result < 0 {
                (self.av_free)(raw.cast());
                let mut packet = packet;
                let mut frame = frame;
                (self.packet_free)(&mut packet);
                (self.frame_free)(&mut frame);
                bail!("av_packet_from_data failed with error {result}");
            }
            let send = (self.send_packet)(self.codec_context, packet);
            if send < 0 {
                (self.packet_unref)(packet);
                let mut packet = packet;
                let mut frame = frame;
                (self.packet_free)(&mut packet);
                (self.frame_free)(&mut frame);
                bail!("avcodec_send_packet failed with error {send}");
            }
            let receive = (self.receive_frame)(self.codec_context, frame);
            if receive < 0 {
                (self.packet_unref)(packet);
                let mut packet = packet;
                let mut frame = frame;
                (self.packet_free)(&mut packet);
                (self.frame_free)(&mut frame);
                bail!("HEVC tile did not decode to a frame (error {receive})");
            }
            let prefix = &*frame;
            let source_width = u32::try_from(prefix.width).context("invalid HEVC frame width")?;
            let source_height =
                u32::try_from(prefix.height).context("invalid HEVC frame height")?;
            if source_width == 0 || source_height == 0 || prefix.data[0].is_null() {
                bail!("FFmpeg returned an empty HEVC frame");
            }
            let source_len = usize::try_from(source_width)
                .and_then(|w| usize::try_from(source_height).map(|h| w * h * 3))
                .context("HEVC frame is too large")?;
            let mut source_rgb = vec![0u8; source_len];
            let mut dst_data = [
                source_rgb.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ];
            let dst_linesize = [i32::try_from(source_width * 3)?, 0, 0, 0];
            let sws = (self.sws_get_context)(
                prefix.width,
                prefix.height,
                prefix.format,
                prefix.width,
                prefix.height,
                PIX_FMT_RGB24,
                SWS_BILINEAR,
                ptr::null(),
                ptr::null(),
                ptr::null(),
            );
            if sws.is_null() {
                bail!(
                    "sws_getContext failed for HEVC frame format {}",
                    prefix.format
                );
            }
            let converted = (self.sws_scale)(
                sws,
                prefix.data.as_ptr() as *const *const u8,
                prefix.linesize.as_ptr(),
                0,
                prefix.height,
                dst_data.as_mut_ptr(),
                dst_linesize.as_ptr(),
            );
            (self.sws_free_context)(sws);
            if converted <= 0 {
                bail!("sws_scale failed for HEVC frame");
            }
            let source = RgbImage::from_raw(source_width, source_height, source_rgb)
                .context("invalid RGB frame returned by swscale")?;
            let mut result = RgbImage::from_pixel(width, height, Rgb([255, 255, 255]));
            for y in 0..source_height.min(height) {
                for x in 0..source_width.min(width) {
                    result.put_pixel(x, y, *source.get_pixel(x, y));
                }
            }
            (self.packet_unref)(packet);
            let mut packet = packet;
            let mut frame = frame;
            (self.packet_free)(&mut packet);
            (self.frame_unref)(frame);
            (self.frame_free)(&mut frame);
            (self.flush_buffers)(self.codec_context);
            Ok(result)
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.codec_context.is_null() {
                let mut context = self.codec_context;
                (self.free_context)(&mut context);
                self.codec_context = ptr::null_mut();
            }
        }
    }
}

unsafe fn load<T: Copy>(library: &Library, symbol: &[u8]) -> Result<T> {
    Ok(*library
        .get::<T>(symbol)
        .with_context(|| format!("missing FFmpeg symbol {}", String::from_utf8_lossy(symbol)))?)
}

fn locate_ffmpeg_dir() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(value) = env::var_os("FFMPEG_HOME") {
        roots.push(PathBuf::from(value));
    }
    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            roots.push(parent.join("ffmpeg"));
            roots.push(parent.join("av.libs"));
        }
    }
    roots.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    roots.into_iter().find(|root| {
        find_dll(root, "avcodec").is_ok()
            && find_dll(root, "avutil").is_ok()
            && find_dll(root, "swscale").is_ok()
    })
}

fn find_dll(directory: &Path, prefix: &str) -> Result<PathBuf> {
    let entries =
        fs::read_dir(directory).with_context(|| format!("scan {}", directory.display()))?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.to_ascii_lowercase()
                            .starts_with(&prefix.to_ascii_lowercase())
                            && name.to_ascii_lowercase().ends_with(".dll")
                    })
        })
        .with_context(|| format!("{} DLL not found in {}", prefix, directory.display()))
}

fn prepend_path(directory: &Path) -> Result<()> {
    let current = env::var_os("PATH").unwrap_or_default();
    let joined = env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(env::split_paths(&current)),
    )?;
    // This affects only the current process and is needed for dependent DLLs
    // such as libx265-*.dll in the bundled PyAV runtime.
    env::set_var("PATH", joined);
    Ok(())
}
