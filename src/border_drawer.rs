use anyhow::Context;
use log::error;
use std::cell::RefCell;
use std::time;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_F, D2D1_COLOR_F, D2D1_COMPOSITE_MODE_SOURCE_OVER,
    D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_SMALL, D2D1_BRUSH_PROPERTIES, D2D1_INTERPOLATION_MODE_LINEAR,
    D2D1_ROUNDED_RECT, D2D1_SWEEP_DIRECTION_CLOCKWISE, ID2D1Brush, ID2D1Factory1,
    ID2D1PathGeometry, ID2D1RenderTarget,
};
use windows_numerics::{Matrix3x2, Vector2};

use crate::APP_STATE;
use crate::animations::{AnimType, Animations};
use crate::border_config::BorderConfig;
use crate::colors::ColorBrush;
use crate::effects::Effects;
use crate::render_backend::{
    D2DDeviceContextDrawGuard, D2DHwndRenderTargetDrawGuard, D2DMultithreadGuard,
    DCompSurfaceDrawGuard, RenderBackend, RenderBackendConfig, TARGET_BITMAP_PROPS,
};
use crate::utils::{
    StandaloneWindowsError, T_E_UNINIT, ToWindowsResult, WindowsCompatibleError,
    WindowsCompatibleResult, WindowsContext, WriteLockable,
};
use crate::window_border::WindowState;

#[derive(Debug, Default)]
pub struct BorderDrawer {
    pub stroke_width: i32,
    // This is WriteLockable so it doesn't accidentally change when the tracking window is in
    // the snapped/arranged state where the borders are supposed to remain square
    pub corner_radius: WriteLockable<f32>,
    pub render_backend: RenderBackend,
    pub active_color: ColorBrush,
    pub inactive_color: ColorBrush,
    pub animations: Animations,
    pub effects: Effects,
    pub last_render_time: Option<time::Instant>,
    pub last_anim_time: Option<time::Instant>,
    pub fill_outer_corners: bool,
    corner_caps: RefCell<Option<(D2D_RECT_F, f32, f32, ID2D1PathGeometry)>>,
}

impl BorderDrawer {
    pub fn configure_appearance(&mut self, config: &BorderConfig, dpi: u32, tracking_window: HWND) {
        let stroke_width = config.width_at(dpi);
        let corner_radius = config.radius_at(stroke_width, dpi, tracking_window);

        self.stroke_width = stroke_width;
        self.corner_radius = WriteLockable::new(corner_radius);
        self.active_color = config.active_color.to_color_brush(true);
        self.inactive_color = config.inactive_color.to_color_brush(false);
        self.animations = config.animations.to_animations();
        self.effects = config.effects.to_effects(dpi);
    }

    pub fn init(
        &mut self,
        width: u32,
        height: u32,
        border_window: HWND,
        bounds: D2D_RECT_F,
        render_backend_config: RenderBackendConfig,
    ) -> WindowsCompatibleResult<()> {
        // Drop our current render backend to avoid issues with recreating existing resources when
        // calling init() multiple times in a row
        self.render_backend = RenderBackend::None;

        self.render_backend = render_backend_config
            .to_render_backend(width, height, border_window, self.effects.is_enabled())
            .windows_context("could not initialize render backend in init()")?;

        let renderer: &ID2D1RenderTarget = match self.render_backend {
            RenderBackend::V2(ref backend) => &backend.d2d_context,
            RenderBackend::Legacy(ref backend) => &backend.render_target,
            RenderBackend::None => {
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "render backend is None"),
                ));
            }
        };

        // We will adjust opacity later. For now, we set it to 0.
        let brush_properties = D2D1_BRUSH_PROPERTIES {
            opacity: 0.0,
            transform: Matrix3x2::identity(),
        };
        self.active_color
            .init_brush(renderer, &bounds, &brush_properties)?;
        self.inactive_color
            .init_brush(renderer, &bounds, &brush_properties)?;

        if self.render_backend.supports_effects() {
            self.effects
                .init_command_lists_if_enabled(&self.render_backend)
                .windows_context("could not initialize command list")?;
        }

        Ok(())
    }

    pub fn uninit(&mut self) {
        self.render_backend = RenderBackend::None;
        let _ = self.active_color.take_brush();
        let _ = self.inactive_color.take_brush();
        let _ = self.effects.take_active_command_list();
        let _ = self.effects.take_inactive_command_list();
    }

    pub fn resize_renderer(&mut self, width: u32, height: u32) -> WindowsCompatibleResult<()> {
        self.render_backend
            .resize(width, height, self.effects.is_enabled())
            .windows_context("could not update render resources")?;

        if self.render_backend.supports_effects() {
            self.effects
                .init_command_lists_if_enabled(&self.render_backend)
                .context("could not initialize command lists")
                .to_windows_result(T_E_UNINIT)?;
        }

        Ok(())
    }

    /// Renders a border onto the internal bitmap along the inside edge of the bounds. Effects
    /// (e.g. glow) are drawn outside, so the caller should pad the bounds if needed to prevent clipping.
    ///
    /// NOTE: bound coordinates should be specified relative to the bitmap, not the screen.
    pub fn render(
        &mut self,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        self.last_render_time = Some(time::Instant::now());

        // Direct2D draws a stroke centered along a given rect, but we want it to be drawn on the
        // inside of 'bounds'. To achieve this, we pad it by half the stroke width.
        let half_stroke_width = self.stroke_width as f32 / 2.0;
        let stroke_rect = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: bounds.left + half_stroke_width,
                top: bounds.top + half_stroke_width,
                right: bounds.right - half_stroke_width,
                bottom: bounds.bottom - half_stroke_width,
            },
            radiusX: *self.corner_radius.get(),
            radiusY: *self.corner_radius.get(),
        };

        // Note that Rust's borrow checker prevents passing the render backend from the match arm,
        // so I'll need to grab it from within the respective functions instead
        match self.render_backend {
            RenderBackend::V2(_) if self.effects.should_apply(window_state) => {
                self.render_v2_with_effects(stroke_rect, bounds, window_state)?
            }
            RenderBackend::V2(_) => self.render_v2(stroke_rect, bounds, window_state)?,
            RenderBackend::Legacy(_) => self.render_legacy(stroke_rect, bounds, window_state)?,
            RenderBackend::None => {
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "render_backend is None"),
                ));
            }
        }

        Ok(())
    }

    fn render_legacy(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::Legacy(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let render_target = &backend.render_target;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            let draw_guard = D2DHwndRenderTargetDrawGuard::begin(render_target);
            render_target.Clear(None);

            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_rectangle(&stroke_rect, &bounds, render_target, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_rectangle(&stroke_rect, &bounds, render_target, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            draw_guard.finish()?;
        }

        Ok(())
    }

    fn render_v2(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::V2(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let d2d_context = &backend.d2d_context;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            let _d2d_multithread_guard = D2DMultithreadGuard::enter()?;

            // Set d2d_context's target back to the target_bitmap so we can draw to the display
            let mut point = Default::default();
            let (surface_draw_guard, dxgi_surface) =
                DCompSurfaceDrawGuard::begin(&backend.d_comp_surface, d2d_context, &mut point)?;
            let target_bitmap = d2d_context
                .CreateBitmapFromDxgiSurface(&dxgi_surface, Some(&TARGET_BITMAP_PROPS))
                .windows_context("target_bitmap")?;
            d2d_context.SetTarget(&target_bitmap);

            // Draw to the target_bitmap
            let draw_guard = D2DDeviceContextDrawGuard::begin(d2d_context);
            d2d_context.Clear(None);

            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_rectangle(&stroke_rect, &bounds, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_rectangle(&stroke_rect, &bounds, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            draw_guard.finish()?;

            surface_draw_guard.finish()?;
            backend
                .d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;
            drop(_d2d_multithread_guard);
        }

        Ok(())
    }

    fn render_v2_with_effects(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::V2(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let d2d_context = &backend.d2d_context;

        let half_stroke_width = stroke_rect.rect.left - bounds.left;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            // Create a rect that covers up to the outer edge of the border
            let border_outer_rect = D2D1_ROUNDED_RECT {
                rect: bounds,
                radiusX: if self.fill_outer_corners {
                    0.0
                } else {
                    stroke_rect.radiusX + half_stroke_width
                },
                radiusY: if self.fill_outer_corners {
                    0.0
                } else {
                    stroke_rect.radiusY + half_stroke_width
                },
            };

            // Set the d2d_context target to the border_bitmap
            let border_bitmap = backend
                .border_bitmap
                .as_ref()
                .context("could not get border_bitmap")
                .to_windows_result(T_E_UNINIT)?;
            d2d_context.SetTarget(border_bitmap);

            // Draw to the border_bitmap
            let draw_guard = D2DDeviceContextDrawGuard::begin(d2d_context);
            d2d_context.Clear(None);

            // We use filled rectangles here because it helps make the effects more visible.
            // Additionally, if someone sets the stroke width to 0, the effects will still be
            // visible (whereas they wouldn't be if we used a hollow rectangle).
            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.fill_rectangle(&border_outer_rect, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.fill_rectangle(&border_outer_rect, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            draw_guard.finish()?;
        }

        unsafe {
            // Set the d2d_context target to the mask_bitmap to create an alpha mask
            let mask_bitmap = backend
                .mask_bitmap
                .as_ref()
                .context("could not get mask_bitmap")
                .to_windows_result(T_E_UNINIT)?;
            d2d_context.SetTarget(mask_bitmap);

            // Create a rect that covers up to the inner edge of the border
            // This rect is used to mask out the inner portion of the border
            let border_inner_rect = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: stroke_rect.rect.left + half_stroke_width,
                    top: stroke_rect.rect.top + half_stroke_width,
                    right: stroke_rect.rect.right - half_stroke_width,
                    bottom: stroke_rect.rect.bottom - half_stroke_width,
                },
                radiusX: stroke_rect.radiusX - half_stroke_width,
                radiusY: stroke_rect.radiusY - half_stroke_width,
            };

            // Create a 100% opaque brush because our active/inactive colors' brushes might not be
            let opaque_brush = d2d_context.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?;

            let draw_guard = D2DDeviceContextDrawGuard::begin(d2d_context);
            d2d_context.Clear(None);

            self.fill_rectangle(&border_inner_rect, d2d_context, &opaque_brush);

            draw_guard.finish()?;
        }

        unsafe {
            let _d2d_multithread_guard = D2DMultithreadGuard::enter()?;

            // Set d2d_context's target back to the target_bitmap so we can draw to the display
            let mut point = Default::default();
            let (surface_draw_guard, dxgi_surface) =
                DCompSurfaceDrawGuard::begin(&backend.d_comp_surface, d2d_context, &mut point)?;
            let target_bitmap = d2d_context
                .CreateBitmapFromDxgiSurface(&dxgi_surface, Some(&TARGET_BITMAP_PROPS))
                .windows_context("target_bitmap")?;
            d2d_context.SetTarget(&target_bitmap);

            // Retrieve our command list (includes border_bitmap, mask_bitmap, and effects)
            let command_list = self
                .effects
                .get_current_command_list(window_state)
                .to_windows_result(T_E_UNINIT)?;

            // Draw to the target_bitmap
            let draw_guard = D2DDeviceContextDrawGuard::begin(d2d_context);
            d2d_context.Clear(None);

            d2d_context.DrawImage(
                command_list,
                None,
                None,
                D2D1_INTERPOLATION_MODE_LINEAR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );

            draw_guard.finish()?;

            surface_draw_guard.finish()?;
            backend
                .d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;

            drop(_d2d_multithread_guard);
        }

        Ok(())
    }

    // NOTE: ID2D1DeviceContext implements From<&ID2D1DeviceContext> for &ID2D1RenderTarget
    fn draw_rectangle(
        &self,
        stroke_rect: &D2D1_ROUNDED_RECT,
        bounds: &D2D_RECT_F,
        renderer: &ID2D1RenderTarget,
        brush: &ID2D1Brush,
    ) {
        unsafe {
            match stroke_rect.radiusX {
                0.0 => {
                    renderer.DrawRectangle(&stroke_rect.rect, brush, self.stroke_width as f32, None)
                }
                _ => {
                    renderer.DrawRoundedRectangle(
                        stroke_rect,
                        brush,
                        self.stroke_width as f32,
                        None,
                    );
                    if self.fill_outer_corners
                        && let Some(caps) = self.get_corner_caps(bounds, stroke_rect)
                    {
                        renderer.FillGeometry(&caps, brush, None);
                    }
                }
            }
        }
    }

    fn get_corner_caps(
        &self,
        bounds: &D2D_RECT_F,
        stroke_rect: &D2D1_ROUNDED_RECT,
    ) -> Option<ID2D1PathGeometry> {
        let radius = stroke_rect.radiusX;
        let stroke_width = self.stroke_width as f32;
        if radius <= 0.0 || stroke_width <= 0.0 {
            return None;
        }

        let mut caps = self.corner_caps.borrow_mut();
        let needs_recreate = match *caps {
            Some((ref b, r, sw, _)) => {
                b.left != bounds.left
                    || b.top != bounds.top
                    || b.right != bounds.right
                    || b.bottom != bounds.bottom
                    || r != radius
                    || sw != stroke_width
            }
            None => true,
        };

        if needs_recreate {
            match create_corner_caps_geometry(
                &APP_STATE.render_factory,
                bounds,
                radius,
                stroke_width,
            ) {
                Ok(geom) => {
                    *caps = Some((*bounds, radius, stroke_width, geom));
                }
                Err(err) => {
                    error!("could not create corner caps geometry: {err:#}");
                    return None;
                }
            }
        }

        caps.as_ref().map(|(_, _, _, geom)| geom.clone())
    }

    // NOTE: ID2D1DeviceContext implements From<&ID2D1DeviceContext> for &ID2D1RenderTarget
    fn fill_rectangle(
        &self,
        rounded_rect: &D2D1_ROUNDED_RECT,
        renderer: &ID2D1RenderTarget,
        brush: &ID2D1Brush,
    ) {
        unsafe {
            match rounded_rect.radiusX {
                0.0 => renderer.FillRectangle(&rounded_rect.rect, brush),
                _ => renderer.FillRoundedRectangle(rounded_rect, brush),
            }
        }
    }

    pub fn set_anims_timer_if_needed(&mut self, border_window: HWND) {
        self.animations
            .set_timer_if_needed(border_window, &mut self.last_anim_time);
    }

    pub fn destroy_anims_timer(&mut self) {
        self.animations.destroy_timer();
    }

    pub fn animate(&mut self, bounds: D2D_RECT_F, window_state: WindowState) -> anyhow::Result<()> {
        let anim_elapsed = self
            .last_anim_time
            .get_or_insert_with(time::Instant::now)
            .elapsed();
        let render_elapsed = self
            .last_render_time
            .get_or_insert_with(time::Instant::now)
            .elapsed();

        let mut update = false;

        let current_anims = self.animations.get_current(window_state).clone();
        for anim_params in current_anims.iter() {
            match anim_params.anim_type {
                AnimType::Spiral | AnimType::ReverseSpiral => {
                    self.animations.animate_spiral(
                        &bounds,
                        &self.active_color,
                        &self.inactive_color,
                        &anim_elapsed,
                        anim_params,
                    );
                    update = true;
                }
                AnimType::Fade => {
                    let correct_active_opacity = if window_state == WindowState::Active {
                        1.0
                    } else {
                        0.0
                    };

                    if self.active_color.get_opacity()? != correct_active_opacity
                        || self.inactive_color.get_opacity()? != 1.0 - correct_active_opacity
                    {
                        self.animations.animate_fade(
                            window_state,
                            &self.active_color,
                            &self.inactive_color,
                            &anim_elapsed,
                            anim_params,
                        )?;
                        update = true;
                    }
                }
            }
        }

        self.last_anim_time = Some(time::Instant::now());

        let render_interval = 1.0 / self.animations.fps as f32;
        let time_diff = render_elapsed.as_secs_f32() - render_interval;
        if update && (time_diff.abs() <= 0.001 || time_diff >= 0.0) {
            self.render(bounds, window_state)?;
        }

        // Halt animation timer when no continuous animations exist and Fade has finished
        let has_continuous = current_anims
            .iter()
            .any(|p| matches!(p.anim_type, AnimType::Spiral | AnimType::ReverseSpiral));
        if !has_continuous {
            let correct_active_opacity = if window_state == WindowState::Active {
                1.0
            } else {
                0.0
            };
            let fade_done = (self.active_color.get_opacity()? - correct_active_opacity).abs()
                < 0.001
                && (self.inactive_color.get_opacity()? - (1.0 - correct_active_opacity)).abs()
                    < 0.001;
            if fade_done {
                self.destroy_anims_timer();
            }
        }

        Ok(())
    }
}

fn create_corner_caps_geometry(
    factory: &ID2D1Factory1,
    bounds: &D2D_RECT_F,
    radius: f32,
    stroke_width: f32,
) -> windows::core::Result<ID2D1PathGeometry> {
    unsafe {
        let path_geometry = factory.CreatePathGeometry()?;
        let sink = path_geometry.Open()?;

        let r_outer = (radius + stroke_width / 2.0)
            .min((bounds.right - bounds.left) / 2.0)
            .min((bounds.bottom - bounds.top) / 2.0);

        if r_outer > 0.0 {
            let arc_size = D2D_SIZE_F {
                width: r_outer,
                height: r_outer,
            };

            // Top-left corner cap
            sink.BeginFigure(
                Vector2 {
                    X: bounds.left,
                    Y: bounds.top,
                },
                D2D1_FIGURE_BEGIN_FILLED,
            );
            sink.AddLine(Vector2 {
                X: bounds.left,
                Y: bounds.top + r_outer,
            });
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: Vector2 {
                    X: bounds.left + r_outer,
                    Y: bounds.top,
                },
                size: arc_size,
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);

            // Top-right corner cap
            sink.BeginFigure(
                Vector2 {
                    X: bounds.right,
                    Y: bounds.top,
                },
                D2D1_FIGURE_BEGIN_FILLED,
            );
            sink.AddLine(Vector2 {
                X: bounds.right - r_outer,
                Y: bounds.top,
            });
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: Vector2 {
                    X: bounds.right,
                    Y: bounds.top + r_outer,
                },
                size: arc_size,
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);

            // Bottom-right corner cap
            sink.BeginFigure(
                Vector2 {
                    X: bounds.right,
                    Y: bounds.bottom,
                },
                D2D1_FIGURE_BEGIN_FILLED,
            );
            sink.AddLine(Vector2 {
                X: bounds.right,
                Y: bounds.bottom - r_outer,
            });
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: Vector2 {
                    X: bounds.right - r_outer,
                    Y: bounds.bottom,
                },
                size: arc_size,
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);

            // Bottom-left corner cap
            sink.BeginFigure(
                Vector2 {
                    X: bounds.left,
                    Y: bounds.bottom,
                },
                D2D1_FIGURE_BEGIN_FILLED,
            );
            sink.AddLine(Vector2 {
                X: bounds.left + r_outer,
                Y: bounds.bottom,
            });
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: Vector2 {
                    X: bounds.left,
                    Y: bounds.bottom - r_outer,
                },
                size: arc_size,
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        }

        sink.Close()?;
        Ok(path_geometry.into())
    }
}
