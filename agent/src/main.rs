// latency-agent — PC emitter for click-to-photon measurement.
// Spec: SPEC.md §3 (raw input, D3D11 flip swapchain, frame accounting).
//
// Renders a white flash (4 refreshes) on every left mouse click plus a
// continuously-updating Gray-code barcode strip that identifies the
// present index. Logs clicks and per-present frame statistics to CSV.
// ESC quits. `--tearing` switches to SyncInterval 0 + ALLOW_TEARING.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    ID3D11VertexShader, ID3D11PixelShader, ID3D11Resource,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_SINGLETHREADED, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11CreateDeviceAndSwapChain,
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
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetSystemMetrics, PeekMessageW,
    PostQuitMessage, RegisterClassW, ShowCursor, TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG,
    PM_REMOVE, RI_MOUSE_LEFT_BUTTON_DOWN, SM_CXSCREEN, SM_CYSCREEN, WINDOW_EX_STYLE, WM_DESTROY,
    WM_INPUT, WM_KEYDOWN, WM_QUIT, WNDCLASSW, WS_POPUP, WS_SYSMENU, WS_VISIBLE,
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
                    let last = LAST_CLICK_QPC.swap(now, Ordering::Relaxed);
                    let debounce = CLICK_DEBOUNCE_MS * qpc_freq() / 1000;
                    if last == 0 || now - last >= debounce {
                        let seq = CLICK_COUNT.fetch_add(1, Ordering::Relaxed);
                        CLICK_PENDING.store(true, Ordering::Release);
                        log_click(now, seq);
                    }
                }
            }
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

// ---------------------------------------------------------------------------
// Shaders

const VS_SRC: &[u8] = b"struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };\r\nVSOut vs_main(uint vid : SV_VertexID) {\r\n    VSOut o;\r\n    float x = float((vid << 1) & 2);\r\n    float y = float(vid & 2);\r\n    o.uv = float2(x, y);\r\n    o.pos = float4(x * 2 - 1, 1 - y * 2, 0, 1);\r\n    return o;\r\n}\r\n\0";

const PS_SRC: &[u8] = b"cbuffer FrameCb : register(b0) {\r\n    uint flashActive;\r\n    uint grayCode;\r\n    uint unused0;\r\n    uint unused1;\r\n};\r\n\r\nfloat4 ps_main(float2 uv : TEXCOORD0) : SV_Target {\r\n    if (uv.y >= 0.02 && uv.y <= 0.06 && uv.x >= 0.10 && uv.x <= 0.90) {\r\n        float fx = (uv.x - 0.10) / 0.80;\r\n        uint cell = (uint)(fx * 16.0);\r\n        if (cell > 15) cell = 15;\r\n        uint bit = (grayCode >> cell) & 1u;\r\n        return bit != 0u ? float4(1,1,1,1) : float4(0,0,0,1);\r\n    }\r\n    if (flashActive != 0u && uv.x >= 0.15 && uv.x <= 0.85 && uv.y >= 0.12 && uv.y <= 0.92) {\r\n        return float4(1,1,1,1);\r\n    }\r\n    return float4(0,0,0,1);\r\n}\r\n\0";

fn pc(b: &[u8]) -> PCSTR {
    PCSTR(b.as_ptr())
}

// ---------------------------------------------------------------------------
// Renderer

#[repr(C)]
struct FrameCb {
    flash_active: u32,
    gray_code: u32,
    _pad: [u32; 2],
}

struct Renderer {
    #[allow(dead_code)] // needed for §7 adjacency self-check later
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain,
    #[allow(dead_code)]
    rtv: ID3D11RenderTargetView,
    cb: ID3D11Buffer,
    #[allow(dead_code)]
    staging: Option<ID3D11Texture2D>,
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

    let tex: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
    let resource: ID3D11Resource = tex.cast()?;
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    device.CreateRenderTargetView(&resource, None, Some(&mut rtv))?;
    let rtv = rtv.unwrap();

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

    context.VSSetShader(&vs, None);
    context.PSSetShader(&ps, None);
    context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);

    // Staging texture for later self-checks (§7 adjacency test); unused for now.
    let tex_desc = D3D11_TEXTURE2D_DESC {
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
    let mut staging = None;
    device.CreateTexture2D(&tex_desc, None, Some(&mut staging))?;
    let staging = staging;

    Ok(Renderer { device, context, swap_chain, rtv, cb, staging, tearing: allow_tearing })
}

impl Renderer {
    unsafe fn render_frame(&self, flash_active: bool, gray_code: u32, vsync: bool) -> windows::core::Result<()> {
        let cb_data = FrameCb { flash_active: flash_active as u32, gray_code, _pad: [0; 2] };
        let cb_res: ID3D11Resource = self.cb.cast()?;
        self.context.UpdateSubresource(&cb_res, 0, None, &cb_data as *const _ as *const core::ffi::c_void, 0, 0);
        let cbs: [Option<ID3D11Buffer>; 1] = [Some(self.cb.clone())];
        self.context.PSSetConstantBuffers(0, Some(&cbs));
        self.context.Draw(3, 0);

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

// ---------------------------------------------------------------------------
// Window + raw input setup

unsafe fn create_window(w: i32, h: i32) -> windows::core::Result<HWND> {
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

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        windows::core::w!("latency-agent"),
        WS_POPUP | WS_VISIBLE | WS_SYSMENU,
        0,
        0,
        w,
        h,
        None,
        None,
        HINSTANCE(hinstance.0),
        None,
    )?;
    ShowCursor(false);
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
    unsafe { run() }.unwrap_or_else(|e| eprintln!("fatal: {e}"));
}

unsafe fn run() -> windows::core::Result<()> {
    let freq = qpc_freq();
    let tearing = std::env::args().any(|a| a == "--tearing");
    let vsync = !tearing;

    let (clicks, frames) = open_logs();
    *LOGGERS.lock().unwrap() = Some(Loggers { clicks, frames, freq });

    let _ = timeBeginPeriod(1);
    let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);

    let w = GetSystemMetrics(SM_CXSCREEN);
    let h = GetSystemMetrics(SM_CYSCREEN);
    let hwnd = create_window(w, h)?;
    register_raw_input(hwnd)?;

    let renderer = create_renderer(hwnd, w as u32, h as u32, tearing)?;

    // Flip-model waitable-object pacing (spec §3.2)
    let sc2: IDXGISwapChain2 = renderer.swap_chain.cast()?;
    sc2.SetMaximumFrameLatency(1)?;
    let waitable: HANDLE = sc2.GetFrameLatencyWaitableObject();
    unsafe { WaitForSingleObjectCompat(waitable, 1000) }; // consume initial release

    let _factory: IDXGIFactory1 = CreateDXGIFactory1()?;

    println!("latency-agent: {}x{}, tearing={tearing}, vsync={vsync}", w, h);
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
    }

    ShowCursor(true);
    println!("done. {} clicks logged, {} frames logged.", CLICK_COUNT.load(Ordering::Relaxed), present_index);
    Ok(())
}

// Thin wrapper so the wait/signature lives in one place.
unsafe fn WaitForSingleObjectCompat(handle: HANDLE, timeout_ms: u32) -> u32 {
    use windows::Win32::System::Threading::WaitForSingleObject;
    WaitForSingleObject(handle, timeout_ms).0
}
