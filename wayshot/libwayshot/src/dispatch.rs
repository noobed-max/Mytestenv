use std::{
    collections::HashSet,
    os::fd::{AsFd, BorrowedFd},
    sync::atomic::{AtomicBool, Ordering},
    fs::File,
};
use wayland_client::Proxy;

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    WEnum::{self, Value},
    delegate_noop,
    globals::GlobalListContents,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::Format,
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
            zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{
        zwlr_layer_shell_v1::ZwlrLayerShellV1,
        zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
    },
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
};

use crate::{
    output::OutputInfo,
    region::{LogicalRegion, Position, Size},
    screencopy::{DMAFrameFormat, FrameFormat, FrameGuard},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};
#[derive(Debug)]
pub struct OutputCaptureState {
    pub outputs: Vec<OutputInfo>,
}

impl Dispatch<WlRegistry, ()> for OutputCaptureState {
    #[tracing::instrument(skip(wl_registry, qh), ret, level = "trace")]
    fn event(
        state: &mut Self,
        wl_registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        /* > The name event is sent after binding the output object. This event
         * is only sent once per output object, and the name does not change
         * over the lifetime of the wl_output global. */

        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wl_output" {
                if version >= 4 {
                    let output = wl_registry.bind::<wl_output::WlOutput, _, _>(name, 4, qh, ());
                    state.outputs.push(OutputInfo {
                        wl_output: output,
                        name: "".to_string(),
                        description: String::new(),
                        transform: wl_output::Transform::Normal,
                        physical_size: Size::default(),
                        logical_region: LogicalRegion::default(),
                    });
                } else {
                    tracing::error!("Ignoring a wl_output with version < 4.");
                }
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for OutputCaptureState {
    #[tracing::instrument(skip(wl_output), ret, level = "trace")]
    fn event(
        state: &mut Self,
        wl_output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output: &mut OutputInfo =
            match state.outputs.iter_mut().find(|x| x.wl_output == *wl_output) {
                Some(output) => output,
                _ => {
                    tracing::error!(
                        "Received event for an output that is not registered: {event:#?}"
                    );
                    return;
                }
            };

        match event {
            wl_output::Event::Name { name } => {
                output.name = name;
            }
            wl_output::Event::Description { description } => {
                output.description = description;
            }
            wl_output::Event::Mode { width, height, .. } => {
                output.physical_size = Size {
                    width: width as u32,
                    height: height as u32,
                };
            }
            wl_output::Event::Geometry {
                transform: WEnum::Value(transform),
                ..
            } => {
                output.transform = transform;
            }
            wl_output::Event::Scale { .. } => {}
            wl_output::Event::Done => {}
            _ => {}
        }
    }
}

delegate_noop!(OutputCaptureState: ignore ZxdgOutputManagerV1);

impl Dispatch<ZxdgOutputV1, usize> for OutputCaptureState {
    #[tracing::instrument(ret, level = "trace")]
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output_info = match state.outputs.get_mut(*index) {
            Some(output_info) => output_info,
            _ => {
                tracing::error!(
                    "Received event for output index {index} that is not registered: {event:#?}"
                );
                return;
            }
        };

        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output_info.logical_region.inner.position = Position { x, y };
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output_info.logical_region.inner.size = Size {
                    width: width as u32,
                    height: height as u32,
                };
            }
            zxdg_output_v1::Event::Done => {}
            zxdg_output_v1::Event::Name { .. } => {}
            zxdg_output_v1::Event::Description { .. } => {}
            _ => {}
        };
    }
}

/// State of the frame after attempting to copy it's data to a wl_buffer.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum FrameState {
    /// Compositor returned a failed event on calling `frame.copy`.
    Failed,
    /// Compositor sent a Ready event on calling `frame.copy`.
    Finished,
}

pub struct CaptureFrameState {
    pub formats: Vec<FrameFormat>,
    pub dmabuf_formats: Vec<DMAFrameFormat>,
    pub state: Option<FrameState>,
    pub buffer_done: AtomicBool,
}
impl Dispatch<ZwpLinuxDmabufV1, ()> for CaptureFrameState {
    fn event(
        _frame: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        _event: zwp_linux_dmabuf_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for CaptureFrameState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxBufferParamsV1,
        _event: zwp_linux_buffer_params_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}


impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for CaptureFrameState{
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for CaptureFrameState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtOutputImageCaptureSourceManagerV1,
        _event: <ExtOutputImageCaptureSourceManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ExtImageCaptureSourceV1, ()> for CaptureFrameState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCaptureSourceV1,
        _event: <ExtImageCaptureSourceV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for CaptureFrameState {
    fn event(
        state: &mut Self,
        proxy: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as Proxy>::Event,
        _: &(),
        _: &wayland_client::Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::Event as ExtImageCopyCaptureSessionV1Event;
        let mut pending_width = None;
        let mut pending_height = None;
        let mut pending_format = None;

        match event {
            ExtImageCopyCaptureSessionV1Event::BufferSize { width, height } => {
                pending_width = Some(width);
                pending_height = Some(height);
            //     state.formats.push(FrameFormat {
            //         format: wl_shm::Format::Argb8888, // Placeholder format, will be updated in shm_format event
            //         size: Size { width, height },
            //         stride: 0, // Placeholder stride, will be calculated in Done event
            //     });
            }
            ExtImageCopyCaptureSessionV1Event::ShmFormat { format } => {
                if let Value(f) = format {
                    pending_format = Some(f);
                } else {
                    tracing::debug!("Received ShmFormat event with unidentified format");
                }
                // if let Value(f) = format {
                // state.formats.push(FrameFormat {
                //     format: f, // Placeholder format, will be updated in shm_format event
                    
                //     stride: 0,
                // }) } else {
                //     tracing::debug!("Received Buffer event with unidentified format");
                // }
            }
            ExtImageCopyCaptureSessionV1Event::Done => {
                if let (Some(width), Some(height), Some(format)) = (pending_width, pending_height, pending_format) {
                    state.formats.push(FrameFormat {
                        format,
                        size: Size { width, height },
                        stride:0 ,
                    });
                } else {
                    tracing::debug!("Missing parameters for frame format creation");
                }
                
                // Reset temporary variables
                pending_width = None;
                pending_height = None;
                pending_format = None;
               
                  state.state.replace(FrameState::Finished);
                          state.buffer_done.store(true, Ordering::SeqCst);
             
                }
                
            ExtImageCopyCaptureSessionV1Event::Stopped => {
                println!("Capture session stopped");
                state.state.replace(FrameState::Failed);
                state.buffer_done.store(true, Ordering::SeqCst);
            }
            
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for CaptureFrameState {
    fn event(
        state: &mut Self,
        proxy: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::Event as ExtImageCopyCaptureFrameV1Event;
        match event {
            ExtImageCopyCaptureFrameV1Event::Ready => {
                println!("Frame captured successfully");
                proxy.destroy();
                // planning to map fd here for buffer data and initialize it to CaptureFrameState or other option
            }
            
            ExtImageCopyCaptureFrameV1Event::Failed { reason }=> {
                println!("Frame capture failed");
                proxy.destroy();
            }
            _ => {}
        }
    }
}
delegate_noop!(CaptureFrameState: ignore WlShm);
delegate_noop!(CaptureFrameState: ignore WlShmPool);
delegate_noop!(CaptureFrameState: ignore WlBuffer);
// delegate_noop!(CaptureFrameState: ignore ExtImageCopyCaptureManagerV1);

// TODO: Create a xdg-shell surface, check for the enter event, grab the output from it.

pub struct WayshotState {}
delegate_noop!(WayshotState: ignore ZwpLinuxDmabufV1);
impl wayland_client::Dispatch<wl_registry::WlRegistry, GlobalListContents> for WayshotState {
    fn event(
        _: &mut WayshotState,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<WayshotState>,
    ) {
    }
}

pub struct LayerShellState {
    pub configured_outputs: HashSet<WlOutput>,
}

delegate_noop!(LayerShellState: ignore WlCompositor);
delegate_noop!(LayerShellState: ignore WlShm);
delegate_noop!(LayerShellState: ignore WlShmPool);
delegate_noop!(LayerShellState: ignore WlBuffer);
delegate_noop!(LayerShellState: ignore ZwlrLayerShellV1);
delegate_noop!(LayerShellState: ignore WlSurface);
delegate_noop!(LayerShellState: ignore WpViewport);
delegate_noop!(LayerShellState: ignore WpViewporter);

impl wayland_client::Dispatch<ZwlrLayerSurfaceV1, WlOutput> for LayerShellState {
    // No need to instrument here, span from lib.rs is automatically used.
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        data: &WlOutput,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width: _,
                height: _,
            } => {
                tracing::debug!("Acking configure");
                state.configured_outputs.insert(data.clone());
                proxy.ack_configure(serial);
                tracing::trace!("Acked configure");
            }
            zwlr_layer_surface_v1::Event::Closed => {
                tracing::debug!("Closed")
            }
            _ => {}
        }
    }
}
pub(crate) struct Card(std::fs::File);

/// Implementing [`AsFd`] is a prerequisite to implementing the traits found
/// in this crate. Here, we are just calling [`File::as_fd()`] on the inner
/// [`File`].
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
/// Simple helper methods for opening a `Card`.
impl Card {
    pub fn open(path: &str) -> Self {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        options.write(true);
        Card(options.open(path).unwrap())
    }
}
#[derive(Debug)]
pub(crate) struct DMABUFState {
    pub linux_dmabuf: ZwpLinuxDmabufV1,
    pub gbmdev: gbm::Device<Card>,
}
