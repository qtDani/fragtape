//! Video playback with Windows' own Media Foundation engine, decoded and scaled on the GPU.
//! Frames are copied out as RGBA at the size they are shown, so any UI can draw them (and draw on top).

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use windows::core::{implement, Interface, BSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

/// Events the engine reports from its own threads, kept as bit flags until read.
#[derive(Default)]
pub struct Flags(AtomicU32);
pub const EV_LOADED: u32 = 1;
pub const EV_SEEKED: u32 = 2;
pub const EV_ENDED: u32 = 4;
pub const EV_ERROR: u32 = 8;
pub const EV_PLAYPAUSE: u32 = 16;

impl Flags {
    /// Read and clear everything.
    pub fn take(&self) -> u32 {
        self.0.swap(0, Ordering::AcqRel)
    }
    /// Read and clear one event; the others stay.
    pub fn take_one(&self, bit: u32) -> bool {
        self.0.fetch_and(!bit, Ordering::AcqRel) & bit != 0
    }
}

#[implement(IMFMediaEngineNotify)]
struct Notify(Arc<Flags>);

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, event: u32, _p1: usize, _p2: u32) -> windows::core::Result<()> {
        let f = match MF_MEDIA_ENGINE_EVENT(event as i32) {
            MF_MEDIA_ENGINE_EVENT_CANPLAY | MF_MEDIA_ENGINE_EVENT_LOADEDDATA => EV_LOADED,
            MF_MEDIA_ENGINE_EVENT_SEEKED => EV_SEEKED,
            MF_MEDIA_ENGINE_EVENT_ENDED => EV_ENDED,
            MF_MEDIA_ENGINE_EVENT_ERROR => EV_ERROR,
            MF_MEDIA_ENGINE_EVENT_PLAY | MF_MEDIA_ENGINE_EVENT_PAUSE => EV_PLAYPAUSE,
            _ => 0,
        };
        self.0 .0.fetch_or(f, Ordering::AcqRel);
        Ok(())
    }
}

pub struct Video {
    engine: IMFMediaEngine,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    target: Option<(u32, u32, ID3D11Texture2D, ID3D11Texture2D)>, // w, h, render target, CPU copy
    pub flags: Arc<Flags>,
    _manager: IMFDXGIDeviceManager,
}

fn startup() {
    // COM is per thread (the window's thread already has it, from winit, so this fails harmlessly there)
    thread_local!(static COM: () = unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); });
    COM.with(|_| {});
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    });
}

impl Video {
    pub fn new(muted: bool) -> windows::core::Result<Video> {
        startup();
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, Default::default(),
                              D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                              Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1]),
                              D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))?;
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
            // the engine decodes on other threads with the same device
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(&device, token)?;

            let flags = Arc::new(Flags::default());
            let notify: IMFMediaEngineNotify = Notify(flags.clone()).into();
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 3)?;
            let attrs = attrs.unwrap();
            attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
            attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
            attrs.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32)?;
            let factory: IMFMediaEngineClassFactory = CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
            let engine = factory.CreateInstance(0, &attrs)?;
            if muted {
                engine.SetMuted(true)?;
            }
            Ok(Video { engine, device, context, target: None, flags, _manager: manager })
        }
    }

    pub fn open(&self, path: &Path) -> windows::core::Result<()> {
        unsafe {
            // load the video, not only its header, so a paused seek can show a frame
            self.engine.SetPreload(MF_MEDIA_ENGINE_PRELOAD_AUTOMATIC)?;
            self.engine.SetSource(&BSTR::from(path.to_string_lossy().as_ref()))
        }
    }
    pub fn play(&self) { unsafe { let _ = self.engine.Play(); } }
    pub fn pause(&self) { unsafe { let _ = self.engine.Pause(); } }
    pub fn paused(&self) -> bool { unsafe { self.engine.IsPaused().as_bool() } }
    pub fn ended(&self) -> bool { unsafe { self.engine.IsEnded().as_bool() } }
    pub fn seek(&self, t: f64) { unsafe { let _ = self.engine.SetCurrentTime(t.max(0.0)); } }
    pub fn seeking(&self) -> bool { unsafe { self.engine.IsSeeking().as_bool() } }
    pub fn time(&self) -> f64 { unsafe { self.engine.GetCurrentTime() } }
    pub fn duration(&self) -> f64 { let d = unsafe { self.engine.GetDuration() }; if d.is_finite() { d } else { 0.0 } }
    pub fn ready(&self) -> bool { unsafe { self.engine.GetReadyState() >= 1 } } // HAVE_METADATA
    pub fn set_volume(&self, v: f64) { unsafe { let _ = self.engine.SetVolume(v.clamp(0.0, 1.0)); } }
    pub fn set_muted(&self, m: bool) { unsafe { let _ = self.engine.SetMuted(m); } }
    pub fn set_rate(&self, r: f64) { unsafe { let _ = self.engine.SetPlaybackRate(r); } }
    pub fn native_size(&self) -> (u32, u32) {
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { let _ = self.engine.GetNativeVideoSize(Some(&mut w), Some(&mut h)); }
        (w, h)
    }
    /// True when the engine has a frame we haven't fetched yet.
    pub fn new_frame(&self) -> bool {
        // S_OK = a new frame; S_FALSE = nothing new (the wrapper reports both as Ok, so ask directly)
        let mut pts = 0i64;
        let hr = unsafe { (Interface::vtable(&self.engine).OnVideoStreamTick)(Interface::as_raw(&self.engine), &mut pts) };
        hr.0 == 0
    }

    fn ensure_target(&mut self, w: u32, h: u32) -> windows::core::Result<()> {
        if matches!(self.target, Some((tw, th, ..)) if tw == w && th == h) {
            return Ok(());
        }
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC {
                Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32, CPUAccessFlags: 0, MiscFlags: 0,
            };
            let mut rt = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut rt))?;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut st = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut st))?;
            self.target = Some((w, h, rt.unwrap(), st.unwrap()));
        }
        Ok(())
    }

    /// The current frame scaled (on the GPU) to fit w x h, letterboxed in black, as RGBA rows.
    pub fn frame(&mut self, w: u32, h: u32, out: &mut Vec<u8>) -> windows::core::Result<()> {
        let (w, h) = (w.max(2), h.max(2));
        self.ensure_target(w, h)?;
        let (vw, vh) = self.native_size();
        let (vw, vh) = (vw.max(1) as f64, vh.max(1) as f64);
        let k = (w as f64 / vw).min(h as f64 / vh);
        let (dw, dh) = ((vw * k).round() as i32, (vh * k).round() as i32);
        let dst = RECT { left: (w as i32 - dw) / 2, top: (h as i32 - dh) / 2, right: (w as i32 - dw) / 2 + dw, bottom: (h as i32 - dh) / 2 + dh };
        let (_, _, rt, st) = self.target.as_ref().unwrap();
        unsafe {
            let black = MFARGB { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbAlpha: 255 };
            self.engine.TransferVideoFrame(rt, None, &dst, Some(&black))?;
            self.context.CopyResource(st, rt);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
            out.resize((w * h * 4) as usize, 0);
            let src = std::slice::from_raw_parts(map.pData as *const u8, (map.RowPitch * h) as usize);
            for y in 0..h as usize {
                let row = &src[y * map.RowPitch as usize..][..w as usize * 4];
                let o = &mut out[y * w as usize * 4..][..w as usize * 4];
                for (d, s) in o.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                    d[0] = s[2]; d[1] = s[1]; d[2] = s[0]; d[3] = 255; // BGRA -> RGBA
                }
            }
            self.context.Unmap(st, 0);
        }
        Ok(())
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        unsafe { let _ = self.engine.Shutdown(); }
    }
}
