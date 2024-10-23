use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Context, ContextCompat};
use color_eyre::Result;
use image::RgbaImage;
use log::{error, warn};
use smithay_client_toolkit::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay_client_toolkit::reexports::client::protocol::wl_output::{Transform, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_surface;
use smithay_client_toolkit::reexports::client::QueueHandle;
use smithay_client_toolkit::shell::wlr_layer::{LayerSurface, LayerSurfaceConfigure};
use smithay_client_toolkit::{
    reexports::calloop::timer::{TimeoutAction, Timer},
    shell::WaylandSurface,
};

use crate::render::{EglContext, Renderer};
use crate::wpaperd::Wpaperd;
use crate::{display_info::DisplayInfo, wallpaper_info::WallpaperInfo};
use crate::{image_loader::ImageLoader, image_picker::ImagePicker};

#[derive(Debug)]
pub enum EventSource {
    NotSet,
    Running(RegistrationToken),
    // The contained value is the duration that was left on the previous timer, used for starting the next timer.
    Paused(Duration),
}

pub struct Surface {
    wl_surface: wl_surface::WlSurface,
    wl_output: WlOutput,
    layer: LayerSurface,
    egl_context: EglContext,
    renderer: Renderer,
    pub image_picker: ImagePicker,
    event_source: EventSource,
    pub wallpaper_info: WallpaperInfo,
    info: Rc<RefCell<DisplayInfo>>,
    image_loader: Rc<RefCell<ImageLoader>>,
    window_drawn: bool,
    loading_image: Option<(PathBuf, usize)>,
    loading_image_tries: u8,
    /// Determines whether we should skip the next transition. Used to skip
    /// the first transition when starting up.
    ///
    /// See [crate::wallpaper_info::WallpaperInfo]'s `initial_transition` field
    skip_next_transition: bool,
    /// Pause state of the automatic wallpaper sequence.
    /// Setting this to true will mean only an explicit next/previous wallpaper command will change
    /// the wallpaper.
    should_pause: bool,
}

impl Surface {
    pub fn new(
        wpaperd: &Wpaperd,
        wl_layer: LayerSurface,
        wl_output: WlOutput,
        info: DisplayInfo,
        wallpaper_info: WallpaperInfo,
        egl_display: egl::Display,
        qh: &QueueHandle<Wpaperd>,
    ) -> Self {
        let wl_surface = wl_layer.wl_surface().clone();
        let egl_context = EglContext::new(egl_display, &wl_surface);
        // Make the egl context as current to make the renderer creation work
        egl_context
            .make_current()
            .expect("EGL context switching to work");

        // Commit the surface
        wl_surface.commit();

        let image_picker = ImagePicker::new(
            &wallpaper_info,
            &wl_surface,
            wpaperd.filelist_cache.clone(),
            wpaperd.wallpaper_groups.clone(),
        );

        let image = black_image();
        let info = Rc::new(RefCell::new(info));

        let renderer = unsafe {
            Renderer::new(
                image.into(),
                info.clone(),
                0,
                wallpaper_info.transition.clone(),
                info.borrow().transform,
            )
            .expect("unable to create the renderer")
        };

        let first_transition = !wallpaper_info.initial_transition;
        let mut surface = Self {
            wl_output,
            layer: wl_layer,
            info,
            wl_surface,
            egl_context,
            renderer,
            image_picker,
            event_source: EventSource::NotSet,
            wallpaper_info,
            window_drawn: false,
            should_pause: false,
            image_loader: wpaperd.image_loader.clone(),
            loading_image: None,
            loading_image_tries: 0,
            skip_next_transition: first_transition,
        };

        // Start loading the wallpaper as soon as possible (i.e. surface creation)
        // It will still be loaded as a texture when we have an openGL context
        if let Err(err) = surface.load_wallpaper(qh) {
            warn!("{err:?}");
        }

        surface
    }

    /// Returns true if something has been drawn to the surface
    pub fn draw(&mut self, qh: &QueueHandle<Wpaperd>, time: Option<u32>) -> Result<()> {
        let info = self.info.borrow();
        let width = info.adjusted_width();
        let height = info.adjusted_height();
        // Drop the borrow to self
        drop(info);

        // Use the correct context before loading the texture and drawing
        self.egl_context.make_current()?;

        let wallpaper_loaded = self.load_wallpaper(qh)?;

        if self.renderer.transition_running() {
            // Recalculate the current progress, the transition might end now
            let transition_running = self.renderer.update_transition_status(time.unwrap_or(0));
            // If we don't have any time passed, just consider the transition to be ended
            if transition_running {
                // Don't call queue_draw as it calls load_wallpaper again
                self.wl_surface.frame(qh, self.wl_surface.clone());
            } else {
                self.renderer.transition_finished();
            }
        } else if !wallpaper_loaded {
            self.wl_surface.frame(qh, self.wl_surface.clone());
            if self.window_drawn {
                // We need to call commit, otherwise the call to frame above doesn't work
                self.wl_surface().commit();
                return Ok(());
            }
        }

        unsafe { self.renderer.draw()? }

        self.renderer.clear_after_draw()?;
        self.egl_context.swap_buffers()?;

        // Reset the context
        egl::API
            .make_current(self.egl_context.display, None, None, None)
            .context("Resetting the GL context")?;

        // Mark the entire surface as damaged
        self.wl_surface.damage_buffer(0, 0, width, height);

        // Finally, commit the surface
        self.wl_surface.commit();

        Ok(())
    }

    // Call surface::frame when this return false
    pub fn load_wallpaper(&mut self, qh: &QueueHandle<Wpaperd>) -> Result<bool> {
        Ok(loop {
            // If we were not already trying to load an image
            if self.loading_image.is_none() {
                if let Some(item) = self
                    .image_picker
                    .get_image_from_path(&self.wallpaper_info.path, qh)
                {
                    if self.image_picker.current_image() == item.0
                        && !self.image_picker.is_reloading()
                    {
                        break true;
                    } else {
                        // We are trying to load a new image
                        self.loading_image = Some(item);
                    }
                } else {
                    // we don't need to load any image
                    break true;
                }
            }
            let (image_path, index) = self
                .loading_image
                .as_ref()
                .expect("loading image to be set")
                .clone();

            if self.renderer.transition_running() {
                break true;
            }

            let res = self
                .image_loader
                .borrow_mut()
                .background_load(image_path.to_owned(), self.name());
            match res {
                crate::image_loader::ImageLoaderStatus::Loaded(data) => {
                    // Renderer::load_wallpaper load the wallpaper in a openGL texture
                    // Set the correct opengl context
                    self.egl_context.make_current()?;
                    self.renderer.load_wallpaper(
                        data.into(),
                        self.wallpaper_info.mode,
                        self.wallpaper_info.offset,
                    )?;

                    let transition_time = if self.skip_next_transition {
                        0
                    } else {
                        self.wallpaper_info.transition_time
                    };
                    self.skip_next_transition = false;

                    if self.image_picker.is_reloading() {
                        self.image_picker.reloaded();
                    } else {
                        self.image_picker.update_current_image(image_path, index);
                        self.renderer.start_transition(transition_time);
                    }
                    // Restart the counter
                    self.loading_image_tries = 0;
                    self.loading_image = None;
                    break true;
                }
                crate::image_loader::ImageLoaderStatus::Waiting => {
                    // wait until the image has been loaded
                    break false;
                }
                crate::image_loader::ImageLoaderStatus::Error => {
                    // We don't want to try too many times
                    self.loading_image_tries += 1;
                    // The image we were trying to load failed
                    self.loading_image = None;
                }
            }
            // If we have tried too many times, stop
            if self.loading_image_tries == 5 {
                break true;
            }
        })
    }

    pub fn name(&self) -> String {
        self.info.borrow().name.to_string()
    }

    pub fn description(&self) -> String {
        self.info.borrow().description.to_string()
    }

    /// Resize the surface
    pub fn resize(&mut self, qh: &QueueHandle<Wpaperd>) -> Result<()> {
        let info = self.info.borrow();
        let width = info.adjusted_width();
        let height = info.adjusted_height();
        // Drop the borrow to self
        drop(info);
        // self.layer.set_size(width as u32, height as u32);
        let display_name = self.name();
        self.egl_context
            .resize(&self.wl_surface, width, height)
            .with_context(|| {
                format!("unable to switch resize EGL context for display {display_name}",)
            })?;
        self.egl_context.make_current().with_context(|| {
            format!("unable to switch the openGL context for display {display_name}")
        })?;
        self.renderer.resize().with_context(|| {
            format!("unable to resize the GL window for display {display_name}")
        })?;
        // If we resize, stop immediately any lingering transition
        self.renderer.force_transition_end();

        // Queue drawing for the next frame. We can directly draw here, but we would still
        // need to queue the draw for the next frame, otherwise wpaperd doesn't work at startup
        self.queue_draw(qh);

        Ok(())
    }

    pub fn change_size(&mut self, configure: LayerSurfaceConfigure, qh: &QueueHandle<Wpaperd>) {
        let mut info = self.info.borrow_mut();
        if info.change_size(configure) {
            drop(info);
            if let Err(err) = self.resize(qh) {
                error!("{err:?}");
            }
        }
    }

    pub fn change_transform(&mut self, transform: Transform, qh: &QueueHandle<Wpaperd>) {
        let mut info = self.info.borrow_mut();
        if info.change_transform(transform) {
            drop(info);
            self.wl_surface.set_buffer_transform(transform);
            if let Err(err) = self
                .resize(qh)
                .and_then(|_| {
                    self.renderer
                        .set_mode(self.wallpaper_info.mode, self.wallpaper_info.offset)
                })
                .and_then(|_| unsafe { self.renderer.set_projection_matrix(transform) })
            {
                error!("{err:?}");
            }
        }
    }

    pub fn change_scale_factor(&mut self, scale_factor: i32, qh: &QueueHandle<Wpaperd>) {
        let mut info = self.info.borrow_mut();
        if info.change_scale_factor(scale_factor) {
            drop(info);
            self.wl_surface.set_buffer_scale(scale_factor);
            // Resize the gl viewport
            if let Err(err) = self.resize(qh) {
                error!("{err:?}");
            }
        }
    }

    /// Check that the dimensions are valid
    pub fn is_configured(&self) -> bool {
        let info = self.info.borrow();
        info.width != 0 && info.height != 0
    }

    pub fn has_been_drawn(&self) -> bool {
        self.window_drawn
    }

    pub fn drawn(&mut self) {
        self.window_drawn = true;
    }

    /// Update the wallpaper_info of this Surface
    /// return true if the duration has changed
    pub fn update_wallpaper_info(
        &mut self,
        handle: &LoopHandle<Wpaperd>,
        qh: &QueueHandle<Wpaperd>,
        mut wallpaper_info: WallpaperInfo,
    ) {
        if self.wallpaper_info == wallpaper_info {
            return;
        }

        // Put the new value in place
        std::mem::swap(&mut self.wallpaper_info, &mut wallpaper_info);
        let path_changed = self.wallpaper_info.path != wallpaper_info.path;
        self.image_picker.update_sorting(
            self.wallpaper_info.sorting,
            &self.wallpaper_info.path,
            path_changed,
            wallpaper_info.drawn_images_queue_size,
        );
        if path_changed {
            // ask the image_picker to pick a new a image
            self.image_picker.next_image(&self.wallpaper_info.path, qh);
            self.queue_draw(qh);
        }
        if self.wallpaper_info.duration != wallpaper_info.duration {
            match (self.wallpaper_info.duration, wallpaper_info.duration) {
                (None, None) => {
                    unreachable!()
                }
                // There was a duration before but now it has been removed
                (None, Some(_)) => {
                    if let EventSource::Running(registration_token) = self.event_source {
                        handle.remove(registration_token);
                    }
                }
                // There wasn't a duration before but now it has been added or it has changed
                (Some(new_duration), None) | (Some(new_duration), Some(_)) => {
                    if let EventSource::Running(registration_token) = self.event_source {
                        handle.remove(registration_token);
                    }

                    // if the path has not changed or the duration has changed
                    // and the remaining time is great than 0
                    let timer = if let (false, Some(remaining_time)) = (
                        path_changed,
                        remaining_duration(new_duration, self.image_picker.image_changed_instant),
                    ) {
                        Some(Timer::from_duration(remaining_time))
                    } else {
                        // otherwise draw the image immediately, the next timer
                        // will be set to the new duration
                        Some(Timer::immediate())
                    };

                    self.event_source = EventSource::NotSet;
                    self.add_timer(timer, handle, qh.clone());
                }
            }
        }

        if self.wallpaper_info.mode != wallpaper_info.mode
            || self.wallpaper_info.offset != wallpaper_info.offset
        {
            if let Err(err) = self.egl_context.make_current().and_then(|_| {
                self.renderer
                    .set_mode(self.wallpaper_info.mode, self.wallpaper_info.offset)
            }) {
                error!("{err:?}");
            }
            if !path_changed {
                // We should draw immediately
                if let Err(err) = self.draw(qh, None) {
                    warn!("{err:?}");
                }
            }
        }
        if self.wallpaper_info.transition != wallpaper_info.transition {
            match self.egl_context.make_current() {
                Ok(_) => {
                    let transform = self.renderer.display_info.borrow().transform;
                    self.renderer
                        .update_transition(self.wallpaper_info.transition.clone(), transform);
                }
                Err(err) => {
                    error!("{err:?}");
                }
            }
        }
        if self.wallpaper_info.drawn_images_queue_size != wallpaper_info.drawn_images_queue_size {
            self.image_picker
                .update_queue_size(self.wallpaper_info.drawn_images_queue_size);
        }
        if self.wallpaper_info.transition_time != wallpaper_info.transition_time {
            self.renderer
                .update_transition_time(self.wallpaper_info.transition_time);
        }
    }

    /// Add a new timer in the event_loop for the current duration
    /// Stop if there is already a timer added
    pub fn add_timer(
        &mut self,
        timer: Option<Timer>,
        handle: &LoopHandle<Wpaperd>,
        qh: QueueHandle<Wpaperd>,
    ) {
        if matches!(self.event_source, EventSource::Running(_)) {
            return;
        }
        let Some(duration) = self.wallpaper_info.duration else {
            return;
        };

        let timer = timer.unwrap_or(Timer::from_duration(duration));

        let name = self.name().clone();
        let registration_token = handle
            .insert_source(
                timer,
                move |_deadline, _: &mut (), wpaperd: &mut Wpaperd| {
                    let surface = match wpaperd
                        .surface_from_name(&name)
                        .with_context(|| format!("expecting surface {name} to be available"))
                    {
                        Ok(surface) => surface,
                        Err(err) => {
                            error!("{err:?}");
                            return TimeoutAction::Drop;
                        }
                    };

                    if let Some(duration) = surface.wallpaper_info.duration {
                        // Check that the timer has expired
                        // if the daemon received a next or previous image command
                        // the timer will be reset and we need to account that here
                        // i.e. there is a timer of 1 minute. The user changes the image
                        // with a previous wallpaper command at 50 seconds.
                        // The timer will be reset to 1 minute and the image will be changed
                        if let Some(remaining_time) =
                            remaining_duration(duration, surface.image_picker.image_changed_instant)
                        {
                            TimeoutAction::ToDuration(remaining_time)
                        } else {
                            // Change the drawn image
                            surface
                                .image_picker
                                .next_image(&surface.wallpaper_info.path, &qh);
                            surface.queue_draw(&qh);
                            TimeoutAction::ToDuration(duration)
                        }
                    } else {
                        TimeoutAction::Drop
                    }
                },
            )
            .expect("Failed to insert event source!");

        self.event_source = EventSource::Running(registration_token);
    }

    /// Handle updating the timer based on the pause state of the automatic wallpaper sequence.
    /// Remove the timer if pausing, and add a new timer with the remaining duration of the old
    /// timer when resuming.
    pub fn handle_pause_state(&mut self, handle: &LoopHandle<Wpaperd>, qh: QueueHandle<Wpaperd>) {
        match (self.should_pause, &self.event_source) {
            // Should pause, but timer is still currently running
            (true, EventSource::Running(registration_token)) => {
                let remaining_duration = self.get_remaining_duration().unwrap_or_default();

                handle.remove(*registration_token);
                self.event_source = EventSource::Paused(remaining_duration);
            }
            // Should resume, but timer is not currently running
            (false, EventSource::Paused(duration)) => {
                self.add_timer(Some(Timer::from_duration(*duration)), handle, qh.clone());
            }
            // Otherwise no update is necessary
            (_, _) => {}
        }
    }

    #[inline]
    pub fn queue_draw(&mut self, qh: &QueueHandle<Wpaperd>) {
        // Start loading the next image immediately
        if let Err(err) = self.load_wallpaper(qh) {
            warn!("{err:?}");
        }
        self.wl_surface.frame(qh, self.wl_surface.clone());
        self.wl_surface.commit();
    }

    #[inline]
    fn get_remaining_duration(&self) -> Option<Duration> {
        let duration = self.wallpaper_info.duration?;
        remaining_duration(duration, self.image_picker.image_changed_instant)
    }

    /// Indicate to the main event loop that the automatic wallpaper sequence for this [`Surface`]
    /// should be paused.
    /// The actual pausing/resuming is handled in [`Surface::handle_pause_state`]
    #[inline]
    pub fn pause(&mut self) {
        self.should_pause = true;
    }
    /// Indicate to the main event loop that the automatic wallpaper sequence for this [`Surface`]
    /// should be resumed.
    /// The actual pausing/resuming is handled in [`Surface::handle_pause_state`]
    #[inline]
    pub fn resume(&mut self) {
        self.should_pause = false;
    }

    /// Toggle the pause state for this [`Surface`], which is responsible for indicating to the main
    /// event loop that the automatic wallpaper sequence should be paused.
    /// The actual pausing/resuming is handled in [`Surface::handle_pause_state`]
    #[inline]
    pub fn toggle_pause(&mut self) {
        if self.should_pause() {
            self.resume();
        } else {
            self.pause();
        };
    }

    /// Returns a boolean representing whether this [`Surface`] is set to indicate to the main event
    /// loop that its automatic wallpaper sequence should be paused.
    #[inline]
    pub fn should_pause(&self) -> bool {
        self.should_pause
    }

    pub fn wl_surface(&self) -> &wl_surface::WlSurface {
        &self.wl_surface
    }

    pub fn wl_output(&self) -> &WlOutput {
        &self.wl_output
    }

    pub fn layer(&self) -> &LayerSurface {
        &self.layer
    }
}

fn black_image() -> RgbaImage {
    RgbaImage::from_raw(1, 1, vec![0, 0, 0, 255]).unwrap()
}

fn remaining_duration(duration: Duration, image_changed: Instant) -> Option<Duration> {
    // The timer has already expired
    let diff = image_changed.elapsed();
    if duration.saturating_sub(diff).is_zero() {
        None
    } else {
        Some(duration - diff)
    }
}
