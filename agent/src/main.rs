// latency-agent — PC emitter for click-to-photon measurement.
// Spec: SPEC.md §3 (raw input, D3D11 flip swapchain, frame accounting).
//
// Renders a white flash (4 refreshes) on every left mouse click plus a
// continuously-updating Gray-code barcode strip that identifies the
// present index. Logs clicks and per-present frame statistics to CSV.
// ESC quits. `--tearing` switches to SyncInterval 0 + ALLOW_TEARING.

#![cfg(windows)]
#![cfg_attr(windows, windows_subsystem = "windows")]
#![allow(non_snake_case)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    ID3D11VertexShader, ID3D11PixelShader, ID3D11Resource, ID3D11RasterizerState,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_SINGLETHREADED, D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RASTERIZER_DESC,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIEWPORT, D3D11CreateDeviceAndSwapChain,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
    DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIFactory1, IDXGISwapChain, IDXGISwapChain2, DXGI_FRAME_STATISTICS, DXGI_PRESENT,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, CreateDXGIFactory1,
};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetSystemMetrics,
    MessageBoxW, PeekMessageW, PostQuitMessage, RegisterClassW, SetWindowTextW, ShowCursor,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG, MB_ICONERROR, PM_REMOVE,
    RI_MOUSE_LEFT_BUTTON_DOWN, SM_CXSCREEN, SM_CYSCREEN, WINDOW_EX_STYLE, WM_DESTROY, WM_INPUT,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_QUIT, WNDCLASSW, WS_CAPTION, WS_OVERLAPPED, WS_POPUP,
    WS_SYSMENU, WS_VISIBLE,
};

// ---------------------------------------------------------------------------
// Tunables (spec §3.4)

const FLASH_FRAMES: u32 = 4;          // hold flash for 4 refreshes
const CLICK_DEBOUNCE_MS: u64 = 150;   // spec §3.1
// Geometry constants are embedded in the pixel shader; kept here as
// documentation (barcode strip and flash rect in normalized [0,1] coords).
//   barcode: x 0.10–0.90, y 0.02–0.06, 16 Gray-code cells
//   flash:   x 0.15–0.85, y 0.12–0.92 (≥40% screen area, spec §3.4)

// ---------------------------------------------------------------------------
// QPC helpers

fn qpc() -> u64 {
    let mut v: i64 = 0;
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v as u64
}

fn qpc_freq() -> u64 {
    let mut f: i64 = 0;
    let _ = unsafe { QueryPerformanceFrequency(&mut f) };
    f as u64
}

// ---------------------------------------------------------------------------
// Shared state touched from the window procedure

static LAST_CLICK_QPC: AtomicU64 = AtomicU64::new(0);
static CLICK_PENDING: AtomicBool = AtomicBool::new(false);
static CLICK_COUNT: AtomicU32 = AtomicU32::new(0);
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

struct Loggers {
    clicks: Option<File>,
    frames: Option<File>,
    freq: u64,
}

static LOGGERS: Mutex<Option<Loggers>> = Mutex::new(None);

fn log_click(qpc_ts: u64, seq: u32) {
    if let Some(l) = LOGGERS.lock().unwrap().as_mut() {
        if let Some(f) = l.clicks.as_mut() {
            let _ = writeln!(f, "{},{},{}", seq, qpc_ts, (qpc_ts as f64 / l.freq as f64 * 1000.0) as u64);
            let _ = f.flush();
        }
    }
}

fn log_frame(own_index: u32, t_present: u64, stats: &DXGI_FRAME_STATISTICS) {
    if let Some(l) = LOGGERS.lock().unwrap().as_mut() {
        if let Some(f) = l.frames.as_mut() {
            let _ = writeln!(
                f,
                "{},{},{},{},{}",
                own_index, t_present, stats.PresentCount, stats.SyncQPCTime, stats.PresentRefreshCount
            );
            let _ = f.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Window procedure: QPC first, parse later (spec §3.1)

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            // First statement: stamp the clock before touching the packet.
            let now = qpc();

            let hraw = HRAWINPUT(lparam.0 as *mut core::ffi::c_void);
            let mut size: u32 = 0;
            let _ = GetRawInputData(hraw, RID_INPUT, None, &mut size, std::mem::size_of::<RAWINPUTHEADER>() as u32);
            if size == 0 {
                return LRESULT(0);
            }
            let mut buf = vec![0u8; size as usize];
            let written = GetRawInputData(
                hraw,
                RID_INPUT,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            if written == u32::MAX {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let raw = &*(buf.as_ptr() as *const RAWINPUT);
            if raw.header.dwType == RIM_TYPEMOUSE.0 {
                let mouse = &raw.data.mouse;
                // ulButtons lives in the RAWMOUSE_0 union.
                let buttons = mouse.Anonymous.ulButtons;
                if buttons & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
                    register_click(now);
                }
            }
            LRESULT(0)
        }
        // Safety net: also accept the translated message path in case raw
        // input is unavailable. Debounce dedupes against WM_INPUT.
        WM_LBUTTONDOWN => {
            register_click(qpc());
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u32 == VK_ESCAPE.0 as u32 => {
            QUIT_REQUESTED.store(true, Ordering::Relaxed);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn register_click(now: u64) {
    let last = LAST_CLICK_QPC.swap(now, Ordering::Relaxed);
    let debounce = CLICK_DEBOUNCE_MS * qpc_freq() / 1000;
    if last == 0 || now - last >= debounce {
        let seq = CLICK_COUNT.fetch_add(1, Ordering::Relaxed);
        CLICK_PENDING.store(true, Ordering::Release);
        log_click(now, seq);
    }
}

// ---------------------------------------------------------------------------
// Shaders

const VS_SRC: &[u8] = b"struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };\r\nVSOut vs_main(uint vid : SV_VertexID) {\r\n    VSOut o;\r\n    float x = float((vid << 1) & 2);\r\n    float y = float(vid & 2);\r\n    o.uv = float2(x, y);\r\n    o.pos = float4(x * 2 - 1, 1 - y * 2, 0, 1);\r\n    return o;\r\n}\r\n\0";

// Region math is driven by SV_POSITION (pixel coordinates) rather than an
// interpolated TEXCOORD, so it does not depend on attribute interpolation.
const PS_SRC: &[u8] = b"cbuffer FrameCb : register(b0) {\r\n    uint flashActive;\r\n    uint grayCode;\r\n    float2 screenSize;\r\n};\r\n\r\nfloat4 ps_main(float4 pos : SV_POSITION) : SV_Target {\r\n    float2 uv = pos.xy / screenSize;\r\n    if (uv.y >= 0.02 && uv.y <= 0.06 && uv.x >= 0.10 && uv.x <= 0.90) {\r\n        float fx = (uv.x - 0.10) / 0.80;\r\n        uint cell = (uint)(fx * 16.0);\r\n        if (cell > 15) cell = 15;\r\n        uint bit = (grayCode >> cell) & 1u;\r\n        return bit != 0u ? float4(1,1,1,1) : float4(0,0,0,1);\r\n    }\r\n    if (flashActive != 0u && uv.x >= 0.15 && uv.x <= 0.85 && uv.y >= 0.12 && uv.y <= 0.92) {\r\n        return float4(1,1,1,1);\r\n    }\r\n    return float4(0,0,0,1);\r\n}\r\n\0";

/// Diagnostic shader: ignores the constant buffer and returns white for every
/// fragment. Used by --selftest to distinguish "geometry not rasterizing" from
/// "constant buffer not reaching the shader".
const PS_WHITE_SRC: &[u8] =
    b"float4 ps_main(float4 pos : SV_POSITION) : SV_Target { return float4(1,1,1,1); }\r\n\0";

/// Diagnostic shader: R = uv.x, G = uv.y, B = constant-buffer flag.
/// Sampling a grid of points shows whether uv spans the viewport correctly
/// and whether the cbuffer reached the shader.
const PS_DIAG_SRC: &[u8] = b"cbuffer FrameCb : register(b0) {\r\n    uint flashActive;\r\n    uint grayCode;\r\n    float2 screenSize;\r\n};\r\n\r\nfloat4 ps_main(float4 pos : SV_POSITION) : SV_Target {\r\n    float2 uv = pos.xy / screenSize;\r\n    float flag = (flashActive != 0u) ? 1.0 : 0.0;\r\n    return float4(uv.x, uv.y, flag, 1.0);\r\n}\r\n\0";

fn pc(b: &[u8]) -> PCSTR {
    PCSTR(b.as_ptr())
}

// ---------------------------------------------------------------------------
// Renderer

#[repr(C)]
struct FrameCb {
    flash_active: u32,
    gray_code: u32,
    screen_w: f32,
    screen_h: f32,
}

struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain,
    width: f32,
    height: f32,
    cb: ID3D11Buffer,
    ps_scene: ID3D11PixelShader,
    ps_white: ID3D11PixelShader,
    ps_diag: ID3D11PixelShader,
    tearing: bool,
}

unsafe fn create_renderer(hwnd: HWND, width: u32, height: u32, allow_tearing: bool) -> windows::core::Result<Renderer> {
    let mut flags = D3D11_CREATE_DEVICE_FLAG(D3D11_CREATE_DEVICE_SINGLETHREADED.0);
    if allow_tearing {
        flags.0 |= 0x20; // D3D11_CREATE_DEVICE_ALLOW_TEARING
    }
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut sc_flags = DXGI_SWAP_CHAIN_FLAG(DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0);
    if allow_tearing {
        sc_flags.0 |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0;
    }
    let sc_desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: width,
            Height: height,
            RefreshRate: DXGI_RATIONAL { Numerator: 0, Denominator: 1 },
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
            Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
        },
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: hwnd,
        Windowed: true.into(),
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        Flags: sc_flags.0 as u32,
    };

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut swap_chain: Option<IDXGISwapChain> = None;
    D3D11CreateDeviceAndSwapChain(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        None,
        flags,
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&sc_desc),
        Some(&mut swap_chain),
        Some(&mut device),
        None,
        Some(&mut context),
    )?;

    let device = device.unwrap();
    let context = context.unwrap();
    let swap_chain = swap_chain.unwrap();

    // NOTE: no render-target view is cached here on purpose. A flip-model
    // swapchain rotates its buffers on every Present, so the RTV must be
    // re-created from GetBuffer(0) each frame (see render_frame).

    let cb_desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<FrameCb>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut cb: Option<ID3D11Buffer> = None;
    device.CreateBuffer(&cb_desc, None, Some(&mut cb))?;
    let cb = cb.unwrap();

    let mut vs_blob = None;
    D3DCompile(
        VS_SRC.as_ptr() as *const core::ffi::c_void,
        VS_SRC.len() - 1,
        None,
        None,
        None,
        pc(b"vs_main\0"),
        pc(b"vs_5_0\0"),
        0,
        0,
        &mut vs_blob,
        None,
    )?;
    let vs_blob = vs_blob.unwrap();
    let vs_bytes = std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize());
    let mut vs: Option<ID3D11VertexShader> = None;
    device.CreateVertexShader(vs_bytes, None, Some(&mut vs))?;
    let vs = vs.unwrap();

    let mut ps_blob = None;
    D3DCompile(
        PS_SRC.as_ptr() as *const core::ffi::c_void,
        PS_SRC.len() - 1,
        None,
        None,
        None,
        pc(b"ps_main\0"),
        pc(b"ps_5_0\0"),
        0,
        0,
        &mut ps_blob,
        None,
    )?;
    let ps_blob = ps_blob.unwrap();
    let ps_bytes = std::slice::from_raw_parts(ps_blob.GetBufferPointer() as *const u8, ps_blob.GetBufferSize());
    let mut ps: Option<ID3D11PixelShader> = None;
    device.CreatePixelShader(ps_bytes, None, Some(&mut ps))?;
    let ps = ps.unwrap();

    let mut white_blob = None;
    D3DCompile(
        PS_WHITE_SRC.as_ptr() as *const core::ffi::c_void,
        PS_WHITE_SRC.len() - 1,
        None,
        None,
        None,
        pc(b"ps_main\0"),
        pc(b"ps_5_0\0"),
        0,
        0,
        &mut white_blob,
        None,
    )?;
    let white_blob = white_blob.unwrap();
    let white_bytes = std::slice::from_raw_parts(white_blob.GetBufferPointer() as *const u8, white_blob.GetBufferSize());
    let mut ps_white: Option<ID3D11PixelShader> = None;
    device.CreatePixelShader(white_bytes, None, Some(&mut ps_white))?;
    let ps_white = ps_white.unwrap();

    let mut diag_blob = None;
    D3DCompile(
        PS_DIAG_SRC.as_ptr() as *const core::ffi::c_void,
        PS_DIAG_SRC.len() - 1,
        None,
        None,
        None,
        pc(b"ps_main\0"),
        pc(b"ps_5_0\0"),
        0,
        0,
        &mut diag_blob,
        None,
    )?;
    let diag_blob = diag_blob.unwrap();
    let diag_bytes = std::slice::from_raw_parts(diag_blob.GetBufferPointer() as *const u8, diag_blob.GetBufferSize());
    let mut ps_diag: Option<ID3D11PixelShader> = None;
    device.CreatePixelShader(diag_bytes, None, Some(&mut ps_diag))?;
    let ps_diag = ps_diag.unwrap();

    context.VSSetShader(&vs, None);
    context.PSSetShader(&ps, None);
    context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

    // Rasterizer: cull none so the fullscreen triangle always draws.
    let rs_desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        FrontCounterClockwise: false.into(),
        DepthBias: 0,
        DepthBiasClamp: 0.0,
        SlopeScaledDepthBias: 0.0,
        DepthClipEnable: true.into(),
        ScissorEnable: false.into(),
        MultisampleEnable: false.into(),
        AntialiasedLineEnable: false.into(),
    };
    let mut rs: Option<ID3D11RasterizerState> = None;
    device.CreateRasterizerState(&rs_desc, Some(&mut rs))?;
    context.RSSetState(&rs.unwrap());

    // Viewport: without this, nothing renders (D3D11 default is empty).
    let vp = D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: width as f32,
        Height: height as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    context.RSSetViewports(Some(&[vp]));

    Ok(Renderer {
        device,
        context,
        swap_chain,
        width: width as f32,
        height: height as f32,
        cb,
        ps_scene: ps,
        ps_white,
        ps_diag,
        tearing: allow_tearing,
    })
}

impl Renderer {
    unsafe fn render_frame(&self, flash_active: bool, gray_code: u32, vsync: bool) -> windows::core::Result<()> {
        // Re-acquire the current back buffer: with DXGI_SWAP_EFFECT_FLIP_DISCARD
        // the buffer index rotates after every Present, so a cached RTV would
        // point at the buffer that is currently on screen.
        let back_buffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
        let resource: ID3D11Resource = back_buffer.cast()?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        self.device.CreateRenderTargetView(&resource, None, Some(&mut rtv))?;
        let rtv = rtv.unwrap();

        self.draw_scene(&rtv, flash_active, gray_code);

        let mut sync = 1u32;
        let mut present_flags = DXGI_PRESENT(0);
        if self.tearing && !vsync {
            sync = 0;
            present_flags = DXGI_PRESENT_ALLOW_TEARING;
        }
        self.swap_chain.Present(sync, present_flags).ok()?;
        Ok(())
    }
}

impl Renderer {
    /// Draw the scene into a given render target.
    unsafe fn draw_scene(&self, rtv: &ID3D11RenderTargetView, flash_active: bool, gray_code: u32) {
        self.context.PSSetShader(&self.ps_scene, None);
        let clear = [0.0f32, 0.0, 0.0, 1.0];
        self.context.ClearRenderTargetView(rtv, &clear);
        self.context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);

        let cb_data = FrameCb {
            flash_active: flash_active as u32,
            gray_code,
            screen_w: self.width,
            screen_h: self.height,
        };
        if let Ok(cb_res) = self.cb.cast::<ID3D11Resource>() {
            self.context.UpdateSubresource(&cb_res, 0, None, &cb_data as *const _ as *const core::ffi::c_void, 0, 0);
            let cbs: [Option<ID3D11Buffer>; 1] = [Some(self.cb.clone())];
            self.context.PSSetConstantBuffers(0, Some(&cbs));
        }
        self.context.Draw(3, 0);
    }

    /// Renders the scene into an offscreen texture and reads pixels back.
    /// Independent of the swapchain (a post-Present back buffer is discarded,
    /// so reading it back would prove nothing). Used by --selftest.
    unsafe fn selftest_readback(&self, width: u32, height: u32) -> windows::core::Result<String> {
        let color_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut offscreen: Option<ID3D11Texture2D> = None;
        self.device.CreateTexture2D(&color_desc, None, Some(&mut offscreen))?;
        let offscreen = offscreen.unwrap();
        let offscreen_res: ID3D11Resource = offscreen.cast()?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        self.device.CreateRenderTargetView(&offscreen_res, None, Some(&mut rtv))?;
        let rtv = rtv.unwrap();

        // Pass 1: unconditional white shader — tests geometry/rasterization
        // and the readback path with no constant-buffer involvement.
        self.context.PSSetShader(&self.ps_white, None);
        let clear = [0.0f32, 0.0, 0.0, 1.0];
        self.context.ClearRenderTargetView(&rtv, &clear);
        self.context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
        self.context.Draw(3, 0);

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        self.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        let dst: ID3D11Resource = staging.cast()?;

        let copy_and_map = |this: &Renderer| -> windows::core::Result<D3D11_MAPPED_SUBRESOURCE> {
            this.context.CopyResource(&dst, &offscreen_res);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            this.context.Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            Ok(m)
        };
        // Texture format is DXGI_FORMAT_R8G8B8A8_UNORM, so bytes are R,G,B,A.
        let sample_at = |m: &D3D11_MAPPED_SUBRESOURCE, fx: f32, fy: f32| -> (u8, u8, u8) {
            let x = ((width as f32 * fx) as usize).min(width as usize - 1);
            let y = ((height as f32 * fy) as usize).min(height as usize - 1);
            let row = (m.pData as *const u8).add(y * m.RowPitch as usize);
            let px = row.add(x * 4);
            (*px, *px.add(1), *px.add(2)) // (r, g, b)
        };

        // Sample pass 1 (white shader)
        let m1 = copy_and_map(self)?;
        let white_shader_center = sample_at(&m1, 0.5, 0.5);
        let row_pitch = m1.RowPitch;
        self.context.Unmap(&dst, 0);

        // Pass 1b: diagnostic shader with the constant buffer bound.
        self.context.PSSetShader(&self.ps_diag, None);
        self.context.ClearRenderTargetView(&rtv, &clear);
        self.context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
        let cb_data = FrameCb { flash_active: 1, gray_code: 0, screen_w: width as f32, screen_h: height as f32 };
        let cb_res: ID3D11Resource = self.cb.cast()?;
        self.context.UpdateSubresource(&cb_res, 0, None, &cb_data as *const _ as *const core::ffi::c_void, 0, 0);
        let cbs: [Option<ID3D11Buffer>; 1] = [Some(self.cb.clone())];
        self.context.PSSetConstantBuffers(0, Some(&cbs));
        self.context.Draw(3, 0);
        let m1b = copy_and_map(self)?;
        let diag_grid: Vec<String> = [(0.1f32, 0.1f32), (0.25, 0.25), (0.5, 0.5), (0.75, 0.75), (0.9, 0.9)]
            .iter()
            .map(|(fx, fy)| format!("({:.0},{:.0})={:?}", fx, fy, sample_at(&m1b, *fx, *fy)))
            .collect();
        self.context.Unmap(&dst, 0);

        // Pass 2: the real scene shader with flash lit and alternating cells
        self.draw_scene(&rtv, true, 0xAAAA);
        let m2 = copy_and_map(self)?;
        let flash = sample_at(&m2, 0.5, 0.5);   // inside flash rect -> white
        let dark = sample_at(&m2, 0.02, 0.5);   // left margin -> black
        let bar0 = sample_at(&m2, 0.12, 0.04);  // cell 0: bit0 of 0xAAAA = 0 -> black
        let bar1 = sample_at(&m2, 0.17, 0.04);  // cell 1: bit1 = 1 -> white
        let bar3 = sample_at(&m2, 0.27, 0.04);  // cell 3: bit3 = 1 -> white
        self.context.Unmap(&dst, 0);

        let verdict = |p: (u8, u8, u8), want_white: bool| {
            let is_white = p.0 > 200 && p.1 > 200 && p.2 > 200;
            if is_white == want_white { "ok" } else { "MISMATCH" }
        };

        Ok(format!(
            "offscreen {}x{}  rowPitch {}\n\
             [pass1 white-shader] center = {:?}  (want white)\n\
             [pass1b diag uv grid] R=uv.x G=uv.y B=cbflag\n    {}\n\
             [pass2 scene]  flash(0.50,0.50) = {:?}  want white  {}\n\
             [pass2 scene]  dark(0.02,0.50)  = {:?}  want black  {}\n\
             [pass2 scene]  barcode cell0    = {:?}  want black  {}\n\
             [pass2 scene]  barcode cell1    = {:?}  want white  {}\n\
             [pass2 scene]  barcode cell3    = {:?}  want white  {}\n",
            width, height, row_pitch,
            white_shader_center,
            diag_grid.join("\n    "),
            flash, verdict(flash, true),
            dark, verdict(dark, false),
            bar0, verdict(bar0, false),
            bar1, verdict(bar1, true),
            bar3, verdict(bar3, true),
        ))
    }
}

// ---------------------------------------------------------------------------
// Window + raw input setup

unsafe fn create_window(w: i32, h: i32, fullscreen: bool) -> windows::core::Result<HWND> {
    let hinstance: HMODULE = GetModuleHandleW(None)?;
    let class_name = PCWSTR::from_raw(windows::core::w!("LatencyAgentClass").as_ptr());

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(hinstance.0),
        hIcon: Default::default(),
        hCursor: Default::default(),
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: class_name,
    };
    RegisterClassW(&wc);

    let (style, x, y, cw, ch) = if fullscreen {
        (WS_POPUP | WS_VISIBLE | WS_SYSMENU, 0, 0, w, h)
    } else {
        // Windowed debug mode: 1280x720, centred.
        let cw = 1280.min(w - 80);
        let ch = 720.min(h - 120);
        (
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            (w - cw) / 2,
            (h - ch) / 2,
            cw,
            ch,
        )
    };

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        windows::core::w!("latency-agent"),
        style,
        x,
        y,
        cw,
        ch,
        None,
        None,
        HINSTANCE(hinstance.0),
        None,
    )?;
    if fullscreen {
        ShowCursor(false);
    }
    Ok(hwnd)
}

unsafe fn register_raw_input(hwnd: HWND) -> windows::core::Result<()> {
    let rid = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32)
}

// ---------------------------------------------------------------------------
// Main

fn open_logs() -> (Option<File>, Option<File>) {
    let mut clicks = OpenOptions::new().create(true).append(true).open("clicks.csv").ok();
    if let Some(f) = clicks.as_mut() {
        let _ = writeln!(f, "seq,qpc,ms_epoch");
    }
    let mut frames = OpenOptions::new().create(true).append(true).open("frames.csv").ok();
    if let Some(f) = frames.as_mut() {
        let _ = writeln!(f, "own_index,qpc_t_present,dxgi_present_count,sync_qpc_time,present_refresh_count");
    }
    (clicks, frames)
}

fn gray(v: u32) -> u32 {
    v ^ (v >> 1)
}

fn main() {
    if let Err(e) = unsafe { run() } {
        let msg = format!("latency-agent fatal error:\n{e}");
        let wide: Vec<u16> = msg.encode_utf16().chain([0]).collect();
        unsafe {
            let _ = MessageBoxW(
                None,
                windows::core::PCWSTR(wide.as_ptr()),
                windows::core::w!("latency-agent"),
                MB_ICONERROR,
            );
        }
    }
}

unsafe fn run() -> windows::core::Result<()> {
    let freq = qpc_freq();
    let tearing = std::env::args().any(|a| a == "--tearing");
    let fullscreen = std::env::args().any(|a| a == "--fullscreen");
    let vsync = !tearing;

    let (clicks, frames) = open_logs();
    *LOGGERS.lock().unwrap() = Some(Loggers { clicks, frames, freq });

    let _ = timeBeginPeriod(1);
    let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);

    let w = GetSystemMetrics(SM_CXSCREEN);
    let h = GetSystemMetrics(SM_CYSCREEN);
    let hwnd = create_window(w, h, fullscreen)?;
    register_raw_input(hwnd)?;

    // Match the swapchain to the actual client area. A mismatch between
    // swapchain size and client rect is a common source of nothing-showing.
    let mut rect = windows::Win32::Foundation::RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    let (cw, ch) = if fullscreen {
        (w as u32, h as u32)
    } else {
        ((rect.right - rect.left) as u32, (rect.bottom - rect.top) as u32)
    };
    println!("client area: {cw}x{ch}");

    let renderer = create_renderer(hwnd, cw, ch, tearing)?;

    if std::env::args().any(|a| a == "--selftest") {
        let report = renderer.selftest_readback(cw, ch)?;
        std::fs::write("selftest.txt", &report).ok();
        println!("{report}");
        let wide: Vec<u16> = format!("selftest written to selftest.txt\n\n{report}").encode_utf16().chain([0]).collect();
        let _ = MessageBoxW(None, PCWSTR(wide.as_ptr()), windows::core::w!("latency-agent selftest"), MB_ICONERROR);
        return Ok(());
    }

    // Flip-model waitable-object pacing (spec §3.2)
    let sc2: IDXGISwapChain2 = renderer.swap_chain.cast()?;
    sc2.SetMaximumFrameLatency(1)?;
    let waitable: HANDLE = sc2.GetFrameLatencyWaitableObject();
    unsafe { WaitForSingleObjectCompat(waitable, 1000) }; // consume initial release

    let _factory: IDXGIFactory1 = CreateDXGIFactory1()?;

    println!(
        "latency-agent: screen {}x{}, mode={}, tearing={tearing}, vsync={vsync}",
        w, h,
        if fullscreen { "fullscreen" } else { "windowed" }
    );
    println!("clicks.csv / frames.csv written to cwd. ESC to quit.");

    let mut present_index: u32 = 0;
    let mut flash_frames_left: u32 = 0;
    let mut msg = MSG::default();

    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                QUIT_REQUESTED.store(true, Ordering::Relaxed);
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if QUIT_REQUESTED.load(Ordering::Relaxed) {
            break;
        }

        if CLICK_PENDING.swap(false, Ordering::AcqRel) {
            flash_frames_left = FLASH_FRAMES;
        }

        // Wait for the waitable object released by the previous present.
        WaitForSingleObjectCompat(waitable, 1000);

        let flash_active = flash_frames_left > 0;
        let t_present = qpc(); // immediately before Present (spec §3.3)
        renderer.render_frame(flash_active, gray(present_index), vsync)?;

        let mut stats = DXGI_FRAME_STATISTICS::default();
        if sc2.GetFrameStatistics(&mut stats).is_ok() && stats.PresentCount > 0 {
            log_frame(present_index, t_present, &stats);
        }

        if flash_frames_left > 0 {
            flash_frames_left -= 1;
        }
        present_index = present_index.wrapping_add(1);

        // Live status in the title bar: proof that clicks and frames flow,
        // even when the flash rect is hard to see during bring-up.
        if present_index % 15 == 0 {
            let title = format!(
                "latency-agent [{}]  clicks={}  frame={}{}",
                if fullscreen { "fullscreen" } else { "windowed" },
                CLICK_COUNT.load(Ordering::Relaxed),
                present_index,
                if flash_active { "  FLASH" } else { "" },
            );
            let wide: Vec<u16> = title.encode_utf16().chain([0]).collect();
            let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
        }
    }

    if fullscreen {
        ShowCursor(true);
    }
    println!("done. {} clicks logged, {} frames logged.", CLICK_COUNT.load(Ordering::Relaxed), present_index);
    Ok(())
}

// Thin wrapper so the wait/signature lives in one place.
unsafe fn WaitForSingleObjectCompat(handle: HANDLE, timeout_ms: u32) -> u32 {
    use windows::Win32::System::Threading::WaitForSingleObject;
    WaitForSingleObject(handle, timeout_ms).0
}
