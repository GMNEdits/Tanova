//! Tanova desktop application shell.

use std::error::Error;
use std::sync::Arc;

/// GPU handles shared by the UI thread and the background render thread.
///
/// The `Arc` wrapper is deliberate: wgpu's `Device` and `Queue` are cheap to
/// clone internally, but a single shared instance keeps one GPU context for
/// the whole application.
#[derive(Clone)]
pub struct GpuContext {
    /// The single logical device every renderer targets.
    pub device: Arc<wgpu::Device>,
    /// The submission queue paired with `device`.
    pub queue: Arc<wgpu::Queue>,
}

/// Requests a high-performance adapter and opens a device on it.
pub async fn init_gpu() -> Result<GpuContext, Box<dyn Error>> {
    let instance = wgpu::Instance::default();

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        })
        .await?;

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("tanova-device"),
            ..Default::default()
        })
        .await?;

    let info = adapter.get_info();
    println!("GPU: {} ({:?})", info.name, info.backend);

    Ok(GpuContext {
        device: Arc::new(device),
        queue: Arc::new(queue),
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let gpu = pollster::block_on(init_gpu())?;
    println!(
        "wgpu device ready; holding {} shared GPU handle(s)",
        Arc::strong_count(&gpu.device)
    );
    Ok(())
}
