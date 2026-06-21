//! Hardware decode setup and GPU-to-system frame download. The FFmpeg FFI
//! lives here behind safe functions. Any failure falls back to software.

use ffmpeg_next as ffmpeg;

#[cfg(target_os = "windows")]
const HW_DEVICE: ffmpeg::ffi::AVHWDeviceType =
    ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
#[cfg(target_os = "windows")]
const HW_PIXEL: ffmpeg::ffi::AVPixelFormat = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11;

#[cfg(target_os = "macos")]
const HW_DEVICE: ffmpeg::ffi::AVHWDeviceType =
    ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX;
#[cfg(target_os = "macos")]
const HW_PIXEL: ffmpeg::ffi::AVPixelFormat = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX;

#[cfg(target_os = "linux")]
const HW_DEVICE: ffmpeg::ffi::AVHWDeviceType = ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;
#[cfg(target_os = "linux")]
const HW_PIXEL: ffmpeg::ffi::AVPixelFormat = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;

/// Owns one reference to an FFmpeg hardware device context, freed on drop.
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
struct HwDevice(*mut ffmpeg::ffi::AVBufferRef);

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
impl HwDevice {
    /// Create the platform's default hardware device, or None if it is
    /// unavailable.
    fn create() -> Option<Self> {
        let mut device: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
        // SAFETY: on success (ret >= 0) av_hwdevice_ctx_create writes a new
        // owned reference into `device`; the null options request the
        // platform default. Drop releases that reference.
        let ret = unsafe {
            ffmpeg::ffi::av_hwdevice_ctx_create(
                &mut device,
                HW_DEVICE,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ret < 0 || device.is_null() {
            return None;
        }
        Some(Self(device))
    }
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null reference created in `create` and
        // owned solely by this guard; av_buffer_unref drops and nulls it.
        unsafe { ffmpeg::ffi::av_buffer_unref(&mut self.0) };
    }
}

/// Pick the hardware pixel format if the decoder offers it, otherwise let
/// libavcodec choose a software format (the transparent fallback).
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
unsafe extern "C" fn get_hw_format(
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    fmt: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    // SAFETY: libavcodec passes a valid context and a candidate-format list
    // terminated by AV_PIX_FMT_NONE; we only walk within it and hand the
    // default chooser back the same pointers.
    unsafe {
        let mut p = fmt;
        while *p != ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == HW_PIXEL {
                return HW_PIXEL;
            }
            p = p.add(1);
        }
        ffmpeg::ffi::avcodec_default_get_format(ctx, fmt)
    }
}

/// Attach a hardware device to the decoder context. Returns whether it
/// took; a false return leaves the context ready for software decode.
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
pub fn try_init_hw(context: &mut ffmpeg::codec::context::Context) -> bool {
    let Some(device) = HwDevice::create() else {
        return false;
    };
    // SAFETY: `as_mut_ptr` yields the live context's AVCodecContext. We give
    // it its own reference to the device (av_buffer_ref) and install the
    // format callback; the context owns that reference for its lifetime. The
    // `device` guard frees our original reference when it drops below.
    unsafe {
        let raw = context.as_mut_ptr();
        (*raw).hw_device_ctx = ffmpeg::ffi::av_buffer_ref(device.0);
        (*raw).get_format = Some(get_hw_format);
    }
    true
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn try_init_hw(_context: &mut ffmpeg::codec::context::Context) -> bool {
    false
}

/// Whether a decoded frame lives in GPU memory and must be downloaded.
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
pub fn is_hw_frame(frame: &ffmpeg::frame::Video) -> bool {
    frame.format() == ffmpeg::format::Pixel::from(HW_PIXEL)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn is_hw_frame(_frame: &ffmpeg::frame::Video) -> bool {
    false
}

/// Copy a GPU frame down to system memory (NV12), carrying its color
/// properties. Returns None on a failed transfer so the caller drops just
/// that frame.
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
pub fn download_frame(frame: &ffmpeg::frame::Video) -> Option<ffmpeg::frame::Video> {
    let mut sw = ffmpeg::frame::Video::empty();
    // SAFETY: both are valid frames owned by ffmpeg-next; `frame` is a
    // hardware frame, so av_hwframe_transfer_data downloads its planes into
    // the empty `sw`. Flags 0 is the default transfer; ret < 0 is failure.
    let transferred =
        unsafe { ffmpeg::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), frame.as_ptr(), 0) };
    if transferred < 0 {
        return None;
    }
    // SAFETY: same two valid frames; av_frame_copy_props carries color
    // space, range, and timing from the GPU frame so the converter picks the
    // right matrix. A negative return means the props could not be set.
    let props = unsafe { ffmpeg::ffi::av_frame_copy_props(sw.as_mut_ptr(), frame.as_ptr()) };
    if props < 0 {
        return None;
    }
    Some(sw)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn download_frame(_frame: &ffmpeg::frame::Video) -> Option<ffmpeg::frame::Video> {
    None
}
