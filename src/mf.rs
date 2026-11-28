//! Media Foundation + Direct3D 11 viewer for hardware capture devices.
//!
//! This is the path the Windows Camera app takes: the device is read with an MF
//! Source Reader in low-latency mode, compressed formats (MJPEG/H.264, e.g. from
//! an ATEM Mini) are decoded on the GPU where the driver offers a hardware
//! decoder, and the D3D11 video processor converts, scales and letterboxes the
//! frame straight into a flip-model swap chain that queues at most one frame.
//! Everything runs on one worker thread; the UI thread only reads flags.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, HSTRING};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, PostMessageW};

use crate::capture::{debug_log, Options};

const VS: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    failed: AtomicBool,
    done: AtomicBool,
    frames: AtomicU64,
    dropped: AtomicU64,
}

pub struct MfViewer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    pub format: String,
    started: Instant,
}

impl MfViewer {
    /// Opens the device behind `link` (its DirectShow DevicePath, which is the
    /// same interface path MF uses) and returns once the first frame is on screen.
    pub fn open(link: &str, hwnd: HWND, notify_msg: u32, opts: Options) -> Result<MfViewer, String> {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        let link = link.to_string();
        let hwnd_raw = hwnd.0 as isize;
        let sh = shared.clone();
        let thread = std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
            }
            let hwnd = HWND(hwnd_raw as _);
            let mut started = false;
            let r = unsafe { run(&link, hwnd, opts, &sh, &tx, &mut started) };
            if let Err(e) = r {
                debug_log(&format!("MF: {e:?}"));
                if started {
                    sh.failed.store(true, Ordering::SeqCst);
                    unsafe {
                        let _ = PostMessageW(Some(hwnd), notify_msg, WPARAM(0), LPARAM(0));
                    }
                } else {
                    let _ = tx.send(Err(format!("{e:?}")));
                }
            }
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
            sh.done.store(true, Ordering::SeqCst);
        });

        let mut v = MfViewer { shared, thread: Some(thread), format: String::new(), started: Instant::now() };
        match rx.recv_timeout(Duration::from_secs(6)) {
            Ok(Ok(format)) => {
                v.format = format;
                Ok(v)
            }
            Ok(Err(e)) => Err(e), // v drops: joins the finished thread
            Err(_) => Err("no frame within 6 s".into()),
        }
    }

    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::SeqCst)
    }

    pub fn debug_snapshot(&self) {
        let n = self.shared.frames.load(Ordering::Relaxed);
        let d = self.shared.dropped.load(Ordering::Relaxed);
        let secs = self.started.elapsed().as_secs_f64();
        debug_log(&format!("MF: {n} frames ({d} dropped) in {secs:.1} s = {:.1} fps [{}]", n as f64 / secs, self.format));
    }
}

impl Drop for MfViewer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // ReadSample returns with the next frame (or an error), so this is quick;
        // if a device hangs, detach instead of freezing the UI.
        let deadline = Instant::now() + Duration::from_millis(1500);
        while !self.shared.done.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.shared.done.load(Ordering::SeqCst) {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker

struct ShutdownOnDrop(IMFMediaSource);
impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Shutdown();
        }
    }
}

#[derive(Clone, Copy)]
struct Frame {
    w: u32,
    h: u32,
    sub: GUID,
    fps: (u32, u32),
    /// D3D11 video processor stream colour space bits.
    color_space: u32,
}

unsafe fn run(
    link: &str,
    hwnd: HWND,
    opts: Options,
    sh: &Shared,
    tx: &mpsc::Sender<Result<String, String>>,
    started: &mut bool,
) -> windows::core::Result<()> {
    // --- D3D11 device, shared with Media Foundation for GPU decoding
    let (mut dev, mut ctx) = (None, None);
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut dev),
        None,
        Some(&mut ctx),
    )?;
    let dev: ID3D11Device = dev.ok_or_else(fail)?;
    let ctx: ID3D11DeviceContext = ctx.ok_or_else(fail)?;
    if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let vdev: ID3D11VideoDevice = dev.cast()?;
    let vctx: ID3D11VideoContext = ctx.cast()?;

    let mut token = 0u32;
    let mut mgr = None;
    MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
    let mgr = mgr.ok_or_else(fail)?;
    mgr.ResetDevice(&dev, token)?;

    // --- device source + low-latency source reader
    let mut a = None;
    MFCreateAttributes(&mut a, 2)?;
    let a = a.ok_or_else(fail)?;
    a.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;
    a.SetString(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, &HSTRING::from(link))?;
    let source = MFCreateDeviceSource(&a)?;
    let _guard = ShutdownOnDrop(source.clone());

    let mut ra = None;
    MFCreateAttributes(&mut ra, 4)?;
    let ra = ra.ok_or_else(fail)?;
    ra.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &mgr)?;
    ra.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    ra.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
    ra.SetUINT32(&MF_LOW_LATENCY, 1)?;
    let reader = MFCreateSourceReaderFromMediaSource(&source, &ra)?;

    // --- formats: pick the device mode, then ask for something displayable
    let (native, desc) = choose_native(&reader, opts.max_height)?;
    reader.SetCurrentMediaType(VS, None, &native)?;
    let native_sub = native.GetGUID(&MF_MT_SUBTYPE)?;
    let displayable = [MFVideoFormat_NV12, MFVideoFormat_YUY2, MFVideoFormat_RGB32];
    if !displayable.contains(&native_sub) {
        let mut ok = false;
        for sub in displayable {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &sub)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, native.GetUINT64(&MF_MT_FRAME_SIZE)?)?;
            if let Ok(r) = native.GetUINT64(&MF_MT_FRAME_RATE) {
                t.SetUINT64(&MF_MT_FRAME_RATE, r)?;
            }
            if reader.SetCurrentMediaType(VS, None, &t).is_ok() {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err(fail());
        }
    }
    let mut frame = describe(&reader.GetCurrentMediaType(VS)?, native_sub)?;
    debug_log(&format!("MF: device mode {desc}, output {}", sub_name(&frame.sub)));

    // --- swap chain on our window
    let factory: IDXGIFactory2 = dev.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()?;
    let (mut cw, mut ch) = client_size(hwnd);
    let mut scd = DXGI_SWAP_CHAIN_DESC1 {
        Width: cw,
        Height: ch,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        ..Default::default()
    };
    let swap = match factory.CreateSwapChainForHwnd(&dev, hwnd, &scd, None, None) {
        Ok(s) => s,
        Err(_) => {
            scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
            factory.CreateSwapChainForHwnd(&dev, hwnd, &scd, None, None)?
        }
    };
    let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
    if let Ok(d1) = dev.cast::<IDXGIDevice1>() {
        let _ = d1.SetMaximumFrameLatency(1);
    }

    let mut vp: Option<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)> = None;
    let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
    let mut upload: Option<ID3D11Texture2D> = None;
    let mut on_gpu = true;

    while !sh.stop.load(Ordering::SeqCst) {
        let (mut flags, mut ts) = (0u32, 0i64);
        let mut sample = None;
        reader.ReadSample(VS, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample))?;
        let flags = MF_SOURCE_READER_FLAG(flags as i32);
        if flags.0 & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) != 0 {
            return Err(fail());
        }
        if flags.0 & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0 {
            frame = describe(&reader.GetCurrentMediaType(VS)?, native_sub)?;
            vp = None;
            out_view = None;
            upload = None;
        }
        let Some(sample) = sample else { continue };

        // window size changed (fullscreen toggle, resize)?
        let (w, h) = client_size(hwnd);
        if (w, h) != (cw, ch) {
            out_view = None;
            swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
            (cw, ch) = (w, h);
        }

        // the frame as a D3D11 texture (GPU decode), or uploaded from memory
        let buf = sample.GetBufferByIndex(0)?;
        let (tex, slice) = match buf.cast::<IMFDXGIBuffer>() {
            Ok(db) => {
                let mut p = std::ptr::null_mut();
                db.GetResource(&ID3D11Texture2D::IID, &mut p)?;
                (ID3D11Texture2D::from_raw(p), db.GetSubresourceIndex()?)
            }
            Err(_) => {
                on_gpu = false;
                (upload_frame(&dev, &ctx, &buf, &frame, &mut upload)?, 0)
            }
        };

        if vp.is_none() {
            let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: frame.fps.0, Denominator: frame.fps.1 },
                InputWidth: frame.w,
                InputHeight: frame.h,
                OutputFrameRate: DXGI_RATIONAL { Numerator: frame.fps.0, Denominator: frame.fps.1 },
                OutputWidth: cw,
                OutputHeight: ch,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&cd)?;
            let p = vdev.CreateVideoProcessor(&en, 0)?;
            vctx.VideoProcessorSetStreamFrameFormat(&p, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&p, 0, false);
            vctx.VideoProcessorSetStreamColorSpace(&p, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: frame.color_space });
            vctx.VideoProcessorSetOutputColorSpace(&p, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 }); // RGB 0-255
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
            };
            vctx.VideoProcessorSetOutputBackgroundColor(&p, false, &black);
            vp = Some((en, p));
            out_view = None;
        }
        let (en, p) = vp.as_ref().unwrap();

        if out_view.is_none() {
            let bb: ID3D11Texture2D = swap.GetBuffer(0)?;
            let d = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut ov = None;
            vdev.CreateVideoProcessorOutputView(&bb, en, &d, Some(&mut ov))?;
            out_view = ov;
        }
        let ov = out_view.as_ref().ok_or_else(fail)?;

        let src = RECT { left: 0, top: 0, right: frame.w as i32, bottom: frame.h as i32 };
        let full = RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 };
        let dst = letterbox(frame.w, frame.h, cw, ch);
        vctx.VideoProcessorSetStreamSourceRect(p, 0, true, Some(&src));
        vctx.VideoProcessorSetStreamDestRect(p, 0, true, Some(&dst));
        vctx.VideoProcessorSetOutputTargetRect(p, true, Some(&full));

        let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice } },
        };
        let mut iv = None;
        vdev.CreateVideoProcessorInputView(&tex, en, &ivd, Some(&mut iv))?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(iv),
            ..Default::default()
        };
        let r = vctx.VideoProcessorBlt(p, ov, 0, std::slice::from_ref(&stream));
        ManuallyDrop::drop(&mut stream.pInputSurface);
        r?;
        // Never wait for vsync: if the previous frame is still queued (source
        // faster than the display), drop this one so no backlog can build up.
        let hr = swap.Present(1, DXGI_PRESENT_DO_NOT_WAIT);
        if hr == DXGI_ERROR_WAS_STILL_DRAWING {
            sh.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            hr.ok()?;
        }

        sh.frames.fetch_add(1, Ordering::Relaxed);
        if !*started {
            *started = true;
            let _ = tx.send(Ok(format!("{desc} -> {}, D3D11", if on_gpu { "GPU" } else { "CPU copy" })));
        }
    }
    Ok(())
}

fn fail() -> windows::core::Error {
    windows::Win32::Foundation::E_FAIL.into()
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32)
}

fn letterbox(vw: u32, vh: u32, cw: u32, ch: u32) -> RECT {
    let (vw, vh, cw, ch) = (vw as i64, vh as i64, cw as i64, ch as i64);
    if vw == 0 || vh == 0 {
        return RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 };
    }
    if cw * vh > ch * vw {
        let w = ch * vw / vh;
        let x = (cw - w) / 2;
        RECT { left: x as i32, top: 0, right: (x + w) as i32, bottom: ch as i32 }
    } else {
        let h = cw * vh / vw;
        let y = (ch - h) / 2;
        RECT { left: 0, top: y as i32, right: cw as i32, bottom: (y + h) as i32 }
    }
}

fn sub_name(g: &GUID) -> String {
    match *g {
        x if x == MFVideoFormat_MJPG => "MJPEG".into(),
        x if x == MFVideoFormat_H264 => "H.264".into(),
        x if x == MFVideoFormat_NV12 => "NV12".into(),
        x if x == MFVideoFormat_YUY2 => "YUY2".into(),
        x if x == MFVideoFormat_RGB32 => "RGB32".into(),
        _ => {
            let b = g.data1.to_le_bytes();
            if b.iter().all(|c| c.is_ascii_alphanumeric()) {
                String::from_utf8_lossy(&b).into_owned()
            } else {
                format!("{g:?}")
            }
        }
    }
}

/// Picks the device mode the same way as the DirectShow path: smooth (>=24 fps)
/// first, then the largest size within `max_height`, then fps, then the format
/// cheapest to show (raw > MJPEG > H.264/HEVC).
unsafe fn choose_native(reader: &IMFSourceReader, max_height: i32) -> windows::core::Result<(IMFMediaType, String)> {
    let max_h = if max_height > 0 { max_height as u32 } else { u32::MAX };
    let mut best: Option<(IMFMediaType, (bool, bool, u64, u64, u8), String)> = None;
    let mut i = 0;
    while let Ok(t) = reader.GetNativeMediaType(VS, i) {
        i += 1;
        let Ok(size) = t.GetUINT64(&MF_MT_FRAME_SIZE) else { continue };
        let (w, h) = ((size >> 32) as u32, size as u32);
        let fps = match t.GetUINT64(&MF_MT_FRAME_RATE) {
            Ok(r) if (r as u32) != 0 => (r >> 32) as f64 / (r as u32) as f64,
            _ => 30.0,
        };
        let sub = t.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        let cost = if sub == MFVideoFormat_H264 || sub == MFVideoFormat_HEVC {
            0
        } else if sub == MFVideoFormat_MJPG {
            1
        } else {
            2
        };
        let score = (fps >= 23.5, h <= max_h, w as u64 * h as u64, (fps * 100.0).round() as u64, cost);
        debug_log(&format!("MF:   mode {w}x{h} {fps:.2} fps {}", sub_name(&sub)));
        if best.as_ref().is_none_or(|b| score > b.1) {
            best = Some((t, score, format!("{w}x{h} @ {fps:.2} fps {}", sub_name(&sub))));
        }
    }
    best.map(|(t, _, d)| (t, d)).ok_or_else(fail)
}

unsafe fn describe(t: &IMFMediaType, native_sub: GUID) -> windows::core::Result<Frame> {
    let size = t.GetUINT64(&MF_MT_FRAME_SIZE)?;
    let (w, h) = ((size >> 32) as u32, size as u32);
    let fps = match t.GetUINT64(&MF_MT_FRAME_RATE) {
        Ok(r) if (r as u32) != 0 => ((r >> 32) as u32, r as u32),
        _ => (30, 1),
    };
    let sub = t.GetGUID(&MF_MT_SUBTYPE)?;
    // JPEG is full-range BT.601; otherwise trust the type, defaulting to
    // studio range with BT.709 for HD and BT.601 for SD.
    let jpeg = native_sub == MFVideoFormat_MJPG;
    let full_range = match t.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) {
        Ok(v) => v == MFNominalRange_0_255.0 as u32,
        Err(_) => jpeg,
    };
    let bt709 = match t.GetUINT32(&MF_MT_YUV_MATRIX) {
        Ok(v) => v == MFVideoTransferMatrix_BT709.0 as u32,
        Err(_) => !jpeg && h >= 720,
    };
    // bit 2: YCbCr_Matrix (1 = BT.709), bits 4-5: Nominal_Range (1 = 16-235, 2 = 0-255)
    let color_space = (if bt709 { 1 << 2 } else { 0 }) | (if full_range { 2 << 4 } else { 1 << 4 });
    Ok(Frame { w, h, sub, fps, color_space })
}

/// Software-decoded frame: copy it into a dynamic texture the video processor can read.
unsafe fn upload_frame(
    dev: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    buf: &IMFMediaBuffer,
    f: &Frame,
    cache: &mut Option<ID3D11Texture2D>,
) -> windows::core::Result<ID3D11Texture2D> {
    let (format, row_bytes, planes) = match f.sub {
        s if s == MFVideoFormat_NV12 => (DXGI_FORMAT_NV12, f.w as usize, 2),
        s if s == MFVideoFormat_YUY2 => (DXGI_FORMAT_YUY2, f.w as usize * 2, 1),
        _ => (DXGI_FORMAT_B8G8R8A8_UNORM, f.w as usize * 4, 1),
    };
    if cache.is_none() {
        let d = D3D11_TEXTURE2D_DESC {
            Width: f.w,
            Height: f.h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        dev.CreateTexture2D(&d, None, Some(&mut t))?;
        *cache = t;
    }
    let tex = cache.clone().ok_or_else(fail)?;

    let (mut p, mut pitch) = (std::ptr::null_mut::<u8>(), 0i32);
    let b2 = buf.cast::<IMF2DBuffer>().ok();
    match &b2 {
        Some(b) => b.Lock2D(&mut p, &mut pitch)?,
        None => {
            buf.Lock(&mut p, None, None)?;
            pitch = row_bytes as i32;
        }
    }
    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
    let r = ctx.Map(&tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m));
    if r.is_ok() {
        let dst = m.pData as *mut u8;
        let rows = f.h as usize;
        for i in 0..rows {
            std::ptr::copy_nonoverlapping(p.offset(i as isize * pitch as isize), dst.add(i * m.RowPitch as usize), row_bytes);
        }
        if planes == 2 {
            // NV12 chroma plane follows the luma plane in both layouts
            let src_uv = p.offset(rows as isize * pitch as isize);
            let dst_uv = dst.add(rows * m.RowPitch as usize);
            for i in 0..rows / 2 {
                std::ptr::copy_nonoverlapping(src_uv.offset(i as isize * pitch as isize), dst_uv.add(i * m.RowPitch as usize), row_bytes);
            }
        }
        ctx.Unmap(&tex, 0);
    }
    match &b2 {
        Some(b) => {
            let _ = b.Unlock2D();
        }
        None => {
            let _ = buf.Unlock();
        }
    }
    r?;
    Ok(tex)
}
