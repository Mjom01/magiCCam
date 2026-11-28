//! DirectShow capture: device enumeration + a minimal "source -> renderer" graph.
//!
//! DirectShow is used on purpose: it is the one API on Windows that sees *every*
//! kind of video source — UVC webcams, the OBS Virtual Camera, capture cards,
//! Blackmagic DeckLink / UltraStudio / Intensity, ATEM Mini (UVC) — and it can
//! render straight into our window with a GPU renderer (EVR / VMR-9), so in the
//! common case no frame ever touches our own code.
//!
//! Some sources are picky (e.g. software virtual cameras insist on their own
//! buffers and refuse to feed a GPU renderer directly), so opening walks a small
//! plan of renderer x connection x format combinations, verifies that the stream
//! actually keeps running, and remembers what worked per device.

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;

use windows::core::{w, Interface, GUID, PCWSTR};
use windows::Win32::Foundation::{COLORREF, E_FAIL, HWND, RECT};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Media::DirectShow::*;
use windows::Win32::Media::MediaFoundation::{
    IMFGetService, IMFVideoDisplayControl, AM_MEDIA_TYPE, CLSID_CaptureGraphBuilder2,
    CLSID_EnhancedVideoRenderer, CLSID_FilterGraph, CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory,
    CLSID_VideoMixingRenderer9, FORMAT_VideoInfo, FORMAT_VideoInfo2, MEDIATYPE_Interleaved, MEDIATYPE_Video,
    MFVideoARMode_PreservePicture, MR_VIDEO_RENDER_SERVICE, PIN_CATEGORY_CAPTURE, PIN_CATEGORY_PREVIEW,
    VIDEOINFOHEADER, VIDEOINFOHEADER2,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IMoniker, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BSTR};
use windows::Win32::UI::WindowsAndMessaging::{WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS};

/// Set MAGICCAM_DEBUG=1 to get %TEMP%\magiccam_debug.txt
pub fn debug_log(msg: &str) {
    use std::io::Write;
    if std::env::var_os("MAGICCAM_DEBUG").is_none() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("magiccam_debug.txt"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

// ---------------------------------------------------------------------------
// Devices

pub struct Device {
    pub name: String,
    /// Unique hardware path (absent for software sources such as OBS Virtual Camera).
    pub path: Option<String>,
    moniker: IMoniker,
}

impl Device {
    /// Stable key used to remember the device between runs.
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

#[derive(Clone, Copy, Default, PartialEq)]
pub enum ClockMode {
    /// Clock for hardware sources, none for software virtual cameras.
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Engine {
    /// Media Foundation (GPU decode) for hardware devices, DirectShow otherwise.
    #[default]
    Auto,
    Mf,
    DirectShow,
}

#[derive(Clone, Copy, Default)]
pub struct Options {
    pub engine: Engine,
    pub clock: ClockMode,
    /// Never touch the device's output format (always true for DeckLink-style cards).
    pub keep_format: bool,
    /// Upper bound for the auto-selected resolution (keeps MJPEG decode cheap on weak CPUs).
    pub max_height: i32,
}

// ---------------------------------------------------------------------------
// Viewer: whatever is showing the live picture

pub trait Viewer {
    fn format(&self) -> &str;
    fn renderer_name(&self) -> &'static str;
    fn resize(&self, rc: RECT);
    fn repaint(&self, hwnd: HWND, hdc: HDC);
    /// Drains notifications; true when the source is gone or the stream died.
    fn failed(&self) -> bool;
    fn display_changed(&self);
    /// True when the viewer paints the whole client area itself (incl. black bars).
    fn paints_window(&self) -> bool;
    fn debug_snapshot(&self);
}

/// Hardware devices (they have a device path) go through Media Foundation with
/// GPU decoding — lowest latency, like the Windows Camera app. Software sources
/// (OBS Virtual Camera...), DeckLink-style cards, and anything MF cannot open
/// use the DirectShow graph.
pub fn open_viewer(dev: &Device, hwnd: HWND, notify_msg: u32, opts: Options) -> windows::core::Result<Box<dyn Viewer>> {
    if opts.engine != Engine::DirectShow {
        if let Some(path) = &dev.path {
            match crate::mf::MfViewer::open(path, hwnd, notify_msg, opts) {
                Ok(v) => {
                    debug_log(&format!("playing {} via Media Foundation [{}]", dev.name, v.format));
                    return Ok(Box::new(v));
                }
                Err(e) => debug_log(&format!("Media Foundation failed for {}: {e} - using DirectShow", dev.name)),
            }
        }
    }
    Ok(Box::new(Graph::open(dev, hwnd, notify_msg, opts)?))
}

impl Viewer for crate::mf::MfViewer {
    fn format(&self) -> &str {
        &self.format
    }
    fn renderer_name(&self) -> &'static str {
        "Media Foundation + D3D11"
    }
    fn resize(&self, _rc: RECT) {} // the render thread follows the window size
    fn repaint(&self, _hwnd: HWND, _hdc: HDC) {} // continuously presented
    fn failed(&self) -> bool {
        crate::mf::MfViewer::failed(self)
    }
    fn display_changed(&self) {}
    fn paints_window(&self) -> bool {
        true
    }
    fn debug_snapshot(&self) {
        crate::mf::MfViewer::debug_snapshot(self)
    }
}

impl Viewer for Graph {
    fn format(&self) -> &str {
        &self.format
    }
    fn renderer_name(&self) -> &'static str {
        Graph::renderer_name(self)
    }
    fn resize(&self, rc: RECT) {
        Graph::resize(self, rc)
    }
    fn repaint(&self, hwnd: HWND, hdc: HDC) {
        Graph::repaint(self, hwnd, hdc)
    }
    fn failed(&self) -> bool {
        Graph::failed(self)
    }
    fn display_changed(&self) {
        Graph::display_changed(self)
    }
    fn paints_window(&self) -> bool {
        Graph::paints_window(self)
    }
    fn debug_snapshot(&self) {
        Graph::debug_snapshot(self)
    }
}

// ---------------------------------------------------------------------------
// Graph

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// Enhanced Video Renderer (Vista+), GPU scaling.
    Evr,
    /// Video Mixing Renderer 9 (XP+), GPU scaling.
    Vmr9,
    /// Whatever DirectShow picks, in its own child window. Last resort.
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Link {
    /// Source pin straight into the renderer: zero extra work.
    Direct,
    /// Let DirectShow insert decoders/converters (MJPEG, picky virtual cams...).
    Smart,
}

/// Tried in order; for each step every format alternative is tried.
const PLAN: [(Kind, Link); 5] = [
    (Kind::Evr, Link::Direct),
    (Kind::Vmr9, Link::Direct),
    (Kind::Evr, Link::Smart),
    (Kind::Vmr9, Link::Smart),
    (Kind::Default, Link::Smart),
];

/// How many format alternatives (same size & fps, other pixel format) to try.
const MAX_ALTS: usize = 4;

thread_local! {
    /// device key -> (plan step, format alternative) that worked last time.
    static WORKING: RefCell<HashMap<String, (usize, usize)>> = RefCell::new(HashMap::new());
}

enum Renderer {
    Evr(IMFVideoDisplayControl),
    Vmr9(IVMRWindowlessControl9),
    Legacy(IVideoWindow, Option<IBasicVideo>),
}

pub struct Graph {
    control: IMediaControl,
    renderer: Renderer,
    graph: IGraphBuilder,
    pub format: String,
}

enum BuildError {
    /// This format alternative does not exist — stop trying further ones.
    NoSuchFormat,
    Failed(windows::core::Error),
}

impl From<windows::core::Error> for BuildError {
    fn from(e: windows::core::Error) -> Self {
        BuildError::Failed(e)
    }
}

impl Graph {
    pub fn open(dev: &Device, hwnd: HWND, notify_msg: u32, opts: Options) -> windows::core::Result<Graph> {
        let key = dev.key();
        let mut attempts: Vec<(usize, usize)> = Vec::new();
        if let Some(w) = WORKING.with(|m| m.borrow().get(&key).copied()) {
            attempts.push(w);
        }
        for step in 0..PLAN.len() {
            for alt in 0..MAX_ALTS {
                if !attempts.contains(&(step, alt)) {
                    attempts.push((step, alt));
                }
            }
        }

        // Creating the source filter can be slow (some drivers take ~1 s), so one
        // instance is moved from graph to graph between attempts.
        let src: IBaseFilter = unsafe { dev.moniker.BindToObject(None, None)? };
        let mut last = windows::core::Error::from(E_FAIL);
        let mut n_alts = usize::MAX; // learned when an alternative turns out not to exist
        for (step, alt) in attempts {
            if alt >= n_alts {
                continue;
            }
            let (kind, link) = PLAN[step];
            match unsafe { Self::build(dev, &src, hwnd, notify_msg, opts, kind, link, alt) } {
                Ok(g) => {
                    WORKING.with(|m| m.borrow_mut().insert(key, (step, alt)));
                    return Ok(g);
                }
                Err(BuildError::NoSuchFormat) => n_alts = n_alts.min(alt),
                Err(BuildError::Failed(e)) => {
                    debug_log(&format!("{kind:?}/{link:?} alt {alt} failed: {e:?}"));
                    last = e;
                }
            }
        }
        WORKING.with(|m| m.borrow_mut().remove(&dev.key()));
        Err(last)
    }

    /// Builds and starts one attempt; on failure the source is taken out of the
    /// graph again so the next attempt can reuse it.
    unsafe fn build(
        dev: &Device,
        src: &IBaseFilter,
        hwnd: HWND,
        notify_msg: u32,
        opts: Options,
        kind: Kind,
        link: Link,
        alt: usize,
    ) -> Result<Graph, BuildError> {
        let graph: IGraphBuilder = CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)?;
        graph.AddFilter(src, w!("Source"))?;
        let r = Self::build_in(&graph, dev, src, hwnd, notify_msg, opts, kind, link, alt);
        if r.is_err() {
            if let Ok(c) = graph.cast::<IMediaControl>() {
                let _ = c.Stop();
            }
            let _ = graph.RemoveFilter(src);
        }
        r
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn build_in(
        graph: &IGraphBuilder,
        dev: &Device,
        src: &IBaseFilter,
        hwnd: HWND,
        notify_msg: u32,
        opts: Options,
        kind: Kind,
        link: Link,
        alt: usize,
    ) -> Result<Graph, BuildError> {
        let cgb: ICaptureGraphBuilder2 = CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER)?;
        cgb.SetFiltergraph(graph)?;

        let lname = dev.name.to_lowercase();
        let pro_card = ["decklink", "intensity", "ultrastudio"].iter().any(|k| lname.contains(k));
        let mut format = String::from("device default");
        if !(opts.keep_format || pro_card) {
            match set_format(&cgb, src, opts.max_height, alt) {
                Some(f) => format = f,
                None if alt > 0 => return Err(BuildError::NoSuchFormat),
                None => {}
            }
        } else if alt > 0 {
            return Err(BuildError::NoSuchFormat);
        }

        let (renderer, rfilter): (Option<Renderer>, Option<IBaseFilter>) = match kind {
            Kind::Evr => {
                let f: IBaseFilter = CoCreateInstance(&CLSID_EnhancedVideoRenderer, None, CLSCTX_INPROC_SERVER)?;
                graph.AddFilter(&f, w!("EVR"))?;
                let ctl: IMFVideoDisplayControl = f.cast::<IMFGetService>()?.GetService(&MR_VIDEO_RENDER_SERVICE)?;
                ctl.SetVideoWindow(hwnd)?;
                ctl.SetAspectRatioMode(MFVideoARMode_PreservePicture.0 as u32)?;
                let _ = ctl.SetBorderColor(COLORREF(0));
                (Some(Renderer::Evr(ctl)), Some(f))
            }
            Kind::Vmr9 => {
                let f: IBaseFilter = CoCreateInstance(&CLSID_VideoMixingRenderer9, None, CLSCTX_INPROC_SERVER)?;
                graph.AddFilter(&f, w!("VMR9"))?;
                f.cast::<IVMRFilterConfig9>()?.SetRenderingMode(VMR9Mode_Windowless.0 as u32)?;
                let ctl: IVMRWindowlessControl9 = f.cast()?;
                ctl.SetVideoClippingWindow(hwnd)?;
                ctl.SetAspectRatioMode(VMR9ARMode_LetterBox.0 as u32)?;
                let _ = ctl.SetBorderColor(COLORREF(0));
                (Some(Renderer::Vmr9(ctl)), Some(f))
            }
            Kind::Default => (None, None),
        };

        let mut ok = false;
        'pins: for cat in [Some(&PIN_CATEGORY_CAPTURE), Some(&PIN_CATEGORY_PREVIEW), None] {
            for mt in [&MEDIATYPE_Video, &MEDIATYPE_Interleaved] {
                let Ok(pin) = cgb.FindPin(src, PINDIR_OUTPUT, cat.map(|c| c as *const GUID), Some(mt as *const GUID), true, 0)
                else {
                    continue;
                };
                let r = match (&rfilter, link) {
                    (Some(f), Link::Direct) => match pin_of(f, PINDIR_INPUT) {
                        Some(ip) => graph.ConnectDirect(&pin, &ip, None),
                        None => Err(E_FAIL.into()),
                    },
                    (Some(f), Link::Smart) => match pin_of(f, PINDIR_INPUT) {
                        Some(ip) => graph.Connect(&pin, &ip),
                        None => Err(E_FAIL.into()),
                    },
                    (None, _) => graph.Render(&pin),
                };
                if r.is_ok() {
                    ok = true;
                    break 'pins;
                }
            }
        }
        if !ok && link == Link::Smart {
            // Odd sources: let the capture graph builder figure it out.
            'outer: for cat in [&PIN_CATEGORY_CAPTURE, &PIN_CATEGORY_PREVIEW] {
                for mt in [&MEDIATYPE_Video, &MEDIATYPE_Interleaved] {
                    if cgb.RenderStream(Some(cat as *const GUID), mt, src, None::<&IBaseFilter>, rfilter.as_ref()).is_ok() {
                        ok = true;
                        break 'outer;
                    }
                }
            }
        }
        if !ok {
            return Err(BuildError::Failed(E_FAIL.into()));
        }

        let renderer = match renderer {
            Some(r) => r,
            None => {
                let vw: IVideoWindow = graph.cast()?;
                vw.SetOwner(hwnd.0 as isize)?;
                vw.SetWindowStyle((WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN).0 as i32)?;
                let _ = vw.SetMessageDrain(hwnd.0 as isize);
                Renderer::Legacy(vw, graph.cast().ok())
            }
        };

        log_filters(graph);
        // Clock: hardware sources (UVC/KS devices such as webcams, ATEM, capture
        // cards, DeckLink) timestamp frames against the graph clock, so with a
        // clock the renderer drops late frames and latency stays minimal — without
        // one, any startup backlog would become permanent delay. Software
        // DirectShow sources (OBS Virtual Camera, NVIDIA Broadcast) use timestamps
        // no graph clock matches, so their frames are only shown without a clock.
        let use_clock = match opts.clock {
            ClockMode::On => true,
            ClockMode::Off => false,
            ClockMode::Auto => dev.path.is_some() || pro_card,
        };
        if !use_clock {
            if let Ok(mf) = graph.cast::<IMediaFilter>() {
                let _ = mf.SetSyncSource(None::<&windows::Win32::Media::IReferenceClock>);
            }
        }
        let control: IMediaControl = graph.cast()?;
        let g = Graph { control, renderer, graph: graph.clone(), format };
        g.control.Run()?;

        // Converter chains sometimes connect fine and then stop at once
        // (e.g. a pixel format nobody can convert). Give them a moment.
        if link == Link::Smart && g.stopped_within(600) {
            return Err(BuildError::Failed(E_FAIL.into()));
        }

        if let Ok(ev) = g.graph.cast::<IMediaEventEx>() {
            let _ = ev.SetNotifyWindow(hwnd.0 as isize, notify_msg as i32, 0);
        }
        debug_log(&format!("playing {}: {kind:?}/{link:?} alt {alt} clock={use_clock} [{}]", dev.name, g.format));
        Ok(g)
    }

    /// Waits up to `ms` for an event saying the stream ended or failed.
    unsafe fn stopped_within(&self, ms: i32) -> bool {
        let Ok(ev) = self.graph.cast::<IMediaEvent>() else { return false };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms as u64);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as i32;
            if left <= 0 {
                return false;
            }
            let (mut code, mut p1, mut p2) = (0i32, 0isize, 0isize);
            if ev.GetEvent(&mut code, &mut p1, &mut p2, left).is_err() {
                return false; // timeout: still running
            }
            let _ = ev.FreeEventParams(code, p1, p2);
            if is_failure(code as u32) {
                return true;
            }
        }
    }

    /// Drains graph events; returns true when the device is gone or the stream died.
    pub fn failed(&self) -> bool {
        let mut failed = false;
        unsafe {
            let Ok(ev) = self.graph.cast::<IMediaEventEx>() else { return false };
            let (mut code, mut p1, mut p2) = (0i32, 0isize, 0isize);
            while ev.GetEvent(&mut code, &mut p1, &mut p2, 0).is_ok() {
                debug_log(&format!("event {code} p1={p1:#x} p2={p2:#x}"));
                failed |= is_failure(code as u32);
                let _ = ev.FreeEventParams(code, p1, p2);
            }
        }
        failed
    }

    pub fn resize(&self, rc: RECT) {
        unsafe {
            match &self.renderer {
                Renderer::Evr(ctl) => {
                    let _ = ctl.SetVideoPosition(std::ptr::null(), &rc);
                }
                Renderer::Vmr9(ctl) => {
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

    /// Called from WM_PAINT so the last frame is redrawn.
    pub fn repaint(&self, hwnd: HWND, hdc: HDC) {
        unsafe {
            match &self.renderer {
                Renderer::Evr(ctl) => {
                    let _ = ctl.RepaintVideo();
                }
                Renderer::Vmr9(ctl) => {
                    let _ = ctl.RepaintVideo(hwnd, hdc);
                }
                Renderer::Legacy(..) => {}
            }
        }
    }

    /// Debug aid: logs size and mean brightness of the frame on screen.
    pub fn debug_snapshot(&self) {
        unsafe {
            let (w, h, mean) = match &self.renderer {
                Renderer::Evr(ctl) => {
                    let mut bih = windows::Win32::Graphics::Gdi::BITMAPINFOHEADER {
                        biSize: size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32,
                        ..Default::default()
                    };
                    let (mut dib, mut cb, mut ts) = (std::ptr::null_mut(), 0u32, 0i64);
                    if let Err(e) = ctl.GetCurrentImage(&mut bih, &mut dib, &mut cb, &mut ts) {
                        debug_log(&format!("snapshot failed: {e:?}"));
                        return;
                    }
                    let data = std::slice::from_raw_parts(dib, cb as usize);
                    let mean = data.iter().map(|&b| b as u64).sum::<u64>() as f64 / cb.max(1) as f64;
                    CoTaskMemFree(Some(dib as _));
                    (bih.biWidth, bih.biHeight, mean)
                }
                _ => {
                    debug_log("snapshot: only implemented for EVR");
                    return;
                }
            };
            debug_log(&format!("snapshot {w}x{h} mean byte value {mean:.1}"));
        }
    }

    pub fn display_changed(&self) {
        if let Renderer::Vmr9(ctl) = &self.renderer {
            unsafe {
                let _ = ctl.DisplayModeChanged();
            }
        }
    }

    /// True when the renderer paints the whole client area itself (incl. black bars).
    pub fn paints_window(&self) -> bool {
        !matches!(self.renderer, Renderer::Legacy(..))
    }

    pub fn renderer_name(&self) -> &'static str {
        match self.renderer {
            Renderer::Evr(_) => "EVR",
            Renderer::Vmr9(_) => "VMR-9",
            Renderer::Legacy(..) => "legacy renderer",
        }
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        unsafe {
            let _ = self.control.Stop();
            if let Renderer::Legacy(vw, _) = &self.renderer {
                let _ = vw.SetVisible(0);
                let _ = vw.SetOwner(0);
            }
            if let Ok(ev) = self.graph.cast::<IMediaEventEx>() {
                let _ = ev.SetNotifyWindow(0, 0, 0);
            }
        }
    }
}

fn is_failure(code: u32) -> bool {
    code == EC_DEVICE_LOST || code == EC_ERRORABORT || code == EC_USERABORT || code == EC_COMPLETE
}

unsafe fn pin_of(f: &IBaseFilter, dir: PIN_DIRECTION) -> Option<IPin> {
    let pins = f.EnumPins().ok()?;
    loop {
        let mut p = [None];
        let mut n = 0u32;
        if pins.Next(&mut p, Some(&mut n)).is_err() || n == 0 {
            return None;
        }
        let pin = p[0].take()?;
        if pin.QueryDirection().ok() == Some(dir) {
            return Some(pin);
        }
    }
}

unsafe fn log_filters(graph: &IGraphBuilder) {
    if std::env::var_os("MAGICCAM_DEBUG").is_none() {
        return;
    }
    let Ok(ef) = graph.EnumFilters() else { return };
    loop {
        let mut f = [None];
        let mut n = 0u32;
        if ef.Next(&mut f, Some(&mut n)).is_err() || n == 0 {
            break;
        }
        if let Some(f) = &f[0] {
            let mut info = FILTER_INFO::default();
            if f.QueryFilterInfo(&mut info).is_ok() {
                ManuallyDrop::drop(&mut info.pGraph);
                let end = info.achName.iter().position(|&c| c == 0).unwrap_or(128);
                debug_log(&format!("  filter: {}", String::from_utf16_lossy(&info.achName[..end])));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Format selection

const MJPG: GUID = GUID::from_u128(0x47504A4D_0000_0010_8000_00AA00389B71);
const H264: GUID = GUID::from_u128(0x34363248_0000_0010_8000_00AA00389B71);
const HEVC: GUID = GUID::from_u128(0x43564548_0000_0010_8000_00AA00389B71);
const NV12: GUID = GUID::from_u128(0x3231564E_0000_0010_8000_00AA00389B71);
const YUY2: GUID = GUID::from_u128(0x32595559_0000_0010_8000_00AA00389B71);
const I420: GUID = GUID::from_u128(0x30323449_0000_0010_8000_00AA00389B71);

struct Caps {
    w: i32,
    h: i32,
    fps: f64,
    sub: GUID,
}

fn subtype_name(g: &GUID) -> String {
    match *g {
        MJPG => "MJPEG".into(),
        H264 => "H.264".into(),
        HEVC => "HEVC".into(),
        NV12 => "NV12".into(),
        YUY2 => "YUY2".into(),
        I420 => "I420".into(),
        _ => {
            // FOURCC-style subtypes: print the four characters
            let b = g.data1.to_le_bytes();
            if g.data2 == 0 && b.iter().all(|c| c.is_ascii_alphanumeric()) {
                String::from_utf8_lossy(&b).into_owned()
            } else {
                "RGB".into()
            }
        }
    }
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

/// Ranks the device's formats and applies alternative number `alt`.
///
/// Best mode: smooth (>=24 fps) first, then the largest resolution not above
/// `max_height`, then the highest fps. Alternatives are the other pixel formats
/// of that same mode, cheapest to display first (raw > MJPEG > H.264/HEVC).
unsafe fn set_format(cgb: &ICaptureGraphBuilder2, src: &IBaseFilter, max_height: i32, alt: usize) -> Option<String> {
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

    // (index, mode score, pixel-format cost)
    let mut all: Vec<(i32, (bool, bool, i64, i64), u8)> = Vec::new();
    for i in 0..count {
        let mut pmt = std::ptr::null_mut();
        if cfg.GetStreamCaps(i, &mut pmt, buf.as_mut_ptr()).is_err() || pmt.is_null() {
            continue;
        }
        if let Some(c) = parse(&*pmt) {
            let cost = match c.sub {
                H264 | HEVC => 0,
                MJPG => 1,
                _ => 2,
            };
            let mode = (c.fps >= 23.5, c.h <= max_h, c.w as i64 * c.h as i64, (c.fps * 100.0).round() as i64);
            all.push((i, mode, cost));
        }
        free_mt(pmt);
    }
    let best_mode = all.iter().map(|a| a.1).max()?;
    let mut alts: Vec<_> = all.into_iter().filter(|a| a.1 == best_mode).collect();
    alts.sort_by_key(|a| std::cmp::Reverse(a.2)); // stable: keeps the device's own order otherwise
    let idx = alts.get(alt)?.0;

    let mut pmt = std::ptr::null_mut();
    cfg.GetStreamCaps(idx, &mut pmt, buf.as_mut_ptr()).ok()?;
    let desc = parse(&*pmt).map(|c| format!("{}x{} @ {:.2} fps {}", c.w, c.h, c.fps, subtype_name(&c.sub)));
    let ok = cfg.SetFormat(pmt).is_ok();
    free_mt(pmt);
    if ok { desc } else { None }
}
