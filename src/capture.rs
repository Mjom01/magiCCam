//! DirectShow capture: device enumeration + a minimal "source -> renderer" graph.
//!
//! DirectShow is used on purpose: it is the one API on Windows that sees *every*
//! kind of video source — UVC webcams, the OBS Virtual Camera, capture cards,
//! Blackmagic DeckLink / UltraStudio / Intensity, ATEM Mini (UVC) — and it can
//! render straight into our window via the Enhanced Video Renderer, so no frame
//! ever touches our own code (zero copies, GPU scaling, works on ancient boxes).

use std::mem::ManuallyDrop;

use windows::core::{w, Interface, GUID, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Media::DirectShow::*;
use windows::Win32::Media::MediaFoundation::{
    IMFGetService, IMFVideoDisplayControl, AM_MEDIA_TYPE, CLSID_CaptureGraphBuilder2,
    CLSID_EnhancedVideoRenderer, CLSID_FilterGraph, CLSID_SystemDeviceEnum,
    CLSID_VideoInputDeviceCategory, FORMAT_VideoInfo, FORMAT_VideoInfo2, MEDIATYPE_Interleaved,
    MEDIATYPE_Video, MFVideoARMode_PreservePicture, MR_VIDEO_RENDER_SERVICE, PIN_CATEGORY_CAPTURE,
    PIN_CATEGORY_PREVIEW, VIDEOINFOHEADER, VIDEOINFOHEADER2,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IMoniker, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BSTR};
use windows::Win32::UI::WindowsAndMessaging::{WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS};

pub struct Device {
    pub name: String,
    /// Unique hardware path (absent for software sources such as OBS Virtual Camera).
    pub path: Option<String>,
    moniker: IMoniker,
}

impl Device {
    /// Stable key used to remember the last device between runs.
    pub fn key(&self) -> String {
        self.path.clone().unwrap_or_else(|| self.name.clone())
    }
}

pub fn enumerate() -> Vec<Device> {
    let mut out = Vec::new();
    unsafe {
        let Ok(dev_enum) =
            CoCreateInstance::<_, ICreateDevEnum>(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)
        else {
            return out;
        };
        let mut em = None;
        if dev_enum.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut em, 0).is_err() {
            return out;
        }
        let Some(em) = em else { return out }; // S_FALSE: no devices
        loop {
            let mut m = [None];
            let mut fetched = 0u32;
            if em.Next(&mut m, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(moniker) = m[0].take() else { break };
            let Ok(bag) = moniker.BindToStorage::<_, _, IPropertyBag>(None, None) else { continue };
            let name = read_prop(&bag, w!("FriendlyName")).unwrap_or_else(|| "Unknown camera".into());
            let path = read_prop(&bag, w!("DevicePath"));
            out.push(Device { name, path, moniker });
        }
    }
    out
}

unsafe fn read_prop(bag: &IPropertyBag, name: PCWSTR) -> Option<String> {
    let mut var = VARIANT::default();
    bag.Read(name, &mut var, None).ok()?;
    let inner = &var.Anonymous.Anonymous;
    let s = if inner.vt == VT_BSTR {
        Some(inner.Anonymous.bstrVal.to_string())
    } else {
        None
    };
    let _ = VariantClear(&mut var);
    s.filter(|s| !s.is_empty())
}

#[derive(Clone, Copy, Default)]
pub struct Options {
    /// Never touch the device's output format (always true for DeckLink-style cards).
    pub keep_format: bool,
    /// Upper bound for the auto-selected resolution (keeps MJPEG decode cheap on weak CPUs).
    pub max_height: i32,
}

enum Renderer {
    Evr(IMFVideoDisplayControl),
    Legacy(IVideoWindow, Option<IBasicVideo>),
}

pub struct Graph {
    control: IMediaControl,
    renderer: Renderer,
    _graph: IGraphBuilder,
    pub format: String,
}

impl Graph {
    pub fn open(dev: &Device, hwnd: HWND, notify_msg: u32, opts: Options) -> windows::core::Result<Graph> {
        // Prefer EVR (GPU scaling, letterboxing built in); fall back to whatever
        // renderer DirectShow picks (VMR/old Video Renderer) on very old systems.
        unsafe {
            match Self::build(dev, hwnd, notify_msg, opts, true) {
                Ok(g) => Ok(g),
                Err(_) => Self::build(dev, hwnd, notify_msg, opts, false),
            }
        }
    }

    unsafe fn build(dev: &Device, hwnd: HWND, notify_msg: u32, opts: Options, use_evr: bool) -> windows::core::Result<Graph> {
        let graph: IGraphBuilder = CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)?;
        let cgb: ICaptureGraphBuilder2 = CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER)?;
        cgb.SetFiltergraph(&graph)?;

        let src: IBaseFilter = dev.moniker.BindToObject(None, None)?;
        graph.AddFilter(&src, w!("Source"))?;

        let lname = dev.name.to_lowercase();
        let pro_card = ["decklink", "intensity", "ultrastudio"].iter().any(|k| lname.contains(k));
        let mut format = String::from("device default");
        if !(opts.keep_format || pro_card) {
            if let Some(f) = choose_format(&cgb, &src, opts.max_height) {
                format = f;
            }
        }

        let mut evr_ctl = None;
        let mut evr_filter: Option<IBaseFilter> = None;
        if use_evr {
            let evr: IBaseFilter = CoCreateInstance(&CLSID_EnhancedVideoRenderer, None, CLSCTX_INPROC_SERVER)?;
            graph.AddFilter(&evr, w!("EVR"))?;
            let svc: IMFGetService = evr.cast()?;
            let ctl: IMFVideoDisplayControl = svc.GetService(&MR_VIDEO_RENDER_SERVICE)?;
            ctl.SetVideoWindow(hwnd)?;
            ctl.SetAspectRatioMode(MFVideoARMode_PreservePicture.0 as u32)?;
            let _ = ctl.SetBorderColor(COLORREF(0));
            evr_ctl = Some(ctl);
            evr_filter = Some(evr);
        }

        // Capture pin first (full frame rate, no Smart Tee), then preview pin;
        // Interleaved covers DV-style sources.
        let mut ok = false;
        'outer: for cat in [&PIN_CATEGORY_CAPTURE, &PIN_CATEGORY_PREVIEW] {
            for mt in [&MEDIATYPE_Video, &MEDIATYPE_Interleaved] {
                if cgb
                    .RenderStream(Some(cat as *const GUID), mt, &src, None::<&IBaseFilter>, evr_filter.as_ref())
                    .is_ok()
                {
                    ok = true;
                    break 'outer;
                }
            }
        }
        if !ok {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
        }

        let renderer = match evr_ctl {
            Some(ctl) => Renderer::Evr(ctl),
            None => {
                let vw: IVideoWindow = graph.cast()?;
                vw.SetOwner(hwnd.0 as isize)?;
                vw.SetWindowStyle((WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN).0 as i32)?;
                let _ = vw.SetMessageDrain(hwnd.0 as isize);
                Renderer::Legacy(vw, graph.cast().ok())
            }
        };

        if let Ok(ev) = graph.cast::<IMediaEventEx>() {
            let _ = ev.SetNotifyWindow(hwnd.0 as isize, notify_msg as i32, 0);
        }

        let control: IMediaControl = graph.cast()?;
        let g = Graph { control, renderer, _graph: graph, format };
        g.control.Run()?;
        Ok(g)
    }

    pub fn resize(&self, rc: RECT) {
        unsafe {
            match &self.renderer {
                Renderer::Evr(ctl) => {
                    let _ = ctl.SetVideoPosition(std::ptr::null(), &rc);
                }
                Renderer::Legacy(vw, bv) => {
                    let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
                    let (mut vw_, mut vh_) = (0i32, 0i32);
                    let (mut x, mut y, mut w, mut h) = (0, 0, cw, ch);
                    if let Some(bv) = bv {
                        if bv.GetVideoSize(&mut vw_, &mut vh_).is_ok() && vw_ > 0 && vh_ > 0 {
                            // letterbox / pillarbox
                            if cw as i64 * vh_ as i64 > ch as i64 * vw_ as i64 {
                                w = (ch as i64 * vw_ as i64 / vh_ as i64) as i32;
                                x = (cw - w) / 2;
                            } else {
                                h = (cw as i64 * vh_ as i64 / vw_ as i64) as i32;
                                y = (ch - h) / 2;
                            }
                        }
                    }
                    let _ = vw.SetWindowPosition(x, y, w, h);
                }
            }
        }
    }

    /// Needed in WM_PAINT for EVR (windowless) so the last frame is redrawn.
    pub fn repaint(&self) {
        if let Renderer::Evr(ctl) = &self.renderer {
            unsafe {
                let _ = ctl.RepaintVideo();
            }
        }
    }

    pub fn is_evr(&self) -> bool {
        matches!(self.renderer, Renderer::Evr(_))
    }
}

/// Drains graph events; returns true when the device is gone or the stream died.
pub fn device_failed(graph: &Graph) -> bool {
    let mut failed = false;
    unsafe {
        let Ok(ev) = graph._graph.cast::<IMediaEventEx>() else { return false };
        let (mut code, mut p1, mut p2) = (0i32, 0isize, 0isize);
        while ev.GetEvent(&mut code, &mut p1, &mut p2, 0).is_ok() {
            let c = code as u32;
            if c == EC_DEVICE_LOST || c == EC_ERRORABORT || c == EC_USERABORT || c == EC_COMPLETE {
                failed = true;
            }
            let _ = ev.FreeEventParams(code, p1, p2);
        }
    }
    failed
}

impl Drop for Graph {
    fn drop(&mut self) {
        unsafe {
            let _ = self.control.Stop();
            if let Renderer::Legacy(vw, _) = &self.renderer {
                let _ = vw.SetVisible(0);
                let _ = vw.SetOwner(0);
            }
            if let Ok(ev) = self._graph.cast::<IMediaEventEx>() {
                let _ = ev.SetNotifyWindow(0, 0, 0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Format selection

const MJPG: GUID = GUID::from_u128(0x47504A4D_0000_0010_8000_00AA00389B71);
const H264: GUID = GUID::from_u128(0x34363248_0000_0010_8000_00AA00389B71);
const HEVC: GUID = GUID::from_u128(0x43564548_0000_0010_8000_00AA00389B71);

struct Caps {
    w: i32,
    h: i32,
    fps: f64,
    sub: GUID,
}

unsafe fn parse(mt: &AM_MEDIA_TYPE) -> Option<Caps> {
    if mt.majortype != MEDIATYPE_Video || mt.pbFormat.is_null() {
        return None;
    }
    let (atpf, bmi) = if mt.formattype == FORMAT_VideoInfo && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER>() {
        let v = &*(mt.pbFormat as *const VIDEOINFOHEADER);
        (v.AvgTimePerFrame, v.bmiHeader)
    } else if mt.formattype == FORMAT_VideoInfo2 && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER2>() {
        let v = &*(mt.pbFormat as *const VIDEOINFOHEADER2);
        (v.AvgTimePerFrame, v.bmiHeader)
    } else {
        return None;
    };
    let fps = if atpf > 0 { 10_000_000.0 / atpf as f64 } else { 30.0 };
    Some(Caps { w: bmi.biWidth, h: bmi.biHeight.abs(), fps, sub: mt.subtype })
}

unsafe fn free_mt(pmt: *mut AM_MEDIA_TYPE) {
    if pmt.is_null() {
        return;
    }
    let mt = &mut *pmt;
    if mt.cbFormat != 0 && !mt.pbFormat.is_null() {
        CoTaskMemFree(Some(mt.pbFormat as _));
    }
    ManuallyDrop::drop(&mut mt.pUnk);
    CoTaskMemFree(Some(pmt as _));
}

/// Picks the "best live" format: smooth (>=24 fps) first, then the largest
/// resolution not above `max_height`, then the highest fps, then the cheapest
/// to decode (raw > MJPEG > H.264/HEVC, which may need a missing decoder).
unsafe fn choose_format(cgb: &ICaptureGraphBuilder2, src: &IBaseFilter, max_height: i32) -> Option<String> {
    let mut cfg: Option<IAMStreamConfig> = None;
    for cat in [&PIN_CATEGORY_CAPTURE, &PIN_CATEGORY_PREVIEW] {
        let mut p = std::ptr::null_mut();
        if cgb
            .FindInterface(Some(cat as *const GUID), Some(&MEDIATYPE_Video), src, &IAMStreamConfig::IID, &mut p)
            .is_ok()
            && !p.is_null()
        {
            cfg = Some(IAMStreamConfig::from_raw(p));
            break;
        }
    }
    let cfg = cfg?;
    let (mut count, mut size) = (0i32, 0i32);
    cfg.GetNumberOfCapabilities(&mut count, &mut size).ok()?;
    let mut buf = vec![0u8; size.max(256) as usize];
    let max_h = if max_height > 0 { max_height } else { i32::MAX };

    let mut best: Option<(i32, (bool, bool, i64, i64, u8))> = None;
    for i in 0..count {
        let mut pmt = std::ptr::null_mut();
        if cfg.GetStreamCaps(i, &mut pmt, buf.as_mut_ptr()).is_err() || pmt.is_null() {
            continue;
        }
        if let Some(c) = parse(&*pmt) {
            let cost = if c.sub == H264 || c.sub == HEVC { 0 } else if c.sub == MJPG { 1 } else { 2 };
            let score = (
                c.fps >= 23.5,
                c.h <= max_h,
                c.w as i64 * c.h as i64,
                (c.fps * 100.0) as i64,
                cost,
            );
            if best.as_ref().map_or(true, |(_, s)| score > *s) {
                best = Some((i, score));
            }
        }
        free_mt(pmt);
    }

    let (idx, _) = best?;
    let mut pmt = std::ptr::null_mut();
    cfg.GetStreamCaps(idx, &mut pmt, buf.as_mut_ptr()).ok()?;
    let desc = parse(&*pmt).map(|c| {
        let kind = if c.sub == MJPG { "MJPEG" } else if c.sub == H264 { "H.264" } else if c.sub == HEVC { "HEVC" } else { "raw" };
        format!("{}x{} @ {:.2} fps {}", c.w, c.h, c.fps, kind)
    });
    let ok = cfg.SetFormat(pmt).is_ok();
    free_mt(pmt);
    if ok { desc } else { None }
}
