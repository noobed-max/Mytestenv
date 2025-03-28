//! `libwayshot` is a convenient wrapper over the wlroots screenshot protocol
//! that provides a simple API to take screenshots with.
//!
//! To get started, look at [`WayshotConnection`].

mod convert;
mod dispatch;
mod error;
mod image_util;
pub mod output;
pub mod region;
mod screencopy;

use std::{
    collections::HashSet,
    ffi::c_void,
    fs::File,
    os::fd::{AsFd, IntoRawFd, OwnedFd},
    sync::atomic::{AtomicBool, Ordering},
    thread,
};
use wayland_client::Proxy;

use dispatch::{DMABUFState, LayerShellState};
use image::{DynamicImage, imageops::replace};
use khronos_egl::{self as egl, Instance};
use memmap2::MmapMut;
use region::{EmbeddedRegion, RegionCapturer};
use screencopy::{DMAFrameFormat, DMAFrameGuard, EGLImageGuard, FrameData, FrameGuard};
use tracing::debug;
use wayland_client::{
    Connection, EventQueue,
    globals::{GlobalList, registry_queue_init},
    protocol::{
        wl_compositor::WlCompositor,
        wl_output::{Transform, WlOutput},
        wl_shm::{self, WlShm},
    },
};
use wayland_client::protocol::wl_shm::Format;
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
        viewporter::client::wp_viewporter::WpViewporter,
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::ZxdgOutputV1,
    },
};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{
        zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
        zwlr_layer_surface_v1::Anchor,
    },
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
};

use crate::{
    convert::create_converter,
    dispatch::{CaptureFrameState, FrameState, OutputCaptureState, WayshotState},
    output::OutputInfo,
    region::{LogicalRegion, Size},
    screencopy::{FrameCopy, FrameFormat, create_shm_fd},
};

pub use crate::error::{Error, Result};

pub mod reexport {
    use wayland_client::protocol::wl_output;
    pub use wl_output::{Transform, WlOutput};
}
use gbm::{BufferObject, BufferObjectFlags, Device as GBMDevice};
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
/// Struct to store wayland connection and globals list.
/// # Example usage
///
/// ```ignore
/// use libwayshot::WayshotConnection;
/// let wayshot_connection = WayshotConnection::new()?;
/// let image_buffer = wayshot_connection.screenshot_all()?;
/// ```
#[derive(Debug)]
pub struct WayshotConnection {
    pub conn: Connection,
    pub globals: GlobalList,
    output_infos: Vec<OutputInfo>,
    dmabuf_state: Option<DMABUFState>,
}

impl WayshotConnection {
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env()?;

        Self::from_connection(conn)
    }

    /// Recommended if you already have a [`wayland_client::Connection`].
    pub fn from_connection(conn: Connection) -> Result<Self> {
        let (globals, _) = registry_queue_init::<WayshotState>(&conn)?;

        let mut initial_state = Self {
            conn,
            globals,
            output_infos: Vec::new(),
            dmabuf_state: None,
        };

        initial_state.refresh_outputs()?;

        Ok(initial_state)
    }

    /// Fetch all accessible wayland outputs.
    pub fn get_all_outputs(&self) -> &[OutputInfo] {
        self.output_infos.as_slice()
    }

    /// refresh the outputs, to get new outputs
    pub fn refresh_outputs(&mut self) -> Result<()> {
        // Connecting to wayland environment.
        let mut state = OutputCaptureState {
            outputs: Vec::new(),
        };
        let mut event_queue = self.conn.new_event_queue::<OutputCaptureState>();
        let qh = event_queue.handle();

        // Bind to xdg_output global.
        let zxdg_output_manager = match self.globals.bind::<ZxdgOutputManagerV1, _, _>(
            &qh,
            3..=3,
            (),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    "Failed to create ZxdgOutputManagerV1 version 3. Does your compositor implement ZxdgOutputManagerV1?"
                );
                panic!("{:#?}", e);
            }
        };

        // Fetch all outputs; when their names arrive, add them to the list
        let _ = self.conn.display().get_registry(&qh, ());
        event_queue.roundtrip(&mut state)?;

        // We loop over each output and request its position data.
        let xdg_outputs: Vec<ZxdgOutputV1> = state
            .outputs
            .iter()
            .enumerate()
            .map(|(index, output)| {
                zxdg_output_manager.get_xdg_output(&output.wl_output, &qh, index)
            })
            .collect();

        event_queue.roundtrip(&mut state)?;

        for xdg_output in xdg_outputs {
            xdg_output.destroy();
        }

        if state.outputs.is_empty() {
            tracing::error!("Compositor did not advertise any wl_output devices!");
            return Err(Error::NoOutputs);
        }
        tracing::trace!("Outputs detected: {:#?}", state.outputs);
        self.output_infos = state.outputs;

        Ok(())
    }
    pub fn screenshot_all(&self, cursor_overlay: bool) -> Result<DynamicImage> {
        self.screenshot_outputs(self.get_all_outputs(), cursor_overlay)
    }
    pub fn screenshot_outputs(
        &self,
        outputs: &[OutputInfo],
        cursor_overlay: bool,
    ) -> Result<DynamicImage> {
        if outputs.is_empty() {
            return Err(Error::NoOutputs);
        }

        self.screenshot_region_capturer(RegionCapturer::Outputs(outputs.to_owned()), cursor_overlay)
    }
    #[tracing::instrument(skip_all, fields(max_scale = tracing::field::Empty))]
    fn screenshot_region_capturer(
        &self,
        region_capturer: RegionCapturer,
        cursor_overlay: bool,
    ) -> Result<DynamicImage> {
        let outputs_capture_regions: Vec<(OutputInfo, Option<EmbeddedRegion>)> =
            match region_capturer {
                RegionCapturer::Outputs(ref outputs) => outputs
                    .iter()
                    .map(|output_info| (output_info.clone(), None))
                    .collect(),
                RegionCapturer::Region(capture_region) => self
                    .get_all_outputs()
                    .iter()
                    .filter_map(|output_info| {
                        tracing::span!(
                            tracing::Level::DEBUG,
                            "filter_map",
                            output = format!(
                                "{output_info} at {region}",
                                output_info = format!("{output_info}"),
                                region = LogicalRegion::from(output_info),
                            ),
                            capture_region = format!("{}", capture_region),
                        )
                        .in_scope(|| {
                            if let Some(relative_region) =
                                EmbeddedRegion::new(capture_region, output_info.into())
                            {
                                tracing::debug!("Intersection found: {}", relative_region);
                                Some((output_info.clone(), Some(relative_region)))
                            } else {
                                tracing::debug!("No intersection found");
                                None
                            }
                        })
                    })
                    .collect(),
                RegionCapturer::Freeze(_) => self
                    .get_all_outputs()
                    .iter()
                    .map(|output_info| (output_info.clone(), None))
                    .collect(),
            };

        let frames = self.capture_frame_copies(&outputs_capture_regions, cursor_overlay)?;

        let capture_region: LogicalRegion = match region_capturer {
            RegionCapturer::Outputs(outputs) => outputs.as_slice().try_into()?,
            RegionCapturer::Region(region) => region,
            RegionCapturer::Freeze(callback) => {
                self.overlay_frames_and_select_region(&frames, callback)?
            }
        };

        // TODO When freeze was used, we can still further remove the outputs
        // that don't intersect with the capture region.

        thread::scope(|scope| {
            let max_scale = outputs_capture_regions
                .iter()
                .map(|(output_info, _)| output_info.scale())
                .fold(1.0, f64::max);

            tracing::Span::current().record("max_scale", max_scale);

            let rotate_join_handles = frames
                .into_iter()
                .map(|(frame_copy, _, _)| {
                    scope.spawn(move || {
                        let image = (&frame_copy).try_into()?;
                        Ok((
                            image_util::rotate_image_buffer(
                                image,
                                frame_copy.transform,
                                frame_copy.logical_region.inner.size,
                                max_scale,
                            ),
                            frame_copy,
                        ))
                    })
                })
                .collect::<Vec<_>>();

            rotate_join_handles
                .into_iter()
                .flat_map(|join_handle| join_handle.join())
                .fold(
                    None,
                    |composite_image: Option<Result<_>>, image: Result<_>| {
                        // Default to a transparent image.
                        let composite_image = composite_image.unwrap_or_else(|| {
                            Ok(DynamicImage::new_rgba8(
                                (capture_region.inner.size.width as f64 * max_scale) as u32,
                                (capture_region.inner.size.height as f64 * max_scale) as u32,
                            ))
                        });

                        Some(|| -> Result<_> {
                            let mut composite_image = composite_image?;
                            let (image, frame_copy) = image?;
                            let (x, y) = (
                                ((frame_copy.logical_region.inner.position.x as f64
                                    - capture_region.inner.position.x as f64)
                                    * max_scale) as i64,
                                ((frame_copy.logical_region.inner.position.y as f64
                                    - capture_region.inner.position.y as f64)
                                    * max_scale) as i64,
                            );
                            tracing::span!(
                                tracing::Level::DEBUG,
                                "replace",
                                frame_copy_region = format!("{}", frame_copy.logical_region),
                                capture_region = format!("{}", capture_region),
                                x = x,
                                y = y,
                            )
                            .in_scope(|| {
                                tracing::debug!("Replacing parts of the final image");
                                replace(&mut composite_image, &image, x, y);
                            });

                            Ok(composite_image)
                        }())
                    },
                )
                .ok_or_else(|| {
                    tracing::error!("Provided capture region doesn't intersect with any outputs!");
                    Error::NoOutputs
                })?
        })
    }
    fn overlay_frames_and_select_region(
        &self,
        frames: &[(FrameCopy, FrameGuard, OutputInfo)],
        callback: Box<dyn Fn() -> Result<LogicalRegion, Error>>,
    ) -> Result<LogicalRegion> {
        let mut state = LayerShellState {
            configured_outputs: HashSet::new(),
        };
        let mut event_queue: EventQueue<LayerShellState> =
            self.conn.new_event_queue::<LayerShellState>();
        let qh = event_queue.handle();

        let compositor = match self.globals.bind::<WlCompositor, _, _>(&qh, 3..=3, ()) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    "Failed to create compositor. Does your compositor implement WlCompositor?"
                );
                tracing::error!("err: {e}");
                return Err(Error::ProtocolNotFound(
                    "WlCompositor not found".to_string(),
                ));
            }
        };
        let layer_shell = match self.globals.bind::<ZwlrLayerShellV1, _, _>(&qh, 1..=1, ()) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    "Failed to create layer shell. Does your compositor implement WlrLayerShellV1?"
                );
                tracing::error!("err: {e}");
                return Err(Error::ProtocolNotFound(
                    "WlrLayerShellV1 not found".to_string(),
                ));
            }
        };
        let viewporter = self.globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok();
        if viewporter.is_none() {
            tracing::info!(
                "Compositor does not support wp_viewporter, display scaling may be inaccurate."
            );
        }

        let mut layer_shell_surfaces = Vec::with_capacity(frames.len());

        for (frame_copy, frame_guard, output_info) in frames {
            tracing::span!(
                tracing::Level::DEBUG,
                "overlay_frames::surface",
                output = format!("{output_info}")
            )
            .in_scope(|| -> Result<()> {
                let surface = compositor.create_surface(&qh, ());

                let layer_surface = layer_shell.get_layer_surface(
                    &surface,
                    Some(&output_info.wl_output),
                    Layer::Top,
                    "wayshot".to_string(),
                    &qh,
                    output_info.wl_output.clone(),
                );

                layer_surface.set_exclusive_zone(-1);
                layer_surface.set_anchor(Anchor::Top | Anchor::Left);
                layer_surface.set_size(
                    frame_copy.frame_format.size.width,
                    frame_copy.frame_format.size.height,
                );

                debug!("Committing surface creation changes.");
                surface.commit();

                debug!("Waiting for layer surface to be configured.");
                while !state.configured_outputs.contains(&output_info.wl_output) {
                    event_queue.blocking_dispatch(&mut state)?;
                }

                surface.set_buffer_transform(output_info.transform);
                // surface.set_buffer_scale(output_info.scale());
                surface.attach(Some(&frame_guard.buffer), 0, 0);

                if let Some(viewporter) = viewporter.as_ref() {
                    let viewport = viewporter.get_viewport(&surface, &qh, ());
                    viewport.set_destination(
                        output_info.logical_region.inner.size.width as i32,
                        output_info.logical_region.inner.size.height as i32,
                    );
                }

                debug!("Committing surface with attached buffer.");
                surface.commit();
                layer_shell_surfaces.push((surface, layer_surface));
                event_queue.blocking_dispatch(&mut state)?;

                Ok(())
            })?;
        }

        let callback_result = callback();

        debug!("Unmapping and destroying layer shell surfaces.");
        for (surface, layer_shell_surface) in layer_shell_surfaces.iter() {
            surface.attach(None, 0, 0);
            surface.commit(); 
            layer_shell_surface.destroy();
        }
        event_queue.roundtrip(&mut state)?;

        callback_result
    }
    pub fn capture_frame_copies(
        &self,
        output_capture_regions: &[(OutputInfo, Option<EmbeddedRegion>)],
        cursor_overlay: bool,
    ) -> Result<Vec<(FrameCopy, FrameGuard, OutputInfo)>> {
        output_capture_regions
            .iter()
            .map(|(output_info, capture_region)| {
                self.capture_frame_copy(cursor_overlay, output_info, *capture_region)
                    .map(|(frame_copy, frame_guard)| (frame_copy, frame_guard, output_info.clone()))
            })
            .collect()    }
    fn capture_frame_copy(
        &self,
        cursor_overlay: bool,
        output_info: &OutputInfo,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<(FrameCopy, FrameGuard)> {
        // Create an in memory file and return it's file descriptor.
        let fd = create_shm_fd()?;
        // Create a writeable memory map backed by a mem_file.
        let mem_file = File::from(fd);

        let mut state = CaptureFrameState {
            formats: Vec::new(),
            dmabuf_formats: Vec::new(),
            state: None,
            buffer_done: AtomicBool::new(false),
        };
        
        let mut event_queue = self.conn.new_event_queue::<CaptureFrameState>();
        let qh = event_queue.handle();

        let output_capture_source_manager = match self.globals.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(
            &qh,
            1..=ExtOutputImageCaptureSourceManagerV1::interface().version,
            (),
        ) {
            Ok(manager) => manager,
            Err(e) => {
                tracing::error!("Failed to bind ExtOutputImageCaptureSourceManagerV1: {:?}", e);
                return Err(Error::ProtocolNotFound(
                    "ExtOutputImageCaptureSourceManagerV1 not found".to_string(),
                ));
            }
        };
        let capture_src = output_capture_source_manager.create_source(&output_info.wl_output, &qh, ());

        let screencopy_manager = match self.globals.bind::<ExtImageCopyCaptureManagerV1, _, _>(
            &qh,
            1..=ExtImageCopyCaptureManagerV1::interface().version,
            (),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Failed to bind ExtImageCopyCaptureManagerV1: {:?}", e);
                return Err(Error::ProtocolNotFound(
                    "ExtImageCopyCaptureManagerV1 not found".to_string(),
                ));
            }
        };

        tracing::debug!("Capturing output(shm buffer)...");
        let frame_session = screencopy_manager.create_session(&capture_src, Options::empty(), &qh, ());


        // Empty internal event buffer until buffer_done is set to true which is when the Buffer done
        // event is fired, aka the capture from the compositor is successful.
        while !state.buffer_done.load(Ordering::SeqCst) {
            event_queue.blocking_dispatch(&mut state)?;
        }

        tracing::trace!(
            "Received compositor frame buffer formats: {:#?}",
            state.formats
        );
        // Filter advertised wl_shm formats and select the first one that matches.
        let frame_format = state
            .formats
            .iter()
            .find(|frame| {
                matches!(
                    frame.format,
                    wl_shm::Format::Xbgr2101010
                        | wl_shm::Format::Abgr2101010
                        | wl_shm::Format::Argb8888
                        | wl_shm::Format::Xrgb8888
                        | wl_shm::Format::Xbgr8888
                        | wl_shm::Format::Bgr888
                )
            })
            .copied()
            // Check if frame format exists.
            .ok_or_else(|| {
                tracing::error!("No suitable frame format found");
                Error::NoSupportedBufferFormat
            })?;
            let bytes_per_pixel = match frame_format.format {
                wayland_client::protocol::wl_shm::Format::Argb8888 | wayland_client::protocol::wl_shm::Format::Xrgb8888
                | wayland_client::protocol::wl_shm::Format::Abgr8888 | wayland_client::protocol::wl_shm::Format::Xbgr8888 => 4,
                wayland_client::protocol::wl_shm::Format::Rgb565 => 2,
                _ => {
                    eprintln!("supported SHM format for buffer creation ");
                    0
                }
            };
            if let Some(old_format) = state.formats.pop() {
                let new_format = FrameFormat {
                    stride: frame_format.size.width * bytes_per_pixel as u32,
                    ..old_format
                };
                state.formats.push(new_format);
            }
            
        tracing::trace!("Selected frame buffer format: {:#?}", frame_format);
        mem_file.set_len(frame_format.byte_size())?;
        // proceed to add this loop
        let shm = self.globals.bind::<WlShm, _, _>(&qh, 1..=1, ())?;
        let shm_pool = shm.create_pool(
            mem_file.as_fd(),
            frame_format
                .byte_size()
                .try_into()
                .map_err(|_| Error::BufferTooSmall)?,
            &qh,
            (),
        );
        let buffer = shm_pool.create_buffer(
            0,
            frame_format.size.width as i32,
            frame_format.size.height as i32,
            frame_format.stride as i32,
            frame_format.format,
            &qh,
            (),
        );
        
        let mut frame: Option<ExtImageCopyCaptureFrameV1>; 
        let frame_before_capture = frame_session.create_frame(&qh, ());
        let frame = frame_before_capture.clone();
        frame.attach_buffer(&buffer);
        frame.capture();

        let frame_guard = FrameGuard {
                buffer,
                shm_pool,
            };
        event_queue.blocking_dispatch(&mut state)?;

        let mut frame_mmap = unsafe { MmapMut::map_mut(&mem_file)? };
        let data = &mut *frame_mmap;
        let frame_color_type = match create_converter(frame_format.format) {
            Some(converter) => converter.convert_inplace(data),
            _ => {
                tracing::error!("Unsupported buffer format: {:?}", frame_format.format);
                tracing::error!(
                    "You can send a feature request for the above format to the mailing list for wayshot over at https://sr.ht/~shinyzenith/wayshot."
                );
                return Err(Error::NoSupportedBufferFormat);
            }
        };
        let rotated_physical_size = match output_info.transform {
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
                Size {
                    width: frame_format.size.height,
                    height: frame_format.size.width,
                }
            }
            _ => frame_format.size,
        };
        let frame_copy = FrameCopy {
            frame_format,
            frame_color_type,
            frame_data: FrameData::Mmap(frame_mmap),
            transform: output_info.transform,
            logical_region: capture_region
                .map(|capture_region| capture_region.logical())
                .unwrap_or(output_info.logical_region),
            physical_size: rotated_physical_size,
        };
        tracing::debug!("Created frame copy: {:#?}", frame_copy);
        Ok((frame_copy, frame_guard))
    }


}

