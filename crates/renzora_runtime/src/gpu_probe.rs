//! Shared desktop capability probe, before the renderer fixes its device features.

use std::sync::OnceLock;

use bevy::prelude::info;
use renzora::RendererBackend;
#[cfg(any(test, feature = "solari"))]
use wgpu::Features;
use wgpu::{Backends, DeviceType};

pub(super) fn desktop_backend(preference: RendererBackend) -> Backends {
    match preference {
        RendererBackend::Auto => {
            if cfg!(any(target_os = "macos", target_os = "ios")) {
                Backends::METAL
            } else {
                Backends::VULKAN
            }
        }
        RendererBackend::Dx12 => Backends::DX12,
        RendererBackend::Vulkan => Backends::VULKAN,
        RendererBackend::Metal => Backends::METAL,
        RendererBackend::Gl => Backends::GL,
    }
}

pub(super) fn selected_backend() -> Backends {
    desktop_backend(renzora::load_renderer_backend())
}

pub(super) struct AdapterCapabilities {
    #[cfg(any(test, feature = "solari"))]
    pub features: Features,
    pub integrated: bool,
}

fn integrated_or_software(device_type: DeviceType) -> bool {
    matches!(device_type, DeviceType::IntegratedGpu | DeviceType::Cpu)
}

#[derive(Default)]
struct ProbeCache(OnceLock<Option<AdapterCapabilities>>);

impl ProbeCache {
    fn get_or_probe(
        &self,
        probe: impl FnOnce() -> Option<AdapterCapabilities>,
    ) -> Option<&AdapterCapabilities> {
        // Failure is cached too: querying another hint must not retry a costly
        // unavailable adapter. Renderer startup reports its own device failure.
        self.0.get_or_init(probe).as_ref()
    }
}

pub(super) fn capabilities() -> Option<&'static AdapterCapabilities> {
    static CACHE: ProbeCache = ProbeCache(OnceLock::new());
    CACHE.get_or_probe(|| {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: selected_backend(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            bevy::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            }));
        match adapter {
            Ok(adapter) => {
                let info = adapter.get_info();
                let integrated = integrated_or_software(info.device_type);
                if integrated {
                    info!(
                        "[runtime] integrated/software adapter detected ({}) — the editor \
                         will suggest the Low graphics tier",
                        info.name
                    );
                }
                Some(AdapterCapabilities {
                    #[cfg(any(test, feature = "solari"))]
                    features: adapter.features(),
                    integrated,
                })
            }
            Err(error) => {
                info!("[runtime] GPU capability probe found no adapter ({error})");
                None
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn both_hints_share_one_probe_and_failure_is_not_retried() {
        for available in [false, true] {
            let cache = ProbeCache::default();
            let calls = Cell::new(0);
            let probe = || {
                calls.set(calls.get() + 1);
                available.then_some(AdapterCapabilities {
                    features: Features::POLYGON_MODE_LINE,
                    integrated: true,
                })
            };
            for _ in 0..1000 {
                assert_eq!(
                    cache.get_or_probe(probe).is_some_and(|c| c.integrated),
                    available
                );
                assert_eq!(
                    cache
                        .get_or_probe(probe)
                        .is_some_and(|c| c.features.contains(Features::POLYGON_MODE_LINE)),
                    available
                );
            }
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn backend_and_device_classification_preserve_existing_policy() {
        for (preference, expected) in [
            (RendererBackend::Dx12, Backends::DX12),
            (RendererBackend::Vulkan, Backends::VULKAN),
            (RendererBackend::Metal, Backends::METAL),
            (RendererBackend::Gl, Backends::GL),
        ] {
            assert_eq!(desktop_backend(preference), expected);
        }
        assert_eq!(
            desktop_backend(RendererBackend::Auto),
            if cfg!(any(target_os = "macos", target_os = "ios")) {
                Backends::METAL
            } else {
                Backends::VULKAN
            }
        );
        assert!(integrated_or_software(DeviceType::IntegratedGpu));
        assert!(integrated_or_software(DeviceType::Cpu));
        assert!(!integrated_or_software(DeviceType::DiscreteGpu));
        assert!(!integrated_or_software(DeviceType::VirtualGpu));
        assert!(!integrated_or_software(DeviceType::Other));
    }
}
