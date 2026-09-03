//! Minimal D3D12 compute pipeline built for *sustained* load: a device, a queue, two
//! VRAM-resident buffers, one compute shader, and a small ring of command allocators.
//!
//! The design constraint that shapes everything here is that a GPU clocks down and
//! drops to near-idle power the moment its queue drains. An earlier version of this
//! module round-tripped the whole 64 MiB working set through the CPU on every single
//! dispatch — upload-heap memcpy, PCIe copy up, dispatch, PCIe copy back, a fresh
//! 64 MiB allocation, memcpy out, then a full checksum fold — with a blocking fence
//! wait around it. Measured against this machine's RX 6700 XT that left the GPU idle
//! the large majority of every cycle: the reported clock sawtoothed to 0 MHz and the
//! board pulled ~86 W against a ~230 W rating. It was a correct self-check and a
//! useless stress test.
//!
//! The fix is to separate the *data* path from the *work* path:
//!
//! * The seed pattern is uploaded to VRAM **once** (`upload_seed`). The shader reads
//!   it from a read-only `StructuredBuffer` and writes a separate UAV, so every
//!   dispatch is idempotent — dispatch N produces byte-identical output to dispatch 1
//!   without anything being re-sent across PCIe.
//! * Dispatches are submitted in *batches* ([`GpuComputeContext::submit_batch`]) with
//!   [`FRAMES_IN_FLIGHT`] batches queued ahead, so the CPU is always recording work
//!   the GPU has not reached yet and the queue never empties.
//! * Readback happens on its own slow cadence, appended to a batch the GPU is already
//!   going to run, and the checksum is folded straight out of the mapped readback
//!   buffer — no intermediate allocation, no copy.
//!
//! The correctness property is unchanged: a fixed deterministic input must always
//! produce the same output on healthy hardware, so a checksum mismatch is a real
//! fault. That still doubles as the VRAM integrity signal described in
//! `stress::gpu_kernel` — both buffers live on the default (VRAM-resident) heap and
//! every checked dispatch reads one and rewrites the other in full.

use windows::core::{s, Interface};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_FEATURE_LEVEL_11_0, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::win::{WinError, WinResult};

/// Elements per buffer. At 4 bytes/float this is 64 MiB each for the source and
/// destination buffers — large enough to exercise real VRAM cells and bandwidth, and
/// exactly 65 535 groups at 256 threads/group, which is the dispatch limit.
const ELEMENT_COUNT: u32 = 16_776_960;
const THREADS_PER_GROUP: u32 = 256;

/// Per-thread iterations of the inner FMA loop.
///
/// Tuned against the TDR watchdog rather than for raw throughput: Windows resets a GPU
/// whose work does not complete within ~2 s, so a single dispatch must stay
/// comfortably under that on the *slowest* GPU this might run on, not the fastest.
/// Sustained load comes from queueing many dispatches back to back (see
/// [`GpuComputeContext::submit_batch`]), not from making one dispatch enormous.
///
/// Measured on this machine's RX 6700 XT: ~50 ms per dispatch. That leaves roughly a
/// 40x margin, which is the point — this tool runs on strangers' laptops, and an
/// integrated GPU an order of magnitude slower must still finish a dispatch well
/// inside the watchdog rather than getting reset mid-test.
const SHADER_ITERS: u32 = 3_072;

/// Independent FMA chains per thread. A single dependent chain is limited by FMA
/// latency rather than throughput on any GPU that cannot cover the stall with
/// occupancy alone; four independent chains keep the vector ALUs fed regardless.
const FMA_CHAINS: u32 = 4;

// ---------------------------------------------------------------------------
// Rejected: interleaving extra VRAM reads into the FMA loop to raise board power.
//
// Worth recording because the idea is obviously right and turned out to be wrong here.
// A register-resident FMA loop leaves the memory subsystem idle, and VRAM plus the
// memory controller are a large slice of board power, so tapping memory inside the
// loop should have pushed this machine's RX 6700 XT from 139 W toward its ~230 W
// rating. Two attempts, both measured:
//
//   * Per-thread hashed offsets. Uncoalesced — each 32-lane wave issues 32 separate
//     transactions instead of one. 805M of those per dispatch overran the TDR watchdog
//     and Windows reset the GPU mid-test (DXGI_ERROR_DEVICE_HUNG), taking every
//     subsequent test in the process down with it.
//   * Coalesced offsets (`id.x` + a per-iteration stride), which is the textbook fix.
//     Dispatch time went 50ms -> 296ms, a 6x cost, and board power moved 139 W -> 142 W.
//     Three watts.
//
// The second result is the informative one: ~13 GB/s of added traffic bought ~2% more
// power, so reaching the TBP figure this way is not available at any acceptable cost,
// and the 296ms dispatch had also walked back into TDR range for slower GPUs.
//
// The reason the number falls short is not a defect in this kernel. A 230 W TBP is
// reached by mixed graphics work — raster, texture, RT units all lit at once — not by
// pure FP32 compute. Under this load the card sits at full boost (2562 MHz) and is
// neither power- nor thermally-limited, which is the honest reading of the hardware
// and exactly what the stress test should report. Chasing the sticker number by
// distorting the workload would make the test slower, more fragile, and less true.
// ---------------------------------------------------------------------------

/// Batches queued ahead of the GPU. Three is the standard depth: one executing, one
/// queued, one being recorded — enough that the CPU is never the reason the queue
/// runs dry, without queueing so far ahead that cancellation becomes sluggish.
const FRAMES_IN_FLIGHT: usize = 3;

// ---------------------------------------------------------------------------
// Graphics (raster) pass
//
// The compute dispatch above is pure FP32 ALU work: it never touches the rasterizer,
// the render-output/blend hardware (ROP), or a bound render target — the blocks a
// game workload actually spends a large share of its time on. "All aspects loaded"
// means those blocks too, not just the compute units, so every batch also draws one
// fullscreen triangle into an off-screen render target with a pixel shader that runs
// the same style of FMA chain per pixel. Self-checked the same way as the compute
// buffers: the shader's output depends only on each pixel's fixed screen-space
// position, so for a fixed viewport it is exactly reproducible draw to draw, and a
// periodic copy-to-readback + checksum catches a fault in the raster/shader/ROP path
// the same way `read_checksum` catches one in the compute path.
// ---------------------------------------------------------------------------

/// Off-screen render target dimensions. Chosen so the natural (unpadded) row pitch —
/// `RT_WIDTH * 4` bytes for RGBA8 — is already a multiple of D3D12's mandatory 256-byte
/// texture-copy row alignment, so the readback copy needs no per-row padding logic.
const RT_WIDTH: u32 = 3840;
const RT_HEIGHT: u32 = 2160;

const _: () = assert!(
    (RT_WIDTH * 4) % 256 == 0,
    "RT_WIDTH * 4 must stay a multiple of 256 (D3D12_TEXTURE_DATA_PITCH_ALIGNMENT) for the unpadded readback copy below to be correct"
);

/// Per-pixel FMA iterations. Reuses [`SHADER_ITERS`]'s tuning rather than a separate
/// constant: the render target has roughly half the compute dispatch's element count
/// (8.3M pixels vs. 16.8M threads), so at the same per-invocation iteration count a
/// draw takes roughly half as long as a dispatch — comfortably inside the same TDR
/// margin already established for the compute path.
const PS_ITERS: u32 = SHADER_ITERS;

/// A healthy dispatch finishes in tens of milliseconds. Ten seconds means the device
/// is hung or was reset out from under us; blocking forever (as this module used to)
/// would hang the whole stress run with no diagnosis.
const GPU_WAIT_TIMEOUT_MS: u32 = 10_000;

fn compute_shader_source() -> String {
    // Chains are generated from FMA_CHAINS rather than written out, so retuning the
    // constant actually changes the shader instead of silently disagreeing with it.
    //
    // Each chain converges toward the fixed point c/(1-k), but slowly: over
    // SHADER_ITERS the input still meaningfully determines the output, so lanes do not
    // all collapse to the same value and the self-check keeps its discriminating power.
    let decls: String = (0..FMA_CHAINS)
        .map(|i| format!("    float v{i} = s + {}.0 * 0.25;\n", i))
        .collect();
    let body: String = (0..FMA_CHAINS)
        .map(|i| format!("        v{i} = v{i} * 0.9999991 + 1.0000003;\n"))
        .collect();
    let sum: String = (0..FMA_CHAINS)
        .map(|i| format!("v{i}"))
        .collect::<Vec<_>>()
        .join(" + ");

    format!(
        r#"
StructuredBuffer<float> src : register(t0);
RWStructuredBuffer<float> dst : register(u0);

[numthreads({THREADS_PER_GROUP}, 1, 1)]
void CSMain(uint3 id : SV_DispatchThreadID) {{
    float s = src[id.x];
{decls}    [loop]
    for (uint i = 0; i < {SHADER_ITERS}; i++) {{
{body}    }}
    dst[id.x] = {sum};
}}
"#
    )
}

/// Vertex + pixel shader source for the graphics pass. The vertex shader is the
/// standard fullscreen-triangle trick — three vertices synthesized from `SV_VertexID`
/// with no vertex/index buffer at all, covering the whole viewport with one triangle
/// that overshoots it on two corners. The pixel shader's only input is its own
/// rasterized screen position, so output is a pure function of the (fixed) viewport —
/// deterministic and idempotent across draws, the same property `compute_shader_source`
/// relies on for its self-check.
fn graphics_shader_source() -> String {
    format!(
        r#"
struct VSOut {{
    float4 pos : SV_POSITION;
}};

VSOut VSMain(uint id : SV_VertexID) {{
    VSOut o;
    float2 uv = float2(float((id << 1) & 2), float(id & 2));
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}}

float4 PSMain(VSOut input) : SV_TARGET {{
    float s = input.pos.x * 0.013 + input.pos.y * 0.029;
    float v0 = s + 0.0;
    float v1 = s + 0.25;
    float v2 = s + 0.5;
    float v3 = s + 0.75;
    [loop]
    for (uint i = 0; i < {PS_ITERS}; i++) {{
        v0 = v0 * 0.9999991 + 1.0000003;
        v1 = v1 * 0.9999991 + 1.0000003;
        v2 = v2 * 0.9999991 + 1.0000003;
        v3 = v3 * 0.9999991 + 1.0000003;
    }}
    return float4(frac(v0), frac(v1), frac(v2), frac(v3));
}}
"#
    )
}

/// Re-enumerate DXGI adapters and return the one matching `vendor_id`/`device_id` —
/// the same adapter `probes::gpu::probe()` already identified, so the stress test
/// runs against whichever GPU the inventory scan showed the buyer.
pub fn select_adapter(vendor_id: u32, device_id: u32) -> WinResult<IDXGIAdapter1> {
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| WinError::from_win("CreateDXGIFactory1", &e))?;

        let mut index = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            index += 1;
            if let Ok(desc) = adapter.GetDesc1() {
                if desc.VendorId == vendor_id && desc.DeviceId == device_id {
                    return Ok(adapter);
                }
            }
        }
    }
    Err(WinError::new("no DXGI adapter matched the requested vendor/device id", 0))
}

/// One slot in the submission ring: its own allocator and list (an allocator cannot be
/// reset while the GPU is still reading commands from it) plus the fence value that
/// tells us when the GPU has finished with them.
struct Frame {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence_value: u64,
}

pub struct GpuComputeContext {
    /// Kept solely to interrogate `GetDeviceRemovedReason` — see [`Self::wait_for_value`].
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    frames: Vec<Frame>,
    next_frame: usize,
    fence: ID3D12Fence,
    fence_event: HANDLE,
    fence_value: u64,
    root_signature: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// Dropped once the seed reaches VRAM — it is 64 MiB of system memory with no
    /// further purpose, and this tool is measuring someone else's machine.
    buf_upload: Option<ID3D12Resource>,
    buf_src: ID3D12Resource,
    buf_dst: ID3D12Resource,
    buf_readback: ID3D12Resource,
    pub element_count: u32,

    // Graphics (raster) pass — see the module-level "Graphics (raster) pass" comment.
    root_signature_gfx: ID3D12RootSignature,
    pso_gfx: ID3D12PipelineState,
    render_target: ID3D12Resource,
    /// Kept alive so `rtv_handle` (a raw offset into it) stays valid; never touched
    /// again after `create()` computes that handle.
    #[allow(dead_code)]
    rtv_heap: ID3D12DescriptorHeap,
    rtv_handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    buf_readback_gfx: ID3D12Resource,
}

impl GpuComputeContext {
    pub fn create(adapter: &IDXGIAdapter1) -> WinResult<Self> {
        unsafe {
            let mut device_opt: Option<ID3D12Device> = None;
            D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, &mut device_opt)
                .map_err(|e| WinError::from_win("D3D12CreateDevice", &e))?;
            let device = device_opt.ok_or_else(|| WinError::new("D3D12CreateDevice returned no device", 0))?;

            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .map_err(|e| WinError::from_win("CreateCommandQueue", &e))?;

            let mut frames = Vec::with_capacity(FRAMES_IN_FLIGHT);
            for _ in 0..FRAMES_IN_FLIGHT {
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .map_err(|e| WinError::from_win("CreateCommandAllocator", &e))?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .map_err(|e| WinError::from_win("CreateCommandList", &e))?;
                list.Close().map_err(|e| WinError::from_win("CommandList.Close", &e))?;
                frames.push(Frame { allocator, list, fence_value: 0 });
            }

            let fence: ID3D12Fence = device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .map_err(|e| WinError::from_win("CreateFence", &e))?;
            let fence_event = CreateEventW(None, false, false, None)
                .map_err(|e| WinError::from_win("CreateEventW", &e))?;

            let root_signature = create_root_signature(&device)?;
            let pso = create_pipeline_state(&device, &root_signature)?;

            let buffer_size = (ELEMENT_COUNT as u64) * 4;
            let buf_upload = create_buffer(
                &device,
                buffer_size,
                D3D12_HEAP_TYPE_UPLOAD,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                false,
            )?;
            let buf_src = create_buffer(
                &device,
                buffer_size,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_STATE_COPY_DEST,
                false,
            )?;
            let buf_dst = create_buffer(
                &device,
                buffer_size,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                true,
            )?;
            let buf_readback = create_buffer(
                &device,
                buffer_size,
                D3D12_HEAP_TYPE_READBACK,
                D3D12_RESOURCE_STATE_COPY_DEST,
                false,
            )?;

            let root_signature_gfx = create_graphics_root_signature(&device)?;
            let pso_gfx = create_graphics_pipeline_state(&device, &root_signature_gfx)?;
            let render_target = create_render_target(&device)?;
            let rtv_heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
                .map_err(|e| WinError::from_win("CreateDescriptorHeap(RTV)", &e))?;
            let rtv_handle = rtv_heap.GetCPUDescriptorHandleForHeapStart();
            device.CreateRenderTargetView(&render_target, None, rtv_handle);

            let gfx_buffer_size = (RT_WIDTH as u64) * (RT_HEIGHT as u64) * 4;
            let buf_readback_gfx = create_buffer(
                &device,
                gfx_buffer_size,
                D3D12_HEAP_TYPE_READBACK,
                D3D12_RESOURCE_STATE_COPY_DEST,
                false,
            )?;

            Ok(Self {
                device,
                queue,
                frames,
                next_frame: 0,
                fence,
                fence_event,
                fence_value: 0,
                root_signature,
                pso,
                buf_upload: Some(buf_upload),
                buf_src,
                buf_dst,
                buf_readback,
                element_count: ELEMENT_COUNT,
                root_signature_gfx,
                pso_gfx,
                render_target,
                rtv_heap,
                rtv_handle,
                buf_readback_gfx,
            })
        }
    }

    /// Upload the deterministic seed to VRAM once, transition it to a shader-readable
    /// state, and release the staging buffer. Every later dispatch reads this same
    /// resident data, which is what removes the per-dispatch PCIe traffic.
    pub fn upload_seed(&mut self, pattern: &[f32]) -> WinResult<()> {
        debug_assert_eq!(pattern.len(), self.element_count as usize);
        let upload = self
            .buf_upload
            .clone()
            .ok_or_else(|| WinError::new("seed already uploaded", 0))?;

        unsafe {
            let mut mapped: *mut u8 = std::ptr::null_mut();
            upload
                .Map(0, None, Some(&mut mapped as *mut _ as *mut _))
                .map_err(|e| WinError::from_win("Map(upload)", &e))?;
            std::ptr::copy_nonoverlapping(pattern.as_ptr() as *const u8, mapped, pattern.len() * 4);
            upload.Unmap(0, None);

            let frame = &self.frames[0];
            frame
                .allocator
                .Reset()
                .map_err(|e| WinError::from_win("CommandAllocator.Reset", &e))?;
            frame
                .list
                .Reset(&frame.allocator, None)
                .map_err(|e| WinError::from_win("CommandList.Reset", &e))?;

            frame.list.CopyResource(&self.buf_src, &upload);
            transition(
                &frame.list,
                &self.buf_src,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            frame.list.Close().map_err(|e| WinError::from_win("CommandList.Close", &e))?;
            let lists = [Some(frame.list.cast::<ID3D12CommandList>().unwrap())];
            self.queue.ExecuteCommandLists(&lists);

            self.fence_value += 1;
            self.queue
                .Signal(&self.fence, self.fence_value)
                .map_err(|e| WinError::from_win("CommandQueue.Signal", &e))?;
            let seeded_at = self.fence_value;
            self.frames[0].fence_value = seeded_at;
            self.wait_for_value(seeded_at)?;
        }

        // Safe to release only now that the GPU has finished reading it.
        self.buf_upload = None;
        Ok(())
    }

    /// Record and submit `dispatches` back-to-back dispatches as a single command
    /// list, optionally appending a copy of the result into the readback buffer.
    ///
    /// Does not wait for the GPU: it blocks only when all [`FRAMES_IN_FLIGHT`] ring
    /// slots are still in use, which is the point — the CPU runs ahead and the queue
    /// stays fed.
    ///
    /// Returns the fence value this submission signals. When `capture_result` is set,
    /// pass that value to [`Self::is_complete`] and read the checksum once it reports
    /// true — polling rather than blocking is what keeps the self-check from draining
    /// the queue it took this whole rewrite to keep full.
    pub fn submit_batch(&mut self, dispatches: u32, capture_result: bool) -> WinResult<u64> {
        let slot = self.next_frame % FRAMES_IN_FLIGHT;
        self.wait_for_value(self.frames[slot].fence_value)?;

        unsafe {
            let frame = &self.frames[slot];
            frame
                .allocator
                .Reset()
                .map_err(|e| WinError::from_win("CommandAllocator.Reset", &e))?;
            frame
                .list
                .Reset(&frame.allocator, &self.pso)
                .map_err(|e| WinError::from_win("CommandList.Reset", &e))?;

            frame.list.SetComputeRootSignature(&self.root_signature);
            frame
                .list
                .SetComputeRootShaderResourceView(0, self.buf_src.GetGPUVirtualAddress());
            frame
                .list
                .SetComputeRootUnorderedAccessView(1, self.buf_dst.GetGPUVirtualAddress());

            let groups = self.element_count / THREADS_PER_GROUP;
            for _ in 0..dispatches.max(1) {
                frame.list.Dispatch(groups, 1, 1);
                // Every dispatch writes the same addresses with the same values, so
                // overlapping them would be benign in practice — but "benign in
                // practice" is not a property to rely on in a tool whose entire job is
                // deciding whether a GPU is faulty. The barrier costs microseconds
                // against a dispatch measured in tens of milliseconds.
                uav_barrier(&frame.list, &self.buf_dst);
            }

            if capture_result {
                transition(
                    &frame.list,
                    &self.buf_dst,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                );
                frame.list.CopyResource(&self.buf_readback, &self.buf_dst);
                transition(
                    &frame.list,
                    &self.buf_dst,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                );
            }

            // Graphics (raster) pass: exactly one draw every batch, regardless of the
            // compute dispatch count. See the module-level "Graphics (raster) pass"
            // comment for why — this loads the rasterizer/pixel-shader-ALU/ROP path
            // that the dispatches above never touch, sharing this same command list,
            // ring slot and fence so it costs no new bookkeeping machinery.
            frame.list.SetGraphicsRootSignature(&self.root_signature_gfx);
            frame.list.SetPipelineState(&self.pso_gfx);
            frame.list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: RT_WIDTH as f32,
                Height: RT_HEIGHT as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            frame.list.RSSetScissorRects(&[RECT { left: 0, top: 0, right: RT_WIDTH as i32, bottom: RT_HEIGHT as i32 }]);
            frame.list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            frame
                .list
                .OMSetRenderTargets(1, Some(&self.rtv_handle as *const _), false, None);
            frame.list.DrawInstanced(3, 1, 0, 0);

            if capture_result {
                transition(
                    &frame.list,
                    &self.render_target,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                );
                let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::ManuallyDrop::new(Some(self.buf_readback_gfx.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: 0,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                                Width: RT_WIDTH,
                                Height: RT_HEIGHT,
                                Depth: 1,
                                RowPitch: RT_WIDTH * 4,
                            },
                        },
                    },
                };
                let src_loc = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::ManuallyDrop::new(Some(self.render_target.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                frame.list.CopyTextureRegion(&dst_loc, 0, 0, 0, &src_loc, None);
                transition(
                    &frame.list,
                    &self.render_target,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                );
            }

            frame.list.Close().map_err(|e| WinError::from_win("CommandList.Close", &e))?;
            let lists = [Some(frame.list.cast::<ID3D12CommandList>().unwrap())];
            self.queue.ExecuteCommandLists(&lists);

            self.fence_value += 1;
            self.queue
                .Signal(&self.fence, self.fence_value)
                .map_err(|e| WinError::from_win("CommandQueue.Signal", &e))?;
        }

        self.frames[slot].fence_value = self.fence_value;
        self.next_frame = self.next_frame.wrapping_add(1);
        Ok(self.fence_value)
    }

    /// Block until everything submitted so far has completed.
    pub fn wait_for_submitted(&mut self) -> WinResult<()> {
        self.wait_for_value(self.fence_value)
    }

    /// Has the GPU passed `fence_value` yet? Non-blocking.
    pub fn is_complete(&self, fence_value: u64) -> bool {
        unsafe { self.fence.GetCompletedValue() >= fence_value }
    }

    /// Fold a checksum directly out of the mapped readback buffer.
    ///
    /// Deliberately reads through the mapping rather than copying into a `Vec` first:
    /// the old copy-then-fold path allocated and zeroed 64 MiB on every checkpoint,
    /// which was pure CPU time during which the GPU had nothing to do.
    pub fn read_checksum(&self) -> WinResult<u64> {
        let byte_len = self.element_count as usize * 4;
        unsafe {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let range = D3D12_RANGE { Begin: 0, End: byte_len };
            self.buf_readback
                .Map(0, Some(&range), Some(&mut ptr as *mut _ as *mut _))
                .map_err(|e| WinError::from_win("Map(readback)", &e))?;
            let data = std::slice::from_raw_parts(ptr as *const f32, self.element_count as usize);
            let sum = data
                .iter()
                .fold(0u64, |acc, x| acc ^ (x.to_bits() as u64).wrapping_mul(0x9E37_79B9));
            // Written range {0,0}: the CPU only read, so there is nothing to flush back.
            let written = D3D12_RANGE { Begin: 0, End: 0 };
            self.buf_readback.Unmap(0, Some(&written));
            Ok(sum)
        }
    }

    /// Fold a checksum out of the graphics pass's mapped readback buffer — the raster
    /// self-check's counterpart to [`Self::read_checksum`]. Raw RGBA8 bytes rather than
    /// `f32` bit patterns, but the same idea: any single corrupted byte anywhere in the
    /// render target changes the fold.
    pub fn read_graphics_checksum(&self) -> WinResult<u64> {
        let byte_len = (RT_WIDTH as usize) * (RT_HEIGHT as usize) * 4;
        unsafe {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let range = D3D12_RANGE { Begin: 0, End: byte_len };
            self.buf_readback_gfx
                .Map(0, Some(&range), Some(&mut ptr as *mut _ as *mut _))
                .map_err(|e| WinError::from_win("Map(readback_gfx)", &e))?;
            let data = std::slice::from_raw_parts(ptr, byte_len);
            let sum = data
                .iter()
                .fold(0u64, |acc, x| acc ^ (*x as u64).wrapping_mul(0x9E37_79B9));
            let written = D3D12_RANGE { Begin: 0, End: 0 };
            self.buf_readback_gfx.Unmap(0, Some(&written));
            Ok(sum)
        }
    }

    /// First `n` result values, for sanity checks that the shader actually ran.
    pub fn read_prefix(&self, n: usize) -> WinResult<Vec<f32>> {
        let n = n.min(self.element_count as usize);
        unsafe {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let range = D3D12_RANGE { Begin: 0, End: n * 4 };
            self.buf_readback
                .Map(0, Some(&range), Some(&mut ptr as *mut _ as *mut _))
                .map_err(|e| WinError::from_win("Map(readback)", &e))?;
            let out = std::slice::from_raw_parts(ptr as *const f32, n).to_vec();
            let written = D3D12_RANGE { Begin: 0, End: 0 };
            self.buf_readback.Unmap(0, Some(&written));
            Ok(out)
        }
    }

    fn wait_for_value(&self, value: u64) -> WinResult<()> {
        if value == 0 {
            return Ok(());
        }
        unsafe {
            if self.fence.GetCompletedValue() < value {
                self.fence
                    .SetEventOnCompletion(value, self.fence_event)
                    .map_err(|e| WinError::from_win("SetEventOnCompletion", &e))?;
                if WaitForSingleObject(self.fence_event, GPU_WAIT_TIMEOUT_MS) != WAIT_OBJECT_0 {
                    return Err(WinError::new(
                        "GPU did not signal completion within 10s — the device is hung",
                        0,
                    ));
                }
            }
            self.check_device_alive()
        }
    }

    /// A completed fence is *not* proof the work ran.
    ///
    /// When Windows' TDR watchdog resets a GPU, every fence on the dead device reports
    /// `UINT64_MAX`, so a wait returns instantly and everything downstream looks like a
    /// fast, successful run. This module's own TDR-budget test passed against a hung
    /// device exactly that way before this check existed — the most dangerous possible
    /// bug in a tool whose entire output is a verdict on someone's hardware, because it
    /// turns "this GPU crashed under load" into "this GPU is fine".
    unsafe fn check_device_alive(&self) -> WinResult<()> {
        self.device.GetDeviceRemovedReason().map_err(|e| {
            WinError::from_win(
                "GPU was reset or removed mid-run (TDR) — it did not survive the load",
                &e,
            )
        })
    }
}

impl Drop for GpuComputeContext {
    fn drop(&mut self) {
        // Mandatory, not tidiness: with work queued ahead, releasing the buffers and
        // command lists while the GPU is still reading them is a use-after-free. The
        // previous version could skip this only because it waited on every dispatch.
        let _ = self.wait_for_value(self.fence_value);
        unsafe {
            let _ = CloseHandle(self.fence_event);
        }
    }
}

unsafe fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let b = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: std::mem::ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
        ..Default::default()
    };
    list.ResourceBarrier(&[b]);
}

unsafe fn uav_barrier(list: &ID3D12GraphicsCommandList, resource: &ID3D12Resource) {
    let b = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(Some(resource.clone())),
            }),
        },
        ..Default::default()
    };
    list.ResourceBarrier(&[b]);
}

unsafe fn create_buffer(
    device: &ID3D12Device,
    size: u64,
    heap_type: D3D12_HEAP_TYPE,
    initial_state: D3D12_RESOURCE_STATES,
    allow_uav: bool,
) -> WinResult<ID3D12Resource> {
    let heap_props = D3D12_HEAP_PROPERTIES {
        Type: heap_type,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if allow_uav {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAG_NONE
        },
        ..Default::default()
    };

    let mut resource: Option<ID3D12Resource> = None;
    device
        .CreateCommittedResource(&heap_props, D3D12_HEAP_FLAG_NONE, &desc, initial_state, None, &mut resource)
        .map_err(|e| WinError::from_win("CreateCommittedResource", &e))?;
    resource.ok_or_else(|| WinError::new("CreateCommittedResource returned no resource", 0))
}

unsafe fn create_root_signature(device: &ID3D12Device) -> WinResult<ID3D12RootSignature> {
    // Two root descriptors — an SRV for the resident seed and a UAV for the result.
    // Root descriptors rather than a descriptor table for the same reason as before:
    // with exactly two buffers, a heap would be pure ceremony.
    let params = [
        D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Flags: D3D12_ROOT_DESCRIPTOR_FLAG_NONE,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Flags: D3D12_ROOT_DESCRIPTOR_FLAG_NONE,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
    ];

    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
                ..Default::default()
            },
        },
    };

    let mut blob: Option<ID3DBlob> = None;
    let mut error_blob: Option<ID3DBlob> = None;
    D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error_blob))
        .map_err(|e| WinError::from_win("D3D12SerializeVersionedRootSignature", &e))?;
    let blob = blob.ok_or_else(|| WinError::new("root signature serialization returned no blob", 0))?;

    let bytes = std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
    device
        .CreateRootSignature(0, bytes)
        .map_err(|e| WinError::from_win("CreateRootSignature", &e))
}

unsafe fn create_pipeline_state(device: &ID3D12Device, root_sig: &ID3D12RootSignature) -> WinResult<ID3D12PipelineState> {
    let source = compute_shader_source();
    let source_bytes = source.as_bytes();

    let mut shader_blob: Option<ID3DBlob> = None;
    let mut error_blob: Option<ID3DBlob> = None;
    let hr = D3DCompile(
        source_bytes.as_ptr() as *const _,
        source_bytes.len(),
        None,
        None,
        None,
        s!("CSMain"),
        s!("cs_5_0"),
        0,
        0,
        &mut shader_blob,
        Some(&mut error_blob),
    );
    if hr.is_err() {
        let msg = error_blob
            .map(|b| {
                let bytes = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                String::from_utf8_lossy(bytes).to_string()
            })
            .unwrap_or_else(|| "unknown shader compile error".into());
        return Err(WinError::new(Box::leak(format!("D3DCompile: {msg}").into_boxed_str()), 0));
    }
    let shader_blob = shader_blob.ok_or_else(|| WinError::new("D3DCompile returned no blob", 0))?;

    let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
        pRootSignature: std::mem::ManuallyDrop::new(Some(root_sig.clone())),
        CS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: shader_blob.GetBufferPointer(),
            BytecodeLength: shader_blob.GetBufferSize(),
        },
        ..Default::default()
    };

    device
        .CreateComputePipelineState(&desc)
        .map_err(|e| WinError::from_win("CreateComputePipelineState", &e))
}

/// Empty root signature for the graphics pass: the pixel shader's only input is its
/// own rasterized screen position (see [`graphics_shader_source`]), so there is
/// nothing to bind — zero root parameters is a legitimate, minimal root signature, not
/// a placeholder for something still to come.
unsafe fn create_graphics_root_signature(device: &ID3D12Device) -> WinResult<ID3D12RootSignature> {
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: 0,
                pParameters: std::ptr::null(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
                ..Default::default()
            },
        },
    };

    let mut blob: Option<ID3DBlob> = None;
    let mut error_blob: Option<ID3DBlob> = None;
    D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error_blob))
        .map_err(|e| WinError::from_win("D3D12SerializeVersionedRootSignature(graphics)", &e))?;
    let blob = blob.ok_or_else(|| WinError::new("graphics root signature serialization returned no blob", 0))?;

    let bytes = std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
    device
        .CreateRootSignature(0, bytes)
        .map_err(|e| WinError::from_win("CreateRootSignature(graphics)", &e))
}

unsafe fn compile_hlsl(source: &str, entry: windows::core::PCSTR, target: windows::core::PCSTR) -> WinResult<ID3DBlob> {
    let source_bytes = source.as_bytes();
    let mut blob: Option<ID3DBlob> = None;
    let mut error_blob: Option<ID3DBlob> = None;
    let hr = D3DCompile(
        source_bytes.as_ptr() as *const _,
        source_bytes.len(),
        None,
        None,
        None,
        entry,
        target,
        0,
        0,
        &mut blob,
        Some(&mut error_blob),
    );
    if hr.is_err() {
        let msg = error_blob
            .map(|b| {
                let bytes = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                String::from_utf8_lossy(bytes).to_string()
            })
            .unwrap_or_else(|| "unknown shader compile error".into());
        return Err(WinError::new(Box::leak(format!("D3DCompile: {msg}").into_boxed_str()), 0));
    }
    blob.ok_or_else(|| WinError::new("D3DCompile returned no blob", 0))
}

unsafe fn create_graphics_pipeline_state(
    device: &ID3D12Device,
    root_sig: &ID3D12RootSignature,
) -> WinResult<ID3D12PipelineState> {
    let source = graphics_shader_source();
    let vs_blob = compile_hlsl(&source, s!("VSMain"), s!("vs_5_0"))?;
    let ps_blob = compile_hlsl(&source, s!("PSMain"), s!("ps_5_0"))?;

    let mut rtv_formats = [DXGI_FORMAT_UNKNOWN; 8];
    rtv_formats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;

    let mut blend_desc = D3D12_BLEND_DESC::default();
    // RenderTargetWriteMask defaults to 0 (nothing written) even with blending
    // disabled — D3D12_COLOR_WRITE_ENABLE_ALL is R|G|B|A = 0x0F, and leaving this at
    // its zero default would make every draw a silent no-op: the render target would
    // never change, so a stale/cleared value would compare equal to itself forever and
    // the self-check would never catch anything.
    blend_desc.RenderTarget[0].RenderTargetWriteMask = 0x0F;

    let rasterizer_desc = D3D12_RASTERIZER_DESC {
        FillMode: D3D12_FILL_MODE_SOLID,
        // NONE, not the API default BACK: this pipeline never establishes a specific
        // winding order for the synthesized fullscreen triangle, and culling it away
        // by accident would silently turn every draw into a no-op — the same failure
        // shape as the write-mask footgun above.
        CullMode: D3D12_CULL_MODE_NONE,
        FrontCounterClockwise: BOOL(0),
        DepthClipEnable: BOOL(1),
        ..Default::default()
    };

    let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: std::mem::ManuallyDrop::new(Some(root_sig.clone())),
        VS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: vs_blob.GetBufferPointer(),
            BytecodeLength: vs_blob.GetBufferSize(),
        },
        PS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: ps_blob.GetBufferPointer(),
            BytecodeLength: ps_blob.GetBufferSize(),
        },
        BlendState: blend_desc,
        // Full mask: 0 here would mean "no samples pass," i.e. nothing rendered.
        SampleMask: u32::MAX,
        RasterizerState: rasterizer_desc,
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        RTVFormats: rtv_formats,
        DSVFormat: DXGI_FORMAT_UNKNOWN,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };

    device
        .CreateGraphicsPipelineState(&desc)
        .map_err(|e| WinError::from_win("CreateGraphicsPipelineState", &e))
}

unsafe fn create_render_target(device: &ID3D12Device) -> WinResult<ID3D12Resource> {
    let heap_props = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: RT_WIDTH as u64,
        Height: RT_HEIGHT,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        ..Default::default()
    };

    let mut resource: Option<ID3D12Resource> = None;
    device
        .CreateCommittedResource(
            &heap_props,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            None,
            &mut resource,
        )
        .map_err(|e| WinError::from_win("CreateCommittedResource(render target)", &e))?;
    resource.ok_or_else(|| WinError::new("CreateCommittedResource(render target) returned no resource", 0))
}

/// Serializes tests that put the GPU under sustained load.
///
/// There is one GPU and several tests across this crate that now saturate it for
/// seconds at a time. Run concurrently they contend for the same queue, which makes
/// timing assertions meaningless — a cancellation-latency test queued behind another
/// test's full-load run measures the other test, and a power test's "idle" baseline
/// picks up someone else's load. Everything unrelated still runs in parallel.
#[cfg(test)]
pub(crate) static GPU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`GPU_TEST_LOCK`], ignoring poisoning — a panicking test must not disable
/// every other GPU test in the run.
#[cfg(test)]
pub(crate) fn gpu_test_guard() -> std::sync::MutexGuard<'static, ()> {
    GPU_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn first_hardware_adapter() -> IDXGIAdapter1 {
        let gpus = crate::probes::gpu::probe().expect("DXGI adapter enumeration failed");
        let g = gpus
            .iter()
            .find(|g| !g.is_software)
            .expect("no hardware GPU on this machine — this test needs one");
        select_adapter(g.vendor_id, g.device_id).expect("failed to re-select the adapter for D3D12")
    }

    /// Returns the guard alongside the context so every caller holds the GPU lock for
    /// as long as it holds the context — forgetting it is what makes these flaky.
    fn seeded_context() -> (GpuComputeContext, std::sync::MutexGuard<'static, ()>) {
        let guard = gpu_test_guard();
        let adapter = first_hardware_adapter();
        let mut ctx = GpuComputeContext::create(&adapter).expect("failed to create D3D12 compute context");
        let pattern: Vec<f32> = (0..ctx.element_count).map(|i| 1.0 + (i % 97) as f32 * 0.01).collect();
        ctx.upload_seed(&pattern).expect("seed upload failed");
        (ctx, guard)
    }

    /// The correctness property the stress kernel depends on: the identical shader over
    /// the identical resident input must produce bit-identical output every time. Not a
    /// mock — a real D3D12 device, the real shader, this machine's real hardware.
    ///
    /// This also pins down the property that makes sustained load possible at all: the
    /// seed is uploaded once and never re-sent, so repeated dispatches must be
    /// idempotent rather than accumulating on their own previous output.
    #[test]
    fn repeated_dispatches_are_idempotent_on_this_machines_real_gpu() {
        let (mut ctx, _gpu) = seeded_context();

        ctx.submit_batch(1, true).expect("first batch failed");
        ctx.wait_for_submitted().expect("wait failed");
        let first = ctx.read_checksum().expect("checksum failed");
        let prefix = ctx.read_prefix(4).expect("prefix read failed");

        ctx.submit_batch(3, true).expect("second batch failed");
        ctx.wait_for_submitted().expect("wait failed");
        let second = ctx.read_checksum().expect("checksum failed");

        assert_eq!(
            first, second,
            "the same shader over the same resident input produced different output across batches"
        );
        assert!(prefix.iter().all(|v| v.is_finite()), "shader output contains non-finite values");
        // Sanity: the shader actually did something rather than passing input through.
        assert!(prefix[0] > 100.0, "output {} looks like untouched input", prefix[0]);
    }

    /// The whole point of the rewrite: submitting ahead must not block. If the CPU had
    /// to wait for the GPU on every batch the queue would drain between them, which is
    /// exactly what made the GPU idle down to 0 MHz and ~86 W before this change.
    #[test]
    fn submitting_within_the_ring_depth_does_not_block_on_the_gpu() {
        let (mut ctx, _gpu) = seeded_context();

        // Prime one batch so there is real work in flight to run ahead of.
        ctx.submit_batch(2, false).expect("priming batch failed");

        let start = Instant::now();
        for _ in 0..FRAMES_IN_FLIGHT - 1 {
            ctx.submit_batch(2, false).expect("batch submission failed");
        }
        let elapsed = start.elapsed();

        // Recording and submitting is CPU-side bookkeeping; a GPU batch is tens of
        // milliseconds. A generous bound still separates "ran ahead" from "waited".
        assert!(
            elapsed.as_millis() < 20,
            "submission blocked on the GPU ({elapsed:?}) — the queue will drain between batches"
        );
        ctx.wait_for_submitted().expect("wait failed");
    }

    /// Guards the TDR budget: a single dispatch must stay well under the ~2s watchdog.
    /// If a future tuning bump to SHADER_ITERS or FMA_CHAINS pushes past this, the GPU
    /// gets reset mid-run on slower hardware instead of being stress tested.
    #[test]
    fn a_single_dispatch_stays_well_inside_the_tdr_watchdog() {
        let (mut ctx, _gpu) = seeded_context();

        // Warm up: the first dispatch pays shader/pipeline warm-up costs.
        ctx.submit_batch(1, false).expect("warmup failed");
        ctx.wait_for_submitted().expect("wait failed");

        let start = Instant::now();
        ctx.submit_batch(1, false).expect("timed batch failed");
        ctx.wait_for_submitted().expect("wait failed");
        let per_dispatch = start.elapsed();

        // This machine measures ~50ms. The bound is deliberately far above that and
        // far below the 2s watchdog: it exists to catch a future retune of
        // SHADER_ITERS/FMA_CHAINS that would leave slower GPUs getting TDR-reset
        // mid-run, which would look like a hardware fault rather than a tuning bug.
        assert!(
            per_dispatch.as_millis() < 400,
            "one dispatch took {per_dispatch:?}; too close to the ~2s TDR watchdog for slower GPUs"
        );
        println!("per-dispatch: {per_dispatch:?} ({FMA_CHAINS} chains x {SHADER_ITERS} iters)");
    }
}

